//! Ollama (local models) LLM provider

use async_trait::async_trait;
use futures::Stream;

use super::{
    ChatMessage, LlmProvider, LlmProviderConfig, LlmRequest, LlmResponse,
    MessageRole, ModelInfo, ProviderError, ProviderResult, StreamChunk, TokenUsage,
};

/// Ollama provider for local models
pub struct OllamaProvider {
    endpoint: String,
    default_model: String,
}

impl OllamaProvider {
    /// Create new Ollama provider
    pub fn new(config: &LlmProviderConfig) -> ProviderResult<Self> {
        let endpoint = config.endpoint.clone()
            .unwrap_or_else(|| "http://localhost:11434".into());

        let default_model = config.model.clone()
            .unwrap_or_else(|| "llama3.2".into());

        Ok(Self {
            endpoint,
            default_model,
        })
    }

    /// Common local models
    fn common_models() -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "llama3.2".into(),
                name: "Llama 3.2".into(),
                provider: "ollama".into(),
                context_length: 128000,
                supports_functions: true,
                supports_vision: false,
                input_cost_per_1k: None,
                output_cost_per_1k: None,
            },
            ModelInfo {
                id: "llama3.1".into(),
                name: "Llama 3.1".into(),
                provider: "ollama".into(),
                context_length: 128000,
                supports_functions: true,
                supports_vision: false,
                input_cost_per_1k: None,
                output_cost_per_1k: None,
            },
            ModelInfo {
                id: "qwen2.5-coder".into(),
                name: "Qwen 2.5 Coder".into(),
                provider: "ollama".into(),
                context_length: 32768,
                supports_functions: true,
                supports_vision: false,
                input_cost_per_1k: None,
                output_cost_per_1k: None,
            },
            ModelInfo {
                id: "codellama".into(),
                name: "Code Llama".into(),
                provider: "ollama".into(),
                context_length: 16384,
                supports_functions: false,
                supports_vision: false,
                input_cost_per_1k: None,
                output_cost_per_1k: None,
            },
            ModelInfo {
                id: "mistral".into(),
                name: "Mistral 7B".into(),
                provider: "ollama".into(),
                context_length: 32768,
                supports_functions: true,
                supports_vision: false,
                input_cost_per_1k: None,
                output_cost_per_1k: None,
            },
            ModelInfo {
                id: "mixtral".into(),
                name: "Mixtral 8x7B".into(),
                provider: "ollama".into(),
                context_length: 32768,
                supports_functions: true,
                supports_vision: false,
                input_cost_per_1k: None,
                output_cost_per_1k: None,
            },
            ModelInfo {
                id: "deepseek-coder-v2".into(),
                name: "DeepSeek Coder V2".into(),
                provider: "ollama".into(),
                context_length: 128000,
                supports_functions: true,
                supports_vision: false,
                input_cost_per_1k: None,
                output_cost_per_1k: None,
            },
            ModelInfo {
                id: "phi3".into(),
                name: "Phi-3".into(),
                provider: "ollama".into(),
                context_length: 128000,
                supports_functions: false,
                supports_vision: false,
                input_cost_per_1k: None,
                output_cost_per_1k: None,
            },
        ]
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    async fn list_models(&self) -> ProviderResult<Vec<ModelInfo>> {
        // Try to fetch from Ollama API
        let client = reqwest::Client::new();
        let response = client
            .get(format!("{}/api/tags", self.endpoint))
            .send()
            .await;

        match response {
            Ok(resp) if resp.status().is_success() => {
                let result: serde_json::Value = resp.json().await
                    .map_err(|e| ProviderError::Api(e.to_string()))?;

                let models: Vec<ModelInfo> = result["models"]
                    .as_array()
                    .map(|arr| {
                        arr.iter().map(|m| {
                            let name = m["name"].as_str().unwrap_or("unknown");
                            ModelInfo {
                                id: name.to_string(),
                                name: name.to_string(),
                                provider: "ollama".into(),
                                context_length: 4096, // Default, actual varies
                                supports_functions: false,
                                supports_vision: false,
                                input_cost_per_1k: None,
                                output_cost_per_1k: None,
                            }
                        }).collect()
                    })
                    .unwrap_or_else(|| Self::common_models());

                Ok(models)
            }
            _ => Ok(Self::common_models()),
        }
    }

    async fn chat(&self, request: LlmRequest) -> ProviderResult<LlmResponse> {
        let model = request.model.as_deref().unwrap_or(&self.default_model);

        // Convert messages to Ollama format
        let messages: Vec<serde_json::Value> = request.messages.iter().map(|m| {
            serde_json::json!({
                "role": match m.role {
                    MessageRole::System => "system",
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    _ => "user",
                },
                "content": m.content,
            })
        }).collect();

        // Build request body
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": false,
        });

        // Add options
        let mut options = serde_json::Map::new();
        if let Some(temp) = request.temperature {
            options.insert("temperature".into(), serde_json::json!(temp));
        }
        if let Some(top_p) = request.top_p {
            options.insert("top_p".into(), serde_json::json!(top_p));
        }
        if let Some(max_tokens) = request.max_tokens {
            options.insert("num_predict".into(), serde_json::json!(max_tokens));
        }
        if let Some(ref stop) = request.stop {
            options.insert("stop".into(), serde_json::json!(stop));
        }
        if !options.is_empty() {
            body["options"] = serde_json::Value::Object(options);
        }

        // Make API request
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}/api/chat", self.endpoint))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Api(error_text));
        }

        let result: serde_json::Value = response.json().await
            .map_err(|e| ProviderError::Api(e.to_string()))?;

        // Parse response
        let content = result["message"]["content"].as_str().unwrap_or("").to_string();

        let message = ChatMessage {
            role: MessageRole::Assistant,
            content,
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        };

        // Ollama provides eval_count (completion) and prompt_eval_count (prompt)
        let usage = Some(TokenUsage {
            prompt_tokens: result["prompt_eval_count"].as_u64().unwrap_or(0) as usize,
            completion_tokens: result["eval_count"].as_u64().unwrap_or(0) as usize,
            total_tokens: (result["prompt_eval_count"].as_u64().unwrap_or(0) +
                          result["eval_count"].as_u64().unwrap_or(0)) as usize,
        });

        Ok(LlmResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: model.to_string(),
            message,
            finish_reason: Some("stop".to_string()),
            usage,
        })
    }

    async fn chat_stream(
        &self,
        request: LlmRequest,
    ) -> ProviderResult<Box<dyn Stream<Item = ProviderResult<StreamChunk>> + Send + Unpin>> {
        let model = request.model.clone().unwrap_or_else(|| self.default_model.clone());

        // Convert messages to Ollama format
        let messages: Vec<serde_json::Value> = request.messages.iter().map(|m| {
            serde_json::json!({
                "role": match m.role {
                    MessageRole::System => "system",
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    _ => "user",
                },
                "content": m.content,
            })
        }).collect();

        // Build request body with streaming enabled
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": true,
        });

        // Add options
        let mut options = serde_json::Map::new();
        if let Some(temp) = request.temperature {
            options.insert("temperature".into(), serde_json::json!(temp));
        }
        if let Some(top_p) = request.top_p {
            options.insert("top_p".into(), serde_json::json!(top_p));
        }
        if let Some(max_tokens) = request.max_tokens {
            options.insert("num_predict".into(), serde_json::json!(max_tokens));
        }
        if let Some(ref stop) = request.stop {
            options.insert("stop".into(), serde_json::json!(stop));
        }
        if !options.is_empty() {
            body["options"] = serde_json::Value::Object(options);
        }

        // Make streaming API request
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}/api/chat", self.endpoint))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Api(error_text));
        }

        // Create async stream from NDJSON response (Ollama uses newline-delimited JSON)
        let stream = async_stream::stream! {
            use futures::StreamExt;
            let mut byte_stream = response.bytes_stream();
            let mut buffer = String::new();
            let stream_id = uuid::Uuid::new_v4().to_string();
            let mut sent_role = false;

            while let Some(chunk_result) = byte_stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        buffer.push_str(&String::from_utf8_lossy(&chunk));

                        // Process complete JSON lines
                        while let Some(pos) = buffer.find('\n') {
                            let line = buffer[..pos].to_string();
                            buffer = buffer[pos + 1..].to_string();

                            if line.trim().is_empty() {
                                continue;
                            }

                            match serde_json::from_str::<serde_json::Value>(&line) {
                                Ok(json) => {
                                    let content = json["message"]["content"]
                                        .as_str()
                                        .map(|s| s.to_string());

                                    let done = json["done"].as_bool().unwrap_or(false);

                                    let chunk = StreamChunk {
                                        id: stream_id.clone(),
                                        delta: super::ChatDelta {
                                            role: if !sent_role {
                                                sent_role = true;
                                                Some(MessageRole::Assistant)
                                            } else {
                                                None
                                            },
                                            content,
                                            function_call: None,
                                            tool_calls: None,
                                        },
                                        finish_reason: if done { Some("stop".to_string()) } else { None },
                                    };
                                    yield Ok(chunk);

                                    if done {
                                        return;
                                    }
                                }
                                Err(e) => {
                                    yield Err(ProviderError::Api(format!("Failed to parse JSON: {}", e)));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(ProviderError::Network(e.to_string()));
                    }
                }
            }
        };

        Ok(Box::new(Box::pin(stream)))
    }

    fn count_tokens(&self, text: &str, _model: &str) -> ProviderResult<usize> {
        // Approximate token count
        Ok(text.len() / 4)
    }

    fn supports_model(&self, _model: &str) -> bool {
        // Ollama can run any model that's been pulled
        true
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        Self::common_models().into_iter().find(|m| m.id == model)
    }
}
