//! HeliosDB client implementation

use crate::error::{Error, Result};
use crate::types::*;
use reqwest::Client as HttpClient;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Client configuration
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Base URL of HeliosDB instance
    pub base_url: String,
    /// API key for authentication
    pub api_key: Option<String>,
    /// Default branch
    pub branch: String,
    /// Request timeout
    pub timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:8080".to_string(),
            api_key: None,
            branch: "main".to_string(),
            timeout: Duration::from_secs(30),
        }
    }
}

/// HeliosDB client
#[derive(Clone)]
pub struct Client {
    config: ClientConfig,
    http: HttpClient,
}

impl Client {
    /// Create a new client
    pub fn new(config: ClientConfig) -> Result<Self> {
        let http = HttpClient::builder()
            .timeout(config.timeout)
            .build()?;

        Ok(Self { config, http })
    }

    /// Execute a SQL query
    pub async fn query(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<QueryResult> {
        self.query_on_branch(&self.config.branch, sql, params).await
    }

    /// Execute a SQL query on a specific branch
    pub async fn query_on_branch(
        &self,
        branch: &str,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<QueryResult> {
        #[derive(Serialize)]
        struct QueryBody {
            sql: String,
            params: Vec<serde_json::Value>,
        }

        let body = QueryBody {
            sql: sql.to_string(),
            params: params.iter().map(|p| p.to_sql_value()).collect(),
        };

        self.post(&format!("/v1/branches/{}/query", branch), &body)
            .await
    }

    /// Execute a statement and return rows affected
    pub async fn exec(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<i64> {
        let result = self.query(sql, params).await?;
        Ok(result.rows_affected)
    }

    /// Start a vector search builder
    pub fn vector_search<'a>(&'a self, store: &'a str, query: &'a str) -> VectorSearchBuilder<'a> {
        VectorSearchBuilder {
            client: self,
            store,
            query: VectorSearchQuery::Text(query.to_string()),
            options: VectorSearchOptions::default(),
        }
    }

    /// Search by vector
    pub fn vector_search_by_vector<'a>(
        &'a self,
        store: &'a str,
        vector: Vec<f32>,
    ) -> VectorSearchBuilder<'a> {
        VectorSearchBuilder {
            client: self,
            store,
            query: VectorSearchQuery::Vector(vector),
            options: VectorSearchOptions::default(),
        }
    }

    /// Store text with automatic embedding
    pub async fn store_text(
        &self,
        store: &str,
        text: &str,
        metadata: Option<HashMap<String, serde_json::Value>>,
    ) -> Result<String> {
        #[derive(Serialize)]
        struct StoreBody {
            texts: Vec<String>,
            metadatas: Vec<HashMap<String, serde_json::Value>>,
        }

        #[derive(Deserialize)]
        struct StoreResponse {
            ids: Vec<String>,
        }

        let body = StoreBody {
            texts: vec![text.to_string()],
            metadatas: vec![metadata.unwrap_or_default()],
        };

        let response: StoreResponse = self
            .post(&format!("/v1/vectors/stores/{}/texts", store), &body)
            .await?;

        Ok(response.ids.into_iter().next().unwrap_or_default())
    }

    /// List all vector stores
    pub async fn list_vector_stores(&self) -> Result<Vec<VectorStore>> {
        #[derive(Deserialize)]
        struct Response {
            stores: Vec<VectorStore>,
        }

        let response: Response = self.get("/v1/vectors/stores").await?;
        Ok(response.stores)
    }

    /// Create a vector store
    pub async fn create_vector_store(
        &self,
        name: &str,
        dimensions: usize,
        metric: &str,
    ) -> Result<VectorStore> {
        #[derive(Serialize)]
        struct CreateBody {
            name: String,
            dimensions: usize,
            metric: String,
        }

        self.post(
            "/v1/vectors/stores",
            &CreateBody {
                name: name.to_string(),
                dimensions,
                metric: metric.to_string(),
            },
        )
        .await
    }

    /// List all branches
    pub async fn list_branches(&self) -> Result<Vec<Branch>> {
        #[derive(Deserialize)]
        struct Response {
            branches: Vec<Branch>,
        }

        let response: Response = self.get("/v1/branches").await?;
        Ok(response.branches)
    }

    /// Create a new branch
    pub async fn create_branch(&self, name: &str, from_branch: &str) -> Result<Branch> {
        #[derive(Serialize)]
        struct CreateBody {
            name: String,
            from_branch: String,
        }

        self.post(
            "/v1/branches",
            &CreateBody {
                name: name.to_string(),
                from_branch: from_branch.to_string(),
            },
        )
        .await
    }

    /// Merge branches
    pub async fn merge_branch(&self, source: &str, target: &str) -> Result<()> {
        #[derive(Serialize)]
        struct MergeBody {
            target: String,
        }

        self.post::<_, ()>(
            &format!("/v1/branches/{}/merge", source),
            &MergeBody {
                target: target.to_string(),
            },
        )
        .await
    }

    /// Get agent memory for a session
    pub fn memory(&self, session_id: &str) -> AgentMemory {
        AgentMemory {
            client: self.clone(),
            session_id: session_id.to_string(),
        }
    }

    /// Query at a specific point in time
    pub async fn query_at(
        &self,
        sql: &str,
        timestamp: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<QueryResult> {
        #[derive(Serialize)]
        struct QueryBody {
            sql: String,
            params: Vec<serde_json::Value>,
            as_of_timestamp: String,
        }

        let body = QueryBody {
            sql: sql.to_string(),
            params: params.iter().map(|p| p.to_sql_value()).collect(),
            as_of_timestamp: timestamp.to_string(),
        };

        self.post(&format!("/v1/branches/{}/query", self.config.branch), &body)
            .await
    }

    /// Natural language query
    pub async fn nl_query(&self, question: &str) -> Result<(QueryResult, String)> {
        #[derive(Serialize)]
        struct NLBody {
            question: String,
            branch: String,
        }

        let response: NLQueryResponse = self
            .post(
                "/v1/nl/query",
                &NLBody {
                    question: question.to_string(),
                    branch: self.config.branch.clone(),
                },
            )
            .await?;

        let result = QueryResult {
            rows: response.rows,
            columns: response.columns,
            rows_affected: 0,
        };

        Ok((result, response.sql))
    }

    /// Check server health
    pub async fn health(&self) -> Result<Health> {
        self.get("/health").await
    }

    /// Set default branch
    pub fn set_branch(&mut self, branch: &str) {
        self.config.branch = branch.to_string();
    }

    /// Get current branch
    pub fn branch(&self) -> &str {
        &self.config.branch
    }

    // HTTP helpers

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{}", self.config.base_url, path);
        let mut request = self.http.get(&url);

        if let Some(ref key) = self.config.api_key {
            request = request.header("X-API-Key", key);
        }

        let response = request.send().await?;
        self.handle_response(response).await
    }

    async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T> {
        let url = format!("{}{}", self.config.base_url, path);
        let mut request = self.http.post(&url).json(body);

        if let Some(ref key) = self.config.api_key {
            request = request.header("X-API-Key", key);
        }

        let response = request.send().await?;
        self.handle_response(response).await
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let url = format!("{}{}", self.config.base_url, path);
        let mut request = self.http.delete(&url);

        if let Some(ref key) = self.config.api_key {
            request = request.header("X-API-Key", key);
        }

        let response = request.send().await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(Error::Api {
                status: response.status().as_u16(),
                message: response.text().await.unwrap_or_default(),
            })
        }
    }

    async fn handle_response<T: DeserializeOwned>(&self, response: reqwest::Response) -> Result<T> {
        let status = response.status();

        if status.is_success() {
            Ok(response.json().await?)
        } else {
            let message = response.text().await.unwrap_or_default();

            Err(match status.as_u16() {
                401 => Error::Authentication,
                404 => Error::NotFound(message),
                429 => Error::RateLimit,
                _ => Error::Api {
                    status: status.as_u16(),
                    message,
                },
            })
        }
    }
}

/// Vector search query type
pub enum VectorSearchQuery {
    Text(String),
    Vector(Vec<f32>),
}

/// Vector search builder
pub struct VectorSearchBuilder<'a> {
    client: &'a Client,
    store: &'a str,
    query: VectorSearchQuery,
    options: VectorSearchOptions,
}

impl<'a> VectorSearchBuilder<'a> {
    /// Set maximum results
    pub fn top_k(mut self, k: usize) -> Self {
        self.options.top_k = Some(k);
        self
    }

    /// Set minimum similarity score
    pub fn min_score(mut self, score: f64) -> Self {
        self.options.min_score = Some(score);
        self
    }

    /// Set metadata filter
    pub fn filter(mut self, filter: HashMap<String, serde_json::Value>) -> Self {
        self.options.filter = Some(filter);
        self
    }

    /// Execute the search
    pub async fn execute(self) -> Result<Vec<VectorSearchResult>> {
        #[derive(Serialize)]
        struct SearchBody {
            #[serde(skip_serializing_if = "Option::is_none")]
            text: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            vector: Option<Vec<f32>>,
            top_k: usize,
            #[serde(skip_serializing_if = "Option::is_none")]
            min_score: Option<f64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            filter: Option<HashMap<String, serde_json::Value>>,
        }

        #[derive(Deserialize)]
        struct SearchResponse {
            results: Vec<VectorSearchResult>,
        }

        let (text, vector) = match self.query {
            VectorSearchQuery::Text(t) => (Some(t), None),
            VectorSearchQuery::Vector(v) => (None, Some(v)),
        };

        let endpoint = if text.is_some() {
            format!("/v1/vectors/stores/{}/search/text", self.store)
        } else {
            format!("/v1/vectors/stores/{}/search", self.store)
        };

        let body = SearchBody {
            text,
            vector,
            top_k: self.options.top_k.unwrap_or(10),
            min_score: self.options.min_score,
            filter: self.options.filter,
        };

        let response: SearchResponse = self.client.post(&endpoint, &body).await?;
        Ok(response.results)
    }
}

/// Agent memory interface
#[derive(Clone)]
pub struct AgentMemory {
    client: Client,
    session_id: String,
}

impl AgentMemory {
    /// Add a message to memory
    pub async fn add(&self, role: &str, content: &str) -> Result<()> {
        #[derive(Serialize)]
        struct AddBody {
            role: String,
            content: String,
        }

        self.client
            .post::<_, ()>(
                &format!("/v1/agents/memory/{}/add", self.session_id),
                &AddBody {
                    role: role.to_string(),
                    content: content.to_string(),
                },
            )
            .await
    }

    /// Get messages from memory
    pub async fn get(&self, limit: usize) -> Result<Vec<Message>> {
        #[derive(Deserialize)]
        struct Response {
            messages: Vec<Message>,
        }

        let response: Response = self
            .client
            .get(&format!(
                "/v1/agents/memory/{}/messages?limit={}",
                self.session_id, limit
            ))
            .await?;

        Ok(response.messages)
    }

    /// Search memory semantically
    pub async fn search(&self, query: &str, top_k: usize) -> Result<Vec<VectorSearchResult>> {
        #[derive(Serialize)]
        struct SearchBody {
            query: String,
            top_k: usize,
        }

        #[derive(Deserialize)]
        struct Response {
            results: Vec<VectorSearchResult>,
        }

        let response: Response = self
            .client
            .post(
                &format!("/v1/agents/memory/{}/search", self.session_id),
                &SearchBody {
                    query: query.to_string(),
                    top_k,
                },
            )
            .await?;

        Ok(response.results)
    }

    /// Clear memory
    pub async fn clear(&self) -> Result<()> {
        self.client
            .delete(&format!("/v1/agents/memory/{}", self.session_id))
            .await
    }
}

/// Trait for converting values to SQL parameters
pub trait ToSqlValue {
    fn to_sql_value(&self) -> serde_json::Value;
}

impl ToSqlValue for i32 {
    fn to_sql_value(&self) -> serde_json::Value {
        serde_json::Value::Number((*self).into())
    }
}

impl ToSqlValue for i64 {
    fn to_sql_value(&self) -> serde_json::Value {
        serde_json::Value::Number((*self).into())
    }
}

impl ToSqlValue for f64 {
    fn to_sql_value(&self) -> serde_json::Value {
        serde_json::Number::from_f64(*self)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    }
}

impl ToSqlValue for &str {
    fn to_sql_value(&self) -> serde_json::Value {
        serde_json::Value::String(self.to_string())
    }
}

impl ToSqlValue for String {
    fn to_sql_value(&self) -> serde_json::Value {
        serde_json::Value::String(self.clone())
    }
}

impl ToSqlValue for bool {
    fn to_sql_value(&self) -> serde_json::Value {
        serde_json::Value::Bool(*self)
    }
}

impl<T: ToSqlValue> ToSqlValue for Option<T> {
    fn to_sql_value(&self) -> serde_json::Value {
        match self {
            Some(v) => v.to_sql_value(),
            None => serde_json::Value::Null,
        }
    }
}
