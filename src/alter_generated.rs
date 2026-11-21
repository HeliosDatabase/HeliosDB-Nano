//! Generated columns in `ALTER TABLE`: `ADD COLUMN … GENERATED ALWAYS AS (expr)
//! STORED` computes the value of every row that exists, in the statement's one
//! write group; a column a generated column reads cannot be dropped or renamed
//! under it. See `generated_columns` for how a generated column is recorded.

use std::sync::Arc;

use super::{AlterStatement, RowStep};
use crate::{sql, EmbeddedDatabase, Error, Result, Value};

impl EmbeddedDatabase {
    /// A column a generated column reads cannot be dropped or renamed under it:
    /// the generation expression names it.
    pub(super) fn alter_refuse_generated_readers(st: &AlterStatement<'_>, column_name: &str, verb: &str) -> Result<()> {
        for other in &st.schema.columns {
            if other.name != column_name
                && crate::generated_columns::generated_reads(other.default_expr.as_deref(), column_name)
            {
                return Err(Error::query_execution(format!(
                    "cannot {verb} column \"{}\" of relation \"{}\" because other objects depend on it: generated \
                     column \"{}\". Drop that column first.",
                    column_name, st.table, other.name
                )));
            }
        }
        Ok(())
    }

    /// `ADD COLUMN … GENERATED ALWAYS AS (expr) STORED`: the value of every row
    /// that exists, computed from the row as the statement has it, and written with
    /// the column in the statement's group.
    pub(super) fn alter_add_generated_column(
        &self,
        st: &mut AlterStatement<'_>,
        column_def: &sql::logical_plan::ColumnDef,
        expr: &sql::LogicalExpr,
    ) -> Result<()> {
        if st.own_rows {
            return Err(Error::ddl_over_staged_rows(
                "ALTER TABLE … ADD COLUMN … GENERATED",
                &st.table,
            ));
        }
        let table = st.table.clone();
        let rows = self.alter_current_rows(st)?;
        let evaluator = sql::Evaluator::new(Arc::new(st.schema.clone()));
        let max_bytes = self.storage.config().storage.max_atomic_rewrite_bytes;
        for row in &rows {
            let row_id = row
                .row_id
                .ok_or_else(|| Error::internal("ADD COLUMN … GENERATED: a stored row has no row id"))?;
            let value = evaluator
                .evaluate(expr, row)
                .and_then(|value| {
                    if matches!(value, Value::Null) || value.data_type() == column_def.data_type {
                        Ok(value)
                    } else {
                        evaluator.cast_value(value, &column_def.data_type)
                    }
                })
                .map_err(|e| {
                    Error::query_execution(format!(
                        "the generated column \"{}\" of relation \"{}\" cannot be computed for row {}: {}",
                        column_def.name, table, row_id, e
                    ))
                })?;
            if column_def.not_null && matches!(value, Value::Null) {
                return Err(Error::add_column_null_on_existing_rows(&table, &column_def.name));
            }
            st.per_row_bytes = st
                .per_row_bytes
                .saturating_add(bincode::serialized_size(row).unwrap_or(0).saturating_mul(2));
            if st.per_row_bytes > max_bytes {
                return Err(Error::atomic_rewrite_too_large(&table, st.per_row_bytes, max_bytes));
            }
            st.per_row.entry(row_id).or_default().push(value);
        }
        st.missing.remove(&column_def.name);
        st.steps.push(RowStep::AppendPerRow);
        st.schema.columns.push(crate::Column {
            name: column_def.name.clone(),
            data_type: column_def.data_type.clone(),
            nullable: !column_def.not_null,
            primary_key: column_def.primary_key,
            source_table: None,
            source_table_name: Some(table),
            default_expr: Self::serialize_default_expr(&column_def.default),
            unique: column_def.unique,
            storage_mode: column_def.storage_mode,
        });
        Ok(())
    }
}
