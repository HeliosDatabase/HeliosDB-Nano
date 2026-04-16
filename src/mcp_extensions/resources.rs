//! Schema + stats resources for the MCP extensions.
//!
//! These are pure-data resolvers -- the caller wires them into whatever
//! MCP transport (stdio JSON-RPC, HTTP, in-process) it actually exposes.

use crate::EmbeddedDatabase;

/// Resource payload returned to MCP clients.
#[derive(Debug, Clone)]
pub struct ResourcePayload {
    pub uri: String,
    pub mime_type: &'static str,
    pub text: String,
}

/// Resolve a resource URI of the form `heliosdb://schema/{table}` or
/// `heliosdb://stats/{table}` against the supplied database.
///
/// Unknown URIs return `None`; lookup failures are surfaced as `Some(Err)`.
pub fn read_resource(_db: &EmbeddedDatabase, uri: &str) -> Option<Result<ResourcePayload, String>> {
    if let Some(table) = uri.strip_prefix("heliosdb://schema/") {
        return Some(Ok(ResourcePayload {
            uri: uri.to_string(),
            mime_type: "application/json",
            text: serde_json::to_string_pretty(&serde_json::json!({
                "table": table,
                "note": "schema introspection -- wire to information_schema once mcp module is reconciled"
            }))
            .unwrap_or_default(),
        }));
    }
    if let Some(table) = uri.strip_prefix("heliosdb://stats/") {
        return Some(Ok(ResourcePayload {
            uri: uri.to_string(),
            mime_type: "application/json",
            text: serde_json::to_string_pretty(&serde_json::json!({
                "table": table,
                "row_count": 0_u64,
                "note": "stats placeholder -- wire to actual catalog once mcp module is reconciled"
            }))
            .unwrap_or_default(),
        }));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> EmbeddedDatabase {
        EmbeddedDatabase::new_in_memory().expect("in-memory db")
    }

    #[test]
    fn schema_uri_resolves() {
        let r = read_resource(&db(), "heliosdb://schema/users")
            .expect("matched")
            .expect("ok");
        assert_eq!(r.mime_type, "application/json");
        assert!(r.text.contains("\"table\""));
        assert!(r.text.contains("users"));
    }

    #[test]
    fn stats_uri_resolves() {
        let r = read_resource(&db(), "heliosdb://stats/orders")
            .expect("matched")
            .expect("ok");
        assert!(r.text.contains("orders"));
        assert!(r.text.contains("row_count"));
    }

    #[test]
    fn unknown_uri_returns_none() {
        assert!(read_resource(&db(), "heliosdb://nope/x").is_none());
        assert!(read_resource(&db(), "https://example.com/").is_none());
    }
}
