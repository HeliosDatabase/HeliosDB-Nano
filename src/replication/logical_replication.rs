//! Logical Replication Layer
//!
//! Provides filtering and transformation capabilities for WAL replication,
//! inspired by Oracle GoldenGate-style change data capture.
//!
//! # Features
//!
//! - **Table filtering**: Include/exclude specific tables
//! - **Row filtering**: Filter rows based on predicates
//! - **Column mapping**: Rename or transform columns
//! - **Type transformations**: Convert data types during replication
//! - **Aggregation**: Combine changes for efficiency
//!
//! # Architecture
//!
//! ```text
//! Physical WAL          Logical Replication Pipeline
//! ┌──────────┐    ┌─────────┐   ┌──────────┐   ┌─────────┐
//! │ WalEntry │───►│ Decoder │──►│ Filters  │──►│Transform│──► Standby
//! └──────────┘    └─────────┘   └──────────┘   └─────────┘
//!                     │              │              │
//!                     ▼              ▼              ▼
//!              Extract change   Apply filter   Apply column
//!              table/row data   predicates     mappings
//! ```
//!
//! # Example Configuration
//!
//! ```rust,ignore
//! let config = LogicalReplicationConfig {
//!     table_filters: vec![
//!         TableFilter::include("users"),
//!         TableFilter::include("orders"),
//!         TableFilter::exclude("audit_log"),
//!     ],
//!     column_mappings: vec![
//!         ColumnMapping::rename("users", "email", "user_email"),
//!         ColumnMapping::transform("orders", "amount", |v| v * 100), // cents
//!     ],
//!     row_filters: vec![
//!         RowFilter::new("users", "status != 'deleted'"),
//!     ],
//! };
//! ```

use super::transport::{LogicalEntryPayload, LogicalOperation, LogicalValue};
use super::wal_replicator::{Lsn, WalEntry, WalEntryType};
use super::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// =============================================================================
// FILTER TYPES
// =============================================================================

/// Table filter rule
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableFilter {
    /// Table name pattern (supports wildcards)
    pub pattern: String,
    /// Include or exclude
    pub action: FilterAction,
}

impl TableFilter {
    /// Create an include filter
    pub fn include(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            action: FilterAction::Include,
        }
    }

    /// Create an exclude filter
    pub fn exclude(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            action: FilterAction::Exclude,
        }
    }

    /// Check if table matches this filter
    pub fn matches(&self, table: &str) -> bool {
        if self.pattern == "*" {
            return true;
        }

        if self.pattern.contains('*') {
            // Simple wildcard matching
            let parts: Vec<&str> = self.pattern.split('*').collect();
            if parts.len() == 2 {
                let prefix = parts[0];
                let suffix = parts[1];
                return table.starts_with(prefix) && table.ends_with(suffix);
            }
        }

        self.pattern == table
    }
}

/// Filter action
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterAction {
    Include,
    Exclude,
}

/// Row filter predicate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowFilter {
    /// Table name
    pub table: String,
    /// Filter predicate (SQL-like expression)
    pub predicate: String,
}

impl RowFilter {
    /// Create a new row filter
    pub fn new(table: impl Into<String>, predicate: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            predicate: predicate.into(),
        }
    }

    /// Evaluate the filter against a row
    pub fn evaluate(&self, row: &ChangeRow) -> bool {
        // For now, use simple field comparison parsing
        // In production, this would be a proper expression evaluator
        self.evaluate_predicate(row)
    }

    fn evaluate_predicate(&self, row: &ChangeRow) -> bool {
        // Simple predicate parser: "field = 'value'" or "field != 'value'"
        let predicate = self.predicate.trim();

        // Handle != operator
        if let Some((field, value)) = predicate.split_once("!=") {
            let field = field.trim();
            let value = value.trim().trim_matches('\'');

            if let Some(row_value) = row.get_field(field) {
                return row_value != value;
            }
            return true; // Field doesn't exist, pass through
        }

        // Handle = operator
        if let Some((field, value)) = predicate.split_once('=') {
            let field = field.trim();
            let value = value.trim().trim_matches('\'');

            if let Some(row_value) = row.get_field(field) {
                return row_value == value;
            }
            return true;
        }

        // Handle > operator
        if let Some((field, value)) = predicate.split_once('>') {
            let field = field.trim();
            let value = value.trim();

            if let Some(row_value) = row.get_field(field) {
                if let (Ok(rv), Ok(v)) = (row_value.parse::<i64>(), value.parse::<i64>()) {
                    return rv > v;
                }
            }
            return true;
        }

        // Handle < operator
        if let Some((field, value)) = predicate.split_once('<') {
            let field = field.trim();
            let value = value.trim();

            if let Some(row_value) = row.get_field(field) {
                if let (Ok(rv), Ok(v)) = (row_value.parse::<i64>(), value.parse::<i64>()) {
                    return rv < v;
                }
            }
            return true;
        }

        true // Default: pass through
    }
}

// =============================================================================
// COLUMN MAPPING
// =============================================================================

/// Column mapping transformation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnMapping {
    /// Source table
    pub table: String,
    /// Source column
    pub source_column: String,
    /// Target column name (None = same as source)
    pub target_column: Option<String>,
    /// Transformation type
    pub transform: ColumnTransform,
}

impl ColumnMapping {
    /// Create a rename mapping
    pub fn rename(
        table: impl Into<String>,
        source: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        Self {
            table: table.into(),
            source_column: source.into(),
            target_column: Some(target.into()),
            transform: ColumnTransform::Rename,
        }
    }

    /// Create a drop mapping (exclude column)
    pub fn drop(table: impl Into<String>, column: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            source_column: column.into(),
            target_column: None,
            transform: ColumnTransform::Drop,
        }
    }

    /// Create a type cast mapping
    pub fn cast(
        table: impl Into<String>,
        column: impl Into<String>,
        target_type: DataType,
    ) -> Self {
        Self {
            table: table.into(),
            source_column: column.into(),
            target_column: None,
            transform: ColumnTransform::Cast(target_type),
        }
    }

    /// Create a multiply transform (for currency conversion, etc.)
    pub fn multiply(table: impl Into<String>, column: impl Into<String>, factor: f64) -> Self {
        Self {
            table: table.into(),
            source_column: column.into(),
            target_column: None,
            transform: ColumnTransform::Multiply(factor),
        }
    }

    /// Create a mask transform (for PII)
    pub fn mask(table: impl Into<String>, column: impl Into<String>, mask_char: char) -> Self {
        Self {
            table: table.into(),
            source_column: column.into(),
            target_column: None,
            transform: ColumnTransform::Mask(mask_char),
        }
    }
}

/// Column transformation type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ColumnTransform {
    /// Just rename the column
    Rename,
    /// Drop the column
    Drop,
    /// Cast to different type
    Cast(DataType),
    /// Multiply numeric value
    Multiply(f64),
    /// Mask string value (for PII)
    Mask(char),
    /// Hash the value
    Hash,
    /// Custom SQL expression
    Expression(String),
}

/// Data types for casting
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataType {
    Integer,
    Float,
    String,
    Boolean,
    Timestamp,
    Json,
    Bytes,
}

// =============================================================================
// CHANGE EVENTS
// =============================================================================

/// Decoded change event from WAL
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEvent {
    /// LSN of the change
    pub lsn: Lsn,
    /// Transaction ID
    pub tx_id: Option<u64>,
    /// Table name
    pub table: String,
    /// Schema name
    pub schema: Option<String>,
    /// Operation type
    pub operation: ChangeOperation,
    /// Row data
    pub row: ChangeRow,
    /// Old row data (for updates/deletes)
    pub old_row: Option<ChangeRow>,
    /// Timestamp
    pub timestamp: u64,
}

/// Change operation type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeOperation {
    Insert,
    Update,
    Delete,
}

/// Row data with field access
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangeRow {
    /// Fields as key-value pairs
    pub fields: HashMap<String, FieldValue>,
}

impl ChangeRow {
    /// Create a new empty row
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a field
    pub fn set_field(&mut self, name: impl Into<String>, value: FieldValue) {
        self.fields.insert(name.into(), value);
    }

    /// Get a field value as string
    pub fn get_field(&self, name: &str) -> Option<String> {
        self.fields.get(name).map(|v| v.to_string())
    }

    /// Get a field value
    pub fn get(&self, name: &str) -> Option<&FieldValue> {
        self.fields.get(name)
    }
}

/// Field value with type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FieldValue {
    Null,
    Integer(i64),
    Float(f64),
    String(String),
    Boolean(bool),
    Bytes(Vec<u8>),
    Timestamp(u64),
}

impl std::fmt::Display for FieldValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldValue::Null => write!(f, "NULL"),
            FieldValue::Integer(v) => write!(f, "{}", v),
            FieldValue::Float(v) => write!(f, "{}", v),
            FieldValue::String(v) => write!(f, "{}", v),
            FieldValue::Boolean(v) => write!(f, "{}", v),
            FieldValue::Bytes(v) => write!(f, "<{} bytes>", v.len()),
            FieldValue::Timestamp(v) => write!(f, "{}", v),
        }
    }
}

// =============================================================================
// LOGICAL REPLICATION CONFIGURATION
// =============================================================================

/// Logical replication configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogicalReplicationConfig {
    /// Table filters (in order of precedence)
    pub table_filters: Vec<TableFilter>,
    /// Row filters per table
    pub row_filters: Vec<RowFilter>,
    /// Column mappings
    pub column_mappings: Vec<ColumnMapping>,
    /// Whether to replicate DDL changes
    pub replicate_ddl: bool,
    /// Whether to replicate truncate operations
    pub replicate_truncate: bool,
    /// Batch size for logical changes
    pub batch_size: usize,
}

impl LogicalReplicationConfig {
    /// Create a new config
    pub fn new() -> Self {
        Self {
            batch_size: 1000,
            ..Default::default()
        }
    }

    /// Add a table filter
    pub fn add_table_filter(mut self, filter: TableFilter) -> Self {
        self.table_filters.push(filter);
        self
    }

    /// Add a row filter
    pub fn add_row_filter(mut self, filter: RowFilter) -> Self {
        self.row_filters.push(filter);
        self
    }

    /// Add a column mapping
    pub fn add_column_mapping(mut self, mapping: ColumnMapping) -> Self {
        self.column_mappings.push(mapping);
        self
    }
}

// =============================================================================
// LOGICAL REPLICATION PIPELINE
// =============================================================================

/// Logical replication pipeline
///
/// Transforms physical WAL entries into filtered/transformed logical changes.
pub struct LogicalReplicationPipeline {
    /// Configuration
    config: LogicalReplicationConfig,
    /// Statistics
    stats: Arc<RwLock<PipelineStats>>,
    /// Current transaction state
    current_tx: Arc<RwLock<Option<TransactionState>>>,
}

/// Pipeline statistics
#[derive(Debug, Clone, Default)]
pub struct PipelineStats {
    /// Total entries processed
    pub entries_processed: u64,
    /// Entries passed filter
    pub entries_passed: u64,
    /// Entries filtered out
    pub entries_filtered: u64,
    /// Transformations applied
    pub transformations_applied: u64,
    /// Errors encountered
    pub errors: u64,
}

/// Transaction state for grouping changes
struct TransactionState {
    tx_id: u64,
    start_lsn: Lsn,
    changes: Vec<ChangeEvent>,
}

impl LogicalReplicationPipeline {
    /// Create a new pipeline
    pub fn new(config: LogicalReplicationConfig) -> Self {
        Self {
            config,
            stats: Arc::new(RwLock::new(PipelineStats::default())),
            current_tx: Arc::new(RwLock::new(None)),
        }
    }

    /// Process a WAL entry through the pipeline
    pub async fn process(&self, entry: &WalEntry) -> Result<Option<ChangeEvent>> {
        let mut stats = self.stats.write().await;
        stats.entries_processed += 1;

        // Decode WAL entry to change event
        let event = match self.decode_entry(entry) {
            Some(e) => e,
            None => return Ok(None), // Not a data change (DDL, checkpoint, etc.)
        };

        // Apply table filter
        if !self.should_replicate_table(&event.table) {
            stats.entries_filtered += 1;
            return Ok(None);
        }

        // Apply row filter
        if !self.should_replicate_row(&event.table, &event.row) {
            stats.entries_filtered += 1;
            return Ok(None);
        }

        // Apply transformations
        let transformed = self.apply_transformations(event)?;
        stats.entries_passed += 1;

        Ok(Some(transformed))
    }

    /// Process a batch of entries
    pub async fn process_batch(&self, entries: &[WalEntry]) -> Result<Vec<ChangeEvent>> {
        let mut results = Vec::with_capacity(entries.len());

        for entry in entries {
            if let Some(event) = self.process(entry).await? {
                results.push(event);
            }
        }

        Ok(results)
    }

    /// Decode a WAL entry into a change event
    fn decode_entry(&self, entry: &WalEntry) -> Option<ChangeEvent> {
        // Only process data changes
        let operation = match entry.entry_type {
            WalEntryType::Insert => ChangeOperation::Insert,
            WalEntryType::Update => ChangeOperation::Update,
            WalEntryType::Delete => ChangeOperation::Delete,
            _ => return None,
        };

        // Parse the WAL data to extract table and row info
        // This is a simplified implementation - real parsing would be more complex
        let decoded = self.parse_wal_data(&entry.data)?;

        Some(ChangeEvent {
            lsn: entry.lsn,
            tx_id: decoded.tx_id,
            table: decoded.table,
            schema: decoded.schema,
            operation,
            row: decoded.row,
            old_row: decoded.old_row,
            timestamp: chrono::Utc::now().timestamp_micros() as u64,
        })
    }

    /// Parse WAL data into structured form
    fn parse_wal_data(&self, data: &[u8]) -> Option<DecodedWalData> {
        // Try to deserialize as JSON (simplified format)
        // In production, this would parse the actual WAL format
        if let Ok(decoded) = serde_json::from_slice::<DecodedWalData>(data) {
            return Some(decoded);
        }

        // Fallback: treat as raw bytes with placeholder values
        Some(DecodedWalData {
            tx_id: None,
            table: "unknown".to_string(),
            schema: None,
            row: ChangeRow::new(),
            old_row: None,
        })
    }

    /// Check if table should be replicated
    fn should_replicate_table(&self, table: &str) -> bool {
        if self.config.table_filters.is_empty() {
            return true; // No filters = replicate all
        }

        let mut should_include = false;
        let mut explicitly_excluded = false;

        for filter in &self.config.table_filters {
            if filter.matches(table) {
                match filter.action {
                    FilterAction::Include => should_include = true,
                    FilterAction::Exclude => explicitly_excluded = true,
                }
            }
        }

        should_include && !explicitly_excluded
    }

    /// Check if row should be replicated
    fn should_replicate_row(&self, table: &str, row: &ChangeRow) -> bool {
        for filter in &self.config.row_filters {
            if filter.table == table || filter.table == "*" {
                if !filter.evaluate(row) {
                    return false;
                }
            }
        }

        true
    }

    /// Apply column transformations to a change event
    fn apply_transformations(&self, mut event: ChangeEvent) -> Result<ChangeEvent> {
        for mapping in &self.config.column_mappings {
            if mapping.table != event.table && mapping.table != "*" {
                continue;
            }

            // Apply transformation to the row
            event.row = self.transform_row(&event.row, mapping)?;

            // Also transform old_row for updates/deletes
            if let Some(old_row) = event.old_row.take() {
                event.old_row = Some(self.transform_row(&old_row, mapping)?);
            }
        }

        Ok(event)
    }

    /// Apply a single column transformation
    fn transform_row(&self, row: &ChangeRow, mapping: &ColumnMapping) -> Result<ChangeRow> {
        let mut new_row = row.clone();

        // Get the source value
        let value = match row.get(&mapping.source_column) {
            Some(v) => v.clone(),
            None => return Ok(new_row), // Column doesn't exist, skip
        };

        match &mapping.transform {
            ColumnTransform::Rename => {
                new_row.fields.remove(&mapping.source_column);
                if let Some(target) = &mapping.target_column {
                    new_row.fields.insert(target.clone(), value);
                }
            }
            ColumnTransform::Drop => {
                new_row.fields.remove(&mapping.source_column);
            }
            ColumnTransform::Cast(target_type) => {
                let converted = self.cast_value(&value, *target_type)?;
                let target = mapping.target_column.as_ref().unwrap_or(&mapping.source_column);
                new_row.fields.insert(target.clone(), converted);
            }
            ColumnTransform::Multiply(factor) => {
                let multiplied = self.multiply_value(&value, *factor)?;
                let target = mapping.target_column.as_ref().unwrap_or(&mapping.source_column);
                new_row.fields.insert(target.clone(), multiplied);
            }
            ColumnTransform::Mask(mask_char) => {
                let masked = self.mask_value(&value, *mask_char);
                let target = mapping.target_column.as_ref().unwrap_or(&mapping.source_column);
                new_row.fields.insert(target.clone(), masked);
            }
            ColumnTransform::Hash => {
                let hashed = self.hash_value(&value);
                let target = mapping.target_column.as_ref().unwrap_or(&mapping.source_column);
                new_row.fields.insert(target.clone(), hashed);
            }
            ColumnTransform::Expression(_expr) => {
                // TODO: Implement expression evaluation
            }
        }

        Ok(new_row)
    }

    /// Cast a value to a different type
    fn cast_value(&self, value: &FieldValue, target: DataType) -> Result<FieldValue> {
        Ok(match (value, target) {
            (FieldValue::Integer(i), DataType::Float) => FieldValue::Float(*i as f64),
            (FieldValue::Integer(i), DataType::String) => FieldValue::String(i.to_string()),
            (FieldValue::Float(f), DataType::Integer) => FieldValue::Integer(*f as i64),
            (FieldValue::Float(f), DataType::String) => FieldValue::String(f.to_string()),
            (FieldValue::String(s), DataType::Integer) => {
                FieldValue::Integer(s.parse().unwrap_or(0))
            }
            (FieldValue::String(s), DataType::Float) => {
                FieldValue::Float(s.parse().unwrap_or(0.0))
            }
            (FieldValue::Boolean(b), DataType::Integer) => FieldValue::Integer(if *b { 1 } else { 0 }),
            (FieldValue::Boolean(b), DataType::String) => FieldValue::String(b.to_string()),
            _ => value.clone(), // No conversion needed or not supported
        })
    }

    /// Multiply a numeric value
    fn multiply_value(&self, value: &FieldValue, factor: f64) -> Result<FieldValue> {
        Ok(match value {
            FieldValue::Integer(i) => FieldValue::Integer((*i as f64 * factor) as i64),
            FieldValue::Float(f) => FieldValue::Float(f * factor),
            _ => value.clone(),
        })
    }

    /// Mask a string value
    fn mask_value(&self, value: &FieldValue, mask_char: char) -> FieldValue {
        match value {
            FieldValue::String(s) => {
                let masked: String = s.chars().map(|_| mask_char).collect();
                FieldValue::String(masked)
            }
            _ => value.clone(),
        }
    }

    /// Hash a value
    fn hash_value(&self, value: &FieldValue) -> FieldValue {
        let bytes = match value {
            FieldValue::String(s) => s.as_bytes().to_vec(),
            FieldValue::Bytes(b) => b.clone(),
            FieldValue::Integer(i) => i.to_le_bytes().to_vec(),
            FieldValue::Float(f) => f.to_le_bytes().to_vec(),
            _ => vec![],
        };

        let hash = blake3::hash(&bytes);
        FieldValue::String(hash.to_hex().to_string())
    }

    /// Convert change event to logical entry payload
    pub fn to_logical_payload(&self, event: &ChangeEvent) -> LogicalEntryPayload {
        let operation = match event.operation {
            ChangeOperation::Insert => LogicalOperation::Insert,
            ChangeOperation::Update => LogicalOperation::Update,
            ChangeOperation::Delete => LogicalOperation::Delete,
        };

        // Convert row fields to LogicalValue map
        let new_values = Some(
            event
                .row
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), Self::field_to_logical_value(v)))
                .collect(),
        );

        // Convert old_row if present
        let old_values = event.old_row.as_ref().map(|r| {
            r.fields
                .iter()
                .map(|(k, v)| (k.clone(), Self::field_to_logical_value(v)))
                .collect()
        });

        LogicalEntryPayload {
            lsn: event.lsn,
            tx_id: event.tx_id,
            schema: event.schema.clone().unwrap_or_default(),
            table: event.table.clone(),
            operation,
            old_values,
            new_values,
            timestamp_us: event.timestamp,
        }
    }

    /// Convert FieldValue to LogicalValue
    fn field_to_logical_value(value: &FieldValue) -> LogicalValue {
        match value {
            FieldValue::Null => LogicalValue::Null,
            FieldValue::Integer(i) => LogicalValue::Int(*i),
            FieldValue::Float(f) => LogicalValue::Float(*f),
            FieldValue::String(s) => LogicalValue::Text(s.clone()),
            FieldValue::Boolean(b) => LogicalValue::Bool(*b),
            FieldValue::Bytes(b) => LogicalValue::Bytes(b.clone()),
            FieldValue::Timestamp(t) => LogicalValue::Timestamp(*t as i64),
        }
    }

    /// Get pipeline statistics
    pub async fn stats(&self) -> PipelineStats {
        self.stats.read().await.clone()
    }

    /// Reset statistics
    pub async fn reset_stats(&self) {
        *self.stats.write().await = PipelineStats::default();
    }
}

/// Decoded WAL data (internal)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DecodedWalData {
    tx_id: Option<u64>,
    table: String,
    schema: Option<String>,
    row: ChangeRow,
    old_row: Option<ChangeRow>,
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_filter_exact_match() {
        let filter = TableFilter::include("users");
        assert!(filter.matches("users"));
        assert!(!filter.matches("orders"));
    }

    #[test]
    fn test_table_filter_wildcard() {
        let filter = TableFilter::include("audit_*");
        assert!(filter.matches("audit_log"));
        assert!(filter.matches("audit_events"));
        assert!(!filter.matches("users"));
    }

    #[test]
    fn test_table_filter_all() {
        let filter = TableFilter::include("*");
        assert!(filter.matches("anything"));
        assert!(filter.matches("any_table"));
    }

    #[test]
    fn test_row_filter_equality() {
        let filter = RowFilter::new("users", "status = 'active'");

        let mut row = ChangeRow::new();
        row.set_field("status", FieldValue::String("active".to_string()));
        assert!(filter.evaluate(&row));

        row.set_field("status", FieldValue::String("deleted".to_string()));
        assert!(!filter.evaluate(&row));
    }

    #[test]
    fn test_row_filter_inequality() {
        let filter = RowFilter::new("users", "status != 'deleted'");

        let mut row = ChangeRow::new();
        row.set_field("status", FieldValue::String("active".to_string()));
        assert!(filter.evaluate(&row));

        row.set_field("status", FieldValue::String("deleted".to_string()));
        assert!(!filter.evaluate(&row));
    }

    #[test]
    fn test_row_filter_comparison() {
        let filter = RowFilter::new("orders", "amount > 100");

        let mut row = ChangeRow::new();
        row.set_field("amount", FieldValue::Integer(150));
        assert!(filter.evaluate(&row));

        row.set_field("amount", FieldValue::Integer(50));
        assert!(!filter.evaluate(&row));
    }

    #[test]
    fn test_column_mapping_rename() {
        let mapping = ColumnMapping::rename("users", "email", "user_email");
        assert_eq!(mapping.source_column, "email");
        assert_eq!(mapping.target_column, Some("user_email".to_string()));
    }

    #[test]
    fn test_column_mapping_drop() {
        let mapping = ColumnMapping::drop("users", "password_hash");
        assert!(matches!(mapping.transform, ColumnTransform::Drop));
    }

    #[test]
    fn test_field_value_display() {
        assert_eq!(FieldValue::Null.to_string(), "NULL");
        assert_eq!(FieldValue::Integer(42).to_string(), "42");
        assert_eq!(FieldValue::Float(3.14).to_string(), "3.14");
        assert_eq!(FieldValue::String("hello".to_string()).to_string(), "hello");
        assert_eq!(FieldValue::Boolean(true).to_string(), "true");
    }

    #[tokio::test]
    async fn test_pipeline_table_filtering() {
        let config = LogicalReplicationConfig::new()
            .add_table_filter(TableFilter::include("users"))
            .add_table_filter(TableFilter::include("orders"))
            .add_table_filter(TableFilter::exclude("audit_*"));

        let pipeline = LogicalReplicationPipeline::new(config);

        assert!(pipeline.should_replicate_table("users"));
        assert!(pipeline.should_replicate_table("orders"));
        assert!(!pipeline.should_replicate_table("audit_log"));
        assert!(!pipeline.should_replicate_table("unknown_table"));
    }

    #[tokio::test]
    async fn test_pipeline_row_filtering() {
        let config = LogicalReplicationConfig::new()
            .add_row_filter(RowFilter::new("users", "status != 'deleted'"));

        let pipeline = LogicalReplicationPipeline::new(config);

        let mut active_row = ChangeRow::new();
        active_row.set_field("status", FieldValue::String("active".to_string()));
        assert!(pipeline.should_replicate_row("users", &active_row));

        let mut deleted_row = ChangeRow::new();
        deleted_row.set_field("status", FieldValue::String("deleted".to_string()));
        assert!(!pipeline.should_replicate_row("users", &deleted_row));
    }

    #[test]
    fn test_transform_multiply() {
        let config = LogicalReplicationConfig::new();
        let pipeline = LogicalReplicationPipeline::new(config);

        let value = FieldValue::Integer(100);
        let result = pipeline.multiply_value(&value, 1.5).unwrap();

        assert!(matches!(result, FieldValue::Integer(150)));
    }

    #[test]
    fn test_transform_mask() {
        let config = LogicalReplicationConfig::new();
        let pipeline = LogicalReplicationPipeline::new(config);

        let value = FieldValue::String("secret123".to_string());
        let result = pipeline.mask_value(&value, '*');

        assert!(matches!(result, FieldValue::String(s) if s == "*********"));
    }

    #[test]
    fn test_transform_cast() {
        let config = LogicalReplicationConfig::new();
        let pipeline = LogicalReplicationPipeline::new(config);

        let value = FieldValue::Integer(42);
        let result = pipeline.cast_value(&value, DataType::String).unwrap();

        assert!(matches!(result, FieldValue::String(s) if s == "42"));
    }

    #[test]
    fn test_transform_hash() {
        let config = LogicalReplicationConfig::new();
        let pipeline = LogicalReplicationPipeline::new(config);

        let value = FieldValue::String("test".to_string());
        let result = pipeline.hash_value(&value);

        assert!(matches!(result, FieldValue::String(s) if !s.is_empty()));
    }

    #[tokio::test]
    async fn test_pipeline_stats() {
        let config = LogicalReplicationConfig::new();
        let pipeline = LogicalReplicationPipeline::new(config);

        let stats = pipeline.stats().await;
        assert_eq!(stats.entries_processed, 0);
        assert_eq!(stats.entries_passed, 0);
    }
}
