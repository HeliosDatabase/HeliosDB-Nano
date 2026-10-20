//! Write-Ahead Log (WAL) implementation
//!
//! Provides durability guarantees through write-ahead logging.
//! Uses RocksDB's WriteBatch for atomic operations and built-in WAL support.

use super::tde;
use super::write_gate::WriteGate;
use crate::crypto::KeyManager;
use crate::{Error, Result};
use parking_lot::Mutex;
use rocksdb::{WriteBatch, WriteOptions, DB};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};

// Import HA state for replication broadcast (when ha-tier1 is enabled)
#[cfg(feature = "ha-tier1")]
use crate::replication::ha_state::ha_state;

/// Write-Ahead Log operation types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WalOperation {
    // === DML Operations ===
    /// Insert a tuple into a table
    Insert {
        table: String,
        key: Vec<u8>,
        tuple: Vec<u8>,
    },
    /// Update a tuple in a table
    Update {
        table: String,
        key: Vec<u8>,
        tuple: Vec<u8>,
    },
    /// Delete a tuple from a table
    Delete { table: String, key: Vec<u8> },
    /// Truncate all rows from a table
    Truncate { table: String },

    // === Table DDL ===
    /// Create a new table
    CreateTable { table: String, schema: Vec<u8> },
    /// Drop a table
    DropTable { table: String },
    /// Alter table column storage mode
    AlterColumnStorage {
        table: String,
        column: String,
        /// Serialized ColumnStorageMode
        storage_mode: Vec<u8>,
    },

    // === Index Operations ===
    /// Create an index
    CreateIndex {
        name: String,
        table: String,
        column: String,
        index_type: Option<String>,
        /// Serialized index options
        options: Vec<u8>,
    },
    /// Drop an index
    DropIndex { name: String },

    // === Trigger Operations ===
    /// Create a trigger
    CreateTrigger {
        name: String,
        table: String,
        /// Serialized trigger definition
        definition: Vec<u8>,
    },
    /// Drop a trigger
    DropTrigger { name: String, table: Option<String> },

    // === Function/Procedure Operations ===
    /// Create a function
    CreateFunction {
        name: String,
        /// Serialized function definition
        definition: Vec<u8>,
    },
    /// Drop a function
    DropFunction { name: String },
    /// Create a procedure
    CreateProcedure {
        name: String,
        /// Serialized procedure definition
        definition: Vec<u8>,
    },
    /// Drop a procedure
    DropProcedure { name: String },

    // === Materialized View Operations ===
    /// Create a materialized view
    CreateMaterializedView {
        name: String,
        /// Serialized view definition
        definition: Vec<u8>,
    },
    /// Drop a materialized view
    DropMaterializedView { name: String },
    /// Refresh a materialized view
    RefreshMaterializedView {
        name: String,
        concurrent: bool,
        incremental: bool,
    },

    // === Constraint Operations ===
    /// Add a constraint to a table
    AddConstraint {
        table: String,
        /// Serialized constraint definition
        constraint: Vec<u8>,
    },
    /// Drop a constraint from a table
    DropConstraint { table: String, constraint_name: String },

    // === Transaction Control ===
    /// Transaction begin
    Begin { tx_id: u64 },
    /// Transaction commit
    Commit { tx_id: u64 },
    /// Transaction abort
    Abort { tx_id: u64 },

    // === HA Replication Operations ===
    /// Update a sequence counter (for HA replication)
    /// Ensures auto-increment values are preserved across failover
    UpdateCounter { table_name: String, new_value: u64 },

    /// Rename a table (ALTER TABLE … RENAME TO). Belongs with the Table-DDL
    /// section above, but bincode encodes the variant *index*, so new
    /// variants must be appended at the END to keep existing on-disk WALs
    /// decodable. Without this entry, replay re-applies CreateTable+Inserts
    /// for the old name and resurrects the renamed-away table.
    RenameTable { old_table: String, new_table: String },

    // === GH#36: the remaining schema-changing DDL =========================
    // APPENDED AT THE END for the same bincode-discriminant reason as
    // `RenameTable` above: inserting a variant mid-enum silently reinterprets
    // every retained entry in every existing store.
    /// A full-schema replacement from `ALTER TABLE … ADD/DROP/RENAME COLUMN`
    /// or `ALTER COLUMN … DROP NOT NULL`. The blob is the complete new
    /// `Schema` (bincode), not a delta, so replay is idempotent.
    AlterTableSchema { table: String, schema: Vec<u8> },
    /// A plain view definition (`ViewMetadata`, bincode).
    CreateView { name: String, definition: Vec<u8> },
    /// Drop a plain view.
    DropView { name: String },
    /// A sequence definition, exactly the tagged frame `Catalog::save_sequence`
    /// persists (`SEQ_DEF_MAGIC` + version + bincode).
    CreateSequence { name: String, definition: Vec<u8> },
    /// An altered sequence definition (same tagged frame).
    AlterSequence { name: String, definition: Vec<u8> },
    /// Drop a sequence (definition + durable state).
    DropSequence { name: String },
}

/// WAL entry with metadata
///
/// Format:
/// - LSN (Log Sequence Number): Monotonically increasing ID
/// - Timestamp: Unix timestamp in microseconds
/// - Operation: The actual operation to log
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalEntry {
    /// Log Sequence Number (LSN) - unique, monotonic
    pub lsn: u64,
    /// Timestamp in microseconds
    pub timestamp: u64,
    /// Operation to log
    pub operation: WalOperation,
}

impl WalEntry {
    /// Create a new WAL entry
    pub fn new(lsn: u64, operation: WalOperation) -> Self {
        // Get current timestamp - if system clock is before UNIX_EPOCH (extremely unlikely),
        // we fall back to 0 to ensure we can still create the entry
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);

        Self {
            lsn,
            timestamp,
            operation,
        }
    }

    /// Serialize entry to bytes
    pub fn serialize(&self) -> Result<Vec<u8>> {
        bincode::serialize(self).map_err(|e| Error::storage(format!("Failed to serialize WAL entry: {}", e)))
    }

    /// Deserialize entry from bytes
    pub fn deserialize(data: &[u8]) -> Result<Self> {
        bincode::deserialize(data).map_err(|e| Error::storage(format!("Failed to deserialize WAL entry: {}", e)))
    }
}

// ==================== `wal:entries:` value codec ====================
//
// An `Insert`/`Update` operation carries the row tuple itself, so a WAL entry
// is a second full image of a row, living in the same RocksDB store as the
// `data:` key it describes. Its stored form therefore follows the same rule
// every other row keyspace does (`src/storage/tde.rs`), keyed on the storage
// key rather than on which function wrote it.
//
// These two functions are the ONLY place `wal:entries:` bytes cross the
// storage boundary in either direction: every `append*` path seals through
// `stored_entry_bytes`, and every reader — crash replay, integrity
// verification, truncation, retention cleanup, metrics — opens through
// `decode_entry`. Free functions rather than methods because the group-commit
// thread builds its batch off the `WriteAheadLog`.
//
// The HA/CDC broadcast path is deliberately untouched: `broadcast_after_append`
// and `broadcast_after_batch` are handed the in-memory `&WalOperation`, not the
// stored bytes, so a standby receives exactly what it received before and seals
// what it applies under its OWN key on its own write path. What one node has on
// disk is not what another node should have on disk.

/// Seal a WAL entry into the bytes stored at `wal:entries:{lsn}`.
///
/// `Vec` in, `Vec` out: with encryption disabled `tde::seal_owned` hands back
/// the serialized buffer UNMOVED, so the default configuration pays one
/// `Option` check and no copy.
fn stored_entry_bytes(key_manager: Option<&KeyManager>, entry: &WalEntry) -> Result<Vec<u8>> {
    tde::seal_owned(key_manager, entry.serialize()?)
}

/// Open the bytes stored at `wal:entries:{lsn}` back into an entry.
///
/// Goes through the same tolerant rule as every other stored value, so a WAL
/// written before this keyspace was sealed still replays: an entry that is not
/// ciphertext under the configured key is deserialized as it stands.
fn decode_entry(key_manager: Option<&KeyManager>, storage_key: &[u8], raw: &[u8]) -> Result<WalEntry> {
    WalEntry::deserialize(&tde::open_ref(key_manager, storage_key, raw)?)
}

/// WAL synchronization mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalSyncMode {
    /// Synchronous - fsync on every write (safest, slowest)
    Sync,
    /// Asynchronous - OS-managed flush (faster, less safe)
    Async,
    /// Group commit - batch multiple operations (balanced)
    GroupCommit,
}

/// Pending write for group commit
struct PendingWrite {
    entry: WalEntry,
    result_tx: crossbeam::channel::Sender<Result<u64>>,
}

/// WAL integrity report
#[derive(Debug, Clone)]
pub struct WalIntegrityReport {
    /// Total entries scanned
    pub total_entries: u64,
    /// Corrupted entries found
    pub corrupted_entries: Vec<u64>,
    /// Missing LSN gaps detected
    pub missing_lsns: Vec<(u64, u64)>,
    /// Duplicate LSNs found
    pub duplicate_lsns: Vec<u64>,
    /// Timestamp ordering violations
    pub timestamp_violations: Vec<u64>,
    /// Overall integrity status
    pub is_valid: bool,
}

impl WalIntegrityReport {
    /// Create a new empty report
    fn new() -> Self {
        Self {
            total_entries: 0,
            corrupted_entries: Vec::new(),
            missing_lsns: Vec::new(),
            duplicate_lsns: Vec::new(),
            timestamp_violations: Vec::new(),
            is_valid: true,
        }
    }

    /// Mark report as invalid
    fn mark_invalid(&mut self) {
        self.is_valid = false;
    }
}

/// Replay statistics
#[derive(Debug, Clone)]
pub struct ReplayStats {
    /// Number of operations replayed
    pub operations_replayed: u64,
    /// Number of operations skipped (already applied)
    pub operations_skipped: u64,
    /// Number of errors encountered
    pub errors: u64,
    /// Replay start LSN
    pub start_lsn: u64,
    /// Replay end LSN
    pub end_lsn: u64,
    /// Replay duration in milliseconds
    pub duration_ms: u64,
}

/// Cleanup statistics
#[derive(Debug, Clone)]
pub struct CleanupStats {
    /// Number of entries deleted
    pub entries_deleted: u64,
    /// Bytes freed
    pub bytes_freed: u64,
    /// Oldest LSN remaining
    pub oldest_lsn: u64,
    /// Newest LSN
    pub newest_lsn: u64,
}

/// WAL metrics for monitoring
#[derive(Debug, Clone)]
pub struct WalMetrics {
    /// Current LSN
    pub current_lsn: u64,
    /// Total entries in WAL
    pub entry_count: u64,
    /// Estimated WAL size in bytes
    pub size_bytes: u64,
    /// Oldest entry LSN
    pub oldest_lsn: u64,
    /// Newest entry LSN
    pub newest_lsn: u64,
    /// Oldest entry timestamp
    pub oldest_timestamp: u64,
    /// Newest entry timestamp
    pub newest_timestamp: u64,
    /// WAL sync mode
    pub sync_mode: WalSyncMode,
}

/// One reserved LSN in the ordered publisher.
enum Slot {
    /// Allocated; its write has not resolved yet.
    Reserved,
    /// Durable (or about to be) and ready to be shown to standbys.
    Ready(WalOperation),
    /// Allocated but never persisted (failed write or abandoned): a hole.
    Skipped,
}

struct PublishState {
    /// Lowest LSN not yet resolved. Everything below it has been published or skipped.
    next: u64,
    slots: std::collections::BTreeMap<u64, Slot>,
}

/// Publishes logical WAL entries to standbys strictly in LSN order, and only after
/// the write that made them durable has succeeded.
///
/// Two committers can finish out of LSN order (an fsync stalls one of them). Showing
/// LSN 6 before LSN 5 lets a standby advance its cursor past 5 and then drop 5 as
/// "already applied". So an entry is published only when every lower LSN has resolved
/// (published or skipped), and `published_lsn` is the watermark below which the
/// persisted history is complete: catch-up must never read above it.
struct Publisher {
    state: Mutex<PublishState>,
    published: AtomicU64,
}

impl Publisher {
    fn new(last_lsn: u64) -> Self {
        Self {
            state: Mutex::new(PublishState {
                next: last_lsn + 1,
                slots: std::collections::BTreeMap::new(),
            }),
            published: AtomicU64::new(last_lsn),
        }
    }

    /// Allocate `count` contiguous LSNs and register them as unresolved. Allocation
    /// and registration happen under one lock so the publisher can never see a gap
    /// it does not know about.
    fn reserve(&self, current: &AtomicU64, count: u64) -> u64 {
        let mut state = self.state.lock();
        let first = current.fetch_add(count, Ordering::SeqCst) + 1;
        for lsn in first..first + count {
            state.slots.insert(lsn, Slot::Reserved);
        }
        first
    }

    /// Resolve reserved LSNs (`Some` = publish, `None` = skip) and publish every
    /// entry that is now contiguous with the watermark, in order.
    fn resolve(&self, items: Vec<(u64, Option<WalOperation>)>) {
        let mut state = self.state.lock();
        for (lsn, operation) in items {
            let slot = match operation {
                Some(operation) => Slot::Ready(operation),
                None => Slot::Skipped,
            };
            state.slots.insert(lsn, slot);
        }
        loop {
            let next = state.next;
            if matches!(state.slots.get(&next), None | Some(Slot::Reserved)) {
                break;
            }
            if let Some(Slot::Ready(_operation)) = state.slots.remove(&next) {
                #[cfg(feature = "ha-tier1")]
                {
                    if ha_state().broadcast_wal_operation(next, &_operation).is_some() {
                        debug!("WAL entry {} published to standbys", next);
                    }
                }
            }
            state.next = next + 1;
        }
        self.published.store(state.next - 1, Ordering::SeqCst);
    }
}

/// Statement-scoped capture of logical WAL operations.
///
/// An autocommit statement logs its row changes while it runs, but it can still fail
/// afterwards and roll back. Logging then would publish history for data that never
/// committed. While a frame is open for the statement's transaction, `append*` calls
/// on that thread are buffered instead; the transaction takes them at commit and
/// writes them in the SAME RocksDB batch as its data, and a rollback simply drops
/// the frame. With no frame open, appends behave exactly as before.
// `Guard` is returned by a `pub(crate)` method that `lib.rs` calls, so the module has to be
// crate-visible even though `wal` itself is private.
#[allow(clippy::redundant_pub_crate)]
pub(crate) mod statement_capture {
    use super::WalOperation;
    use std::cell::RefCell;
    use std::marker::PhantomData;

    struct Frame {
        txn_id: u64,
        wal_id: usize,
        ops: Vec<WalOperation>,
        sync: bool,
    }

    thread_local! {
        static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
    }

    /// Closes its frame (discarding anything not taken) on drop. Not `Send`: a frame
    /// belongs to the thread that executes the statement.
    pub struct Guard {
        depth: usize,
        _thread_bound: PhantomData<*const ()>,
    }

    pub fn begin(txn_id: u64, wal_id: usize) -> Guard {
        let depth = FRAMES.with(|frames| {
            let mut frames = frames.borrow_mut();
            frames.push(Frame {
                txn_id,
                wal_id,
                ops: Vec::new(),
                sync: false,
            });
            frames.len()
        });
        Guard {
            depth,
            _thread_bound: PhantomData,
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            FRAMES.with(|frames| frames.borrow_mut().truncate(self.depth.saturating_sub(1)));
        }
    }

    /// Buffer `operation` if the innermost frame belongs to this WAL; otherwise give
    /// it back so the caller appends it immediately.
    pub(super) fn offer(wal_id: usize, operation: WalOperation, sync: bool) -> Option<WalOperation> {
        FRAMES.with(|frames| {
            let mut frames = frames.borrow_mut();
            match frames.last_mut() {
                Some(frame) if frame.wal_id == wal_id => {
                    frame.ops.push(operation);
                    frame.sync |= sync;
                    None
                }
                _ => Some(operation),
            }
        })
    }

    /// Take what the transaction `txn_id` buffered, and whether any of it asked for
    /// a synchronous write.
    pub fn take(txn_id: u64, wal_id: usize) -> Option<(Vec<WalOperation>, bool)> {
        FRAMES.with(|frames| {
            frames
                .borrow_mut()
                .iter_mut()
                .rev()
                .find(|frame| frame.txn_id == txn_id && frame.wal_id == wal_id)
                .map(|frame| {
                    (
                        std::mem::take(&mut frame.ops),
                        std::mem::replace(&mut frame.sync, false),
                    )
                })
        })
    }
}

/// Entries reserved and sealed into a caller's batch but not yet durable.
///
/// Created by [`WriteAheadLog::stage`]. After the caller's write the owner MUST call
/// exactly one of [`WriteAheadLog::publish_staged`] / [`WriteAheadLog::abandon_staged`];
/// dropping it unresolved would stall publication of every later entry.
#[must_use = "a staged WAL batch must be published or abandoned"]
pub struct StagedWal {
    entries: Vec<WalEntry>,
}

/// Write-Ahead Log
///
/// Provides durability through write-ahead logging. All modifications are
/// logged before being applied to the main database.
///
/// The WAL uses RocksDB's built-in WAL support with WriteBatch for atomic
/// operations. Each entry is assigned a monotonically increasing LSN.
///
/// **Group Commit Optimization**:
/// When in GroupCommit mode, writes are batched together and flushed
/// periodically (default: 10ms) to reduce fsync overhead. This provides
/// 10-100x throughput improvement while maintaining durability with
/// bounded latency increase.
///
/// Recovery process:
/// 1. On startup, check for incomplete WAL entries
/// 2. Replay entries that aren't reflected in the main database
/// 3. Continue normal operation
pub struct WriteAheadLog {
    /// RocksDB instance (shares the same DB as the storage engine). Written
    /// only through `gate`.
    db: Arc<DB>,
    /// The write gate of the engine that owns `db`.
    gate: Arc<WriteGate>,
    /// Current LSN (Log Sequence Number)
    current_lsn: Arc<AtomicU64>,
    /// In-order, after-durable publication of entries to standbys.
    publisher: Arc<Publisher>,
    /// Synchronization mode
    sync_mode: WalSyncMode,
    /// Write options for WAL operations
    write_opts: WriteOptions,
    /// Pending writes queue for group commit (None if not in GroupCommit mode)
    commit_queue: Option<Arc<Mutex<VecDeque<PendingWrite>>>>,
    /// Background commit thread handle (None if not in GroupCommit mode)
    commit_thread: Option<Arc<Mutex<Option<JoinHandle<()>>>>>,
    /// Batch timeout for group commit (default: 10ms)
    batch_timeout: Duration,
    /// The storage boundary's key manager, or `None` when encryption is
    /// disabled. `wal:entries:` values are row images and are sealed with it;
    /// see the value codec above.
    key_manager: Option<Arc<KeyManager>>,
    /// GH#35 (A3): advance the durable checkpoint every N appends (0 =
    /// disabled). Bounds the crash window: a process killed without running
    /// `Drop` leaves at most this many entries above the checkpoint.
    checkpoint_interval_entries: u64,
    /// GH#35 (A3): advance the checkpoint at most this often (0 = disabled).
    checkpoint_interval_secs: u64,
    /// Appends issued since the last periodic checkpoint.
    appends_since_checkpoint: AtomicU64,
    /// Epoch seconds of the last periodic checkpoint (lock-free time check).
    last_checkpoint_epoch_secs: AtomicU64,
    /// How many entries to keep BELOW the checkpoint for standbys (0 = keep
    /// none). The checkpoint is the redo floor; entries kept under it are never
    /// replayed on this store, only served to a standby catching up. See
    /// [`Self::truncate_to_checkpoint`] and [`Self::PRUNED_THROUGH_KEY`].
    retain_entries: u64,
}

impl WriteAheadLog {
    /// Open or create a WAL with no encryption key.
    ///
    /// Thin wrapper over [`WriteAheadLog::open_with_key_manager`]; every caller
    /// that has a key manager must use that one instead, so that the WAL agrees
    /// with the rest of the storage boundary about the stored form. `gate` is
    /// the write gate of the engine that owns `db` (`WriteGate::open()` for a
    /// handle the caller owns outright).
    pub fn open(db: Arc<DB>, gate: Arc<WriteGate>, sync_mode: WalSyncMode) -> Result<Self> {
        Self::open_with_key_manager(db, gate, sync_mode, None)
    }

    /// Open or create a WAL
    ///
    /// The WAL uses RocksDB's built-in WAL functionality. This implementation
    /// adds logical WAL entries on top of RocksDB's physical WAL.
    ///
    /// `key_manager` is taken at construction rather than through a later
    /// setter because the group-commit thread is spawned here and captures it:
    /// a key installed after the spawn would leave that thread writing in a
    /// different form from every other append path. The same holds for `gate`.
    pub fn open_with_key_manager(
        db: Arc<DB>,
        gate: Arc<WriteGate>,
        sync_mode: WalSyncMode,
        key_manager: Option<Arc<KeyManager>>,
    ) -> Result<Self> {
        // Configure write options based on sync mode
        let mut write_opts = WriteOptions::default();
        match sync_mode {
            WalSyncMode::Sync => {
                write_opts.set_sync(true);
                info!("WAL initialized in synchronous mode (fsync on every write)");
            }
            WalSyncMode::Async => {
                write_opts.set_sync(false);
                write_opts.disable_wal(false); // Enable WAL but async
                info!("WAL initialized in asynchronous mode");
            }
            WalSyncMode::GroupCommit => {
                write_opts.set_sync(false);
                write_opts.disable_wal(false);
                info!("WAL initialized in group commit mode");
            }
        }

        // Recover the last LSN from the database
        let current_lsn = Self::recover_last_lsn(&db)?;
        info!("WAL recovered with LSN starting at {}", current_lsn);

        // Initialize group commit structures if needed
        let (commit_queue, commit_thread) = if sync_mode == WalSyncMode::GroupCommit {
            let queue = Arc::new(Mutex::new(VecDeque::new()));
            (Some(queue), Some(Arc::new(Mutex::new(None))))
        } else {
            (None, None)
        };

        let batch_timeout = Duration::from_millis(10); // 10ms batch window

        let wal = Self {
            db: Arc::clone(&db),
            gate: Arc::clone(&gate),
            current_lsn: Arc::new(AtomicU64::new(current_lsn)),
            publisher: Arc::new(Publisher::new(current_lsn)),
            sync_mode,
            write_opts,
            commit_queue: commit_queue.clone(),
            commit_thread: commit_thread.clone(),
            batch_timeout,
            key_manager,
            // GH#35 (A3) is OFF until the engine installs its policy below;
            // see `with_checkpoint_policy`.
            checkpoint_interval_entries: 0,
            checkpoint_interval_secs: 0,
            appends_since_checkpoint: AtomicU64::new(0),
            last_checkpoint_epoch_secs: AtomicU64::new(Self::now_epoch_secs()),
            retain_entries: 0,
        };

        // Start group commit thread if in GroupCommit mode
        if sync_mode == WalSyncMode::GroupCommit {
            if let Some(queue) = commit_queue {
                let db_clone = Arc::clone(&db);
                let gate_clone = Arc::clone(&gate);
                let publisher_clone = Arc::clone(&wal.publisher);
                let batch_timeout = wal.batch_timeout;
                let key_manager_clone = wal.key_manager.clone();

                let handle = thread::spawn(move || {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        Self::group_commit_loop(
                            db_clone,
                            gate_clone,
                            queue,
                            publisher_clone,
                            batch_timeout,
                            key_manager_clone,
                        );
                    })) {
                        Ok(()) => {}
                        Err(panic_info) => {
                            let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                                s.to_string()
                            } else if let Some(s) = panic_info.downcast_ref::<String>() {
                                s.clone()
                            } else {
                                "unknown panic".to_string()
                            };
                            error!("WAL group commit thread panicked: {}", msg);
                        }
                    }
                });

                if let Some(thread_handle) = &commit_thread {
                    *thread_handle.lock() = Some(handle);
                }
            }
        }

        Ok(wal)
    }

    /// GH#35 (A3): install the periodic-checkpoint policy. Called by the
    /// engine right after open with `StorageConfig::wal_checkpoint_interval_*`.
    /// Either argument at 0 disables that trigger; both at 0 disables A3.
    pub fn with_checkpoint_policy(mut self, entries: u64, secs: u64) -> Self {
        self.checkpoint_interval_entries = entries;
        self.checkpoint_interval_secs = secs;
        self
    }

    /// Keep the newest `entries` LSNs below each checkpoint instead of deleting
    /// them, so a standby that is behind can still be caught up (0 = delete
    /// everything up to the checkpoint, the pre-4.42 behaviour). Called by the
    /// engine with `ReplicationConfig::effective_logical_wal_retention`.
    pub fn with_retention(mut self, entries: u64) -> Self {
        self.retain_entries = entries;
        self
    }

    fn now_epoch_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// GH#35 (A3): after an append, advance the durable checkpoint when the
    /// configured entry or time threshold is due.
    ///
    /// Safety: an append is issued just before its operation's data write, so
    /// `current_lsn` may name an operation still in flight. Advancing past it
    /// is deliberate and bounded — the caller is not told the statement
    /// succeeded until its data write returns, so the only thing a crash can
    /// lose here is an operation the client never saw acknowledged, whereas
    /// leaving history above the checkpoint is what destroys live rows. The
    /// destructive-replay policy (`wal_replay_destructive_ddl`) is the
    /// fail-closed backstop for the same window.
    fn maybe_advance_checkpoint(&self) {
        if self.checkpoint_interval_entries == 0 && self.checkpoint_interval_secs == 0 {
            return;
        }
        let appends = self.appends_since_checkpoint.fetch_add(1, Ordering::AcqRel) + 1;
        let due_by_entries = self.checkpoint_interval_entries > 0 && appends >= self.checkpoint_interval_entries;
        let due_by_time = self.checkpoint_interval_secs > 0
            && Self::now_epoch_secs().saturating_sub(self.last_checkpoint_epoch_secs.load(Ordering::Acquire))
                >= self.checkpoint_interval_secs;
        if !due_by_entries && !due_by_time {
            return;
        }
        let lsn = self.current_lsn();
        if lsn == 0 {
            return;
        }
        match self.truncate_to_checkpoint(lsn) {
            Ok(_) => {
                self.appends_since_checkpoint.store(0, Ordering::Release);
                self.last_checkpoint_epoch_secs
                    .store(Self::now_epoch_secs(), Ordering::Release);
            }
            Err(e) => warn!("logical WAL periodic checkpoint at LSN {} failed: {}", lsn, e),
        }
    }

    /// Recover the last LSN from the database
    fn recover_last_lsn(db: &DB) -> Result<u64> {
        // Check if we have a stored LSN marker
        match db.get(b"wal:last_lsn") {
            Ok(Some(data)) => {
                let lsn = u64::from_le_bytes(
                    data.as_slice()
                        .try_into()
                        .map_err(|_| Error::storage("Invalid LSN format"))?,
                );
                debug!("Recovered last LSN: {}", lsn);
                Ok(lsn)
            }
            Ok(None) => {
                debug!("No previous LSN found, starting from 0");
                Ok(0)
            }
            Err(e) => {
                warn!("Failed to recover LSN: {}, starting from 0", e);
                Ok(0)
            }
        }
    }

    /// Reserve `count` contiguous LSNs with the publisher.
    fn reserve(&self, count: u64) -> u64 {
        self.publisher.reserve(&self.current_lsn, count)
    }

    /// Identity of this log, for matching statement-capture frames.
    fn capture_id(&self) -> usize {
        Arc::as_ptr(&self.current_lsn) as usize
    }

    /// Open a statement-capture frame for transaction `txn_id` on this log.
    pub(crate) fn begin_statement_capture(&self, txn_id: u64) -> statement_capture::Guard {
        statement_capture::begin(txn_id, self.capture_id())
    }

    /// What `txn_id`'s statement buffered (see [`statement_capture`]).
    pub(crate) fn take_statement_capture(&self, txn_id: u64) -> Option<(Vec<WalOperation>, bool)> {
        statement_capture::take(txn_id, self.capture_id())
    }

    /// Highest LSN below which every entry is either published or a known hole.
    /// Catch-up reads must not go above it.
    pub fn published_lsn(&self) -> u64 {
        self.publisher.published.load(Ordering::SeqCst)
    }

    /// Resolve one reserved LSN as persisted, publish in order, and (sync modes)
    /// wait for the standby acknowledgement.
    fn publish_one(&self, lsn: u64, operation: WalOperation) {
        self.publisher.resolve(vec![(lsn, Some(operation))]);
        Self::wait_replicated(lsn);
    }

    /// Resolve a reserved LSN whose write failed: a hole, never published.
    fn skip(&self, lsns: impl IntoIterator<Item = u64>) {
        self.publisher
            .resolve(lsns.into_iter().map(|lsn| (lsn, None)).collect());
    }

    /// In sync/semi-sync mode block (bounded) until a standby has the entry. A no-op
    /// without a connected standby, so a lone primary never pays the timeout.
    pub fn wait_replicated(lsn: u64) {
        #[cfg(feature = "ha-tier1")]
        {
            use crate::replication::ha_state::HARole;
            let state = ha_state();
            if state.get_role() == HARole::Primary && !state.get_standbys().is_empty() {
                // Faster-safe: wait for the first standby ACK, not all.
                const DEFAULT_SYNC_TIMEOUT_MS: u64 = 10000;
                if let Err(e) = state.wait_for_sync(lsn, DEFAULT_SYNC_TIMEOUT_MS) {
                    // Log but don't fail: data is written locally; degraded mode.
                    warn!("Sync replication wait for LSN {}: {}", lsn, e);
                }
            }
        }
        #[cfg(not(feature = "ha-tier1"))]
        {
            let _ = lsn;
        }
    }

    /// Seal `operations` into `batch` under freshly reserved LSNs WITHOUT writing.
    ///
    /// The caller writes `batch` (its data plus these entries plus `wal:last_lsn`) in
    /// ONE RocksDB write, then calls [`Self::publish_staged`] on success or
    /// [`Self::abandon_staged`] on failure. History and data are therefore atomic:
    /// there is no state where a standby can be shown an entry for data that did not
    /// commit, and none where committed data lacks its entry.
    pub fn stage(&self, operations: Vec<WalOperation>, batch: &mut WriteBatch) -> Result<Option<StagedWal>> {
        if operations.is_empty() {
            return Ok(None);
        }
        let count = operations.len() as u64;
        let first = self.reserve(count);
        let mut entries = Vec::with_capacity(operations.len());
        let mut sealed = Vec::with_capacity(operations.len());
        for (offset, operation) in operations.into_iter().enumerate() {
            let entry = WalEntry::new(first + offset as u64, operation);
            match stored_entry_bytes(self.key_manager.as_deref(), &entry) {
                Ok(data) => sealed.push((format!("wal:entries:{:020}", entry.lsn), data)),
                Err(e) => {
                    self.skip(first..first + count);
                    return Err(e);
                }
            }
            entries.push(entry);
        }
        // Only now touch the caller's batch, so a sealing failure leaves it untouched.
        for (key, data) in &sealed {
            batch.put(key.as_bytes(), data);
        }
        batch.put(b"wal:last_lsn", (first + count - 1).to_le_bytes());
        Ok(Some(StagedWal { entries }))
    }

    /// The staged batch is durable: publish its entries in LSN order. Returns the
    /// highest LSN, to pass to [`Self::wait_replicated`] once locks are released.
    pub fn publish_staged(&self, staged: StagedWal) -> u64 {
        let highest = staged.entries.last().map_or(0, |entry| entry.lsn);
        self.publisher
            .resolve(staged.entries.into_iter().map(|e| (e.lsn, Some(e.operation))).collect());
        self.maybe_advance_checkpoint();
        highest
    }

    /// The staged batch was not written: its LSNs become holes and nothing is published.
    pub fn abandon_staged(&self, staged: StagedWal) {
        self.skip(staged.entries.iter().map(|entry| entry.lsn));
    }

    /// Append an entry to the WAL
    ///
    /// Returns the LSN of the appended entry.
    ///
    /// The entry is written to RocksDB with a special WAL prefix.
    /// If sync_mode is Sync, the write is fsynced immediately.
    /// If sync_mode is GroupCommit, the write is queued and batched.
    pub fn append(&self, operation: WalOperation) -> Result<u64> {
        // Inside a statement-capture frame the entry is buffered and written with the
        // transaction's own commit batch (see `statement_capture`).
        let Some(operation) = statement_capture::offer(self.capture_id(), operation, true) else {
            return Ok(0);
        };

        // Use group commit path if enabled
        if self.sync_mode == WalSyncMode::GroupCommit {
            return self.append_group_commit(operation);
        }

        // Original synchronous/async path
        let lsn = self.reserve(1);
        let entry = WalEntry::new(lsn, operation);

        // Serialize and seal the entry (see the value codec above).
        let data = match stored_entry_bytes(self.key_manager.as_deref(), &entry) {
            Ok(data) => data,
            Err(e) => {
                self.skip([lsn]);
                return Err(e);
            }
        };

        // Create a batch for atomic write
        let mut batch = WriteBatch::default();

        // Write the WAL entry with key: wal:entries:{lsn}
        let key = format!("wal:entries:{:020}", lsn);
        batch.put(key.as_bytes(), &data);

        // Update last LSN marker
        batch.put(b"wal:last_lsn", lsn.to_le_bytes());

        // Write atomically
        if let Err(e) = self.gate.write(&self.db, batch, Some(&self.write_opts)) {
            self.skip([lsn]);
            return Err(Error::storage(format!("Failed to append WAL entry: {}", e)));
        }

        // Publish to standbys in LSN order (and, in sync/semi-sync mode, wait for the ACK).
        self.publish_one(lsn, entry.operation);

        // GH#35 (A3): periodic checkpoint advancement.
        self.maybe_advance_checkpoint();

        debug!("Appended WAL entry with LSN {}", lsn);
        Ok(lsn)
    }

    /// Append a WAL entry without fsync (for DDL operations where metadata
    /// is already crash-safe in RocksDB). Saves ~1ms per DDL operation.
    pub fn append_nosync(&self, operation: WalOperation) -> Result<u64> {
        let Some(operation) = statement_capture::offer(self.capture_id(), operation, false) else {
            return Ok(0);
        };
        let lsn = self.reserve(1);
        let entry = WalEntry::new(lsn, operation);
        let data = match stored_entry_bytes(self.key_manager.as_deref(), &entry) {
            Ok(data) => data,
            Err(e) => {
                self.skip([lsn]);
                return Err(e);
            }
        };

        let mut batch = WriteBatch::default();
        let key = format!("wal:entries:{:020}", lsn);
        batch.put(key.as_bytes(), &data);
        batch.put(b"wal:last_lsn", lsn.to_le_bytes());

        // Write without fsync
        let mut nosync_opts = WriteOptions::default();
        nosync_opts.set_sync(false);
        if let Err(e) = self.gate.write(&self.db, batch, Some(&nosync_opts)) {
            self.skip([lsn]);
            return Err(Error::storage(format!("Failed to append WAL entry: {}", e)));
        }

        // P0#2: still publish to HA standbys (and honor sync/semi-sync ACK).
        // The nosync path only skips the local fsync — it must NOT skip replication.
        self.publish_one(lsn, entry.operation);

        // GH#35 (A3): periodic checkpoint advancement.
        self.maybe_advance_checkpoint();

        Ok(lsn)
    }

    /// Append a whole batch of operations as ONE RocksDB write, then broadcast
    /// the batch and wait for sync ONCE — not once per entry.
    ///
    /// Built for `Transaction::commit_with_timestamp`: an explicit or session
    /// transaction's writes are only fully known at COMMIT, so they arrive here
    /// as a set rather than one at a time (ROADMAP_V5.md §1.8). Looping over
    /// `append`/`append_nosync` instead would be quietly catastrophic under
    /// sync/semi-sync replication: each call runs its own
    /// `broadcast_after_append`, so an N-row transaction would pay N
    /// independent `wait_for_sync` calls of up to `DEFAULT_SYNC_TIMEOUT_MS`
    /// each. `broadcast_after_batch`'s existing "wait once, on the highest LSN"
    /// contract is exactly right for a committed batch and is reused verbatim.
    ///
    /// `sync` fsyncs this WAL-keyspace write (legacy strict logical-WAL
    /// durability — `StorageConfig::logical_wal_per_statement`). The default,
    /// `false`, follows the same P0#2 rationale as `append_nosync`: the
    /// caller's own commit WriteBatch is the durability boundary, not this
    /// one — and, exactly as there, skipping the fsync must NOT skip
    /// replication, so both arms broadcast.
    ///
    /// LSNs are handed out by a tight `next_lsn()` loop with no yield point, so
    /// one transaction's entries land on contiguous LSNs that no other
    /// committer can interleave into. That is a property a future atomic-apply
    /// standby can lean on instead of adding a `tx_id` field to the per-row
    /// variants (ROADMAP_V5.md §1.8 follow-up).
    ///
    /// Returns the assigned LSNs in the same order as `operations`, or
    /// `Ok(vec![])` — touching neither RocksDB nor the broadcast channel — when
    /// `operations` is empty (a transaction that staged nothing loggable).
    pub fn append_batch(&self, operations: Vec<WalOperation>, sync: bool) -> Result<Vec<u64>> {
        let mut batch = WriteBatch::default();
        let Some(staged) = self.stage(operations, &mut batch)? else {
            return Ok(Vec::new());
        };
        let lsns: Vec<u64> = staged.entries.iter().map(|entry| entry.lsn).collect();

        // Deliberately bypasses `WalSyncMode::GroupCommit` the way `append_nosync`
        // does: the "batched/nosync family" of appends always writes directly.
        let written = if sync {
            self.gate.write(&self.db, batch, Some(&self.write_opts))
        } else {
            let mut nosync_opts = WriteOptions::default();
            nosync_opts.set_sync(false);
            self.gate.write(&self.db, batch, Some(&nosync_opts))
        };
        if let Err(e) = written {
            self.abandon_staged(staged);
            return Err(Error::storage(format!("Failed to append WAL batch: {}", e)));
        }

        let highest = self.publish_staged(staged);
        Self::wait_replicated(highest);
        debug!("Appended WAL batch: {} entries, LSN {}", lsns.len(), highest);
        Ok(lsns)
    }

    /// Append `operation` together with the caller's data `batch` in ONE write, so
    /// the entry exists exactly when the data does. Inside a statement-capture frame
    /// the data is written now and the entry is deferred to the transaction commit.
    pub fn append_with_batch(
        &self,
        operation: WalOperation,
        mut batch: WriteBatch,
        options: Option<&WriteOptions>,
    ) -> Result<u64> {
        let write = |batch: WriteBatch| -> Result<()> {
            self.gate
                .write(&self.db, batch, options)
                .map_err(|e| Error::storage(format!("Failed to write data with WAL entry: {}", e)))
        };
        let Some(operation) = statement_capture::offer(self.capture_id(), operation, false) else {
            write(batch)?;
            return Ok(0);
        };
        let Some(staged) = self.stage(vec![operation], &mut batch)? else {
            write(batch)?;
            return Ok(0);
        };
        if let Err(e) = write(batch) {
            self.abandon_staged(staged);
            return Err(e);
        }
        let lsn = self.publish_staged(staged);
        Self::wait_replicated(lsn);
        Ok(lsn)
    }

    /// Append a WAL entry and wait for synchronous replication acknowledgement
    ///
    /// This method appends the operation to WAL and then blocks until standbys
    /// have acknowledged receipt (semi-sync) or application (sync) of the entry,
    /// based on the configured sync mode.
    ///
    /// # Arguments
    /// * `operation` - The WAL operation to append
    ///
    /// # Returns
    /// * `Ok(lsn)` - The LSN of the appended entry
    /// * `Err` - If WAL append fails or sync wait times out
    pub fn append_sync(&self, operation: WalOperation) -> Result<u64> {
        // First, append the entry normally
        let lsn = self.append(operation)?;

        // Then wait for synchronous replication based on configured mode
        #[cfg(feature = "ha-tier1")]
        {
            use crate::replication::ha_state::ha_state;

            // Default timeout of 30 seconds for sync wait
            const DEFAULT_SYNC_TIMEOUT_MS: u64 = 30000;

            if let Err(e) = ha_state().wait_for_sync(lsn, DEFAULT_SYNC_TIMEOUT_MS) {
                // Log warning but don't fail the append - the data is safely written locally
                // This allows the system to continue operating in degraded mode
                warn!("Sync replication timeout for LSN {}: {}", lsn, e);
            }
        }

        Ok(lsn)
    }

    /// Append entry using group commit
    fn append_group_commit(&self, operation: WalOperation) -> Result<u64> {
        let lsn = self.reserve(1);
        let entry = WalEntry::new(lsn, operation);

        // Create channel for receiving result
        let (tx, rx) = crossbeam::channel::bounded(1);

        // Queue the write
        if let Some(queue) = &self.commit_queue {
            let pending = PendingWrite { entry, result_tx: tx };
            queue.lock().push_back(pending);
        } else {
            self.skip([lsn]);
            return Err(Error::storage("Group commit queue not initialized"));
        }

        // Wait for batch commit to complete
        let result = match rx.recv() {
            Ok(result) => result,
            Err(e) => Err(Error::storage(format!("Group commit failed: {}", e))),
        };
        if result.is_ok() {
            // GH#35 (A3): periodic checkpoint advancement.
            self.maybe_advance_checkpoint();
        }
        result
    }

    /// Flush WAL to disk
    ///
    /// Forces an fsync of the WAL. Only needed in Async or GroupCommit modes.
    /// In Sync mode, this is a no-op as every write is already fsynced.
    pub fn flush(&self) -> Result<()> {
        match self.sync_mode {
            WalSyncMode::Sync => {
                // Already synced on every write
                Ok(())
            }
            WalSyncMode::Async | WalSyncMode::GroupCommit => {
                self.db
                    .flush_wal(true)
                    .map_err(|e| Error::storage(format!("Failed to flush WAL: {}", e)))?;
                debug!("WAL flushed to disk");
                Ok(())
            }
        }
    }

    /// Storage key holding this store's WAL checkpoint LSN.
    ///
    /// The value is a little-endian `u64` `N` and means exactly one thing:
    /// **every WAL entry with `lsn <= N` has already been applied to this
    /// store's data keyspace and must never be replayed again.**
    ///
    /// The key's ABSENCE is meaningful too, and is the crux of the data-loss
    /// fix in `StorageEngine::open`: a store that has never recorded a
    /// checkpoint carries retained `wal:entries:` written by a build that
    /// never truncated them, so its retained log is history, not redo.
    pub const CHECKPOINT_KEY: &'static [u8] = b"wal:checkpoint";

    /// Prefix under which individual entries live: `wal:entries:{lsn:020}`.
    pub const ENTRY_PREFIX: &'static [u8] = b"wal:entries:";

    /// Storage key holding the history floor: a little-endian `u64` `P` meaning
    /// **every entry with `lsn <= P` has been deleted.** It differs from the
    /// checkpoint (the redo floor) when the store keeps history for standbys
    /// ([`Self::with_retention`]); catch-up can serve any position `>= P`.
    ///
    /// Stores written before this key existed deleted every entry up to their
    /// checkpoint, so its absence means "the checkpoint" (see
    /// [`Self::pruned_through`]).
    pub const PRUNED_THROUGH_KEY: &'static [u8] = b"wal:pruned_through";

    /// Parse the LSN out of a `wal:entries:{lsn:020}` storage key.
    ///
    /// Key-level rather than value-level on purpose: it needs neither
    /// decryption nor bincode, so it still works for an entry whose VALUE is
    /// torn or was sealed under a key we no longer hold. Both the checkpoint
    /// scan and the truncation below rely on that — a single undecodable
    /// record must not be able to wedge recovery or leave the log
    /// un-truncatable forever.
    fn lsn_from_entry_key(key: &[u8]) -> Option<u64> {
        let digits = key.strip_prefix(Self::ENTRY_PREFIX)?;
        std::str::from_utf8(digits).ok()?.parse::<u64>().ok()
    }

    /// Read the durable checkpoint LSN, or `None` if this store has never
    /// recorded one.
    ///
    /// `None` is NOT the same as `Some(0)`: see [`Self::CHECKPOINT_KEY`].
    pub fn checkpoint_lsn(&self) -> Result<Option<u64>> {
        match self.db.get(Self::CHECKPOINT_KEY) {
            Ok(Some(data)) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::storage("Invalid WAL checkpoint format"))?;
                Ok(Some(u64::from_le_bytes(bytes)))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(Error::storage(format!("Failed to read WAL checkpoint: {}", e))),
        }
    }

    /// The history floor: every entry with `lsn <= ` the returned value has been
    /// deleted, so a standby can be caught up from any position at or above it.
    ///
    /// Without a [`Self::PRUNED_THROUGH_KEY`] marker the floor is the checkpoint,
    /// because that is what earlier builds deleted through; a store that never
    /// checkpointed has deleted nothing (0).
    pub fn pruned_through(&self) -> Result<u64> {
        match self.db.get(Self::PRUNED_THROUGH_KEY) {
            Ok(Some(data)) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::storage("Invalid WAL history-floor format"))?;
                Ok(u64::from_le_bytes(bytes))
            }
            Ok(None) => Ok(self.checkpoint_lsn()?.unwrap_or(0)),
            Err(e) => Err(Error::storage(format!("Failed to read the WAL history floor: {}", e))),
        }
    }

    /// Survey the retained `wal:entries:` keyspace WITHOUT decoding any value.
    ///
    /// Returns `(count, min_lsn, max_lsn)`, or `None` when nothing is
    /// retained. Used at open to classify the store before deciding whether to
    /// replay, so the decision itself can never be changed by a corrupt
    /// record.
    pub fn retained_entry_span(&self) -> Result<Option<(usize, u64, u64)>> {
        let mut count = 0usize;
        let mut min_lsn = u64::MAX;
        let mut max_lsn = 0u64;

        let iter = self.db.prefix_iterator(Self::ENTRY_PREFIX);
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("WAL entry scan iterator error: {}", e)))?;
            if !key.starts_with(Self::ENTRY_PREFIX) {
                break;
            }
            count += 1;
            if let Some(lsn) = Self::lsn_from_entry_key(&key) {
                min_lsn = min_lsn.min(lsn);
                max_lsn = max_lsn.max(lsn);
            }
            // A key under the entry prefix that does not parse is counted (the
            // log is not empty) but cannot move the span: replay parses the
            // same way, so it can never be replayed either.
        }

        if count == 0 {
            Ok(None)
        } else if min_lsn == u64::MAX {
            // Only unparseable keys: present, but with no usable LSN span.
            Ok(Some((count, 0, 0)))
        } else {
            Ok(Some((count, min_lsn, max_lsn)))
        }
    }

    /// Record `up_to_lsn` as the checkpoint and delete the entries it makes
    /// unnecessary, in ONE RocksDB `WriteBatch`.
    ///
    /// With no retention every entry with `lsn <= up_to_lsn` is deleted. With a
    /// retention of `n` entries ([`Self::with_retention`]) the newest `n` LSNs
    /// at or below the checkpoint stay, for standbys to catch up from: they are
    /// below the redo floor, so open-time recovery never replays them. The
    /// history floor ([`Self::PRUNED_THROUGH_KEY`]) records what was actually
    /// deleted and only moves forward.
    ///
    /// Atomicity here is the whole point. The two facts "these entries are
    /// gone" and "everything up to this LSN is applied" must never disagree on
    /// disk: a truncate that landed without its checkpoint would leave a store
    /// looking un-checkpointed, and a checkpoint that landed without its
    /// truncate would leave entries that are correctly never replayed but also
    /// never reclaimed. `WriteBatch` gives all-or-nothing; the write is
    /// fsynced because this is the one durability boundary that later recovery
    /// decisions are read from.
    ///
    /// LSNs come from the KEY, not the value, so an undecodable record is
    /// still reclaimed rather than pinning the log forever (the pre-existing
    /// [`Self::truncate`] decodes every value and aborts on the first bad one).
    ///
    /// Returns the number of entries deleted.
    pub fn truncate_to_checkpoint(&self, up_to_lsn: u64) -> Result<usize> {
        let delete_through = up_to_lsn.saturating_sub(self.retain_entries);
        let pruned_through = self.pruned_through()?.max(delete_through);
        let mut batch = WriteBatch::default();
        let mut deleted = 0usize;

        let iter = self.db.prefix_iterator(Self::ENTRY_PREFIX);
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("WAL truncate iterator error: {}", e)))?;
            if !key.starts_with(Self::ENTRY_PREFIX) {
                break;
            }
            match Self::lsn_from_entry_key(&key) {
                Some(lsn) if lsn <= delete_through => {
                    batch.delete(&key);
                    deleted += 1;
                }
                // Keys are zero-padded LSNs, so they sort numerically: once one is
                // kept, so is everything after it. Stopping keeps a checkpoint's
                // cost proportional to what it deletes, not to the retained
                // window (which would otherwise be rescanned every checkpoint).
                Some(_) if self.retain_entries > 0 => break,
                Some(_) => {}
                None => {
                    // Unparseable key under the entry prefix: replay parses the
                    // same way and would skip it forever, so reclaim it.
                    batch.delete(&key);
                    deleted += 1;
                }
            }
        }

        batch.put(Self::CHECKPOINT_KEY, up_to_lsn.to_le_bytes());
        batch.put(Self::PRUNED_THROUGH_KEY, pruned_through.to_le_bytes());

        // Keep the LSN counter monotonic across sessions: the checkpoint is a
        // floor, so the next session's first append must be > up_to_lsn or it
        // is born BELOW the checkpoint and every future replay skips it. This
        // matters because `current_lsn` also advances on commit-only
        // increments (`increment_lsn`, no entry written), so the durable
        // `wal:last_lsn` can trail the checkpoint target by the number of
        // commits that issued no logical entry.
        let durable_last = match self.db.get(b"wal:last_lsn") {
            Ok(Some(bytes)) => {
                let mut buf = [0u8; 8];
                if bytes.len() == 8 {
                    buf.copy_from_slice(&bytes);
                }
                u64::from_le_bytes(buf)
            }
            _ => 0,
        };
        if up_to_lsn > durable_last {
            batch.put(b"wal:last_lsn", up_to_lsn.to_le_bytes());
        }

        let mut sync_opts = WriteOptions::default();
        sync_opts.set_sync(true);
        self.gate
            .write(&self.db, batch, Some(&sync_opts))
            .map_err(|e| Error::storage(format!("Failed to checkpoint WAL: {}", e)))?;

        if self.retain_entries > 0 {
            info!(
                "WAL checkpoint recorded at LSN {} ({} entries reclaimed; history kept for standbys above LSN {})",
                up_to_lsn, deleted, pruned_through
            );
        } else {
            info!(
                "WAL checkpoint recorded at LSN {} ({} entries reclaimed)",
                up_to_lsn, deleted
            );
        }
        Ok(deleted)
    }

    /// Replay WAL entries for crash recovery
    ///
    /// Returns all retained WAL entries in LSN order. Equivalent to
    /// `replay_from(0)`; kept for the callers that want the whole retained log
    /// (integrity checks, tooling, tests). Startup recovery goes through
    /// [`Self::replay_from`] with the durable checkpoint instead.
    pub fn replay(&self) -> Result<Vec<WalEntry>> {
        self.replay_from(0)
    }

    /// Replay only the entries strictly after `after_lsn`.
    ///
    /// `after_lsn` is a checkpoint: every entry at or below it is already
    /// applied to the data keyspace, so re-executing it is not redo, it is
    /// re-running history — which for DDL (`DropTable`, `Truncate`) destroys
    /// live rows. Entries at or below the checkpoint are skipped on the KEY,
    /// without decoding the value at all.
    pub fn replay_from(&self, after_lsn: u64) -> Result<Vec<WalEntry>> {
        if after_lsn == 0 {
            info!("Starting WAL replay for crash recovery");
        } else {
            info!(
                "Starting WAL replay for crash recovery after checkpoint LSN {}",
                after_lsn
            );
        }
        let mut entries = Vec::new();
        let mut skipped_checkpointed = 0usize;
        let prefix = Self::ENTRY_PREFIX;

        // Iterate over all WAL entries. Keys are `wal:entries:{lsn:020}`, so
        // the prefix iterator yields them in ascending-LSN order.
        let iter = self.db.prefix_iterator(prefix);
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("WAL replay iterator error: {}", e)))?;

            // Skip if not a WAL entry
            if !key.starts_with(prefix) {
                break;
            }

            // Checkpoint filter, applied on the key so an already-applied entry
            // is never even decoded.
            if let Some(lsn) = Self::lsn_from_entry_key(&key) {
                if lsn <= after_lsn {
                    skipped_checkpointed += 1;
                    continue;
                }
            }

            // C13: a record that fails to decode is a torn/corrupt TAIL — the
            // last append at crash time may be partial. Because entries are in
            // ascending-LSN order, everything BEFORE it is a valid, complete
            // prefix and is safe to replay; a record after a gap can never have
            // been acknowledged (LSN is issued in order), so we stop here and
            // keep the good prefix instead of aborting the entire recovery
            // (which the old `?` did, discarding every recovered entry over one
            // bad tail record).
            match decode_entry(self.key_manager.as_deref(), &key, &value) {
                Ok(entry) => {
                    debug!("Replaying WAL entry with LSN {}", entry.lsn);
                    entries.push(entry);
                }
                Err(e) => {
                    warn!(
                        "WAL replay stopping at first undecodable record ({}): keeping {} valid entries recovered before it (torn tail)",
                        e,
                        entries.len()
                    );
                    break;
                }
            }
        }

        if skipped_checkpointed > 0 {
            info!(
                "WAL replay skipped {} entries at or below checkpoint LSN {}",
                skipped_checkpointed, after_lsn
            );
        }
        info!("WAL replay complete, {} entries recovered", entries.len());
        Ok(entries)
    }

    /// Truncate WAL up to a given LSN (for checkpointing)
    ///
    /// Removes WAL entries older than the specified LSN.
    /// This should only be called after a successful checkpoint.
    pub fn truncate(&self, up_to_lsn: u64) -> Result<()> {
        info!("Truncating WAL entries up to LSN {}", up_to_lsn);
        let mut batch = WriteBatch::default();
        let prefix = b"wal:entries:";

        // Find entries to delete
        let iter = self.db.prefix_iterator(prefix);
        let mut deleted_count = 0;

        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("WAL truncate iterator error: {}", e)))?;

            if !key.starts_with(prefix) {
                break;
            }

            // Parse LSN from entry
            let entry = decode_entry(self.key_manager.as_deref(), &key, &value)?;
            if entry.lsn <= up_to_lsn {
                batch.delete(&key);
                deleted_count += 1;
            }
        }

        // Apply deletions
        self.gate
            .write(&self.db, batch, None)
            .map_err(|e| Error::storage(format!("Failed to truncate WAL: {}", e)))?;

        info!("Truncated {} WAL entries", deleted_count);
        Ok(())
    }

    /// Get current LSN
    pub fn current_lsn(&self) -> u64 {
        self.current_lsn.load(Ordering::SeqCst)
    }

    /// Increment LSN without appending a WAL entry
    ///
    /// Used by transaction commit to track operations even when
    /// the transaction system bypasses WAL for durability.
    /// Returns the new LSN value.
    pub fn increment_lsn(&self) -> u64 {
        // Goes through the publisher so the number is a known hole, not a gap that
        // would stall publication of every later entry.
        let lsn = self.reserve(1);
        self.skip([lsn]);
        lsn
    }

    /// Get synchronization mode
    pub fn sync_mode(&self) -> WalSyncMode {
        self.sync_mode
    }

    /// Change synchronization mode
    ///
    /// Note: This reconfigures write options but doesn't affect in-flight writes.
    /// Changing to/from GroupCommit mode is not supported after initialization.
    pub fn set_sync_mode(&mut self, mode: WalSyncMode) {
        // Warn if trying to change to/from GroupCommit mode
        if (self.sync_mode == WalSyncMode::GroupCommit || mode == WalSyncMode::GroupCommit) && self.sync_mode != mode {
            warn!("Cannot dynamically change to/from GroupCommit mode. This change may not take full effect.");
        }

        self.sync_mode = mode;
        match mode {
            WalSyncMode::Sync => {
                self.write_opts.set_sync(true);
            }
            WalSyncMode::Async | WalSyncMode::GroupCommit => {
                self.write_opts.set_sync(false);
                self.write_opts.disable_wal(false);
            }
        }
        info!("WAL sync mode changed to {:?}", mode);
    }

    /// Group commit background thread loop
    ///
    /// Periodically flushes batched writes to reduce fsync overhead.
    /// Runs until the WAL is dropped or an unrecoverable error occurs.
    fn group_commit_loop(
        db: Arc<DB>,
        gate: Arc<WriteGate>,
        queue: Arc<Mutex<VecDeque<PendingWrite>>>,
        publisher: Arc<Publisher>,
        batch_timeout: Duration,
        key_manager: Option<Arc<KeyManager>>,
    ) {
        info!("Group commit thread started (batch timeout: {:?})", batch_timeout);

        loop {
            // Sleep for batch timeout
            thread::sleep(batch_timeout);

            // Drain queue
            let pending: Vec<PendingWrite> = {
                let mut q = queue.lock();
                if q.is_empty() {
                    continue;
                }
                q.drain(..).collect()
            };

            if pending.is_empty() {
                continue;
            }

            debug!("Group commit: processing {} pending writes", pending.len());

            // Build batch
            let mut batch = WriteBatch::default();
            let mut last_lsn = 0u64;
            let mut sealing_failed = std::collections::HashSet::new();

            for write in &pending {
                let lsn = write.entry.lsn;
                last_lsn = last_lsn.max(lsn);

                // Serialize and seal the entry — the same codec every other
                // append path uses, so a group-committed entry lands in the
                // same stored form as a directly appended one.
                match stored_entry_bytes(key_manager.as_deref(), &write.entry) {
                    Ok(data) => {
                        let key = format!("wal:entries:{:020}", lsn);
                        batch.put(key.as_bytes(), &data);
                    }
                    Err(e) => {
                        // Send error to waiter
                        let _ = write.result_tx.send(Err(e));
                        sealing_failed.insert(write.entry.lsn);
                        continue;
                    }
                }
            }

            // Update last LSN marker
            batch.put(b"wal:last_lsn", last_lsn.to_le_bytes());

            // Flush batch with fsync
            let mut write_opts = WriteOptions::default();
            write_opts.set_sync(true);

            match gate.write(&db, batch, Some(&write_opts)) {
                Ok(()) => {
                    // Publish to standbys in LSN order and, in sync/semi-sync mode,
                    // wait for the highest batch LSN before reporting success.
                    let highest = pending.iter().map(|write| write.entry.lsn).max().unwrap_or(0);
                    let mut items = Vec::with_capacity(pending.len());
                    let mut waiters = Vec::with_capacity(pending.len());
                    for write in pending {
                        let lsn = write.entry.lsn;
                        let operation = (!sealing_failed.contains(&lsn)).then_some(write.entry.operation);
                        items.push((lsn, operation));
                        waiters.push((write.result_tx, lsn));
                    }
                    publisher.resolve(items);
                    Self::wait_replicated(highest);

                    // Notify all waiters of success
                    for (result_tx, lsn) in waiters {
                        let _ = result_tx.send(Ok(lsn));
                    }
                    debug!("Group commit: successfully flushed {} writes", last_lsn);
                }
                Err(e) => {
                    let err_msg = format!("Group commit batch write failed: {}", e);
                    // The LSNs become holes; nothing is published for them.
                    publisher.resolve(pending.iter().map(|write| (write.entry.lsn, None)).collect());
                    // Notify all waiters of failure
                    for write in pending {
                        let _ = write.result_tx.send(Err(Error::storage(err_msg.clone())));
                    }
                    error!("Group commit failed: {}", e);
                }
            }
        }
    }

    /// Verify WAL integrity on startup
    ///
    /// Performs comprehensive integrity checks:
    /// - Validates all entries can be deserialized
    /// - Checks for LSN sequence continuity
    /// - Detects duplicate LSNs
    /// - Validates timestamp ordering
    /// - Reports corruption details
    pub fn verify_integrity(&self) -> Result<WalIntegrityReport> {
        info!("Starting WAL integrity verification");
        let mut report = WalIntegrityReport::new();
        let prefix = b"wal:entries:";

        let mut lsn_map: HashMap<u64, usize> = HashMap::new();
        let mut last_lsn: Option<u64> = None;
        let mut last_timestamp: u64 = 0;
        let mut entry_count = 0u64;

        // Scan all entries
        let iter = self.db.prefix_iterator(prefix);
        for item in iter {
            match item {
                Ok((key, value)) => {
                    if !key.starts_with(prefix) {
                        break;
                    }

                    entry_count += 1;

                    // Try to deserialize
                    match decode_entry(self.key_manager.as_deref(), &key, &value) {
                        Ok(entry) => {
                            // Check for duplicate LSN
                            if let Some(count) = lsn_map.get_mut(&entry.lsn) {
                                *count += 1;
                                report.duplicate_lsns.push(entry.lsn);
                                report.mark_invalid();
                                warn!("Duplicate LSN detected: {}", entry.lsn);
                            } else {
                                lsn_map.insert(entry.lsn, 1);
                            }

                            // Check for LSN gaps
                            if let Some(prev_lsn) = last_lsn {
                                if entry.lsn != prev_lsn + 1 {
                                    report.missing_lsns.push((prev_lsn + 1, entry.lsn - 1));
                                    report.mark_invalid();
                                    warn!("LSN gap detected: {} to {}", prev_lsn + 1, entry.lsn - 1);
                                }
                            }

                            // Check timestamp ordering
                            if entry.timestamp < last_timestamp {
                                report.timestamp_violations.push(entry.lsn);
                                warn!("Timestamp ordering violation at LSN {}", entry.lsn);
                                // Don't mark as invalid - timestamps can be out of order in distributed systems
                            }

                            last_lsn = Some(entry.lsn);
                            last_timestamp = entry.timestamp;
                        }
                        Err(e) => {
                            // Corruption detected
                            if let Some(lsn) = last_lsn {
                                report.corrupted_entries.push(lsn + 1);
                            } else {
                                report.corrupted_entries.push(0);
                            }
                            report.mark_invalid();
                            error!("Corrupted WAL entry detected: {}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("Error reading WAL entry: {}", e);
                    report.mark_invalid();
                }
            }
        }

        report.total_entries = entry_count;

        if report.is_valid {
            info!("WAL integrity verification passed: {} entries verified", entry_count);
        } else {
            error!(
                "WAL integrity verification FAILED: {} corrupted, {} gaps, {} duplicates",
                report.corrupted_entries.len(),
                report.missing_lsns.len(),
                report.duplicate_lsns.len()
            );
        }

        Ok(report)
    }

    /// Replay WAL operations after crash with detailed statistics
    ///
    /// This enhanced replay method:
    /// - Tracks replay statistics
    /// - Handles partial writes gracefully
    /// - Verifies data consistency
    /// - Provides progress reporting
    pub fn replay_with_stats(&self, from_lsn: u64) -> Result<ReplayStats> {
        let start_time = SystemTime::now();
        info!("Starting WAL replay from LSN {}", from_lsn);

        let mut stats = ReplayStats {
            operations_replayed: 0,
            operations_skipped: 0,
            errors: 0,
            start_lsn: from_lsn,
            end_lsn: from_lsn,
            duration_ms: 0,
        };

        let prefix = b"wal:entries:";
        let iter = self.db.prefix_iterator(prefix);

        for item in iter {
            match item {
                Ok((key, value)) => {
                    if !key.starts_with(prefix) {
                        break;
                    }

                    match decode_entry(self.key_manager.as_deref(), &key, &value) {
                        Ok(entry) => {
                            if entry.lsn < from_lsn {
                                stats.operations_skipped += 1;
                                continue;
                            }

                            stats.end_lsn = entry.lsn;

                            // In a production system, we would re-apply operations here
                            // For now, we just count them as this is delegated to RocksDB's WAL
                            debug!("Replaying operation at LSN {}: {:?}", entry.lsn, entry.operation);
                            stats.operations_replayed += 1;
                        }
                        Err(e) => {
                            error!("Failed to deserialize entry during replay: {}", e);
                            stats.errors += 1;
                        }
                    }
                }
                Err(e) => {
                    error!("Error reading entry during replay: {}", e);
                    stats.errors += 1;
                }
            }
        }

        if let Ok(elapsed) = start_time.elapsed() {
            stats.duration_ms = elapsed.as_millis() as u64;
        }

        info!(
            "WAL replay complete: {} operations replayed, {} skipped, {} errors in {}ms",
            stats.operations_replayed, stats.operations_skipped, stats.errors, stats.duration_ms
        );

        Ok(stats)
    }

    /// Rotate WAL log files
    ///
    /// Creates a new WAL segment when size limit is reached.
    /// This is a logical rotation since we use RocksDB's WAL.
    ///
    /// For production, this would:
    /// - Create a new WAL file
    /// - Archive the old WAL file
    /// - Update the active WAL pointer
    pub fn rotate(&self) -> Result<()> {
        info!("Rotating WAL");

        // Get current metrics to determine if rotation is needed
        let metrics = self.metrics()?;

        // Force a flush and checkpoint
        self.flush()?;

        // In RocksDB, rotation is handled internally
        // We can trigger compaction to help manage WAL files
        self.db
            .flush_wal(true)
            .map_err(|e| Error::storage(format!("Failed to rotate WAL: {}", e)))?;

        info!("WAL rotated successfully at LSN {}", metrics.current_lsn);
        Ok(())
    }

    /// Clean up old WAL files based on retention policy
    ///
    /// Deletes WAL entries older than the retention period.
    /// Keeps minimum required entries for recovery.
    ///
    /// # Arguments
    /// * `retention_seconds` - Maximum age of WAL entries to keep
    /// * `min_entries_to_keep` - Minimum number of entries to always retain
    pub fn cleanup_old_logs(&self, retention_seconds: u64, min_entries_to_keep: u64) -> Result<CleanupStats> {
        info!(
            "Starting WAL cleanup (retention: {}s, min entries: {})",
            retention_seconds, min_entries_to_keep
        );

        let mut stats = CleanupStats {
            entries_deleted: 0,
            bytes_freed: 0,
            oldest_lsn: 0,
            newest_lsn: 0,
        };

        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let cutoff_time = current_time.saturating_sub(retention_seconds);

        let cutoff_time_micros = cutoff_time * 1_000_000;

        let prefix = b"wal:entries:";
        let mut entries_to_delete = Vec::new();
        let mut total_entries = 0u64;

        // First pass: identify entries to delete
        let iter = self.db.prefix_iterator(prefix);
        for item in iter {
            match item {
                Ok((key, value)) => {
                    if !key.starts_with(prefix) {
                        break;
                    }

                    total_entries += 1;

                    match decode_entry(self.key_manager.as_deref(), &key, &value) {
                        Ok(entry) => {
                            if stats.oldest_lsn == 0 {
                                stats.oldest_lsn = entry.lsn;
                            }
                            stats.newest_lsn = entry.lsn;

                            // Only delete if:
                            // 1. Entry is older than retention period
                            // 2. We have enough entries remaining
                            if entry.timestamp < cutoff_time_micros {
                                let remaining = total_entries - entries_to_delete.len() as u64;
                                if remaining > min_entries_to_keep {
                                    entries_to_delete.push((key.to_vec(), value.len()));
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to deserialize entry during cleanup: {}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("Error reading entry during cleanup: {}", e);
                }
            }
        }

        // Second pass: delete identified entries
        if !entries_to_delete.is_empty() {
            let mut batch = WriteBatch::default();
            for (key, size) in &entries_to_delete {
                batch.delete(key);
                stats.bytes_freed += *size as u64;
            }

            self.gate
                .write(&self.db, batch, None)
                .map_err(|e| Error::storage(format!("Failed to cleanup WAL: {}", e)))?;

            stats.entries_deleted = entries_to_delete.len() as u64;
        }

        info!(
            "WAL cleanup complete: {} entries deleted, {} bytes freed",
            stats.entries_deleted, stats.bytes_freed
        );

        Ok(stats)
    }

    /// Get WAL metrics for monitoring
    ///
    /// Returns detailed metrics including:
    /// - Current LSN and entry count
    /// - Size estimation
    /// - Oldest and newest entries
    /// - Sync mode
    pub fn metrics(&self) -> Result<WalMetrics> {
        let prefix = b"wal:entries:";
        let mut metrics = WalMetrics {
            current_lsn: self.current_lsn(),
            entry_count: 0,
            size_bytes: 0,
            oldest_lsn: 0,
            newest_lsn: 0,
            oldest_timestamp: 0,
            newest_timestamp: 0,
            sync_mode: self.sync_mode,
        };

        let iter = self.db.prefix_iterator(prefix);
        for item in iter {
            match item {
                Ok((key, value)) => {
                    if !key.starts_with(prefix) {
                        break;
                    }

                    metrics.entry_count += 1;
                    metrics.size_bytes += key.len() as u64 + value.len() as u64;

                    if let Ok(entry) = decode_entry(self.key_manager.as_deref(), &key, &value) {
                        if metrics.oldest_lsn == 0 {
                            metrics.oldest_lsn = entry.lsn;
                            metrics.oldest_timestamp = entry.timestamp;
                        }
                        metrics.newest_lsn = entry.lsn;
                        metrics.newest_timestamp = entry.timestamp;
                    }
                }
                Err(e) => {
                    warn!("Error reading entry for metrics: {}", e);
                }
            }
        }

        debug!(
            "WAL metrics: {} entries, {} bytes, LSN range {}-{}",
            metrics.entry_count, metrics.size_bytes, metrics.oldest_lsn, metrics.newest_lsn
        );

        Ok(metrics)
    }
}

// Legacy compatibility - keeping old enum for backward compatibility
/// WAL entry type (legacy)
#[derive(Debug, Clone)]
pub enum WalEntryLegacy {
    /// Put operation
    Put { key: Vec<u8>, value: Vec<u8> },
    /// Delete operation
    Delete { key: Vec<u8> },
    /// Transaction begin
    Begin { tx_id: u64 },
    /// Transaction commit
    Commit { tx_id: u64 },
    /// Transaction abort
    Abort { tx_id: u64 },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use rocksdb::Options;
    use tempfile::TempDir;

    fn create_test_db() -> (TempDir, Arc<DB>) {
        let temp_dir = TempDir::new().unwrap();
        let mut opts = Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, temp_dir.path()).unwrap();
        (temp_dir, Arc::new(db))
    }

    fn insert_op(n: u8) -> WalOperation {
        WalOperation::Insert {
            table: "t".to_string(),
            key: format!("data:t:{n}").into_bytes(),
            tuple: vec![n],
        }
    }

    /// A staged batch whose write FAILED must leave no history: nothing persisted,
    /// nothing published, and its LSNs become holes that do not stall later entries.
    #[test]
    fn abandoned_staged_entries_leave_no_history_and_do_not_stall_publication() {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync).unwrap();

        let mut lost = WriteBatch::default();
        let staged = wal.stage(vec![insert_op(1), insert_op(2)], &mut lost).unwrap().unwrap();
        // The caller's write fails: the batch is dropped unwritten.
        wal.abandon_staged(staged);
        drop(lost);
        assert!(wal.replay().unwrap().is_empty(), "nothing may have been persisted");

        // A later entry still publishes (the holes did not stall the watermark).
        let lsn = wal.append(insert_op(3)).unwrap();
        assert_eq!(lsn, 3);
        assert_eq!(wal.published_lsn(), 3);
        let entries = wal.replay().unwrap();
        assert_eq!(entries.iter().map(|e| e.lsn).collect::<Vec<_>>(), vec![3]);
    }

    fn retained_lsns(wal: &WriteAheadLog) -> Vec<u64> {
        wal.replay().unwrap().iter().map(|e| e.lsn).collect()
    }

    /// Without retention a checkpoint deletes everything up to it (the 4.41.1
    /// behaviour), and the history floor equals the checkpoint.
    #[test]
    fn checkpoint_without_retention_deletes_through_the_checkpoint() {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync).unwrap();
        for n in 1..=10 {
            wal.append(insert_op(n)).unwrap();
        }
        assert_eq!(wal.pruned_through().unwrap(), 0, "nothing deleted yet");
        assert_eq!(wal.truncate_to_checkpoint(10).unwrap(), 10);
        assert!(retained_lsns(&wal).is_empty());
        assert_eq!(wal.checkpoint_lsn().unwrap(), Some(10));
        assert_eq!(wal.pruned_through().unwrap(), 10);
    }

    /// Field J: with retention the newest entries under the checkpoint stay, the
    /// checkpoint (redo floor) still advances, and the history floor records only
    /// what was actually deleted and never moves backwards.
    #[test]
    fn checkpoint_with_retention_keeps_a_window_below_the_checkpoint() {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync)
            .unwrap()
            .with_retention(4);
        for n in 1..=10 {
            wal.append(insert_op(n)).unwrap();
        }
        assert_eq!(wal.truncate_to_checkpoint(10).unwrap(), 6);
        assert_eq!(retained_lsns(&wal), vec![7, 8, 9, 10]);
        assert_eq!(wal.checkpoint_lsn().unwrap(), Some(10));
        assert_eq!(wal.pruned_through().unwrap(), 6);
        // Open-time recovery replays only past the checkpoint: the window is history.
        assert!(wal.replay_from(10).unwrap().is_empty());

        // The window follows the checkpoint forward.
        for n in 11..=12 {
            wal.append(insert_op(n)).unwrap();
        }
        assert_eq!(wal.truncate_to_checkpoint(12).unwrap(), 2);
        assert_eq!(retained_lsns(&wal), vec![9, 10, 11, 12]);
        assert_eq!(wal.pruned_through().unwrap(), 8);

        // A checkpoint smaller than the window deletes nothing; the floor stays.
        let widened = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync)
            .unwrap()
            .with_retention(1000);
        assert_eq!(widened.truncate_to_checkpoint(12).unwrap(), 0);
        assert_eq!(
            widened.pruned_through().unwrap(),
            8,
            "the history floor never moves back"
        );
        assert_eq!(retained_lsns(&widened), vec![9, 10, 11, 12]);
    }

    /// A store checkpointed by 4.41.1 or earlier has no history-floor marker; it
    /// deleted everything up to its checkpoint, so that is its floor.
    #[test]
    fn missing_history_floor_marker_means_the_checkpoint() {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync).unwrap();
        for n in 1..=5 {
            wal.append(insert_op(n)).unwrap();
        }
        wal.truncate_to_checkpoint(5).unwrap();
        db.delete(WriteAheadLog::PRUNED_THROUGH_KEY).unwrap();
        assert_eq!(wal.pruned_through().unwrap(), 5);
        for n in 6..=8 {
            wal.append(insert_op(n)).unwrap();
        }
        // A retention-enabled checkpoint after upgrade keeps the new window but
        // cannot claim the old, already-deleted entries back.
        let upgraded = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync)
            .unwrap()
            .with_retention(100);
        upgraded.truncate_to_checkpoint(8).unwrap();
        assert_eq!(retained_lsns(&upgraded), vec![6, 7, 8]);
        assert_eq!(upgraded.pruned_through().unwrap(), 5);
    }

    /// Two committers finishing out of LSN order must not let the watermark (and so
    /// catch-up and the live feed) jump over the earlier, still-unresolved entry.
    #[test]
    fn publication_waits_for_the_lowest_unresolved_lsn() {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync).unwrap();

        let mut first = WriteBatch::default();
        let a = wal.stage(vec![insert_op(1)], &mut first).unwrap().unwrap();
        let mut second = WriteBatch::default();
        let b = wal.stage(vec![insert_op(2)], &mut second).unwrap().unwrap();

        // B (LSN 2) lands and is published first.
        db.write(second).unwrap();
        wal.publish_staged(b);
        assert_eq!(
            wal.published_lsn(),
            0,
            "LSN 2 must not be visible while LSN 1 is unresolved"
        );

        // A (LSN 1) lands: both become visible, in order.
        db.write(first).unwrap();
        wal.publish_staged(a);
        assert_eq!(wal.published_lsn(), 2);
    }

    /// Entries buffered in a statement-capture frame are written only if the owner
    /// takes them; a rolled-back statement (frame dropped) leaves no history.
    #[test]
    fn statement_capture_defers_appends_and_discards_on_rollback() {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync).unwrap();

        {
            let _frame = wal.begin_statement_capture(7);
            assert_eq!(wal.append(insert_op(1)).unwrap(), 0, "captured, not appended");
            assert_eq!(wal.append_nosync(insert_op(2)).unwrap(), 0);
            assert!(wal.replay().unwrap().is_empty());
            // Statement fails: the frame is dropped without being taken.
        }
        assert!(
            wal.replay().unwrap().is_empty(),
            "a rolled-back statement leaves no history"
        );
        assert_eq!(
            wal.append(insert_op(3)).unwrap(),
            1,
            "with no frame, appends are immediate again"
        );

        // The committing transaction takes the buffered operations.
        let frame = wal.begin_statement_capture(9);
        wal.append(insert_op(4)).unwrap();
        let (ops, wants_sync) = wal.take_statement_capture(9).unwrap();
        assert_eq!(ops.len(), 1);
        assert!(wants_sync, "a synchronous append asks for a synchronous commit write");
        assert!(
            wal.take_statement_capture(8).is_none(),
            "another transaction's frame is not taken"
        );
        drop(frame);
    }

    /// Data and its history entry are one write: a failed combined write leaves neither.
    #[test]
    fn append_with_batch_writes_data_and_entry_together() {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync).unwrap();
        db.put(b"data:t:1", b"row").unwrap();

        let mut batch = WriteBatch::default();
        batch.delete(b"data:t:1");
        let lsn = wal
            .append_with_batch(
                WalOperation::Delete {
                    table: "t".into(),
                    key: b"data:t:1".to_vec(),
                },
                batch,
                None,
            )
            .unwrap();
        assert_eq!(lsn, 1);
        assert!(db.get(b"data:t:1").unwrap().is_none());
        assert_eq!(wal.replay().unwrap().len(), 1);
        assert_eq!(wal.published_lsn(), 1);
    }

    #[test]
    fn test_wal_basic_operations() {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(db, WriteGate::open(), WalSyncMode::Sync).unwrap();

        // Append entries
        let lsn1 = wal
            .append(WalOperation::Insert {
                table: "users".to_string(),
                key: b"data:users:1".to_vec(),
                tuple: vec![1, 2, 3],
            })
            .unwrap();

        let lsn2 = wal
            .append(WalOperation::Delete {
                table: "users".to_string(),
                key: vec![4, 5, 6],
            })
            .unwrap();

        assert_eq!(lsn1, 1);
        assert_eq!(lsn2, 2);
        assert_eq!(wal.current_lsn(), 2);
    }

    #[test]
    fn test_wal_replay() {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync).unwrap();

        // Append some entries
        wal.append(WalOperation::CreateTable {
            table: "test".to_string(),
            schema: vec![7, 8, 9],
        })
        .unwrap();

        wal.append(WalOperation::Insert {
            table: "test".to_string(),
            key: b"data:test:1".to_vec(),
            tuple: vec![1, 2, 3],
        })
        .unwrap();

        // Replay
        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].lsn, 1);
        assert_eq!(entries[1].lsn, 2);
    }

    /// ROADMAP_V5.md §1.8: `append_batch` is the primitive
    /// `Transaction::commit_with_timestamp` uses to emit a whole explicit
    /// transaction at once. It must assign CONTIGUOUS LSNs (the property a
    /// future atomic-apply standby leans on instead of a per-row `tx_id`),
    /// persist every entry, and advance `wal:last_lsn` to the highest — all
    /// from ONE RocksDB write, so the batch pays ONE sync-wait rather than one
    /// per row.
    #[test]
    fn test_append_batch_assigns_contiguous_lsns_and_persists_all() {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Async).unwrap();

        // Pre-existing entry, so the batch does not start at LSN 1.
        wal.append(WalOperation::CreateTable {
            table: "batched".to_string(),
            schema: vec![1],
        })
        .unwrap();

        let ops = vec![
            WalOperation::Insert {
                table: "batched".to_string(),
                key: b"data:batched:1".to_vec(),
                tuple: vec![1],
            },
            WalOperation::Update {
                table: "batched".to_string(),
                key: b"data:batched:2".to_vec(),
                tuple: vec![2],
            },
            WalOperation::Delete {
                table: "batched".to_string(),
                key: b"data:batched:3".to_vec(),
            },
            WalOperation::UpdateCounter {
                table_name: "batched".to_string(),
                new_value: 3,
            },
        ];
        let lsns = wal.append_batch(ops, false).unwrap();

        assert_eq!(lsns, vec![2, 3, 4, 5], "batch LSNs must be contiguous");
        assert_eq!(wal.current_lsn(), 5);

        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 5, "every batched entry must be persisted");
        let ops: Vec<&WalOperation> = entries.iter().map(|e| &e.operation).collect();
        assert!(ops
            .iter()
            .any(|op| matches!(op, WalOperation::Insert { table, .. } if table == "batched")));
        assert!(ops
            .iter()
            .any(|op| matches!(op, WalOperation::Update { table, .. } if table == "batched")));
        assert!(ops
            .iter()
            .any(|op| matches!(op, WalOperation::Delete { table, .. } if table == "batched")));
        assert!(ops
            .iter()
            .any(|op| matches!(op, WalOperation::UpdateCounter { new_value, .. } if *new_value == 3)));

        // `wal:last_lsn` must reflect the highest LSN in the batch, so a
        // reopened WAL resumes after it rather than reusing LSNs.
        let reopened = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Async).unwrap();
        assert_eq!(reopened.current_lsn(), 5);
    }

    /// An empty batch (a transaction that staged nothing WAL-loggable) must
    /// not touch RocksDB, burn an LSN, or broadcast.
    #[test]
    fn test_append_batch_empty_is_a_noop() {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(db, WriteGate::open(), WalSyncMode::Async).unwrap();

        let lsns = wal.append_batch(Vec::new(), false).unwrap();
        assert!(lsns.is_empty());
        assert_eq!(wal.current_lsn(), 0);
        assert!(wal.replay().unwrap().is_empty());
    }

    #[test]
    fn test_wal_replay_tolerates_torn_tail() {
        // C13: a corrupt/partial record at the tail (LSN 3) must NOT abort the
        // whole replay — the valid prefix (LSN 1, 2) is kept and replay stops
        // at the first undecodable record.
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync).unwrap();

        wal.append(WalOperation::CreateTable {
            table: "t".to_string(),
            schema: vec![1],
        })
        .unwrap();
        wal.append(WalOperation::Insert {
            table: "t".to_string(),
            key: b"data:t:1".to_vec(),
            tuple: vec![9],
        })
        .unwrap();

        // Inject a torn record at the next LSN key: valid key, garbage value.
        let torn_key = format!("wal:entries:{:020}", 3);
        db.put(torn_key.as_bytes(), b"\xff\xff not a valid WalEntry \x00\x01")
            .unwrap();

        // Old behavior: `?` on the failed decode discarded everything and
        // returned Err. New behavior: keep the good prefix, stop at the tear.
        let entries = wal.replay().expect("replay must not error on a torn tail");
        assert_eq!(entries.len(), 2, "valid prefix must survive the torn tail");
        assert_eq!(entries[0].lsn, 1);
        assert_eq!(entries[1].lsn, 2);
    }

    #[test]
    fn test_wal_truncate() {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync).unwrap();

        // Append multiple entries
        for i in 0..5 {
            wal.append(WalOperation::Insert {
                table: "test".to_string(),
                key: format!("data:test:{}", i).into_bytes(),
                tuple: vec![i],
            })
            .unwrap();
        }

        // Truncate first 3 entries
        wal.truncate(3).unwrap();

        // Replay should only return entries 4 and 5
        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].lsn, 4);
        assert_eq!(entries[1].lsn, 5);
    }

    #[test]
    fn test_wal_recovery() {
        let (temp, db) = create_test_db();

        // Create WAL and append entries
        {
            let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync).unwrap();
            wal.append(WalOperation::Insert {
                table: "test".to_string(),
                key: b"data:test:1".to_vec(),
                tuple: vec![1, 2, 3],
            })
            .unwrap();
        }

        // "Crash" - drop the WAL
        // Reopen and check LSN is recovered
        let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), WalSyncMode::Sync).unwrap();
        assert_eq!(wal.current_lsn(), 1);

        // Next entry should have LSN 2
        let lsn = wal
            .append(WalOperation::Insert {
                table: "test".to_string(),
                key: b"data:test:2".to_vec(),
                tuple: vec![4, 5, 6],
            })
            .unwrap();
        assert_eq!(lsn, 2);
    }

    #[test]
    fn test_wal_sync_modes() {
        let (_temp, db) = create_test_db();

        // Test all sync modes
        for mode in [WalSyncMode::Sync, WalSyncMode::Async, WalSyncMode::GroupCommit] {
            let wal = WriteAheadLog::open(Arc::clone(&db), WriteGate::open(), mode).unwrap();
            assert_eq!(wal.sync_mode(), mode);

            let lsn = wal
                .append(WalOperation::Insert {
                    table: "test".to_string(),
                    key: b"data:test:1".to_vec(),
                    tuple: vec![1, 2, 3],
                })
                .unwrap();
            assert!(lsn > 0);

            wal.flush().unwrap();
        }
    }
}
