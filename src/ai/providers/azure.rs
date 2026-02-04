//! Azure OpenAI LLM provider

use async_trait::async_trait;
use futures::Stream;

use super::{
    ChatMessage, LlmProvider, LlmProviderConfig, LlmRequest, LlmResponse,
    ModelInfo, ProviderError, ProviderResult, StreamChunk, TokenUsage,
};

/// Azure OpenAI provider
pub struct AzureOpenAiProvider {
    api_key: String,
    endpoint: String,
    deployment: String,
    api_version: String,
}

impl AzureOpenAiProvider {
    /// Create new Azure OpenAI provider
    pub fn new(config: &LlmProviderConfig) -> ProviderResult<Self> {
        let api_key = config.api_key.clone()
            .or_else(|| std::env::var("AZURE_OPENAI_API_KEY").ok())
            .ok_or_else(|| ProviderError::Config("Azure OpenAI API key required".into()))?;

        let endpoint = config.endpoint.clone()
            .or_else(|| std::env::var("AZURE_OPENAI_ENDPOINT").ok())
            .ok_or_else(|| ProviderError::Config("Azure OpenAI endpoint required".into()))?;

        let deployment = config.deployment.clone()
            .or_else(|| std::env::var("AZURE_OPENAI_DEPLOYMENT").ok())
            .ok_or_else(|| ProviderError::Config("Azure deployment name required".into()))?;

        let api_version = config.api_version.clone()
            .unwrap_or_else(|| "2024-08-01-preview".into());

        Ok(Self {
            api_key,
            endpoint,
            deployment,
            api_version,
        })
    }
}

#[async_trait]
impl LlmProvider for AzureOpenAiProvider {
    fn name(&self) -> &str {
        "azure"
    }

    async fn list_models(&self) -> ProviderResult<Vec<ModelInfo>> {
        // Azure uses deployments, so we return info about the configured deployment
        Ok(vec![ModelInfo {
            id: self.deployment.clone(),
            name: format!("Azure: {}", self.deployment),
            provider: "azure".into(),
            context_length: 128000, // Depends on deployed model
            supports_functions: true,
            supports_vision: true,
            input_cost_per_1k: None, // Azure pricing varies
            output_cost_per_1k: None,
        }])
    }

    async fn chat(&self, request: LlmRequest) -> ProviderResult<LlmResponse> {
        // Build request body (same as OpenAI)
        let mut body = serde_json::json!({
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

        // Build Azure-specific URL
        let url = format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.endpoint.trim_end_matches('/'),
            self.deployment,
            self.api_version
        );

        // Make API request
        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .header("api-key", &self.api_key)
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

        // Parse response (same as OpenAI)
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
            model: self.deployment.clone(),
            message,
            finish_reason: choice["finish_reason"].as_str().map(|s| s.to_string()),
            usage,
        })
    }

    async fn chat_stream(
        &self,
        request: LlmRequest,
    ) -> ProviderResult<Box<dyn Stream<Item = ProviderResult<StreamChunk>> + Send + Unpin>> {
        // Build request body with streaming enabled
        let mut body = serde_json::json!({
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

        // Build Azure-specific URL
        let url = format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.endpoint.trim_end_matches('/'),
            self.deployment,
            self.api_version
        );

        // Make streaming API request
        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .header("api-key", &self.api_key)
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

        // Create async stream from SSE response (same format as OpenAI)
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
        Ok(text.len() / 4)
    }

    fn supports_model(&self, model: &str) -> bool {
        model == self.deployment
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        if model == self.deployment {
            Some(ModelInfo {
                id: self.deployment.clone(),
                name: format!("Azure: {}", self.deployment),
                provider: "azure".into(),
                context_length: 128000,
                supports_functions: true,
                supports_vision: true,
                input_cost_per_1k: None,
                output_cost_per_1k: None,
            })
        } else {
            None
        }
    }
}
