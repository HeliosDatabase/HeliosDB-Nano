//! AI module for HeliosDB-Lite
//!
//! Provides AI-native features including:
//! - Natural language query processing
//! - Pluggable LLM providers
//! - RAG pipeline
//! - Semantic search
//! - Query validation and sandboxing

pub mod providers;
pub mod nl_query;
pub mod rag;
pub mod semantic;
pub mod sandbox;

pub use providers::{LlmProvider, LlmProviderConfig, LlmResponse};
pub use nl_query::{NlQueryEngine, NlQueryRequest, NlQueryResponse};
pub use rag::{RagPipeline, RagConfig, RagResponse};
pub use semantic::{SemanticSearch, SemanticSearchConfig};
pub use sandbox::{QuerySandbox, SandboxConfig, SandboxResult};
