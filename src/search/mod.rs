//! Full-text + hybrid search.
//!
//! HelixDB-inspired (idea 2 of the integration plan).
//!
//! ## Modules
//!
//! - [`tokenizer`] -- Unicode-aware word splitting + lowercasing
//! - [`bm25`]      -- inverted-index BM25 scorer
//! - [`reranker`]  -- Reciprocal Rank Fusion + Maximal Marginal Relevance
//! - [`hybrid`]    -- glue that fuses BM25 + vector results
//!
//! ## Persistence
//!
//! As with the graph module (idea 1), the BM25 index is in-memory --
//! the existing `StorageEngine` does not expose column families and
//! shoehorning a CF-per-index layout into the single-DB engine is
//! out of scope for this drop. Indexes are rebuilt on startup from
//! the source table; persistence is left as a follow-up.

pub mod bm25;
pub mod hybrid;
pub mod reranker;
pub mod tokenizer;

pub use bm25::{Bm25Index, Bm25Params, Bm25Score};
pub use hybrid::{hybrid_search, FusionMethod, HybridHit, ScoredHit};
pub use reranker::{mmr_rerank, rrf_fuse, RrfParams};
pub use tokenizer::tokenize;
