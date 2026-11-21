//! The access-inquiry functions: `has_table_privilege` and its siblings, and
//! `pg_has_role`. They answer from the same gate that admits statements, so
//! what they say is what a statement would be told.

use std::sync::Arc;

use crate::storage::StorageEngine;
use crate::{Error, Result, Value};

use super::access::{current_principal, Principal};
use super::catalog::{DATABASE, FUNCTION, SCHEMA, SEQUENCE, TABLE};
use super::gate::{self, Gate, RequiredColumns};

/// True for a function name this module answers.
pub fn is_access_function(name: &str) -> bool {
    matches!(
        name,
        "has_table_privilege"
            | "has_any_column_privilege"
            | "has_column_privilege"
            | "has_schema_privilege"
            | "has_sequence_privilege"
            | "has_function_privilege"
            | "has_database_privilege"
            | "pg_has_role"
    )
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// An object name as SQL would read it: unquoted names fold to lower case,
/// and a leading `public.` is dropped.
fn object_key(written: &str) -> String {
    let parts: Vec<String> = written
        .split('.')
        .map(|part| {
            let part = part.trim();
            if part.len() >= 2 && part.starts_with('"') && part.ends_with('"') {
                part.trim_matches('"').to_string()
            } else {
                part.to_lowercase()
            }
        })
        .collect();
    match parts.as_slice() {
        [schema, rest @ ..] if schema == "public" && !rest.is_empty() => rest.join("."),
        _ => parts.join("."),
    }
}

/// One privilege of a comma-separated list, with whether the grant option was asked for.
fn privilege_words(list: &str) -> Vec<(String, bool)> {
    list.split(',')
        .map(|item| {
            let upper = item.trim().to_ascii_uppercase();
            match upper.strip_suffix("WITH GRANT OPTION") {
                Some(name) => (name.trim().to_string(), true),
                None => (upper, false),
            }
        })
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

fn gate_for<'a>(storage: &'a StorageEngine, role: Option<&str>) -> Result<Option<Gate<'a>>> {
    match role {
        None => {
            let principal = current_principal(storage);
            if principal == Principal::Unrestricted {
                return Ok(None);
            }
            Gate::for_principal(storage, principal).map(Some)
        }
        Some(role) => {
            let gate = Gate::for_principal(
                storage,
                Principal::Role {
                    name: Arc::from(role),
                    operator: false,
                },
            )?;
            if role != "public" && !gate.snapshot().role_exists(role) {
                return Err(super::catalog::no_such_role(role));
            }
            Ok(Some(gate))
        }
    }
}

fn object_answer(gate: &Gate<'_>, kind: &str, object: &str, privilege: &str, grantable: bool) -> bool {
    if gate.is_superuser() || gate.owns(kind, object) {
        return true;
    }
    if !grantable {
        return gate.has_object_privilege(kind, object, privilege);
    }
    gate.snapshot().object_grant(kind, object, privilege, gate.identities()) == Some(true)
}

fn relation_answer(gate: &Gate<'_>, relation: &str, privilege: &str, columns: &RequiredColumns, grantable: bool) -> bool {
    if gate.is_superuser() || gate.owns(TABLE, relation) {
        return true;
    }
    if grantable {
        return gate
            .snapshot()
            .object_grant(TABLE, relation, privilege, gate.identities())
            == Some(true);
    }
    gate.relation_privilege(relation, privilege, columns)
}

/// Evaluate one access-inquiry function. NULL for a NULL argument, as in
/// PostgreSQL.
pub fn evaluate(name: &str, args: &[Value]) -> Result<Value> {
    let texts: Vec<Option<String>> = args.iter().map(text).collect();
    if texts.iter().any(Option::is_none) {
        return Ok(Value::Null);
    }
    let texts: Vec<String> = texts.into_iter().flatten().collect();
    let expected = if name == "has_column_privilege" { 3 } else { 2 };
    let (role, rest) = if texts.len() == expected + 1 {
        (texts.first().map(String::as_str), texts.get(1..).unwrap_or(&[]))
    } else if texts.len() == expected {
        (None, texts.as_slice())
    } else {
        return Err(Error::query_execution(format!(
            "function {name} takes {expected} or {} arguments",
            expected + 1
        )));
    };
    let Some(storage) = crate::sql::sequences::current_engine() else {
        // No database behind this evaluation: nothing restricts it.
        return Ok(Value::Boolean(true));
    };
    let Some(gate) = gate_for(&storage, role)? else {
        return Ok(Value::Boolean(true));
    };
    let first = rest.first().map(String::as_str).unwrap_or_default();
    let last = rest.last().map(String::as_str).unwrap_or_default();
    if name == "pg_has_role" {
        let own = gate.principal().name().unwrap_or("").to_string();
        let target = object_key(first);
        let mode = last.trim().to_ascii_uppercase();
        let held = if mode.starts_with("USAGE") {
            gate.snapshot().identities(&own)
        } else {
            gate.snapshot().member_closure(&own)
        };
        return Ok(Value::Boolean(
            gate.is_superuser() || held.iter().any(|role| role == &target),
        ));
    }
    let object = object_key(first);
    let mut answer = false;
    for (privilege, grantable) in privilege_words(last) {
        let privilege = if privilege == "TEMP" { "TEMPORARY".to_string() } else { privilege };
        let kind = match name {
            "has_schema_privilege" => SCHEMA,
            "has_sequence_privilege" => SEQUENCE,
            "has_function_privilege" => FUNCTION,
            "has_database_privilege" => DATABASE,
            _ => TABLE,
        };
        if !super::catalog::privileges_of(kind).contains(&privilege.as_str()) {
            return Err(Error::query_execution(format!(
                "unrecognized privilege type: \"{privilege}\""
            )));
        }
        answer |= match name {
            "has_table_privilege" => relation_answer(&gate, &object, &privilege, &RequiredColumns::TableWide, grantable),
            "has_any_column_privilege" => {
                relation_answer(&gate, &object, &privilege, &RequiredColumns::AnyColumn, grantable)
            }
            "has_column_privilege" => {
                let column = rest.get(1).map(String::as_str).unwrap_or_default();
                let mut columns = std::collections::BTreeSet::new();
                columns.insert(column.trim_matches('"').to_ascii_lowercase());
                relation_answer(&gate, &object, &privilege, &RequiredColumns::Named(columns), grantable)
            }
            "has_function_privilege" => {
                let bare = object.split('(').next().unwrap_or(&object);
                object_answer(&gate, kind, &gate::routine_key(bare), &privilege, grantable)
            }
            _ => object_answer(&gate, kind, &object, &privilege, grantable),
        };
    }
    Ok(Value::Boolean(answer))
}
