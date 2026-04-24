//! MCP resource resolvers.
//!
//! Four URI families are supported:
//!
//! * `heliosdb://schema` — list of every user table.
//! * `heliosdb://branches` — every branch known to the database.
//! * `heliosdb://schema/{table}` — column metadata for a single table.
//! * `heliosdb://stats/{table}` — row-count for a single table.
//!
//! Catalog access goes through `db.storage.catalog()` (and
//! `db.storage.list_branches()` for branches) rather than SQL —
//! these are O(1) catalog reads with no parser round-trip and no
//! dependency on `pg_catalog` views.

use serde_json::json;

use crate::{EmbeddedDatabase, Value};

/// Content returned to MCP clients on `resources/read`.
#[derive(Debug, Clone)]
pub struct ResourcePayload {
    pub uri: String,
    pub mime_type: &'static str,
    pub text: String,
}

/// Catalogue of static URIs the server advertises in `resources/list`.
#[must_use]
pub fn list_resources() -> Vec<(String, &'static str, &'static str)> {
    vec![
        (
            "heliosdb://schema".to_string(),
            "Database Schema",
            "Every table in the public schema.",
        ),
        (
            "heliosdb://branches".to_string(),
            "Branches",
            "Every database branch.",
        ),
    ]
}

/// Resolve a resource URI against the supplied database.
///
/// Returns `None` for unrecognised URIs, `Some(Err)` for lookup failures,
/// `Some(Ok(payload))` otherwise.
pub fn read_resource(
    db: &EmbeddedDatabase,
    uri: &str,
) -> Option<Result<ResourcePayload, String>> {
    if uri == "heliosdb://schema" {
        return Some(all_tables(db, uri));
    }
    if uri == "heliosdb://branches" {
        return Some(all_branches(db, uri));
    }
    if let Some(table) = uri.strip_prefix("heliosdb://schema/") {
        return Some(table_schema(db, uri, table));
    }
    if let Some(table) = uri.strip_prefix("heliosdb://stats/") {
        return Some(table_stats(db, uri, table));
    }
    None
}

fn all_tables(db: &EmbeddedDatabase, uri: &str) -> Result<ResourcePayload, String> {
    let names = db
        .storage
        .catalog()
        .list_tables()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|n| !n.starts_with("helios_") && !n.starts_with("mv_"))
        .collect::<Vec<_>>();
    Ok(ResourcePayload {
        uri: uri.to_string(),
        mime_type: "application/json",
        text: serde_json::to_string_pretty(&json!({ "tables": names }))
            .unwrap_or_default(),
    })
}

fn all_branches(db: &EmbeddedDatabase, uri: &str) -> Result<ResourcePayload, String> {
    let names: Vec<String> = db
        .storage
        .list_branches()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|b| b.name)
        .collect();
    Ok(ResourcePayload {
        uri: uri.to_string(),
        mime_type: "application/json",
        text: serde_json::to_string_pretty(&json!({ "branches": names }))
            .unwrap_or_default(),
    })
}

fn table_schema(
    db: &EmbeddedDatabase,
    uri: &str,
    table: &str,
) -> Result<ResourcePayload, String> {
    let schema = db
        .storage
        .catalog()
        .get_table_schema(table)
        .map_err(|e| e.to_string())?;
    let cols: Vec<_> = schema
        .columns
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "data_type": format!("{:?}", c.data_type),
                "nullable": c.nullable,
                "primary_key": c.primary_key,
            })
        })
        .collect();
    Ok(ResourcePayload {
        uri: uri.to_string(),
        mime_type: "application/json",
        text: serde_json::to_string_pretty(&json!({
            "table": table,
            "columns": cols,
        }))
        .unwrap_or_default(),
    })
}

fn table_stats(
    db: &EmbeddedDatabase,
    uri: &str,
    table: &str,
) -> Result<ResourcePayload, String> {
    // COUNT(*) is the cheapest correct answer Nano gives us today.
    // Catalog doesn't carry maintained row counts; an explicit query is
    // accurate and adapts to whatever branch / time-travel scope the
    // session is in.
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let row_count = match db.query(&sql, &[]) {
        Ok(rows) => rows
            .first()
            .and_then(|t| t.values.first())
            .map(|v| match v {
                Value::Int8(n) => *n,
                Value::Int4(n) => i64::from(*n),
                _ => 0,
            })
            .unwrap_or(0),
        Err(e) => return Err(e.to_string()),
    };
    Ok(ResourcePayload {
        uri: uri.to_string(),
        mime_type: "application/json",
        text: serde_json::to_string_pretty(&json!({
            "table": table,
            "row_count": row_count,
        }))
        .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> EmbeddedDatabase {
        EmbeddedDatabase::new_in_memory().expect("in-memory db")
    }

    #[test]
    fn schema_all_resolves() {
        let d = db();
        d.execute("CREATE TABLE t (id INT4)").unwrap();
        let r = read_resource(&d, "heliosdb://schema")
            .expect("matched")
            .expect("ok");
        assert_eq!(r.mime_type, "application/json");
        assert!(r.text.contains("\"t\""));
    }

    #[test]
    fn branches_resolves() {
        let r = read_resource(&db(), "heliosdb://branches")
            .expect("matched")
            .expect("ok");
        assert!(r.text.contains("branches"));
        assert!(r.text.contains("main"));
    }

    #[test]
    fn schema_table_resolves() {
        let d = db();
        d.execute("CREATE TABLE users (id INT4, name TEXT)").unwrap();
        let r = read_resource(&d, "heliosdb://schema/users")
            .expect("matched")
            .expect("ok");
        assert!(r.text.contains("users"));
        assert!(r.text.contains("name"));
    }

    #[test]
    fn stats_table_resolves() {
        let d = db();
        d.execute("CREATE TABLE orders (id INT4)").unwrap();
        d.execute("INSERT INTO orders VALUES (1)").unwrap();
        d.execute("INSERT INTO orders VALUES (2)").unwrap();
        let r = read_resource(&d, "heliosdb://stats/orders")
            .expect("matched")
            .expect("ok");
        assert!(r.text.contains("orders"));
        assert!(r.text.contains("row_count"));
    }

    #[test]
    fn unknown_uri_is_none() {
        assert!(read_resource(&db(), "heliosdb://nope/x").is_none());
        assert!(read_resource(&db(), "https://example.com").is_none());
    }
}
