//! Dirty State Tracking for HeliosDB-Lite
//!
//! Tracks uncommitted changes for multi-user ACID in-memory mode (v3.1.0).
//! Provides lock-free dirty state management with sequential numbering
//! for incremental dump support.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use parking_lot::RwLock;
use std::time::Instant;
use std::collections::VecDeque;

/// Type of change operation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeType {
    Insert,
    Update,
    Delete,
}

/// A single tracked change
#[derive(Debug, Clone)]
pub struct Change {
    /// Unique sequence number for ordering
    pub sequence_number: u64,
    /// Timestamp when the change occurred
    pub timestamp: Instant,
    /// Type of change operation
    pub change_type: ChangeType,
    /// Table name
    pub table_name: String,
    /// Row key (primary key or unique identifier)
    pub row_key: String,
    /// Old values (None for Insert, Some for Update/Delete)
    pub old_values: Option<Vec<u8>>,
    /// New values (None for Delete, Some for Insert/Update)
    pub new_values: Option<Vec<u8>>,
}

/// Result type for DirtyTracker operations
pub type Result<T> = std::result::Result<T, DirtyTrackerError>;

/// Error types for DirtyTracker
#[derive(Debug, Clone)]
pub enum DirtyTrackerError {
    /// Buffer overflow - max change buffer size exceeded
    BufferOverflow { max_size: usize, current_size: usize },
    /// Lock contention error
    LockError(String),
    /// Invalid parameters
    InvalidParameter(String),
}

impl std::fmt::Display for DirtyTrackerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BufferOverflow { max_size, current_size } => {
                write!(f, "Buffer overflow: current size {} exceeds max {}", current_size, max_size)
            }
            Self::LockError(msg) => write!(f, "Lock error: {}", msg),
            Self::InvalidParameter(msg) => write!(f, "Invalid parameter: {}", msg),
        }
    }
}

impl std::error::Error for DirtyTrackerError {}

/// Maximum number of changes to buffer in memory
const DEFAULT_MAX_BUFFER_SIZE: usize = 100_000;

/// Internal change buffer structure
struct ChangeBuffer {
    /// Circular buffer of changes
    changes: VecDeque<Change>,
    /// Maximum buffer size before overflow
    max_size: usize,
    /// Next sequence number to assign
    next_sequence: u64,
}

impl ChangeBuffer {
    fn new(max_size: usize) -> Self {
        Self {
            changes: VecDeque::with_capacity(max_size),
            max_size,
            next_sequence: 1,
        }
    }

    fn add_change(&mut self, change: Change) -> Result<()> {
        // Check for buffer overflow
        if self.changes.len() >= self.max_size {
            // In a circular buffer, remove oldest change
            self.changes.pop_front();
        }

        self.changes.push_back(change);
        self.next_sequence += 1;
        Ok(())
    }

    fn get_changes_since(&self, seq_num: u64) -> Vec<Change> {
        self.changes
            .iter()
            .filter(|c| c.sequence_number > seq_num)
            .cloned()
            .collect()
    }

    fn clear(&mut self) {
        self.changes.clear();
    }

    fn len(&self) -> usize {
        self.changes.len()
    }

    fn next_sequence_number(&self) -> u64 {
        self.next_sequence
    }
}

/// Tracks dirty state for in-memory database changes
///
/// Provides lock-free dirty flag tracking with detailed change history
/// for incremental dump support. Thread-safe for concurrent access.
pub struct DirtyTracker {
    /// Change history buffer
    changes: Arc<RwLock<ChangeBuffer>>,
    /// Sequence number of last dump
    last_dump_seq: Arc<AtomicU64>,
    /// Lock-free dirty flag
    is_dirty: Arc<AtomicBool>,
}

impl DirtyTracker {
    /// Create a new DirtyTracker with default buffer size
    pub fn new() -> Self {
        Self::with_max_buffer_size(DEFAULT_MAX_BUFFER_SIZE)
    }

    /// Create a new DirtyTracker with custom buffer size
    pub fn with_max_buffer_size(max_size: usize) -> Self {
        Self {
            changes: Arc::new(RwLock::new(ChangeBuffer::new(max_size))),
            last_dump_seq: Arc::new(AtomicU64::new(0)),
            is_dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Track an insert operation
    ///
    /// # Arguments
    /// * `table` - Table name
    /// * `row_key` - Row key (primary key)
    /// * `values` - Serialized row values
    pub fn track_insert(&self, table: &str, row_key: &str, values: &[u8]) -> Result<()> {
        if table.is_empty() || row_key.is_empty() {
            return Err(DirtyTrackerError::InvalidParameter(
                "Table name and row key cannot be empty".to_string()
            ));
        }

        let mut buffer = self.changes.write();

        let sequence = buffer.next_sequence_number();
        let change = Change {
            sequence_number: sequence,
            timestamp: Instant::now(),
            change_type: ChangeType::Insert,
            table_name: table.to_string(),
            row_key: row_key.to_string(),
            old_values: None,
            new_values: Some(values.to_vec()),
        };

        buffer.add_change(change)?;
        self.is_dirty.store(true, Ordering::Release);
        Ok(())
    }

    /// Track an update operation
    ///
    /// # Arguments
    /// * `table` - Table name
    /// * `row_key` - Row key (primary key)
    /// * `old_values` - Serialized old row values
    /// * `new_values` - Serialized new row values
    pub fn track_update(
        &self,
        table: &str,
        row_key: &str,
        old_values: &[u8],
        new_values: &[u8],
    ) -> Result<()> {
        if table.is_empty() || row_key.is_empty() {
            return Err(DirtyTrackerError::InvalidParameter(
                "Table name and row key cannot be empty".to_string()
            ));
        }

        let mut buffer = self.changes.write();

        let sequence = buffer.next_sequence_number();
        let change = Change {
            sequence_number: sequence,
            timestamp: Instant::now(),
            change_type: ChangeType::Update,
            table_name: table.to_string(),
            row_key: row_key.to_string(),
            old_values: Some(old_values.to_vec()),
            new_values: Some(new_values.to_vec()),
        };

        buffer.add_change(change)?;
        self.is_dirty.store(true, Ordering::Release);
        Ok(())
    }

    /// Track a delete operation
    ///
    /// # Arguments
    /// * `table` - Table name
    /// * `row_key` - Row key (primary key)
    /// * `values` - Serialized row values being deleted
    pub fn track_delete(&self, table: &str, row_key: &str, values: &[u8]) -> Result<()> {
        if table.is_empty() || row_key.is_empty() {
            return Err(DirtyTrackerError::InvalidParameter(
                "Table name and row key cannot be empty".to_string()
            ));
        }

        let mut buffer = self.changes.write();

        let sequence = buffer.next_sequence_number();
        let change = Change {
            sequence_number: sequence,
            timestamp: Instant::now(),
            change_type: ChangeType::Delete,
            table_name: table.to_string(),
            row_key: row_key.to_string(),
            old_values: Some(values.to_vec()),
            new_values: None,
        };

        buffer.add_change(change)?;
        self.is_dirty.store(true, Ordering::Release);
        Ok(())
    }

    /// Check if there are uncommitted changes
    pub fn is_dirty(&self) -> bool {
        self.is_dirty.load(Ordering::Acquire)
    }

    /// Get count of dirty changes since last dump
    pub fn get_dirty_count(&self) -> u64 {
        let buffer = self.changes.read();
        let last_dump = self.last_dump_seq.load(Ordering::Acquire);
        buffer.changes
            .iter()
            .filter(|c| c.sequence_number > last_dump)
            .count() as u64
    }

    /// Get list of tables with dirty changes
    pub fn get_dirty_tables(&self) -> Vec<String> {
        let buffer = self.changes.read();
        let last_dump = self.last_dump_seq.load(Ordering::Acquire);

        let mut table_set = std::collections::HashSet::new();
        for change in buffer.changes.iter() {
            if change.sequence_number > last_dump {
                table_set.insert(change.table_name.clone());
            }
        }

        let mut tables: Vec<String> = table_set.into_iter().collect();
        tables.sort();
        tables
    }

    /// Clear dirty state after successful dump
    ///
    /// This marks the current sequence number as the last dump point
    /// and clears the dirty flag if no new changes occurred.
    pub fn clear_dirty_state(&self) -> Result<()> {
        let buffer = self.changes.read();
        // Store the last USED sequence number (not the next one)
        // This ensures changes made after clear are correctly counted as dirty
        let last_used_seq = buffer.next_sequence_number().saturating_sub(1);
        drop(buffer);

        self.last_dump_seq.store(last_used_seq, Ordering::Release);

        // Check if there are any new changes since we read the sequence
        let buffer = self.changes.read();
        if buffer.next_sequence_number() == last_used_seq + 1 {
            self.is_dirty.store(false, Ordering::Release);
        }

        Ok(())
    }

    /// Get all changes since a specific sequence number
    ///
    /// # Arguments
    /// * `seq_num` - Sequence number to get changes after
    ///
    /// # Returns
    /// Vector of changes with sequence number greater than `seq_num`
    pub fn get_changes_since(&self, seq_num: u64) -> Vec<Change> {
        let buffer = self.changes.read();
        buffer.get_changes_since(seq_num)
    }

    /// Get the current sequence number
    pub fn current_sequence_number(&self) -> u64 {
        let buffer = self.changes.read();
        buffer.next_sequence_number()
    }

    /// Get the last dump sequence number
    pub fn last_dump_sequence_number(&self) -> u64 {
        self.last_dump_seq.load(Ordering::Acquire)
    }
}

impl Default for DirtyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for DirtyTracker {
    fn clone(&self) -> Self {
        Self {
            changes: Arc::clone(&self.changes),
            last_dump_seq: Arc::clone(&self.last_dump_seq),
            is_dirty: Arc::clone(&self.is_dirty),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_track_insert() {
        let tracker = DirtyTracker::new();
        assert!(!tracker.is_dirty());

        let values = vec![1, 2, 3, 4];
        tracker.track_insert("users", "key1", &values).unwrap();

        assert!(tracker.is_dirty());
        assert_eq!(tracker.get_dirty_count(), 1);
    }

    #[test]
    fn test_track_update() {
        let tracker = DirtyTracker::new();

        let old_values = vec![1, 2, 3];
        let new_values = vec![4, 5, 6];
        tracker.track_update("users", "key1", &old_values, &new_values).unwrap();

        assert!(tracker.is_dirty());
        assert_eq!(tracker.get_dirty_count(), 1);

        let changes = tracker.get_changes_since(0);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::Update);
        assert_eq!(changes[0].table_name, "users");
        assert_eq!(changes[0].row_key, "key1");
        assert_eq!(changes[0].old_values, Some(old_values));
        assert_eq!(changes[0].new_values, Some(new_values));
    }

    #[test]
    fn test_track_delete() {
        let tracker = DirtyTracker::new();

        let values = vec![1, 2, 3];
        tracker.track_delete("users", "key1", &values).unwrap();

        assert!(tracker.is_dirty());
        assert_eq!(tracker.get_dirty_count(), 1);

        let changes = tracker.get_changes_since(0);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::Delete);
        assert!(changes[0].new_values.is_none());
        assert!(changes[0].old_values.is_some());
    }

    #[test]
    fn test_dirty_state_flag() {
        let tracker = DirtyTracker::new();
        assert!(!tracker.is_dirty());

        tracker.track_insert("users", "key1", &[1, 2, 3]).unwrap();
        assert!(tracker.is_dirty());

        tracker.clear_dirty_state().unwrap();
        assert!(!tracker.is_dirty());
    }

    #[test]
    fn test_changes_since_sequence() {
        let tracker = DirtyTracker::new();

        tracker.track_insert("users", "key1", &[1]).unwrap();
        tracker.track_insert("users", "key2", &[2]).unwrap();
        tracker.track_insert("users", "key3", &[3]).unwrap();

        let changes = tracker.get_changes_since(0);
        assert_eq!(changes.len(), 3);

        let changes = tracker.get_changes_since(1);
        assert_eq!(changes.len(), 2);

        let changes = tracker.get_changes_since(2);
        assert_eq!(changes.len(), 1);

        let changes = tracker.get_changes_since(3);
        assert_eq!(changes.len(), 0);
    }

    #[test]
    fn test_get_dirty_tables() {
        let tracker = DirtyTracker::new();

        tracker.track_insert("users", "key1", &[1]).unwrap();
        tracker.track_insert("orders", "key2", &[2]).unwrap();
        tracker.track_insert("users", "key3", &[3]).unwrap();
        tracker.track_insert("products", "key4", &[4]).unwrap();

        let mut tables = tracker.get_dirty_tables();
        tables.sort();

        assert_eq!(tables, vec!["orders", "products", "users"]);
    }

    #[test]
    fn test_buffer_overflow_handling() {
        let tracker = DirtyTracker::with_max_buffer_size(3);

        // Add 5 changes to a buffer that holds max 3
        tracker.track_insert("users", "key1", &[1]).unwrap();
        tracker.track_insert("users", "key2", &[2]).unwrap();
        tracker.track_insert("users", "key3", &[3]).unwrap();
        tracker.track_insert("users", "key4", &[4]).unwrap();
        tracker.track_insert("users", "key5", &[5]).unwrap();

        // Should have only the last 3 changes
        let changes = tracker.get_changes_since(0);
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].row_key, "key3");
        assert_eq!(changes[1].row_key, "key4");
        assert_eq!(changes[2].row_key, "key5");
    }

    #[test]
    fn test_concurrent_tracking() {
        let tracker = Arc::new(DirtyTracker::new());
        let mut handles = vec![];

        // Spawn 10 threads, each inserting 100 records
        for thread_id in 0..10 {
            let tracker_clone = Arc::clone(&tracker);
            let handle = thread::spawn(move || {
                for i in 0..100 {
                    let key = format!("key_{}_{}", thread_id, i);
                    let values = vec![thread_id as u8, i as u8];
                    tracker_clone.track_insert("users", &key, &values).unwrap();
                }
            });
            handles.push(handle);
        }

        // Wait for all threads to complete
        for handle in handles {
            handle.join().unwrap();
        }

        // Should have tracked all 1000 changes
        assert!(tracker.is_dirty());
        assert_eq!(tracker.get_dirty_count(), 1000);
    }

    #[test]
    fn test_clear_dirty_state_incremental() {
        let tracker = DirtyTracker::new();

        // First batch of changes
        tracker.track_insert("users", "key1", &[1]).unwrap();
        tracker.track_insert("users", "key2", &[2]).unwrap();
        assert_eq!(tracker.get_dirty_count(), 2);

        // Clear dirty state (simulate dump)
        tracker.clear_dirty_state().unwrap();
        assert!(!tracker.is_dirty());
        assert_eq!(tracker.get_dirty_count(), 0);

        // Second batch of changes
        tracker.track_insert("users", "key3", &[3]).unwrap();
        tracker.track_insert("users", "key4", &[4]).unwrap();
        assert_eq!(tracker.get_dirty_count(), 2);

        // Should have all 4 changes in history
        let all_changes = tracker.get_changes_since(0);
        assert_eq!(all_changes.len(), 4);

        // But only 2 are "dirty" (since last dump)
        let last_dump = tracker.last_dump_sequence_number();
        let dirty_changes = tracker.get_changes_since(last_dump);
        assert_eq!(dirty_changes.len(), 2);
    }

    #[test]
    fn test_invalid_parameters() {
        let tracker = DirtyTracker::new();

        // Empty table name
        let result = tracker.track_insert("", "key1", &[1, 2, 3]);
        assert!(result.is_err());

        // Empty row key
        let result = tracker.track_insert("users", "", &[1, 2, 3]);
        assert!(result.is_err());
    }

    #[test]
    fn test_sequence_numbers_monotonic() {
        let tracker = DirtyTracker::new();

        tracker.track_insert("users", "key1", &[1]).unwrap();
        let seq1 = tracker.current_sequence_number();

        tracker.track_insert("users", "key2", &[2]).unwrap();
        let seq2 = tracker.current_sequence_number();

        tracker.track_insert("users", "key3", &[3]).unwrap();
        let seq3 = tracker.current_sequence_number();

        // Sequence numbers should be strictly increasing
        assert!(seq2 > seq1);
        assert!(seq3 > seq2);
    }

    #[test]
    fn test_clone_shares_state() {
        let tracker1 = DirtyTracker::new();
        tracker1.track_insert("users", "key1", &[1]).unwrap();

        let tracker2 = tracker1.clone();

        // Both should see the same state
        assert!(tracker2.is_dirty());
        assert_eq!(tracker2.get_dirty_count(), 1);

        // Changes to clone affect original
        tracker2.track_insert("users", "key2", &[2]).unwrap();
        assert_eq!(tracker1.get_dirty_count(), 2);
    }
}
