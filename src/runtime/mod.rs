//! Per-request runtime utilities.
//!
//! This module hosts cross-cutting helpers that live for the lifetime of a
//! single query / request and are torn down in one shot when the request
//! completes. The flagship helper is [`arena::RequestArena`] -- a thin
//! wrapper around [`bumpalo::Bump`] which lets transient buffers (HNSW
//! candidate lists, scratch row vectors, BM25 term lists, ...) be allocated
//! without touching the global allocator on every step.
//!
//! RAG-native (see external-project_INTEGRATION_PLAN idea 3).

pub mod arena;

pub use arena::RequestArena;
