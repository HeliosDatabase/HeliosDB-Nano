//! Query DTOs (Data Transfer Objects)
//!
//! Request and response models for SQL query execution.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Request to execute a read-only SQL query
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QueryRequest {
    /// SQL query to execute
    pub sql: String,

    /// Query parameters (optional, for parameterized queries)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Vec<QueryParameter>>,

    /// Time-travel specification (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of: Option<AsOfSpec>,

    /// Query timeout in milliseconds (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// Request to execute a DDL/DML statement
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExecuteRequest {
    /// SQL statement to execute
    pub sql: String,

    /// Statement parameters (optional, for parameterized queries)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Vec<QueryParameter>>,

    /// Statement timeout in milliseconds (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// Query parameter value
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum QueryParameter {
    /// Null value
    Null,
    /// Integer (32-bit)
    Int4(i32),
    /// Integer (64-bit)
    Int8(i64),
    /// Float (32-bit)
    Float4(f32),
    /// Float (64-bit)
    Float8(f64),
    /// Text string
    String(String),
    /// Boolean
    Boolean(bool),
    /// JSON value
    Json(serde_json::Value),
}

/// Time-travel specification for queries
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AsOfSpec {
    /// Current time (default)
    Now,

    /// Specific timestamp (Unix timestamp in milliseconds)
    Timestamp {
        /// Unix timestamp in milliseconds
        value: u64,
    },

    /// Specific transaction ID
    Transaction {
        /// Transaction ID
        value: u64,
    },

    /// System Change Number (SCN)
    Scn {
        /// SCN value
        value: u64,
    },
}

/// Response for a SQL query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResponse {
    /// Column names
    pub columns: Vec<String>,

    /// Column data types
    pub column_types: Vec<String>,

    /// Result rows (each row is a map of column_name -> value)
    pub rows: Vec<HashMap<String, serde_json::Value>>,

    /// Total number of rows returned
    pub row_count: usize,

    /// Execution time in milliseconds
    pub execution_time_ms: u64,
}

/// Response for a SQL statement execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteResponse {
    /// Statement type (e.g., INSERT, UPDATE, DELETE, CREATE TABLE)
    pub statement_type: String,

    /// Number of rows affected
    pub affected_rows: u64,

    /// Execution time in milliseconds
    pub execution_time_ms: u64,

    /// Optional message with additional information
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// Conversion implementations

impl From<QueryParameter> for crate::Value {
    fn from(param: QueryParameter) -> Self {
        match param {
            QueryParameter::Null => crate::Value::Null,
            QueryParameter::Int4(v) => crate::Value::Int4(v),
            QueryParameter::Int8(v) => crate::Value::Int8(v),
            QueryParameter::Float4(v) => crate::Value::Float4(v),
            QueryParameter::Float8(v) => crate::Value::Float8(v),
            QueryParameter::String(v) => crate::Value::String(v),
            QueryParameter::Boolean(v) => crate::Value::Boolean(v),
            QueryParameter::Json(v) => crate::Value::Json(v.to_string()),
        }
    }
}

impl From<&crate::Value> for serde_json::Value {
    fn from(value: &crate::Value) -> Self {
        match value {
            crate::Value::Null => serde_json::Value::Null,
            crate::Value::Boolean(v) => serde_json::Value::Bool(*v),
            crate::Value::Int2(v) => serde_json::Value::Number((*v).into()),
            crate::Value::Int4(v) => serde_json::Value::Number((*v).into()),
            crate::Value::Int8(v) => serde_json::Value::Number((*v).into()),
            crate::Value::Float4(v) => {
                serde_json::Number::from_f64(*v as f64)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            }
            crate::Value::Float8(v) => {
                serde_json::Number::from_f64(*v)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            }
            crate::Value::Numeric(v) => {
                // Try to parse as a JSON number, preserving precision
                v.parse::<serde_json::Number>()
                    .map(serde_json::Value::Number)
                    .unwrap_or_else(|_| serde_json::Value::String(v.clone()))
            }
            crate::Value::String(v) => serde_json::Value::String(v.clone()),
            crate::Value::Bytes(v) => {
                // Encode binary data as base64
                serde_json::Value::String(STANDARD.encode(v))
            }
            crate::Value::Uuid(v) => serde_json::Value::String(v.to_string()),
            crate::Value::Timestamp(v) => serde_json::Value::String(v.to_rfc3339()),
            crate::Value::Date(v) => serde_json::Value::String(v.format("%Y-%m-%d").to_string()),
            crate::Value::Time(v) => serde_json::Value::String(v.format("%H:%M:%S%.f").to_string()),
            crate::Value::Json(v) => {
                // Parse the JSON string to serde_json::Value
                serde_json::from_str(v).unwrap_or(serde_json::Value::Null)
            }
            crate::Value::Array(v) => {
                serde_json::Value::Array(
                    v.iter().map(|val| serde_json::Value::from(val)).collect()
                )
            }
            crate::Value::Vector(v) => {
                serde_json::Value::Array(
                    v.iter().map(|f| {
                        serde_json::Number::from_f64(*f as f64)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null)
                    }).collect()
                )
            }
            // Storage references (should be resolved before API response)
            crate::Value::DictRef { dict_id } => {
                serde_json::Value::String(format!("dict:{}", dict_id))
            }
            crate::Value::CasRef { hash } => {
                serde_json::Value::String(format!("cas:{}", hex::encode(hash)))
            }
            crate::Value::ColumnarRef => serde_json::Value::Null,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_query_request_serialization() {
        let request = QueryRequest {
            sql: "SELECT * FROM users WHERE id = $1".to_string(),
            params: Some(vec![QueryParameter::Int4(1)]),
            as_of: None,
            timeout_ms: Some(5000),
        };

        let json = serde_json::to_string(&request).unwrap();
        let deserialized: QueryRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.sql, "SELECT * FROM users WHERE id = $1");
        assert!(deserialized.params.is_some());
        assert_eq!(deserialized.timeout_ms, Some(5000));
    }

    #[test]
    fn test_execute_request_serialization() {
        let request = ExecuteRequest {
            sql: "INSERT INTO users (id, name) VALUES ($1, $2)".to_string(),
            params: Some(vec![
                QueryParameter::Int4(1),
                QueryParameter::String("Alice".to_string()),
            ]),
            timeout_ms: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        let deserialized: ExecuteRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.sql, "INSERT INTO users (id, name) VALUES ($1, $2)");
        assert!(deserialized.params.is_some());
    }

    #[test]
    fn test_as_of_spec_serialization() {
        let specs = vec![
            AsOfSpec::Now,
            AsOfSpec::Timestamp { value: 1234567890 },
            AsOfSpec::Transaction { value: 42 },
            AsOfSpec::Scn { value: 100 },
        ];

        for spec in specs {
            let json = serde_json::to_string(&spec).unwrap();
            let deserialized: AsOfSpec = serde_json::from_str(&json).unwrap();
            // Successfully round-trips
            drop(deserialized);
        }
    }

    #[test]
    fn test_query_parameter_conversion() {
        let param = QueryParameter::Int4(42);
        let value: crate::Value = param.into();
        assert!(matches!(value, crate::Value::Int4(42)));

        let param = QueryParameter::String("test".to_string());
        let value: crate::Value = param.into();
        assert!(matches!(value, crate::Value::String(_)));
    }

    #[test]
    fn test_query_response_serialization() {
        let mut row = HashMap::new();
        row.insert("id".to_string(), serde_json::json!(1));
        row.insert("name".to_string(), serde_json::json!("Alice"));

        let response = QueryResponse {
            columns: vec!["id".to_string(), "name".to_string()],
            column_types: vec!["int4".to_string(), "text".to_string()],
            rows: vec![row],
            row_count: 1,
            execution_time_ms: 42,
        };

        let json = serde_json::to_string(&response).unwrap();
        let deserialized: QueryResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.row_count, 1);
        assert_eq!(deserialized.columns.len(), 2);
        assert_eq!(deserialized.execution_time_ms, 42);
    }

    #[test]
    fn test_execute_response_serialization() {
        let response = ExecuteResponse {
            statement_type: "INSERT".to_string(),
            affected_rows: 5,
            execution_time_ms: 23,
            message: Some("Successfully inserted 5 rows".to_string()),
        };

        let json = serde_json::to_string(&response).unwrap();
        let deserialized: ExecuteResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.statement_type, "INSERT");
        assert_eq!(deserialized.affected_rows, 5);
        assert_eq!(deserialized.execution_time_ms, 23);
    }
}
