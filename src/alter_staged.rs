//! `ALTER TABLE … ADD [CONSTRAINT n] UNIQUE (…)` / `… CHECK (…)` over rows the
//! statement's own transaction inserted (seed-then-constrain migrations).
//!
//! A constraint is validated, and its index built, over the committed rows. Over
//! a transaction's own staged changes of the table it was therefore refused
//! (0A000). When those changes are INSERTS ONLY the statement now runs: the
//! staged rows are validated with the committed ones (a CHECK they violate is
//! 23514, a duplicate key among them or against a committed row 23505, nothing
//! written), and a UNIQUE constraint's index takes their keys the way `CREATE
//! INDEX` in a transaction gives its index the staged rows, with the undo that
//! takes them out again on ROLLBACK. DDL is not transactional: after a ROLLBACK
//! the constraint stays, over the committed rows it was validated against.
//!
//! Still refused, as `CREATE INDEX` in a transaction refuses them: a staged
//! UPDATE or DELETE of the table, a savepoint in the transaction, a table with
//! column storage, a branch session. And every other constraint clause: PRIMARY
//! KEY, FOREIGN KEY, and the `ADD COLUMN … UNIQUE / CHECK / REFERENCES` forms.

use super::{apply_steps, AlterStatement};
use crate::storage::art_manager::StagedIndexRow;
use crate::storage::{StorageEngine, Transaction};
use crate::{sql, ColumnStorageMode, EmbeddedDatabase, Error, Result, Tuple};

impl EmbeddedDatabase {
    /// Is `plan` an `ALTER TABLE` of ADD UNIQUE / ADD CHECK clauses alone, over a
    /// table whose staged changes in `txn` are inserts only (see the module docs)?
    /// Then the statement validates the staged rows itself and is not refused.
    pub(crate) fn constraint_over_staged_inserts(
        &self,
        plan: &sql::LogicalPlan,
        txn: Option<&Transaction>,
    ) -> Result<bool> {
        let Some(txn) = txn else {
            return Ok(false);
        };
        let clauses = match plan {
            sql::LogicalPlan::AlterTableMulti { operations } => operations.as_slice(),
            single => std::slice::from_ref(single),
        };
        let mut table: Option<&str> = None;
        for clause in clauses {
            match clause {
                sql::LogicalPlan::AlterTableAddUnique { table_name, .. }
                | sql::LogicalPlan::AlterTableAddCheck { table_name, .. } => table = Some(table_name.as_str()),
                _ => return Ok(false),
            }
        }
        let Some(table) = table else {
            return Ok(false);
        };
        if !txn.has_staged_changes(table) || txn.has_savepoints() || self.storage.is_branch_active() {
            return Ok(false);
        }
        let Ok(schema) = self.storage.catalog().get_table_schema(table) else {
            return Ok(false);
        };
        if schema
            .columns
            .iter()
            .any(|column| column.storage_mode != ColumnStorageMode::Default)
        {
            return Ok(false);
        }
        let prefix = format!("data:{table}:");
        for (key, staged) in txn.staged_with_prefix(prefix.as_bytes()) {
            // A staged delete, or a staged image of a row that is committed (an
            // UPDATE): a ROLLBACK would bring back an image this statement did not
            // validate.
            if staged.is_none() || self.storage.get(&key)?.is_some() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The rows the statement's transaction inserted into its table, as the
    /// statement has them so far (shaped, with the earlier clauses applied). None
    /// when the transaction has staged no row of the table.
    pub(super) fn alter_own_inserted_rows(&self, st: &AlterStatement<'_>) -> Result<Vec<Tuple>> {
        let Some(txn) = st.own_txn() else {
            return Ok(Vec::new());
        };
        let prefix = format!("data:{}:", st.table);
        let mut rows = Vec::new();
        for (key, staged) in txn.staged_with_prefix(prefix.as_bytes()) {
            let (Some(row_id), Some(bytes)) = (StorageEngine::parse_row_id_after_prefix(&key, prefix.len()), staged)
            else {
                continue;
            };
            let mut tuple: Tuple = bincode::deserialize(&bytes)
                .map_err(|e| Error::storage(format!("Failed to deserialize a staged row: {e}")))?;
            tuple.row_id = Some(row_id);
            st.base_shape.apply(&mut tuple);
            apply_steps(&st.steps, &mut tuple, st.own_per_row.get(&key))?;
            rows.push(tuple);
        }
        Ok(rows)
    }

    /// Give the UNIQUE index `index_name`, just built over the committed rows, the
    /// keys of the rows the statement's transaction inserted. A duplicate among
    /// them, or against a committed row, is the error (23505). On success the
    /// transaction's undo log takes them out again on ROLLBACK, once the
    /// statement's group is written.
    pub(super) fn alter_index_own_inserted_rows<'e>(
        &'e self,
        st: &mut AlterStatement<'e>,
        index_name: &str,
        constraint: &str,
    ) -> Result<()> {
        let Some(txn) = st.own_txn() else {
            return Ok(());
        };
        let staged: Vec<StagedIndexRow> = self
            .alter_own_inserted_rows(st)?
            .into_iter()
            .filter_map(|tuple| {
                tuple.row_id.map(|row_id| StagedIndexRow {
                    row_id,
                    committed: None,
                    staged: Some(tuple),
                })
            })
            .collect();
        if staged.is_empty() {
            return Ok(());
        }
        let keys = self
            .storage
            .art_indexes()
            .apply_staged_rows_to_index(index_name, &st.schema, &staged)
            .map_err(|e| {
                Error::constraint_violation(format!(
                    "could not create unique constraint \"{}\" on relation \"{}\": {}",
                    constraint, st.table, e
                ))
            })?;
        let index_name = index_name.to_string();
        st.publish_after.push(Box::new(move || {
            self.push_art_undo_first(txn, crate::ArtUndoOp::StagedRowsInIndex { index_name, rows: keys });
        }));
        Ok(())
    }
}
