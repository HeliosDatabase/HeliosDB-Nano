//! Pluggable LLM providers for HeliosDB-Lite
//!
//! Supports multiple LLM providers:
//! - OpenAI (GPT-4, GPT-3.5)
//! - Anthropic (Claude)
//! - Azure OpenAI
//! - Google (Gemini)
//! - Local models (Ollama, llama.cpp)
//! - Custom providers via trait implementation

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

mod openai;
mod anthropic;
mod azure;
mod ollama;
mod google;

pub use openai::OpenAiProvider;
pub use anthropic::AnthropicProvider;
pub use azure::AzureOpenAiProvider;
pub use ollama::OllamaProvider;
pub use google::GoogleProvider;

/// LLM provider error
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("API error: {0}")]
    Api(String),
    #[error("Rate limit exceeded")]
    RateLimit,
    #[error("Invalid configuration: {0}")]
    Config(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("Unsupported model: {0}")]
    UnsupportedModel(String),
    #[error("Token limit exceeded: {0}")]
    TokenLimit(String),
}

/// Result type for provider operations
pub type ProviderResult<T> = Result<T, ProviderError>;

/// LLM message role
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Function,
    Tool,
}

/// Chat message for LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Function call in response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Tool call in response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

/// Function/tool definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

/// Tool definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDef,
}

/// LLM request parameters
#[derive(Debug, Clone, Default)]
pub struct LlmRequest {
    pub messages: Vec<ChatMessage>,
    pub model: Option<String>,
    pub max_tokens: Option<usize>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop: Option<Vec<String>>,
    pub tools: Option<Vec<ToolDef>>,
    pub tool_choice: Option<serde_json::Value>,
    pub stream: bool,
}

/// LLM response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub id: String,
    pub model: String,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
    pub usage: Option<TokenUsage>,
}

/// Token usage statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
}

/// Streaming chunk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    pub id: String,
    pub delta: ChatDelta,
    pub finish_reason: Option<String>,
}

/// Delta content in stream
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatDelta {
    pub role: Option<MessageRole>,
    pub content: Option<String>,
    pub function_call: Option<FunctionCallDelta>,
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// Function call delta
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// Tool call delta
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub call_type: Option<String>,
    pub function: Option<FunctionCallDelta>,
}

/// Provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmProviderConfig {
    /// Provider type
    pub provider: String,
    /// API key (or path to key file)
    pub api_key: Option<String>,
    /// API endpoint (for custom/local providers)
    pub endpoint: Option<String>,
    /// Default model
    pub model: Option<String>,
    /// Organization ID (for OpenAI)
    pub organization: Option<String>,
    /// Azure deployment name
    pub deployment: Option<String>,
    /// Azure API version
    pub api_version: Option<String>,
    /// Connection timeout (ms)
    pub timeout_ms: Option<u64>,
    /// Max retries
    pub max_retries: Option<usize>,
    /// Custom headers
    pub headers: Option<HashMap<String, String>>,
}

/// Trait for LLM providers
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Provider name
    fn name(&self) -> &str;

    /// List available models
    async fn list_models(&self) -> ProviderResult<Vec<ModelInfo>>;

    /// Chat completion
    async fn chat(&self, request: LlmRequest) -> ProviderResult<LlmResponse>;

    /// Streaming chat completion
    async fn chat_stream(
        &self,
        request: LlmRequest,
    ) -> ProviderResult<Box<dyn futures::Stream<Item = ProviderResult<StreamChunk>> + Send + Unpin>>;

    /// Count tokens for text
    fn count_tokens(&self, text: &str, model: &str) -> ProviderResult<usize>;

    /// Check if model is supported
    fn supports_model(&self, model: &str) -> bool;

    /// Get model info
    fn model_info(&self, model: &str) -> Option<ModelInfo>;
}

/// Model information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub context_length: usize,
    pub supports_functions: bool,
    pub supports_vision: bool,
    pub input_cost_per_1k: Option<f64>,
    pub output_cost_per_1k: Option<f64>,
}

/// Provider registry
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    default_provider: Option<String>,
}

impl ProviderRegistry {
    /// Create new registry
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            default_provider: None,
        }
    }

    /// Register a provider
    pub fn register(&mut self, name: &str, provider: Arc<dyn LlmProvider>) {
        self.providers.insert(name.to_string(), provider);
    }

    /// Set default provider
    pub fn set_default(&mut self, name: &str) -> bool {
        if self.providers.contains_key(name) {
            self.default_provider = Some(name.to_string());
            true
        } else {
            false
        }
    }

    /// Get provider by name
    pub fn get(&self, name: &str) -> Option<Arc<dyn LlmProvider>> {
        self.providers.get(name).cloned()
    }

    /// Get default provider
    pub fn get_default(&self) -> Option<Arc<dyn LlmProvider>> {
        self.default_provider
            .as_ref()
            .and_then(|name| self.providers.get(name).cloned())
    }

    /// List all providers
    pub fn list(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    /// Create provider from config
    pub fn from_config(config: &LlmProviderConfig) -> ProviderResult<Arc<dyn LlmProvider>> {
        match config.provider.as_str() {
            "openai" => Ok(Arc::new(OpenAiProvider::new(config)?)),
            "anthropic" => Ok(Arc::new(AnthropicProvider::new(config)?)),
            "azure" => Ok(Arc::new(AzureOpenAiProvider::new(config)?)),
            "ollama" => Ok(Arc::new(OllamaProvider::new(config)?)),
            "google" | "gemini" => Ok(Arc::new(GoogleProvider::new(config)?)),
            _ => Err(ProviderError::Config(format!(
                "Unknown provider: {}",
                config.provider
            ))),
        }
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}
