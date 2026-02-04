//! GIN (Generalized Inverted Index) implementation for JSONB
//!
//! GIN indexes are inverted indexes optimized for indexing composite values,
//! particularly JSONB data. They maintain a mapping from keys/paths to the
//! tuples (rows) containing those keys.
//!
//! This implementation provides PostgreSQL-compatible GIN indexing for JSONB columns.

use crate::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// GIN Index for JSONB columns
///
/// An inverted index that maps JSON keys/paths to row IDs containing them.
/// This enables fast lookups for key existence, containment, and path queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GinIndex {
    /// Index name
    pub name: String,

    /// Table name this index belongs to
    pub table_name: String,

    /// Column name being indexed
    pub column_name: String,

    /// Inverted index: key -> set of row IDs
    /// Keys are extracted from JSON objects/arrays recursively
    index: HashMap<String, HashSet<u64>>,

    /// Path index: full JSON path -> set of row IDs
    /// For more precise queries like jsonb_path_query
    path_index: HashMap<String, HashSet<u64>>,

    /// Value index: value hash -> set of row IDs
    /// For containment queries (@>, <@)
    value_index: HashMap<String, HashSet<u64>>,

    /// Statistics
    pub total_keys: usize,
    pub total_paths: usize,
    pub indexed_rows: usize,
}

impl GinIndex {
    /// Create a new GIN index
    pub fn new(name: String, table_name: String, column_name: String) -> Self {
        Self {
            name,
            table_name,
            column_name,
            index: HashMap::new(),
            path_index: HashMap::new(),
            value_index: HashMap::new(),
            total_keys: 0,
            total_paths: 0,
            indexed_rows: 0,
        }
    }

    /// Insert a JSON value into the index
    ///
    /// Extracts all keys, paths, and values from the JSON and adds them to the index
    /// mapped to the given row_id.
    pub fn insert(&mut self, row_id: u64, json_value: &serde_json::Value) -> Result<()> {
        // Extract keys recursively
        let mut keys = HashSet::new();
        let mut paths = Vec::new();
        let mut values = HashSet::new();

        self.extract_keys_paths_values(json_value, "", &mut keys, &mut paths, &mut values);

        // Add to key index
        for key in keys {
            self.index.entry(key)
                .or_insert_with(HashSet::new)
                .insert(row_id);
        }

        // Add to path index
        for path in paths {
            self.path_index.entry(path)
                .or_insert_with(HashSet::new)
                .insert(row_id);
        }

        // Add to value index
        for value_hash in values {
            self.value_index.entry(value_hash)
                .or_insert_with(HashSet::new)
                .insert(row_id);
        }

        self.indexed_rows += 1;
        self.total_keys = self.index.len();
        self.total_paths = self.path_index.len();

        Ok(())
    }

    /// Remove a row from the index
    pub fn delete(&mut self, row_id: u64, json_value: &serde_json::Value) -> Result<()> {
        // Extract keys, paths, and values
        let mut keys = HashSet::new();
        let mut paths = Vec::new();
        let mut values = HashSet::new();

        self.extract_keys_paths_values(json_value, "", &mut keys, &mut paths, &mut values);

        // Remove from key index
        for key in keys {
            if let Some(row_set) = self.index.get_mut(&key) {
                row_set.remove(&row_id);
                if row_set.is_empty() {
                    self.index.remove(&key);
                }
            }
        }

        // Remove from path index
        for path in paths {
            if let Some(row_set) = self.path_index.get_mut(&path) {
                row_set.remove(&row_id);
                if row_set.is_empty() {
                    self.path_index.remove(&path);
                }
            }
        }

        // Remove from value index
        for value_hash in values {
            if let Some(row_set) = self.value_index.get_mut(&value_hash) {
                row_set.remove(&row_id);
                if row_set.is_empty() {
                    self.value_index.remove(&value_hash);
                }
            }
        }

        self.indexed_rows = self.indexed_rows.saturating_sub(1);
        self.total_keys = self.index.len();
        self.total_paths = self.path_index.len();

        Ok(())
    }

    /// Search for rows containing a specific key
    pub fn search_key(&self, key: &str) -> Option<Vec<u64>> {
        self.index.get(key).map(|set| set.iter().copied().collect())
    }

    /// Search for rows containing any of the given keys
    pub fn search_any_key(&self, keys: &[String]) -> Option<Vec<u64>> {
        let mut result = HashSet::new();

        for key in keys {
            if let Some(row_set) = self.index.get(key) {
                result.extend(row_set);
            }
        }

        if result.is_empty() {
            None
        } else {
            Some(result.into_iter().collect())
        }
    }

    /// Search for rows containing all of the given keys
    pub fn search_all_keys(&self, keys: &[String]) -> Option<Vec<u64>> {
        if keys.is_empty() {
            return None;
        }

        // Start with rows containing the first key
        let mut result: HashSet<u64> = self.index.get(&keys[0])?.iter().copied().collect();

        // Intersect with rows containing each subsequent key
        for key in &keys[1..] {
            if let Some(row_set) = self.index.get(key) {
                result.retain(|row_id| row_set.contains(row_id));
            } else {
                // If any key is not found, no rows can contain all keys
                return None;
            }
        }

        if result.is_empty() {
            None
        } else {
            Some(result.into_iter().collect())
        }
    }

    /// Search for rows at a specific JSON path
    pub fn search_path(&self, path: &str) -> Option<Vec<u64>> {
        self.path_index.get(path).map(|set| set.iter().copied().collect())
    }

    /// Search for rows containing a specific value
    pub fn search_value(&self, value: &serde_json::Value) -> Option<Vec<u64>> {
        let value_hash = self.hash_value(value);
        self.value_index.get(&value_hash).map(|set| set.iter().copied().collect())
    }

    /// Extract keys, paths, and values from JSON recursively
    fn extract_keys_paths_values(
        &self,
        value: &serde_json::Value,
        current_path: &str,
        keys: &mut HashSet<String>,
        paths: &mut Vec<String>,
        values: &mut HashSet<String>,
    ) {
        match value {
            serde_json::Value::Object(obj) => {
                for (key, val) in obj {
                    // Add key to key set
                    keys.insert(key.clone());

                    // Build path
                    let path = if current_path.is_empty() {
                        key.clone()
                    } else {
                        format!("{}.{}", current_path, key)
                    };
                    paths.push(path.clone());

                    // Recursively process value
                    self.extract_keys_paths_values(val, &path, keys, paths, values);
                }
            }
            serde_json::Value::Array(arr) => {
                for (idx, val) in arr.iter().enumerate() {
                    // Build path with array index
                    let path = if current_path.is_empty() {
                        format!("[{}]", idx)
                    } else {
                        format!("{}[{}]", current_path, idx)
                    };
                    paths.push(path.clone());

                    // Recursively process value
                    self.extract_keys_paths_values(val, &path, keys, paths, values);
                }
            }
            _ => {
                // Leaf value - add to value index
                values.insert(self.hash_value(value));
            }
        }
    }

    /// Hash a JSON value for the value index
    fn hash_value(&self, value: &serde_json::Value) -> String {
        // Use the JSON string representation as the hash
        // In production, would use a proper hash function
        value.to_string()
    }

    /// Get index statistics
    pub fn statistics(&self) -> GinIndexStats {
        GinIndexStats {
            name: self.name.clone(),
            table_name: self.table_name.clone(),
            column_name: self.column_name.clone(),
            total_keys: self.total_keys,
            total_paths: self.total_paths,
            total_values: self.value_index.len(),
            indexed_rows: self.indexed_rows,
        }
    }
}

/// GIN Index statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GinIndexStats {
    pub name: String,
    pub table_name: String,
    pub column_name: String,
    pub total_keys: usize,
    pub total_paths: usize,
    pub total_values: usize,
    pub indexed_rows: usize,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_gin_index_creation() {
        let index = GinIndex::new(
            "test_idx".to_string(),
            "test_table".to_string(),
            "data".to_string(),
        );

        assert_eq!(index.name, "test_idx");
        assert_eq!(index.total_keys, 0);
        assert_eq!(index.indexed_rows, 0);
    }

    #[test]
    fn test_gin_index_insert() {
        let mut index = GinIndex::new(
            "test_idx".to_string(),
            "test_table".to_string(),
            "data".to_string(),
        );

        let json = json!({
            "name": "Alice",
            "age": 30,
            "tags": ["rust", "database"]
        });

        index.insert(1, &json).expect("Failed to insert");

        assert_eq!(index.indexed_rows, 1);
        assert!(index.total_keys >= 3); // name, age, tags
    }

    #[test]
    fn test_gin_index_key_search() {
        let mut index = GinIndex::new(
            "test_idx".to_string(),
            "test_table".to_string(),
            "data".to_string(),
        );

        let json1 = json!({"name": "Alice", "city": "NYC"});
        let json2 = json!({"name": "Bob", "country": "USA"});

        index.insert(1, &json1).unwrap();
        index.insert(2, &json2).unwrap();

        // Search for "name" key - should find both
        let results = index.search_key("name").unwrap();
        assert_eq!(results.len(), 2);

        // Search for "city" key - should find only row 1
        let results = index.search_key("city").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], 1);
    }

    #[test]
    fn test_gin_index_any_key_search() {
        let mut index = GinIndex::new(
            "test_idx".to_string(),
            "test_table".to_string(),
            "data".to_string(),
        );

        let json1 = json!({"name": "Alice", "city": "NYC"});
        let json2 = json!({"name": "Bob", "country": "USA"});

        index.insert(1, &json1).unwrap();
        index.insert(2, &json2).unwrap();

        // Search for any of ["city", "country"]
        let keys = vec!["city".to_string(), "country".to_string()];
        let results = index.search_any_key(&keys).unwrap();
        assert_eq!(results.len(), 2); // Both rows have at least one key
    }

    #[test]
    fn test_gin_index_all_keys_search() {
        let mut index = GinIndex::new(
            "test_idx".to_string(),
            "test_table".to_string(),
            "data".to_string(),
        );

        let json1 = json!({"name": "Alice", "city": "NYC", "age": 30});
        let json2 = json!({"name": "Bob", "country": "USA"});

        index.insert(1, &json1).unwrap();
        index.insert(2, &json2).unwrap();

        // Search for all of ["name", "city"]
        let keys = vec!["name".to_string(), "city".to_string()];
        let results = index.search_all_keys(&keys).unwrap();
        assert_eq!(results.len(), 1); // Only row 1 has both
        assert_eq!(results[0], 1);
    }

    #[test]
    fn test_gin_index_delete() {
        let mut index = GinIndex::new(
            "test_idx".to_string(),
            "test_table".to_string(),
            "data".to_string(),
        );

        let json = json!({"name": "Alice", "age": 30});

        index.insert(1, &json).unwrap();
        assert_eq!(index.indexed_rows, 1);

        index.delete(1, &json).unwrap();
        assert_eq!(index.indexed_rows, 0);
        assert_eq!(index.total_keys, 0);
    }

    #[test]
    fn test_gin_index_nested_json() {
        let mut index = GinIndex::new(
            "test_idx".to_string(),
            "test_table".to_string(),
            "data".to_string(),
        );

        let json = json!({
            "user": {
                "name": "Alice",
                "address": {
                    "city": "NYC"
                }
            }
        });

        index.insert(1, &json).unwrap();

        // Should index all nested keys
        assert!(index.search_key("user").is_some());
        assert!(index.search_key("name").is_some());
        assert!(index.search_key("address").is_some());
        assert!(index.search_key("city").is_some());
    }
}
