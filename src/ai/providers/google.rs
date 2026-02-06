//! Google (Gemini) LLM provider

use async_trait::async_trait;
use futures::Stream;

use super::{
    ChatMessage, LlmProvider, LlmProviderConfig, LlmRequest, LlmResponse,
    MessageRole, ModelInfo, ProviderError, ProviderResult, StreamChunk, TokenUsage,
};

/// Google Gemini provider
pub struct GoogleProvider {
    api_key: String,
    endpoint: String,
    default_model: String,
}

impl GoogleProvider {
    /// Create new Google provider
    pub fn new(config: &LlmProviderConfig) -> ProviderResult<Self> {
        let api_key = config.api_key.clone()
            .or_else(|| std::env::var("GOOGLE_API_KEY").ok())
            .or_else(|| std::env::var("GEMINI_API_KEY").ok())
            .ok_or_else(|| ProviderError::Config("Google API key required".into()))?;

        let endpoint = config.endpoint.clone()
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".into());

        let default_model = config.model.clone()
            .unwrap_or_else(|| "gemini-1.5-pro".into());

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
                id: "gemini-2.0-flash-exp".into(),
                name: "Gemini 2.0 Flash".into(),
                provider: "google".into(),
                context_length: 1048576,
                supports_functions: true,
                supports_vision: true,
                input_cost_per_1k: None,
                output_cost_per_1k: None,
            },
            ModelInfo {
                id: "gemini-1.5-pro".into(),
                name: "Gemini 1.5 Pro".into(),
                provider: "google".into(),
                context_length: 2097152,
                supports_functions: true,
                supports_vision: true,
                input_cost_per_1k: Some(0.00125),
                output_cost_per_1k: Some(0.005),
            },
            ModelInfo {
                id: "gemini-1.5-flash".into(),
                name: "Gemini 1.5 Flash".into(),
                provider: "google".into(),
                context_length: 1048576,
                supports_functions: true,
                supports_vision: true,
                input_cost_per_1k: Some(0.000075),
                output_cost_per_1k: Some(0.0003),
            },
            ModelInfo {
                id: "gemini-1.0-pro".into(),
                name: "Gemini 1.0 Pro".into(),
                provider: "google".into(),
                context_length: 32768,
                supports_functions: true,
                supports_vision: false,
                input_cost_per_1k: Some(0.0005),
                output_cost_per_1k: Some(0.0015),
            },
        ]
    }

    /// Convert messages to Gemini format
    fn convert_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<serde_json::Value>) {
        let mut system_instruction = None;
        let mut contents = Vec::new();

        for msg in messages {
            match msg.role {
                MessageRole::System => {
                    system_instruction = Some(msg.content.clone());
                }
                MessageRole::User => {
                    contents.push(serde_json::json!({
                        "role": "user",
                        "parts": [{"text": msg.content}]
                    }));
                }
                MessageRole::Assistant => {
                    contents.push(serde_json::json!({
                        "role": "model",
                        "parts": [{"text": msg.content}]
                    }));
                }
                _ => {}
            }
        }

        (system_instruction, contents)
    }
}

#[async_trait]
impl LlmProvider for GoogleProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "google"
    }

    async fn list_models(&self) -> ProviderResult<Vec<ModelInfo>> {
        Ok(Self::available_models())
    }

    // SAFETY: All JSON indexing uses serde_json::Value which returns Value::Null for missing keys,
    // never panics. String slicing in SSE parsing is bounds-checked by find() positions.
    #[allow(clippy::indexing_slicing)]
    async fn chat(&self, request: LlmRequest) -> ProviderResult<LlmResponse> {
        let model = request.model.as_deref().unwrap_or(&self.default_model);
        let (system_instruction, contents) = Self::convert_messages(&request.messages);

        // Build request body
        let mut body = serde_json::json!({
            "contents": contents,
        });

        // Add system instruction
        if let Some(system) = system_instruction {
            body["systemInstruction"] = serde_json::json!({
                "parts": [{"text": system}]
            });
        }

        // Generation config
        let mut generation_config = serde_json::Map::new();
        if let Some(max_tokens) = request.max_tokens {
            generation_config.insert("maxOutputTokens".into(), serde_json::json!(max_tokens));
        }
        if let Some(temp) = request.temperature {
            generation_config.insert("temperature".into(), serde_json::json!(temp));
        }
        if let Some(top_p) = request.top_p {
            generation_config.insert("topP".into(), serde_json::json!(top_p));
        }
        if let Some(ref stop) = request.stop {
            generation_config.insert("stopSequences".into(), serde_json::json!(stop));
        }
        if !generation_config.is_empty() {
            body["generationConfig"] = serde_json::Value::Object(generation_config);
        }

        // Add tools if specified
        if let Some(ref tools) = request.tools {
            let gemini_tools: Vec<serde_json::Value> = tools.iter().map(|t| {
                serde_json::json!({
                    "functionDeclarations": [{
                        "name": t.function.name,
                        "description": t.function.description,
                        "parameters": t.function.parameters,
                    }]
                })
            }).collect();
            body["tools"] = serde_json::json!(gemini_tools);
        }

        // Build URL
        let url = format!(
            "{}/models/{}:generateContent?key={}",
            self.endpoint,
            model,
            self.api_key
        );

        // Make API request
        let client = reqwest::Client::new();
        let response = client
            .post(&url)
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
        let candidate = &result["candidates"][0];
        let content = &candidate["content"]["parts"][0];

        let text = content["text"].as_str().unwrap_or("").to_string();

        // Handle function calls
        let tool_calls = content.get("functionCall").map(|fc| {
            vec![super::ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                call_type: "function".to_string(),
                function: super::FunctionCall {
                    name: fc["name"].as_str().unwrap_or("").to_string(),
                    arguments: serde_json::to_string(&fc["args"]).unwrap_or_default(),
                },
            }]
        });

        let message = ChatMessage {
            role: MessageRole::Assistant,
            content: text,
            name: None,
            function_call: None,
            tool_calls,
            tool_call_id: None,
        };

        let usage = result.get("usageMetadata").map(|u| TokenUsage {
            prompt_tokens: u["promptTokenCount"].as_u64().unwrap_or(0) as usize,
            completion_tokens: u["candidatesTokenCount"].as_u64().unwrap_or(0) as usize,
            total_tokens: u["totalTokenCount"].as_u64().unwrap_or(0) as usize,
        });

        Ok(LlmResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: model.to_string(),
            message,
            finish_reason: candidate["finishReason"].as_str().map(|s| s.to_string()),
            usage,
        })
    }

    // SAFETY: All JSON indexing uses serde_json::Value which returns Value::Null for missing keys.
    // String slicing in SSE buffer parsing is bounds-checked by find() positions.
    #[allow(clippy::indexing_slicing)]
    async fn chat_stream(
        &self,
        request: LlmRequest,
    ) -> ProviderResult<Box<dyn Stream<Item = ProviderResult<StreamChunk>> + Send + Unpin>> {
        let model = request.model.clone().unwrap_or_else(|| self.default_model.clone());
        let (system_instruction, contents) = Self::convert_messages(&request.messages);

        // Build request body
        let mut body = serde_json::json!({
            "contents": contents,
        });

        if let Some(system) = system_instruction {
            body["systemInstruction"] = serde_json::json!({
                "parts": [{"text": system}]
            });
        }

        // Generation config
        let mut generation_config = serde_json::Map::new();
        if let Some(max_tokens) = request.max_tokens {
            generation_config.insert("maxOutputTokens".into(), serde_json::json!(max_tokens));
        }
        if let Some(temp) = request.temperature {
            generation_config.insert("temperature".into(), serde_json::json!(temp));
        }
        if let Some(top_p) = request.top_p {
            generation_config.insert("topP".into(), serde_json::json!(top_p));
        }
        if let Some(ref stop) = request.stop {
            generation_config.insert("stopSequences".into(), serde_json::json!(stop));
        }
        if !generation_config.is_empty() {
            body["generationConfig"] = serde_json::Value::Object(generation_config);
        }

        if let Some(ref tools) = request.tools {
            let gemini_tools: Vec<serde_json::Value> = tools.iter().map(|t| {
                serde_json::json!({
                    "functionDeclarations": [{
                        "name": t.function.name,
                        "description": t.function.description,
                        "parameters": t.function.parameters,
                    }]
                })
            }).collect();
            body["tools"] = serde_json::json!(gemini_tools);
        }

        // Build streaming URL
        let url = format!(
            "{}/models/{}:streamGenerateContent?key={}&alt=sse",
            self.endpoint,
            model,
            self.api_key
        );

        // Make streaming API request
        let client = reqwest::Client::new();
        let response = client
            .post(&url)
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
            let stream_id = uuid::Uuid::new_v4().to_string();
            let mut sent_role = false;

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
                                    match serde_json::from_str::<serde_json::Value>(data) {
                                        Ok(json) => {
                                            if let Some(candidates) = json.get("candidates").and_then(|c| c.as_array()) {
                                                for candidate in candidates {
                                                    let content = &candidate["content"];
                                                    if let Some(parts) = content.get("parts").and_then(|p| p.as_array()) {
                                                        for part in parts {
                                                            let text = part.get("text").and_then(|t| t.as_str()).map(|s| s.to_string());

                                                            // Handle function call
                                                            let tool_calls = part.get("functionCall").map(|fc| {
                                                                vec![super::ToolCallDelta {
                                                                    index: 0,
                                                                    id: Some(uuid::Uuid::new_v4().to_string()),
                                                                    call_type: Some("function".to_string()),
                                                                    function: Some(super::FunctionCallDelta {
                                                                        name: fc.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()),
                                                                        arguments: fc.get("args").map(|a| serde_json::to_string(a).unwrap_or_default()),
                                                                    }),
                                                                }]
                                                            });

                                                            let chunk = StreamChunk {
                                                                id: stream_id.clone(),
                                                                delta: super::ChatDelta {
                                                                    role: if !sent_role {
                                                                        sent_role = true;
                                                                        Some(MessageRole::Assistant)
                                                                    } else {
                                                                        None
                                                                    },
                                                                    content: text,
                                                                    function_call: None,
                                                                    tool_calls,
                                                                },
                                                                finish_reason: candidate.get("finishReason").and_then(|f| f.as_str()).map(|s| s.to_string()),
                                                            };
                                                            yield Ok(chunk);
                                                        }
                                                    }
                                                }
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
        // Approximate
        Ok(text.len() / 4)
    }

    fn supports_model(&self, model: &str) -> bool {
        Self::available_models().iter().any(|m| m.id == model)
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        Self::available_models().into_iter().find(|m| m.id == model)
    }
}
