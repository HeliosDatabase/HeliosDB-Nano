//! The catalog views: `pg_policies`, `pg_policy`, and the row-security flags
//! of `pg_class`. Everything is read from the storage engine, so the views
//! need nothing but the engine their producers already get.

use std::collections::HashMap;

use super::record::TableSecurity;
use super::store;
use crate::storage::StorageEngine;
use crate::{Result, Tuple, Value};

fn text(value: Option<&str>) -> Value {
    match value {
        Some(value) => Value::String(value.to_string()),
        None => Value::Null,
    }
}

/// `{a,b}` as PostgreSQL prints a `name[]`; no role means `{public}`.
fn roles(roles: &[String]) -> String {
    if roles.is_empty() {
        "{public}".to_string()
    } else {
        format!("{{{}}}", roles.join(","))
    }
}

/// The switches of every table that has any, by table name.
pub fn table_switches(storage: &StorageEngine) -> Result<HashMap<String, TableSecurity>> {
    Ok(store::read_tables(storage)?
        .into_iter()
        .map(|(name, entry)| (name, entry.security))
        .collect())
}

/// Rows of `pg_policies`: schemaname, tablename, policyname, permissive,
/// roles, cmd, qual, with_check.
pub fn pg_policies_rows(storage: &StorageEngine) -> Result<Vec<Tuple>> {
    let mut tables: Vec<_> = store::read_tables(storage)?.into_values().collect();
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    let mut rows = Vec::new();
    for table in tables {
        let (schema, name) = crate::sql::Planner::split_schema_key(&table.name);
        for policy in &table.policies {
            rows.push(Tuple::new(vec![
                Value::String(schema.clone()),
                Value::String(name.clone()),
                Value::String(policy.name.clone()),
                Value::String(if policy.permissive { "PERMISSIVE" } else { "RESTRICTIVE" }.to_string()),
                Value::String(roles(&policy.roles)),
                Value::String(policy.command.keyword().to_string()),
                text(policy.using.as_deref()),
                text(policy.with_check.as_deref()),
            ]));
        }
    }
    Ok(rows)
}

/// Rows of `pg_policy`: oid, polname, polrelid, polcmd, polpermissive,
/// polroles, polqual, polwithcheck. `table_oid` maps a table name to its
/// `pg_class.oid`.
pub fn pg_policy_rows(storage: &StorageEngine, table_oid: &dyn Fn(&str) -> Option<i32>) -> Result<Vec<Tuple>> {
    let mut tables: Vec<_> = store::read_tables(storage)?.into_values().collect();
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    let mut rows = Vec::new();
    for table in tables {
        let relid = table_oid(&table.name).map_or(Value::Null, Value::Int4);
        for policy in &table.policies {
            rows.push(Tuple::new(vec![
                Value::Int4(i32::try_from(policy.oid).unwrap_or(i32::MAX)),
                Value::String(policy.name.clone()),
                relid.clone(),
                Value::String(policy.command.letter().to_string()),
                Value::Boolean(policy.permissive),
                Value::String(roles(&policy.roles)),
                text(policy.using.as_deref()),
                text(policy.with_check.as_deref()),
            ]));
        }
    }
    Ok(rows)
}
