//! Pluggable embedder. Nano ships no inference runtime; a user who
//! wants `body_vec` populated points `HttpEmbedder` at an external
//! endpoint (ollama, voyage, openai, custom — any service returning
//! `{"embedding": [f32, ...]}` for a POSTed `{"input": "..."}`).
//!
//! If no embedder is configured, `NoopEmbedder` is used and `body_vec`
//! stays `NULL`. BM25 and hybrid retrieval still work — every callable
//! `lsp_*` function has a BM25 or literal-match path.

use crate::{Error, Result};

/// Dimensionality is determined by the server; Nano doesn't care as
/// long as the vector is stable across calls.
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> Result<Option<Vec<f32>>>;
}

/// Default. Never emits a vector — `body_vec` stays `NULL`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEmbedder;

impl Embedder for NoopEmbedder {
    fn embed(&self, _text: &str) -> Result<Option<Vec<f32>>> {
        Ok(None)
    }
}

/// Calls a configured HTTP endpoint at embed time. Keeps the request
/// shape deliberately simple:
///
/// ```json
/// POST <endpoint>    { "input": "<text>" }
/// -> 200             { "embedding": [0.1, 0.2, ...] }
/// ```
///
/// Users who need a different wire shape can write their own
/// `Embedder`. Phase 1 scope is the above plus a bearer token.
#[derive(Debug, Clone)]
pub struct HttpEmbedder {
    endpoint: String,
    bearer_token: Option<String>,
    timeout_ms: u64,
}

impl HttpEmbedder {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            bearer_token: None,
            timeout_ms: 30_000,
        }
    }

    pub fn with_bearer(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }
}

impl Embedder for HttpEmbedder {
    fn embed(&self, text: &str) -> Result<Option<Vec<f32>>> {
        // Use blocking reqwest — the index path is itself synchronous
        // (runs inside a single `code_index` call), and we'd otherwise
        // need to pull in a Tokio runtime inside the embedded path.
        // reqwest is already a workspace dependency.
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_millis(self.timeout_ms))
            .build()
            .map_err(|e| Error::query_execution(format!("embedder client: {e}")))?;

        let body = serde_json::json!({ "input": text });
        let mut req = client.post(&self.endpoint).json(&body);
        if let Some(tok) = &self.bearer_token {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .map_err(|e| Error::query_execution(format!("embedder request: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::query_execution(format!(
                "embedder returned HTTP {}",
                resp.status()
            )));
        }
        let parsed: EmbeddingResponse = resp
            .json()
            .map_err(|e| Error::query_execution(format!("embedder response: {e}")))?;
        Ok(Some(parsed.embedding))
    }
}

#[derive(serde::Deserialize)]
struct EmbeddingResponse {
    embedding: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_returns_none() {
        let e = NoopEmbedder;
        assert!(e.embed("anything").unwrap().is_none());
    }
}
