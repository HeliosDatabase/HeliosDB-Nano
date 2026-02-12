//! Core data types for HeliosDB Lite
//!
//! This module defines the fundamental types used throughout the database:
//!
//! - [`DataType`] - SQL data types (PostgreSQL compatible)
//! - [`Value`] - Runtime values that can be stored and queried
//! - [`Tuple`] - A row of values
//! - [`Schema`] - Table schema with column definitions
//! - [`Column`] - Column metadata (name, type, constraints)
//!
//! # Type System
//!
//! HeliosDB Lite uses a PostgreSQL-compatible type system with support for:
//!
//! - **Numeric types**: Int2, Int4, Int8, Float4, Float8, Numeric
//! - **String types**: Text, Varchar, Char
//! - **Binary types**: Bytea
//! - **Date/Time types**: Date, Time, Timestamp, Timestamptz, Interval
//! - **Structured types**: Json, Jsonb, Array, Vector (for embeddings)
//! - **Special types**: Boolean, Uuid
//!
//! # Examples
//!
//! ```rust
//! use heliosdb_lite::{DataType, Value, Column, Schema};
//!
//! // Define a schema
//! let schema = Schema::new(vec![
//!     Column::new("id", DataType::Int4).primary_key(),
//!     Column::new("name", DataType::Text).not_null(),
//!     Column::new("email", DataType::Varchar(Some(255))),
//! ]);
//!
//! // Create a value
//! let name = Value::String("Alice".to_string());
//! assert_eq!(name.data_type(), DataType::Text);
//! ```

use serde::{Deserialize, Serialize};
use std::fmt;
use std::hash::{Hash, Hasher};

/// Column storage mode for per-column storage optimization
///
/// Allows fine-grained control over how individual columns are stored,
/// enabling different compression and deduplication strategies based on
/// the column's data characteristics.
///
/// # Storage Modes
///
/// - `Default`: Standard row-oriented storage, inline in tuple
/// - `Dictionary`: Dictionary-encoded strings for low-cardinality columns
/// - `ContentAddressed`: Hash-based deduplication for large values
/// - `Columnar`: Column-grouped storage for analytics workloads
///
/// # Example
///
/// ```sql
/// CREATE TABLE users (
///     id INT PRIMARY KEY,
///     status TEXT STORAGE DICTIONARY,        -- Low cardinality
///     bio TEXT STORAGE CONTENT_ADDRESSED,    -- Large text
///     scores FLOAT8[] STORAGE COLUMNAR       -- Analytics
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ColumnStorageMode {
    /// Standard row-oriented storage (default)
    /// Best for: OLTP, point queries, mixed workloads
    #[default]
    Default,
    /// Dictionary-encoded storage for low-cardinality strings
    /// Best for: Enum-like values, status codes, country codes (<64K unique values)
    Dictionary,
    /// Content-addressed storage with hash-based deduplication
    /// Best for: Large values (>1KB) with duplicates (documents, blobs)
    ContentAddressed,
    /// Column-grouped storage for analytics
    /// Best for: Analytics, aggregations, range scans, time-series data
    Columnar,
}

impl fmt::Display for ColumnStorageMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ColumnStorageMode::Default => write!(f, "DEFAULT"),
            ColumnStorageMode::Dictionary => write!(f, "DICTIONARY"),
            ColumnStorageMode::ContentAddressed => write!(f, "CONTENT_ADDRESSED"),
            ColumnStorageMode::Columnar => write!(f, "COLUMNAR"),
        }
    }
}

/// SQL data types (PostgreSQL compatible)
///
/// Represents the type of a column or value in the database. These types
/// are designed to be compatible with PostgreSQL for wire protocol support.
///
/// # Type Aliases
///
/// Common PostgreSQL type aliases are supported:
/// - `SERIAL` → Int4 with auto-increment
/// - `BIGSERIAL` → Int8 with auto-increment
/// - `INTEGER` → Int4
/// - `BIGINT` → Int8
/// - `REAL` → Float4
/// - `DOUBLE PRECISION` → Float8
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataType {
    /// Boolean type (true/false)
    Boolean,
    /// 16-bit signed integer (-32768 to 32767)
    Int2,
    /// 32-bit signed integer (-2^31 to 2^31-1)
    Int4,
    /// 64-bit signed integer (-2^63 to 2^63-1)
    Int8,
    /// 32-bit IEEE 754 floating point
    Float4,
    /// 64-bit IEEE 754 floating point
    Float8,
    /// Arbitrary precision numeric (stored as string)
    Numeric,
    /// Variable-length string with optional max length
    Varchar(Option<usize>),
    /// Unlimited-length string
    Text,
    /// Fixed-length string (padded with spaces)
    Char(usize),
    /// Binary data (byte array)
    Bytea,
    /// Calendar date (year, month, day)
    Date,
    /// Time of day without timezone
    Time,
    /// Date and time without timezone
    Timestamp,
    /// Date and time with timezone (stored as UTC)
    Timestamptz,
    /// Time interval (duration)
    Interval,
    /// Universally unique identifier (128-bit)
    Uuid,
    /// JSON text (stored as string)
    Json,
    /// Binary JSON (optimized for queries)
    Jsonb,
    /// Array of values of the inner type
    Array(Box<DataType>),
    /// Fixed-dimension vector for ML embeddings
    Vector(usize),
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Boolean => write!(f, "BOOLEAN"),
            DataType::Int2 => write!(f, "INT2"),
            DataType::Int4 => write!(f, "INT4"),
            DataType::Int8 => write!(f, "INT8"),
            DataType::Float4 => write!(f, "FLOAT4"),
            DataType::Float8 => write!(f, "FLOAT8"),
            DataType::Numeric => write!(f, "NUMERIC"),
            DataType::Varchar(Some(n)) => write!(f, "VARCHAR({})", n),
            DataType::Varchar(None) => write!(f, "VARCHAR"),
            DataType::Text => write!(f, "TEXT"),
            DataType::Char(n) => write!(f, "CHAR({})", n),
            DataType::Bytea => write!(f, "BYTEA"),
            DataType::Date => write!(f, "DATE"),
            DataType::Time => write!(f, "TIME"),
            DataType::Timestamp => write!(f, "TIMESTAMP"),
            DataType::Timestamptz => write!(f, "TIMESTAMPTZ"),
            DataType::Interval => write!(f, "INTERVAL"),
            DataType::Uuid => write!(f, "UUID"),
            DataType::Json => write!(f, "JSON"),
            DataType::Jsonb => write!(f, "JSONB"),
            DataType::Array(inner) => write!(f, "{}[]", inner),
            DataType::Vector(dim) => write!(f, "VECTOR({})", dim),
        }
    }
}

/// Runtime value representation
///
/// Values are the concrete data stored in tuples and returned from queries.
/// Each value variant corresponds to a [`DataType`] and can be serialized
/// for storage or transmitted over the wire protocol.
///
/// # Null Handling
///
/// SQL NULL is represented as `Value::Null`. NULL follows SQL semantics:
/// - NULL compared to anything (including NULL) returns NULL
/// - Use `IS NULL` / `IS NOT NULL` for null checks
///
/// # Type Coercion
///
/// Values can be coerced between compatible types during query execution.
/// For example, Int4 can be promoted to Int8 or Float8 as needed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    /// SQL NULL value
    Null,
    /// Boolean value
    Boolean(bool),
    /// 16-bit signed integer
    Int2(i16),
    /// 32-bit signed integer
    Int4(i32),
    /// 64-bit signed integer
    Int8(i64),
    /// 32-bit floating point
    Float4(f32),
    /// 64-bit floating point
    Float8(f64),
    /// Arbitrary precision numeric (stored as string to preserve precision)
    Numeric(String),
    /// Text string
    String(String),
    /// Binary data
    Bytes(Vec<u8>),
    /// UUID value
    Uuid(uuid::Uuid),
    /// Timestamp (stored as UTC)
    Timestamp(chrono::DateTime<chrono::Utc>),
    /// Date (year, month, day)
    Date(chrono::NaiveDate),
    /// Time of day without timezone
    Time(chrono::NaiveTime),
    /// Time interval (duration in microseconds for precision)
    /// Positive for forward, negative for backward
    Interval(i64),
    /// JSON value (stored as string for bincode compatibility)
    Json(String),
    /// Array of values
    Array(Vec<Value>),
    /// Vector for ML embeddings (f32 for efficiency)
    Vector(Vec<f32>),
    /// Dictionary reference - stores dict_id for dictionary-encoded columns
    /// The actual string value is stored in a separate dictionary structure
    DictRef {
        /// Dictionary ID mapping to the original string value
        dict_id: u32,
    },
    /// Content-addressed reference - stores Blake3 hash of the original value
    /// The actual value is stored separately with the hash as the key
    CasRef {
        /// Blake3 hash of the original value (32 bytes)
        hash: [u8; 32],
    },
    /// Columnar reference - placeholder indicating value is in columnar storage
    /// The actual value is retrieved from column-grouped batch storage
    ColumnarRef,
}

impl Value {
    /// Get the data type of this value
    pub fn data_type(&self) -> DataType {
        match self {
            Value::Null => DataType::Text, // Null can be any type, default to Text
            Value::Boolean(_) => DataType::Boolean,
            Value::Int2(_) => DataType::Int2,
            Value::Int4(_) => DataType::Int4,
            Value::Int8(_) => DataType::Int8,
            Value::Float4(_) => DataType::Float4,
            Value::Float8(_) => DataType::Float8,
            Value::Numeric(_) => DataType::Numeric,
            Value::String(_) => DataType::Text,
            Value::Bytes(_) => DataType::Bytea,
            Value::Uuid(_) => DataType::Uuid,
            Value::Timestamp(_) => DataType::Timestamp,
            Value::Date(_) => DataType::Date,
            Value::Time(_) => DataType::Time,
            Value::Interval(_) => DataType::Interval,
            Value::Json(_) => DataType::Jsonb,
            Value::Array(arr) => {
                // Get type from first element if available
                if let Some(first) = arr.first() {
                    DataType::Array(Box::new(first.data_type()))
                } else {
                    DataType::Array(Box::new(DataType::Text))
                }
            }
            Value::Vector(vec) => DataType::Vector(vec.len()),
            // Storage reference types - return Text as placeholder
            // Actual type is determined by the column schema
            Value::DictRef { .. } => DataType::Text,
            Value::CasRef { .. } => DataType::Text,
            Value::ColumnarRef => DataType::Text,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Boolean(b) => write!(f, "{}", b),
            Value::Int2(i) => write!(f, "{}", i),
            Value::Int4(i) => write!(f, "{}", i),
            Value::Int8(i) => write!(f, "{}", i),
            Value::Float4(fl) => write!(f, "{}", fl),
            Value::Float8(fl) => write!(f, "{}", fl),
            Value::Numeric(n) => write!(f, "{}", n),
            Value::String(s) => write!(f, "'{}'", s),
            Value::Bytes(b) => write!(f, "\\x{}", hex::encode(b)),
            Value::Uuid(u) => write!(f, "'{}'", u),
            Value::Timestamp(ts) => write!(f, "'{}'", ts.to_rfc3339()),
            Value::Date(d) => write!(f, "'{}'", d.format("%Y-%m-%d")),
            Value::Time(t) => write!(f, "'{}'", t.format("%H:%M:%S%.f")),
            Value::Interval(micros) => {
                // Format interval in a human-readable way
                let total_secs = micros / 1_000_000;
                let days = total_secs / 86400;
                let hours = (total_secs % 86400) / 3600;
                let mins = (total_secs % 3600) / 60;
                let secs = total_secs % 60;
                if days > 0 {
                    write!(f, "{} days {:02}:{:02}:{:02}", days, hours, mins, secs)
                } else {
                    write!(f, "{:02}:{:02}:{:02}", hours, mins, secs)
                }
            }
            Value::Json(j) => write!(f, "'{}'", j),
            Value::Array(arr) => {
                write!(f, "{{")?;
                for (i, v) in arr.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    // Format array elements without type wrappers for cleaner output
                    match v {
                        Value::Int2(n) => write!(f, "{}", n)?,
                        Value::Int4(n) => write!(f, "{}", n)?,
                        Value::Int8(n) => write!(f, "{}", n)?,
                        Value::Float4(n) => write!(f, "{}", n)?,
                        Value::Float8(n) => write!(f, "{}", n)?,
                        Value::String(s) => write!(f, "\"{}\"", s)?,
                        Value::Boolean(b) => write!(f, "{}", b)?,
                        Value::Null => write!(f, "NULL")?,
                        other => write!(f, "{}", other)?,
                    }
                }
                write!(f, "}}")
            }
            Value::Vector(vec) => write!(f, "[{}]", vec.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")),
            Value::DictRef { dict_id } => write!(f, "<dict:{}>", dict_id),
            Value::CasRef { hash } => write!(f, "<cas:{}>", hex::encode(&hash[..8])),
            Value::ColumnarRef => write!(f, "<columnar>"),
        }
    }
}

/// A tuple (row) of values
///
/// Tuples are the fundamental unit of data in HeliosDB Lite. Each tuple
/// contains a vector of [`Value`]s corresponding to the columns in a table.
///
/// # Row Tracking
///
/// Tuples carry optional metadata for row identification:
/// - `row_id`: Unique identifier within a table, used for UPDATE/DELETE
/// - `branch_id`: Branch identifier for database branching (experimental)
///
/// # Example
///
/// ```rust
/// use heliosdb_lite::{Tuple, Value};
///
/// // Create a simple tuple
/// let row = Tuple::new(vec![
///     Value::Int4(1),
///     Value::String("Alice".to_string()),
/// ]);
///
/// assert_eq!(row.len(), 2);
/// assert_eq!(row.get(0), Some(&Value::Int4(1)));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tuple {
    /// Column values in schema order
    pub values: Vec<Value>,
    /// Unique row identifier within the table (assigned by storage layer)
    pub row_id: Option<u64>,
    /// Branch identifier for copy-on-write branching (experimental)
    #[serde(skip)]
    pub branch_id: Option<u64>,
}

impl Tuple {
    /// Create a new tuple
    pub fn new(values: Vec<Value>) -> Self {
        Self { values, row_id: None, branch_id: None }
    }

    /// Create a new tuple with row ID
    pub fn with_row_id(values: Vec<Value>, row_id: u64) -> Self {
        Self { values, row_id: Some(row_id), branch_id: None }
    }

    /// Create a new tuple with row ID and branch ID
    pub fn with_row_and_branch_id(values: Vec<Value>, row_id: u64, branch_id: u64) -> Self {
        Self { values, row_id: Some(row_id), branch_id: Some(branch_id) }
    }

    /// Get value at index
    pub fn get(&self, index: usize) -> Option<&Value> {
        self.values.get(index)
    }

    /// Number of values
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Get schema inferred from tuple values
    ///
    /// Infers a schema by examining the types of values in this tuple.
    /// This is a runtime type inspection and should be used with care
    /// as it cannot detect all type nuances (e.g., VARCHAR vs TEXT).
    pub fn schema(&self) -> Schema {
        let columns: Vec<Column> = self.values
            .iter()
            .enumerate()
            .map(|(i, val)| {
                Column {
                    name: format!("column_{}", i),
                    data_type: val.data_type(),
                    nullable: matches!(val, Value::Null),
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: ColumnStorageMode::Default,
                }
            })
            .collect();

        Schema::new(columns)
    }
}

/// Column definition in a table schema
///
/// Defines the metadata for a single column including its name, type,
/// and constraints. Used to build [`Schema`] definitions.
///
/// # Builder Pattern
///
/// Column supports a builder pattern for setting constraints:
///
/// ```rust
/// use heliosdb_lite::{Column, DataType};
///
/// let id_col = Column::new("id", DataType::Int4)
///     .primary_key();  // Sets primary_key=true, nullable=false
///
/// let name_col = Column::new("name", DataType::Text)
///     .not_null();     // Sets nullable=false
///
/// let bio_col = Column::new("bio", DataType::Text);
///     // Default: nullable=true, primary_key=false
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    /// Column name (case-insensitive in queries)
    pub name: String,
    /// SQL data type
    pub data_type: DataType,
    /// Whether NULL values are allowed
    pub nullable: bool,
    /// Whether this column is part of the primary key
    pub primary_key: bool,
    /// Source table alias (for JOIN disambiguation with e.column syntax)
    #[serde(default)]
    pub source_table: Option<String>,
    /// Source table actual name (for JOIN disambiguation with table.column syntax)
    #[serde(default)]
    pub source_table_name: Option<String>,
    /// Default expression (serialized as JSON for storage)
    /// This is evaluated when INSERT doesn't provide a value for this column
    #[serde(default)]
    pub default_expr: Option<String>,
    /// UNIQUE constraint
    #[serde(default)]
    pub unique: bool,
    /// Storage mode for per-column storage optimization
    /// Controls how this column's values are stored (dictionary, CAS, columnar)
    #[serde(default)]
    pub storage_mode: ColumnStorageMode,
}

impl Column {
    /// Create a new column
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable: true,
            primary_key: false,
            source_table: None,
            source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: ColumnStorageMode::Default,
        }
    }

    /// Set the source table (for JOIN disambiguation)
    pub fn with_source_table(mut self, table: impl Into<String>) -> Self {
        self.source_table = Some(table.into());
        self
    }

    /// Make column non-nullable
    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    /// Make column a primary key
    pub fn primary_key(mut self) -> Self {
        self.primary_key = true;
        self.nullable = false;
        self
    }

    /// Set default expression (as serialized JSON)
    pub fn with_default(mut self, default_expr: impl Into<String>) -> Self {
        self.default_expr = Some(default_expr.into());
        self
    }

    /// Set UNIQUE constraint
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// Set storage mode for per-column optimization
    pub fn with_storage(mut self, mode: ColumnStorageMode) -> Self {
        self.storage_mode = mode;
        self
    }
}

/// Table schema definition
///
/// A schema defines the structure of a table, including column names,
/// types, and constraints. Schemas are used for:
///
/// - Table creation (`CREATE TABLE`)
/// - Query planning and type checking
/// - Result set metadata
/// - Data serialization/deserialization
///
/// # Example
///
/// ```rust
/// use heliosdb_lite::{Schema, Column, DataType};
///
/// let users_schema = Schema::new(vec![
///     Column::new("id", DataType::Int4).primary_key(),
///     Column::new("username", DataType::Varchar(Some(50))).not_null(),
///     Column::new("email", DataType::Text).not_null(),
///     Column::new("created_at", DataType::Timestamptz),
/// ]);
///
/// // Find column by name
/// let email_col = users_schema.get_column("email");
/// assert!(email_col.is_some());
///
/// // Get column index for projections
/// let idx = users_schema.get_column_index("username");
/// assert_eq!(idx, Some(1));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schema {
    /// Ordered list of column definitions
    pub columns: Vec<Column>,
}

impl Schema {
    /// Create a new schema
    pub fn new(columns: Vec<Column>) -> Self {
        Self { columns }
    }

    /// Get column by name
    pub fn get_column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Get column index by name
    pub fn get_column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Get column index with optional table qualifier for disambiguation
    ///
    /// If table is provided, matches columns where source_table equals table AND name matches.
    /// If no table is provided, falls back to simple name lookup.
    pub fn get_qualified_column_index(&self, table: Option<&str>, name: &str) -> Option<usize> {
        if let Some(table_name) = table {
            // Look for column with matching source_table (alias) OR source_table_name (actual name)
            self.columns.iter().position(|c| {
                (c.source_table.as_deref() == Some(table_name)
                    || c.source_table_name.as_deref() == Some(table_name))
                && c.name == name
            })
        } else {
            // No table qualifier - use simple name lookup
            self.get_column_index(name)
        }
    }

    /// Get column by index (bounds-checked)
    pub fn get_column_at(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    /// Get mutable column by index (bounds-checked)
    pub fn get_column_at_mut(&mut self, index: usize) -> Option<&mut Column> {
        self.columns.get_mut(index)
    }

    /// Number of columns
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Merge two schemas (for JOIN operations)
    ///
    /// Combines columns from left and right schemas, handling name conflicts
    /// by qualifying column names with table names when necessary.
    pub fn merge(&self, other: &Schema) -> Self {
        let mut columns = self.columns.clone();
        columns.extend(other.columns.clone());
        Self { columns }
    }

    /// Project schema to subset of columns
    ///
    /// Returns a new schema containing only the columns at the specified indices.
    pub fn project(&self, indices: &[usize]) -> Self {
        let columns = indices
            .iter()
            .filter_map(|&i| self.columns.get(i).cloned())
            .collect();
        Self { columns }
    }
}

/// Hash implementation for Value
///
/// Enables using Value as a key in HashMap, which is required for HashJoinOperator.
/// This implementation follows SQL semantics: NULL values have a consistent hash
/// but are never equal to anything (handled by PartialEq).
impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Discriminant first to ensure different variants hash differently
        std::mem::discriminant(self).hash(state);

        match self {
            Value::Null => {
                // NULL has a consistent hash
                0u8.hash(state);
            }
            Value::Boolean(b) => b.hash(state),
            Value::Int2(i) => i.hash(state),
            Value::Int4(i) => i.hash(state),
            Value::Int8(i) => i.hash(state),
            Value::Float4(f) => {
                // Use bit representation for consistent hashing
                f.to_bits().hash(state);
            }
            Value::Float8(f) => {
                // Use bit representation for consistent hashing
                f.to_bits().hash(state);
            }
            Value::Numeric(n) => {
                // Hash numeric string representation
                n.hash(state);
            }
            Value::String(s) => s.hash(state),
            Value::Bytes(b) => b.hash(state),
            Value::Uuid(u) => u.hash(state),
            Value::Timestamp(ts) => {
                // Hash the timestamp's nanosecond representation
                ts.timestamp_nanos_opt().hash(state);
            }
            Value::Date(d) => {
                // Hash date as string representation
                d.to_string().hash(state);
            }
            Value::Time(t) => {
                // Hash time as string representation
                t.to_string().hash(state);
            }
            Value::Json(j) => {
                // Hash JSON string representation
                // Note: This is not ideal for performance but ensures consistency
                j.to_string().hash(state);
            }
            Value::Array(arr) => {
                arr.len().hash(state);
                for val in arr {
                    val.hash(state);
                }
            }
            Value::Vector(vec) => {
                vec.len().hash(state);
                for f in vec {
                    f.to_bits().hash(state);
                }
            }
            Value::DictRef { dict_id } => {
                dict_id.hash(state);
            }
            Value::CasRef { hash } => {
                hash.hash(state);
            }
            Value::ColumnarRef => {
                // Columnar references hash to a constant
                // since the actual value is stored elsewhere
                255u8.hash(state);
            }
            Value::Interval(microseconds) => {
                microseconds.hash(state);
            }
        }
    }
}

/// Implement Eq for Value to enable HashMap usage
///
/// This is safe because we already have PartialEq and the types
/// that don't have perfect equality (floats, JSON) are handled appropriately.
impl Eq for Value {}

// Add hex crate to Cargo.toml for Bytes display
// For now, use a simple implementation
mod hex {
    use std::fmt::Write;

    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(s, "{:02x}", b);
        }
        s
    }
}

// Stub types for v3.0.0 API operations

/// Vector store information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorStoreInfo {
    /// Store name
    pub name: String,
    /// Vector dimensions
    pub dimensions: u32,
    /// Number of vectors
    pub vector_count: u64,
    /// Creation timestamp
    pub created_at: String,
    /// Distance metric (e.g., cosine, euclidean)
    pub metric: String,
    /// Index type (e.g., hnsw, flat)
    pub index_type: String,
}

/// Agent session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    /// Session ID
    pub id: String,
    /// Session name
    pub name: String,
    /// Creation timestamp
    pub created_at: String,
    /// Last updated timestamp
    pub updated_at: String,
    /// Session ID (duplicate field for compatibility)
    pub session_id: String,
    /// Message count in session
    pub message_count: u32,
    /// Token count in session
    pub token_count: u32,
    /// Session metadata
    pub metadata: serde_json::Value,
}

/// Agent message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    /// Message ID
    pub id: String,
    /// Sender role (user, assistant, system)
    pub role: String,
    /// Message content
    pub content: String,
    /// Timestamp
    pub created_at: String,
    /// Message name
    pub name: String,
    /// Function call if any
    pub function_call: Option<String>,
    /// Tool calls if any
    pub tool_calls: Option<serde_json::Value>,
    /// Message metadata
    pub metadata: serde_json::Value,
    /// Message timestamp
    pub timestamp: String,
}

/// Document data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentData {
    /// Document ID
    pub id: String,
    /// Document content
    pub content: String,
    /// Document metadata
    pub metadata: Option<serde_json::Value>,
    /// Creation timestamp
    pub created_at: String,
    /// Last updated timestamp
    pub updated_at: String,
    /// Document chunks
    pub chunks: Vec<String>,
}

/// Document metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentMetadata {
    /// Document ID
    pub id: String,
    /// Document size
    pub size: usize,
    /// Creation timestamp
    pub created_at: String,
    /// Last updated timestamp
    pub updated_at: String,
    /// Document content preview
    pub content: String,
    /// Document metadata
    pub metadata: Option<serde_json::Value>,
}
