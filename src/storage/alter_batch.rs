//! Spec R5.5 (`docs/plans/specs/R5_5_drop_column_and_multi_alter.md`): the storage half of
//! one `ALTER TABLE` statement written as ONE group.
//!
//! The executor (`EmbeddedDatabase`'s `alter_table` module) runs every clause of the
//! statement against the table as the statement has it so far, in memory, and stages
//! nothing until the last clause has been asked everything it can refuse. Then:
//!
//! * [`StorageEngine::stage_rewritten_rows`] stages every row's new image, when a clause
//!   rewrites rows (`DROP COLUMN`, a volatile default, `SET STORAGE`), each with its
//!   version at the statement's one timestamp, up to `[storage] max_atomic_rewrite_bytes`;
//! * `Catalog::stage_altered_table` stages the catalog records of [`AlteredTable`]: the
//!   schema, the missing values, the index definitions a rename names, the shape record
//!   a branch anchor reads and the layout record a read of an older version reads
//!   (`storage::table_layout`), with their logical-WAL entries;
//! * the constraint, identity and columnar records the clauses changed;
//!
//! all into the statement's `DdlBatch`, written once. In-memory state (the schema and
//! constraint caches, the index registry) is published after the write.

use std::collections::HashMap;

use rocksdb::{Direction, IteratorMode, ReadOptions, WriteBatch};

use super::row_shape::{RowShape, StagedRewrite};
use super::StorageEngine;
use crate::{Error, Result, Schema, Tuple, Value};

/// What an `ALTER TABLE` statement did to main's table's catalog, as a whole.
#[derive(Debug, Clone)]
pub struct AlteredTable {
    /// The table.
    pub table: String,
    /// Its schema when the statement began.
    pub base: Schema,
    /// The schema it leaves.
    pub schema: Schema,
    /// The missing values when the statement began, by column name.
    pub base_missing: HashMap<String, Value>,
    /// The missing values it leaves, by column name: none is NULL.
    pub missing: HashMap<String, Value>,
    /// Every `RENAME COLUMN`, in order.
    pub column_renames: Vec<(String, String)>,
    /// Every `DROP COLUMN`, in order: the table's width before it, and the position.
    pub drops: Vec<(usize, usize)>,
    /// A clause changed what a version written before the statement is read under
    /// (`DROP COLUMN`, `RENAME COLUMN`): the shape record a branch anchor asks.
    pub shape_changed: bool,
    /// The statement's one timestamp: its rewritten rows' versions, its layout record.
    pub at: u64,
    /// What makes a version older than the statement unreadable under the table's
    /// definition now, if anything (`SET STORAGE` out of a stored representation).
    pub cut: Option<super::table_layout::HistoryCut>,
}

impl AlteredTable {
    /// Did the statement change anything the catalog holds?
    pub fn changed(&self) -> bool {
        self.schema != self.base || self.missing != self.base_missing || !self.column_renames.is_empty()
    }
}

/// What a rewrite does to one row: `row_id`'s image, shaped to the table's width
/// when the statement began, and the same row with side-storage references resolved.
/// The closure changes the stored tuple and its logical history tuple together; it
/// runs once per row, so volatile expressions and storage encoders are never repeated.
pub type RowRewrite<'r> = dyn FnMut(u64, &mut Tuple, &mut Tuple) -> Result<()> + 'r;

/// `ALTER TABLE … ALTER COLUMN … SET STORAGE`: the column, and its move.
#[derive(Debug, Clone)]
pub struct ColumnMigration {
    /// Its position in the table.
    pub position: usize,
    /// The column, as it is before.
    pub column: crate::Column,
    /// The representation its values have.
    pub old_mode: crate::ColumnStorageMode,
    /// The one they move to.
    pub new_mode: crate::ColumnStorageMode,
}

/// How large a rewrite's group may grow, and the refusal above it.
pub struct RewriteLimit<'l> {
    /// `[storage] max_atomic_rewrite_bytes`.
    pub max_bytes: u64,
    /// The 54000 a group of this many bytes is refused with.
    pub refuse: &'l dyn Fn(u64) -> Error,
}

/// A conservative encoded size for one B4 copy. Its payload is the logical row,
/// not the compact stored row's dictionary/CAS/columnar references. Branch IDs can
/// be any u64, so use the longest key form, include the frozen marker, and allow
/// the full varint/tag overhead of both puts. The supplied image is already sealed
/// with the row codec, whose framing size is the same for data: and bdata:.
pub fn frozen_row_copy_bytes(table: &str, row: u64, sealed_logical_bytes: usize) -> u64 {
    let row_key = super::branch_view::branch_row_key(u64::MAX, table, row);
    let marker_key = super::branch_view::branch_frozen_key(u64::MAX, table, row);
    u64::try_from(sealed_logical_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(u64::try_from(row_key.len()).unwrap_or(u64::MAX))
        .saturating_add(u64::try_from(marker_key.len()).unwrap_or(u64::MAX))
        .saturating_add(22)
}

impl StorageEngine {
    /// Spec R5.5: stage into `batch` the new image of every row of main's `table`, each
    /// row decoded as stored, shaped to the table's width when the statement began
    /// (`shape`, so a row older than a column reads its missing value), then given to
    /// `rewrite`. With time travel on, each new image gets a version at `timestamp`
    /// and a row with no version at all its image before, as a version older than any
    /// snapshot (`SnapshotManager::stage_rewritten_row_version`), and the batch a
    /// `snapshot:` record of `timestamp`: a read at a snapshot from before the
    /// statement still finds the rows as they were, one after it the new images.
    /// Publish the returned snapshot once `batch` is written
    /// (`SnapshotManager::publish_rewrite_snapshot`).
    ///
    /// `batch` is measured after each row, with the copies main's live branches take
    /// of the rows (phase B4): above `limit` the statement is refused (54000), and
    /// nothing has been written. The walk seeks to the table's rows and stops past
    /// them.
    pub fn stage_rewritten_rows(
        &self,
        table: &str,
        shape: &RowShape,
        timestamp: u64,
        rewrite: &mut RowRewrite<'_>,
        limit: &RewriteLimit<'_>,
        batch: &mut WriteBatch,
    ) -> Result<StagedRewrite> {
        // A rewrite makes the per-table committed-write watermark untrustworthy (an
        // open reader's fast path would serve rows past its snapshot); its re-check
        // reads the absent entry as "changed".
        self.invalidate_write_watermark(table);
        let snapshot = if self.time_travel_enabled() {
            Some(self.snapshot_manager().stage_rewrite_snapshot(batch, timestamp)?)
        } else {
            None
        };
        // Resolve references under the original catalog before a DROP shifts
        // positions, a rename moves names, or a storage migration rewrites sidecars.
        let old_schema = self.catalog().get_table_schema(table)?;
        let prefix = format!("data:{table}:");
        let copying = self.branches_copying_main_rows(table)?;
        let mut copy_bytes = 0u64;
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let rows = self
            .db
            .iterator_opt(IteratorMode::From(prefix.as_bytes(), Direction::Forward), read_opts);
        let mut rewritten = 0u64;
        for item in rows {
            let (key, raw) = item.map_err(|e| Error::storage(format!("Iterator error: {e}")))?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            let Some(row_id) = Self::parse_row_id_after_prefix(&key, prefix.len()) else {
                continue;
            };
            let stored = self.decode_stored_ref(&key, &raw)?;
            let mut tuple: Tuple = bincode::deserialize(&stored)
                .map_err(|e| Error::storage(format!("Failed to deserialize tuple: {e}")))?;
            shape.apply(&mut tuple);
            let mut logical = tuple.clone();
            self.resolve_column_storage_refs(table, &old_schema, row_id, &mut logical)?;
            if logical
                .values
                .iter()
                .any(|value| matches!(value, Value::DictRef { .. } | Value::CasRef { .. } | Value::ColumnarRef))
            {
                return Err(Error::storage(format!(
                    "cannot resolve side storage before rewriting table '{table}' row {row_id}"
                )));
            }
            let old_logical = if snapshot.is_some() || copying > 0 {
                let image = bincode::serialize(&logical)
                    .map_err(|e| Error::storage(format!("Failed to serialize logical pre-image: {e}")))?;
                Some(super::tde::seal_row_value(self.key_manager_ref(), &key, &image)?.into_owned())
            } else {
                None
            };
            let frozen_bytes = old_logical
                .as_ref()
                .map_or(0, |image| frozen_row_copy_bytes(table, row_id, image.len()));
            rewrite(row_id, &mut tuple, &mut logical)?;
            let image =
                bincode::serialize(&tuple).map_err(|e| Error::storage(format!("Failed to serialize tuple: {e}")))?;
            let sealed = super::tde::seal_row_value(self.key_manager_ref(), &key, &image)?;
            if let (Some(_), Some(old_logical)) = (&snapshot, old_logical) {
                let image = bincode::serialize(&logical)
                    .map_err(|e| Error::storage(format!("Failed to serialize logical rewrite: {e}")))?;
                let logical_sealed = super::tde::seal_row_value(self.key_manager_ref(), &key, &image)?;
                self.snapshot_manager().stage_logical_rewritten_row_version(
                    batch,
                    table,
                    row_id,
                    timestamp,
                    logical_sealed.as_ref(),
                    &old_logical,
                )?;
            }
            batch.put(&key, sealed.as_ref());
            rewritten += 1;
            copy_bytes = copy_bytes.saturating_add(frozen_bytes.saturating_mul(copying));
            let bytes = (batch.size_in_bytes() as u64).saturating_add(copy_bytes);
            if bytes > limit.max_bytes {
                return Err((limit.refuse)(bytes));
            }
        }
        Ok(StagedRewrite {
            rows: rewritten,
            snapshot,
            frozen_copy_bytes: copy_bytes,
        })
    }
}
