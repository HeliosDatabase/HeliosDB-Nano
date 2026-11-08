//! Incoming FK actions for ordinary UPDATE and ON CONFLICT DO UPDATE.
//!
//! PostgreSQL reference: docs/18/sql-createtable.html, REFERENCES clauses:
//! actions run in the command; only NO ACTION checks may be deferred. The
//! complete action graph stages in the caller's transaction and has one commit.

use crate::sql::{ConstraintEnforcement, ForeignKeyConstraint, ReferentialAction};
use crate::{
    sql, storage, EmbeddedDatabase, Error, FkValidationMode, FkValidationSource, LockedRow, Result, Schema, Tuple,
    Value,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

type Change = (u64, Tuple, Tuple);
type Event = (u64, Tuple, Option<Tuple>);

pub(crate) type FkTimingKey = (String, String);

pub(crate) fn fk_timing_key(fk: &ForeignKeyConstraint) -> FkTimingKey {
    (fk.table_name.clone(), fk.name.clone())
}

/// Shared only within ONE validation invocation. Immutable row/key obligations
/// never capture or mutate this cache, so SET IMMEDIATE, COMMIT and savepoint
/// rewind always start with a fresh view of the current staged dataset.
#[derive(Default)]
pub(crate) struct FkValidationRun {
    // A high-level commit preflight must detect strict refusals before consuming
    // its ART undo owner. Nonfatal audit publication belongs to actual COMMIT.
    preflight_only: bool,
    rows: HashMap<(u64, String), Arc<Vec<Tuple>>>,
    keys: HashMap<(u64, String, Vec<String>), Arc<HashSet<Vec<Value>>>>,
}
impl FkValidationRun {
    pub(crate) fn preflight() -> Self {
        Self {
            preflight_only: true,
            ..Self::default()
        }
    }

    fn keys(
        &mut self,
        storage: &storage::StorageEngine,
        txn: &storage::Transaction,
        branch: Option<&crate::session::BranchBinding>,
        table: &str,
        schema: &Schema,
        columns: &[String],
    ) -> Result<Arc<HashSet<Vec<Value>>>> {
        let level = branch.map_or(0, |binding| binding.branch_id);
        let set_key = (level, table.to_string(), columns.to_vec());
        if let Some(keys) = self.keys.get(&set_key) {
            return Ok(Arc::clone(keys));
        }
        let row_key = (level, table.to_string());
        let rows = match self.rows.get(&row_key) {
            Some(rows) => Arc::clone(rows),
            None => {
                let rows = Arc::new(rows_at(storage, txn, branch, table, schema)?);
                self.rows.insert(row_key, Arc::clone(&rows));
                rows
            }
        };
        let keys = Arc::new(
            rows.iter()
                .map(|row| key(schema, row, columns))
                .collect::<Result<HashSet<_>>>()?,
        );
        self.keys.insert(set_key, Arc::clone(&keys));
        Ok(keys)
    }
}

/// Read committed rows and this transaction's current logical overlay. Explicit
/// branch binding is captured by deferred checks, so a later session switch can
/// never redirect a check to a different branch.
fn rows_at(
    storage: &storage::StorageEngine,
    txn: &storage::Transaction,
    branch: Option<&crate::session::BranchBinding>,
    table: &str,
    schema: &Schema,
) -> Result<Vec<Tuple>> {
    if let Some(branch) = branch {
        return storage::BranchView::from_binding(storage, branch).scan(Some(txn), table, schema);
    }
    let committed = storage.scan_table_with_schema(table, schema)?;
    let shape = storage.row_shape(table, schema.columns.len())?;
    txn.merge_with_write_set_shaped(table, committed, &shape)
}

fn row_at(
    storage: &storage::StorageEngine,
    txn: &storage::Transaction,
    branch: Option<&crate::session::BranchBinding>,
    table: &str,
    schema: &Schema,
    row_id: u64,
) -> Result<Option<Tuple>> {
    if let Some(branch) = branch {
        return storage::BranchView::from_binding(storage, branch).get(Some(txn), table, row_id, schema);
    }
    let Some(raw) = txn.get_latest(&storage::StorageEngine::build_data_key(table, row_id))? else {
        return Ok(None);
    };
    let mut row: Tuple =
        bincode::deserialize(&raw).map_err(|error| Error::storage(format!("FK current child image: {error}")))?;
    storage.shape_row(table, schema.columns.len(), &mut row)?;
    storage.resolve_column_storage_refs(table, schema, row_id, &mut row)?;
    row.row_id = Some(row_id);
    Ok(Some(row))
}

fn current_fk(storage: &storage::StorageEngine, fk: &ForeignKeyConstraint) -> Result<Option<ForeignKeyConstraint>> {
    if !storage.catalog().table_exists(&fk.table_name)? {
        return Ok(None);
    }
    Ok(storage
        .catalog()
        .load_table_constraints(&fk.table_name)?
        .foreign_keys
        .into_iter()
        .find(|current| {
            current.name == fk.name
                && current.columns == fk.columns
                && current.references_table == fk.references_table
                && current.references_columns == fk.references_columns
                && current.enforcement != ConstraintEnforcement::NotEnforced
        }))
}

fn key(schema: &Schema, row: &Tuple, columns: &[String]) -> Result<Vec<Value>> {
    columns
        .iter()
        .map(|name| {
            schema
                .get_column_index(name)
                .and_then(|i| row.values.get(i))
                .cloned()
                .ok_or_else(|| Error::internal(format!("FK column {name} is absent from its stored row")))
        })
        .collect()
}

fn matches_key(schema: &Schema, row: &Tuple, columns: &[String], wanted: &[Value]) -> bool {
    !wanted.is_empty()
        && columns.len() == wanted.len()
        && wanted.iter().all(|v| !matches!(v, Value::Null))
        && key(schema, row, columns).is_ok_and(|have| have.iter().all(|v| !matches!(v, Value::Null)) && have == wanted)
}

fn validate_parent_change(
    storage: &storage::StorageEngine,
    txn: &storage::Transaction,
    branch: Option<&crate::session::BranchBinding>,
    fk: &ForeignKeyConstraint,
    child_schema: &Schema,
    parent_schema: &Schema,
    old_key: &[Value],
) -> Result<()> {
    // NO ACTION constrains the final relationship, not a particular parent's
    // identity. A replacement row restoring the old key is valid.
    let child_key = EmbeddedDatabase::coerce_fk_probe_values(child_schema, &fk.columns, old_key);
    let references = rows_at(storage, txn, branch, &fk.table_name, child_schema)?
        .iter()
        .any(|row| matches_key(child_schema, row, &fk.columns, &child_key));
    if !references {
        return Ok(());
    }
    let parent_key = EmbeddedDatabase::coerce_fk_probe_values(parent_schema, &fk.references_columns, old_key);
    if rows_at(storage, txn, branch, &fk.references_table, parent_schema)?
        .iter()
        .any(|row| matches_key(parent_schema, row, &fk.references_columns, &parent_key))
    {
        return Ok(());
    }
    Err(Error::constraint_violation(format!(
        "Foreign key constraint '{}' violated: cannot remove referenced key in '{}' - referenced by '{}'",
        fk.name, fk.references_table, fk.table_name
    )))
}

impl EmbeddedDatabase {
    /// Inbound tables are locked before any row is read or staged. Exclusive
    /// child locks also exclude an INSERT of a new old-key reference between
    /// checking children and committing the parent's changed key. Discover the
    /// transitive graph for every declared action: a child's FK may be its PK.
    pub(crate) fn fk_update_lock_tables(&self, table: &str, columns: &[String]) -> Result<Vec<String>> {
        self.fk_parent_lock_tables(table, Some(columns))
    }

    pub(crate) fn fk_delete_lock_tables(&self, table: &str) -> Result<Vec<String>> {
        self.fk_parent_lock_tables(table, None)
    }

    fn fk_parent_lock_tables(&self, table: &str, columns: Option<&[String]>) -> Result<Vec<String>> {
        if *self.fk_validation_mode.read() == FkValidationMode::Off
            || *self.fk_validation_source.read() == FkValidationSource::Proxy
        {
            return Ok(Vec::new());
        }
        let catalog = self.storage.catalog();
        let mut found = BTreeMap::new();
        let mut pending = vec![table.to_string()];
        while let Some(parent) = pending.pop() {
            for fk in catalog.get_referencing_fks(&parent)? {
                if fk.enforcement == ConstraintEnforcement::NotEnforced {
                    continue;
                }
                if parent == table
                    && columns.is_some_and(|columns| {
                        !fk.references_columns
                            .iter()
                            .any(|referenced| columns.iter().any(|column| column.eq_ignore_ascii_case(referenced)))
                    })
                {
                    continue;
                }
                if found.insert(fk.table_name.clone(), ()).is_none() && fk.table_name != table {
                    pending.push(fk.table_name);
                }
            }
        }
        if found.is_empty() {
            return Ok(Vec::new());
        }
        found.insert(table.to_string(), ());
        Ok(found.into_keys().collect())
    }

    /// A current child-row obligation, rather than a validation of historical
    /// probe values or unrelated legacy rows admitted while checks were off.
    pub(crate) fn defer_updated_fk(
        &self,
        txn: &storage::Transaction,
        fk: &ForeignKeyConstraint,
        row_id: u64,
    ) -> Result<()> {
        let storage = Arc::clone(&self.storage);
        let mode = Arc::clone(&self.fk_validation_mode);
        let branch = storage.active_branch();
        let fk = fk.clone();
        let identity = bincode::serialize(&(
            branch.as_ref().map_or(0, |binding| binding.branch_id),
            &fk.table_name,
            &fk.name,
            0u8,
            row_id,
        ))
        .map_err(|error| Error::storage(format!("FK child obligation identity: {error}")))?;
        txn.defer_parent_fk_check(identity, fk_timing_key(&fk), move |txn, run| {
            let Some(current) = current_fk(&storage, &fk)? else {
                return Ok(());
            };
            let child_schema = storage.catalog().get_table_schema(&current.table_name)?;
            let Some(child) = row_at(
                &storage,
                txn,
                branch.as_deref(),
                &current.table_name,
                &child_schema,
                row_id,
            )?
            else {
                return Ok(());
            };
            let values = key(&child_schema, &child, &current.columns)?;
            if values.iter().any(|value| matches!(value, Value::Null)) {
                return Ok(());
            }
            let parent_schema = storage.catalog().get_table_schema(&current.references_table)?;
            let wanted = EmbeddedDatabase::coerce_fk_probe_values(&parent_schema, &current.references_columns, &values);
            if run
                .keys(
                    &storage,
                    txn,
                    branch.as_deref(),
                    &current.references_table,
                    &parent_schema,
                    &current.references_columns,
                )?
                .contains(&wanted)
            {
                return Ok(());
            }
            let mode = *mode.read();
            if mode == FkValidationMode::Audit || current.enforcement == ConstraintEnforcement::LockFree {
                return if run.preflight_only {
                    Ok(())
                } else {
                    EmbeddedDatabase::record_fk_violation_with(&storage, mode, &current, &values)
                };
            }
            Err(Error::constraint_violation(format!(
                "Foreign key constraint '{}' violated at COMMIT: child '{}' references a missing parent",
                current.name, current.table_name
            )))
        });
        Ok(())
    }

    fn defer_parent_key_change(
        &self,
        txn: &storage::Transaction,
        fk: &ForeignKeyConstraint,
        old_key: Vec<Value>,
    ) -> Result<()> {
        let storage = Arc::clone(&self.storage);
        let mode = Arc::clone(&self.fk_validation_mode);
        let branch = storage.active_branch();
        let fk = fk.clone();
        let identity = bincode::serialize(&(
            branch.as_ref().map_or(0, |binding| binding.branch_id),
            &fk.table_name,
            &fk.name,
            1u8,
            &old_key,
        ))
        .map_err(|error| Error::storage(format!("FK parent obligation identity: {error}")))?;
        txn.defer_parent_fk_check(identity, fk_timing_key(&fk), move |txn, run| {
            let Some(current) = current_fk(&storage, &fk)? else {
                return Ok(());
            };
            let child_schema = storage.catalog().get_table_schema(&current.table_name)?;
            let parent_schema = storage.catalog().get_table_schema(&current.references_table)?;
            let mode = *mode.read();
            let child_key = EmbeddedDatabase::coerce_fk_probe_values(&child_schema, &current.columns, &old_key);
            let parent_key =
                EmbeddedDatabase::coerce_fk_probe_values(&parent_schema, &current.references_columns, &old_key);
            let has_refs = run
                .keys(
                    &storage,
                    txn,
                    branch.as_deref(),
                    &current.table_name,
                    &child_schema,
                    &current.columns,
                )?
                .contains(&child_key);
            let has_parent = !has_refs
                || run
                    .keys(
                        &storage,
                        txn,
                        branch.as_deref(),
                        &current.references_table,
                        &parent_schema,
                        &current.references_columns,
                    )?
                    .contains(&parent_key);
            let check = if has_parent {
                Ok(())
            } else {
                Err(Error::constraint_violation(format!(
                    "Foreign key constraint '{}' violated: cannot remove referenced key in '{}' - referenced by '{}'",
                    current.name, current.references_table, current.table_name
                )))
            };
            match check {
                Err(Error::ConstraintViolation(_))
                    if mode == FkValidationMode::Audit || current.enforcement == ConstraintEnforcement::LockFree =>
                {
                    if run.preflight_only {
                        Ok(())
                    } else {
                        EmbeddedDatabase::record_fk_violation_with(&storage, mode, &current, &old_key)
                    }
                }
                result => result,
            }
        });
        Ok(())
    }

    /// INSERT SELECT learns row IDs only after its batch is staged. Queue the
    /// same immutable row obligations there; an ordinary preflight cannot do so.
    pub(crate) fn defer_inserted_fk_rows(&self, txn: &storage::Transaction, table: &str, ids: &[u64]) -> Result<()> {
        let mode = *self.fk_validation_mode.read();
        if mode == FkValidationMode::Off || *self.fk_validation_source.read() == FkValidationSource::Proxy {
            return Ok(());
        }
        for fk in self.storage.catalog().load_table_constraints(table)?.foreign_keys {
            if fk.enforcement != ConstraintEnforcement::NotEnforced
                && (mode == FkValidationMode::Deferred
                    || fk.enforcement == ConstraintEnforcement::Deferred
                    || txn.defers_fk_check(&fk))
            {
                for &id in ids {
                    self.defer_updated_fk(txn, &fk, id)?;
                }
            }
        }
        Ok(())
    }

    fn check_updated_fk_row(
        &self,
        txn: &storage::Transaction,
        table: &str,
        schema: &Schema,
        row: &Tuple,
    ) -> Result<()> {
        let mode = *self.fk_validation_mode.read();
        if mode == FkValidationMode::Off || *self.fk_validation_source.read() == FkValidationSource::Proxy {
            return Ok(());
        }
        for fk in self.storage.catalog().load_table_constraints(table)?.foreign_keys {
            if fk.enforcement == ConstraintEnforcement::NotEnforced {
                continue;
            }
            let values = key(schema, row, &fk.columns)?;
            if values.iter().any(|value| matches!(value, Value::Null)) {
                continue;
            }
            if mode == FkValidationMode::Deferred
                || fk.enforcement == ConstraintEnforcement::Deferred
                || txn.defers_fk_check(&fk)
            {
                self.defer_updated_fk(
                    txn,
                    &fk,
                    row.row_id
                        .ok_or_else(|| Error::internal("FK changed child has no row id"))?,
                )?;
            } else if !self.check_referencing_rows_exist(
                &fk.references_table,
                &fk.references_columns,
                &values,
                Some(txn),
            )? {
                if mode == FkValidationMode::Audit || fk.enforcement == ConstraintEnforcement::LockFree {
                    self.record_fk_violation(&fk, &values)?;
                } else {
                    return Err(Error::constraint_violation(format!(
                        "Foreign key constraint '{}' violated: child '{}' references a missing parent",
                        fk.name, table
                    )));
                }
            }
        }
        Ok(())
    }

    fn fk_actions_needed(&self, table: &str, include_outgoing: bool) -> Result<bool> {
        if *self.fk_validation_mode.read() == FkValidationMode::Off
            || *self.fk_validation_source.read() == FkValidationSource::Proxy
        {
            return Ok(false);
        }
        let catalog = self.storage.catalog();
        Ok(!catalog.get_referencing_fks(table)?.is_empty()
            || (include_outgoing && !catalog.load_table_constraints(table)?.foreign_keys.is_empty()))
    }

    /// Root assignments are already staged. Preserve their explicit FK edits
    /// when selecting action members; generated changes retain their first image
    /// so a newly assigned key cannot be cascaded a second time by another root.
    pub(crate) fn apply_fk_parent_updates(
        &self,
        txn: &storage::Transaction,
        table: &str,
        schema: &Schema,
        changes: &[Change],
    ) -> Result<()> {
        if changes.is_empty() || !self.fk_actions_needed(table, true)? {
            return Ok(());
        }
        let events: Vec<Event> = changes
            .iter()
            .map(|(id, old, new)| (*id, old.clone(), Some(new.clone())))
            .collect();
        self.apply_fk_parent_changes(txn, table, schema, &events)
    }

    /// The caller has selected and staged every root tombstone. A recursive
    /// cascade or generated update joins that same transaction and action graph.
    pub(crate) fn apply_fk_parent_deletes(
        &self,
        txn: &storage::Transaction,
        table: &str,
        schema: &Schema,
        deleted: &[(u64, Tuple)],
    ) -> Result<()> {
        if deleted.is_empty() || !self.fk_actions_needed(table, false)? {
            return Ok(());
        }
        let events: Vec<Event> = deleted.iter().map(|(id, old)| (*id, old.clone(), None)).collect();
        self.apply_fk_parent_changes(txn, table, schema, &events)
    }

    fn stage_fk_child_delete(
        &self,
        txn: &storage::Transaction,
        view: Option<&storage::BranchView<'_>>,
        table: &str,
        schema: &Schema,
        row_id: u64,
        row: &Tuple,
    ) -> Result<()> {
        if let Some(view) = view {
            return view.delete(Some(txn), table, row_id);
        }
        self.storage
            .stage_columnar_deletes_grouped_in_transaction(table, &[row_id], schema, txn)?;
        txn.delete(storage::StorageEngine::build_data_key(table, row_id))?;
        let values = Self::tuple_column_values(schema, row);
        txn.note_index_key_removal(table);
        // Register the inverse before a fallible index operation can partially
        // remove keys. Full/statement/savepoint rollback restores the old row.
        self.push_art_undo(
            txn,
            crate::ArtUndoOp::RestoreDeleted {
                table_name: table.to_string(),
                row_id,
                col_values: values.clone(),
            },
        );
        self.remove_index_entries_held(txn, table, row_id, &values)
            .map_err(|error| Error::storage(format!("FK cascade index delete: {error}")))?;
        if self.vector_dml_gate(table) {
            let undo = self
                .storage
                .vector_indexes()
                .on_row_delete_staged(table, row_id, Some(schema), Some(row));
            self.push_vector_undo(txn, undo);
        }
        if !txn.derives_row_wal_at_commit() {
            txn.log_on_commit(storage::WalOperation::Delete {
                table: table.to_string(),
                key: storage::StorageEngine::build_data_key(table, row_id),
            });
        }
        self.storage.row_cache().invalidate(table, row_id);
        Ok(())
    }

    fn apply_fk_parent_changes(
        &self,
        txn: &storage::Transaction,
        table: &str,
        schema: &Schema,
        changes: &[Event],
    ) -> Result<()> {
        let branch = self.storage.active_branch();
        let view = branch
            .as_ref()
            .map(|b| storage::BranchView::from_binding(&self.storage, b));
        let mut before: HashMap<(String, u64), Tuple> = changes
            .iter()
            .filter_map(|(id, _, new)| new.as_ref().map(|new| ((table.to_string(), *id), new.clone())))
            .collect();
        let mut work: VecDeque<(String, Schema, Event)> = changes
            .iter()
            .cloned()
            .map(|change| (table.to_string(), schema.clone(), change))
            .collect();
        let mut touched: BTreeMap<(String, u64), Schema> = BTreeMap::new();
        let mut seen = HashSet::new();
        let mut assigned: HashMap<(String, u64, usize), Value> = HashMap::new();
        let mut checks = Vec::new();
        while let Some((parent, parent_schema, (id, old, new))) = work.pop_front() {
            if let Some(new) = &new {
                touched.insert((parent.clone(), id), parent_schema.clone());
                if old.values == new.values {
                    continue;
                }
            }
            if !seen.insert((
                parent.clone(),
                id,
                old.values.clone(),
                new.as_ref().map(|row| row.values.clone()),
            )) {
                continue;
            }
            for fk in self.storage.catalog().get_referencing_fks(&parent)? {
                if fk.enforcement == ConstraintEnforcement::NotEnforced {
                    continue;
                }
                let old_key = key(&parent_schema, &old, &fk.references_columns)?;
                let new_key = new
                    .as_ref()
                    .map(|new| key(&parent_schema, new, &fk.references_columns))
                    .transpose()?;
                if new_key.as_ref().is_some_and(|new_key| old_key == *new_key)
                    || old_key.iter().any(|v| matches!(v, Value::Null))
                {
                    continue;
                }
                let child_schema = self.storage.catalog().get_table_schema(&fk.table_name)?;
                let wanted = Self::coerce_fk_probe_values(&child_schema, &fk.columns, &old_key);
                let epoch = self.row_epoch_for(view.as_ref(), &fk.table_name);
                let current = rows_at(&self.storage, txn, branch.as_deref(), &fk.table_name, &child_schema)?;
                // PostgreSQL implements RESTRICT as a non-deferred AFTER
                // check of current child rows. A self-row whose FK was also
                // changed in this command no longer references the old key.
                let has_current_refs = current
                    .iter()
                    .any(|row| matches_key(&child_schema, row, &fk.columns, &wanted));
                let members: Vec<Tuple> = current
                    .into_iter()
                    .filter(|row| {
                        let original = row
                            .row_id
                            .and_then(|id| before.get(&(fk.table_name.clone(), id)))
                            .unwrap_or(row);
                        matches_key(&child_schema, original, &fk.columns, &wanted)
                    })
                    .collect();
                match if new.is_some() { fk.on_update } else { fk.on_delete } {
                    ReferentialAction::Restrict if has_current_refs => {
                        if *self.fk_validation_mode.read() == FkValidationMode::Audit
                            || fk.enforcement == ConstraintEnforcement::LockFree
                        {
                            self.record_fk_violation(&fk, &old_key)?;
                            continue;
                        }
                        return Err(Error::constraint_violation(format!(
                            "Foreign key constraint '{}' violated: cannot update row in '{}' - referenced by '{}'",
                            fk.name, parent, fk.table_name
                        )));
                    }
                    ReferentialAction::Restrict => {}
                    ReferentialAction::NoAction => {
                        let defer = fk.enforcement == ConstraintEnforcement::Deferred
                            || *self.fk_validation_mode.read() == FkValidationMode::Deferred
                            || txn.defers_fk_check(&fk);
                        if defer {
                            self.defer_parent_key_change(txn, &fk, old_key)?;
                        } else {
                            checks.push((fk, child_schema, parent_schema.clone(), old_key));
                        }
                    }
                    action => {
                        for child in members {
                            let current_child = match self.lock_row_for_change_on(
                                txn,
                                view.as_ref(),
                                &fk.table_name,
                                &child_schema,
                                child,
                                epoch,
                            )? {
                                LockedRow::Same(row) | LockedRow::Moved(row) => row,
                                LockedRow::Gone => continue,
                            };
                            let row_id = current_child
                                .row_id
                                .ok_or_else(|| Error::internal("FK child has no row id"))?;
                            before
                                .entry((fk.table_name.clone(), row_id))
                                .or_insert_with(|| current_child.clone());
                            if action == ReferentialAction::Cascade && new.is_none() {
                                self.stage_fk_child_delete(
                                    txn,
                                    view.as_ref(),
                                    &fk.table_name,
                                    &child_schema,
                                    row_id,
                                    &current_child,
                                )?;
                                work.push_back((
                                    fk.table_name.clone(),
                                    child_schema.clone(),
                                    (row_id, current_child, None),
                                ));
                                continue;
                            }
                            let mut updated = current_child.clone();
                            let evaluator = sql::Evaluator::new(Arc::new(Schema::new(Vec::new())));
                            for (position, column) in fk.columns.iter().enumerate() {
                                let index = child_schema
                                    .get_column_index(column)
                                    .ok_or_else(|| Error::internal("FK child column is absent"))?;
                                let definition = &child_schema.columns[index];
                                let value = match action {
                                    ReferentialAction::Cascade => {
                                        let value = new_key
                                            .as_ref()
                                            .ok_or_else(|| Error::internal("UPDATE cascade has no new parent key"))?
                                            .get(position)
                                            .cloned()
                                            .ok_or_else(|| Error::internal("FK key arity changed"))?;
                                        if matches!(value, Value::Null) {
                                            value
                                        } else {
                                            evaluator.cast_value(value, &definition.data_type)?
                                        }
                                    }
                                    ReferentialAction::SetNull => Value::Null,
                                    ReferentialAction::SetDefault => match &definition.default_expr {
                                        Some(json) => {
                                            let expr: sql::LogicalExpr = serde_json::from_str(json)
                                                .map_err(|e| Error::storage(format!("FK default: {e}")))?;
                                            sql::default_volatility::evaluate_column_default_with(
                                                &evaluator,
                                                &Tuple::new(Vec::new()),
                                                &expr,
                                                &definition.data_type,
                                            )?
                                        }
                                        None => Value::Null,
                                    },
                                    _ => unreachable!(),
                                };
                                if let Some(prior) =
                                    assigned.insert((fk.table_name.clone(), row_id, index), value.clone())
                                {
                                    if prior != value {
                                        return Err(Error::constraint_violation("conflicting ON UPDATE actions assign different values to the same child column"));
                                    }
                                }
                                updated.values[index] = value;
                            }
                            if current_child.values == updated.values {
                                continue;
                            }
                            Self::check_not_null_for_materialized_insert(&updated.values, &child_schema)?;
                            // Reuse ordinary UPDATE's CHECK and uniqueness validation;
                            // outgoing FKs wait until the entire graph is staged.
                            let constraints = self.storage.catalog().load_table_constraints(&fk.table_name)?;
                            self.validate_on_conflict_updated_row(
                                &fk.table_name,
                                &child_schema,
                                &constraints,
                                &current_child,
                                &updated,
                                Some(txn),
                            )?;
                            if let Some(view) = &view {
                                view.put(Some(txn), &fk.table_name, row_id, &updated)?;
                            } else {
                                self.stage_upsert_update(
                                    txn,
                                    &fk.table_name,
                                    &child_schema,
                                    row_id,
                                    &current_child,
                                    &updated,
                                )?;
                                if !txn.derives_row_wal_at_commit() {
                                    let bytes = bincode::serialize(&updated)
                                        .map_err(|e| Error::storage(format!("FK child image: {e}")))?;
                                    txn.log_on_commit(storage::WalOperation::Update {
                                        table: fk.table_name.clone(),
                                        key: format!("data:{}:{row_id}", fk.table_name).into_bytes(),
                                        tuple: bytes,
                                    });
                                }
                            }
                            work.push_back((
                                fk.table_name.clone(),
                                child_schema.clone(),
                                (row_id, current_child, Some(updated)),
                            ));
                        }
                    }
                }
            }
        }
        for (fk, child_schema, parent_schema, old_key) in checks {
            if let Err(error) = validate_parent_change(
                &self.storage,
                txn,
                branch.as_deref(),
                &fk,
                &child_schema,
                &parent_schema,
                &old_key,
            ) {
                if matches!(error, Error::ConstraintViolation(_))
                    && (*self.fk_validation_mode.read() == FkValidationMode::Audit
                        || fk.enforcement == ConstraintEnforcement::LockFree)
                {
                    self.record_fk_violation(&fk, &old_key)?;
                } else {
                    return Err(error);
                }
            }
        }
        // A self-referencing row may name the key another row of this same
        // command just acquired. Validate against the finished staged graph.
        let mut tables: BTreeMap<String, (Schema, HashSet<u64>)> = BTreeMap::new();
        for ((table, id), schema) in touched {
            tables
                .entry(table)
                .or_insert_with(|| (schema, HashSet::new()))
                .1
                .insert(id);
        }
        for (table, (schema, ids)) in tables {
            if self
                .storage
                .catalog()
                .load_table_constraints(&table)?
                .foreign_keys
                .is_empty()
            {
                continue;
            }
            for row in rows_at(&self.storage, txn, branch.as_deref(), &table, &schema)? {
                if row.row_id.is_some_and(|id| ids.contains(&id)) {
                    self.check_updated_fk_row(txn, &table, &schema, &row)?;
                }
            }
        }
        Ok(())
    }
}
