//! Dictionary encoding module for low-cardinality string columns
//!
//! Provides dictionary-based storage optimization where string values are
//! replaced with compact integer IDs. This significantly reduces storage
//! for columns with few unique values (e.g., status codes, country codes).
//!
//! # Key Format
//!
//! ```text
//! dict:{table}:{column} -> bincode-serialized ColumnDictionary
//! ```
//!
//! # Example
//!
//! ```sql
//! CREATE TABLE orders (
//!     id INT PRIMARY KEY,
//!     status TEXT STORAGE DICTIONARY  -- 'pending', 'shipped', 'delivered'
//! );
//! ```

use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use parking_lot::RwLock;
use rocksdb::DB;

use crate::{Error, Result};

/// Single column dictionary mapping strings to compact IDs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnDictionary {
    /// Forward mapping: string value -> dict_id
    value_to_id: HashMap<String, u32>,
    /// Reverse mapping: dict_id -> string value (index = dict_id)
    id_to_value: Vec<String>,
}

impl ColumnDictionary {
    /// Create a new empty dictionary
    pub fn new() -> Self {
        Self {
            value_to_id: HashMap::new(),
            id_to_value: Vec::new(),
        }
    }

    /// Encode a string value to its dict_id, inserting if new
    ///
    /// # Returns
    /// The dict_id for the given string value
    pub fn encode(&mut self, value: &str) -> u32 {
        if let Some(&id) = self.value_to_id.get(value) {
            return id;
        }

        // Insert new value
        let id = self.id_to_value.len() as u32;
        self.id_to_value.push(value.to_string());
        self.value_to_id.insert(value.to_string(), id);
        id
    }

    /// Decode a dict_id back to its string value
    ///
    /// # Returns
    /// The original string value, or None if the ID is invalid
    pub fn decode(&self, id: u32) -> Option<&str> {
        self.id_to_value.get(id as usize).map(|s| s.as_str())
    }

    /// Get the number of unique values in the dictionary
    pub fn len(&self) -> usize {
        self.id_to_value.len()
    }

    /// Check if dictionary is empty
    pub fn is_empty(&self) -> bool {
        self.id_to_value.is_empty()
    }

    /// Check if dictionary contains a value
    pub fn contains(&self, value: &str) -> bool {
        self.value_to_id.contains_key(value)
    }

    /// Get all values in the dictionary
    pub fn values(&self) -> &[String] {
        &self.id_to_value
    }
}

impl Default for ColumnDictionary {
    fn default() -> Self {
        Self::new()
    }
}

/// Cache entry for a column dictionary
struct DictCacheEntry {
    /// The dictionary itself
    dict: ColumnDictionary,
    /// Whether the dictionary has been modified since last persist
    dirty: bool,
}

/// Manages dictionaries for all tables and columns
///
/// Provides caching and persistence of dictionaries to RocksDB.
pub struct DictionaryManager {
    /// Cache: (table, column) -> dictionary entry
    cache: RwLock<HashMap<(String, String), DictCacheEntry>>,
}

impl DictionaryManager {
    /// Create a new dictionary manager
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Build the RocksDB key for a column dictionary
    fn dict_key(table: &str, column: &str) -> Vec<u8> {
        format!("dict:{}:{}", table, column).into_bytes()
    }

    /// Load a dictionary from RocksDB
    fn load_dict(db: &DB, table: &str, column: &str) -> Result<ColumnDictionary> {
        let key = Self::dict_key(table, column);
        match db.get(&key).map_err(|e| Error::storage(format!("Failed to load dictionary: {}", e)))? {
            Some(data) => {
                bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize dictionary: {}", e)))
            }
            None => Ok(ColumnDictionary::new()),
        }
    }

    /// Save a dictionary to RocksDB
    fn save_dict(db: &DB, table: &str, column: &str, dict: &ColumnDictionary) -> Result<()> {
        let key = Self::dict_key(table, column);
        let data = bincode::serialize(dict)
            .map_err(|e| Error::storage(format!("Failed to serialize dictionary: {}", e)))?;
        db.put(&key, &data)
            .map_err(|e| Error::storage(format!("Failed to save dictionary: {}", e)))?;
        Ok(())
    }

    /// Encode a string value for a table.column, loading/creating dictionary as needed
    ///
    /// # Arguments
    /// * `db` - RocksDB instance for persistence
    /// * `table` - Table name
    /// * `column` - Column name
    /// * `value` - String value to encode
    ///
    /// # Returns
    /// The dict_id for the encoded value
    pub fn encode(&self, db: &DB, table: &str, column: &str, value: &str) -> Result<u32> {
        let cache_key = (table.to_string(), column.to_string());

        // Try read-only lookup first (common case - value already exists)
        {
            let cache = self.cache.read();
            if let Some(entry) = cache.get(&cache_key) {
                if let Some(&id) = entry.dict.value_to_id.get(value) {
                    return Ok(id);
                }
            }
        }

        // Value not found or dict not loaded - need write lock
        let mut cache = self.cache.write();

        // Load dictionary if not in cache
        if !cache.contains_key(&cache_key) {
            let dict = Self::load_dict(db, table, column)?;
            cache.insert(cache_key.clone(), DictCacheEntry { dict, dirty: false });
        }

        // Get mutable reference to entry
        let entry = cache.get_mut(&cache_key)
            .ok_or_else(|| Error::storage("Dictionary cache entry missing"))?;

        // Encode the value (may insert new entry)
        let old_len = entry.dict.len();
        let id = entry.dict.encode(value);

        // Mark dirty if we added a new value
        if entry.dict.len() > old_len {
            entry.dirty = true;
        }

        Ok(id)
    }

    /// Decode a dict_id back to its string value
    ///
    /// # Arguments
    /// * `db` - RocksDB instance for loading dictionary
    /// * `table` - Table name
    /// * `column` - Column name
    /// * `dict_id` - Dictionary ID to decode
    ///
    /// # Returns
    /// The original string value
    pub fn decode(&self, db: &DB, table: &str, column: &str, dict_id: u32) -> Result<String> {
        let cache_key = (table.to_string(), column.to_string());

        // Try read-only lookup first
        {
            let cache = self.cache.read();
            if let Some(entry) = cache.get(&cache_key) {
                if let Some(value) = entry.dict.decode(dict_id) {
                    return Ok(value.to_string());
                }
            }
        }

        // Dictionary not loaded - need write lock to load it
        let mut cache = self.cache.write();

        // Load dictionary if not in cache
        if !cache.contains_key(&cache_key) {
            let dict = Self::load_dict(db, table, column)?;
            cache.insert(cache_key.clone(), DictCacheEntry { dict, dirty: false });
        }

        // Decode the value
        let entry = cache.get(&cache_key)
            .ok_or_else(|| Error::storage("Dictionary cache entry missing"))?;

        entry.dict.decode(dict_id)
            .map(|s| s.to_string())
            .ok_or_else(|| Error::storage(format!(
                "Invalid dict_id {} for {}.{}", dict_id, table, column
            )))
    }

    /// Flush all dirty dictionaries to RocksDB
    ///
    /// # Arguments
    /// * `db` - RocksDB instance for persistence
    pub fn flush(&self, db: &DB) -> Result<()> {
        let mut cache = self.cache.write();

        for ((table, column), entry) in cache.iter_mut() {
            if entry.dirty {
                Self::save_dict(db, table, column, &entry.dict)?;
                entry.dirty = false;
            }
        }

        Ok(())
    }

    /// Flush dictionary for a specific table.column
    pub fn flush_column(&self, db: &DB, table: &str, column: &str) -> Result<()> {
        let cache_key = (table.to_string(), column.to_string());
        let mut cache = self.cache.write();

        if let Some(entry) = cache.get_mut(&cache_key) {
            if entry.dirty {
                Self::save_dict(db, table, column, &entry.dict)?;
                entry.dirty = false;
            }
        }

        Ok(())
    }

    /// Get statistics for a dictionary
    pub fn stats(&self, db: &DB, table: &str, column: &str) -> Result<DictionaryStats> {
        let cache_key = (table.to_string(), column.to_string());

        // Try read from cache first
        {
            let cache = self.cache.read();
            if let Some(entry) = cache.get(&cache_key) {
                return Ok(DictionaryStats {
                    unique_values: entry.dict.len(),
                    dirty: entry.dirty,
                });
            }
        }

        // Load from disk
        let dict = Self::load_dict(db, table, column)?;
        Ok(DictionaryStats {
            unique_values: dict.len(),
            dirty: false,
        })
    }

    /// Clear the dictionary cache (for testing)
    pub fn clear_cache(&self) {
        self.cache.write().clear();
    }

    /// Drop a dictionary from cache and storage
    pub fn drop_dictionary(&self, db: &DB, table: &str, column: &str) -> Result<()> {
        let cache_key = (table.to_string(), column.to_string());

        // Remove from cache
        self.cache.write().remove(&cache_key);

        // Remove from storage
        let key = Self::dict_key(table, column);
        db.delete(&key)
            .map_err(|e| Error::storage(format!("Failed to delete dictionary: {}", e)))?;

        Ok(())
    }
}

impl Default for DictionaryManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics for a dictionary
#[derive(Debug, Clone)]
pub struct DictionaryStats {
    /// Number of unique values in the dictionary
    pub unique_values: usize,
    /// Whether the dictionary has unsaved changes
    pub dirty: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_column_dictionary_basic() {
        let mut dict = ColumnDictionary::new();

        // Encode values
        let id1 = dict.encode("pending");
        let id2 = dict.encode("shipped");
        let id3 = dict.encode("pending"); // Duplicate

        assert_eq!(id1, 0);
        assert_eq!(id2, 1);
        assert_eq!(id3, 0); // Same as id1

        // Decode values
        assert_eq!(dict.decode(0), Some("pending"));
        assert_eq!(dict.decode(1), Some("shipped"));
        assert_eq!(dict.decode(2), None);

        // Length
        assert_eq!(dict.len(), 2);
    }

    #[test]
    fn test_column_dictionary_serialization() {
        let mut dict = ColumnDictionary::new();
        dict.encode("foo");
        dict.encode("bar");
        dict.encode("baz");

        // Serialize
        let data = bincode::serialize(&dict).unwrap();

        // Deserialize
        let restored: ColumnDictionary = bincode::deserialize(&data).unwrap();

        assert_eq!(restored.len(), 3);
        assert_eq!(restored.decode(0), Some("foo"));
        assert_eq!(restored.decode(1), Some("bar"));
        assert_eq!(restored.decode(2), Some("baz"));
    }
}
