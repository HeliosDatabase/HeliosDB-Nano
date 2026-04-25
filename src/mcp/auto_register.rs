//! Auto-registration surface for MCP tools.
//!
//! Lets any `#[cfg(feature = "mcp-endpoint")]` module submit a
//! [`McpExtensionTool`] entry into a process-wide inventory; the
//! main [`super::tools::list_tools`] / [`super::tools::call_tool`]
//! dispatcher consults the inventory alongside the hand-rolled
//! catalogue.
//!
//! The point is to satisfy FR 5's "every new SQL/Rust function
//! auto-exposes as an MCP tool" promise without forcing every new
//! function to also touch `tools.rs`. Each module that owns a
//! function can declare its MCP-tool surface in-place via the
//! [`mcp_tool!`] convenience macro.
//!
//! Implementation note: Rust doesn't have function-attribute proc
//! macros without a separate proc-macro crate, so the macro is
//! declarative (`macro_rules!`) and wraps `inventory::submit!`. The
//! ergonomic shape is the same as a `#[mcp_tool]` attribute would be.

use serde_json::Value as JsonValue;

use crate::EmbeddedDatabase;

use super::tools::ToolOutcome;

/// Function pointer for an inventory-registered MCP tool handler.
///
/// `db` may be `None` for transports that don't thread a database
/// reference (e.g. some HTTP smoke probes). Handlers that need DB
/// access should return `ToolOutcome::err(...)` in that case rather
/// than panic.
pub type McpToolHandler = fn(db: Option<&EmbeddedDatabase>, args: JsonValue) -> ToolOutcome;

/// Schema-builder thunk so we don't need to hold a `serde_json::Value`
/// in a `static` (which would require interior mutability or `Lazy`).
pub type McpToolSchema = fn() -> JsonValue;

/// One inventory entry — name + description + schema thunk + handler.
pub struct McpExtensionTool {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: McpToolSchema,
    pub handler: McpToolHandler,
}

inventory::collect!(McpExtensionTool);

/// Iterate every inventory-registered tool. Order is unspecified.
pub fn registered() -> impl Iterator<Item = &'static McpExtensionTool> {
    inventory::iter::<McpExtensionTool>.into_iter()
}

/// Dispatch an inventory-registered tool by name. Returns `None` if
/// the name isn't in the inventory so the caller can fall through to
/// the hand-rolled catalogue.
pub fn try_call(
    db: Option<&EmbeddedDatabase>,
    name: &str,
    args: JsonValue,
) -> Option<ToolOutcome> {
    for entry in registered() {
        if entry.name == name {
            return Some((entry.handler)(db, args));
        }
    }
    None
}

/// Declarative wrapper around `inventory::submit!` so call sites
/// don't need to depend on `inventory` directly.
///
/// ```ignore
/// mcp_tool! {
///     name: "helios_lsp_definition",
///     description: "Locate where a symbol is defined.",
///     schema: || serde_json::json!({ "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] }),
///     handler: |db, args| { /* ... */ },
/// }
/// ```
#[macro_export]
macro_rules! mcp_tool {
    (
        name: $name:literal,
        description: $desc:literal,
        schema: $schema:expr,
        handler: $handler:expr $(,)?
    ) => {
        inventory::submit! {
            $crate::mcp::auto_register::McpExtensionTool {
                name: $name,
                description: $desc,
                schema: $schema,
                handler: $handler,
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dummy(_: Option<&EmbeddedDatabase>, _: JsonValue) -> ToolOutcome {
        ToolOutcome::ok(json!({ "ok": true }))
    }

    inventory::submit! {
        McpExtensionTool {
            name: "helios_test_dummy",
            description: "test fixture",
            schema: || json!({ "type": "object", "properties": {} }),
            handler: dummy,
        }
    }

    #[test]
    fn inventory_includes_dummy() {
        let names: Vec<_> = registered().map(|t| t.name).collect();
        assert!(names.contains(&"helios_test_dummy"), "have: {names:?}");
    }

    #[test]
    fn try_call_dispatches() {
        let r = try_call(None, "helios_test_dummy", json!({})).expect("matched");
        assert!(!r.is_error);
    }

    #[test]
    fn try_call_misses_unknown() {
        assert!(try_call(None, "definitely_not_a_tool", json!({})).is_none());
    }
}
