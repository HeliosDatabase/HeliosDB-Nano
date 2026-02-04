//! Implementation of dump/restore traits for EmbeddedDatabase

use crate::{EmbeddedDatabase, Result, Tuple, Schema};
use crate::storage::dump::{DatabaseInterface, DatabaseRestoreInterface, IndexMetadata};

impl DatabaseInterface for EmbeddedDatabase {
    fn list_tables(&self) -> Result<Vec<String>> {
        let catalog = self.storage.catalog();
        catalog.list_tables()
    }

    fn get_table_schema(&self, table: &str) -> Result<Schema> {
        let catalog = self.storage.catalog();
        catalog.get_table_schema(table)
    }

    fn scan_table(&self, table: &str) -> Result<Vec<Tuple>> {
        self.storage.scan_table(table)
    }

    fn get_table_indexes(&self, table: &str) -> Result<Vec<IndexMetadata>> {
        // Get vector indexes from the vector index manager
        let vector_indexes = self.storage.vector_indexes();
        let all_metadata = vector_indexes.list_all_metadata();

        // Filter indexes for this specific table and convert to IndexMetadata
        let indexes: Vec<IndexMetadata> = all_metadata
            .into_iter()
            .filter(|meta| meta.table_name == table)
            .map(|meta| {
                let index_type = match &meta.index_type {
                    crate::storage::VectorIndexType::Standard(_) => "hnsw",
                    crate::storage::VectorIndexType::Quantized(_) => "hnsw_pq",
                };
                IndexMetadata {
                    name: meta.name,
                    index_type: index_type.to_string(),
                    columns: vec![meta.column_name],
                    is_unique: false, // Vector indexes are not unique constraint indexes
                }
            })
            .collect();

        Ok(indexes)
    }
}

impl DatabaseRestoreInterface for EmbeddedDatabase {
    fn create_table(&mut self, name: &str, schema: Schema) -> Result<()> {
        let catalog = self.storage.catalog();
        catalog.create_table(name, schema)?;
        Ok(())
    }

    fn create_index(&mut self, table: &str, index: &IndexMetadata) -> Result<()> {
        // Build and execute CREATE INDEX SQL statement
        // Handle different index types (hnsw, btree, etc.)
        let using_clause = match index.index_type.as_str() {
            "hnsw" | "hnsw_pq" => "USING hnsw",
            "btree" => "",  // Default type
            "hash" => "USING hash",
            "gin" => "USING gin",
            _ => "", // Default to btree
        };

        let columns = index.columns.join(", ");
        let unique_clause = if index.is_unique { "UNIQUE " } else { "" };

        let sql = format!(
            "CREATE {}INDEX {} ON {} {} ({})",
            unique_clause,
            index.name,
            table,
            using_clause,
            columns
        );

        // Execute the CREATE INDEX statement
        self.execute(&sql)?;
        Ok(())
    }

    fn insert_row(&mut self, table: &str, row: Tuple) -> Result<()> {
        self.storage.insert_tuple(table, row)?;
        Ok(())
    }
}
