//! Vector row images belong to their heap write group. Persistent graph hooks
//! publish only after that group, and failed publication fences actual state.
use super::write_outcome::WriteOutcome;
use super::{layout_epochs::LayoutEpochs, time_travel::PendingHistoryRead, time_travel::SnapshotManager};
use super::{tde, VectorIndexManager, WriteGate};
use crate::crypto::KeyManager;
use crate::{Error, Result, Schema, Tuple, Value};
use rocksdb::{WriteBatch, DB};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Owned component handles, never an Engine Arc or a transaction slot.
pub(crate) struct VectorRowContext {
    db: Arc<DB>,
    manager: Arc<VectorIndexManager>,
    snapshots: Arc<SnapshotManager>,
    gate: Arc<WriteGate>,
    layouts: Arc<LayoutEpochs>,
    key_manager: Option<Arc<KeyManager>>,
    data_epoch: Arc<AtomicU64>,
}
impl VectorRowContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        db: Arc<DB>,
        manager: Arc<VectorIndexManager>,
        snapshots: Arc<SnapshotManager>,
        gate: Arc<WriteGate>,
        layouts: Arc<LayoutEpochs>,
        key_manager: Option<Arc<KeyManager>>,
        data_epoch: Arc<AtomicU64>,
    ) -> Self {
        Self {
            db,
            manager,
            snapshots,
            gate,
            layouts,
            key_manager,
            data_epoch,
        }
    }
    pub(crate) fn plan(self: &Arc<Self>) -> VectorRowPlan {
        VectorRowPlan {
            context: Arc::clone(self),
            rows: BTreeSet::new(),
            schemas: HashMap::new(),
            faults: BTreeMap::new(),
            settled: None,
            write_called: false,
        }
    }
    fn fence(&self, table: &str, reason: &str) {
        // Fence cached plans/rows BEFORE retiring the handles they could use.
        self.layouts.quarantine(table);
        self.gate.quarantine_table(table, reason);
        self.data_epoch.fetch_add(1, Ordering::Release);
        tracing::error!(table, reason, "vector row publication is unavailable until reopen");
    }
}

pub(crate) struct VectorRowPlan {
    context: Arc<VectorRowContext>,
    rows: BTreeSet<(String, u64)>,
    schemas: HashMap<String, Schema>,
    faults: BTreeMap<String, Option<u64>>,
    settled: Option<bool>,
    write_called: bool,
}
impl VectorRowPlan {
    pub(crate) fn capture(&mut self, table: &str, row: u64) {
        if !self.context.manager.table_has_indexes(table) {
            return;
        }
        self.rows.insert((table.to_string(), row));
        self.faults
            .entry(table.to_string())
            .or_insert_with(|| self.context.manager.hook_error_generation(table));
    }
    /// Input values are transaction plaintext; sidecars may be pending in the
    /// same batch. Decode current logical vectors, including attmissing values.
    pub(crate) fn stage(
        &mut self,
        key: &[u8],
        plain: Option<&[u8]>,
        pending: Option<&PendingHistoryRead<'_>>,
        batch: &mut WriteBatch,
    ) -> Result<()> {
        let Some(rest) = key.strip_prefix(b"data:") else {
            return Ok(());
        };
        let Some((table, row)) = std::str::from_utf8(rest).ok().and_then(|text| text.rsplit_once(':')) else {
            return Ok(());
        };
        let Some(row) = row.parse::<u64>().ok() else {
            return Ok(());
        };
        if !self.context.manager.table_has_indexes(table) {
            return Ok(());
        }
        self.capture(table, row);
        if !cfg!(feature = "vector-persist") {
            // Only marker invalidation is compiled in this mode; its manager
            // explicitly ignores schema/tuple. Avoid new heap/schema reads,
            // decryption and tuple copies for ordinary HNSW transactions.
            return self.context.manager.stage_row_images(
                table,
                row,
                &Schema { columns: Vec::new() },
                None,
                batch,
                self.context.gate.write_group_ceiling(),
            );
        }
        if !self.schemas.contains_key(table) {
            let key = format!("meta:table:{table}");
            let raw = match pending.and_then(|lookup| lookup(key.as_bytes())) {
                Some(raw) => raw,
                None => self.context.db.get(key.as_bytes())?,
            }
            .ok_or_else(|| Error::storage(format!("missing schema for vector row table '{table}'")))?;
            let decoded = tde::open_ref(self.context.key_manager.as_deref(), key.as_bytes(), &raw)?;
            let schema: Schema = bincode::deserialize(decoded.as_ref())
                .map_err(|error| Error::storage(format!("vector row schema: {error}")))?;
            self.schemas.insert(table.to_string(), schema);
        }
        let schema = self
            .schemas
            .get(table)
            .ok_or_else(|| Error::internal("vector row schema cache missing"))?;
        let tuple = plain
            .map(|plain| -> Result<Tuple> {
                let logical = self
                    .context
                    .snapshots
                    .logical_current_row_for_history(table, row, plain, pending)?;
                let mut tuple: Tuple = bincode::deserialize(logical.as_ref())
                    .map_err(|error| Error::storage(format!("vector row image: {error}")))?;
                while tuple.values.len() < schema.columns.len() {
                    let column = schema
                        .columns
                        .get(tuple.values.len())
                        .ok_or_else(|| Error::internal("vector missing-value column"))?;
                    let key = super::Catalog::attmissing_key(table, &column.name);
                    let raw = match pending.and_then(|lookup| lookup(&key)) {
                        Some(raw) => raw,
                        None => self.context.db.get(&key)?,
                    };
                    let value = match raw {
                        Some(raw) => {
                            let plain = tde::open_ref(self.context.key_manager.as_deref(), &key, &raw)?;
                            bincode::deserialize::<Value>(plain.as_ref())
                                .map_err(|error| Error::storage(format!("vector missing value: {error}")))?
                        }
                        None => Value::Null,
                    };
                    tuple.values.push(value);
                }
                tuple.values.truncate(schema.columns.len());
                Ok(tuple)
            })
            .transpose()?;
        self.context.manager.stage_row_images(
            table,
            row,
            schema,
            tuple.as_ref(),
            batch,
            self.context.gate.write_group_ceiling(),
        )
    }
    pub(crate) fn stage_tuple(
        &mut self,
        table: &str,
        row: u64,
        schema: &Schema,
        tuple: Option<&Tuple>,
        batch: &mut WriteBatch,
    ) -> Result<()> {
        self.capture(table, row);
        self.context.manager.stage_row_images(
            table,
            row,
            schema,
            tuple,
            batch,
            self.context.gate.write_group_ceiling(),
        )
    }

    pub(crate) fn row_group_applied(&self) {
        if !self.rows.is_empty() {
            self.context.manager.row_group_applied();
        }
    }

    pub(crate) fn after_write_crash_point(&self) {
        #[cfg(feature = "internal-tests")]
        for table in self.faults.keys() {
            super::engine::exit_after_ddl_write("row_transaction", table);
        }
    }

    pub(crate) fn publish(&self) -> Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        self.context.manager.row_group_applied();
        let mut first_error = None;
        for (table, row) in &self.rows {
            if let Err(error) = self.context.manager.publish_row_images(table, *row) {
                // Finish every owned row/index publication before settlement.
                // Its heap group already applied; dropping later callbacks
                // would leave other committed ownership changes unpublished.
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
    /// A native-OK ownership callback can fail after persistent publication.
    /// Settlement already fenced that fault; report its typed availability
    /// error without a DB read or an additional durable repair attempt.
    pub(crate) fn check_publication_available(&self) -> Result<()> {
        for table in self.faults.keys() {
            self.context.gate.check_table_available(table)?;
        }
        Ok(())
    }

    pub(crate) fn write_call_started(&mut self) {
        self.write_called = true;
    }

    /// Undo is safe only for a rejected original group. An uncertain native
    /// result is fenced before its census drops; no healthy proof read is used.
    pub(crate) fn settle(&mut self, outcome: WriteOutcome, publication_complete: bool) -> bool {
        if let Some(undo) = self.settled {
            return undo;
        }
        let mut may_undo = matches!(outcome, WriteOutcome::NotWritten);
        let mut retire = Vec::new();
        for (table, before) in &self.faults {
            let hook_failed = before.is_none() || self.context.manager.hook_error_generation(table) != *before;
            if hook_failed
                || matches!(outcome, WriteOutcome::Unknown)
                || (matches!(outcome, WriteOutcome::Applied) && !publication_complete)
            {
                may_undo = false;
                let reason = if hook_failed {
                    "vector graph hook failed"
                } else if matches!(outcome, WriteOutcome::Unknown) {
                    "native heap write outcome is unknown"
                } else {
                    "applied heap write has unfinished vector publication"
                };
                // Fence every affected table before waiting for any persistent
                // lease: a waiter must not keep another affected table readable.
                self.context.fence(table, reason);
                retire.push((table, reason));
            }
        }
        for (table, reason) in retire {
            self.context.manager.retire_table_indexes(table, reason);
        }
        self.settled = Some(may_undo);
        may_undo
    }
}
impl Drop for VectorRowPlan {
    fn drop(&mut self) {
        if self.settled.is_none() {
            self.settle(
                if self.write_called {
                    WriteOutcome::Unknown
                } else {
                    WriteOutcome::NotWritten
                },
                false,
            );
        }
    }
}
