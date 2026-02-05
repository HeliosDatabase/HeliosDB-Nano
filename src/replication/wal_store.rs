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
//! └── checkpoint.dat      (checkpoint marker)
//! ```
//!
//! # Segment Format
//!
//! Each segment file contains:
//! - Header (32 bytes): magic, version, segment_id, start_lsn, entry_count
//! - Entries: [length (4 bytes) | entry_type (1 byte) | lsn (8 bytes) | checksum (4 bytes) | data]
//!
//! # Batch Catch-Up Flow
//!
//! 1. Standby connects with current_lsn = X
//! 2. Primary checks: primary_lsn = Y where Y > X
//! 3. Primary fetches entries [X+1, Y] from WAL store
//! 4. Primary sends WalBatch messages (configurable batch size)
//! 5. After catch-up, switch to real-time streaming

use super::wal_replicator::{Lsn, WalEntry, WalEntryType};
use super::{ReplicationError, Result};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write as IoWrite};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

// WAL file magic number
const WAL_MAGIC: u32 = 0x57414C31; // "WAL1"
const WAL_VERSION: u32 = 1;
const SEGMENT_HEADER_SIZE: usize = 32;

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

/// Current segment writer state
struct SegmentWriter {
    /// Segment ID
    segment_id: u64,
    /// File handle
    file: BufWriter<File>,
    /// File path
    path: PathBuf,
    /// Start LSN
    start_lsn: Lsn,
    /// Current byte offset
    offset: u64,
    /// Entry count
    entry_count: u64,
}

/// WAL Store - manages WAL persistence and retrieval
///
/// Provides durable storage for WAL entries with segment-based organization.
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
    /// All entries (in-memory storage + disk)
    entries: Arc<RwLock<BTreeMap<Lsn, WalEntry>>>,
    /// Minimum retained LSN
    min_retained_lsn: Arc<AtomicU64>,
    /// Current segment writer
    writer: Arc<RwLock<Option<SegmentWriter>>>,
    /// Last checkpoint LSN
    checkpoint_lsn: Arc<AtomicU64>,
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
            writer: Arc::new(RwLock::new(None)),
            checkpoint_lsn: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Initialize the WAL store (load existing segments)
    pub async fn init(&self) -> Result<()> {
        // Create WAL directory if it doesn't exist
        if let Err(e) = fs::create_dir_all(&self.config.wal_dir) {
            tracing::warn!("Failed to create WAL directory: {}", e);
            // Continue anyway - might be in-memory mode
        }

        // Scan for existing segments
        let mut max_lsn: Lsn = 0;
        let mut max_segment_id: u64 = 0;
        let mut min_lsn: Lsn = u64::MAX;

        if let Ok(dir_entries) = fs::read_dir(&self.config.wal_dir) {
            for entry in dir_entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |ext| ext == "wal") {
                    if let Some(segment_info) = self.load_segment_metadata(&path).await {
                        tracing::info!(
                            "Loaded segment {}: LSN {} - {}, {} entries",
                            segment_info.segment_id,
                            segment_info.start_lsn,
                            segment_info.end_lsn,
                            segment_info.entry_count
                        );

                        if segment_info.end_lsn > max_lsn {
                            max_lsn = segment_info.end_lsn;
                        }
                        if segment_info.start_lsn < min_lsn {
                            min_lsn = segment_info.start_lsn;
                        }
                        if segment_info.segment_id > max_segment_id {
                            max_segment_id = segment_info.segment_id;
                        }

                        // Load entries into memory for quick access
                        if let Err(e) = self.load_segment_entries(&path, &segment_info).await {
                            tracing::warn!("Failed to load segment entries: {}", e);
                        }

                        // Update LSN index
                        {
                            let mut index = self.lsn_index.write().await;
                            for lsn in segment_info.start_lsn..=segment_info.end_lsn {
                                index.insert(lsn, segment_info.segment_id);
                            }
                        }

                        // Store segment metadata
                        {
                            let mut segments = self.segments.write().await;
                            segments.insert(segment_info.segment_id, segment_info);
                        }
                    }
                }
            }
        }

        // Load checkpoint marker
        let checkpoint_path = self.config.wal_dir.join("checkpoint.dat");
        if let Ok(mut file) = File::open(&checkpoint_path) {
            let mut buf = [0u8; 8];
            if file.read_exact(&mut buf).is_ok() {
                let checkpoint = u64::from_le_bytes(buf);
                self.checkpoint_lsn.store(checkpoint, Ordering::SeqCst);
                tracing::info!("Loaded checkpoint LSN: {}", checkpoint);
            }
        }

        // Set current state
        self.current_lsn.store(max_lsn, Ordering::SeqCst);
        self.current_segment.store(max_segment_id, Ordering::SeqCst);
        if min_lsn != u64::MAX {
            self.min_retained_lsn.store(min_lsn, Ordering::SeqCst);
        }

        tracing::info!(
            "WAL store initialized at {:?}, current_lsn={}, segments={}",
            self.config.wal_dir,
            max_lsn,
            max_segment_id
        );

        Ok(())
    }

    /// Load segment metadata from file header
    async fn load_segment_metadata(&self, path: &PathBuf) -> Option<WalSegmentInfo> {
        let file = File::open(path).ok()?;
        let mut reader = BufReader::new(file);

        // Read header
        let mut header = [0u8; SEGMENT_HEADER_SIZE];
        reader.read_exact(&mut header).ok()?;

        // Parse header
        let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        if magic != WAL_MAGIC {
            tracing::warn!("Invalid WAL magic in {:?}", path);
            return None;
        }

        let _version = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        let segment_id = u64::from_le_bytes([
            header[8], header[9], header[10], header[11],
            header[12], header[13], header[14], header[15],
        ]);
        let start_lsn = u64::from_le_bytes([
            header[16], header[17], header[18], header[19],
            header[20], header[21], header[22], header[23],
        ]);
        let entry_count = u64::from_le_bytes([
            header[24], header[25], header[26], header[27],
            header[28], header[29], header[30], header[31],
        ]);

        // Scan to find end_lsn and actual entry count
        let mut actual_count = 0u64;
        let mut end_lsn = start_lsn;

        loop {
            // Read entry header: length (4) + type (1) + lsn (8) + checksum (4)
            let mut entry_header = [0u8; 17];
            if reader.read_exact(&mut entry_header).is_err() {
                break;
            }

            let length = u32::from_le_bytes([
                entry_header[0], entry_header[1], entry_header[2], entry_header[3],
            ]) as usize;
            let lsn = u64::from_le_bytes([
                entry_header[5], entry_header[6], entry_header[7], entry_header[8],
                entry_header[9], entry_header[10], entry_header[11], entry_header[12],
            ]);

            // Skip data
            if reader.seek(SeekFrom::Current(length as i64)).is_err() {
                break;
            }

            actual_count += 1;
            end_lsn = lsn;
        }

        let file_size = fs::metadata(path).ok()?.len();

        Some(WalSegmentInfo {
            segment_id,
            start_lsn,
            end_lsn,
            entry_count: if actual_count > 0 { actual_count } else { entry_count },
            size_bytes: file_size,
            is_complete: true, // Existing segments are complete
            path: path.clone(),
        })
    }

    /// Load segment entries into memory
    async fn load_segment_entries(&self, path: &PathBuf, info: &WalSegmentInfo) -> Result<()> {
        let file = File::open(path)
            .map_err(|e| ReplicationError::Storage(format!("Failed to open segment: {}", e)))?;
        let mut reader = BufReader::new(file);

        // Skip header
        reader.seek(SeekFrom::Start(SEGMENT_HEADER_SIZE as u64))
            .map_err(|e| ReplicationError::Storage(format!("Seek failed: {}", e)))?;

        let mut entries = self.entries.write().await;

        for _ in 0..info.entry_count {
            // Read entry header
            let mut entry_header = [0u8; 17];
            if reader.read_exact(&mut entry_header).is_err() {
                break;
            }

            let length = u32::from_le_bytes([
                entry_header[0], entry_header[1], entry_header[2], entry_header[3],
            ]) as usize;
            let entry_type = entry_header[4];
            let lsn = u64::from_le_bytes([
                entry_header[5], entry_header[6], entry_header[7], entry_header[8],
                entry_header[9], entry_header[10], entry_header[11], entry_header[12],
            ]);
            let checksum = u32::from_le_bytes([
                entry_header[13], entry_header[14], entry_header[15], entry_header[16],
            ]);

            // Read data
            let mut data = vec![0u8; length];
            if reader.read_exact(&mut data).is_err() {
                break;
            }

            // Verify checksum
            let computed_checksum = crc32fast::hash(&data);
            if computed_checksum != checksum {
                tracing::warn!("Checksum mismatch for LSN {}: expected {}, got {}", lsn, checksum, computed_checksum);
                continue;
            }

            let entry = WalEntry {
                lsn,
                tx_id: None, // tx_id not stored in segment format v1
                entry_type: Self::u8_to_entry_type(entry_type),
                data,
                checksum,
            };

            entries.insert(lsn, entry);
        }

        Ok(())
    }

    /// Convert u8 to WalEntryType
    fn u8_to_entry_type(value: u8) -> WalEntryType {
        match value {
            0 => WalEntryType::Insert,
            1 => WalEntryType::Update,
            2 => WalEntryType::Delete,
            3 => WalEntryType::TxBegin,
            4 => WalEntryType::TxCommit,
            5 => WalEntryType::TxRollback,
            6 => WalEntryType::Checkpoint,
            7 => WalEntryType::SchemaChange,
            8 => WalEntryType::BranchOp,
            _ => WalEntryType::Insert,
        }
    }

    /// Convert WalEntryType to u8
    fn entry_type_to_u8(entry_type: WalEntryType) -> u8 {
        match entry_type {
            WalEntryType::Insert => 0,
            WalEntryType::Update => 1,
            WalEntryType::Delete => 2,
            WalEntryType::TxBegin => 3,
            WalEntryType::TxCommit => 4,
            WalEntryType::TxRollback => 5,
            WalEntryType::Checkpoint => 6,
            WalEntryType::SchemaChange => 7,
            WalEntryType::BranchOp => 8,
        }
    }

    /// Create a new segment file
    async fn create_segment(&self, segment_id: u64, start_lsn: Lsn) -> Result<SegmentWriter> {
        let filename = format!("segment_{:06}.wal", segment_id);
        let path = self.config.wal_dir.join(&filename);

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| ReplicationError::Storage(format!("Failed to create segment: {}", e)))?;

        let mut writer = BufWriter::new(file);

        // Write header
        let mut header = [0u8; SEGMENT_HEADER_SIZE];
        header[0..4].copy_from_slice(&WAL_MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&WAL_VERSION.to_le_bytes());
        header[8..16].copy_from_slice(&segment_id.to_le_bytes());
        header[16..24].copy_from_slice(&start_lsn.to_le_bytes());
        // entry_count will be updated on close

        writer.write_all(&header)
            .map_err(|e| ReplicationError::Storage(format!("Failed to write header: {}", e)))?;

        if self.config.fsync_on_write {
            writer.flush()
                .map_err(|e| ReplicationError::Storage(format!("Flush failed: {}", e)))?;
        }

        tracing::info!("Created new segment {} at {:?}", segment_id, path);

        Ok(SegmentWriter {
            segment_id,
            file: writer,
            path,
            start_lsn,
            offset: SEGMENT_HEADER_SIZE as u64,
            entry_count: 0,
        })
    }

    /// Write entry to disk
    async fn write_entry_to_disk(&self, entry: &WalEntry) -> Result<()> {
        let mut writer_guard = self.writer.write().await;

        // Check if we need to rotate segment
        let needs_new_segment = match &*writer_guard {
            None => true,
            Some(w) => {
                w.entry_count >= self.config.max_entries_per_segment as u64 ||
                w.offset >= self.config.max_segment_size as u64
            }
        };

        if needs_new_segment {
            // Close current segment if exists
            if let Some(mut old_writer) = writer_guard.take() {
                self.close_segment(&mut old_writer).await?;
            }

            // Create new segment
            let new_segment_id = self.current_segment.fetch_add(1, Ordering::SeqCst) + 1;
            let new_writer = self.create_segment(new_segment_id, entry.lsn).await?;
            *writer_guard = Some(new_writer);
        }

        // Write entry
        if let Some(ref mut writer) = *writer_guard {
            // Entry format: length (4) + type (1) + lsn (8) + checksum (4) + data
            let length = entry.data.len() as u32;
            let entry_type = Self::entry_type_to_u8(entry.entry_type);

            writer.file.write_all(&length.to_le_bytes())
                .map_err(|e| ReplicationError::Storage(format!("Write failed: {}", e)))?;
            writer.file.write_all(&[entry_type])
                .map_err(|e| ReplicationError::Storage(format!("Write failed: {}", e)))?;
            writer.file.write_all(&entry.lsn.to_le_bytes())
                .map_err(|e| ReplicationError::Storage(format!("Write failed: {}", e)))?;
            writer.file.write_all(&entry.checksum.to_le_bytes())
                .map_err(|e| ReplicationError::Storage(format!("Write failed: {}", e)))?;
            writer.file.write_all(&entry.data)
                .map_err(|e| ReplicationError::Storage(format!("Write failed: {}", e)))?;

            if self.config.fsync_on_write {
                writer.file.flush()
                    .map_err(|e| ReplicationError::Storage(format!("Flush failed: {}", e)))?;
            }

            writer.offset += 17 + entry.data.len() as u64;
            writer.entry_count += 1;

            // Update LSN index
            {
                let mut index = self.lsn_index.write().await;
                index.insert(entry.lsn, writer.segment_id);
            }
        }

        Ok(())
    }

    /// Close and finalize a segment
    async fn close_segment(&self, writer: &mut SegmentWriter) -> Result<()> {
        // Flush remaining data
        writer.file.flush()
            .map_err(|e| ReplicationError::Storage(format!("Flush failed: {}", e)))?;

        // Update header with entry count
        let file = writer.file.get_mut();
        file.seek(SeekFrom::Start(24))
            .map_err(|e| ReplicationError::Storage(format!("Seek failed: {}", e)))?;
        file.write_all(&writer.entry_count.to_le_bytes())
            .map_err(|e| ReplicationError::Storage(format!("Write failed: {}", e)))?;
        file.sync_all()
            .map_err(|e| ReplicationError::Storage(format!("Sync failed: {}", e)))?;

        // Store segment metadata
        let segment_info = WalSegmentInfo {
            segment_id: writer.segment_id,
            start_lsn: writer.start_lsn,
            end_lsn: self.current_lsn.load(Ordering::SeqCst),
            entry_count: writer.entry_count,
            size_bytes: writer.offset,
            is_complete: true,
            path: writer.path.clone(),
        };

        {
            let mut segments = self.segments.write().await;
            segments.insert(writer.segment_id, segment_info);
        }

        tracing::info!(
            "Closed segment {} with {} entries",
            writer.segment_id,
            writer.entry_count
        );

        Ok(())
    }

    /// Append a WAL entry
    pub async fn append(&self, entry: WalEntry) -> Result<Lsn> {
        let lsn = entry.lsn;

        // Store in entries map (in-memory)
        {
            let mut entries = self.entries.write().await;
            entries.insert(lsn, entry.clone());
        }

        // Add to cache
        {
            let mut cache = self.cache.write().await;
            cache.push_back(entry.clone());
            while cache.len() > self.config.cache_size {
                cache.pop_front();
            }
        }

        // Update current LSN
        self.current_lsn.store(lsn, Ordering::SeqCst);

        // Write to disk
        if let Err(e) = self.write_entry_to_disk(&entry).await {
            tracing::warn!("Failed to write entry to disk: {} (continuing with in-memory)", e);
        }

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

        // Clean up cache
        {
            let mut cache = self.cache.write().await;
            cache.retain(|e| e.lsn >= lsn);
        }

        // Clean up LSN index
        {
            let mut index = self.lsn_index.write().await;
            index.retain(|k, _| *k >= lsn);
        }

        // Clean up old segment files
        {
            let mut segments = self.segments.write().await;
            let old_segments: Vec<u64> = segments
                .iter()
                .filter(|(_, s)| s.end_lsn < lsn)
                .map(|(id, _)| *id)
                .collect();

            for seg_id in old_segments {
                if let Some(seg) = segments.remove(&seg_id) {
                    if let Err(e) = fs::remove_file(&seg.path) {
                        tracing::warn!("Failed to remove old segment file: {}", e);
                    } else {
                        tracing::info!("Removed old segment {} at {:?}", seg_id, seg.path);
                    }
                }
            }
        }

        tracing::info!("Truncated {} entries before LSN {}", count, lsn);
        Ok(count)
    }

    /// Create a checkpoint (flush and mark a safe point)
    pub async fn checkpoint(&self) -> Result<Lsn> {
        let checkpoint_lsn = self.current_lsn.load(Ordering::SeqCst);

        // Flush current segment
        {
            let mut writer_guard = self.writer.write().await;
            if let Some(ref mut writer) = *writer_guard {
                writer.file.flush()
                    .map_err(|e| ReplicationError::Storage(format!("Flush failed: {}", e)))?;
                if let Ok(file) = writer.file.get_mut().try_clone() {
                    let _ = file.sync_all();
                }
            }
        }

        // Write checkpoint marker
        let checkpoint_path = self.config.wal_dir.join("checkpoint.dat");
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&checkpoint_path)
        {
            if file.write_all(&checkpoint_lsn.to_le_bytes()).is_ok() {
                let _ = file.sync_all();
            }
        }

        self.checkpoint_lsn.store(checkpoint_lsn, Ordering::SeqCst);
        tracing::info!("WAL checkpoint at LSN {}", checkpoint_lsn);
        Ok(checkpoint_lsn)
    }

    /// Get last checkpoint LSN
    pub fn checkpoint_lsn(&self) -> Lsn {
        self.checkpoint_lsn.load(Ordering::SeqCst)
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
            checkpoint_lsn: self.checkpoint_lsn.load(Ordering::SeqCst),
        }
    }

    /// Close the WAL store
    pub async fn close(&self) -> Result<()> {
        // Close current segment
        {
            let mut writer_guard = self.writer.write().await;
            if let Some(mut writer) = writer_guard.take() {
                self.close_segment(&mut writer).await?;
            }
        }

        // Final checkpoint
        let _ = self.checkpoint().await;

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
    /// Last checkpoint LSN
    pub checkpoint_lsn: Lsn,
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
    use tempfile::tempdir;

    fn make_entry(lsn: Lsn, data: Vec<u8>) -> WalEntry {
        let checksum = crc32fast::hash(&data);
        WalEntry {
            lsn,
            tx_id: None,
            entry_type: WalEntryType::Insert,
            data,
            checksum,
        }
    }

    /// Create a test config with temp directory and fsync disabled
    fn test_config() -> (WalStoreConfig, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let config = WalStoreConfig {
            wal_dir: dir.path().to_path_buf(),
            fsync_on_write: false, // Disable for fast tests
            ..Default::default()
        };
        (config, dir)
    }

    #[tokio::test]
    async fn test_wal_store_creation() {
        let (config, _dir) = test_config();
        let store = WalStore::new(config);
        store.init().await.expect("init failed");
        assert_eq!(store.current_lsn(), 0);
    }

    #[tokio::test]
    async fn test_append_and_get() {
        let (config, _dir) = test_config();
        let store = WalStore::new(config);
        store.init().await.expect("init failed");

        let entry = make_entry(1, vec![1, 2, 3]);
        store.append(entry.clone()).await.expect("append failed");

        let retrieved = store.get(1).await.expect("entry not found");
        assert_eq!(retrieved.lsn, 1);
        assert_eq!(retrieved.data, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_get_batch() {
        let (config, _dir) = test_config();
        let store = WalStore::new(config);
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
        let (config, _dir) = test_config();
        let store = WalStore::new(config);
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
        let (config, _dir) = test_config();
        let store = WalStore::new(config);
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
        let (config, _dir) = test_config();
        let store = WalStore::new(config);
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
        let (config, _dir) = test_config();
        let store = WalStore::new(config);
        store.init().await.expect("init failed");

        for i in 1..=10 {
            let entry = make_entry(i, vec![i as u8]);
            store.append(entry).await.expect("append failed");
        }

        let stats = store.stats().await;
        assert_eq!(stats.current_lsn, 10);
        assert_eq!(stats.total_entries, 10);
    }

    #[tokio::test]
    async fn test_checkpoint() {
        let (config, _dir) = test_config();
        let store = WalStore::new(config);
        store.init().await.expect("init failed");

        for i in 1..=10 {
            let entry = make_entry(i, vec![i as u8]);
            store.append(entry).await.expect("append failed");
        }

        let checkpoint = store.checkpoint().await.expect("checkpoint failed");
        assert_eq!(checkpoint, 10);
        assert_eq!(store.checkpoint_lsn(), 10);
    }
}
