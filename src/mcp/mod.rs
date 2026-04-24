//! Model Context Protocol (MCP) for HeliosDB-Nano.
//!
//! Gated on the `mcp-endpoint` feature. Exposes:
//!
//! * JSON-RPC 2.0 dispatcher (`rpc::handle_rpc`, `rpc::handle_rpc_with_db`)
//!   — transport-agnostic, reusable from stdio, HTTP, WebSocket, SSE.
//! * Stdio server (`server::McpServer`) — used by `heliosdb-nano mcp-server`
//!   and by MCP clients that spawn HeliosDB as a subprocess.
//! * Unified tool catalogue (`tools::list_tools` / `tools::call_tool`) —
//!   10 DB-backed tools (query / schema / branch / search / time-travel)
//!   + 6 in-process RAG tools (BM25 index, hybrid search, graph
//!   add-edge / traverse / path, embed-and-store).
//! * Resource catalogue (`resources::read_resource`) — `heliosdb://schema`,
//!   `heliosdb://branches`, `heliosdb://schema/{t}`, `heliosdb://stats/{t}`.
//!
//! Process-wide BM25 / graph state survives across calls via
//! `tools::BM25_INDEXES` and `tools::GRAPH_STORE` (`once_cell::Lazy`),
//! matching the warmth semantics of the earlier `mcp_extensions`
//! module.

pub mod protocol;
pub mod tools;
pub mod resources;
pub mod rpc;
pub mod server;

pub use protocol::{
    Capabilities, InitializeResult, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    Prompt, PromptArgument, PromptContent, PromptMessage, PromptsCapability, Resource,
    ResourceContent, ResourceReference, ResourcesCapability, ServerInfo, Tool, ToolContent,
    ToolResult, ToolsCapability, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST,
    METHOD_NOT_FOUND, PARSE_ERROR,
};
pub use resources::{list_resources, read_resource, ResourcePayload};
pub use rpc::{handle_rpc, handle_rpc_with_db, RpcError, RpcRequest, RpcResponse};
pub use server::McpServer;
pub use tools::{call_tool, list_tools, ToolDescriptor, ToolOutcome, BM25_INDEXES, GRAPH_STORE};
