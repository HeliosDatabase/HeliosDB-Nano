//! Data operation DTOs (Data Transfer Objects)
//!
//! Request and response models for data CRUD operations.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use crate::{Tuple, Value, Schema, Column};

/// Response for listing tables
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableListResponse {
    /// List of table names
    pub tables: Vec<TableInfoResponse>,

    /// Total count of tables
    pub total: usize,
}

/// Information about a single table
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableInfoResponse {
    /// Table name
    pub name: String,

    /// Number of columns
    pub column_count: usize,

    /// Estimated row count (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
}

/// Query parameters for data retrieval
#[derive(Debug, Clone, Deserialize)]
pub struct DataQueryParams {
    /// Filter expression (WHERE clause without the WHERE keyword)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,

    /// Columns to select (comma-separated, e.g., "id,name,email")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<String>,

    /// Page number (1-based)
    #[serde(default = "default_page")]
    pub page: u32,

    /// Page size (number of rows per page)
    #[serde(default = "default_limit")]
    pub limit: u32,

    /// Time-travel query: timestamp to query data as of
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of: Option<u64>,

    /// Order by clause (e.g., "id DESC", "name ASC")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_by: Option<String>,

    /// Include total row count in response (may add overhead for large tables)
    #[serde(default)]
    pub include_total: bool,
}

fn default_page() -> u32 {
    1
}

fn default_limit() -> u32 {
    100
}

/// Response for data query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataQueryResponse {
    /// Column schema information
    pub columns: Vec<ColumnInfo>,

    /// Data rows (each row is a map of column_name -> value)
    pub rows: Vec<HashMap<String, serde_json::Value>>,

    /// Pagination info
    pub pagination: PaginationInfo,

    /// Query metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<QueryMetadata>,
}

/// Column information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfo {
    /// Column name
    pub name: String,

    /// Data type
    pub data_type: String,

    /// Whether column is nullable
    pub nullable: bool,

    /// Whether column is part of primary key
    pub primary_key: bool,
}

/// Pagination information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginationInfo {
    /// Current page number (1-based)
    pub page: u32,

    /// Page size
    pub limit: u32,

    /// Total number of rows (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,

    /// Whether there are more pages
    pub has_more: bool,
}

/// Query execution metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryMetadata {
    /// Time-travel timestamp used (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of_timestamp: Option<u64>,

    /// Number of rows in current page
    pub row_count: usize,
}

/// Request to insert data
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InsertDataRequest {
    /// Rows to insert (each row is a map of column_name -> value)
    pub rows: Vec<HashMap<String, serde_json::Value>>,

    /// Whether to return the inserted row IDs
    #[serde(default)]
    pub return_ids: bool,
}

/// Response for data insertion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertDataResponse {
    /// Number of rows inserted
    pub inserted: u64,

    /// Inserted row IDs (if return_ids was true)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_ids: Option<Vec<u64>>,

    /// Message describing the result
    pub message: String,
}

/// Request to update data
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpdateDataRequest {
    /// Values to update (column_name -> new_value)
    pub values: HashMap<String, serde_json::Value>,

    /// Filter expression (WHERE clause without the WHERE keyword)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

/// Response for data update
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateDataResponse {
    /// Number of rows updated
    pub updated: u64,

    /// Message describing the result
    pub message: String,
}

/// Request to delete data
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeleteDataRequest {
    /// Filter expression (WHERE clause without the WHERE keyword)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

/// Response for data deletion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteDataResponse {
    /// Number of rows deleted
    pub deleted: u64,

    /// Message describing the result
    pub message: String,
}

// Helper conversion functions

impl From<&Column> for ColumnInfo {
    fn from(column: &Column) -> Self {
        ColumnInfo {
            name: column.name.clone(),
            data_type: format!("{:?}", column.data_type),
            nullable: column.nullable,
            primary_key: column.primary_key,
        }
    }
}

impl From<&Schema> for Vec<ColumnInfo> {
    fn from(schema: &Schema) -> Self {
        schema.columns.iter().map(ColumnInfo::from).collect()
    }
}

/// Convert a Tuple to a HashMap with column names
pub fn tuple_to_map(tuple: &Tuple, schema: &Schema) -> HashMap<String, serde_json::Value> {
    let mut map = HashMap::new();

    for (idx, value) in tuple.values.iter().enumerate() {
        if let Some(column) = schema.columns.get(idx) {
            map.insert(column.name.clone(), value_to_json(value));
        }
    }

    map
}

/// Convert a Value to serde_json::Value
pub fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Int2(i) => serde_json::Value::Number((*i).into()),
        Value::Int4(i) => serde_json::Value::Number((*i).into()),
        Value::Int8(i) => serde_json::Value::Number((*i).into()),
        Value::Float4(f) => {
            serde_json::Number::from_f64(*f as f64)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null)
        }
        Value::Float8(f) => {
            serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null)
        }
        Value::Numeric(n) => {
            // Try to parse as a JSON number, preserving precision
            n.parse::<serde_json::Number>()
                .map(serde_json::Value::Number)
                .unwrap_or_else(|_| serde_json::Value::String(n.clone()))
        }
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Bytes(b) => {
            use base64::Engine;
            serde_json::Value::String(base64::prelude::BASE64_STANDARD.encode(b))
        }
        Value::Uuid(u) => serde_json::Value::String(u.to_string()),
        Value::Timestamp(ts) => serde_json::Value::String(ts.to_rfc3339()),
        Value::Date(d) => serde_json::Value::String(d.format("%Y-%m-%d").to_string()),
        Value::Time(t) => serde_json::Value::String(t.format("%H:%M:%S%.f").to_string()),
        Value::Json(json_str) => {
            serde_json::from_str(json_str).unwrap_or(serde_json::Value::String(json_str.clone()))
        }
        Value::Array(arr) => {
            serde_json::Value::Array(
                arr.iter().map(value_to_json).collect()
            )
        }
        Value::Vector(vec) => {
            serde_json::Value::Array(
                vec.iter()
                    .filter_map(|f| serde_json::Number::from_f64(*f as f64))
                    .map(serde_json::Value::Number)
                    .collect()
            )
        }
        // Storage references (should be resolved before JSON conversion)
        Value::DictRef { dict_id } => serde_json::Value::String(format!("dict:{}", dict_id)),
        Value::CasRef { hash } => serde_json::Value::String(format!("cas:{}", hex::encode(hash))),
        Value::ColumnarRef => serde_json::Value::Null,
        Value::Interval(microseconds) => serde_json::Value::Number((*microseconds).into()),
    }
}

/// Batch inference response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchInferResponse {
    /// Inferred schemas
    pub schemas: Vec<serde_json::Value>,

    /// Detected relationships
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relationships: Option<Vec<serde_json::Value>>,
}

/// Schema optimization response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationResponse {
    /// Optimized DDL
    pub optimized_ddl: String,

    /// Changes made
    pub changes: Vec<String>,

    /// Estimated performance improvement percentage
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_improvement: Option<f64>,
}

/// Schema comparison response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaComparisonResponse {
    /// Differences found
    pub differences: Vec<String>,

    /// SQL migration script (if applicable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_sql: Option<String>,

    /// Compatibility score (0-1)
    pub compatibility_score: f64,
}

/// Natural language schema response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NaturalLanguageSchemaResponse {
    /// Generated DDL or schema
    pub schema: String,

    /// Explanation
    pub explanation: String,

    /// Sample data (if requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub samples: Option<Vec<serde_json::Value>>,

    /// Suggestions
    pub suggestions: Vec<String>,
}

/// Convert serde_json::Value to our Value type
pub fn json_to_value(json: &serde_json::Value, target_type: &crate::DataType) -> Result<Value, String> {
    match (json, target_type) {
        (serde_json::Value::Null, _) => Ok(Value::Null),
        (serde_json::Value::Bool(b), crate::DataType::Boolean) => Ok(Value::Boolean(*b)),
        (serde_json::Value::Number(n), crate::DataType::Int2) => {
            n.as_i64()
                .and_then(|i| i16::try_from(i).ok())
                .map(Value::Int2)
                .ok_or_else(|| format!("Invalid Int2 value: {}", n))
        }
        (serde_json::Value::Number(n), crate::DataType::Int4) => {
            n.as_i64()
                .and_then(|i| i32::try_from(i).ok())
                .map(Value::Int4)
                .ok_or_else(|| format!("Invalid Int4 value: {}", n))
        }
        (serde_json::Value::Number(n), crate::DataType::Int8) => {
            n.as_i64()
                .map(Value::Int8)
                .ok_or_else(|| format!("Invalid Int8 value: {}", n))
        }
        (serde_json::Value::Number(n), crate::DataType::Float4) => {
            n.as_f64()
                .map(|f| Value::Float4(f as f32))
                .ok_or_else(|| format!("Invalid Float4 value: {}", n))
        }
        (serde_json::Value::Number(n), crate::DataType::Float8) => {
            n.as_f64()
                .map(Value::Float8)
                .ok_or_else(|| format!("Invalid Float8 value: {}", n))
        }
        (serde_json::Value::String(s), crate::DataType::Text | crate::DataType::Varchar(_) | crate::DataType::Char(_)) => {
            Ok(Value::String(s.clone()))
        }
        (serde_json::Value::String(s), crate::DataType::Bytea) => {
            // Decode base64 or hex string to bytes
            use base64::Engine;
            if let Some(hex_str) = s.strip_prefix("\\x") {
                // PostgreSQL-style hex format
                hex::decode(hex_str)
                    .map(Value::Bytes)
                    .map_err(|e| format!("Invalid hex bytes: {}", e))
            } else {
                // Try base64 decode
                base64::prelude::BASE64_STANDARD.decode(s.as_bytes())
                    .map(Value::Bytes)
                    .map_err(|e| format!("Invalid base64 bytes: {}", e))
            }
        }
        (serde_json::Value::String(s), crate::DataType::Uuid) => {
            s.parse::<uuid::Uuid>()
                .map(Value::Uuid)
                .map_err(|e| format!("Invalid UUID: {}", e))
        }
        (serde_json::Value::String(s), crate::DataType::Timestamp | crate::DataType::Timestamptz) => {
            s.parse::<chrono::DateTime<chrono::Utc>>()
                .map(Value::Timestamp)
                .map_err(|e| format!("Invalid timestamp: {}", e))
        }
        (serde_json::Value::String(s), crate::DataType::Json | crate::DataType::Jsonb) => {
            Ok(Value::Json(s.clone()))
        }
        (serde_json::Value::Object(_) | serde_json::Value::Array(_), crate::DataType::Json | crate::DataType::Jsonb) => {
            Ok(Value::Json(json.to_string()))
        }
        (serde_json::Value::Array(arr), crate::DataType::Vector(expected_dim)) => {
            let values: Result<Vec<f32>, String> = arr.iter()
                .map(|v| v.as_f64()
                    .map(|f| f as f32)
                    .ok_or_else(|| format!("Invalid vector element: {}", v))
                )
                .collect();

            let values = values?;

            if values.len() != *expected_dim {
                return Err(format!(
                    "Vector dimension mismatch: expected {}, got {}",
                    expected_dim,
                    values.len()
                ));
            }

            Ok(Value::Vector(values))
        }
        _ => Err(format!(
            "Cannot convert {:?} to {:?}",
            json,
            target_type
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::DataType;

    #[test]
    fn test_value_to_json() {
        assert_eq!(value_to_json(&Value::Null), serde_json::Value::Null);
        assert_eq!(value_to_json(&Value::Boolean(true)), serde_json::Value::Bool(true));
        assert_eq!(value_to_json(&Value::Int4(42)), serde_json::Value::Number(42.into()));
        assert_eq!(value_to_json(&Value::String("test".to_string())), serde_json::Value::String("test".to_string()));
    }

    #[test]
    fn test_json_to_value() {
        let json = serde_json::Value::Number(42.into());
        let result = json_to_value(&json, &DataType::Int4);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), Value::Int4(42)));

        let json = serde_json::Value::String("test".to_string());
        let result = json_to_value(&json, &DataType::Text);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), Value::String(s) if s == "test"));
    }

    #[test]
    fn test_column_info_conversion() {
        let column = Column {
            name: "id".to_string(),
            data_type: DataType::Int4,
            nullable: false,
            primary_key: true,
            source_table: None,
            source_table_name: None,
        default_expr: None,
        unique: false,
        };

        let info = ColumnInfo::from(&column);
        assert_eq!(info.name, "id");
        assert!(!info.nullable);
        assert!(info.primary_key);
    }

    #[test]
    fn test_tuple_to_map() {
        let schema = Schema::new(vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            },
            Column {
                name: "name".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            },
        ]);

        let tuple = Tuple::new(vec![
            Value::Int4(1),
            Value::String("Alice".to_string()),
        ]);

        let map = tuple_to_map(&tuple, &schema);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("id"), Some(&serde_json::Value::Number(1.into())));
        assert_eq!(map.get("name"), Some(&serde_json::Value::String("Alice".to_string())));
    }
}
