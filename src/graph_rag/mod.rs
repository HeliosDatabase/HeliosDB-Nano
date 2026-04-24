//! GraphRAG track (FR 4) — universal cross-modal graph schema
//! (`_hdb_graph_*`) plus a seed → expand → rerank query API.
//!
//! Phase 3 MVP ships the Rust API + tables only. The `WITH CONTEXT`
//! SQL clause, graph-weighted HNSW tie-breaking, and semantic-Merkle
//! invalidation are tracked follow-ups — see
//! `FEATURE_REQUEST_graphrag_with_context.md`.
//!
//! Gated on feature `graph-rag`, which implies `code-graph` (the
//! projection from `_hdb_code_symbols` to `_hdb_graph_nodes` is
//! meaningless without a code-graph source).

pub mod schema;
pub mod search;

pub use schema::{ensure_tables, project_code_symbols, GraphRagStats};
pub use search::{graph_rag_search, Direction, GraphRagHit, GraphRagOptions};
