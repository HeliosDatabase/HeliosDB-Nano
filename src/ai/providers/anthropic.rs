//! Anthropic (Claude) LLM provider

use async_trait::async_trait;
use futures::Stream;

use super::{
    ChatMessage, LlmProvider, LlmProviderConfig, LlmRequest, LlmResponse,
    MessageRole, ModelInfo, ProviderError, ProviderResult, StreamChunk, TokenUsage,
};

/// Anthropic provider
pub struct AnthropicProvider {
    api_key: String,
    endpoint: String,
    default_model: String,
}

impl AnthropicProvider {
    /// Create new Anthropic provider
    pub fn new(config: &LlmProviderConfig) -> ProviderResult<Self> {
        let api_key = config.api_key.clone()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .ok_or_else(|| ProviderError::Config("Anthropic API key required".into()))?;

        let endpoint = config.endpoint.clone()
            .unwrap_or_else(|| "https://api.anthropic.com/v1".into());

        let default_model = config.model.clone()
            .unwrap_or_else(|| "claude-3-5-sonnet-20241022".into());

        Ok(Self {
            api_key,
            endpoint,
            default_model,
        })
    }

    /// Get available models
    fn available_models() -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "claude-opus-4-5-20251101".into(),
                name: "Claude Opus 4.5".into(),
                provider: "anthropic".into(),
                context_length: 200000,
                supports_functions: true,
                supports_vision: true,
                input_cost_per_1k: Some(0.015),
                output_cost_per_1k: Some(0.075),
            },
            ModelInfo {
                id: "claude-sonnet-4-5-20251101".into(),
                name: "Claude Sonnet 4.5".into(),
                provider: "anthropic".into(),
                context_length: 200000,
                supports_functions: true,
                supports_vision: true,
                input_cost_per_1k: Some(0.003),
                output_cost_per_1k: Some(0.015),
            },
            ModelInfo {
                id: "claude-3-5-sonnet-20241022".into(),
                name: "Claude 3.5 Sonnet".into(),
                provider: "anthropic".into(),
                context_length: 200000,
                supports_functions: true,
                supports_vision: true,
                input_cost_per_1k: Some(0.003),
                output_cost_per_1k: Some(0.015),
            },
            ModelInfo {
                id: "claude-3-5-haiku-20241022".into(),
                name: "Claude 3.5 Haiku".into(),
                provider: "anthropic".into(),
                context_length: 200000,
                supports_functions: true,
                supports_vision: true,
                input_cost_per_1k: Some(0.001),
                output_cost_per_1k: Some(0.005),
            },
            ModelInfo {
                id: "claude-3-opus-20240229".into(),
                name: "Claude 3 Opus".into(),
                provider: "anthropic".into(),
                context_length: 200000,
                supports_functions: true,
                supports_vision: true,
                input_cost_per_1k: Some(0.015),
                output_cost_per_1k: Some(0.075),
            },
        ]
    }

    /// Convert messages to Anthropic format
    fn convert_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<serde_json::Value>) {
        let mut system_prompt = None;
        let mut converted = Vec::new();

        for msg in messages {
            match msg.role {
                MessageRole::System => {
                    system_prompt = Some(msg.content.clone());
                }
                MessageRole::User => {
                    converted.push(serde_json::json!({
                        "role": "user",
                        "content": msg.content,
                    }));
                }
                MessageRole::Assistant => {
                    converted.push(serde_json::json!({
                        "role": "assistant",
                        "content": msg.content,
                    }));
                }
                MessageRole::Tool => {
                    converted.push(serde_json::json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": msg.tool_call_id,
                            "content": msg.content,
                        }],
                    }));
                }
                _ => {}
            }
        }

        (system_prompt, converted)
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn list_models(&self) -> ProviderResult<Vec<ModelInfo>> {
        Ok(Self::available_models())
    }

    async fn chat(&self, request: LlmRequest) -> ProviderResult<LlmResponse> {
        let model = request.model.as_deref().unwrap_or(&self.default_model);
        let (system_prompt, messages) = Self::convert_messages(&request.messages);

        // Build request body
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "max_tokens": request.max_tokens.unwrap_or(4096),
        });

        if let Some(system) = system_prompt {
            body["system"] = serde_json::json!(system);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(top_p) = request.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }
        if let Some(ref stop) = request.stop {
            body["stop_sequences"] = serde_json::json!(stop);
        }
        if let Some(ref tools) = request.tools {
            // Convert OpenAI-style tools to Anthropic format
            let anthropic_tools: Vec<serde_json::Value> = tools.iter().map(|t| {
                serde_json::json!({
                    "name": t.function.name,
                    "description": t.function.description,
                    "input_schema": t.function.parameters,
                })
            }).collect();
            body["tools"] = serde_json::json!(anthropic_tools);
        }

        // Make API request
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}/messages", self.endpoint))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if response.status() == 429 {
            return Err(ProviderError::RateLimit);
        }

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Api(error_text));
        }

        let result: serde_json::Value = response.json().await
            .map_err(|e| ProviderError::Api(e.to_string()))?;

        // Parse response
        let content = &result["content"][0];
        let text = content["text"].as_str().unwrap_or("").to_string();

        // Handle tool use
        let tool_calls = if content["type"].as_str() == Some("tool_use") {
            Some(vec![super::ToolCall {
                id: content["id"].as_str().unwrap_or("").to_string(),
                call_type: "function".to_string(),
                function: super::FunctionCall {
                    name: content["name"].as_str().unwrap_or("").to_string(),
                    arguments: serde_json::to_string(&content["input"]).unwrap_or_default(),
                },
            }])
        } else {
            None
        };

        let message = ChatMessage {
            role: MessageRole::Assistant,
            content: text,
            name: None,
            function_call: None,
            tool_calls,
            tool_call_id: None,
        };

        let usage = result.get("usage").map(|u| TokenUsage {
            prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0) as usize,
            completion_tokens: u["output_tokens"].as_u64().unwrap_or(0) as usize,
            total_tokens: (u["input_tokens"].as_u64().unwrap_or(0) +
                          u["output_tokens"].as_u64().unwrap_or(0)) as usize,
        });

        Ok(LlmResponse {
            id: result["id"].as_str().unwrap_or("").to_string(),
            model: result["model"].as_str().unwrap_or(model).to_string(),
            message,
            finish_reason: result["stop_reason"].as_str().map(|s| s.to_string()),
            usage,
        })
    }

    async fn chat_stream(
        &self,
        request: LlmRequest,
    ) -> ProviderResult<Box<dyn Stream<Item = ProviderResult<StreamChunk>> + Send + Unpin>> {
        let model = request.model.clone().unwrap_or_else(|| self.default_model.clone());
        let (system_prompt, messages) = Self::convert_messages(&request.messages);

        // Build request body with streaming enabled
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "max_tokens": request.max_tokens.unwrap_or(4096),
            "stream": true,
        });

        if let Some(system) = system_prompt {
            body["system"] = serde_json::json!(system);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(top_p) = request.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }
        if let Some(ref stop) = request.stop {
            body["stop_sequences"] = serde_json::json!(stop);
        }
        if let Some(ref tools) = request.tools {
            let anthropic_tools: Vec<serde_json::Value> = tools.iter().map(|t| {
                serde_json::json!({
                    "name": t.function.name,
                    "description": t.function.description,
                    "input_schema": t.function.parameters,
                })
            }).collect();
            body["tools"] = serde_json::json!(anthropic_tools);
        }

        // Make streaming API request
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}/messages", self.endpoint))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if response.status() == 429 {
            return Err(ProviderError::RateLimit);
        }

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Api(error_text));
        }

        // Create async stream from SSE response
        let stream = async_stream::stream! {
            use futures::StreamExt;
            let mut byte_stream = response.bytes_stream();
            let mut buffer = String::new();
            let mut current_id = String::new();

            while let Some(chunk_result) = byte_stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        buffer.push_str(&String::from_utf8_lossy(&chunk));

                        // Process complete SSE events
                        while let Some(pos) = buffer.find("\n\n") {
                            let event = buffer[..pos].to_string();
                            buffer = buffer[pos + 2..].to_string();

                            let mut event_type = String::new();
                            let mut event_data = String::new();

                            for line in event.lines() {
                                if let Some(t) = line.strip_prefix("event: ") {
                                    event_type = t.to_string();
                                } else if let Some(d) = line.strip_prefix("data: ") {
                                    event_data = d.to_string();
                                }
                            }

                            if event_data.is_empty() {
                                continue;
                            }

                            match serde_json::from_str::<serde_json::Value>(&event_data) {
                                Ok(json) => {
                                    match event_type.as_str() {
                                        "message_start" => {
                                            if let Some(id) = json["message"]["id"].as_str() {
                                                current_id = id.to_string();
                                            }
                                            let chunk = StreamChunk {
                                                id: current_id.clone(),
                                                delta: super::ChatDelta {
                                                    role: Some(MessageRole::Assistant),
                                                    content: None,
                                                    function_call: None,
                                                    tool_calls: None,
                                                },
                                                finish_reason: None,
                                            };
                                            yield Ok(chunk);
                                        }
                                        "content_block_delta" => {
                                            let delta_obj = &json["delta"];
                                            let content = delta_obj.get("text")
                                                .and_then(|t| t.as_str())
                                                .map(|s| s.to_string());

                                            // Handle tool input delta
                                            let tool_calls = if delta_obj.get("type").and_then(|t| t.as_str()) == Some("input_json_delta") {
                                                let partial_json = delta_obj.get("partial_json")
                                                    .and_then(|p| p.as_str())
                                                    .unwrap_or("");
                                                Some(vec![super::ToolCallDelta {
                                                    index: json["index"].as_u64().unwrap_or(0) as usize,
                                                    id: None,
                                                    call_type: None,
                                                    function: Some(super::FunctionCallDelta {
                                                        name: None,
                                                        arguments: Some(partial_json.to_string()),
                                                    }),
                                                }])
                                            } else {
                                                None
                                            };

                                            let chunk = StreamChunk {
                                                id: current_id.clone(),
                                                delta: super::ChatDelta {
                                                    role: None,
                                                    content,
                                                    function_call: None,
                                                    tool_calls,
                                                },
                                                finish_reason: None,
                                            };
                                            yield Ok(chunk);
                                        }
                                        "content_block_start" => {
                                            // Handle tool use start
                                            let content_block = &json["content_block"];
                                            if content_block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                                                let chunk = StreamChunk {
                                                    id: current_id.clone(),
                                                    delta: super::ChatDelta {
                                                        role: None,
                                                        content: None,
                                                        function_call: None,
                                                        tool_calls: Some(vec![super::ToolCallDelta {
                                                            index: json["index"].as_u64().unwrap_or(0) as usize,
                                                            id: content_block.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()),
                                                            call_type: Some("function".to_string()),
                                                            function: Some(super::FunctionCallDelta {
                                                                name: content_block.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()),
                                                                arguments: None,
                                                            }),
                                                        }]),
                                                    },
                                                    finish_reason: None,
                                                };
                                                yield Ok(chunk);
                                            }
                                        }
                                        "message_delta" => {
                                            let stop_reason = json["delta"]["stop_reason"]
                                                .as_str()
                                                .map(|s| s.to_string());
                                            let chunk = StreamChunk {
                                                id: current_id.clone(),
                                                delta: super::ChatDelta {
                                                    role: None,
                                                    content: None,
                                                    function_call: None,
                                                    tool_calls: None,
                                                },
                                                finish_reason: stop_reason,
                                            };
                                            yield Ok(chunk);
                                        }
                                        "message_stop" => {
                                            return;
                                        }
                                        _ => {}
                                    }
                                }
                                Err(e) => {
                                    yield Err(ProviderError::Api(format!("Failed to parse SSE: {}", e)));
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
        // Approximate token count (Claude uses ~3.5 chars per token on average)
        Ok(text.len() * 10 / 35)
    }

    fn supports_model(&self, model: &str) -> bool {
        Self::available_models().iter().any(|m| m.id == model)
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        Self::available_models().into_iter().find(|m| m.id == model)
    }
}
