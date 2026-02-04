//! Offline queue for storing changes when disconnected

use super::{RowDelta, SyncError};
use chrono::Utc;
use rocksdb::{Options, DB};
use std::path::PathBuf;

/// Offline queue for storing changes when disconnected
pub struct OfflineQueue {
    db: DB,
    max_size: usize, // Max queue size (bytes)
    current_size: usize,
}

impl OfflineQueue {
    /// Create a new offline queue
    pub fn new(path: PathBuf, max_size: usize) -> Result<Self, SyncError> {
        let mut opts = Options::default();
        opts.create_if_missing(true);

        let db = DB::open(&opts, path).map_err(|e| SyncError::Storage(e.to_string()))?;

        Ok(Self {
            db,
            max_size,
            current_size: 0,
        })
    }

    /// Enqueue a delta for later sync
    pub fn enqueue(&mut self, delta: RowDelta) -> Result<(), SyncError> {
        let key = format!(
            "queue:{}:{}",
            delta.timestamp.timestamp_millis(),
            hex::encode(&delta.row_id)
        );
        let value =
            bincode::serialize(&delta).map_err(|e| SyncError::Serialization(e.to_string()))?;

        if self.current_size + value.len() > self.max_size {
            return Err(SyncError::QueueFull);
        }

        self.db
            .put(key.as_bytes(), &value)
            .map_err(|e| SyncError::Storage(e.to_string()))?;

        self.current_size += value.len();

        Ok(())
    }

    /// Drain all queued deltas
    pub fn drain(&mut self) -> Result<Vec<RowDelta>, SyncError> {
        let mut deltas = Vec::new();

        let iter = self.db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            let (key, value) = item.map_err(|e| SyncError::Storage(e.to_string()))?;

            let delta: RowDelta = bincode::deserialize(&value)
                .map_err(|e| SyncError::Serialization(e.to_string()))?;

            deltas.push(delta);

            // Delete from queue
            self.db
                .delete(&key)
                .map_err(|e| SyncError::Storage(e.to_string()))?;
        }

        self.current_size = 0;

        Ok(deltas)
    }

    /// Get current queue size in bytes
    pub fn size(&self) -> usize {
        self.current_size
    }

    /// Get number of items in queue
    pub fn count(&self) -> Result<usize, SyncError> {
        let mut count = 0;
        let iter = self.db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            item.map_err(|e| SyncError::Storage(e.to_string()))?;
            count += 1;
        }
        Ok(count)
    }

    /// Clear all queued items
    pub fn clear(&mut self) -> Result<(), SyncError> {
        let keys: Vec<_> = self
            .db
            .iterator(rocksdb::IteratorMode::Start)
            .filter_map(|item| item.ok().map(|(k, _)| k))
            .collect();

        for key in keys {
            self.db
                .delete(&key)
                .map_err(|e| SyncError::Storage(e.to_string()))?;
        }

        self.current_size = 0;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sync::{Operation, VectorClock};
    use tempfile::TempDir;

    #[test]
    fn test_offline_queue() {
        let tmp_dir = TempDir::new().unwrap();
        let queue_path = tmp_dir.path().join("queue");

        let mut queue = OfflineQueue::new(queue_path, 1024 * 1024).unwrap();

        // Create a test delta
        let delta = RowDelta {
            table: "users".to_string(),
            operation: Operation::Insert,
            row_id: vec![1, 2, 3],
            data: vec![4, 5, 6],
            vector_clock: VectorClock::default(),
            timestamp: Utc::now(),
            checksum: 0,
        };

        // Enqueue
        queue.enqueue(delta.clone()).unwrap();
        assert_eq!(queue.count().unwrap(), 1);

        // Drain
        let drained = queue.drain().unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(queue.count().unwrap(), 0);
    }

    #[test]
    fn test_queue_full() {
        let tmp_dir = TempDir::new().unwrap();
        let queue_path = tmp_dir.path().join("queue");

        let mut queue = OfflineQueue::new(queue_path, 100).unwrap(); // Very small

        let delta = RowDelta {
            table: "users".to_string(),
            operation: Operation::Insert,
            row_id: vec![1; 50],
            data: vec![4; 50],
            vector_clock: VectorClock::default(),
            timestamp: Utc::now(),
            checksum: 0,
        };

        // First one should succeed
        queue.enqueue(delta.clone()).unwrap();

        // Second should fail (queue full)
        let result = queue.enqueue(delta.clone());
        assert!(matches!(result, Err(SyncError::QueueFull)));
    }
}
