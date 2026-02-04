//! LLM Integration for Natural Language Query Explanations
//!
//! This module provides integration with Large Language Models (LLMs) to generate
//! natural language explanations of query execution plans.
//!
//! Features:
//! - Converts technical query plans to human-readable narratives
//! - Provides optimization suggestions in plain English
//! - Explains performance bottlenecks and their causes
//! - Suggests query refinements and index improvements

use crate::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// LLM provider type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LLMProvider {
    /// OpenAI API (GPT-4, GPT-3.5, etc.)
    OpenAI,
    /// Anthropic Claude
    Anthropic,
    /// Local model via Ollama
    Ollama,
    /// Azure OpenAI
    AzureOpenAI,
    /// Custom endpoint
    Custom,
}

/// LLM configuration
#[derive(Debug, Clone)]
pub struct LLMConfig {
    pub provider: LLMProvider,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub model: String,
    pub timeout: Duration,
    pub max_tokens: usize,
    pub temperature: f32,
}

impl Default for LLMConfig {
    fn default() -> Self {
        Self {
            provider: LLMProvider::OpenAI,
            endpoint: "https://api.openai.com/v1/chat/completions".to_string(),
            api_key: None,
            model: "gpt-4".to_string(),
            timeout: Duration::from_secs(30),
            max_tokens: 1000,
            temperature: 0.7,
        }
    }
}

impl LLMConfig {
    /// Create config for OpenAI
    pub fn openai(api_key: String, model: impl Into<String>) -> Self {
        Self {
            provider: LLMProvider::OpenAI,
            endpoint: "https://api.openai.com/v1/chat/completions".to_string(),
            api_key: Some(api_key),
            model: model.into(),
            ..Default::default()
        }
    }

    /// Create config for Anthropic Claude
    pub fn anthropic(api_key: String, model: impl Into<String>) -> Self {
        Self {
            provider: LLMProvider::Anthropic,
            endpoint: "https://api.anthropic.com/v1/messages".to_string(),
            api_key: Some(api_key),
            model: model.into(),
            ..Default::default()
        }
    }

    /// Create config for local Ollama
    pub fn ollama(model: impl Into<String>) -> Self {
        Self {
            provider: LLMProvider::Ollama,
            endpoint: "http://localhost:11434/api/generate".to_string(),
            api_key: None,
            model: model.into(),
            ..Default::default()
        }
    }
}

/// LLM-powered query explainer
pub struct LLMExplainer {
    config: LLMConfig,
    client: Option<reqwest::Client>,
}

impl LLMExplainer {
    /// Create a new LLM explainer
    pub fn new(config: LLMConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .ok();

        Self { config, client }
    }

    /// Generate natural language explanation for a query plan
    pub async fn explain_plan(
        &self,
        plan_json: &str,
        total_cost: f64,
        total_rows: usize
    ) -> Result<LLMExplanation> {
        // If no client (network disabled), fall back to rule-based explanation
        if self.client.is_none() {
            return Ok(self.fallback_explanation(plan_json, total_cost, total_rows));
        }

        // Build prompt
        let prompt = self.build_prompt(plan_json, total_cost, total_rows);

        // Call LLM API
        match self.config.provider {
            LLMProvider::OpenAI | LLMProvider::AzureOpenAI => {
                self.call_openai(&prompt).await
            }
            LLMProvider::Anthropic => {
                self.call_anthropic(&prompt).await
            }
            LLMProvider::Ollama => {
                self.call_ollama(&prompt).await
            }
            LLMProvider::Custom => {
                self.call_custom(&prompt).await
            }
        }
    }

    /// Build prompt for LLM
    fn build_prompt(&self, plan_json: &str, total_cost: f64, total_rows: usize) -> String {
        format!(
            r#"You are a database query optimization expert. Explain the following query execution plan in simple, clear language.

Query Execution Plan:
```json
{}
```

Total Estimated Cost: {}
Total Estimated Rows: {}

Please provide:
1. A high-level summary of what this query does (2-3 sentences)
2. Step-by-step walkthrough of the execution plan
3. Performance analysis - is this fast, moderate, or slow? Why?
4. Specific optimization suggestions (if any)
5. Potential issues or warnings (if any)

Format your response as JSON with these fields:
- summary: string
- walkthrough: array of strings (each step)
- performance_category: "Fast" | "Moderate" | "Slow" | "Very Slow"
- estimated_time_ms: number
- bottlenecks: array of strings
- performance_explanation: string
- suggestions: array of strings
- warnings: array of strings
"#,
            plan_json, total_cost, total_rows
        )
    }

    /// Call OpenAI API
    async fn call_openai(&self, prompt: &str) -> Result<LLMExplanation> {
        #[derive(Serialize)]
        struct OpenAIRequest {
            model: String,
            messages: Vec<Message>,
            max_tokens: usize,
            temperature: f32,
        }

        #[derive(Serialize)]
        struct Message {
            role: String,
            content: String,
        }

        #[derive(Deserialize)]
        struct OpenAIResponse {
            choices: Vec<Choice>,
        }

        #[derive(Deserialize)]
        struct Choice {
            message: Message2,
        }

        #[derive(Deserialize)]
        struct Message2 {
            content: String,
        }

        let client = self.client.as_ref().ok_or_else(|| {
            crate::Error::query_execution("HTTP client not configured for LLM explainer".to_string())
        })?;

        let request = OpenAIRequest {
            model: self.config.model.clone(),
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: "You are a helpful database query optimization expert.".to_string(),
                },
                Message {
                    role: "user".to_string(),
                    content: prompt.to_string(),
                },
            ],
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
        };

        let response = client
            .post(&self.config.endpoint)
            .header("Authorization", format!("Bearer {}", self.config.api_key.as_ref().unwrap_or(&String::new())))
            .json(&request)
            .send()
            .await
            .map_err(|e| crate::Error::query_execution(format!("LLM API error: {}", e)))?;

        let response: OpenAIResponse = response
            .json()
            .await
            .map_err(|e| crate::Error::query_execution(format!("LLM response parse error: {}", e)))?;

        let content = response.choices
            .get(0)
            .map(|c| c.message.content.clone())
            .unwrap_or_default();

        // Parse JSON response
        self.parse_llm_response(&content)
    }

    /// Call Anthropic Claude API
    async fn call_anthropic(&self, prompt: &str) -> Result<LLMExplanation> {
        #[derive(Serialize)]
        struct AnthropicRequest {
            model: String,
            max_tokens: usize,
            temperature: f32,
            messages: Vec<Message>,
        }

        #[derive(Serialize)]
        struct Message {
            role: String,
            content: String,
        }

        #[derive(Deserialize)]
        struct AnthropicResponse {
            content: Vec<ContentBlock>,
        }

        #[derive(Deserialize)]
        struct ContentBlock {
            text: String,
        }

        let client = self.client.as_ref().ok_or_else(|| {
            crate::Error::query_execution("HTTP client not configured for LLM explainer".to_string())
        })?;

        let request = AnthropicRequest {
            model: self.config.model.clone(),
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: prompt.to_string(),
                },
            ],
        };

        let response = client
            .post(&self.config.endpoint)
            .header("x-api-key", self.config.api_key.as_ref().unwrap_or(&String::new()))
            .header("anthropic-version", "2023-06-01")
            .json(&request)
            .send()
            .await
            .map_err(|e| crate::Error::query_execution(format!("LLM API error: {}", e)))?;

        let response: AnthropicResponse = response
            .json()
            .await
            .map_err(|e| crate::Error::query_execution(format!("LLM response parse error: {}", e)))?;

        let content = response.content
            .get(0)
            .map(|c| c.text.clone())
            .unwrap_or_default();

        self.parse_llm_response(&content)
    }

    /// Call Ollama API
    async fn call_ollama(&self, prompt: &str) -> Result<LLMExplanation> {
        #[derive(Serialize)]
        struct OllamaRequest {
            model: String,
            prompt: String,
            stream: bool,
        }

        #[derive(Deserialize)]
        struct OllamaResponse {
            response: String,
        }

        let client = self.client.as_ref().ok_or_else(|| {
            crate::Error::query_execution("HTTP client not configured for LLM explainer".to_string())
        })?;

        let request = OllamaRequest {
            model: self.config.model.clone(),
            prompt: prompt.to_string(),
            stream: false,
        };

        let response = client
            .post(&self.config.endpoint)
            .json(&request)
            .send()
            .await
            .map_err(|e| crate::Error::query_execution(format!("LLM API error: {}", e)))?;

        let response: OllamaResponse = response
            .json()
            .await
            .map_err(|e| crate::Error::query_execution(format!("LLM response parse error: {}", e)))?;

        self.parse_llm_response(&response.response)
    }

    /// Call custom API endpoint
    async fn call_custom(&self, prompt: &str) -> Result<LLMExplanation> {
        // Custom implementation - similar to OpenAI
        self.call_openai(prompt).await
    }

    /// Parse LLM JSON response
    fn parse_llm_response(&self, content: &str) -> Result<LLMExplanation> {
        // Try to extract JSON from markdown code blocks
        let json_str = if content.contains("```json") {
            content
                .split("```json")
                .nth(1)
                .and_then(|s| s.split("```").next())
                .unwrap_or(content)
                .trim()
        } else {
            content.trim()
        };

        #[derive(Deserialize)]
        struct LLMResponse {
            summary: String,
            walkthrough: Vec<String>,
            performance_category: String,
            estimated_time_ms: f64,
            bottlenecks: Vec<String>,
            performance_explanation: String,
            suggestions: Vec<String>,
            warnings: Vec<String>,
        }

        let response: LLMResponse = serde_json::from_str(json_str)
            .map_err(|e| crate::Error::query_execution(format!("Failed to parse LLM response: {}", e)))?;

        Ok(LLMExplanation {
            summary: response.summary,
            walkthrough: response.walkthrough,
            performance_category: response.performance_category,
            estimated_time_ms: response.estimated_time_ms,
            bottlenecks: response.bottlenecks,
            performance_explanation: response.performance_explanation,
            suggestions: response.suggestions,
            warnings: response.warnings,
        })
    }

    /// Fallback to rule-based explanation when LLM is unavailable
    fn fallback_explanation(&self, _plan_json: &str, total_cost: f64, total_rows: usize) -> LLMExplanation {
        let (category, time, bottlenecks) = if total_cost < 100.0 {
            ("Fast", total_cost / 10.0, vec![])
        } else if total_cost < 1000.0 {
            ("Moderate", total_cost / 5.0, vec!["Sequential scan on table".to_string()])
        } else {
            ("Slow", total_cost / 2.0, vec!["Large table scan without index".to_string()])
        };

        LLMExplanation {
            summary: format!(
                "This query processes approximately {} rows with an estimated cost of {:.2}.",
                total_rows, total_cost
            ),
            walkthrough: vec![
                "Query execution starts with data retrieval".to_string(),
                "Filters and predicates are applied".to_string(),
                "Results are projected and returned".to_string(),
            ],
            performance_category: category.to_string(),
            estimated_time_ms: time,
            bottlenecks,
            performance_explanation: format!(
                "Query is categorized as '{}' with estimated execution time of {:.2}ms",
                category, time
            ),
            suggestions: vec![
                "Consider adding indexes on frequently queried columns".to_string(),
            ],
            warnings: vec![],
        }
    }
}

/// LLM explanation result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMExplanation {
    pub summary: String,
    pub walkthrough: Vec<String>,
    pub performance_category: String,
    pub estimated_time_ms: f64,
    pub bottlenecks: Vec<String>,
    pub performance_explanation: String,
    pub suggestions: Vec<String>,
    pub warnings: Vec<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_config_openai() {
        let config = LLMConfig::openai("sk-test".to_string(), "gpt-4");
        assert_eq!(config.provider, LLMProvider::OpenAI);
        assert_eq!(config.model, "gpt-4");
        assert!(config.api_key.is_some());
    }

    #[test]
    fn test_llm_config_anthropic() {
        let config = LLMConfig::anthropic("sk-ant-test".to_string(), "claude-3-opus");
        assert_eq!(config.provider, LLMProvider::Anthropic);
        assert_eq!(config.model, "claude-3-opus");
    }

    #[test]
    fn test_llm_config_ollama() {
        let config = LLMConfig::ollama("llama2");
        assert_eq!(config.provider, LLMProvider::Ollama);
        assert_eq!(config.model, "llama2");
        assert!(config.api_key.is_none());
    }

    #[test]
    fn test_fallback_explanation() {
        let config = LLMConfig::default();
        let explainer = LLMExplainer::new(config);

        let explanation = explainer.fallback_explanation("{}", 500.0, 1000);

        assert!(!explanation.summary.is_empty());
        assert!(!explanation.walkthrough.is_empty());
        assert_eq!(explanation.performance_category, "Moderate");
    }

    #[test]
    fn test_parse_llm_response() {
        let config = LLMConfig::default();
        let explainer = LLMExplainer::new(config);

        let json_response = r#"{
            "summary": "Test summary",
            "walkthrough": ["Step 1", "Step 2"],
            "performance_category": "Fast",
            "estimated_time_ms": 10.5,
            "bottlenecks": [],
            "performance_explanation": "Query is fast",
            "suggestions": ["Add index"],
            "warnings": []
        }"#;

        let result = explainer.parse_llm_response(json_response);
        assert!(result.is_ok());

        let explanation = result.unwrap();
        assert_eq!(explanation.summary, "Test summary");
        assert_eq!(explanation.walkthrough.len(), 2);
        assert_eq!(explanation.performance_category, "Fast");
    }

    #[test]
    fn test_parse_llm_response_with_markdown() {
        let config = LLMConfig::default();
        let explainer = LLMExplainer::new(config);

        let markdown_response = r#"Here's the explanation:

```json
{
    "summary": "Test summary",
    "walkthrough": ["Step 1"],
    "performance_category": "Moderate",
    "estimated_time_ms": 50.0,
    "bottlenecks": ["Scan"],
    "performance_explanation": "Moderate performance",
    "suggestions": [],
    "warnings": ["Large table"]
}
```

Hope this helps!"#;

        let result = explainer.parse_llm_response(markdown_response);
        assert!(result.is_ok());

        let explanation = result.unwrap();
        assert_eq!(explanation.summary, "Test summary");
    }
}
