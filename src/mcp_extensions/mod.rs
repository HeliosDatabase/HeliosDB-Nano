//! HelixDB-inspired MCP tool handlers (idea 5).
//!
//! These handlers are designed to be folded into the existing
//! `crate::mcp` server once that module's API drift against
//! `EmbeddedDatabase` is repaired (see `BLOCKER_idea_5.md`). Until
//! then they live as a standalone, fully-tested unit so callers that
//! want to build their own MCP transport (HTTP, stdio, in-process)
//! can call them directly.
//!
//! ## New tools
//!
//! - `heliosdb_bm25_index`     -- create/replace an in-memory BM25 index
//! - `heliosdb_hybrid_search`  -- BM25 + vector fusion via RRF/MMR/Linear
//! - `heliosdb_graph_add_edge` -- add an edge to the in-process graph
//! - `heliosdb_graph_traverse` -- BFS traversal
//! - `heliosdb_graph_path`     -- shortest path (BFS, Dijkstra, bidi)
//! - `heliosdb_embed_and_store`-- index in BM25 + optional SQL insert
//!
//! ## New resources
//!
//! - `heliosdb://schema/{table}` -- per-table schema introspection
//! - `heliosdb://stats/{table}`  -- per-table row-count stats

pub mod resources;
pub mod tools;

pub use resources::{read_resource, ResourcePayload};
pub use tools::{call_tool, list_tools, ToolDescriptor, ToolOutcome};
