//! sprinter 6bc4a4040eb7 (phase B2): the executor half of writes on a branch.
//!
//! The storage half is `storage::BranchView` (reads through the overlay chain,
//! nearest level wins; writes as the branch's own images and tombstones;
//! PRIMARY KEY / UNIQUE over the branch's view). These helpers are what both
//! DML executor families — the text family (`execute_in_transaction_inner`)
//! and the params family (`execute_plan_with_params_inner`) — call on a
//! branch, so the two cannot drift: a new row, the `ON CONFLICT` arbiter, the
//! FK actions (`CASCADE` / `SET NULL`) and the "does a matching row exist"
//! probe behind FOREIGN KEY / RESTRICT.
//!
//! What they never do on a branch: touch main's `data:` keys, main's ART or
//! HNSW indexes, main's columnar side data, or the logical WAL (a branch is a
//! local sandbox; a standby serves main).

use crate::{storage, EmbeddedDatabase, Result, Schema, Tuple, Value};

impl EmbeddedDatabase {
    /// INSERT one NEW row on the branch `view`: allocate its row id from the
    /// table's shared counter (ids never collide across levels — the counter
    /// is one per table and persistent), fill a NULL SERIAL / IDENTITY primary
    /// key from it (the same type-gated decision as every main fill site),
    /// then let the view claim, check and write it. With `txn` the row and the
    /// counter are staged in it; without one both are written at once.
    ///
    /// Returns the row id and the row as written (PK filled, `row_id` set) —
    /// what `RETURNING` projects.
    pub(crate) fn branch_insert_row(
        &self,
        view: &storage::BranchView<'_>,
        txn: Option<&storage::Transaction>,
        table_name: &str,
        schema: &Schema,
        mut tuple: Tuple,
    ) -> Result<(u64, Tuple)> {
        let row_id = self.storage.next_row_id_volatile(table_name);
        if let Some(txn) = txn {
            self.storage.stage_row_counter_in_transaction(table_name, row_id, txn)?;
        }
        let mut generated_identity = false;
        for (i, col) in schema.columns.iter().enumerate() {
            crate::note_explicit_identity_pk(&self.storage, table_name, col, tuple.values.get(i));
            if col.primary_key && matches!(tuple.values.get(i), Some(Value::Null)) {
                let filled = crate::pk_autofill_value(table_name, col, row_id)?;
                if let Some(slot) = tuple.values.get_mut(i) {
                    *slot = filled;
                    generated_identity = true;
                }
            }
        }
        if generated_identity {
            crate::note_session_lastval(row_id as i64);
        }
        view.insert_checked(txn, table_name, schema, row_id, &tuple)?;
        if txn.is_none() {
            self.storage.flush_row_counter(table_name)?;
        }
        tuple.row_id = Some(row_id);
        Ok((row_id, tuple))
    }

    /// The row an `ON CONFLICT … DO UPDATE` on a branch updates, from the
    /// row's arbitration (sprinter 6bc4a4040eb7, B2 fix pass): the visible row
    /// holding the arbiter key (any rule's key when the clause has no target).
    ///
    /// A key only CLAIMED by another open transaction on the branch — its
    /// uncommitted insert — is answered the way main answers the same race:
    /// main's eager ART entry names a row that is in neither this
    /// transaction's write set nor committed storage, so main's upsert fails
    /// `ON CONFLICT DO UPDATE: existing row not found in storage` (it does not
    /// wait for the other transaction, as PostgreSQL would). The branch
    /// returns that same error, saying why. No conflict on the arbiter is the
    /// original 23505 (a collision on a constraint the clause did not name) or,
    /// targetless, main's "could not find existing row".
    pub(crate) fn branch_upsert_target(
        arbitration: &storage::BranchArbitration,
        target: Option<&[String]>,
        conflict_message: &str,
    ) -> Result<(u64, Tuple)> {
        match arbitration.conflict_on(target) {
            Some(storage::KeyState::Visible(row)) => {
                let row_id = row
                    .row_id
                    .ok_or_else(|| crate::Error::internal("branch unique conflict without a row id"))?;
                Ok((row_id, row.clone()))
            }
            Some(storage::KeyState::Claimed) => Err(crate::Error::query_execution(format!(
                "ON CONFLICT DO UPDATE: existing row not found in storage (the conflicting key is held by a \
                 concurrent transaction on branch \"{}\")",
                arbitration.branch()
            ))),
            None if target.is_some() => Err(crate::Error::constraint_violation(conflict_message.to_string())),
            None => Err(crate::Error::query_execution(format!(
                "ON CONFLICT DO UPDATE: could not find existing row ({conflict_message})"
            ))),
        }
    }

    /// `ON DELETE CASCADE` on a branch: hide every child row the BRANCH sees
    /// whose `fk_columns` equal `parent_values` (already converted to the
    /// child columns' types by the caller) — tombstones on the branch only,
    /// staged in `txn` when there is one. Main's children, and children the
    /// branch already deleted, are untouched.
    pub(crate) fn branch_cascade_delete(
        view: &storage::BranchView<'_>,
        txn: Option<&storage::Transaction>,
        table_name: &str,
        schema: &Schema,
        fk_columns: &[String],
        parent_values: &[Value],
    ) -> Result<()> {
        for child in view.rows_with_key(txn, table_name, schema, fk_columns, parent_values)? {
            if let Some(row_id) = child.row_id {
                view.delete(txn, table_name, row_id)?;
            }
        }
        Ok(())
    }

    /// `ON DELETE SET NULL` on a branch: the branch's own image of every
    /// child row the branch sees that references `parent_values`, with the FK
    /// columns NULLed.
    pub(crate) fn branch_set_null(
        view: &storage::BranchView<'_>,
        txn: Option<&storage::Transaction>,
        table_name: &str,
        schema: &Schema,
        fk_columns: &[String],
        parent_values: &[Value],
    ) -> Result<()> {
        for child in view.rows_with_key(txn, table_name, schema, fk_columns, parent_values)? {
            let Some(row_id) = child.row_id else {
                continue;
            };
            let mut nulled = child;
            for fk_column in fk_columns {
                if let Some(index) = schema
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(fk_column))
                {
                    if let Some(slot) = nulled.values.get_mut(index) {
                        *slot = Value::Null;
                    }
                }
            }
            view.put(txn, table_name, row_id, &nulled)?;
        }
        Ok(())
    }

    /// INSERT a batch of already-validated rows (NOT NULL / CHECK / FK done by
    /// the caller) on the branch, each through [`Self::branch_insert_row`] —
    /// the branch arm of the transaction batch-insert funnels.
    pub(crate) fn branch_insert_rows(
        &self,
        view: &storage::BranchView<'_>,
        txn: Option<&storage::Transaction>,
        table_name: &str,
        schema: &Schema,
        tuples: Vec<Tuple>,
    ) -> Result<Vec<u64>> {
        let mut ids = Vec::with_capacity(tuples.len());
        for tuple in tuples {
            let (row_id, _) = self.branch_insert_row(view, txn, table_name, schema, tuple)?;
            ids.push(row_id);
        }
        Ok(ids)
    }
}
