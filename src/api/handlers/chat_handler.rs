//! Chat completion API handlers
//!
//! Provides REST API endpoints for AI chat operations:
//! - OpenAI-compatible chat completions
//! - RAG-enhanced responses
//! - Streaming support
//! - Function/tool calling

use axum::{
    extract::{Path, State},
    response::sse::{Event, Sse},
    Json,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;

use crate::api::models::ApiError;
use crate::api::server::AppState;

/// Chat message
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ChatMessage {
    /// Message role (system, user, assistant, function, tool)
    pub role: String,
    /// Message content
    pub content: Option<String>,
    /// Name (for function messages)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Function call
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    /// Tool calls
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Tool call ID (for tool role)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Function call
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Tool call
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

/// Function definition
#[derive(Debug, Deserialize, Clone)]
pub struct FunctionDef {
    pub name: String,
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

/// Tool definition
#[derive(Debug, Deserialize, Clone)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDef,
}

/// Chat completion request
#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    /// Model to use (for routing, default uses configured provider)
    pub model: Option<String>,
    /// Messages in conversation
    pub messages: Vec<ChatMessage>,
    /// Maximum tokens to generate
    pub max_tokens: Option<usize>,
    /// Temperature (0.0-2.0)
    pub temperature: Option<f32>,
    /// Top-p sampling
    pub top_p: Option<f32>,
    /// Number of completions
    pub n: Option<usize>,
    /// Stream response
    #[serde(default)]
    pub stream: bool,
    /// Stop sequences
    pub stop: Option<Vec<String>>,
    /// Presence penalty
    pub presence_penalty: Option<f32>,
    /// Frequency penalty
    pub frequency_penalty: Option<f32>,
    /// Functions (deprecated, use tools)
    pub functions: Option<Vec<FunctionDef>>,
    /// Function call control
    pub function_call: Option<serde_json::Value>,
    /// Tools
    pub tools: Option<Vec<ToolDef>>,
    /// Tool choice
    pub tool_choice: Option<serde_json::Value>,
    /// User identifier
    pub user: Option<String>,
    /// RAG configuration
    pub rag: Option<RagConfig>,
    /// Session ID for memory integration
    pub session_id: Option<String>,
    /// Branch for data context
    pub branch: Option<String>,
}

/// RAG configuration
#[derive(Debug, Deserialize, Clone)]
pub struct RagConfig {
    /// Vector stores to search
    pub vector_stores: Vec<String>,
    /// Number of context documents
    #[serde(default = "default_rag_k")]
    pub top_k: usize,
    /// Minimum relevance score
    pub min_score: Option<f32>,
    /// Include sources in response
    #[serde(default = "default_true")]
    pub include_sources: bool,
    /// Rerank results
    #[serde(default)]
    pub rerank: bool,
    /// Hybrid search alpha
    pub alpha: Option<f32>,
}

fn default_rag_k() -> usize {
    5
}

fn default_true() -> bool {
    true
}

/// Chat completion response
#[derive(Debug, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    pub usage: Option<Usage>,
    /// RAG sources (if RAG enabled)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<RagSource>>,
}

/// Chat choice
#[derive(Debug, Serialize)]
pub struct ChatChoice {
    pub index: usize,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

/// Token usage
#[derive(Debug, Serialize)]
pub struct Usage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
}

/// RAG source reference
#[derive(Debug, Serialize)]
pub struct RagSource {
    pub id: String,
    pub content: String,
    pub score: f32,
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Streaming chunk
#[derive(Debug, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
}

/// Chunk choice
#[derive(Debug, Serialize)]
pub struct ChunkChoice {
    pub index: usize,
    pub delta: ChatDelta,
    pub finish_reason: Option<String>,
}

/// Delta content
#[derive(Debug, Serialize)]
pub struct ChatDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCallDelta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// Function call delta
#[derive(Debug, Serialize)]
pub struct FunctionCallDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// Tool call delta
#[derive(Debug, Serialize)]
pub struct ToolCallDelta {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub call_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCallDelta>,
}

/// List models response
#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelInfo>,
}

/// Model info
#[derive(Debug, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub owned_by: String,
}

/// Embeddings request
#[derive(Debug, Deserialize)]
pub struct EmbeddingsRequest {
    /// Model to use
    pub model: Option<String>,
    /// Input text(s)
    pub input: EmbeddingInput,
    /// Encoding format
    pub encoding_format: Option<String>,
    /// Dimensions (for models that support it)
    pub dimensions: Option<usize>,
    /// User identifier
    pub user: Option<String>,
}

/// Embedding input
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Single(String),
    Multiple(Vec<String>),
}

/// Embeddings response
#[derive(Debug, Serialize)]
pub struct EmbeddingsResponse {
    pub object: String,
    pub model: String,
    pub data: Vec<EmbeddingData>,
    pub usage: EmbeddingUsage,
}

/// Single embedding
#[derive(Debug, Serialize)]
pub struct EmbeddingData {
    pub object: String,
    pub index: usize,
    pub embedding: Vec<f32>,
}

/// Embedding usage
#[derive(Debug, Serialize)]
pub struct EmbeddingUsage {
    pub prompt_tokens: usize,
    pub total_tokens: usize,
}

// ============================================================================
// Handler implementations
// ============================================================================

/// Create chat completion
pub async fn create_chat_completion(
    State(state): State<AppState>,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<Json<ChatCompletionResponse>, ApiError> {
    // Get RAG context if configured
    let rag_context = if let Some(ref rag_config) = req.rag {
        // Extract user query from last user message
        let user_query = req.messages.iter()
            .rev()
            .find(|m| m.role == "user")
            .and_then(|m| m.content.clone())
            .unwrap_or_default();

        let raw_sources = state.db.rag_search(
            &rag_config.vector_stores.first().unwrap_or(&"default".to_string()),
            &user_query,
            rag_config.top_k,
        ).map_err(|e| ApiError::internal(format!("RAG search failed: {}", e)))?;

        let sources: Vec<_> = raw_sources.into_iter().map(|(doc, score, context)| {
            crate::api::models::RagSource {
                id: doc.id.clone(),
                content: doc.content,
                score,
                metadata: doc.metadata,
                context: Some(context),
            }
        }).collect();

        Some(sources)
    } else {
        None
    };

    // Build messages with RAG context
    let mut messages = req.messages.clone();
    if let Some(ref sources) = rag_context {
        // Insert context as system message
        let context = sources.iter()
            .map(|s| format!("Source [{}]: {}", s.id, s.content))
            .collect::<Vec<_>>()
            .join("\n\n");

        let system_msg = ChatMessage {
            role: "system".to_string(),
            content: Some(format!(
                "Use the following context to answer the user's question:\n\n{}\n\n\
                 If the context doesn't contain relevant information, say so.",
                context
            )),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        };

        // Insert after existing system messages
        let system_end = messages.iter()
            .position(|m| m.role != "system")
            .unwrap_or(0);
        messages.insert(system_end, system_msg);
    }

    // Load memory context if session provided
    if let Some(ref session_id) = req.session_id {
        let memory_messages = state.db.get_agent_messages(
            session_id,
        ).map_err(|e| ApiError::internal(format!("Memory load failed: {}", e)))?;

        // Prepend memory (after system messages)
        let system_end = messages.iter()
            .position(|m| m.role != "system")
            .unwrap_or(0);

        for (i, m) in memory_messages.into_iter().enumerate() {
            // Parse function_call JSON string if present
            let function_call = m.function_call.as_ref().and_then(|fc_str| {
                serde_json::from_str::<FunctionCall>(fc_str).ok()
            });

            // Parse tool_calls if present - it's stored as Option<serde_json::Value>
            let tool_calls = m.tool_calls.as_ref().and_then(|tc_val| {
                serde_json::from_value::<Vec<ToolCall>>(tc_val.clone()).ok()
            });

            messages.insert(system_end + i, ChatMessage {
                role: m.role,
                content: Some(m.content),
                name: if m.name.is_empty() { None } else { Some(m.name) },
                function_call,
                tool_calls,
                tool_call_id: None,
            });
        }
    }

    // Generate completion
    let messages_for_api: Vec<(String, String)> = messages.iter().map(|m| {
        (m.role.clone(), m.content.clone().unwrap_or_default())
    }).collect();

    let completion_text = state.db.chat_completion(
        messages_for_api,
    ).map_err(|e| ApiError::internal(format!("Chat completion failed: {}", e)))?;

    let result = crate::api::models::ChatCompletionResult {
        id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        created: chrono::Utc::now().timestamp(),
        model: req.model.clone().unwrap_or_else(|| "gpt-3.5-turbo".to_string()),
        choices: vec![crate::api::models::ChatCompletionChoice {
            index: 0,
            message: crate::api::models::ChatCompletionMessage {
                role: "assistant".to_string(),
                content: completion_text,
                name: None,
                function_call: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: serde_json::json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}),
    };

    // Save to memory if session provided
    if let Some(ref session_id) = req.session_id {
        // Save user message
        if let Some(user_msg) = req.messages.iter().rev().find(|m| m.role == "user") {
            let _ = state.db.add_agent_message(
                session_id,
                "user",
                user_msg.content.as_deref().unwrap_or(""),
            );
        }

        // Save assistant response
        if let Some(choice) = result.choices.first() {
            let _ = state.db.add_agent_message(
                session_id,
                "assistant",
                &choice.message.content,
            );
        }
    }

    // Build response
    let response = ChatCompletionResponse {
        id: result.id,
        object: "chat.completion".to_string(),
        created: result.created,
        model: result.model,
        choices: result.choices.into_iter().map(|c| {
            // Parse function_call from serde_json::Value if present
            let function_call = c.message.function_call.as_ref().and_then(|fc_val| {
                serde_json::from_value::<FunctionCall>(fc_val.clone()).ok()
            });

            // Parse tool_calls from Option<Vec<serde_json::Value>> if present
            let tool_calls = c.message.tool_calls.as_ref().and_then(|tc_list| {
                tc_list.iter()
                    .map(|tc_val| serde_json::from_value::<ToolCall>(tc_val.clone()).ok())
                    .collect::<Option<Vec<ToolCall>>>()
            });

            ChatChoice {
                index: c.index as usize,
                message: ChatMessage {
                    role: c.message.role,
                    content: Some(c.message.content),
                    name: c.message.name,
                    function_call,
                    tool_calls,
                    tool_call_id: None,
                },
                finish_reason: c.finish_reason,
            }
        }).collect(),
        usage: {
            // Parse usage from serde_json::Value
            if let Ok(usage_obj) = serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(result.usage) {
                Some(Usage {
                    prompt_tokens: usage_obj.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                    completion_tokens: usage_obj.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                    total_tokens: usage_obj.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                })
            } else {
                None
            }
        },
        sources: if req.rag.as_ref().map(|r| r.include_sources).unwrap_or(false) {
            rag_context.map(|sources| {
                sources.into_iter().map(|s| {
                    // Convert metadata from Option<serde_json::Value> to HashMap
                    let metadata = match s.metadata {
                        Some(serde_json::Value::Object(map)) => map.into_iter().collect(),
                        _ => HashMap::new(),
                    };

                    RagSource {
                        id: s.id,
                        content: s.content,
                        score: s.score,
                        metadata,
                    }
                }).collect()
            })
        } else {
            None
        },
    };

    Ok(Json(response))
}

/// Streaming chat completion chunk response (OpenAI-compatible)
#[derive(Debug, Serialize)]
struct StreamingChunk {
    id: String,
    object: String,
    created: i64,
    model: String,
    choices: Vec<StreamingChoice>,
}

/// Streaming choice with delta
#[derive(Debug, Serialize)]
struct StreamingChoice {
    index: usize,
    delta: StreamingDelta,
    finish_reason: Option<String>,
}

/// Streaming message delta
#[derive(Debug, Serialize)]
struct StreamingDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

/// Create streaming chat completion
pub async fn create_chat_completion_stream(
    State(state): State<AppState>,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let messages_for_api: Vec<(String, String)> = req.messages.iter().map(|m| {
        (m.role.clone(), m.content.clone().unwrap_or_default())
    }).collect();

    let model = req.model.clone().unwrap_or_else(|| "heliosdb-default".to_string());
    let chunk_id = format!("chatcmpl-{}", uuid::Uuid::new_v4().to_string().replace("-", "")[..24].to_string());
    let created = chrono::Utc::now().timestamp();

    // Try to create the stream from the database
    let stream_result = state.db.chat_completion_stream(messages_for_api);

    // Create SSE stream with proper OpenAI-compatible format
    let sse_stream = async_stream::stream! {
        // First chunk: role declaration
        let role_chunk = StreamingChunk {
            id: chunk_id.clone(),
            object: "chat.completion.chunk".to_string(),
            created,
            model: model.clone(),
            choices: vec![StreamingChoice {
                index: 0,
                delta: StreamingDelta {
                    role: Some("assistant".to_string()),
                    content: None,
                },
                finish_reason: None,
            }],
        };
        if let Ok(json) = serde_json::to_string(&role_chunk) {
            yield Ok(Event::default().data(json));
        }

        // Handle streaming based on result
        match stream_result {
            Ok(content) => {
                // Stream the content character by character or in chunks
                // For now, stream in reasonable chunks
                for chunk_text in content.chars().collect::<Vec<_>>().chunks(20) {
                    let text: String = chunk_text.iter().collect();
                    let content_chunk = StreamingChunk {
                        id: chunk_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created,
                        model: model.clone(),
                        choices: vec![StreamingChoice {
                            index: 0,
                            delta: StreamingDelta {
                                role: None,
                                content: Some(text),
                            },
                            finish_reason: None,
                        }],
                    };
                    if let Ok(json) = serde_json::to_string(&content_chunk) {
                        yield Ok(Event::default().data(json));
                    }
                    // Small delay for streaming effect
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                }
            }
            Err(_) => {
                // Streaming not yet implemented - return helpful message
                let message = "Chat completions require an AI provider configuration. \
                              Configure an OpenAI-compatible endpoint in your HeliosDB settings \
                              or use the non-streaming endpoint for simpler integrations.";

                for chunk_text in message.chars().collect::<Vec<_>>().chunks(30) {
                    let text: String = chunk_text.iter().collect();
                    let content_chunk = StreamingChunk {
                        id: chunk_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created,
                        model: model.clone(),
                        choices: vec![StreamingChoice {
                            index: 0,
                            delta: StreamingDelta {
                                role: None,
                                content: Some(text),
                            },
                            finish_reason: None,
                        }],
                    };
                    if let Ok(json) = serde_json::to_string(&content_chunk) {
                        yield Ok(Event::default().data(json));
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
                }
            }
        }

        // Final chunk: finish_reason
        let final_chunk = StreamingChunk {
            id: chunk_id.clone(),
            object: "chat.completion.chunk".to_string(),
            created,
            model: model.clone(),
            choices: vec![StreamingChoice {
                index: 0,
                delta: StreamingDelta {
                    role: None,
                    content: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
        };
        if let Ok(json) = serde_json::to_string(&final_chunk) {
            yield Ok(Event::default().data(json));
        }

        // Signal end of stream
        yield Ok(Event::default().data("[DONE]"));
    };

    Ok(Sse::new(sse_stream))
}

/// List available models
pub async fn list_models(
    State(state): State<AppState>,
) -> Result<Json<ModelsResponse>, ApiError> {
    let models = state.db.list_chat_models()
        .map_err(|e| ApiError::internal(format!("Failed to list models: {}", e)))?;

    let model_infos: Vec<ModelInfo> = models
        .into_iter()
        .filter_map(|m| {
            // Parse serde_json::Value to extract fields
            if let serde_json::Value::Object(map) = m {
                Some(ModelInfo {
                    id: map.get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    object: "model".to_string(),
                    created: map.get("created")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0),
                    owned_by: map.get("owned_by")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                })
            } else {
                None
            }
        })
        .collect();

    Ok(Json(ModelsResponse {
        object: "list".to_string(),
        data: model_infos,
    }))
}

/// Get model info
pub async fn get_model(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> Result<Json<ModelInfo>, ApiError> {
    let model_val = state.db.get_chat_model(&model_id)
        .map_err(|e| ApiError::not_found(format!("Model not found: {}", e)))?;

    // Parse serde_json::Value to extract fields
    if let serde_json::Value::Object(map) = model_val {
        Ok(Json(ModelInfo {
            id: map.get("id")
                .and_then(|v| v.as_str())
                .unwrap_or(&model_id)
                .to_string(),
            object: "model".to_string(),
            created: map.get("created")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            owned_by: map.get("owned_by")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        }))
    } else {
        Err(ApiError::not_found("Model not found".to_string()))
    }
}

/// Create embeddings
pub async fn create_embeddings(
    State(state): State<AppState>,
    Json(req): Json<EmbeddingsRequest>,
) -> Result<Json<EmbeddingsResponse>, ApiError> {
    let inputs = match req.input {
        EmbeddingInput::Single(s) => vec![s],
        EmbeddingInput::Multiple(v) => v,
    };

    let embeddings_vec = state.db.create_embeddings(
        inputs.clone(),
    ).map_err(|e| ApiError::internal(format!("Embedding failed: {}", e)))?;

    let prompt_tokens: usize = inputs.iter().map(|s| s.split_whitespace().count()).sum();
    let total_tokens = prompt_tokens;

    let embeddings: Vec<EmbeddingData> = embeddings_vec
        .into_iter()
        .enumerate()
        .map(|(i, emb)| EmbeddingData {
            object: "embedding".to_string(),
            index: i,
            embedding: emb,
        })
        .collect();

    Ok(Json(EmbeddingsResponse {
        object: "list".to_string(),
        model: req.model.unwrap_or_else(|| "default".to_string()),
        data: embeddings,
        usage: EmbeddingUsage {
            prompt_tokens,
            total_tokens,
        },
    }))
}
