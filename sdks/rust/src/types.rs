//! Type definitions for HeliosDB client

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Query result from SQL execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    /// Result rows
    pub rows: Vec<HashMap<String, serde_json::Value>>,
    /// Column names
    pub columns: Vec<String>,
    /// Number of rows affected (for INSERT/UPDATE/DELETE)
    #[serde(default)]
    pub rows_affected: i64,
}

/// Vector store information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorStore {
    /// Store name
    pub name: String,
    /// Vector dimensions
    pub dimensions: usize,
    /// Distance metric
    pub metric: String,
    /// Number of vectors stored
    pub count: i64,
}

/// Vector search result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorSearchResult {
    /// Document ID
    pub id: String,
    /// Similarity score
    pub score: f64,
    /// Document content (if available)
    #[serde(default)]
    pub content: Option<String>,
    /// Document metadata
    #[serde(default)]
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

/// Database branch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    /// Branch name
    pub name: String,
    /// Parent branch (if any)
    pub parent: Option<String>,
    /// Creation timestamp
    pub created_at: String,
}

/// Chat message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Role (user, assistant, system)
    pub role: String,
    /// Message content
    pub content: String,
    /// Timestamp
    #[serde(default)]
    pub timestamp: Option<String>,
}

/// Table information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    /// Table name
    pub name: String,
    /// Table columns
    pub columns: Vec<Column>,
    /// Row count (if available)
    #[serde(default)]
    pub row_count: Option<i64>,
}

/// Column information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    /// Column name
    pub name: String,
    /// Data type
    pub data_type: String,
    /// Is nullable
    pub nullable: bool,
    /// Is primary key
    #[serde(default)]
    pub primary_key: bool,
}

/// Health check response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    /// Status (healthy, degraded, unhealthy)
    pub status: String,
    /// Version
    pub version: Option<String>,
}

/// Natural language query response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NLQueryResponse {
    /// Generated SQL
    pub sql: String,
    /// Query results
    pub rows: Vec<HashMap<String, serde_json::Value>>,
    /// Column names
    pub columns: Vec<String>,
}

/// Vector search options builder
#[derive(Debug, Clone, Default)]
pub struct VectorSearchOptions {
    pub(crate) top_k: Option<usize>,
    pub(crate) min_score: Option<f64>,
    pub(crate) filter: Option<HashMap<String, serde_json::Value>>,
    pub(crate) include_vectors: bool,
    pub(crate) include_metadata: bool,
}

impl VectorSearchOptions {
    /// Create new options
    pub fn new() -> Self {
        Self::default()
    }

    /// Set maximum results
    pub fn top_k(mut self, k: usize) -> Self {
        self.top_k = Some(k);
        self
    }

    /// Set minimum similarity score
    pub fn min_score(mut self, score: f64) -> Self {
        self.min_score = Some(score);
        self
    }

    /// Set metadata filter
    pub fn filter(mut self, filter: HashMap<String, serde_json::Value>) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Include vectors in response
    pub fn include_vectors(mut self) -> Self {
        self.include_vectors = true;
        self
    }

    /// Include metadata in response
    pub fn include_metadata(mut self) -> Self {
        self.include_metadata = true;
        self
    }
}

/// SQL parameter value
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SqlValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Json(serde_json::Value),
}

impl From<i32> for SqlValue {
    fn from(v: i32) -> Self {
        SqlValue::Int(v as i64)
    }
}

impl From<i64> for SqlValue {
    fn from(v: i64) -> Self {
        SqlValue::Int(v)
    }
}

impl From<f64> for SqlValue {
    fn from(v: f64) -> Self {
        SqlValue::Float(v)
    }
}

impl From<&str> for SqlValue {
    fn from(v: &str) -> Self {
        SqlValue::String(v.to_string())
    }
}

impl From<String> for SqlValue {
    fn from(v: String) -> Self {
        SqlValue::String(v)
    }
}

impl From<bool> for SqlValue {
    fn from(v: bool) -> Self {
        SqlValue::Bool(v)
    }
}

impl<T: Into<SqlValue>> From<Option<T>> for SqlValue {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(val) => val.into(),
            None => SqlValue::Null,
        }
    }
}
