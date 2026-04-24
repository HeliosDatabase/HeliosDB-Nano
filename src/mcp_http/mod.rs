//! MCP endpoint phase 4 MVP (FR 5).
//!
//! A thin JSON-RPC 2.0 surface over the already-working tool handlers
//! in `src/mcp_extensions/`. Wraps `tools/list`, `tools/call`, and the
//! minimal `initialize` handshake so an MCP-capable agent (Claude
//! Code, Cursor, etc.) can call HeliosDB-Nano with no wrapper
//! process.
//!
//! Out of scope for the MVP (tracked as follow-ups):
//! - WebSocket / SSE framing (HTTP JSON-RPC only for now)
//! - Repairing legacy `src/mcp/` module (BLOCKER_mcp_legacy.md still
//!   accurate — we deliberately avoid it here)
//! - Macro-driven auto-registration of `lsp_*` / `graph_rag_*` as MCP
//!   tools (separate pass)
//! - Auth middleware wiring — the handler is auth-agnostic; plug it
//!   into the existing Axum auth chain at mount time
//!
//! Gated on feature `mcp-endpoint`. Embedded-only callers never
//! compile this module.

pub mod rpc;

pub use rpc::{handle_rpc, RpcRequest, RpcResponse};
