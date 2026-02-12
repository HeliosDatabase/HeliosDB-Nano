//! CLI Import/Export Tools
//!
//! Command-line tools for importing and exporting data in various formats:
//! - CSV, JSON, Parquet, Arrow
//! - SQL dumps
//! - Vector embeddings

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Export format
#[derive(Debug, Clone, PartialEq)]
pub enum ExportFormat {
    Csv,
    Json,
    JsonLines,
    Parquet,
    Arrow,
    Sql,
}

impl ExportFormat {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            "jsonl" | "ndjson" => Some(Self::JsonLines),
            "parquet" | "pq" => Some(Self::Parquet),
            "arrow" | "ipc" => Some(Self::Arrow),
            "sql" => Some(Self::Sql),
            _ => None,
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::JsonLines => "jsonl",
            Self::Parquet => "parquet",
            Self::Arrow => "arrow",
            Self::Sql => "sql",
        }
    }

    pub fn mime_type(&self) -> &'static str {
        match self {
            Self::Csv => "text/csv",
            Self::Json | Self::JsonLines => "application/json",
            Self::Parquet => "application/vnd.apache.parquet",
            Self::Arrow => "application/vnd.apache.arrow.file",
            Self::Sql => "application/sql",
        }
    }
}

/// Export options
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Output format
    pub format: ExportFormat,
    /// Output path (None for stdout)
    pub output: Option<PathBuf>,
    /// Table or query to export
    pub source: ExportSource,
    /// Include headers (CSV)
    pub headers: bool,
    /// Pretty print (JSON)
    pub pretty: bool,
    /// Compression (gzip, zstd, none)
    pub compression: Option<String>,
    /// Batch size for streaming export
    pub batch_size: usize,
    /// Branch to export from
    pub branch: String,
    /// As of timestamp (for time travel)
    pub as_of: Option<String>,
    /// Columns to include (None for all)
    pub columns: Option<Vec<String>>,
    /// Where clause filter
    pub filter: Option<String>,
    /// Order by clause
    pub order_by: Option<String>,
    /// Limit rows
    pub limit: Option<usize>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: ExportFormat::Csv,
            output: None,
            source: ExportSource::Table("".to_string()),
            headers: true,
            pretty: false,
            compression: None,
            batch_size: 10000,
            branch: "main".to_string(),
            as_of: None,
            columns: None,
            filter: None,
            order_by: None,
            limit: None,
        }
    }
}

/// Export source
#[derive(Debug, Clone)]
pub enum ExportSource {
    /// Export entire table
    Table(String),
    /// Export query results
    Query(String),
    /// Export vector store
    VectorStore(String),
    /// Export branch diff
    BranchDiff { from: String, to: String },
    /// Export schema only
    Schema,
}

/// Import options
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Input format (auto-detect if None)
    pub format: Option<ExportFormat>,
    /// Input path
    pub input: PathBuf,
    /// Target table
    pub table: String,
    /// Create table if not exists
    pub create_table: bool,
    /// Drop existing table
    pub drop_existing: bool,
    /// Truncate before import
    pub truncate: bool,
    /// Column mapping (source -> target)
    pub column_mapping: Option<HashMap<String, String>>,
    /// Columns to skip
    pub skip_columns: Option<Vec<String>>,
    /// Batch size for imports
    pub batch_size: usize,
    /// Branch to import into
    pub branch: String,
    /// Continue on error
    pub continue_on_error: bool,
    /// Generate embeddings for text columns
    pub generate_embeddings: Option<Vec<String>>,
    /// Validate data before import
    pub validate: bool,
    /// Dry run (don't actually import)
    pub dry_run: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            format: None,
            input: PathBuf::new(),
            table: "".to_string(),
            create_table: true,
            drop_existing: false,
            truncate: false,
            column_mapping: None,
            skip_columns: None,
            batch_size: 1000,
            branch: "main".to_string(),
            continue_on_error: false,
            generate_embeddings: None,
            validate: true,
            dry_run: false,
        }
    }
}

/// Import/export result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportExportResult {
    pub success: bool,
    pub rows_processed: usize,
    pub rows_failed: usize,
    pub duration_ms: u64,
    pub file_size_bytes: Option<u64>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Data exporter
pub struct Exporter {
    options: ExportOptions,
}

impl Exporter {
    pub fn new(options: ExportOptions) -> Self {
        Self { options }
    }

    /// Build SQL query for export
    pub fn build_query(&self) -> String {
        match &self.options.source {
            ExportSource::Table(table) => {
                let columns = self.options.columns.as_ref()
                    .map(|c| c.join(", "))
                    .unwrap_or_else(|| "*".to_string());

                let mut sql = format!("SELECT {} FROM {}", columns, table);

                if let Some(ref filter) = self.options.filter {
                    sql.push_str(&format!(" WHERE {}", filter));
                }

                if let Some(ref order) = self.options.order_by {
                    sql.push_str(&format!(" ORDER BY {}", order));
                }

                if let Some(limit) = self.options.limit {
                    sql.push_str(&format!(" LIMIT {}", limit));
                }

                sql
            }
            ExportSource::Query(query) => query.clone(),
            ExportSource::VectorStore(store) => {
                format!(
                    "SELECT id, vector, metadata FROM helios_vector_store WHERE store_name = '{}'",
                    store
                )
            }
            ExportSource::BranchDiff { from, to } => {
                format!(
                    "SELECT * FROM helios_branch_diff('{}', '{}')",
                    from, to
                )
            }
            ExportSource::Schema => {
                "SELECT * FROM information_schema.tables WHERE table_schema = 'public'".to_string()
            }
        }
    }

    /// Export to CSV string
    pub fn to_csv(&self, rows: &[HashMap<String, serde_json::Value>]) -> String {
        if rows.is_empty() {
            return String::new();
        }

        let mut output = String::new();
        let Some(first_row) = rows.first() else { return String::new() };
        let columns: Vec<&String> = first_row.keys().collect();

        // Headers
        if self.options.headers {
            output.push_str(&columns.iter().map(|c| escape_csv(c)).collect::<Vec<_>>().join(","));
            output.push('\n');
        }

        // Rows
        for row in rows {
            let values: Vec<String> = columns.iter()
                .map(|col| {
                    match row.get(*col) {
                        Some(serde_json::Value::String(s)) => escape_csv(s),
                        Some(serde_json::Value::Null) => String::new(),
                        Some(v) => escape_csv(&v.to_string()),
                        None => String::new(),
                    }
                })
                .collect();
            output.push_str(&values.join(","));
            output.push('\n');
        }

        output
    }

    /// Export to JSON string
    pub fn to_json(&self, rows: &[HashMap<String, serde_json::Value>]) -> String {
        if self.options.pretty {
            serde_json::to_string_pretty(rows).unwrap_or_default()
        } else {
            serde_json::to_string(rows).unwrap_or_default()
        }
    }

    /// Export to JSON Lines string
    pub fn to_jsonl(&self, rows: &[HashMap<String, serde_json::Value>]) -> String {
        rows.iter()
            .map(|row| serde_json::to_string(row).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Export to SQL INSERT statements
    pub fn to_sql(&self, table: &str, rows: &[HashMap<String, serde_json::Value>]) -> String {
        if rows.is_empty() {
            return String::new();
        }

        let Some(first_row) = rows.first() else { return String::new() };
        let columns: Vec<&String> = first_row.keys().collect();
        let col_names = columns.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ");

        let mut statements = Vec::new();

        for row in rows {
            let values: Vec<String> = columns.iter()
                .map(|col| {
                    match row.get(*col) {
                        Some(serde_json::Value::String(s)) => format!("'{}'", s.replace('\'', "''")),
                        Some(serde_json::Value::Null) => "NULL".to_string(),
                        Some(serde_json::Value::Bool(b)) => b.to_string(),
                        Some(serde_json::Value::Number(n)) => n.to_string(),
                        Some(v) => format!("'{}'", v.to_string().replace('\'', "''")),
                        None => "NULL".to_string(),
                    }
                })
                .collect();

            statements.push(format!(
                "INSERT INTO {} ({}) VALUES ({});",
                table, col_names, values.join(", ")
            ));
        }

        statements.join("\n")
    }
}

/// Data importer
pub struct Importer {
    options: ImportOptions,
}

impl Importer {
    pub fn new(options: ImportOptions) -> Self {
        Self { options }
    }

    /// Detect format from file extension
    pub fn detect_format(&self) -> ExportFormat {
        if let Some(ref format) = self.options.format {
            return format.clone();
        }

        let ext = self.options.input.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        ExportFormat::from_extension(ext).unwrap_or(ExportFormat::Csv)
    }

    /// Parse CSV data
    pub fn parse_csv(&self, data: &str) -> Result<Vec<HashMap<String, serde_json::Value>>, String> {
        let mut rows = Vec::new();
        let mut lines = data.lines();

        // Parse headers
        let headers: Vec<String> = match lines.next() {
            Some(line) => parse_csv_line(line),
            None => return Ok(rows),
        };

        // Parse rows
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }

            let values = parse_csv_line(line);
            let mut row = HashMap::new();

            for (i, header) in headers.iter().enumerate() {
                // Check if column should be skipped
                if let Some(ref skip) = self.options.skip_columns {
                    if skip.contains(header) {
                        continue;
                    }
                }

                // Apply column mapping
                let target_col = self.options.column_mapping.as_ref()
                    .and_then(|m| m.get(header))
                    .unwrap_or(header);

                let value = values.get(i)
                    .map(|v| infer_json_value(v))
                    .unwrap_or(serde_json::Value::Null);

                row.insert(target_col.clone(), value);
            }

            rows.push(row);
        }

        Ok(rows)
    }

    /// Parse JSON data
    pub fn parse_json(&self, data: &str) -> Result<Vec<HashMap<String, serde_json::Value>>, String> {
        serde_json::from_str(data)
            .map_err(|e| format!("JSON parse error: {}", e))
    }

    /// Parse JSON Lines data
    pub fn parse_jsonl(&self, data: &str) -> Result<Vec<HashMap<String, serde_json::Value>>, String> {
        let mut rows = Vec::new();

        for (i, line) in data.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            let row: HashMap<String, serde_json::Value> = serde_json::from_str(line)
                .map_err(|e| format!("JSON parse error on line {}: {}", i + 1, e))?;
            rows.push(row);
        }

        Ok(rows)
    }

    /// Infer table schema from data
    pub fn infer_schema(&self, rows: &[HashMap<String, serde_json::Value>]) -> Vec<ColumnSchema> {
        if rows.is_empty() {
            return Vec::new();
        }

        let mut columns: HashMap<String, ColumnSchema> = HashMap::new();

        for row in rows {
            for (key, value) in row {
                let data_type = infer_sql_type(value);

                columns.entry(key.clone())
                    .and_modify(|col| {
                        // Upgrade type if needed
                        col.data_type = merge_types(&col.data_type, &data_type);
                        if value.is_null() {
                            col.nullable = true;
                        }
                    })
                    .or_insert(ColumnSchema {
                        name: key.clone(),
                        data_type,
                        nullable: value.is_null(),
                        primary_key: key == "id",
                    });
            }
        }

        columns.into_values().collect()
    }

    /// Generate CREATE TABLE statement
    pub fn generate_create_table(&self, schema: &[ColumnSchema]) -> String {
        let columns: Vec<String> = schema.iter()
            .map(|col| {
                let mut def = format!("{} {}", col.name, col.data_type);
                if col.primary_key {
                    def.push_str(" PRIMARY KEY");
                }
                if !col.nullable && !col.primary_key {
                    def.push_str(" NOT NULL");
                }
                def
            })
            .collect();

        format!(
            "CREATE TABLE IF NOT EXISTS {} (\n    {}\n);",
            self.options.table,
            columns.join(",\n    ")
        )
    }

    /// Generate INSERT statements
    pub fn generate_inserts(&self, rows: &[HashMap<String, serde_json::Value>]) -> Vec<String> {
        if rows.is_empty() {
            return Vec::new();
        }

        let Some(first_row) = rows.first() else { return Vec::new() };
        let columns: Vec<&String> = first_row.keys().collect();
        let col_names = columns.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ");

        rows.iter()
            .map(|row| {
                let values: Vec<String> = columns.iter()
                    .map(|col| value_to_sql(row.get(*col)))
                    .collect();

                format!(
                    "INSERT INTO {} ({}) VALUES ({});",
                    self.options.table, col_names, values.join(", ")
                )
            })
            .collect()
    }

    /// Validate data against schema
    pub fn validate(&self, rows: &[HashMap<String, serde_json::Value>], _schema: &[ColumnSchema]) -> Vec<String> {
        let mut errors = Vec::new();

        for (i, _row) in rows.iter().enumerate() {
            // Basic validation - could be extended
            if i >= 1000000 {
                errors.push(format!("Row {} exceeds maximum row limit", i));
            }
        }

        errors
    }
}

/// Column schema for import
#[derive(Debug, Clone)]
pub struct ColumnSchema {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
}

// Helper functions

fn escape_csv(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    current.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            '"' => in_quotes = true,
            ',' if !in_quotes => {
                values.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(c),
        }
    }

    values.push(current.trim().to_string());
    values
}

fn infer_json_value(s: &str) -> serde_json::Value {
    // Try to parse as number
    if let Ok(n) = s.parse::<i64>() {
        return serde_json::Value::Number(n.into());
    }
    if let Ok(n) = s.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(n) {
            return serde_json::Value::Number(n);
        }
    }

    // Try to parse as boolean
    match s.to_lowercase().as_str() {
        "true" | "yes" | "1" => return serde_json::Value::Bool(true),
        "false" | "no" | "0" => return serde_json::Value::Bool(false),
        "" | "null" | "none" => return serde_json::Value::Null,
        _ => {}
    }

    // Try to parse as JSON object/array
    if (s.starts_with('{') && s.ends_with('}')) || (s.starts_with('[') && s.ends_with(']')) {
        if let Ok(v) = serde_json::from_str(s) {
            return v;
        }
    }

    // Default to string
    serde_json::Value::String(s.to_string())
}

fn infer_sql_type(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "TEXT".to_string(),
        serde_json::Value::Bool(_) => "BOOLEAN".to_string(),
        serde_json::Value::Number(n) => {
            if n.is_i64() {
                "BIGINT".to_string()
            } else {
                "DOUBLE PRECISION".to_string()
            }
        }
        serde_json::Value::String(s) => {
            // Check for specific patterns
            if s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4 {
                "UUID".to_string()
            } else if s.parse::<chrono::DateTime<chrono::Utc>>().is_ok() {
                "TIMESTAMP".to_string()
            } else if s.len() > 1000 {
                "TEXT".to_string()
            } else {
                "VARCHAR(255)".to_string()
            }
        }
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => "JSON".to_string(),
    }
}

fn merge_types(t1: &str, t2: &str) -> String {
    // Type hierarchy for merging
    let type_rank = |t: &str| -> u8 {
        match t {
            "BOOLEAN" => 1,
            "INTEGER" | "BIGINT" => 2,
            "DOUBLE PRECISION" | "REAL" => 3,
            "VARCHAR(255)" => 4,
            "TEXT" => 5,
            "JSON" => 6,
            _ => 4,
        }
    };

    if type_rank(t1) >= type_rank(t2) {
        t1.to_string()
    } else {
        t2.to_string()
    }
}

fn value_to_sql(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => "NULL".to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::String(s)) => format!("'{}'", s.replace('\'', "''")),
        Some(v @ serde_json::Value::Array(_)) | Some(v @ serde_json::Value::Object(_)) => {
            format!("'{}'", v.to_string().replace('\'', "''"))
        }
    }
}

use chrono;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_csv_parsing() {
        let importer = Importer::new(ImportOptions::default());
        let csv = "name,age,city\nAlice,30,NYC\nBob,25,LA";
        let rows = importer.parse_csv(csv).unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("name"), Some(&serde_json::Value::String("Alice".to_string())));
    }

    #[test]
    fn test_csv_export() {
        let exporter = Exporter::new(ExportOptions::default());
        let mut row = HashMap::new();
        row.insert("name".to_string(), serde_json::Value::String("Alice".to_string()));
        row.insert("age".to_string(), serde_json::Value::Number(30.into()));

        let csv = exporter.to_csv(&[row]);
        assert!(csv.contains("Alice"));
        assert!(csv.contains("30"));
    }

    #[test]
    fn test_schema_inference() {
        let importer = Importer::new(ImportOptions {
            table: "test".to_string(),
            ..Default::default()
        });

        let mut row = HashMap::new();
        row.insert("id".to_string(), serde_json::Value::Number(1.into()));
        row.insert("name".to_string(), serde_json::Value::String("Test".to_string()));

        let schema = importer.infer_schema(&[row]);
        assert_eq!(schema.len(), 2);
    }
}
