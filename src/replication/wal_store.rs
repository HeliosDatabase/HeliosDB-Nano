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
// Per-record header: length (4) | entry_type (1) | lsn (8) | checksum (4)
const RECORD_HEADER_SIZE: usize = 17;

// =============================================================================
// SEGMENT RECORD SCANNING
// =============================================================================

/// Why a segment scan stopped before reaching a clean end-of-file.
///
/// Every variant is a *normal*, expected outcome, not an error: a torn trailing
/// record is the ordinary state of a write-ahead log after any unclean shutdown,
/// which is the entire premise of a WAL (`docs/plans/ROADMAP_V5.md` §2.8). None of
/// these are reported as `Err` — only genuine I/O failure is.
#[derive(Debug, Clone, Copy)]
enum ScanStop {
    /// Fewer than `RECORD_HEADER_SIZE` bytes remained for a record header.
    HeaderTruncated,
    /// The record's `length` exceeds the bytes physically remaining in the file.
    TornTail { claimed: u64, remaining_in_file: u64 },
    /// The record's `length` fits inside the file but exceeds `max_entry_size`
    /// (`WalStoreConfig::max_segment_size`). Defence in depth for a large file whose
    /// length prefix is corrupt yet still smaller than the file itself.
    ExceedsMaxEntrySize { claimed: u64, max_entry_size: u64 },
    /// `max_entries_per_segment` records were already accepted and the file has more.
    /// Just as plausibly a lowered `max_entries_per_segment` as a hostile file, so
    /// the log message must not claim to know which.
    TooManyEntries { max_entries_per_segment: u64 },
    /// The payload was read in full but does not hash to the checksum its header
    /// promised, so no record boundary after it is provable (see trap 2 below).
    ChecksumMismatch { lsn: Lsn, expected: u32, computed: u32 },
}

/// Result of one pass over a segment's records.
struct ScanOutcome {
    /// Records that passed every check: length in bounds, count in bounds, checksum verified.
    accepted: u64,
    /// LSN of the last accepted record, or the segment's `start_lsn` if none were accepted.
    end_lsn: Lsn,
    /// Byte offset of the offending record and why, if the scan stopped before clean EOF.
    stopped_early: Option<(u64, ScanStop)>,
}

/// The one authoritative pass over a segment's records, running from the reader's
/// current position (immediately after the 32-byte segment header) to either a clean
/// EOF on a record boundary or the first record that cannot be trusted.
///
/// `load_segment_metadata` and `load_segment_entries` both go through here so they
/// cannot disagree about where a segment ends. They used to run two separate
/// hand-rolled loops under different rules — metadata skipped payloads with `seek`
/// and never verified a checksum, entries verified checksums but skipped past
/// failures — and that divergence is what let a single torn segment stop a primary
/// from restarting at all (`docs/plans/ROADMAP_V5.md` §2.8).
///
/// Two traps this function exists to close. Both read as harmless simplifications
/// and must not be "cleaned up" back into the bug:
///
/// 1. **Seeking past EOF succeeds.** `Seek` on a regular file is a pure position
///   update, not I/O, so `seek(SeekFrom::Current(length))` returns `Ok` for any
///   `length`, however absurd. A corrupt length therefore cannot be detected by
///   asking whether the skip failed; it has to be range-checked against the file's
///   real size *before* it is used for anything. That is what `file_len` is for.
///   The old metadata scan skipped this check, walked into a fabricated position,
///   and promoted a garbage LSN into the segment's reported `end_lsn`.
/// 2. **A failed record stops the scan; it is never skipped.** Once a record fails
///   we cannot distinguish payload corruption (length honest, stream still aligned)
///   from a lying length (stream now misaligned, so every "header" after it is
///   really payload noise). Continuing is safe only in the first case and nothing
///   at the point of failure says which case you are in — a misread noise header
///   can claim any length at all, which is exactly how a run of `0x78` filler came
///   to claim a 1.88 GiB record. So any failure keeps the validated prefix and
///   discards the rest of the file. `src/storage/wal.rs`'s replay path already
///   applies the same rule for the same reason.
///
/// Never allocates more than `min(bytes remaining in the file, max_entry_size)` for
/// a single record, and never allocates at all for a record it rejects.
///
/// `on_accept` receives `(lsn, entry_type, checksum, payload)` for every record that
/// passes every check, in file order.
fn scan_segment_records<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    start_lsn: Lsn,
    max_entry_size: u64,
    max_entries_per_segment: u64,
    mut on_accept: impl FnMut(Lsn, u8, u32, Vec<u8>),
) -> std::io::Result<ScanOutcome> {
    let mut outcome = ScanOutcome {
        accepted: 0,
        end_lsn: start_lsn,
        stopped_early: None,
    };

    // Tracked arithmetically rather than re-queried per record: on a `BufReader`
    // every `stream_position()` is an `lseek` syscall, and `read_exact` consumes
    // exactly the bytes it was asked for whenever it succeeds, so this stays exact.
    let mut position = reader.stream_position()?;

    loop {
        // A healthy, cleanly-closed segment always terminates here: its final record
        // ends exactly at EOF, so every record is accepted exactly as it was before
        // this function existed.
        if position >= file_len {
            break;
        }

        if outcome.accepted >= max_entries_per_segment {
            let reason = ScanStop::TooManyEntries {
                max_entries_per_segment,
            };
            outcome.stopped_early = Some((position, reason));
            break;
        }

        let mut entry_header = [0u8; RECORD_HEADER_SIZE];
        match reader.read_exact(&mut entry_header) {
            Ok(()) => {}
            // Ran out of bytes mid-header: an ordinary torn tail, not a failure.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                outcome.stopped_early = Some((position, ScanStop::HeaderTruncated));
                break;
            }
            // A real I/O problem (bad disk, permissions) is a different thing entirely
            // and must not be silently reported as a torn segment.
            Err(e) => return Err(e),
        }

        let length = u32::from_le_bytes([entry_header[0], entry_header[1], entry_header[2], entry_header[3]]);
        let entry_type = entry_header[4];
        let lsn = u64::from_le_bytes([
            entry_header[5],
            entry_header[6],
            entry_header[7],
            entry_header[8],
            entry_header[9],
            entry_header[10],
            entry_header[11],
            entry_header[12],
        ]);
        let checksum = u32::from_le_bytes([entry_header[13], entry_header[14], entry_header[15], entry_header[16]]);

        // Bound the claimed length before it is used for anything at all (trap 1), so
        // the largest allocation this code can make for one record is `max_entry_size`.
        let claimed = u64::from(length);
        let payload_start = position + RECORD_HEADER_SIZE as u64;
        let remaining = file_len.saturating_sub(payload_start);
        if claimed > remaining {
            let reason = ScanStop::TornTail {
                claimed,
                remaining_in_file: remaining,
            };
            outcome.stopped_early = Some((position, reason));
            break;
        }
        if claimed > max_entry_size {
            let reason = ScanStop::ExceedsMaxEntrySize {
                claimed,
                max_entry_size,
            };
            outcome.stopped_early = Some((position, reason));
            break;
        }

        // Provably bounded by the two checks above.
        let mut data = vec![0u8; length as usize];
        match reader.read_exact(&mut data) {
            Ok(()) => {}
            // Only reachable if the file shrank between the size snapshot and this
            // read. Handled like any other torn tail rather than assumed impossible.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                outcome.stopped_early = Some((position, ScanStop::HeaderTruncated));
                break;
            }
            Err(e) => return Err(e),
        }

        let computed = crc32fast::hash(&data);
        if computed != checksum {
            let reason = ScanStop::ChecksumMismatch {
                lsn,
                expected: checksum,
                computed,
            };
            outcome.stopped_early = Some((position, reason));
            break;
        }

        outcome.accepted += 1;
        outcome.end_lsn = lsn;
        position = payload_start + claimed;
        on_accept(lsn, entry_type, checksum, data);
    }

    Ok(outcome)
}

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

                        // Update LSN index with exactly the LSNs that were recovered,
                        // which is what `write_entry_to_disk` already does for live
                        // appends. This used to backfill every value in
                        // `start_lsn..=end_lsn`, and *that* loop is what actually hung
                        // startup (ROADMAP_V5 §2.8): the old metadata scan trusted a
                        // corrupt record's LSN straight into `end_lsn`, so the range
                        // became ~8.7e18 `BTreeMap` inserts. The scan now guarantees
                        // `end_lsn` is a validated LSN, but deriving the index from the
                        // entries actually in memory keeps this loop bounded by
                        // `max_entries_per_segment` through *any* future path, and it
                        // never indexes an LSN the store cannot serve.
                        let recovered_lsns: Vec<Lsn> = {
                            let entries = self.entries.read().await;
                            entries
                                .range(segment_info.start_lsn..=segment_info.end_lsn)
                                .map(|(lsn, _)| *lsn)
                                .collect()
                        };
                        {
                            let mut index = self.lsn_index.write().await;
                            for lsn in recovered_lsns {
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
        // Taken from the open handle before the scan: the scan needs the file's real
        // size to range-check every record's length prefix against it.
        let file_size = file.metadata().ok()?.len();
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
            header[8], header[9], header[10], header[11], header[12], header[13], header[14], header[15],
        ]);
        let start_lsn = u64::from_le_bytes([
            header[16], header[17], header[18], header[19], header[20], header[21], header[22], header[23],
        ]);
        // Bytes 24..32 hold the header's `entry_count`, deliberately not read here.
        // `close_segment` is the only writer of that field, so it is still 0 in every
        // segment whose process died mid-write — precisely the segments this function
        // has to survive. The scan below is the sole source of truth for the count,
        // and the old `if actual_count > 0 { actual_count } else { entry_count }`
        // fallback to the raw header value is gone with it: "the scan found nothing"
        // and "report nothing" are the same fact.
        let scan = scan_segment_records(
            &mut reader,
            file_size,
            start_lsn,
            self.config.max_segment_size as u64,
            self.config.max_entries_per_segment as u64,
            |_, _, _, _| {},
        );
        // A real I/O error here (as opposed to a torn record, which the scan reports
        // as a normal outcome) means this segment cannot be read at all, so it is
        // skipped rather than half-trusted — but never silently.
        let outcome = match scan {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!("Failed to scan WAL segment {:?}: {}", path, e);
                return None;
            }
        };

        if let Some((offset, reason)) = outcome.stopped_early {
            // Debug, not warn: `load_segment_entries` scans the same file immediately
            // afterwards and emits the single operator-facing warning for this event.
            tracing::debug!(
                "Segment {:?} scan stopped at byte offset {} ({:?}) after {} valid records",
                path,
                offset,
                reason,
                outcome.accepted
            );
        }

        Some(WalSegmentInfo {
            segment_id,
            start_lsn,
            end_lsn: outcome.end_lsn,
            entry_count: outcome.accepted,
            size_bytes: file_size,
            is_complete: true, // Existing segments are complete
            path: path.clone(),
        })
    }

    /// Load segment entries into memory
    async fn load_segment_entries(&self, path: &PathBuf, info: &WalSegmentInfo) -> Result<()> {
        let file = File::open(path).map_err(|e| ReplicationError::Storage(format!("Failed to open segment: {}", e)))?;
        let file_size = file
            .metadata()
            .map_err(|e| ReplicationError::Storage(format!("Failed to stat segment: {}", e)))?
            .len();
        let mut reader = BufReader::new(file);

        // Skip header
        reader
            .seek(SeekFrom::Start(SEGMENT_HEADER_SIZE as u64))
            .map_err(|e| ReplicationError::Storage(format!("Seek failed: {}", e)))?;

        // `info.entry_count` deliberately does not bound this loop any more. It used
        // to (`for _ in 0..info.entry_count`), which made recovery depend on a count
        // that a mid-write crash never gets written. The scan is driven by file
        // position and stops itself (ROADMAP_V5 §2.8).
        let outcome = {
            let mut entries = self.entries.write().await;
            let scan = scan_segment_records(
                &mut reader,
                file_size,
                info.start_lsn,
                self.config.max_segment_size as u64,
                self.config.max_entries_per_segment as u64,
                |lsn, entry_type, checksum, data| {
                    let entry = WalEntry {
                        lsn,
                        tx_id: None, // tx_id not stored in segment format v1
                        entry_type: Self::u8_to_entry_type(entry_type),
                        data,
                        checksum,
                    };
                    entries.insert(lsn, entry);
                },
            );
            scan.map_err(|e| ReplicationError::Storage(format!("Failed to read segment: {}", e)))?
        };

        if let Some((offset, reason)) = outcome.stopped_early {
            // The operator-facing signal ROADMAP_V5 §2.8 asks for: name the file and
            // the offset. Still `Ok(())` — a torn tail is expected WAL behaviour, not
            // a failure of this function, and `Err` stays reserved for real I/O errors.
            // The segment itself is left on disk exactly as found: truncating it here
            // would destroy the only forensic record of how it was torn, and would be
            // a silent side effect of every startup rather than a deliberate act.
            tracing::warn!(
                "Torn WAL segment {:?}: scan stopped at byte offset {} ({:?}); recovered {} entries up to LSN {}. \
                 Segment left on disk unmodified.",
                path,
                offset,
                reason,
                outcome.accepted,
                outcome.end_lsn
            );
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

        writer
            .write_all(&header)
            .map_err(|e| ReplicationError::Storage(format!("Failed to write header: {}", e)))?;

        if self.config.fsync_on_write {
            writer
                .flush()
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
                w.entry_count >= self.config.max_entries_per_segment as u64
                    || w.offset >= self.config.max_segment_size as u64
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

            writer
                .file
                .write_all(&length.to_le_bytes())
                .map_err(|e| ReplicationError::Storage(format!("Write failed: {}", e)))?;
            writer
                .file
                .write_all(&[entry_type])
                .map_err(|e| ReplicationError::Storage(format!("Write failed: {}", e)))?;
            writer
                .file
                .write_all(&entry.lsn.to_le_bytes())
                .map_err(|e| ReplicationError::Storage(format!("Write failed: {}", e)))?;
            writer
                .file
                .write_all(&entry.checksum.to_le_bytes())
                .map_err(|e| ReplicationError::Storage(format!("Write failed: {}", e)))?;
            writer
                .file
                .write_all(&entry.data)
                .map_err(|e| ReplicationError::Storage(format!("Write failed: {}", e)))?;

            if self.config.fsync_on_write {
                writer
                    .file
                    .flush()
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
        writer
            .file
            .flush()
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
            has_more = entries
                .range((
                    std::ops::Bound::Excluded(actual_end_lsn),
                    std::ops::Bound::Included(end_lsn),
                ))
                .next()
                .is_some();
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
        entries.range(start_lsn..=end_lsn).map(|(_, e)| e.clone()).collect()
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
                writer
                    .file
                    .flush()
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
            if total > 0.0 {
                done / total * 100.0
            } else {
                100.0
            }
        })
    }
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
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

    /// Create a config rooted at an explicit temp directory, with fsync disabled.
    ///
    /// Never use `WalStoreConfig::default()` unoverridden in a test: its `wal_dir` is
    /// the CWD-relative `./data/wal` (ROADMAP_V5 §2.7, still open), so a test run from
    /// the repository root would read and write the repository's own `data/` tree.
    fn config_for(dir: &tempfile::TempDir) -> WalStoreConfig {
        WalStoreConfig {
            wal_dir: dir.path().to_path_buf(),
            fsync_on_write: false, // Disable for fast tests
            ..Default::default()
        }
    }

    /// Create a test config with temp directory and fsync disabled
    fn test_config() -> (WalStoreConfig, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let config = config_for(&dir);
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

    // -------------------------------------------------------------------------
    // Torn / corrupt segment recovery (ROADMAP_V5 §2.8)
    //
    // A torn trailing record is the normal state of a WAL after any unclean
    // shutdown, so every case below is a state a primary must restart from.
    // -------------------------------------------------------------------------

    /// Hard wall-clock deadline for `init()` in the recovery tests below.
    const INIT_DEADLINE: Duration = Duration::from_secs(10);

    /// Run `WalStore::init()` under a hard deadline and assert it succeeded.
    ///
    /// `tokio::time::timeout(d, store.init())` on its own would NOT work here. The
    /// regression being guarded against is a synchronous loop inside a single `poll()`
    /// — the `lsn_index` backfill over a garbage `end_lsn` — which never yields, so a
    /// timer future sharing its thread would never be polled and the test would hang
    /// anyway. Driving `init()` as its own task on a multi-threaded runtime is what
    /// makes the deadline real, and it needs to be real: this bug hung the repository's
    /// test suite for two days.
    async fn init_within_deadline(store: &Arc<WalStore>) {
        let store = Arc::clone(store);
        let handle = tokio::spawn(async move { store.init().await });
        tokio::time::timeout(INIT_DEADLINE, handle)
            .await
            .expect("WalStore::init() exceeded its deadline - the segment scan is unbounded again")
            .expect("WalStore::init() task panicked")
            .expect("WalStore::init() failed");
    }

    /// A segment header byte-identical to `create_segment`'s, with `entry_count` set
    /// to whatever `close_segment` would have backpatched at offset 24.
    fn segment_header(segment_id: u64, start_lsn: Lsn, entry_count: u64) -> Vec<u8> {
        let mut header = vec![0u8; SEGMENT_HEADER_SIZE];
        header[0..4].copy_from_slice(&WAL_MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&WAL_VERSION.to_le_bytes());
        header[8..16].copy_from_slice(&segment_id.to_le_bytes());
        header[16..24].copy_from_slice(&start_lsn.to_le_bytes());
        header[24..32].copy_from_slice(&entry_count.to_le_bytes());
        header
    }

    /// One record in the layout `write_entry_to_disk` produces. `claimed_length` and
    /// `checksum` are separate arguments so a test can lie about either one.
    fn record_bytes(lsn: Lsn, data: &[u8], claimed_length: u32, checksum: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(RECORD_HEADER_SIZE + data.len());
        out.extend_from_slice(&claimed_length.to_le_bytes());
        out.push(0); // WalEntryType::Insert
        out.extend_from_slice(&lsn.to_le_bytes());
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    /// A well-formed record whose stored checksum matches its payload.
    fn valid_record(lsn: Lsn, data: &[u8]) -> Vec<u8> {
        record_bytes(lsn, data, data.len() as u32, crc32fast::hash(data))
    }

    /// Write raw bytes as `segment_<id>.wal` inside `dir`.
    fn write_segment_file(dir: &std::path::Path, segment_id: u64, bytes: &[u8]) {
        let path = dir.join(format!("segment_{:06}.wal", segment_id));
        fs::write(path, bytes).expect("failed to write test segment");
    }

    /// Every entry currently held by the store, in LSN order.
    async fn recovered_entries(store: &WalStore) -> Vec<WalEntry> {
        store.get_range(0, u64::MAX).await
    }

    /// Case 1: a healthy, cleanly-closed segment must load exactly as it always did.
    /// This is the regression guard for the "byte-identical for healthy segments"
    /// requirement — the new bounds and checksum enforcement may only ever reject
    /// records that were already going to fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valid_segment_loads_all_records() {
        let dir = tempdir().expect("tempdir");
        let mut bytes = segment_header(1, 1, 4);
        for lsn in 1..=4u64 {
            bytes.extend_from_slice(&valid_record(lsn, format!("payload-{}", lsn).as_bytes()));
        }
        write_segment_file(dir.path(), 1, &bytes);

        let store = Arc::new(WalStore::new(config_for(&dir)));
        init_within_deadline(&store).await;

        let recovered = recovered_entries(&store).await;
        assert_eq!(recovered.len(), 4, "a healthy segment must load every record");
        for (idx, entry) in recovered.iter().enumerate() {
            let lsn = idx as u64 + 1;
            assert_eq!(entry.lsn, lsn, "records must be recovered in file order");
            assert_eq!(entry.data, format!("payload-{}", lsn).into_bytes());
            assert_eq!(entry.checksum, crc32fast::hash(&entry.data));
            assert_eq!(entry.entry_type, WalEntryType::Insert);
        }

        assert_eq!(store.current_lsn(), 4);
        let segments = store.list_segments().await;
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start_lsn, 1);
        assert_eq!(segments[0].end_lsn, 4);
        assert_eq!(segments[0].entry_count, 4);
    }

    /// Case 3: killed between two writes, leaving a partial record header.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn header_truncated_mid_record() {
        let dir = tempdir().expect("tempdir");
        let mut bytes = segment_header(1, 1, 0);
        bytes.extend_from_slice(&valid_record(1, b"first"));
        // Nine bytes: fewer than one RECORD_HEADER_SIZE.
        bytes.extend_from_slice(&[0xAB; 9]);
        write_segment_file(dir.path(), 1, &bytes);

        let store = Arc::new(WalStore::new(config_for(&dir)));
        init_within_deadline(&store).await;

        assert_eq!(recovered_entries(&store).await.len(), 1);
        assert!(store.get(1).await.is_some());
        assert_eq!(store.current_lsn(), 1);
        assert_eq!(store.list_segments().await[0].entry_count, 1);
    }

    /// Case 4: killed mid-payload, so the length prefix outruns the file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn payload_truncated_mid_payload() {
        let dir = tempdir().expect("tempdir");
        let mut bytes = segment_header(1, 1, 0);
        bytes.extend_from_slice(&valid_record(1, b"first"));
        // Claims 4096 payload bytes; only 100 ever made it to disk.
        bytes.extend_from_slice(&record_bytes(2, &[0x5A; 100], 4096, 0));
        write_segment_file(dir.path(), 1, &bytes);

        let store = Arc::new(WalStore::new(config_for(&dir)));
        init_within_deadline(&store).await;

        assert_eq!(recovered_entries(&store).await.len(), 1);
        assert!(
            store.get(2).await.is_none(),
            "a record that outruns the file is not data"
        );
        assert_eq!(store.current_lsn(), 1);
    }

    /// Case 5: the allocation bomb, isolated from the checksum path. Before the
    /// remaining-bytes bound this length reached `vec![0u8; length]` directly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn absurd_length_field_rejected_without_allocating() {
        let dir = tempdir().expect("tempdir");
        let mut bytes = segment_header(1, 1, 0);
        bytes.extend_from_slice(&valid_record(1, b"first"));
        bytes.extend_from_slice(&record_bytes(2, &[0x5A; 32], 2_000_000_000, 0));
        assert!(bytes.len() < 1024, "the file must be tiny next to the claimed length");
        write_segment_file(dir.path(), 1, &bytes);

        let store = Arc::new(WalStore::new(config_for(&dir)));
        init_within_deadline(&store).await;

        assert_eq!(recovered_entries(&store).await.len(), 1);
        assert!(store.get(2).await.is_none());
        assert_eq!(store.current_lsn(), 1);
    }

    /// Case 6: the case the old `continue`-past-a-mismatch reader got wrong. The
    /// records after the bad one are individually perfect and must still be dropped:
    /// once a record fails, the stream's alignment is no longer provable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checksum_mismatch_stops_scan_does_not_continue() {
        let dir = tempdir().expect("tempdir");
        let mut bytes = segment_header(1, 1, 0);
        bytes.extend_from_slice(&valid_record(1, b"one"));
        bytes.extend_from_slice(&valid_record(2, b"two"));
        // Correct length, correct LSN, deliberately wrong checksum.
        let corrupt = b"three";
        let bad_checksum = crc32fast::hash(corrupt) ^ 0xFF;
        bytes.extend_from_slice(&record_bytes(3, corrupt, corrupt.len() as u32, bad_checksum));
        bytes.extend_from_slice(&valid_record(4, b"four"));
        bytes.extend_from_slice(&valid_record(5, b"five"));
        write_segment_file(dir.path(), 1, &bytes);

        let store = Arc::new(WalStore::new(config_for(&dir)));
        init_within_deadline(&store).await;

        assert_eq!(recovered_entries(&store).await.len(), 2);
        assert!(store.get(1).await.is_some());
        assert!(store.get(2).await.is_some());
        for lsn in 3..=5u64 {
            assert!(
                store.get(lsn).await.is_none(),
                "LSN {} follows a checksum failure and must be discarded",
                lsn
            );
        }
        assert_eq!(store.current_lsn(), 2);
        assert_eq!(store.list_segments().await[0].entry_count, 2);
    }

    /// Case 7: a zero-length segment file must be skipped without stopping recovery
    /// of the segments beside it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_length_file() {
        let dir = tempdir().expect("tempdir");
        write_segment_file(dir.path(), 1, &[]);
        let mut healthy = segment_header(2, 10, 2);
        healthy.extend_from_slice(&valid_record(10, b"ten"));
        healthy.extend_from_slice(&valid_record(11, b"eleven"));
        write_segment_file(dir.path(), 2, &healthy);

        let store = Arc::new(WalStore::new(config_for(&dir)));
        let empty_path = dir.path().join("segment_000001.wal");
        assert!(
            store.load_segment_metadata(&empty_path).await.is_none(),
            "an empty file cannot even yield a segment header"
        );

        init_within_deadline(&store).await;

        assert_eq!(recovered_entries(&store).await.len(), 2);
        assert_eq!(store.current_lsn(), 11);
        assert_eq!(store.list_segments().await.len(), 1);
    }

    /// Case 8: a file whose magic does not match is not a WAL segment at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn garbage_magic_rejected() {
        let dir = tempdir().expect("tempdir");
        let mut bytes = segment_header(1, 1, 2);
        bytes[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        bytes.extend_from_slice(&valid_record(1, b"one"));
        bytes.extend_from_slice(&valid_record(2, b"two"));
        write_segment_file(dir.path(), 1, &bytes);

        let store = Arc::new(WalStore::new(config_for(&dir)));
        let path = dir.path().join("segment_000001.wal");
        assert!(store.load_segment_metadata(&path).await.is_none());

        init_within_deadline(&store).await;

        assert!(recovered_entries(&store).await.is_empty());
        assert!(store.list_segments().await.is_empty());
        assert_eq!(store.current_lsn(), 0);
    }

    /// Case 9: the header's `entry_count` is untrusted input. Recovery is driven by
    /// file position, so a wildly inflated count changes nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn entry_count_header_lies_high_not_trusted() {
        let dir = tempdir().expect("tempdir");
        let mut bytes = segment_header(1, 1, 500_000);
        bytes.extend_from_slice(&valid_record(1, b"one"));
        bytes.extend_from_slice(&valid_record(2, b"two"));
        write_segment_file(dir.path(), 1, &bytes);

        let store = Arc::new(WalStore::new(config_for(&dir)));
        init_within_deadline(&store).await;

        assert_eq!(recovered_entries(&store).await.len(), 2);
        assert!(store.get(3).await.is_none());
        assert_eq!(store.current_lsn(), 2);
        assert_eq!(store.list_segments().await[0].entry_count, 2);
    }

    /// Case 10: `max_entries_per_segment` is enforced on the read path too, not only
    /// as a write-time rotation trigger. Without it, a file of many small, correctly
    /// checksummed records would drive an unbounded number of iterations.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn more_records_than_max_entries_per_segment_rejected() {
        let dir = tempdir().expect("tempdir");
        let mut bytes = segment_header(1, 1, 5);
        for lsn in 1..=5u64 {
            bytes.extend_from_slice(&valid_record(lsn, b"payload"));
        }
        write_segment_file(dir.path(), 1, &bytes);

        let config = WalStoreConfig {
            max_entries_per_segment: 3,
            ..config_for(&dir)
        };
        let store = Arc::new(WalStore::new(config));
        init_within_deadline(&store).await;

        let recovered = recovered_entries(&store).await;
        assert_eq!(recovered.len(), 3, "the read path enforces the count ceiling");
        assert!(store.get(4).await.is_none());
        assert_eq!(store.current_lsn(), 3);
    }

    /// Case 11: `max_segment_size` does real work independently of the
    /// remaining-bytes bound. The oversized record here fits comfortably inside its
    /// file, so only the configured ceiling can be what rejects it — proven by
    /// loading the very same bytes again under the default ceiling.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_single_record_rejected_by_max_segment_size() {
        let mut bytes = segment_header(1, 1, 2);
        bytes.extend_from_slice(&valid_record(1, b"small-payload"));
        bytes.extend_from_slice(&valid_record(2, &[b'z'; 200]));

        let capped_dir = tempdir().expect("tempdir");
        write_segment_file(capped_dir.path(), 1, &bytes);
        let capped_config = WalStoreConfig {
            max_segment_size: 100,
            ..config_for(&capped_dir)
        };
        let capped = Arc::new(WalStore::new(capped_config));
        init_within_deadline(&capped).await;
        assert_eq!(recovered_entries(&capped).await.len(), 1);
        assert!(capped.get(2).await.is_none(), "200 bytes exceeds a 100-byte ceiling");

        let open_dir = tempdir().expect("tempdir");
        write_segment_file(open_dir.path(), 1, &bytes);
        let open = Arc::new(WalStore::new(config_for(&open_dir)));
        init_within_deadline(&open).await;
        assert_eq!(
            recovered_entries(&open).await.len(),
            2,
            "the same bytes are structurally fine; only the ceiling rejected record 2"
        );
    }
}
