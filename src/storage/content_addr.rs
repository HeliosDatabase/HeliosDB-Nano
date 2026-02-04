//! Content-addressed storage module for deduplicating large values
//!
//! Provides hash-based deduplication for large text and binary values.
//! Values above a threshold size are stored separately with their Blake3
//! hash as the key, allowing automatic deduplication.
//!
//! # Key Format
//!
//! ```text
//! cas:{blake3_hash} -> original value bytes
//! ```
//!
//! # Example
//!
//! ```sql
//! CREATE TABLE documents (
//!     id INT PRIMARY KEY,
//!     content TEXT STORAGE CONTENT_ADDRESSED  -- Large text, deduplicated
//! );
//! ```

use blake3;
use rocksdb::DB;

use crate::{DataType, Error, Result, Value};

/// Minimum size (in bytes) for content-addressed storage
/// Values smaller than this are stored inline (no deduplication benefit)
pub const CAS_MIN_SIZE: usize = 1024;

/// Content-addressed storage manager
///
/// Provides methods to store and retrieve large values using their
/// content hash as the key.
pub struct ContentAddressedStore;

impl ContentAddressedStore {
    /// Build the RocksDB key for a content hash
    ///
    /// Format: `cas:{32-byte-hash}`
    fn cas_key(hash: &[u8; 32]) -> Vec<u8> {
        let mut key = Vec::with_capacity(36);
        key.extend_from_slice(b"cas:");
        key.extend_from_slice(hash);
        key
    }

    /// Store a value if it's large enough, returning a CasRef or the original value
    ///
    /// # Arguments
    /// * `db` - RocksDB instance for persistence
    /// * `value` - Value to potentially store in CAS
    ///
    /// # Returns
    /// * `Value::CasRef` if the value was stored in CAS
    /// * Original value if it's too small for CAS (< 1KB)
    pub fn maybe_store(db: &DB, value: &Value) -> Result<Value> {
        let bytes = match value {
            Value::String(s) if s.len() >= CAS_MIN_SIZE => s.as_bytes(),
            Value::Bytes(b) if b.len() >= CAS_MIN_SIZE => b.as_slice(),
            _ => return Ok(value.clone()),
        };

        // Compute Blake3 hash
        let hash: [u8; 32] = blake3::hash(bytes).into();
        let key = Self::cas_key(&hash);

        // Store if not already present (idempotent)
        let exists = db.get(&key)
            .map_err(|e| Error::storage(format!("CAS lookup failed: {}", e)))?
            .is_some();

        if !exists {
            db.put(&key, bytes)
                .map_err(|e| Error::storage(format!("CAS store failed: {}", e)))?;
        }

        Ok(Value::CasRef { hash })
    }

    /// Store a value unconditionally (for migration), returning the CasRef
    ///
    /// Unlike `maybe_store`, this always stores the value regardless of size.
    pub fn store(db: &DB, value: &Value) -> Result<Value> {
        let bytes = match value {
            Value::String(s) => s.as_bytes(),
            Value::Bytes(b) => b.as_slice(),
            Value::Null => return Ok(value.clone()),
            _ => return Err(Error::storage("CAS only supports String and Bytes types")),
        };

        // Compute Blake3 hash
        let hash: [u8; 32] = blake3::hash(bytes).into();
        let key = Self::cas_key(&hash);

        // Store if not already present (idempotent)
        let exists = db.get(&key)
            .map_err(|e| Error::storage(format!("CAS lookup failed: {}", e)))?
            .is_some();

        if !exists {
            db.put(&key, bytes)
                .map_err(|e| Error::storage(format!("CAS store failed: {}", e)))?;
        }

        Ok(Value::CasRef { hash })
    }

    /// Resolve a CasRef back to its original value
    ///
    /// # Arguments
    /// * `db` - RocksDB instance for retrieval
    /// * `hash` - Blake3 hash of the original value
    /// * `target_type` - Expected data type (Text or Bytea)
    ///
    /// # Returns
    /// The original value (String or Bytes based on target_type)
    pub fn resolve(db: &DB, hash: &[u8; 32], target_type: &DataType) -> Result<Value> {
        let key = Self::cas_key(hash);

        let bytes = db.get(&key)
            .map_err(|e| Error::storage(format!("CAS resolve failed: {}", e)))?
            .ok_or_else(|| Error::storage(format!(
                "CAS reference not found: {}",
                hex::encode(&hash[..8])
            )))?;

        match target_type {
            DataType::Text | DataType::Varchar(_) | DataType::Char(_) | DataType::Json | DataType::Jsonb => {
                let s = String::from_utf8(bytes.to_vec())
                    .map_err(|e| Error::storage(format!("CAS value not valid UTF-8: {}", e)))?;
                Ok(Value::String(s))
            }
            DataType::Bytea => Ok(Value::Bytes(bytes.to_vec())),
            _ => Err(Error::storage(format!(
                "Invalid CAS target type: {}. Expected Text or Bytea.",
                target_type
            ))),
        }
    }

    /// Check if a CAS reference exists in storage
    pub fn exists(db: &DB, hash: &[u8; 32]) -> Result<bool> {
        let key = Self::cas_key(hash);
        db.get(&key)
            .map(|opt| opt.is_some())
            .map_err(|e| Error::storage(format!("CAS exists check failed: {}", e)))
    }

    /// Get the size of a stored CAS value
    pub fn get_size(db: &DB, hash: &[u8; 32]) -> Result<Option<usize>> {
        let key = Self::cas_key(hash);
        db.get(&key)
            .map(|opt| opt.map(|v| v.len()))
            .map_err(|e| Error::storage(format!("CAS size check failed: {}", e)))
    }

    /// Delete a CAS entry (use with caution - may break references)
    ///
    /// This should only be used during garbage collection after verifying
    /// no references exist.
    pub fn delete(db: &DB, hash: &[u8; 32]) -> Result<()> {
        let key = Self::cas_key(hash);
        db.delete(&key)
            .map_err(|e| Error::storage(format!("CAS delete failed: {}", e)))
    }

    /// Compute the hash for a value without storing it
    pub fn compute_hash(value: &Value) -> Option<[u8; 32]> {
        let bytes = match value {
            Value::String(s) => s.as_bytes(),
            Value::Bytes(b) => b.as_slice(),
            _ => return None,
        };
        Some(blake3::hash(bytes).into())
    }
}

/// Hex encoding module for display purposes
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_db() -> (TempDir, DB) {
        let dir = TempDir::new().unwrap();
        let db = DB::open_default(dir.path()).unwrap();
        (dir, db)
    }

    #[test]
    fn test_cas_small_value_not_stored() {
        let (_dir, db) = test_db();

        // Small value should be returned as-is
        let small = Value::String("hello".to_string());
        let result = ContentAddressedStore::maybe_store(&db, &small).unwrap();

        match result {
            Value::String(s) => assert_eq!(s, "hello"),
            _ => panic!("Expected original String value"),
        }
    }

    #[test]
    fn test_cas_large_value_stored() {
        let (_dir, db) = test_db();

        // Large value should be stored and return CasRef
        let large = Value::String("x".repeat(2000));
        let result = ContentAddressedStore::maybe_store(&db, &large).unwrap();

        match result {
            Value::CasRef { hash } => {
                // Resolve should return the original value
                let resolved = ContentAddressedStore::resolve(&db, &hash, &DataType::Text).unwrap();
                match resolved {
                    Value::String(s) => assert_eq!(s, "x".repeat(2000)),
                    _ => panic!("Expected String value"),
                }
            }
            _ => panic!("Expected CasRef"),
        }
    }

    #[test]
    fn test_cas_deduplication() {
        let (_dir, db) = test_db();

        // Store same content twice
        let content = Value::String("y".repeat(2000));
        let ref1 = ContentAddressedStore::maybe_store(&db, &content).unwrap();
        let ref2 = ContentAddressedStore::maybe_store(&db, &content).unwrap();

        // Both should return the same hash
        match (ref1, ref2) {
            (Value::CasRef { hash: h1 }, Value::CasRef { hash: h2 }) => {
                assert_eq!(h1, h2);
            }
            _ => panic!("Expected CasRef values"),
        }
    }

    #[test]
    fn test_cas_bytes() {
        let (_dir, db) = test_db();

        // Large bytes should be stored
        let large = Value::Bytes(vec![0u8; 2000]);
        let result = ContentAddressedStore::maybe_store(&db, &large).unwrap();

        match result {
            Value::CasRef { hash } => {
                // Resolve as Bytea
                let resolved = ContentAddressedStore::resolve(&db, &hash, &DataType::Bytea).unwrap();
                match resolved {
                    Value::Bytes(b) => assert_eq!(b.len(), 2000),
                    _ => panic!("Expected Bytes value"),
                }
            }
            _ => panic!("Expected CasRef"),
        }
    }

    #[test]
    fn test_cas_compute_hash() {
        let value = Value::String("test content".to_string());
        let hash1 = ContentAddressedStore::compute_hash(&value);
        let hash2 = ContentAddressedStore::compute_hash(&value);

        assert!(hash1.is_some());
        assert_eq!(hash1, hash2); // Same content = same hash
    }
}
