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
pub mod sql_rewrite;
pub mod git_hook;
pub mod diff;
pub mod semantic_merkle;
pub mod refactor;

pub use semantic_merkle::{build_or_refresh as merkle_refresh, MerkleStats};
pub use refactor::{rename_apply, RenameApplyOptions, RenameApplyStats};

pub use diff::{
    ast_diff, lsp_body_diff, lsp_references_diff, AsOfRef, AstDiffRow, BodyDiffLine,
    BodyOp, DiffChange, RefDiffRow,
};
pub use embed::{Embedder, NoopEmbedder, HttpEmbedder};
pub use lsp::{
    lsp_call_hierarchy, lsp_definition, lsp_hover, lsp_references, CallDirection,
    CallHierarchyRow, DefinitionHint, DefinitionRow, HoverRow, ReferenceRow,
};
pub use sql_rewrite::{
    detect_create_ast_index, detect_create_semantic_hash_index, detect_pause_resume,
    rewrite_lsp_calls, rewrite_lsp_calls_full, AstIndexDdl, LspRewrite, PauseResume,
    SemanticHashIndexDdl,
};
pub use storage::{
    code_index_with_embedder, register_ast_index, AstIndexMeta, CodeIndexOptions,
    CodeIndexStats, SupportedLanguage,
};
pub use symbols::{
    register_extractor, registered_extractor, registered_extractors, unregister_extractor,
    StaticLanguageExtractor, Symbol, SymbolExtractor, SymbolKind, SymbolRef, SymbolRefKind,
};
