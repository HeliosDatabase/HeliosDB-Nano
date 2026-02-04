//! PostgreSQL system catalog emulation
//!
//! This module provides minimal emulation of PostgreSQL system catalogs
//! (pg_catalog) for client compatibility. Many PostgreSQL clients query
//! these system tables during connection and for introspection.

use crate::{Result, Schema, Tuple, Value, Column, DataType};

/// PostgreSQL catalog emulator
pub struct PgCatalog {
    // Currently stateless, but could cache catalog data
}

impl PgCatalog {
    /// Create a new catalog emulator
    pub fn new() -> Self {
        Self {}
    }

    /// Handle catalog queries
    ///
    /// Returns Some((schema, rows)) if this is a catalog query,
    /// None if it should be handled by the normal query engine
    pub fn handle_query(&self, query: &str) -> Result<Option<(Schema, Vec<Tuple>)>> {
        let query_lower = query.trim().to_lowercase();

        // Check if this queries pg_catalog
        if !query_lower.contains("pg_catalog") && !query_lower.contains("pg_type") &&
           !query_lower.contains("pg_class") && !query_lower.contains("pg_namespace") &&
           !query_lower.contains("pg_attribute") && !query_lower.contains("pg_database") {
            return Ok(None);
        }

        // Common introspection queries
        if query_lower.contains("pg_type") {
            return Ok(Some(self.query_pg_type()?));
        } else if query_lower.contains("pg_class") {
            return Ok(Some(self.query_pg_class()?));
        } else if query_lower.contains("pg_namespace") {
            return Ok(Some(self.query_pg_namespace()?));
        } else if query_lower.contains("pg_database") {
            return Ok(Some(self.query_pg_database()?));
        } else if query_lower.contains("pg_settings") {
            return Ok(Some(self.query_pg_settings()?));
        } else if query_lower.contains("pg_attribute") {
            return Ok(Some(self.query_pg_attribute()?));
        }

        // Return empty result for unknown catalog queries
        Ok(Some((Schema::new(vec![]), vec![])))
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

    /// Query pg_class (relation/table information)
    fn query_pg_class(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("relname", DataType::Text),
            Column::new("relnamespace", DataType::Int4),
            Column::new("relkind", DataType::Text),
            Column::new("relowner", DataType::Int4),
        ]);

        // Return empty - real tables would be listed here
        Ok((schema, vec![]))
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

    /// Query pg_attribute (column information)
    fn query_pg_attribute(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("attrelid", DataType::Int4),
            Column::new("attname", DataType::Text),
            Column::new("atttypid", DataType::Int4),
            Column::new("attnum", DataType::Int2),
            Column::new("attlen", DataType::Int2),
        ]);

        // Return empty - real table columns would be listed here
        Ok((schema, vec![]))
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
}
