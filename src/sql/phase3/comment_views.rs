//! `pg_description`: the comments `COMMENT ON` stored, under the object ids the
//! other catalog views give the same objects.
//!
//! A row per comment on a table, a column (`objsubid` = the column's number), an
//! index, a view, a schema and a constraint (`classoid`: `pg_class` 1259,
//! `pg_namespace` 2615, `pg_constraint` 2606). A comment on an object that no
//! catalog view lists (a function, a sequence, an extension, an object that does
//! not exist) is stored and carried by dumps, and has no row here.

use super::{pg_index_oid, pg_table_oid, pg_view_oid, sorted_art_indexes, sorted_views};
use crate::storage::StorageEngine;
use crate::{Result, Tuple, Value};

/// `pg_class`'s own id.
pub(crate) const PG_CLASS_OID: i32 = 1259;
/// `pg_namespace`'s own id.
pub(crate) const PG_NAMESPACE_OID: i32 = 2615;
/// `pg_constraint`'s own id.
pub(crate) const PG_CONSTRAINT_CLASS_OID: i32 = 2606;

/// One row of `pg_description`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Description {
    pub objoid: i32,
    pub classoid: i32,
    pub objsubid: i32,
    pub description: String,
}

/// Every comment that has an object id.
pub(crate) fn descriptions(storage: &StorageEngine) -> Result<Vec<Description>> {
    let catalog = storage.catalog();
    let comments = catalog.list_comments()?;
    if comments.is_empty() {
        return Ok(Vec::new());
    }
    let tables = catalog.list_tables()?;
    let table_oid = |name: &str| tables.iter().position(|table| table == name).map(pg_table_oid);
    let indexes = sorted_art_indexes(storage);
    let views = sorted_views(storage)?;
    let mut out = Vec::new();
    let mut push = |objoid: i32, classoid: i32, objsubid: i32, text: &str| {
        out.push(Description {
            objoid,
            classoid,
            objsubid,
            description: text.to_string(),
        });
    };
    for comment in &comments {
        match comment.kind.as_str() {
            "table" => {
                if let Some(oid) = table_oid(&comment.name) {
                    push(oid, PG_CLASS_OID, 0, &comment.text);
                }
            }
            "column" => {
                let Some((table, column)) = comment.name.split_once(':') else {
                    continue;
                };
                let (Some(oid), Ok(schema)) = (table_oid(table), catalog.get_table_schema(table)) else {
                    continue;
                };
                if let Some(position) = schema.columns.iter().position(|c| c.name == column) {
                    push(oid, PG_CLASS_OID, position as i32 + 1, &comment.text);
                }
            }
            "index" => {
                if let Some(position) = indexes.iter().position(|(name, _, _, _)| name == &comment.name) {
                    push(pg_index_oid(position), PG_CLASS_OID, 0, &comment.text);
                }
            }
            "view" => {
                let position = views.iter().position(|(schema, name, _)| {
                    (comment.name == *name && schema == "public") || comment.name == format!("{schema}.{name}")
                });
                if let Some(position) = position {
                    push(pg_view_oid(position), PG_CLASS_OID, 0, &comment.text);
                }
            }
            "schema" => {
                if super::all_schema_names(storage)?.contains(&comment.name) {
                    push(
                        crate::sql::Planner::schema_name_to_oid(&comment.name),
                        PG_NAMESPACE_OID,
                        0,
                        &comment.text,
                    );
                }
            }
            _ => {}
        }
    }
    // Constraints, by the ids `pg_constraint` gives them.
    let constraint_comments: Vec<_> = comments.iter().filter(|c| c.kind == "constraint").collect();
    if !constraint_comments.is_empty() {
        if let Ok(rows) = super::SystemViewRegistry::execute_pg_constraint(storage) {
            for row in rows {
                let (Some(Value::Int4(oid)), Some(Value::String(name)), Some(Value::Int4(relid))) = (
                    row.values.first(),
                    row.values.get(1),
                    row.values.get(PG_CONSTRAINT_CONRELID_POSITION),
                ) else {
                    continue;
                };
                let Some(table) = tables
                    .iter()
                    .enumerate()
                    .find(|(index, _)| pg_table_oid(*index) == *relid)
                else {
                    continue;
                };
                let key = format!("{}:{}", table.1, name);
                if let Some(comment) = constraint_comments.iter().find(|c| c.name == key) {
                    push(*oid, PG_CONSTRAINT_CLASS_OID, 0, &comment.text);
                }
            }
        }
    }
    Ok(out)
}

/// Where `conrelid` sits in a `pg_constraint` row (`oid`, `conname`,
/// `connamespace`, `contype`, `conrelid`, …).
const PG_CONSTRAINT_CONRELID_POSITION: usize = 4;

/// `pg_description`'s rows.
pub(crate) fn execute_pg_description(storage: &StorageEngine) -> Result<Vec<Tuple>> {
    Ok(descriptions(storage)?
        .into_iter()
        .map(|row| {
            Tuple::new(vec![
                Value::Int4(row.objoid),
                Value::Int4(row.classoid),
                Value::Int4(row.objsubid),
                Value::String(row.description),
            ])
        })
        .collect())
}
