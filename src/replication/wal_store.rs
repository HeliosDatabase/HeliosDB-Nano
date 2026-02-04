//! WAL Store - Persistent WAL Storage for Replication
//!
//! Provides an interface for storing and retrieving WAL entries.
//! Used for both real-time streaming and batch catch-up.
//!
//! # Storage Layout
//!
//! ```text
//! wal/
//! ├── segment_000001.wal  (entries 0 - 999)
//! ├── segment_000002.wal  (entries 1000 - 1999)
//! ├── segment_000003.wal  (entries 2000 - current)
//! └── index.dat           (LSN -> segment mapping)
//! ```
//!
//! # Batch Catch-Up Flow
//!
//! 1. Standby connects with current_lsn = X
//! 2. Primary checks: primary_lsn = Y where Y > X
//! 3. Primary fetches entries [X+1, Y] from WAL store
//! 4. Primary sends WalBatch messages (configurable batch size)
//! 5. After catch-up, switch to real-time streaming

use super::wal_replicator::{Lsn, WalEntry};
use super::Result;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

/// WAL segment metadata
#[derive(Debug, Clone)]
pub struct WalSegmentInfo {
    /// Segment ID (sequential)
    pub segment_id: u64,
    /// First LSN in this segment
    pub start_lsn: Lsn,
    /// Last LSN in this segment (inclusive)
    pub end_lsn: Lsn,
    /// Number of entries
    pub entry_count: u64,
    /// Segment size in bytes
    pub size_bytes: u64,
    /// Is this segment complete (closed)
    pub is_complete: bool,
    /// Segment file path
    pub path: PathBuf,
}

/// WAL Store configuration
#[derive(Debug, Clone)]
pub struct WalStoreConfig {
    /// Base directory for WAL files
    pub wal_dir: PathBuf,
    /// Maximum segment size in bytes
    pub max_segment_size: usize,
    /// Maximum entries per segment
    pub max_entries_per_segment: usize,
    /// Number of segments to retain
    pub retention_segments: usize,
    /// Enable fsync after each write
    pub fsync_on_write: bool,
    /// In-memory cache size (number of entries)
    pub cache_size: usize,
}

impl Default for WalStoreConfig {
    fn default() -> Self {
        Self {
            wal_dir: PathBuf::from("./data/wal"),
            max_segment_size: 16 * 1024 * 1024, // 16 MB
            max_entries_per_segment: 10_000,
            retention_segments: 64,
            fsync_on_write: true,
            cache_size: 10_000,
        }
    }
}

/// Batch retrieval request
#[derive(Debug, Clone)]
pub struct BatchRequest {
    /// Start LSN (exclusive - fetch entries after this LSN)
    pub from_lsn: Lsn,
    /// End LSN (inclusive, or None for latest)
    pub to_lsn: Option<Lsn>,
    /// Maximum number of entries to return
    pub max_entries: usize,
    /// Maximum bytes to return
    pub max_bytes: usize,
}

impl Default for BatchRequest {
    fn default() -> Self {
        Self {
            from_lsn: 0,
            to_lsn: None,
            max_entries: 1000,
            max_bytes: 10 * 1024 * 1024, // 10 MB
        }
    }
}

/// Batch retrieval result
#[derive(Debug, Clone)]
pub struct BatchResult {
    /// Retrieved entries
    pub entries: Vec<WalEntry>,
    /// First LSN in batch
    pub start_lsn: Lsn,
    /// Last LSN in batch
    pub end_lsn: Lsn,
    /// Whether there are more entries available
    pub has_more: bool,
    /// Total bytes in batch
    pub total_bytes: usize,
}

/// WAL Store - manages WAL persistence and retrieval
///
/// This is an in-memory implementation for now.
/// Production would use memory-mapped files or RocksDB.
pub struct WalStore {
    /// Configuration
    config: WalStoreConfig,
    /// Current write LSN
    current_lsn: Arc<AtomicU64>,
    /// LSN index (LSN -> segment_id)
    lsn_index: Arc<RwLock<BTreeMap<Lsn, u64>>>,
    /// Segment metadata
    segments: Arc<RwLock<HashMap<u64, WalSegmentInfo>>>,
    /// Current segment ID
    current_segment: Arc<AtomicU64>,
    /// In-memory entry cache (for recent entries)
    cache: Arc<RwLock<VecDeque<WalEntry>>>,
    /// All entries (in-memory storage for now)
    entries: Arc<RwLock<BTreeMap<Lsn, WalEntry>>>,
    /// Minimum retained LSN
    min_retained_lsn: Arc<AtomicU64>,
}

impl WalStore {
    /// Create a new WAL store
    pub fn new(config: WalStoreConfig) -> Self {
        Self {
            config,
            current_lsn: Arc::new(AtomicU64::new(0)),
            lsn_index: Arc::new(RwLock::new(BTreeMap::new())),
            segments: Arc::new(RwLock::new(HashMap::new())),
            current_segment: Arc::new(AtomicU64::new(0)),
            cache: Arc::new(RwLock::new(VecDeque::new())),
            entries: Arc::new(RwLock::new(BTreeMap::new())),
            min_retained_lsn: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Initialize the WAL store (load existing segments)
    pub async fn init(&self) -> Result<()> {
        // TODO: Scan wal_dir for existing segments
        // Load segment metadata into memory
        // Set current_lsn to highest LSN found

        tracing::info!("WAL store initialized at {:?}", self.config.wal_dir);
        Ok(())
    }

    /// Append a WAL entry
    pub async fn append(&self, entry: WalEntry) -> Result<Lsn> {
        let lsn = entry.lsn;

        // Store in entries map
        {
            let mut entries = self.entries.write().await;
            entries.insert(lsn, entry.clone());
        }

        // Add to cache
        {
            let mut cache = self.cache.write().await;
            cache.push_back(entry);
            while cache.len() > self.config.cache_size {
                cache.pop_front();
            }
        }

        // Update current LSN
        self.current_lsn.store(lsn, Ordering::SeqCst);

        // Update LSN index
        {
            let segment_id = self.current_segment.load(Ordering::SeqCst);
            let mut index = self.lsn_index.write().await;
            index.insert(lsn, segment_id);
        }

        // TODO: Check if we need to rotate segment
        // TODO: Write to disk if configured

        Ok(lsn)
    }

    /// Get a single entry by LSN
    pub async fn get(&self, lsn: Lsn) -> Option<WalEntry> {
        // Check cache first
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.iter().find(|e| e.lsn == lsn) {
                return Some(entry.clone());
            }
        }

        // Check entries map
        let entries = self.entries.read().await;
        entries.get(&lsn).cloned()
    }

    /// Get a batch of entries for catch-up
    pub async fn get_batch(&self, request: BatchRequest) -> Result<BatchResult> {
        let entries = self.entries.read().await;

        let end_lsn = request.to_lsn.unwrap_or(self.current_lsn.load(Ordering::SeqCst));

        // Get range of entries
        let range = entries.range((
            std::ops::Bound::Excluded(request.from_lsn),
            std::ops::Bound::Included(end_lsn),
        ));

        let mut batch_entries = Vec::new();
        let mut total_bytes = 0;
        let mut actual_start_lsn = 0;
        let mut actual_end_lsn = 0;
        let mut has_more = false;

        for (lsn, entry) in range {
            if batch_entries.len() >= request.max_entries {
                has_more = true;
                break;
            }

            let entry_size = entry.data.len() + 32; // Approximate overhead
            if total_bytes + entry_size > request.max_bytes && !batch_entries.is_empty() {
                has_more = true;
                break;
            }

            if batch_entries.is_empty() {
                actual_start_lsn = *lsn;
            }
            actual_end_lsn = *lsn;

            batch_entries.push(entry.clone());
            total_bytes += entry_size;
        }

        // Check if there are more entries after this batch
        if !has_more && actual_end_lsn < end_lsn {
            has_more = entries.range((
                std::ops::Bound::Excluded(actual_end_lsn),
                std::ops::Bound::Included(end_lsn),
            )).next().is_some();
        }

        Ok(BatchResult {
            entries: batch_entries,
            start_lsn: actual_start_lsn,
            end_lsn: actual_end_lsn,
            has_more,
            total_bytes,
        })
    }

    /// Get all entries in a range (for small ranges)
    pub async fn get_range(&self, start_lsn: Lsn, end_lsn: Lsn) -> Vec<WalEntry> {
        let entries = self.entries.read().await;
        entries
            .range(start_lsn..=end_lsn)
            .map(|(_, e)| e.clone())
            .collect()
    }

    /// Get current write LSN
    pub fn current_lsn(&self) -> Lsn {
        self.current_lsn.load(Ordering::SeqCst)
    }

    /// Get minimum retained LSN
    pub fn min_retained_lsn(&self) -> Lsn {
        self.min_retained_lsn.load(Ordering::SeqCst)
    }

    /// Check if we have entries from a given LSN
    pub async fn has_entries_from(&self, lsn: Lsn) -> bool {
        let min_lsn = self.min_retained_lsn.load(Ordering::SeqCst);
        lsn >= min_lsn
    }

    /// Get segment info for an LSN
    pub async fn get_segment_for_lsn(&self, lsn: Lsn) -> Option<WalSegmentInfo> {
        let index = self.lsn_index.read().await;
        let segment_id = index.range(..=lsn).next_back()?.1;
        let segments = self.segments.read().await;
        segments.get(segment_id).cloned()
    }

    /// List all segments
    pub async fn list_segments(&self) -> Vec<WalSegmentInfo> {
        let segments = self.segments.read().await;
        let mut list: Vec<_> = segments.values().cloned().collect();
        list.sort_by_key(|s| s.segment_id);
        list
    }

    /// Truncate WAL entries before a given LSN (for cleanup)
    pub async fn truncate_before(&self, lsn: Lsn) -> Result<u64> {
        let mut entries = self.entries.write().await;
        let to_remove: Vec<Lsn> = entries.range(..lsn).map(|(k, _)| *k).collect();
        let count = to_remove.len() as u64;

        for key in to_remove {
            entries.remove(&key);
        }

        self.min_retained_lsn.store(lsn, Ordering::SeqCst);

        // Clean up LSN index
        {
            let mut index = self.lsn_index.write().await;
            index.retain(|k, _| *k >= lsn);
        }

        tracing::info!("Truncated {} entries before LSN {}", count, lsn);
        Ok(count)
    }

    /// Create a checkpoint (flush and mark a safe point)
    pub async fn checkpoint(&self) -> Result<Lsn> {
        let checkpoint_lsn = self.current_lsn.load(Ordering::SeqCst);

        // TODO: Flush current segment to disk
        // TODO: Update checkpoint marker

        tracing::info!("WAL checkpoint at LSN {}", checkpoint_lsn);
        Ok(checkpoint_lsn)
    }

    /// Get statistics about the WAL store
    pub async fn stats(&self) -> WalStoreStats {
        let entries = self.entries.read().await;
        let segments = self.segments.read().await;

        WalStoreStats {
            current_lsn: self.current_lsn.load(Ordering::SeqCst),
            min_retained_lsn: self.min_retained_lsn.load(Ordering::SeqCst),
            total_entries: entries.len() as u64,
            total_segments: segments.len() as u64,
            cache_size: self.cache.read().await.len() as u64,
        }
    }

    /// Close the WAL store
    pub async fn close(&self) -> Result<()> {
        // TODO: Flush pending writes
        // TODO: Close file handles
        tracing::info!("WAL store closed");
        Ok(())
    }
}

/// WAL store statistics
#[derive(Debug, Clone)]
pub struct WalStoreStats {
    /// Current write LSN
    pub current_lsn: Lsn,
    /// Minimum retained LSN
    pub min_retained_lsn: Lsn,
    /// Total entries stored
    pub total_entries: u64,
    /// Total segments
    pub total_segments: u64,
    /// Cache size
    pub cache_size: u64,
}

/// Iterator over WAL entries
pub struct WalEntryIterator {
    entries: Vec<WalEntry>,
    position: usize,
}

impl Iterator for WalEntryIterator {
    type Item = WalEntry;

    fn next(&mut self) -> Option<Self::Item> {
        if self.position < self.entries.len() {
            let entry = self.entries[self.position].clone();
            self.position += 1;
            Some(entry)
        } else {
            None
        }
    }
}

// =============================================================================
// BATCH STREAMING HELPERS
// =============================================================================

/// Batch streaming state
pub struct BatchStreamState {
    /// Request parameters
    pub request: BatchRequest,
    /// Last sent LSN
    pub last_sent_lsn: Lsn,
    /// Batch number
    pub batch_num: u32,
    /// Total bytes sent
    pub bytes_sent: usize,
    /// Total entries sent
    pub entries_sent: usize,
    /// Is streaming complete
    pub complete: bool,
}

impl BatchStreamState {
    /// Create a new batch stream state
    pub fn new(from_lsn: Lsn, to_lsn: Option<Lsn>) -> Self {
        Self {
            request: BatchRequest {
                from_lsn,
                to_lsn,
                ..Default::default()
            },
            last_sent_lsn: from_lsn,
            batch_num: 0,
            bytes_sent: 0,
            entries_sent: 0,
            complete: false,
        }
    }

    /// Get next batch from store
    pub async fn next_batch(&mut self, store: &WalStore) -> Result<Option<BatchResult>> {
        if self.complete {
            return Ok(None);
        }

        let mut request = self.request.clone();
        request.from_lsn = self.last_sent_lsn;

        let batch = store.get_batch(request).await?;

        if batch.entries.is_empty() {
            self.complete = true;
            return Ok(None);
        }

        self.last_sent_lsn = batch.end_lsn;
        self.batch_num += 1;
        self.bytes_sent += batch.total_bytes;
        self.entries_sent += batch.entries.len();

        if !batch.has_more {
            self.complete = true;
        }

        Ok(Some(batch))
    }

    /// Check if streaming is complete
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Get progress percentage (if to_lsn is known)
    pub fn progress(&self) -> Option<f64> {
        self.request.to_lsn.map(|to| {
            let total = to.saturating_sub(self.request.from_lsn) as f64;
            let done = self.last_sent_lsn.saturating_sub(self.request.from_lsn) as f64;
            if total > 0.0 { done / total * 100.0 } else { 100.0 }
        })
    }
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::wal_replicator::WalEntryType;

    fn make_entry(lsn: Lsn, data: Vec<u8>) -> WalEntry {
        let checksum = crc32fast::hash(&data);
        WalEntry {
            lsn,
            entry_type: WalEntryType::Insert,
            data,
            checksum,
        }
    }

    #[tokio::test]
    async fn test_wal_store_creation() {
        let config = WalStoreConfig::default();
        let store = WalStore::new(config);
        store.init().await.expect("init failed");
        assert_eq!(store.current_lsn(), 0);
    }

    #[tokio::test]
    async fn test_append_and_get() {
        let store = WalStore::new(WalStoreConfig::default());
        store.init().await.expect("init failed");

        let entry = make_entry(1, vec![1, 2, 3]);
        store.append(entry.clone()).await.expect("append failed");

        let retrieved = store.get(1).await.expect("entry not found");
        assert_eq!(retrieved.lsn, 1);
        assert_eq!(retrieved.data, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_get_batch() {
        let store = WalStore::new(WalStoreConfig::default());
        store.init().await.expect("init failed");

        // Append 100 entries
        for i in 1..=100 {
            let entry = make_entry(i, vec![i as u8; 100]);
            store.append(entry).await.expect("append failed");
        }

        // Get batch of 10
        let request = BatchRequest {
            from_lsn: 0,
            to_lsn: Some(100),
            max_entries: 10,
            max_bytes: 10 * 1024 * 1024,
        };

        let batch = store.get_batch(request).await.expect("get_batch failed");
        assert_eq!(batch.entries.len(), 10);
        assert_eq!(batch.start_lsn, 1);
        assert_eq!(batch.end_lsn, 10);
        assert!(batch.has_more);
    }

    #[tokio::test]
    async fn test_batch_stream_state() {
        let store = WalStore::new(WalStoreConfig::default());
        store.init().await.expect("init failed");

        // Append 50 entries
        for i in 1..=50 {
            let entry = make_entry(i, vec![i as u8; 100]);
            store.append(entry).await.expect("append failed");
        }

        let mut state = BatchStreamState::new(0, Some(50));
        state.request.max_entries = 10;

        let mut batch_count = 0;
        while let Some(batch) = state.next_batch(&store).await.expect("next_batch failed") {
            batch_count += 1;
            assert!(batch.entries.len() <= 10);
        }

        assert_eq!(batch_count, 5);
        assert!(state.is_complete());
        assert_eq!(state.entries_sent, 50);
    }

    #[tokio::test]
    async fn test_truncate() {
        let store = WalStore::new(WalStoreConfig::default());
        store.init().await.expect("init failed");

        // Append 100 entries
        for i in 1..=100 {
            let entry = make_entry(i, vec![i as u8; 10]);
            store.append(entry).await.expect("append failed");
        }

        // Truncate entries before 50
        let removed = store.truncate_before(50).await.expect("truncate failed");
        assert_eq!(removed, 49);

        // Verify entry 49 is gone
        assert!(store.get(49).await.is_none());

        // Verify entry 50 still exists
        assert!(store.get(50).await.is_some());
    }

    #[tokio::test]
    async fn test_get_range() {
        let store = WalStore::new(WalStoreConfig::default());
        store.init().await.expect("init failed");

        for i in 1..=20 {
            let entry = make_entry(i, vec![i as u8]);
            store.append(entry).await.expect("append failed");
        }

        let range = store.get_range(5, 10).await;
        assert_eq!(range.len(), 6); // 5, 6, 7, 8, 9, 10
        assert_eq!(range[0].lsn, 5);
        assert_eq!(range[5].lsn, 10);
    }

    #[tokio::test]
    async fn test_stats() {
        let store = WalStore::new(WalStoreConfig::default());
        store.init().await.expect("init failed");

        for i in 1..=10 {
            let entry = make_entry(i, vec![i as u8]);
            store.append(entry).await.expect("append failed");
        }

        let stats = store.stats().await;
        assert_eq!(stats.current_lsn, 10);
        assert_eq!(stats.total_entries, 10);
    }
}
