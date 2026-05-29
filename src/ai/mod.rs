//! AI module for HeliosDB-Lite
//!
//! Provides AI-native features including:
//! - Natural language query processing
//! - Pluggable LLM providers
//! - RAG pipeline
//! - Semantic search
//! - Query validation and sandboxing

pub mod nl_query;
pub mod providers;
pub mod rag;
pub mod sandbox;
pub mod semantic;

pub use nl_query::{NlQueryEngine, NlQueryRequest, NlQueryResponse};
pub use providers::{LlmProvider, LlmProviderConfig, LlmResponse};
pub use rag::{RagConfig, RagPipeline, RagResponse};
pub use sandbox::{QuerySandbox, SandboxConfig, SandboxResult};
pub use semantic::{SemanticSearch, SemanticSearchConfig};
