//! Code-graph track — AST-aware indexing and LSP-shaped queries.
//!
//! Enabled via the `code-graph` feature flag. Phase 1 scope:
//!
//! - Tree-sitter-backed parsing for a small set of languages (Rust,
//!   Python in MVP; more in phase 2).
//! - Symbol extraction per language (`functions`, `classes`,
//!   `methods`, `structs`, `traits`, `types`).
//! - Flat-prefixed tables the engine treats as plain user tables:
//!   `_hdb_code_files`, `_hdb_code_ast_nodes`, `_hdb_code_symbols`,
//!   `_hdb_code_symbol_refs`.
//! - Rust-level API on `EmbeddedDatabase` (`code_index`,
//!   `lsp_definition`, `lsp_references`, `lsp_call_hierarchy`,
//!   `lsp_hover`).
//! - Optional external HTTP embedder; default is no-op (writes
//!   NULL `body_vec`, BM25 / hybrid retrieval still works).
//!
//! Out of scope for phase 1 (tracked in the plan):
//! - `CREATE EXTENSION hdb_code` DDL
//! - `CREATE AST INDEX ... USING tree_sitter(...)` DDL
//! - Real `_hdb_code.schema` namespacing
//! - Temporal / branch `AS OF` variants (phase 2 = FR 3)
//! - Semantic-Merkle subtree hashing (phase 3)

pub mod parse;
pub mod symbols;
pub mod resolver;
pub mod storage;
pub mod lsp;
pub mod embed;

pub use embed::{Embedder, NoopEmbedder, HttpEmbedder};
pub use lsp::{DefinitionHint, DefinitionRow, ReferenceRow, CallHierarchyRow, HoverRow};
pub use storage::{CodeIndexOptions, CodeIndexStats, SupportedLanguage};
pub use symbols::{Symbol, SymbolKind, SymbolRef, SymbolRefKind};
