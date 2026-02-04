//! RAG (Retrieval-Augmented Generation) Pipeline
//!
//! Combines vector search with LLM generation for context-aware responses.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::ai::providers::{
    ChatMessage, LlmProvider, LlmRequest, MessageRole, ProviderResult,
};

/// RAG configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagConfig {
    /// Vector stores to search
    pub vector_stores: Vec<String>,
    /// Number of context documents to retrieve
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// Minimum relevance score threshold
    pub min_score: Option<f32>,
    /// Whether to rerank results
    #[serde(default)]
    pub rerank: bool,
    /// Hybrid search alpha (0=keyword, 1=semantic)
    pub alpha: Option<f32>,
    /// Include sources in response
    #[serde(default = "default_true")]
    pub include_sources: bool,
    /// Maximum context tokens
    pub max_context_tokens: Option<usize>,
    /// Custom system prompt
    pub system_prompt: Option<String>,
    /// Chunk overlap strategy
    pub chunk_strategy: Option<ChunkStrategy>,
}

fn default_top_k() -> usize {
    5
}

fn default_true() -> bool {
    true
}

/// Chunk overlap strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChunkStrategy {
    /// Return exact chunks
    Exact,
    /// Expand chunks with surrounding context
    Expand { before: usize, after: usize },
    /// Merge overlapping chunks
    Merge { max_length: usize },
}

/// RAG query request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagRequest {
    /// User query
    pub query: String,
    /// RAG configuration
    pub config: RagConfig,
    /// Conversation history
    pub history: Option<Vec<ChatMessage>>,
    /// Additional context
    pub context: Option<String>,
    /// Output format preferences
    pub format: Option<OutputFormat>,
    /// Model override
    pub model: Option<String>,
    /// Temperature
    pub temperature: Option<f32>,
    /// Maximum response tokens
    pub max_tokens: Option<usize>,
}

/// Output format preferences
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputFormat {
    /// Response style (concise, detailed, bullet_points, json)
    pub style: Option<String>,
    /// Language
    pub language: Option<String>,
    /// Include citations inline
    #[serde(default = "default_true")]
    pub cite_sources: bool,
}

/// RAG response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagResponse {
    /// Generated response
    pub response: String,
    /// Retrieved sources
    pub sources: Vec<RagSource>,
    /// Query understanding
    pub query_analysis: Option<QueryAnalysis>,
    /// Token usage
    pub usage: Option<TokenUsage>,
    /// Confidence score
    pub confidence: f32,
    /// Follow-up questions
    pub follow_ups: Option<Vec<String>>,
}

/// Retrieved source document
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagSource {
    /// Source ID
    pub id: String,
    /// Source content
    pub content: String,
    /// Relevance score
    pub score: f32,
    /// Source metadata
    pub metadata: HashMap<String, serde_json::Value>,
    /// Highlight/snippet
    pub highlight: Option<String>,
    /// Source location (page, paragraph, etc.)
    pub location: Option<String>,
}

/// Query analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryAnalysis {
    /// Identified intent
    pub intent: String,
    /// Extracted entities
    pub entities: Vec<ExtractedEntity>,
    /// Reformulated query
    pub reformulated_query: Option<String>,
    /// Keywords
    pub keywords: Vec<String>,
}

/// Extracted entity
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedEntity {
    pub entity: String,
    pub entity_type: String,
    pub confidence: f32,
}

/// Token usage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
    pub context_tokens: usize,
}

/// RAG Pipeline
pub struct RagPipeline {
    /// LLM provider
    llm: Arc<dyn LlmProvider>,
    /// Default configuration
    default_config: RagConfig,
    /// Custom retriever (would interface with vector store)
    retriever: Option<Arc<dyn Retriever>>,
    /// Reranker
    reranker: Option<Arc<dyn Reranker>>,
}

/// Trait for custom retrievers
#[async_trait::async_trait]
pub trait Retriever: Send + Sync {
    async fn retrieve(
        &self,
        query: &str,
        stores: &[String],
        top_k: usize,
        min_score: Option<f32>,
    ) -> ProviderResult<Vec<RagSource>>;
}

/// Trait for rerankers
#[async_trait::async_trait]
pub trait Reranker: Send + Sync {
    async fn rerank(
        &self,
        query: &str,
        documents: Vec<RagSource>,
        top_k: usize,
    ) -> ProviderResult<Vec<RagSource>>;
}

impl RagPipeline {
    /// Create new RAG pipeline
    pub fn new(llm: Arc<dyn LlmProvider>) -> Self {
        Self {
            llm,
            default_config: RagConfig {
                vector_stores: vec!["default".to_string()],
                top_k: 5,
                min_score: Some(0.7),
                rerank: false,
                alpha: Some(0.5),
                include_sources: true,
                max_context_tokens: Some(4000),
                system_prompt: None,
                chunk_strategy: None,
            },
            retriever: None,
            reranker: None,
        }
    }

    /// Set custom retriever
    pub fn with_retriever(mut self, retriever: Arc<dyn Retriever>) -> Self {
        self.retriever = Some(retriever);
        self
    }

    /// Set reranker
    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = Some(reranker);
        self
    }

    /// Set default config
    pub fn with_config(mut self, config: RagConfig) -> Self {
        self.default_config = config;
        self
    }

    /// Execute RAG query
    pub async fn query(&self, request: RagRequest) -> ProviderResult<RagResponse> {
        let config = request.config.clone();

        // Step 1: Retrieve relevant documents
        let mut sources = self.retrieve(&request.query, &config).await?;

        // Step 2: Rerank if enabled
        if config.rerank {
            if let Some(ref reranker) = self.reranker {
                sources = reranker.rerank(&request.query, sources, config.top_k).await?;
            }
        }

        // Step 3: Build context from sources
        let context = self.build_context(&sources, config.max_context_tokens);

        // Step 4: Generate response
        let response = self.generate(&request, &context, &sources).await?;

        Ok(response)
    }

    /// Retrieve documents
    async fn retrieve(&self, query: &str, config: &RagConfig) -> ProviderResult<Vec<RagSource>> {
        if let Some(ref retriever) = self.retriever {
            retriever.retrieve(
                query,
                &config.vector_stores,
                config.top_k,
                config.min_score,
            ).await
        } else {
            // Default: Return empty (would normally call vector store)
            Ok(Vec::new())
        }
    }

    /// Build context from sources
    fn build_context(&self, sources: &[RagSource], max_tokens: Option<usize>) -> String {
        let max_chars = max_tokens.map(|t| t * 4).unwrap_or(16000);
        let mut context = String::new();
        let mut total_chars = 0;

        for (i, source) in sources.iter().enumerate() {
            let entry = format!(
                "[Source {}]: {}\n\n",
                i + 1,
                source.content
            );

            if total_chars + entry.len() > max_chars {
                break;
            }

            context.push_str(&entry);
            total_chars += entry.len();
        }

        context
    }

    /// Generate response using LLM
    async fn generate(
        &self,
        request: &RagRequest,
        context: &str,
        sources: &[RagSource],
    ) -> ProviderResult<RagResponse> {
        // Build system prompt
        let system_prompt = request.config.system_prompt.clone().unwrap_or_else(|| {
            format!(
                r#"You are a helpful assistant that answers questions based on the provided context.

Guidelines:
1. Answer based primarily on the provided context
2. If the context doesn't contain relevant information, say so clearly
3. Cite sources using [Source N] format when making claims
4. Be concise but thorough
5. If asked for opinions, clarify that you're an AI and provide balanced perspectives

Context:
{}

Answer the user's question based on the above context."#,
                context
            )
        });

        // Build messages
        let mut messages = vec![ChatMessage {
            role: MessageRole::System,
            content: system_prompt,
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        }];

        // Add conversation history
        if let Some(ref history) = request.history {
            messages.extend(history.clone());
        }

        // Add current query
        messages.push(ChatMessage {
            role: MessageRole::User,
            content: request.query.clone(),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        });

        // Create LLM request
        let llm_request = LlmRequest {
            messages,
            model: request.model.clone(),
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            ..Default::default()
        };

        // Call LLM
        let response = self.llm.chat(llm_request).await?;
        let response_content = response.message.content;

        // Build response
        Ok(RagResponse {
            follow_ups: self.generate_follow_ups(&request.query, &response_content),
            response: response_content,
            sources: if request.config.include_sources {
                sources.to_vec()
            } else {
                Vec::new()
            },
            query_analysis: None, // Would be populated by query analysis step
            usage: response.usage.map(|u| TokenUsage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
                context_tokens: context.len() / 4, // Approximate
            }),
            confidence: self.calculate_confidence(sources),
        })
    }

    /// Calculate confidence based on source quality
    fn calculate_confidence(&self, sources: &[RagSource]) -> f32 {
        if sources.is_empty() {
            return 0.3;
        }

        let avg_score: f32 = sources.iter().map(|s| s.score).sum::<f32>() / sources.len() as f32;
        let source_count_factor = (sources.len() as f32 / 5.0).min(1.0);

        (avg_score * 0.7 + source_count_factor * 0.3).min(1.0)
    }

    /// Generate follow-up questions
    fn generate_follow_ups(&self, query: &str, response: &str) -> Option<Vec<String>> {
        // Simple heuristic-based follow-ups
        let mut follow_ups = Vec::new();

        if response.contains("however") || response.contains("but") {
            follow_ups.push("Can you elaborate on the exceptions mentioned?".to_string());
        }

        if response.contains("example") || response.contains("such as") {
            follow_ups.push("Can you provide more examples?".to_string());
        }

        if query.contains("how") {
            follow_ups.push("What are the prerequisites for this?".to_string());
        }

        if query.contains("why") {
            follow_ups.push("Are there any alternative explanations?".to_string());
        }

        if follow_ups.is_empty() {
            None
        } else {
            Some(follow_ups)
        }
    }
}

impl Default for RagConfig {
    fn default() -> Self {
        Self {
            vector_stores: vec!["default".to_string()],
            top_k: 5,
            min_score: Some(0.7),
            rerank: false,
            alpha: Some(0.5),
            include_sources: true,
            max_context_tokens: Some(4000),
            system_prompt: None,
            chunk_strategy: None,
        }
    }
}
