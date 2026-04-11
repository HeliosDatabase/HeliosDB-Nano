//! PostgreSQL system catalog emulation
//!
//! This module provides minimal emulation of PostgreSQL system catalogs
//! (pg_catalog) and information_schema for client compatibility.
//! Many PostgreSQL clients query these system tables during connection
//! and for introspection.

use crate::{Result, Schema, Tuple, Value, Column, DataType, EmbeddedDatabase};
use std::sync::Arc;

/// PostgreSQL catalog emulator
pub struct PgCatalog {
    /// Reference to the database for real catalog queries
    database: Option<Arc<EmbeddedDatabase>>,
}

impl PgCatalog {
    /// Create a new catalog emulator (without database access - static responses only)
    pub fn new() -> Self {
        Self { database: None }
    }

    /// Create a new catalog emulator with database access for real table/column metadata
    pub fn with_database(database: Arc<EmbeddedDatabase>) -> Self {
        Self { database: Some(database) }
    }

    /// Handle catalog queries
    ///
    /// Returns Some((schema, rows)) if this is a catalog query,
    /// None if it should be handled by the normal query engine
    pub fn handle_query(&self, query: &str) -> Result<Option<(Schema, Vec<Tuple>)>> {
        let query_lower = query.trim().to_lowercase();

        // Handle SELECT version() - required by SQLAlchemy, psql, pgAdmin, DBeaver
        if query_lower.contains("version()") {
            return Ok(Some(self.query_version()?));
        }

        // Handle SELECT current_schema() - required by SQLAlchemy connection init
        if query_lower.contains("current_schema()") {
            return Ok(Some(Self::query_current_schema()?));
        }

        // Handle SELECT current_database() - required by SQLAlchemy / pgAdmin
        if query_lower.contains("current_database()") {
            return Ok(Some(Self::query_current_database()?));
        }

        // Handle SELECT current_user - required by various PG clients
        if query_lower.contains("current_user")
            && !query_lower.contains("current_user_id")
            && (query_lower.starts_with("select") || query_lower.starts_with("show"))
        {
            return Ok(Some(Self::query_current_user()?));
        }

        // Check for information_schema queries (table and column listing)
        let result = if query_lower.contains("information_schema") {
            if query_lower.contains("information_schema.columns") {
                Some(self.query_information_schema_columns(&query_lower)?)
            } else if query_lower.contains("information_schema.tables") {
                Some(self.query_information_schema_tables(&query_lower)?)
            } else {
                // Return empty for other information_schema queries
                Some((Schema::new(vec![]), vec![]))
            }
        } else if !query_lower.contains("pg_catalog") && !query_lower.contains("pg_type") &&
           !query_lower.contains("pg_class") && !query_lower.contains("pg_namespace") &&
           !query_lower.contains("pg_attribute") && !query_lower.contains("pg_database") {
            return Ok(None);
        } else if query_lower.contains("pg_type") {
            Some(self.query_pg_type()?)
        } else if query_lower.contains("pg_class") {
            Some(self.query_pg_class()?)
        } else if query_lower.contains("pg_namespace") {
            Some(self.query_pg_namespace()?)
        } else if query_lower.contains("pg_database") {
            Some(self.query_pg_database()?)
        } else if query_lower.contains("pg_settings") {
            Some(self.query_pg_settings()?)
        } else if query_lower.contains("pg_attribute") {
            Some(self.query_pg_attribute()?)
        } else {
            // Return empty result for unknown catalog queries
            Some((Schema::new(vec![]), vec![]))
        };

        // Apply column projection based on SELECT clause
        match result {
            Some((schema, rows)) => {
                let projected = Self::project_columns(&query_lower, schema, rows);
                Ok(Some(projected))
            }
            None => Ok(None),
        }
    }

    /// Query information_schema.tables - returns real table metadata from the catalog
    fn query_information_schema_tables(&self, query_lower: &str) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("table_catalog", DataType::Text),
            Column::new("table_schema", DataType::Text),
            Column::new("table_name", DataType::Text),
            Column::new("table_type", DataType::Text),
        ]);

        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };

        // Get real table list from storage catalog
        let catalog = db.storage.catalog();
        let table_names = catalog.list_tables()?;

        // Extract LIKE filter if present (e.g., "table_name LIKE 'tenant_xyz__%'")
        let like_filter = Self::extract_like_filter(query_lower, "table_name");

        let mut rows = Vec::new();
        for name in &table_names {
            // Apply LIKE filter if present
            if let Some(ref pattern) = like_filter {
                if !Self::sql_like_match(name, pattern) {
                    continue;
                }
            }

            rows.push(Tuple::new(vec![
                Value::String("heliosdb".to_string()),
                Value::String("public".to_string()),
                Value::String(name.clone()),
                Value::String("BASE TABLE".to_string()),
            ]));
        }

        Ok((schema, rows))
    }

    /// Query information_schema.columns - returns real column metadata from the catalog
    fn query_information_schema_columns(&self, query_lower: &str) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("table_name", DataType::Text),
            Column::new("column_name", DataType::Text),
            Column::new("data_type", DataType::Text),
            Column::new("is_nullable", DataType::Text),
            Column::new("ordinal_position", DataType::Int4),
            Column::new("is_pk", DataType::Boolean),
        ]);

        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };

        // Extract table_name filter (e.g., "WHERE table_name = 'my_table'")
        let table_filter = Self::extract_eq_filter(query_lower, "table_name");

        let catalog = db.storage.catalog();

        let tables_to_query: Vec<String> = if let Some(ref filter_name) = table_filter {
            // Query specific table
            if catalog.table_exists(filter_name)? {
                vec![filter_name.clone()]
            } else {
                vec![]
            }
        } else {
            // Query all tables
            catalog.list_tables()?
        };

        let mut rows = Vec::new();
        for table_name in &tables_to_query {
            if let Ok(table_schema) = catalog.get_table_schema(table_name) {
                for (i, col) in table_schema.columns.iter().enumerate() {
                    rows.push(Tuple::new(vec![
                        Value::String(table_name.clone()),
                        Value::String(col.name.clone()),
                        Value::String(col.data_type.to_string()),
                        Value::String(if col.nullable { "YES".to_string() } else { "NO".to_string() }),
                        Value::Int4((i + 1) as i32),
                        Value::Boolean(col.primary_key),
                    ]));
                }
            }
        }

        Ok((schema, rows))
    }

    /// Extract a LIKE filter value from a query
    /// E.g., "table_name LIKE 'tenant_xyz__%'" -> Some("tenant_xyz__%")
    fn extract_like_filter(query: &str, column: &str) -> Option<String> {
        let pattern = format!("{} like '", column);
        if let Some(start) = query.find(&pattern) {
            let after = &query[start + pattern.len()..];
            if let Some(end) = after.find('\'') {
                return Some(after[..end].to_string());
            }
        }
        None
    }

    /// Extract an equality filter value from a query
    /// E.g., "table_name = 'my_table'" -> Some("my_table")
    fn extract_eq_filter(query: &str, column: &str) -> Option<String> {
        let pattern = format!("{} = '", column);
        if let Some(start) = query.find(&pattern) {
            let after = &query[start + pattern.len()..];
            if let Some(end) = after.find('\'') {
                return Some(after[..end].to_string());
            }
        }
        None
    }

    /// Apply column projection based on the SELECT clause
    /// Parses "SELECT col1, col2 FROM ..." and returns only the requested columns
    /// Returns all columns for "SELECT *" or if parsing fails
    fn project_columns(query_lower: &str, schema: Schema, rows: Vec<Tuple>) -> (Schema, Vec<Tuple>) {
        // Extract SELECT column list
        let select_cols = Self::parse_select_columns(query_lower);

        // If no specific columns requested (SELECT * or parse failure), return all
        if select_cols.is_empty() {
            return (schema, rows);
        }

        // Build index map: for each requested column, find its position in the full schema
        let col_indices: Vec<usize> = select_cols
            .iter()
            .filter_map(|requested| {
                schema.columns.iter().position(|c| c.name == *requested)
            })
            .collect();

        // If no columns matched, return all (safety fallback)
        if col_indices.is_empty() {
            return (schema, rows);
        }

        // Build projected schema
        let projected_schema = Schema::new(
            // Safety: col_indices validated against schema.columns.len() above
            #[allow(clippy::indexing_slicing)]
            col_indices.iter().map(|&i| schema.columns[i].clone()).collect()
        );

        // Build projected rows
        let projected_rows = rows
            .into_iter()
            .map(|row| {
                let values: Vec<Value> = col_indices
                    .iter()
                    .map(|&i| {
                        row.values.get(i).cloned().unwrap_or(Value::Null)
                    })
                    .collect();
                Tuple::new(values)
            })
            .collect();

        (projected_schema, projected_rows)
    }

    /// Parse SELECT column list from a query string
    /// Returns empty vec for "SELECT *" or if parsing fails
    fn parse_select_columns(query_lower: &str) -> Vec<String> {
        // Find "select" and "from" positions
        let select_pos = match query_lower.find("select") {
            Some(pos) => pos + 6, // skip "select"
            None => return vec![],
        };
        let from_pos = match query_lower.find(" from ") {
            Some(pos) => pos,
            None => return vec![],
        };

        if select_pos >= from_pos {
            return vec![];
        }

        let col_list = query_lower[select_pos..from_pos].trim();

        // SELECT * returns all columns
        if col_list == "*" {
            return vec![];
        }

        // Split by comma, trim, and collect column names
        col_list
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Simple SQL LIKE pattern matching (supports % and _ wildcards)
    fn sql_like_match(text: &str, pattern: &str) -> bool {
        let t_chars: Vec<char> = text.chars().collect();
        let p_chars: Vec<char> = pattern.chars().collect();

        Self::like_match_recursive(&t_chars, &p_chars, 0, 0)
    }

    #[allow(clippy::indexing_slicing)] // Safety: pi/ti bounds checked at function entry and before use
    fn like_match_recursive(text: &[char], pattern: &[char], ti: usize, pi: usize) -> bool {
        if pi == pattern.len() {
            return ti == text.len();
        }

        match pattern[pi] {
            '%' => {
                // % matches zero or more characters
                for i in ti..=text.len() {
                    if Self::like_match_recursive(text, pattern, i, pi + 1) {
                        return true;
                    }
                }
                false
            }
            '_' => {
                // _ matches exactly one character
                if ti < text.len() {
                    Self::like_match_recursive(text, pattern, ti + 1, pi + 1)
                } else {
                    false
                }
            }
            c => {
                if ti < text.len() && text[ti] == c {
                    Self::like_match_recursive(text, pattern, ti + 1, pi + 1)
                } else {
                    false
                }
            }
        }
    }

    /// Return PostgreSQL-compatible version string
    fn query_version(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("version", DataType::Text),
        ]);
        let row = Tuple::new(vec![Value::String(
            "PostgreSQL 16.0 (HeliosDB Nano 3.9.6)".to_string(),
        )]);
        Ok((schema, vec![row]))
    }

    /// Return current schema (always "public")
    fn query_current_schema() -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("current_schema", DataType::Text),
        ]);
        let row = Tuple::new(vec![Value::String("public".to_string())]);
        Ok((schema, vec![row]))
    }

    /// Return current database name
    fn query_current_database() -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("current_database", DataType::Text),
        ]);
        let row = Tuple::new(vec![Value::String("heliosdb".to_string())]);
        Ok((schema, vec![row]))
    }

    /// Return current user
    fn query_current_user() -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("current_user", DataType::Text),
        ]);
        let row = Tuple::new(vec![Value::String("heliosdb".to_string())]);
        Ok((schema, vec![row]))
    }

    /// Query pg_type (type information)
    fn query_pg_type(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("typname", DataType::Text),
            Column::new("typnamespace", DataType::Int4),
            Column::new("typlen", DataType::Int2),
            Column::new("typtype", DataType::Text),
        ]);

        let rows = vec![
            // Common types
            Tuple::new(vec![
                Value::Int4(16), Value::String("bool".to_string()), Value::Int4(11),
                Value::Int2(1), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(20), Value::String("int8".to_string()), Value::Int4(11),
                Value::Int2(8), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(21), Value::String("int2".to_string()), Value::Int4(11),
                Value::Int2(2), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(23), Value::String("int4".to_string()), Value::Int4(11),
                Value::Int2(4), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(25), Value::String("text".to_string()), Value::Int4(11),
                Value::Int2(-1), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(700), Value::String("float4".to_string()), Value::Int4(11),
                Value::Int2(4), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(701), Value::String("float8".to_string()), Value::Int4(11),
                Value::Int2(8), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(1043), Value::String("varchar".to_string()), Value::Int4(11),
                Value::Int2(-1), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(1114), Value::String("timestamp".to_string()), Value::Int4(11),
                Value::Int2(8), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(2950), Value::String("uuid".to_string()), Value::Int4(11),
                Value::Int2(16), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(114), Value::String("json".to_string()), Value::Int4(11),
                Value::Int2(-1), Value::String("b".to_string()),
            ]),
            Tuple::new(vec![
                Value::Int4(3802), Value::String("jsonb".to_string()), Value::Int4(11),
                Value::Int2(-1), Value::String("b".to_string()),
            ]),
        ];

        Ok((schema, rows))
    }

    /// Query pg_class (relation/table information) - returns real tables from catalog
    fn query_pg_class(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("relname", DataType::Text),
            Column::new("relnamespace", DataType::Int4),
            Column::new("relkind", DataType::Text),
            Column::new("relowner", DataType::Int4),
        ]);

        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };

        let catalog = db.storage.catalog();
        let table_names = catalog.list_tables()?;

        let mut rows = Vec::new();
        for (i, name) in table_names.iter().enumerate() {
            rows.push(Tuple::new(vec![
                Value::Int4((16384 + i) as i32), // Start OIDs at 16384 (user tables)
                Value::String(name.clone()),
                Value::Int4(2200), // public namespace
                Value::String("r".to_string()), // regular table
                Value::Int4(10), // owner
            ]));
        }

        Ok((schema, rows))
    }

    /// Query pg_namespace (schema information)
    fn query_pg_namespace(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("nspname", DataType::Text),
            Column::new("nspowner", DataType::Int4),
        ]);

        let rows = vec![
            Tuple::new(vec![
                Value::Int4(11),
                Value::String("pg_catalog".to_string()),
                Value::Int4(10),
            ]),
            Tuple::new(vec![
                Value::Int4(2200),
                Value::String("public".to_string()),
                Value::Int4(10),
            ]),
        ];

        Ok((schema, rows))
    }

    /// Query pg_database (database information)
    fn query_pg_database(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("datname", DataType::Text),
            Column::new("datdba", DataType::Int4),
            Column::new("encoding", DataType::Int4),
        ]);

        let rows = vec![
            Tuple::new(vec![
                Value::Int4(1),
                Value::String("heliosdb".to_string()),
                Value::Int4(10),
                Value::Int4(6), // UTF8
            ]),
        ];

        Ok((schema, rows))
    }

    /// Query pg_settings (configuration parameters)
    fn query_pg_settings(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("name", DataType::Text),
            Column::new("setting", DataType::Text),
            Column::new("unit", DataType::Text),
            Column::new("category", DataType::Text),
        ]);

        let rows = vec![
            Tuple::new(vec![
                Value::String("server_version".to_string()),
                Value::String("17.0".to_string()),
                Value::Null,
                Value::String("Preset Options".to_string()),
            ]),
            Tuple::new(vec![
                Value::String("server_encoding".to_string()),
                Value::String("UTF8".to_string()),
                Value::Null,
                Value::String("Preset Options".to_string()),
            ]),
            Tuple::new(vec![
                Value::String("client_encoding".to_string()),
                Value::String("UTF8".to_string()),
                Value::Null,
                Value::String("Client Connection Defaults".to_string()),
            ]),
            Tuple::new(vec![
                Value::String("max_connections".to_string()),
                Value::String("100".to_string()),
                Value::Null,
                Value::String("Connections and Authentication".to_string()),
            ]),
        ];

        Ok((schema, rows))
    }

    /// Query pg_attribute (column information) - returns real column data from catalog
    fn query_pg_attribute(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("attrelid", DataType::Int4),
            Column::new("attname", DataType::Text),
            Column::new("atttypid", DataType::Int4),
            Column::new("attnum", DataType::Int2),
            Column::new("attlen", DataType::Int2),
        ]);

        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };

        let storage_catalog = db.storage.catalog();
        let table_names = storage_catalog.list_tables()?;

        let mut rows = Vec::new();
        for (ti, table_name) in table_names.iter().enumerate() {
            let oid = (16384 + ti) as i32;
            if let Ok(table_schema) = storage_catalog.get_table_schema(table_name) {
                for (ci, col) in table_schema.columns.iter().enumerate() {
                    let type_oid = Self::datatype_to_oid(&col.data_type);
                    let type_len = Self::datatype_to_len(&col.data_type);
                    rows.push(Tuple::new(vec![
                        Value::Int4(oid),
                        Value::String(col.name.clone()),
                        Value::Int4(type_oid),
                        Value::Int2((ci + 1) as i16),
                        Value::Int2(type_len),
                    ]));
                }
            }
        }

        Ok((schema, rows))
    }

    /// Map DataType to PostgreSQL type OID
    fn datatype_to_oid(dt: &DataType) -> i32 {
        match dt {
            DataType::Boolean => 16,
            DataType::Int2 => 21,
            DataType::Int4 => 23,
            DataType::Int8 => 20,
            DataType::Float4 => 700,
            DataType::Float8 => 701,
            DataType::Numeric => 1700,
            DataType::Varchar(_) => 1043,
            DataType::Text => 25,
            DataType::Char(_) => 1042,
            DataType::Bytea => 17,
            DataType::Date => 1082,
            DataType::Time => 1083,
            DataType::Timestamp => 1114,
            DataType::Timestamptz => 1184,
            DataType::Interval => 1186,
            DataType::Uuid => 2950,
            DataType::Json => 114,
            DataType::Jsonb => 3802,
            DataType::Array(_) => 2277,
            DataType::Vector(_) => 25, // stored as text
        }
    }

    /// Map DataType to PostgreSQL type length
    fn datatype_to_len(dt: &DataType) -> i16 {
        match dt {
            DataType::Boolean => 1,
            DataType::Int2 => 2,
            DataType::Int4 => 4,
            DataType::Int8 => 8,
            DataType::Float4 => 4,
            DataType::Float8 => 8,
            DataType::Timestamp | DataType::Timestamptz => 8,
            DataType::Uuid => 16,
            _ => -1, // variable length
        }
    }
}

impl Default for PgCatalog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_pg_type_query() {
        let catalog = PgCatalog::new();
        let result = catalog.query_pg_type();
        assert!(result.is_ok());

        let (schema, rows) = result.unwrap();
        assert_eq!(schema.columns.len(), 5);
        assert!(rows.len() > 0);
    }

    #[test]
    fn test_pg_namespace_query() {
        let catalog = PgCatalog::new();
        let result = catalog.query_pg_namespace();
        assert!(result.is_ok());

        let (schema, rows) = result.unwrap();
        assert_eq!(schema.columns.len(), 3);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_handle_query_non_catalog() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT * FROM users");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_handle_query_catalog() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT * FROM pg_type");
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn test_handle_query_information_schema_tables() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'");
        assert!(result.is_ok());
        // Without database, returns empty but doesn't error
        // project_columns reduces to only the requested column (table_name)
        let (schema, rows) = result.unwrap().unwrap();
        assert_eq!(schema.columns.len(), 1);
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn test_handle_query_information_schema_columns() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT column_name, data_type FROM information_schema.columns WHERE table_name = 'test'");
        assert!(result.is_ok());
        // project_columns reduces to only the requested columns (column_name, data_type)
        let (schema, rows) = result.unwrap().unwrap();
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn test_like_match() {
        assert!(PgCatalog::sql_like_match("tenant_abc__users", "tenant_abc__%"));
        assert!(PgCatalog::sql_like_match("tenant_abc__orders", "tenant_abc__%"));
        assert!(!PgCatalog::sql_like_match("other_table", "tenant_abc__%"));
        assert!(PgCatalog::sql_like_match("hello", "hel%"));
        assert!(PgCatalog::sql_like_match("hello", "h_llo"));
        assert!(!PgCatalog::sql_like_match("hello", "h_lo"));
    }

    #[test]
    fn test_extract_like_filter() {
        let query = "select table_name from information_schema.tables where table_name like 'tenant_abc__%'";
        assert_eq!(PgCatalog::extract_like_filter(query, "table_name"), Some("tenant_abc__%".to_string()));

        let query = "select table_name from information_schema.tables where table_schema = 'public'";
        assert_eq!(PgCatalog::extract_like_filter(query, "table_name"), None);
    }

    #[test]
    fn test_extract_eq_filter() {
        let query = "select column_name from information_schema.columns c where table_name = 'my_table'";
        assert_eq!(PgCatalog::extract_eq_filter(query, "table_name"), Some("my_table".to_string()));
    }
}
