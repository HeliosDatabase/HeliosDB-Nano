//! `pg_tables`: the user's tables, in PostgreSQL's eight-column shape.

use std::collections::HashSet;

use super::context::DictContext;
use crate::storage::internal_relations::{engine_schema_and_name, is_internal_relation};
use crate::{Result, Tuple, Value};

/// The owner every catalog view reports.
const TABLE_OWNER: &str = "heliosdb";

/// The rows of `pg_tables`, ordered by storage key.
///
/// A table that stores a materialized view's rows, or bookkeeping of an
/// earlier release, is not a row (PostgreSQL lists materialized views in
/// `pg_matviews`, not here). Engine tables are rows, under the schema their
/// dotted spelling uses (`_hdb_code`, `_hdb_graph`).
///
/// `hasindexes` is true when the table has a primary key, a unique column or
/// any index; `hastriggers` when a trigger is defined on it (false when the
/// context has no trigger registry); `hasrules` and `rowsecurity` are false:
/// there are no rules, and row-level security is not a property of a table in
/// this engine's catalog.
pub(crate) fn pg_tables_rows(ctx: &DictContext<'_>) -> Result<Vec<Tuple>> {
    let storage = ctx.storage();
    let catalog = storage.catalog();
    let indexed: HashSet<String> = storage
        .art_indexes()
        .list_indexes()
        .into_iter()
        .map(|(_, table, _, _)| table)
        .chain(
            storage
                .vector_indexes()
                .list_all_metadata()
                .into_iter()
                .map(|metadata| metadata.table_name),
        )
        .collect();
    let with_triggers: HashSet<String> = match ctx.triggers() {
        Some(triggers) => triggers
            .list_all_triggers()?
            .into_iter()
            .map(|trigger| trigger.table_name)
            .collect(),
        None => HashSet::new(),
    };

    let mut rows = Vec::new();
    for table in catalog.list_tables()? {
        if is_internal_relation(&table).is_some_and(|kind| kind.is_hidden_storage()) {
            continue;
        }
        let (schema_name, table_name) = match engine_schema_and_name(&table) {
            Some((schema, bare)) => (schema.to_string(), bare.to_string()),
            None => crate::sql::Planner::split_schema_key(&table),
        };
        let has_indexes = indexed.contains(&table)
            || catalog
                .get_table_schema(&table)
                .is_ok_and(|schema| schema.columns.iter().any(|column| column.primary_key || column.unique));
        rows.push(Tuple::new(vec![
            Value::String(schema_name),                     // schemaname
            Value::String(table_name),                      // tablename
            Value::String(TABLE_OWNER.to_string()),         // tableowner
            Value::Null,                                    // tablespace
            Value::Boolean(has_indexes),                    // hasindexes
            Value::Boolean(false),                          // hasrules
            Value::Boolean(with_triggers.contains(&table)), // hastriggers
            Value::Boolean(false),                          // rowsecurity
        ]));
    }
    Ok(rows)
}
