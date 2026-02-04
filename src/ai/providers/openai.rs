//! OpenAI LLM provider

use async_trait::async_trait;
#[allow(unused_imports)]
use futures::Stream;

use super::{
    ChatMessage, LlmProvider, LlmProviderConfig, LlmRequest, LlmResponse,
    ModelInfo, ProviderError, ProviderResult, StreamChunk, TokenUsage,
};

/// OpenAI provider
pub struct OpenAiProvider {
    api_key: String,
    endpoint: String,
    organization: Option<String>,
    default_model: String,
}

impl OpenAiProvider {
    /// Create new OpenAI provider
    pub fn new(config: &LlmProviderConfig) -> ProviderResult<Self> {
        let api_key = config.api_key.clone()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .ok_or_else(|| ProviderError::Config("OpenAI API key required".into()))?;

        let endpoint = config.endpoint.clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".into());

        let default_model = config.model.clone()
            .unwrap_or_else(|| "gpt-4-turbo-preview".into());

        Ok(Self {
            api_key,
            endpoint,
            organization: config.organization.clone(),
            default_model,
        })
    }

    /// Get available models
    fn available_models() -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "gpt-4-turbo-preview".into(),
                name: "GPT-4 Turbo".into(),
                provider: "openai".into(),
                context_length: 128000,
                supports_functions: true,
                supports_vision: true,
                input_cost_per_1k: Some(0.01),
                output_cost_per_1k: Some(0.03),
            },
            ModelInfo {
                id: "gpt-4".into(),
                name: "GPT-4".into(),
                provider: "openai".into(),
                context_length: 8192,
                supports_functions: true,
                supports_vision: false,
                input_cost_per_1k: Some(0.03),
                output_cost_per_1k: Some(0.06),
            },
            ModelInfo {
                id: "gpt-4o".into(),
                name: "GPT-4o".into(),
                provider: "openai".into(),
                context_length: 128000,
                supports_functions: true,
                supports_vision: true,
                input_cost_per_1k: Some(0.005),
                output_cost_per_1k: Some(0.015),
            },
            ModelInfo {
                id: "gpt-4o-mini".into(),
                name: "GPT-4o Mini".into(),
                provider: "openai".into(),
                context_length: 128000,
                supports_functions: true,
                supports_vision: true,
                input_cost_per_1k: Some(0.00015),
                output_cost_per_1k: Some(0.0006),
            },
            ModelInfo {
                id: "gpt-3.5-turbo".into(),
                name: "GPT-3.5 Turbo".into(),
                provider: "openai".into(),
                context_length: 16385,
                supports_functions: true,
                supports_vision: false,
                input_cost_per_1k: Some(0.0005),
                output_cost_per_1k: Some(0.0015),
            },
        ]
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn list_models(&self) -> ProviderResult<Vec<ModelInfo>> {
        Ok(Self::available_models())
    }

    async fn chat(&self, request: LlmRequest) -> ProviderResult<LlmResponse> {
        let model = request.model.as_deref().unwrap_or(&self.default_model);

        // Build request body
        let mut body = serde_json::json!({
            "model": model,
            "messages": request.messages.iter().map(|m| {
                serde_json::json!({
                    "role": format!("{:?}", m.role).to_lowercase(),
                    "content": m.content,
                })
            }).collect::<Vec<_>>(),
        });

        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(top_p) = request.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }
        if let Some(ref stop) = request.stop {
            body["stop"] = serde_json::json!(stop);
        }
        if let Some(ref tools) = request.tools {
            body["tools"] = serde_json::json!(tools);
        }
        if let Some(ref tool_choice) = request.tool_choice {
            body["tool_choice"] = tool_choice.clone();
        }

        // Make API request
        let client = reqwest::Client::new();
        let mut req_builder = client
            .post(format!("{}/chat/completions", self.endpoint))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json");

        if let Some(ref org) = self.organization {
            req_builder = req_builder.header("OpenAI-Organization", org);
        }

        let response = req_builder
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
        let choice = result["choices"][0].clone();
        let message_data = &choice["message"];

        let message = ChatMessage {
            role: super::MessageRole::Assistant,
            content: message_data["content"].as_str().unwrap_or("").to_string(),
            name: None,
            function_call: message_data.get("function_call").and_then(|fc| {
                serde_json::from_value(fc.clone()).ok()
            }),
            tool_calls: message_data.get("tool_calls").and_then(|tc| {
                serde_json::from_value(tc.clone()).ok()
            }),
            tool_call_id: None,
        };

        let usage = result.get("usage").map(|u| TokenUsage {
            prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as usize,
            completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as usize,
            total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as usize,
        });

        Ok(LlmResponse {
            id: result["id"].as_str().unwrap_or("").to_string(),
            model: result["model"].as_str().unwrap_or(model).to_string(),
            message,
            finish_reason: choice["finish_reason"].as_str().map(|s| s.to_string()),
            usage,
        })
    }

    async fn chat_stream(
        &self,
        request: LlmRequest,
    ) -> ProviderResult<Box<dyn Stream<Item = ProviderResult<StreamChunk>> + Send + Unpin>> {
        let model = request.model.clone().unwrap_or_else(|| self.default_model.clone());

        // Build request body with streaming enabled
        let mut body = serde_json::json!({
            "model": model,
            "messages": request.messages.iter().map(|m| {
                serde_json::json!({
                    "role": format!("{:?}", m.role).to_lowercase(),
                    "content": m.content,
                })
            }).collect::<Vec<_>>(),
            "stream": true,
        });

        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(top_p) = request.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }
        if let Some(ref stop) = request.stop {
            body["stop"] = serde_json::json!(stop);
        }
        if let Some(ref tools) = request.tools {
            body["tools"] = serde_json::json!(tools);
        }

        // Make streaming API request
        let client = reqwest::Client::new();
        let mut req_builder = client
            .post(format!("{}/chat/completions", self.endpoint))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json");

        if let Some(ref org) = self.organization {
            req_builder = req_builder.header("OpenAI-Organization", org);
        }

        let response = req_builder
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

            while let Some(chunk_result) = byte_stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        buffer.push_str(&String::from_utf8_lossy(&chunk));

                        // Process complete SSE events
                        while let Some(pos) = buffer.find("\n\n") {
                            let event = buffer[..pos].to_string();
                            buffer = buffer[pos + 2..].to_string();

                            for line in event.lines() {
                                if let Some(data) = line.strip_prefix("data: ") {
                                    if data == "[DONE]" {
                                        return;
                                    }

                                    match serde_json::from_str::<serde_json::Value>(data) {
                                        Ok(json) => {
                                            if let Some(choice) = json["choices"].get(0) {
                                                let delta = &choice["delta"];
                                                let chunk = StreamChunk {
                                                    id: json["id"].as_str().unwrap_or("").to_string(),
                                                    delta: super::ChatDelta {
                                                        role: delta.get("role").and_then(|r| {
                                                            match r.as_str()? {
                                                                "assistant" => Some(super::MessageRole::Assistant),
                                                                "user" => Some(super::MessageRole::User),
                                                                "system" => Some(super::MessageRole::System),
                                                                _ => None,
                                                            }
                                                        }),
                                                        content: delta.get("content").and_then(|c| c.as_str()).map(|s| s.to_string()),
                                                        function_call: delta.get("function_call").and_then(|fc| {
                                                            Some(super::FunctionCallDelta {
                                                                name: fc.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()),
                                                                arguments: fc.get("arguments").and_then(|a| a.as_str()).map(|s| s.to_string()),
                                                            })
                                                        }),
                                                        tool_calls: delta.get("tool_calls").and_then(|tc| {
                                                            tc.as_array().map(|arr| {
                                                                arr.iter().enumerate().filter_map(|(i, t)| {
                                                                    Some(super::ToolCallDelta {
                                                                        index: t.get("index").and_then(|idx| idx.as_u64()).unwrap_or(i as u64) as usize,
                                                                        id: t.get("id").and_then(|id| id.as_str()).map(|s| s.to_string()),
                                                                        call_type: t.get("type").and_then(|ct| ct.as_str()).map(|s| s.to_string()),
                                                                        function: t.get("function").map(|f| {
                                                                            super::FunctionCallDelta {
                                                                                name: f.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()),
                                                                                arguments: f.get("arguments").and_then(|a| a.as_str()).map(|s| s.to_string()),
                                                                            }
                                                                        }),
                                                                    })
                                                                }).collect()
                                                            })
                                                        }),
                                                    },
                                                    finish_reason: choice.get("finish_reason").and_then(|f| f.as_str()).map(|s| s.to_string()),
                                                };
                                                yield Ok(chunk);
                                            }
                                        }
                                        Err(e) => {
                                            yield Err(ProviderError::Api(format!("Failed to parse SSE: {}", e)));
                                        }
                                    }
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
        // Approximate token count (GPT models use ~4 chars per token on average)
        Ok(text.len() / 4)
    }

    fn supports_model(&self, model: &str) -> bool {
        Self::available_models().iter().any(|m| m.id == model)
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        Self::available_models().into_iter().find(|m| m.id == model)
    }
}
