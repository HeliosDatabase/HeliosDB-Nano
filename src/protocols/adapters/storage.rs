//! Storage adapter layer for protocol integration
//!
//! This module provides a trait-based adapter that bridges HeliosDB Full's
//! LsmStorageEngine interface to HeliosDB Lite's RocksDB-based StorageEngine.

use crate::{Error, Result, StorageEngine};
use crate::storage::Transaction;
use std::sync::Arc;

/// Storage adapter trait
///
/// Provides a unified interface for storage operations that can be implemented
/// by different storage backends. This allows protocol handlers to work with
/// any storage engine that implements this trait.
pub trait StorageAdapter: Send + Sync {
    /// Get a value by key
    ///
    /// # Arguments
    /// * `key` - The key to retrieve
    ///
    /// # Returns
    /// * `Ok(Some(value))` if the key exists
    /// * `Ok(None)` if the key does not exist
    /// * `Err(error)` if an error occurs
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Put a key-value pair
    ///
    /// # Arguments
    /// * `key` - The key to store
    /// * `value` - The value to store
    ///
    /// # Returns
    /// * `Ok(())` on success
    /// * `Err(error)` if an error occurs
    fn put(&self, key: &[u8], value: &[u8]) -> Result<()>;

    /// Delete a key
    ///
    /// # Arguments
    /// * `key` - The key to delete
    ///
    /// # Returns
    /// * `Ok(())` on success (even if key doesn't exist)
    /// * `Err(error)` if an error occurs
    fn delete(&self, key: &[u8]) -> Result<()>;

    /// Scan keys with a given prefix
    ///
    /// # Arguments
    /// * `prefix` - The prefix to scan for
    /// * `limit` - Maximum number of results to return (None for unlimited)
    ///
    /// # Returns
    /// * `Ok(vec)` with matching key-value pairs
    /// * `Err(error)` if an error occurs
    fn scan(&self, prefix: &[u8], limit: Option<usize>) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;

    /// Begin a new transaction
    ///
    /// # Returns
    /// * `Ok(transaction)` on success
    /// * `Err(error)` if an error occurs
    fn begin_transaction(&self) -> Result<Box<dyn TransactionAdapter>>;

    /// Flush any pending writes to disk
    ///
    /// # Returns
    /// * `Ok(())` on success
    /// * `Err(error)` if an error occurs
    fn flush(&self) -> Result<()>;
}

/// Transaction adapter trait
///
/// Provides a unified interface for transactional operations.
pub trait TransactionAdapter: Send + Sync {
    /// Get a value within the transaction context
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Put a key-value pair within the transaction
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()>;

    /// Delete a key within the transaction
    fn delete(&mut self, key: &[u8]) -> Result<()>;

    /// Commit the transaction
    fn commit(self: Box<Self>) -> Result<()>;

    /// Rollback the transaction
    fn rollback(self: Box<Self>) -> Result<()>;
}

/// Implementation of StorageAdapter for HeliosDB Lite's StorageEngine
pub struct LiteStorageAdapter {
    engine: Arc<StorageEngine>,
}

impl LiteStorageAdapter {
    /// Create a new adapter wrapping the given storage engine
    pub fn new(engine: Arc<StorageEngine>) -> Self {
        Self { engine }
    }

    /// Get a reference to the underlying storage engine
    pub fn engine(&self) -> &Arc<StorageEngine> {
        &self.engine
    }
}

impl StorageAdapter for LiteStorageAdapter {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let key_vec = key.to_vec();
        self.engine.get(&key_vec)
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let key_vec = key.to_vec();
        let value_vec = value.to_vec();
        self.engine.put(&key_vec, &value_vec)
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        let key_vec = key.to_vec();
        self.engine.delete(&key_vec)
    }

    fn scan(&self, prefix: &[u8], limit: Option<usize>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        use rocksdb::IteratorMode;

        let mut results = Vec::new();
        let iter = self.engine.db.iterator(IteratorMode::From(prefix, rocksdb::Direction::Forward));

        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Scan error: {}", e)))?;

            // Check if key starts with prefix
            if key.starts_with(prefix) {
                results.push((key.to_vec(), value.to_vec()));

                // Check limit
                if let Some(limit) = limit {
                    if results.len() >= limit {
                        break;
                    }
                }
            } else {
                // Keys are sorted, so we can stop once we pass the prefix
                break;
            }
        }

        Ok(results)
    }

    fn begin_transaction(&self) -> Result<Box<dyn TransactionAdapter>> {
        let txn = self.engine.begin_transaction()?;
        Ok(Box::new(LiteTransactionAdapter { txn: Some(txn) }))
    }

    fn flush(&self) -> Result<()> {
        self.engine.flush()
    }
}

/// Implementation of TransactionAdapter for HeliosDB Lite's Transaction
struct LiteTransactionAdapter {
    txn: Option<Transaction>,
}

impl TransactionAdapter for LiteTransactionAdapter {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.txn.as_ref()
            .ok_or_else(|| Error::transaction("Transaction already consumed"))?
            .get(&key.to_vec())
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.txn.as_mut()
            .ok_or_else(|| Error::transaction("Transaction already consumed"))?
            .put(key.to_vec(), value.to_vec())
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.txn.as_mut()
            .ok_or_else(|| Error::transaction("Transaction already consumed"))?
            .delete(key.to_vec())
    }

    fn commit(mut self: Box<Self>) -> Result<()> {
        let txn = self.txn.take()
            .ok_or_else(|| Error::transaction("Transaction already consumed"))?;
        txn.commit()
    }

    fn rollback(mut self: Box<Self>) -> Result<()> {
        let txn = self.txn.take()
            .ok_or_else(|| Error::transaction("Transaction already consumed"))?;
        txn.rollback()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn test_storage_adapter_get_put() -> Result<()> {
        let config = Config::in_memory();
        let engine = Arc::new(StorageEngine::open_in_memory(&config)?);
        let adapter = LiteStorageAdapter::new(engine);

        let key = b"test_key";
        let value = b"test_value";

        // Put
        adapter.put(key, value)?;

        // Get
        let result = adapter.get(key)?;
        assert_eq!(result, Some(value.to_vec()));
        Ok(())
    }

    #[test]
    fn test_storage_adapter_delete() -> Result<()> {
        let config = Config::in_memory();
        let engine = Arc::new(StorageEngine::open_in_memory(&config)?);
        let adapter = LiteStorageAdapter::new(engine);

        let key = b"test_key";
        let value = b"test_value";

        adapter.put(key, value)?;
        adapter.delete(key)?;

        let result = adapter.get(key)?;
        assert_eq!(result, None);
        Ok(())
    }

    #[test]
    fn test_storage_adapter_scan() -> Result<()> {
        let config = Config::in_memory();
        let engine = Arc::new(StorageEngine::open_in_memory(&config)?);
        let adapter = LiteStorageAdapter::new(engine);

        // Put multiple keys with same prefix
        adapter.put(b"prefix:key1", b"value1")?;
        adapter.put(b"prefix:key2", b"value2")?;
        adapter.put(b"prefix:key3", b"value3")?;
        adapter.put(b"other:key", b"other")?;

        // Scan with prefix
        let results = adapter.scan(b"prefix:", None)?;
        assert_eq!(results.len(), 3);

        // Scan with limit
        let results = adapter.scan(b"prefix:", Some(2))?;
        assert_eq!(results.len(), 2);
        Ok(())
    }

    #[test]
    fn test_transaction_adapter() -> Result<()> {
        let config = Config::in_memory();
        let engine = Arc::new(StorageEngine::open_in_memory(&config)?);
        let adapter = LiteStorageAdapter::new(engine);

        let key = b"txn_key";
        let value = b"txn_value";

        // Begin transaction
        let mut txn = adapter.begin_transaction()?;

        // Put in transaction
        txn.put(key, value)?;

        // Commit
        txn.commit()?;

        // Verify
        let result = adapter.get(key)?;
        assert_eq!(result, Some(value.to_vec()));
        Ok(())
    }

    #[test]
    fn test_transaction_rollback() -> Result<()> {
        let config = Config::in_memory();
        let engine = Arc::new(StorageEngine::open_in_memory(&config)?);
        let adapter = LiteStorageAdapter::new(engine);

        let key = b"rollback_key";
        let value = b"rollback_value";

        // Begin transaction
        let mut txn = adapter.begin_transaction()?;

        // Put in transaction
        txn.put(key, value)?;

        // Rollback
        txn.rollback()?;

        // Verify key doesn't exist
        let result = adapter.get(key)?;
        assert_eq!(result, None);
        Ok(())
    }
}
