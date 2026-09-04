//! Time-Travel Query Support
//!
//! Implements AS OF TIMESTAMP/TRANSACTION/SCN queries for point-in-time database access.
//!
//! This module provides:
//! - Snapshot metadata storage and management
//! - Timestamp-to-snapshot mapping
//! - Transaction-ID-to-snapshot mapping
//! - SCN (System Change Number) tracking
//! - Historical snapshot creation and query execution
//! - Snapshot garbage collection
//!
//! ## Performance Characteristics
//!
//! - AS OF queries have <2x overhead vs current time queries
//! - Snapshot metadata is stored in-memory for fast lookups
//! - Historical versions are stored in RocksDB with efficient key encoding
//! - GC runs periodically to clean up old snapshots
//!
//! ## Key Encoding
//!
//! - Version keys: `v:{table}:{row_id}:{timestamp}`
//! - Snapshot metadata: `snapshot:{timestamp}`
//! - Transaction mapping: `txn_map:{txn_id}`
//! - SCN mapping: `scn_map:{scn}`

use super::tde;
use crate::crypto::KeyManager;
use crate::{Error, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use lru::LruCache;
use parking_lot::{Mutex, RwLock};
use rocksdb::{WriteBatch, WriteOptions, DB};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// System Change Number (Oracle-compatible)
pub type Scn = u64;

/// Transaction ID
pub type TransactionId = u64;

/// W3.2: durable sentinel key. Written once (per DB) the first time elision is
/// enabled, so a later reopen with `elide_latest_version = false` still knows a
/// prior session may have left flagged rows and keeps the materialize-before-
/// overwrite gate armed. Point-read/written only; sorts after every `v*`/`vmeta:`
/// /`vgc:` range so no version scan touches it.
const ELIDE_SENTINEL_KEY: &[u8] = b"w3_2_elide_used";

/// W3.2: reserved high bit of a `v_idx:` event's 8-byte big-endian value (the
/// event's `commit_ts`). SET ⇒ the row version for this event is ELIDED — no
/// `v:` key was written and the value equals the current `data:` row (this is
/// the latest version, not yet materialized by a mutation). CLEAR ⇒ a `v:` key
/// is present, resolved exactly as before W3.2.
///
/// EVERY legacy/pre-W3.2 value has this bit clear (a `commit_ts` is a logical or
/// epoch-micros counter far below 2^62), so an unflagged value decodes as
/// "flag CLEAR = `v:` present" — the forever-fallback that keeps pre-W3.2
/// databases correct (`W3_2_DESIGN.md` §4). NEVER remove that default. Bit 63 is
/// free for ~292k years of epoch-micros (2^63 µs), so it can never collide with
/// a real timestamp.
const VERSION_VALUE_ELIDED_FLAG: u64 = 1 << 63;

/// W3.2: encode a `v_idx:` event value — the 8-byte big-endian `commit_ts`, with
/// the high bit SET when the `v:` copy was elided. `commit_ts` MUST be below
/// 2^63 (always true for logical/epoch-micros timestamps), else the flag bit
/// would corrupt the timestamp.
#[inline]
fn encode_version_index_value(commit_ts: u64, elided: bool) -> [u8; 8] {
    let raw = if elided {
        commit_ts | VERSION_VALUE_ELIDED_FLAG
    } else {
        commit_ts
    };
    raw.to_be_bytes()
}

/// W3.2: decode a `v_idx:` event value into `(commit_ts, elided)`. A value
/// shorter than 8 bytes yields `None`. A value with the high bit clear — which
/// includes every legacy pre-W3.2 value — decodes as `elided = false`.
#[inline]
fn decode_version_index_value(bytes: &[u8]) -> Option<(u64, bool)> {
    let raw = u64::from_be_bytes(bytes.get(0..8)?.try_into().ok()?);
    let elided = raw & VERSION_VALUE_ELIDED_FLAG != 0;
    Some((raw & !VERSION_VALUE_ELIDED_FLAG, elided))
}

/// Snapshot metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    /// Snapshot timestamp (also serves as snapshot ID)
    pub timestamp: u64,
    /// Transaction ID that created this snapshot
    pub transaction_id: TransactionId,
    /// System Change Number
    pub scn: Scn,
    /// W2.2(b): wall-clock creation time as microseconds since the Unix epoch.
    /// Formerly an `Utc::now().to_rfc3339()` `String` — a per-DML heap
    /// allocation + calendar format on the version-write path. Databases
    /// written before this change stored the RFC3339 string; `SnapshotMetadataLegacy`
    /// + `deserialize_snapshot_metadata` read those forever (never remove that
    /// fallback). Display surfaces reconstruct RFC3339 via `wall_clock_rfc3339`.
    pub wall_clock_micros: i64,
    /// Number of active transactions at snapshot time
    pub active_transactions: u64,
    /// Whether this snapshot can be garbage collected
    pub gc_eligible: bool,
}

/// W2.2(b): legacy on-disk layout — `wall_clock_time` was an RFC3339 `String`.
/// Only used by `deserialize_snapshot_metadata` as a fallback when the current
/// (epoch-micros) layout fails to decode. NEVER remove: pre-W2.2 databases keep
/// this format on disk forever.
#[derive(Deserialize)]
struct SnapshotMetadataLegacy {
    timestamp: u64,
    transaction_id: TransactionId,
    scn: Scn,
    wall_clock_time: String,
    active_transactions: u64,
    gc_eligible: bool,
}

impl From<SnapshotMetadataLegacy> for SnapshotMetadata {
    fn from(legacy: SnapshotMetadataLegacy) -> Self {
        let wall_clock_micros = DateTime::parse_from_rfc3339(&legacy.wall_clock_time)
            .map(|dt| dt.timestamp_micros())
            .unwrap_or(0);
        SnapshotMetadata {
            timestamp: legacy.timestamp,
            transaction_id: legacy.transaction_id,
            scn: legacy.scn,
            wall_clock_micros,
            active_transactions: legacy.active_transactions,
            gc_eligible: legacy.gc_eligible,
        }
    }
}

/// W2.2(b): decode persisted `snapshot:` metadata, tolerating both the current
/// epoch-micros layout and the legacy RFC3339-string layout forever.
///
/// bincode is not self-describing, so the two layouts differ on the wire (an
/// `i64` micros field vs a length-prefixed `String`). We try the current layout
/// first; a legacy record fails it deterministically — the string's length
/// prefix is consumed as the `i64`, shifting the trailing `gc_eligible` bool
/// onto an ASCII digit of the timestamp (`'0'..='3'`), which bincode's bool
/// decoder rejects — so we fall back to the legacy layout and convert. The
/// reverse can never misfire: a real micros value read as a `String` length
/// asks for petabytes and overruns the buffer.
fn deserialize_snapshot_metadata(bytes: &[u8]) -> Option<SnapshotMetadata> {
    if let Ok(metadata) = bincode::deserialize::<SnapshotMetadata>(bytes) {
        return Some(metadata);
    }
    bincode::deserialize::<SnapshotMetadataLegacy>(bytes)
        .ok()
        .map(SnapshotMetadata::from)
}

impl SnapshotMetadata {
    /// Create a new snapshot metadata
    pub fn new(timestamp: u64, transaction_id: TransactionId, scn: Scn) -> Self {
        Self {
            timestamp,
            transaction_id,
            scn,
            wall_clock_micros: Utc::now().timestamp_micros(),
            active_transactions: 0,
            gc_eligible: true,
        }
    }

    /// W2.2(b): wall-clock creation time truncated to whole Unix seconds — the
    /// granularity every AS-OF/GC comparison site already used.
    pub fn wall_clock_unix_secs(&self) -> i64 {
        self.wall_clock_micros / 1_000_000
    }

    /// W2.2(b): wall-clock creation time rendered as RFC3339 for display
    /// surfaces (REPL `\snapshots`, `pg`/system-view timestamp columns) that
    /// previously read the raw string field. Epoch fallback on an out-of-range
    /// micros value (corruption only).
    pub fn wall_clock_rfc3339(&self) -> String {
        DateTime::from_timestamp_micros(self.wall_clock_micros)
            .unwrap_or_default()
            .to_rfc3339()
    }
}

/// Snapshot cache key: (table_name, row_id, snapshot_ts)
type SnapshotCacheKey = (String, u64, u64);

/// Snapshot Manager
///
/// Manages historical snapshots for time-travel queries.
pub struct SnapshotManager {
    /// Database handle
    db: Arc<DB>,
    /// In-memory snapshot registry for fast lookups
    snapshots: Arc<RwLock<HashMap<u64, SnapshotMetadata>>>,
    /// Transaction ID to timestamp mapping
    txn_to_timestamp: Arc<RwLock<HashMap<TransactionId, u64>>>,
    /// SCN to timestamp mapping
    scn_to_timestamp: Arc<RwLock<HashMap<Scn, u64>>>,
    /// Current SCN counter.
    /// W2.2(d): a lock-free `AtomicU64` — every DML allocated an SCN under a
    /// `RwLock<u64>` write lock, one of ~6 SnapshotManager lock acquisitions
    /// per version write and a global serialization point at high concurrency.
    current_scn: Arc<AtomicU64>,
    /// Current transaction ID counter (W2.2(d): lock-free `AtomicU64`, see above).
    current_txn_id: Arc<AtomicU64>,
    /// Snapshot read cache for performance. W2.2(c): each entry carries the
    /// per-table generation it was computed at (`snapshot_cache_gen`); a stale
    /// entry is rejected on read and overwritten, replacing the former
    /// per-write O(cache-size) linear scan of `invalidate_cache_for_row`.
    snapshot_cache: Arc<Mutex<LruCache<SnapshotCacheKey, (u64, Option<Vec<u8>>)>>>,
    /// W2.2(c): per-table snapshot-cache generation counters. A committed write
    /// to a table bumps its counter (O(1)); reads compare the cached entry's
    /// stamp and lazily evict on mismatch. Per-table (not per-row) is a safe
    /// over-approximation — over-invalidation only recomputes, never serves
    /// stale data. Not shared across `EmbeddedDatabase` instances (owned here,
    /// beside the cache it guards).
    snapshot_cache_gen: Arc<dashmap::DashMap<String, AtomicU64>>,
    /// Cache configuration
    cache_config: CacheConfig,
    /// GC configuration
    gc_config: GcConfig,
    /// Use non-durable RocksDB writes for memory-only databases.
    non_durable_writes: bool,
    /// Persist snapshot metadata keys. Memory-only databases keep this metadata
    /// in process and do not need it for recovery.
    persist_metadata: bool,
    /// R4.3: version-GC low watermark — the highest GC horizon ever applied
    /// (persisted as `vgc:low_watermark` by the collector). Historical reads
    /// (`AS OF` / `VERSIONS BETWEEN` starts / branch anchors) BELOW this
    /// timestamp fail with a clear error instead of reconstructing state
    /// from a partially collected version chain. `0` = GC never ran.
    gc_low_watermark: AtomicU64,
    /// Transient COPY batch version markers (next-batch item #2). A COPY fast
    /// batch elides per-row `v:`/`v_idx:` in favor of one durable `vmeta:`
    /// range marker; this in-memory interval index answers AS-OF visibility
    /// and drives materialization on the first UPDATE/DELETE of a covered row.
    /// Guarded by an internal atomic fast-out (one relaxed load when empty).
    copy_markers: crate::storage::copy_marker::CopyMarkers,
    /// W3.2: whether NEW main-branch inserts elide the `v:` copy and write a
    /// flagged `v_idx:` event instead (`storage.elide_latest_version`). Set at
    /// open via [`configure_elision`]. Old flagged rows stay flagged when this
    /// is toggled off — the read/materialize paths key off each row's own
    /// durable flag, not this switch.
    elide_latest_version: AtomicBool,
    /// W3.2: whether any elided (flagged) `v_idx:` event might exist in this
    /// database — `true` when elision is enabled now OR the durable
    /// [`ELIDE_SENTINEL_KEY`] was found at open (a prior session may have left
    /// flagged rows even though elision is off now). Gates the
    /// materialize-before-overwrite seek on the mutation hot path: `false` ⇒ one
    /// relaxed atomic load and skip, because no flagged row can exist.
    maybe_elided_rows: AtomicBool,
    /// TDE key manager, mirroring `StorageEngine::key_manager`.
    ///
    /// This type writes `data:` and `v:` values straight to the shared
    /// `Arc<DB>` — including the DEFAULT autocommit INSERT's only `data:`
    /// write (`write_data_version_and_register_snapshot`) — so it must apply
    /// the same storage-boundary rule the engine's `put` applies. Reads have
    /// the matching obligation: `read_at_snapshot`, `read_at_snapshot_linear`
    /// and `scan_versions_between` decode before returning.
    ///
    /// `None` (the default configuration) makes every use a single `Option`
    /// check with no allocation and no copy.
    key_manager: Option<Arc<KeyManager>>,
}

/// Snapshot cache configuration
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Maximum number of cached snapshot entries
    pub max_entries: usize,
    /// Whether to enable snapshot caching
    pub enabled: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            enabled: true,
        }
    }
}

/// Garbage collection configuration
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// Minimum retention period (seconds)
    pub min_retention_seconds: u64,
    /// Maximum number of snapshots to keep
    pub max_snapshots: usize,
    /// Whether to enable automatic GC
    pub auto_gc_enabled: bool,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            min_retention_seconds: 3600, // 1 hour
            max_snapshots: 1000,
            auto_gc_enabled: true,
        }
    }
}

impl SnapshotManager {
    /// Create a new snapshot manager
    pub fn new(db: Arc<DB>) -> Self {
        let cache_config = CacheConfig::default();
        let cache_size = NonZeroUsize::new(cache_config.max_entries)
            .unwrap_or_else(|| NonZeroUsize::new(1000).unwrap_or(NonZeroUsize::MIN));

        Self {
            db,
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            txn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            scn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            current_scn: Arc::new(AtomicU64::new(1)),
            current_txn_id: Arc::new(AtomicU64::new(1)),
            snapshot_cache: Arc::new(Mutex::new(LruCache::new(cache_size))),
            snapshot_cache_gen: Arc::new(dashmap::DashMap::new()),
            cache_config,
            gc_config: GcConfig::default(),
            non_durable_writes: false,
            gc_low_watermark: AtomicU64::new(0),
            persist_metadata: true,
            copy_markers: crate::storage::copy_marker::CopyMarkers::new(),
            elide_latest_version: AtomicBool::new(false),
            maybe_elided_rows: AtomicBool::new(false),
            key_manager: None,
        }
    }

    /// Create a snapshot manager for memory-only databases.
    pub fn new_non_durable(db: Arc<DB>) -> Self {
        let mut manager = Self::new(db);
        manager.non_durable_writes = true;
        manager.persist_metadata = false;
        manager
    }

    /// Create a new snapshot manager with custom GC config
    pub fn with_gc_config(db: Arc<DB>, gc_config: GcConfig) -> Self {
        let cache_config = CacheConfig::default();
        let cache_size = NonZeroUsize::new(cache_config.max_entries)
            .unwrap_or_else(|| NonZeroUsize::new(1000).unwrap_or(NonZeroUsize::MIN));

        Self {
            db,
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            txn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            scn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            current_scn: Arc::new(AtomicU64::new(1)),
            current_txn_id: Arc::new(AtomicU64::new(1)),
            snapshot_cache: Arc::new(Mutex::new(LruCache::new(cache_size))),
            snapshot_cache_gen: Arc::new(dashmap::DashMap::new()),
            cache_config,
            gc_config,
            non_durable_writes: false,
            gc_low_watermark: AtomicU64::new(0),
            persist_metadata: true,
            copy_markers: crate::storage::copy_marker::CopyMarkers::new(),
            elide_latest_version: AtomicBool::new(false),
            maybe_elided_rows: AtomicBool::new(false),
            key_manager: None,
        }
    }

    /// Create a new snapshot manager with custom cache and GC config
    pub fn with_config(db: Arc<DB>, cache_config: CacheConfig, gc_config: GcConfig) -> Self {
        let cache_size = NonZeroUsize::new(cache_config.max_entries)
            .unwrap_or_else(|| NonZeroUsize::new(1000).unwrap_or(NonZeroUsize::MIN));

        Self {
            db,
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            txn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            scn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            current_scn: Arc::new(AtomicU64::new(1)),
            current_txn_id: Arc::new(AtomicU64::new(1)),
            snapshot_cache: Arc::new(Mutex::new(LruCache::new(cache_size))),
            snapshot_cache_gen: Arc::new(dashmap::DashMap::new()),
            cache_config,
            gc_config,
            non_durable_writes: false,
            gc_low_watermark: AtomicU64::new(0),
            persist_metadata: true,
            copy_markers: crate::storage::copy_marker::CopyMarkers::new(),
            elide_latest_version: AtomicBool::new(false),
            maybe_elided_rows: AtomicBool::new(false),
            key_manager: None,
        }
    }

    /// Attach the engine's TDE key manager so the version chain and the fast
    /// autocommit `data:` write are sealed by the same rule `StorageEngine::put`
    /// applies (see the `key_manager` field docs).
    ///
    /// Called at engine construction BEFORE the manager is wrapped in an `Arc`
    /// and published, which is why it can take `&mut self` and why no
    /// synchronization is needed for the field afterwards.
    ///
    /// MUST be called for every manager built against an encryption-enabled
    /// engine. Without it the MVCC version chain is a plaintext copy of every
    /// row on a database whose live rows are ciphertext.
    pub fn set_key_manager(&mut self, key_manager: Option<Arc<KeyManager>>) {
        self.key_manager = key_manager;
    }

    /// Seal a `data:`/`v:` value for storage. `Cow::Borrowed` — no allocation,
    /// no copy — when encryption is disabled.
    #[inline]
    fn seal_stored<'a>(&self, value: &'a [u8]) -> Result<Cow<'a, [u8]>> {
        tde::seal(self.key_manager.as_deref(), value)
    }

    /// Decode a `data:`/`v:` value this manager read straight from RocksDB.
    /// The bytes are returned unmoved when encryption is disabled.
    #[inline]
    fn decode_stored_opt(&self, storage_key: &[u8], raw: Option<Vec<u8>>) -> Result<Option<Vec<u8>>> {
        tde::open_opt(self.key_manager.as_deref(), storage_key, raw)
    }

    /// W3.2: wire the `storage.elide_latest_version` knob at open (called from
    /// `StorageEngine` construction after `recover_snapshots`). Loads the durable
    /// [`ELIDE_SENTINEL_KEY`] so a reopen with elision OFF still arms the
    /// materialize gate for rows a prior session left flagged, and — when
    /// elision is ON — persists the sentinel BEFORE any flagged insert so every
    /// flagged row is preceded by a durable "elision used" marker (crash-safe:
    /// the sentinel rides the WAL ahead of the inserts it gates).
    ///
    /// If that sentinel write FAILS on a durable DB, elision is disabled for this
    /// session instead of proceeding: a flagged insert with no durable sentinel
    /// would be silently unprotected on a later OFF-reopen (which finds no
    /// sentinel, leaves the materialize gate disarmed, and loses the row's AS-OF
    /// value on its next overwrite). Falling back to full `v:` is correct — it
    /// only forgoes the space saving.
    ///
    /// State is per-`SnapshotManager` (one per engine, published via `Arc` before
    /// any concurrent access, so these Relaxed stores are safe), NOT process-global.
    pub fn configure_elision(&self, elide_latest_version: bool) {
        self.elide_latest_version.store(elide_latest_version, Ordering::Relaxed);

        let sentinel_present = matches!(self.db.get(ELIDE_SENTINEL_KEY), Ok(Some(_)));
        if sentinel_present {
            self.maybe_elided_rows.store(true, Ordering::Relaxed);
        }
        if elide_latest_version {
            // Persist the sentinel eagerly (idempotent) so it is durable ahead of
            // the first flagged insert. Memory-only DBs (persist_metadata=false)
            // never reopen, so the sentinel is unnecessary and the in-process flag
            // alone suffices.
            let sentinel_durable = if sentinel_present || !self.persist_metadata {
                true
            } else {
                match self.db.put(ELIDE_SENTINEL_KEY, [1u8]) {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!(
                            "Warning: W3.2 failed to persist elision sentinel; disabling elision \
                             this session to avoid unprotected flagged rows: {}",
                            e
                        );
                        false
                    }
                }
            };
            if sentinel_durable {
                self.maybe_elided_rows.store(true, Ordering::Relaxed);
            } else {
                // No durable sentinel ⇒ do not create flagged inserts a later
                // OFF-reopen could not protect. Fall back to full `v:`.
                self.elide_latest_version.store(false, Ordering::Relaxed);
            }
        }
    }

    /// W3.2: whether NEW main-branch inserts elide the `v:` copy (config knob).
    pub(crate) fn elide_latest_version(&self) -> bool {
        self.elide_latest_version.load(Ordering::Relaxed)
    }

    /// W3.2: whether any flagged (elided) row might exist — the mutation-path
    /// materialize gate. `false` ⇒ skip the per-row `v_idx:` seek entirely.
    pub(crate) fn maybe_elided_rows(&self) -> bool {
        self.maybe_elided_rows.load(Ordering::Relaxed)
    }

    fn write_batch(&self, batch: WriteBatch, context: &str) -> Result<()> {
        if self.non_durable_writes {
            let mut opts = WriteOptions::default();
            opts.set_sync(false);
            opts.disable_wal(true);
            self.db
                .write_opt(batch, &opts)
                .map_err(|e| Error::storage(format!("{}: {}", context, e)))
        } else {
            self.db
                .write(batch)
                .map_err(|e| Error::storage(format!("{}: {}", context, e)))
        }
    }

    /// Register a new snapshot
    ///
    /// This should be called every time a transaction commits to track
    /// the snapshot state at that point in time.
    pub fn register_snapshot(&self, timestamp: u64) -> Result<SnapshotMetadata> {
        let txn_id = self.next_transaction_id();
        self.register_snapshot_internal(timestamp, txn_id)
    }

    /// Register a new snapshot with a specific transaction/LSN ID
    ///
    /// This allows the caller to specify the transaction ID (e.g., WAL LSN)
    /// which enables AS OF TRANSACTION queries to use the same IDs that
    /// users see in the REPL.
    pub fn register_snapshot_with_lsn(&self, timestamp: u64, lsn: u64) -> Result<SnapshotMetadata> {
        // Update our internal counter to stay ahead of externally provided LSNs.
        // W2.2(d): fetch_max is the atomic equivalent of the former
        // `if lsn >= *txn_id { *txn_id = lsn + 1 }` guarded assignment.
        self.current_txn_id.fetch_max(lsn + 1, Ordering::Relaxed);
        self.register_snapshot_internal(timestamp, lsn)
    }

    /// Internal snapshot registration
    fn register_snapshot_internal(&self, timestamp: u64, txn_id: TransactionId) -> Result<SnapshotMetadata> {
        let scn = self.next_scn();

        let metadata = SnapshotMetadata::new(timestamp, txn_id, scn);

        // Store in-memory
        self.snapshots.write().insert(timestamp, metadata.clone());
        self.txn_to_timestamp.write().insert(txn_id, timestamp);
        self.scn_to_timestamp.write().insert(scn, timestamp);

        if self.persist_metadata {
            self.persist_snapshot_metadata(&metadata)?;
        }

        // Run GC if enabled
        if self.gc_config.auto_gc_enabled {
            if let Err(e) = self.gc_if_needed() {
                eprintln!("Warning: Snapshot GC failed: {}", e);
            }
        }

        Ok(metadata)
    }

    /// Get next transaction ID
    fn next_transaction_id(&self) -> TransactionId {
        // W2.2(d): lock-free increment; fetch_add returns the pre-increment value.
        self.current_txn_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Get next SCN
    fn next_scn(&self) -> Scn {
        // W2.2(d): lock-free increment; fetch_add returns the pre-increment value.
        self.current_scn.fetch_add(1, Ordering::Relaxed)
    }

    /// Resolve AS OF clause to a timestamp
    ///
    /// Converts TIMESTAMP/TRANSACTION/SCN to a snapshot timestamp.
    /// For VersionsBetween, this returns an error - use scan_versions_between directly.
    pub fn resolve_as_of(&self, as_of: &crate::sql::logical_plan::AsOfClause) -> Result<u64> {
        use crate::sql::logical_plan::AsOfClause;

        match as_of {
            AsOfClause::Now => {
                // Get current timestamp
                Ok(self.get_current_timestamp())
            }
            // R4.3: every explicit historical anchor must be at or above
            // the version-GC low watermark — below it the version chain may
            // be partially collected and the answer would be wrong.
            AsOfClause::Timestamp(ts_str) => {
                let ts = self.resolve_timestamp(ts_str)?;
                self.check_gc_horizon(ts)?;
                Ok(ts)
            }
            AsOfClause::Transaction(txn_id) => {
                let ts = self.resolve_transaction(*txn_id)?;
                self.check_gc_horizon(ts)?;
                Ok(ts)
            }
            AsOfClause::Scn(scn) => {
                let ts = self.resolve_scn(*scn)?;
                self.check_gc_horizon(ts)?;
                Ok(ts)
            }
            AsOfClause::VersionsBetween { .. } => {
                // VersionsBetween cannot be resolved to a single timestamp
                // The executor should handle this variant separately
                Err(Error::query_execution(
                    "VERSIONS BETWEEN cannot be resolved to a single timestamp. Use scan_versions_between instead.",
                ))
            }
            AsOfClause::Commit(sha) => {
                // AS OF COMMIT queries are handled by the CommitTracker in git_integration
                // The executor should handle this variant separately
                Err(Error::query_execution(format!(
                    "AS OF COMMIT '{}' should be resolved by the CommitTracker. Use git_integration::CommitTracker::get_snapshot_for_commit() instead.",
                    sha
                )))
            }
        }
    }

    /// Resolve timestamp string to snapshot timestamp
    fn resolve_timestamp(&self, ts_str: &str) -> Result<u64> {
        // Accept RFC3339, `YYYY-MM-DD HH:MM:SS[.fff][±HH:MM]` and the `T`
        // variant. An offset-less literal is interpreted as UTC — stated in
        // the error below, because a client in UTC−3 asking for its local
        // 18:30 is asking for 21:30 UTC, which may be in the future.
        let target_time = if let Ok(dt) = DateTime::parse_from_rfc3339(ts_str) {
            dt.timestamp() as u64
        } else if let Ok(dt) = DateTime::parse_from_str(ts_str, "%Y-%m-%d %H:%M:%S%.f%:z")
            .or_else(|_| DateTime::parse_from_str(ts_str, "%Y-%m-%d %H:%M:%S%:z"))
        {
            dt.timestamp() as u64
        } else {
            let dt = NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%d %H:%M:%S%.f")
                .or_else(|_| NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%d %H:%M:%S"))
                .or_else(|_| NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%S%.f"))
                .or_else(|_| NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%S"))
                .map_err(|e| Error::query_execution(format!("Invalid timestamp format: {}", e)))?;
            dt.and_utc().timestamp() as u64
        };

        // Find the closest snapshot <= target time
        let snapshots = self.snapshots.read();
        let mut best_match: Option<u64> = None;
        let mut best_diff = u64::MAX;

        for metadata in snapshots.values() {
            // W2.2(b): compare on epoch-micros metadata (seconds granularity, as before).
            let snap_timestamp = metadata.wall_clock_unix_secs() as u64;
            if snap_timestamp <= target_time {
                let diff = target_time - snap_timestamp;
                if diff < best_diff || (diff == best_diff && best_match.is_none_or(|best| metadata.timestamp > best)) {
                    best_diff = diff;
                    best_match = Some(metadata.timestamp);
                }
            }
        }

        best_match.ok_or_else(|| {
            // Say WHY: the instant the literal was read as, and what exists.
            // "No snapshot found for timestamp" gave the user nothing to act on.
            let mut secs: Vec<i64> = snapshots.values().map(|m| m.wall_clock_unix_secs() as i64).collect();
            secs.sort_unstable();
            let fmt = |s: i64| {
                chrono::DateTime::from_timestamp(s, 0)
                    .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                    .unwrap_or_else(|| s.to_string())
            };
            let target = fmt(target_time as i64);
            match (secs.first(), secs.last()) {
                (Some(&lo), Some(&hi)) => Error::query_execution(format!(
                    "No snapshot at or before {target} (literal '{ts_str}' interpreted as UTC unless it \
                     carries an offset); earliest snapshot {}, latest {} ({} snapshots). Use an explicit \
                     offset such as '… -03:00', a later timestamp, or AS OF TRANSACTION <id>.",
                    fmt(lo),
                    fmt(hi),
                    secs.len()
                )),
                _ => Error::query_execution(format!(
                    "No snapshot at or before {target} (literal '{ts_str}'): this database has no \
                     time-travel snapshots yet — time travel may be disabled for this profile, or no \
                     write has been snapshotted. Use AS OF NOW or AS OF TRANSACTION <id>."
                )),
            }
        })
    }

    /// Resolve timestamp for VERSIONS BETWEEN range queries
    ///
    /// Returns internal LSN timestamp for use in version range queries.
    /// For timestamps, finds the nearest snapshot or uses boundary values.
    ///
    /// R4.3: a range START below the version-GC low watermark errors — the
    /// versions in that part of the range may have been collected, and a
    /// silently partial history is worse than a clear error.
    pub fn resolve_timestamp_for_range(
        &self,
        as_of: &crate::sql::logical_plan::AsOfClause,
        is_start: bool,
    ) -> Result<u64> {
        let ts = self.resolve_timestamp_for_range_inner(as_of, is_start)?;
        if is_start {
            self.check_gc_horizon(ts)?;
        }
        Ok(ts)
    }

    fn resolve_timestamp_for_range_inner(
        &self,
        as_of: &crate::sql::logical_plan::AsOfClause,
        is_start: bool,
    ) -> Result<u64> {
        use crate::sql::logical_plan::AsOfClause;

        match as_of {
            AsOfClause::Now => {
                // For NOW, use the maximum timestamp (current)
                Ok(self.get_current_timestamp())
            }
            AsOfClause::Timestamp(ts_str) => {
                // Parse the target timestamp
                let target_time = if let Ok(dt) = DateTime::parse_from_rfc3339(ts_str) {
                    dt.timestamp() as u64
                } else {
                    let dt = NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%d %H:%M:%S")
                        .or_else(|_| NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%S"))
                        .map_err(|e| Error::query_execution(format!("Invalid timestamp format: {}", e)))?;

                    dt.and_utc().timestamp() as u64
                };

                // Search through snapshots to find matching LSN
                let snapshots = self.snapshots.read();

                if snapshots.is_empty() {
                    // No snapshots - use boundary values for full range
                    return Ok(if is_start { 0 } else { u64::MAX });
                }

                // Find appropriate snapshot based on whether this is start or end
                let mut best_match: Option<u64> = None;

                for metadata in snapshots.values() {
                    // W2.2(b): epoch-micros metadata, seconds granularity as before.
                    let snap_ts_seconds = metadata.wall_clock_unix_secs() as u64;

                    if is_start {
                        // For start: find earliest snapshot >= target
                        if snap_ts_seconds >= target_time {
                            match best_match {
                                Some(best) if metadata.timestamp < best => {
                                    best_match = Some(metadata.timestamp);
                                }
                                None => {
                                    best_match = Some(metadata.timestamp);
                                }
                                _ => {}
                            }
                        }
                    } else {
                        // For end: find latest snapshot <= target
                        if snap_ts_seconds <= target_time {
                            match best_match {
                                Some(best) if metadata.timestamp > best => {
                                    best_match = Some(metadata.timestamp);
                                }
                                None => {
                                    best_match = Some(metadata.timestamp);
                                }
                                _ => {}
                            }
                        }
                    }
                }

                // If no matching snapshot found, use boundary values
                Ok(best_match.unwrap_or(if is_start { 0 } else { u64::MAX }))
            }
            AsOfClause::Transaction(txn_id) => self.resolve_transaction(*txn_id),
            AsOfClause::Scn(scn) => self.resolve_scn(*scn),
            AsOfClause::VersionsBetween { .. } => Err(Error::query_execution(
                "Cannot resolve VersionsBetween to a single timestamp",
            )),
            AsOfClause::Commit(sha) => {
                // AS OF COMMIT queries should be handled by git_integration::CommitTracker
                Err(Error::query_execution(format!(
                    "AS OF COMMIT '{}' should be resolved via git_integration::CommitTracker",
                    sha
                )))
            }
        }
    }

    /// Resolve transaction ID to snapshot timestamp
    fn resolve_transaction(&self, txn_id: TransactionId) -> Result<u64> {
        self.txn_to_timestamp.read().get(&txn_id).copied().ok_or_else(|| {
            Error::query_execution(format!(
                "Transaction {} not found or has been garbage collected",
                txn_id
            ))
        })
    }

    /// Resolve SCN to snapshot timestamp
    pub fn resolve_scn(&self, scn: Scn) -> Result<u64> {
        self.scn_to_timestamp
            .read()
            .get(&scn)
            .copied()
            .ok_or_else(|| Error::query_execution(format!("SCN {} not found or has been garbage collected", scn)))
    }

    /// Get current timestamp
    fn get_current_timestamp(&self) -> u64 {
        // Get the latest snapshot timestamp
        self.snapshots.read().values().map(|m| m.timestamp).max().unwrap_or(1)
    }

    /// R4.3: set the version-GC low watermark (highest horizon ever applied).
    /// Monotonic: never moves backwards.
    pub fn set_gc_low_watermark(&self, ts: u64) {
        self.gc_low_watermark.fetch_max(ts, Ordering::SeqCst);
    }

    /// R4.3: current version-GC low watermark (0 = GC never ran).
    pub fn gc_low_watermark(&self) -> u64 {
        self.gc_low_watermark.load(Ordering::SeqCst)
    }

    /// R4.3: reject historical reads below the version-GC low watermark.
    /// Reads AT the watermark are exact (the collector always keeps the
    /// newest version at-or-below the horizon for every row); reads below
    /// it could silently reconstruct from a pruned chain, so they error.
    pub fn check_gc_horizon(&self, ts: u64) -> Result<()> {
        let watermark = self.gc_low_watermark();
        if ts < watermark {
            return Err(Error::query_execution(format!(
                "historical read at timestamp {} is older than the version-GC low watermark {}: \
                 versions beyond the configured storage.version_retention have been garbage \
                 collected (increase version_retention to keep more history)",
                ts, watermark
            )));
        }
        Ok(())
    }

    /// R4.3: newest recovered/registered snapshot timestamp, if any.
    /// Used to seed the engine's logical timestamp counter at startup so
    /// timestamps stay monotonic across reopens.
    pub fn max_snapshot_timestamp(&self) -> Option<u64> {
        self.snapshots.read().keys().max().copied()
    }

    /// R4.3: largest snapshot timestamp whose wall-clock registration time
    /// is at or before `cutoff_unix_secs`. This is the snapshot-metadata
    /// half of the retention horizon's wall-clock → logical-ts mapping (the
    /// version GC's persisted time anchors are the other half).
    pub fn max_snapshot_ts_at_or_before_wallclock(&self, cutoff_unix_secs: i64) -> Option<u64> {
        let snapshots = self.snapshots.read();
        let mut best: Option<u64> = None;
        for metadata in snapshots.values() {
            // W2.2(b): epoch-micros metadata, seconds granularity as before.
            if metadata.wall_clock_unix_secs() <= cutoff_unix_secs {
                best = Some(best.map_or(metadata.timestamp, |b: u64| b.max(metadata.timestamp)));
            }
        }
        best
    }

    /// Read a versioned value at a specific snapshot (legacy - linear scan)
    ///
    /// This implements the core time-travel query logic with O(N) complexity.
    /// Use read_at_snapshot_indexed() for O(log N) performance.
    #[allow(dead_code)]
    pub fn read_at_snapshot_linear(&self, table_name: &str, row_id: u64, snapshot_ts: u64) -> Result<Option<Vec<u8>>> {
        // Build key prefix for all versions of this row
        let prefix = format!("v:{}:{}:", table_name, row_id);

        // Iterate through versions in reverse chronological order
        // to find the most recent version <= snapshot_ts
        let mut best_version: Option<(u64, Vec<u8>)> = None;

        let iter = self.db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            // Parse key: v:{table}:{row_id}:{timestamp}
            if let Ok(key_str) = std::str::from_utf8(&key) {
                if key_str.starts_with(&prefix) {
                    if let Some(ts_str) = key_str.rsplit(':').next() {
                        if let Ok(ts) = ts_str.parse::<u64>() {
                            if ts <= snapshot_ts {
                                // Check if this is better than our current best
                                let should_update = match &best_version {
                                    None => true,
                                    Some((best_ts, _)) => *best_ts < ts,
                                };
                                if should_update {
                                    best_version = Some((ts, value.to_vec()));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Return the best version found, decoded like every other version read.
        // The scan above keeps only the value, so the exact key is gone by here;
        // the placeholder keeps the real `v:` prefix because the decode rule and
        // the plaintext-passthrough diagnostic are both keyed on the keyspace.
        self.decode_stored_opt(b"v:<version value>", best_version.map(|(_, value)| value))
    }

    /// Read a versioned value at a specific snapshot (optimized with reverse index and cache)
    ///
    /// This implements O(log N) time-travel queries using a reverse timestamp index.
    /// The reverse index uses `u64::MAX - timestamp` to enable efficient lookups.
    /// Additionally, uses an LRU cache for frequently accessed snapshots.
    pub fn read_at_snapshot(&self, table_name: &str, row_id: u64, snapshot_ts: u64) -> Result<Option<Vec<u8>>> {
        // W2.2(c): capture the table's cache generation BEFORE the lookup. An
        // entry is a hit only when its stamp still matches; storing this
        // pre-lookup generation means a write that bumps the counter during the
        // uncached lookup leaves the fresh entry immediately stale (recomputed
        // next read) rather than served as current.
        let cache_gen = if self.cache_config.enabled {
            let gen = self.table_cache_generation(table_name);
            let cache_key = (table_name.to_string(), row_id, snapshot_ts);
            if let Some((entry_gen, cached_value)) = self.snapshot_cache.lock().get(&cache_key) {
                if *entry_gen == gen {
                    // Cache hit - return cloned value
                    return Ok(cached_value.clone());
                }
            }
            Some(gen)
        } else {
            None
        };

        // Cache miss - perform database lookup. `read_at_snapshot_uncached`
        // returns the STORED bytes (`v:` or the `data:` fallback), so decode
        // here — once, at the single point every version read funnels through —
        // and the cache then holds plaintext, exactly as it did before values
        // were sealed. Callers (`Transaction::get`,
        // `StorageEngine::scan_table_at_snapshot`) bincode-deserialize the
        // result and must never see ciphertext.
        let stored = self.read_at_snapshot_uncached(table_name, row_id, snapshot_ts)?;
        let result = self.decode_stored_opt(b"v:<version value>", stored)?;

        // Store in cache if enabled (stamped with the pre-lookup generation)
        if let Some(gen) = cache_gen {
            let cache_key = (table_name.to_string(), row_id, snapshot_ts);
            self.snapshot_cache.lock().put(cache_key, (gen, result.clone()));
        }

        Ok(result)
    }

    /// Read a versioned value without using cache (internal method)
    ///
    /// This is the core implementation that performs the actual database lookup.
    fn read_at_snapshot_uncached(&self, table_name: &str, row_id: u64, snapshot_ts: u64) -> Result<Option<Vec<u8>>> {
        // Use reverse timestamp index for O(log N) lookup
        // Reverse timestamp allows us to find the latest version <= snapshot_ts
        let reverse_ts = u64::MAX - snapshot_ts;

        // Seek to the reverse timestamp index
        // Index format: v_idx:{table}:{row_id}:{reverse_ts} -> {actual_ts}
        let seek_key = format!("v_idx:{}:{}:{:020}", table_name, row_id, reverse_ts);

        // Since we use reverse timestamps (larger actual_ts -> smaller reverse_ts),
        // we need to seek forward to find versions with actual_ts <= snapshot_ts
        // (which have reverse_ts >= our target reverse_ts)
        let mut iter = self.db.iterator(rocksdb::IteratorMode::From(
            seek_key.as_bytes(),
            rocksdb::Direction::Forward,
        ));

        let expected_prefix = format!("v_idx:{}:{}:", table_name, row_id);

        // Check if we found a matching index entry
        if let Some(Ok((key, value))) = iter.next() {
            if let Ok(key_str) = std::str::from_utf8(&key) {
                if key_str.starts_with(&expected_prefix) {
                    // Decode the actual timestamp (and W3.2 elision flag) from the
                    // index value. An unflagged/legacy value decodes as
                    // `elided = false` — the pre-W3.2 `v:`-present invariant.
                    if let Some((actual_ts, elided)) = decode_version_index_value(&value) {
                        // Verify this version is visible to our snapshot. The seek
                        // lands on the newest version at-or-before `snapshot_ts`;
                        // an OLDER-than-newest version is always flag-CLEAR (any
                        // overwrite materializes the prior flagged event first), so
                        // only a truly-latest event can be elided here.
                        if actual_ts <= snapshot_ts {
                            if elided {
                                // W3.2: the `v:` copy was elided — this is the
                                // latest version and its value is the current
                                // `data:` row (no later mutation materialized it,
                                // else the flag would be clear). Read `data:`.
                                let data_key = format!("data:{}:{}", table_name, row_id);
                                return self
                                    .db
                                    .get(data_key.as_bytes())
                                    .map_err(|e| Error::storage(format!("Failed to read elided version data: {}", e)))
                                    .map(|opt| opt.map(|v| v.to_vec()));
                            }
                            // Non-elided: fetch the actual versioned `v:` data.
                            return self.get_version_by_exact_timestamp(table_name, row_id, actual_ts);
                        }
                    }
                }
            }
        }

        // No MVCC version entry found at or before this snapshot.
        // Check if ANY version entries exist for this row. If none exist at all,
        // the row was written through a non-versioned path (fast insert) and should
        // be visible to all snapshots (it predates MVCC tracking).
        let any_prefix = format!("v_idx:{}:{}:", table_name, row_id);
        let any_iter = self.db.iterator(rocksdb::IteratorMode::From(
            any_prefix.as_bytes(),
            rocksdb::Direction::Forward,
        ));
        let has_any_versions = any_iter
            .take(1)
            .filter_map(|item| item.ok())
            .any(|(k, _)| k.starts_with(any_prefix.as_bytes()));

        if !has_any_versions {
            // Item #2: a COPY fast-batch row carries no per-row `v:`/`v_idx:` —
            // its insert timestamp lives in a `vmeta:` range marker instead. If
            // a live marker covers this row, it is visible ONLY from that ts
            // onward; an AS-OF read that predates the COPY must not see it.
            // (Fast-out: `covering_ts` is one relaxed atomic load when no
            // markers exist, so the common non-COPY case pays nothing here.)
            if let Some(marker_ts) = self.copy_markers.covering_ts(table_name, row_id) {
                if snapshot_ts < marker_ts {
                    // Row did not exist yet at this snapshot.
                    return Ok(None);
                }
                // snapshot_ts >= marker_ts: the row is visible and (having no
                // versions) has never been updated, so `data:` is its value —
                // fall through to the direct read below.
            }
            // No MVCC versions at all — row was written via non-versioned path
            // (marker-covered & visible, or a genuine pre-MVCC fast insert).
            // Read from the data key directly.
            let data_key = format!("data:{}:{}", table_name, row_id);
            return self
                .db
                .get(data_key.as_bytes())
                .map_err(|e| Error::storage(format!("Failed to read data key fallback: {}", e)))
                .map(|opt| opt.map(|v| v.to_vec()));
        }

        // Versions exist but none at or before our snapshot — row was created
        // after our snapshot started. Not visible to this transaction.
        Ok(None)
    }

    /// Get a specific version by exact timestamp
    ///
    /// Helper method used by the indexed lookup.
    fn get_version_by_exact_timestamp(&self, table_name: &str, row_id: u64, timestamp: u64) -> Result<Option<Vec<u8>>> {
        let key = format!("v:{}:{}:{}", table_name, row_id, timestamp);
        self.db
            .get(key.as_bytes())
            .map_err(|e| Error::storage(format!("Failed to read version: {}", e)))
            .map(|opt| opt.map(|v| v.to_vec()))
    }

    /// Write a new version of a value
    ///
    /// Called when a transaction commits to create a new historical version.
    /// Also creates a reverse timestamp index entry for efficient lookups.
    /// Invalidates cache entries for this row.
    pub fn write_version(&self, table_name: &str, row_id: u64, timestamp: u64, value: &[u8]) -> Result<()> {
        // Write the actual versioned data. `v:` holds a copy of the row image,
        // so it is sealed exactly like the `data:` row it mirrors — a plaintext
        // version chain beside a ciphertext row would defeat the whole point.
        let key = format!("v:{}:{}:{}", table_name, row_id, timestamp);
        let stored = self.seal_stored(value)?;
        self.db
            .put(key.as_bytes(), stored.as_ref())
            .map_err(|e| Error::storage(format!("Failed to write version: {}", e)))?;

        // Create reverse timestamp index entry
        // Index structure: v_idx:{table}:{row_id}:{reverse_ts} -> {actual_ts}
        // Reverse timestamp = u64::MAX - timestamp for efficient SeekForPrev
        self.create_reverse_timestamp_index(table_name, row_id, timestamp)?;

        // Invalidate cache entries for this row
        // We need to remove all cached entries for this (table, row_id) combination
        // since a new version may affect reads at different snapshot timestamps
        self.invalidate_cache_for_row(table_name, row_id);

        Ok(())
    }

    /// Write a row version and its snapshot metadata in a single RocksDB batch.
    ///
    /// Fast autocommit DML needs the same durable state as `write_version` plus
    /// `register_snapshot(_with_lsn)`, but issuing five separate RocksDB writes
    /// per row dominates single-row insert throughput. This method keeps the
    /// same keys and in-memory indexes while committing them together.
    /// `allow_elide` (W3.2): when the caller can guarantee `data:` holds exactly
    /// this version's logical value (default row-store, NOT side-storage), and
    /// `elide_latest_version` is on, the `v:` copy is elided in favor of a
    /// flagged `v_idx:` event. Callers on the side-storage path (where `data:`
    /// holds the sidecar image, not the version value) MUST pass `false`.
    pub fn write_version_and_register_snapshot(
        &self,
        table_name: &str,
        row_id: u64,
        timestamp: u64,
        value: &[u8],
        lsn: Option<u64>,
        allow_elide: bool,
    ) -> Result<SnapshotMetadata> {
        let (metadata, txn_id, scn) = self.allocate_snapshot_metadata(timestamp, lsn);
        let mut batch = WriteBatch::default();
        self.append_version_snapshot_to_batch(
            &mut batch,
            table_name,
            row_id,
            timestamp,
            value,
            // Nothing else in this batch holds the sealed form of `value`, so
            // the funnel seals it.
            None,
            &metadata,
            allow_elide,
        )?;
        self.write_batch(batch, "Failed to write version snapshot batch")?;
        self.finish_version_snapshot(table_name, row_id, timestamp, txn_id, scn, metadata)
    }

    /// Write current row data plus its time-travel version/snapshot metadata in
    /// one RocksDB batch. Used by fast autocommit INSERT after logical-WAL/HA
    /// has been ruled out by the caller.
    ///
    /// `allow_elide` (W3.2): see `write_version_and_register_snapshot`. This path
    /// puts `data_value` and appends the version in ONE batch, so eliding is only
    /// sound when `data_value == version_value` — the caller (non-side-storage
    /// fast INSERT) guarantees that and passes `!uses_side_storage`.
    pub fn write_data_version_and_register_snapshot(
        &self,
        data_key: &[u8],
        data_value: &[u8],
        table_name: &str,
        row_id: u64,
        timestamp: u64,
        version_value: &[u8],
        lsn: Option<u64>,
        write_options: Option<&WriteOptions>,
        allow_elide: bool,
    ) -> Result<SnapshotMetadata> {
        let (metadata, txn_id, scn) = self.allocate_snapshot_metadata(timestamp, lsn);
        let mut batch = WriteBatch::default();
        // THE DEFAULT AUTOCOMMIT INSERT'S ONLY `data:` WRITE. `insert_tuple_fast`
        // skips `StorageEngine::put` on this arm, so if this put is not sealed
        // the row never is.
        let stored_data = self.seal_stored(data_value)?;
        batch.put(data_key, stored_data.as_ref());

        // ONE cipher invocation per row on this path, not two. The caller gates
        // this route on `!uses_side_storage`, which is exactly the case in which
        // the `v:` twin carries the same row image as `data:` — so the sealed
        // bytes are reused rather than the identical plaintext being encrypted a
        // second time. Storing the same ciphertext under both keys is the choice
        // `Transaction::put_versioned_batch` already makes for the same pair, and
        // it discloses nothing: a reader who can see both keys can see they are
        // equal either way, and it is one plaintext with one nonce, never a nonce
        // reused across two different plaintexts.
        //
        // The comparison runs ONLY when a key is configured, so the default
        // encryption-disabled path pays nothing for it: with no key manager this
        // is a single `Option` check and `sealed_version` is `None`, leaving the
        // funnel below to do exactly what it did before.
        let sealed_version: Option<&[u8]> = match &self.key_manager {
            Some(_) if data_value == version_value => Some(stored_data.as_ref()),
            _ => None,
        };
        self.append_version_snapshot_to_batch(
            &mut batch,
            table_name,
            row_id,
            timestamp,
            version_value,
            sealed_version,
            &metadata,
            allow_elide,
        )?;

        if let Some(opts) = write_options {
            self.db
                .write_opt(batch, opts)
                .map_err(|e| Error::storage(format!("Failed to write data/version snapshot batch: {}", e)))?;
        } else {
            self.write_batch(batch, "Failed to write data/version snapshot batch")?;
        }

        self.finish_version_snapshot(table_name, row_id, timestamp, txn_id, scn, metadata)
    }

    fn allocate_snapshot_metadata(&self, timestamp: u64, lsn: Option<u64>) -> (SnapshotMetadata, TransactionId, Scn) {
        let txn_id = match lsn {
            Some(lsn) => {
                // W2.2(d): keep the counter ahead of externally provided LSNs (atomic fetch_max).
                self.current_txn_id.fetch_max(lsn + 1, Ordering::Relaxed);
                lsn
            }
            None => self.next_transaction_id(),
        };
        let scn = self.next_scn();
        let metadata = SnapshotMetadata::new(timestamp, txn_id, scn);
        (metadata, txn_id, scn)
    }

    /// Item #2: record a durable COPY range marker's presence in the in-memory
    /// interval index after its batch has committed. Called by the storage
    /// engine's fast batch path; the `vmeta:` key itself is written inside that
    /// same `WriteBatch` for atomicity.
    pub(crate) fn record_copy_marker(&self, table_name: &str, first: u64, last: u64, ts: u64) {
        self.copy_markers.insert(table_name, first, last, ts);
    }

    /// Test/inspection hook (item #2): number of live COPY range markers in the
    /// in-memory index. A single COPY batch registers exactly one marker, so a
    /// non-zero count proves the version-elision path was taken (not the
    /// per-row `v:`/`v_idx:` fallback).
    #[doc(hidden)]
    pub fn debug_copy_marker_len(&self) -> usize {
        self.copy_markers.len()
    }

    /// Test/inspection hook (item #2): the COPY insert ts covering `(table,
    /// row_id)`, or `None` if no live marker covers it.
    #[doc(hidden)]
    pub fn debug_copy_marker_ts(&self, table_name: &str, row_id: u64) -> Option<u64> {
        self.copy_markers.covering_ts(table_name, row_id)
    }

    /// Whether ANY `v_idx:` entry exists for `(table, row_id)` — i.e. the row
    /// has at least one materialized version. Used to decide if a marker-covered
    /// row still needs its insert version backfilled.
    fn has_any_version_index(&self, table_name: &str, row_id: u64) -> Result<bool> {
        let prefix = format!("v_idx:{}:{}:", table_name, row_id);
        let mut iter = self.db.iterator(rocksdb::IteratorMode::From(
            prefix.as_bytes(),
            rocksdb::Direction::Forward,
        ));
        Ok(match iter.next() {
            Some(Ok((k, _))) => k.starts_with(prefix.as_bytes()),
            _ => false,
        })
    }

    /// Item #2: whether `table` currently has any live COPY markers — a cheap
    /// gate for TRUNCATE, which must materialize covered rows before removing
    /// `data:` (else AS-OF reads predating the TRUNCATE lose the copied value).
    pub(crate) fn table_has_copy_markers(&self, table_name: &str) -> bool {
        self.copy_markers.table_has_markers(table_name)
    }

    /// Item #2: whether ANY live COPY marker exists (one relaxed atomic load).
    /// The transaction commit-apply loop uses this to skip per-`data:`-key
    /// marker probing entirely on the common no-COPY path.
    pub(crate) fn has_any_copy_markers(&self) -> bool {
        !self.copy_markers.is_empty()
    }

    /// Item #2: durably materialize a marker-covered row's insert version NOW,
    /// in its own write, BEFORE a version-SKIPPING fast UPDATE/DELETE
    /// (`update_tuple_fast*` / `delete_tuple_fast*`) overwrites or removes
    /// `data:`. Without this, the per-row `v:`/`v_idx:` that the pre-item-#2
    /// COPY wrote eagerly (and which survived a later fast mutation) would be
    /// gone, and an AS-OF read in `[copy_ts, mutation_ts)` would wrongly resolve
    /// to the post-mutation `data:` (or find the row deleted). No-op — one
    /// relaxed atomic load — when no markers exist, so the hot fast-DML path is
    /// untaxed on non-COPY workloads. Idempotent: a row that already has a
    /// `v_idx:` is treated as materialized and skipped.
    ///
    /// W3.2: this is ALSO the flagged-latest-version materialize site. Every fast
    /// / generic UPDATE/DELETE funnel that overwrites or removes main `data:`
    /// already calls this before its write, so extending it here covers all those
    /// sites at once: if the row's newest `v_idx:` event is elided (flag SET),
    /// its real `v:` is materialized from the current `data:` and the flag is
    /// cleared BEFORE `data:` changes. Gated by `maybe_elided_rows` (one relaxed
    /// load) so non-elision databases pay nothing.
    pub(crate) fn materialize_copy_marker_row_durable(&self, table_name: &str, row_id: u64) -> Result<()> {
        if !self.copy_markers.is_empty() {
            self.materialize_copy_marker_row_durable_inner(table_name, row_id)?;
        }
        if self.maybe_elided_rows() {
            self.materialize_elided_latest_version_durable(table_name, row_id)?;
        }
        Ok(())
    }

    fn materialize_copy_marker_row_durable_inner(&self, table_name: &str, row_id: u64) -> Result<()> {
        let marker_ts = match self.copy_markers.covering_ts(table_name, row_id) {
            Some(ts) => ts,
            None => return Ok(()),
        };
        if self.has_any_version_index(table_name, row_id)? {
            return Ok(());
        }
        let data_key = format!("data:{}:{}", table_name, row_id);
        let old_value = self
            .db
            .get(data_key.as_bytes())
            .map_err(|e| Error::storage(format!("marker materialize data read failed: {}", e)))?;
        if let Some(old_value) = old_value {
            // `old_value` is the STORED `data:` image, copied verbatim into
            // `v:`. It is deliberately NOT sealed again: on an encrypted
            // database it is already ciphertext (re-sealing would double-encrypt
            // and the read would return a ciphertext blob as if it were a row),
            // and on a legacy plaintext row the tolerant decode handles it. The
            // property being preserved is "the `v:` twin holds exactly what
            // `data:` held", which a verbatim copy gives for free.
            let mut batch = WriteBatch::default();
            let version_key = format!("v:{}:{}:{}", table_name, row_id, marker_ts);
            batch.put(version_key.as_bytes(), &old_value);
            let reverse_ts = u64::MAX - marker_ts;
            let index_key = format!("v_idx:{}:{}:{:020}", table_name, row_id, reverse_ts);
            batch.put(index_key.as_bytes(), marker_ts.to_be_bytes());
            self.write_batch(batch, "copy marker materialize")?;
        }
        Ok(())
    }

    /// W3.2: locate the row's NEWEST `v_idx:` event and, if it is elided (flag
    /// SET), return its `(commit_ts, index_key)`. Only the latest version can be
    /// flagged (every overwrite materializes the prior flagged event first), so
    /// checking the first entry in the row's `v_idx:` prefix — which sorts newest
    /// first (smallest reverse_ts) — is sufficient. `None` when the row has no
    /// `v_idx:` or its newest event is already materialized (flag clear).
    fn find_flagged_latest_version(&self, table_name: &str, row_id: u64) -> Result<Option<(u64, Vec<u8>)>> {
        let prefix = format!("v_idx:{}:{}:", table_name, row_id);
        let mut iter = self.db.iterator(rocksdb::IteratorMode::From(
            prefix.as_bytes(),
            rocksdb::Direction::Forward,
        ));
        if let Some(item) = iter.next() {
            let (key, value) = item.map_err(|e| Error::storage(format!("flagged version seek failed: {}", e)))?;
            if key.starts_with(prefix.as_bytes()) {
                if let Some((event_ts, elided)) = decode_version_index_value(&value) {
                    if elided {
                        return Ok(Some((event_ts, key.to_vec())));
                    }
                }
            }
        }
        Ok(None)
    }

    /// W3.2: durably materialize the row's flagged (elided) latest version NOW,
    /// in its own write, BEFORE a caller overwrites/removes `data:`. Writes the
    /// real `v:{event_ts}` from the current `data:` value and rewrites the
    /// `v_idx:` event value with the flag CLEARED. Idempotent — an
    /// already-materialized (or absent) event is a no-op.
    fn materialize_elided_latest_version_durable(&self, table_name: &str, row_id: u64) -> Result<()> {
        let Some((event_ts, index_key)) = self.find_flagged_latest_version(table_name, row_id)? else {
            return Ok(());
        };
        let data_key = format!("data:{}:{}", table_name, row_id);
        let old_value = self
            .db
            .get(data_key.as_bytes())
            .map_err(|e| Error::storage(format!("elided version materialize data read failed: {}", e)))?;
        if let Some(old_value) = old_value {
            // Verbatim copy of the stored `data:` image — see
            // `materialize_copy_marker_row_durable_inner` for why it must not be
            // sealed again.
            let mut batch = WriteBatch::default();
            let version_key = format!("v:{}:{}:{}", table_name, row_id, event_ts);
            batch.put(version_key.as_bytes(), &old_value);
            batch.put(&index_key, encode_version_index_value(event_ts, false));
            self.write_batch(batch, "elided version materialize")?;
        }
        Ok(())
    }

    /// W3.2: stage the row's flagged (elided) latest-version materialization into
    /// `batch` BEFORE the caller's own `data:` overwrite/delete rides the same
    /// batch (crash-atomic). The `data:` read returns the pre-batch value because
    /// `batch` is not yet committed. Idempotent (no-op if the newest event is
    /// already materialized). Callers gate on `maybe_elided_rows`.
    pub(crate) fn materialize_elided_latest_version(
        &self,
        batch: &mut WriteBatch,
        table_name: &str,
        row_id: u64,
    ) -> Result<()> {
        let Some((event_ts, index_key)) = self.find_flagged_latest_version(table_name, row_id)? else {
            return Ok(());
        };
        let data_key = format!("data:{}:{}", table_name, row_id);
        let old_value = self
            .db
            .get(data_key.as_bytes())
            .map_err(|e| Error::storage(format!("elided version backfill data read failed: {}", e)))?;
        if let Some(old_value) = old_value {
            // Verbatim copy of the stored `data:` image (see
            // `materialize_copy_marker_row_durable_inner`).
            let version_key = format!("v:{}:{}:{}", table_name, row_id, event_ts);
            batch.put(version_key.as_bytes(), &old_value);
            batch.put(&index_key, encode_version_index_value(event_ts, false));
        }
        Ok(())
    }

    /// Item #2: if `(table, row_id)` is covered by a live COPY marker and has no
    /// materialized version yet, stage its insert version (`v:`/`v_idx:` at the
    /// marker's ts, value = current `data:`) into `batch`. Idempotent: once a
    /// `v_idx:` exists the row is considered materialized and this is a no-op.
    /// The `data:` read returns the pre-update value because `batch` (which
    /// carries the new `data:`) has not been committed yet.
    pub(crate) fn materialize_copy_marker_row(
        &self,
        batch: &mut WriteBatch,
        table_name: &str,
        row_id: u64,
        new_timestamp: u64,
    ) -> Result<()> {
        let marker_ts = match self.copy_markers.covering_ts(table_name, row_id) {
            Some(ts) => ts,
            None => return Ok(()),
        };
        // A mutation landing at the exact marker ts needs no separate insert
        // version, and an already-materialized row must not be double-written.
        if marker_ts >= new_timestamp || self.has_any_version_index(table_name, row_id)? {
            return Ok(());
        }
        let data_key = format!("data:{}:{}", table_name, row_id);
        let old_value = self
            .db
            .get(data_key.as_bytes())
            .map_err(|e| Error::storage(format!("marker backfill data read failed: {}", e)))?;
        if let Some(old_value) = old_value {
            // Verbatim copy of the stored `data:` image (see
            // `materialize_copy_marker_row_durable_inner`).
            let version_key = format!("v:{}:{}:{}", table_name, row_id, marker_ts);
            batch.put(version_key.as_bytes(), &old_value);
            let reverse_ts = u64::MAX - marker_ts;
            let index_key = format!("v_idx:{}:{}:{:020}", table_name, row_id, reverse_ts);
            batch.put(index_key.as_bytes(), marker_ts.to_be_bytes());
        }
        Ok(())
    }

    /// Append the `v:` copy, its `v_idx:` event and the optional `snapshot:`
    /// metadata for one version into `batch`.
    ///
    /// `value` is the version's PLAINTEXT row image. `sealed_value` is that same
    /// plaintext ALREADY sealed, for the one caller that has just sealed it for
    /// the `data:` key in this same batch — passing it avoids a second cipher
    /// invocation over identical bytes. `None` means "seal it here", and every
    /// caller that has no sealed copy to hand passes `None`.
    fn append_version_snapshot_to_batch(
        &self,
        batch: &mut WriteBatch,
        table_name: &str,
        row_id: u64,
        timestamp: u64,
        value: &[u8],
        sealed_value: Option<&[u8]>,
        metadata: &SnapshotMetadata,
        allow_elide: bool,
    ) -> Result<()> {
        // Item #2: if this row is still covered by a live COPY marker (inserted
        // via the version-eliding fast batch) and has not yet been materialized,
        // write its insert version FROM the current on-disk `data:` value before
        // recording the new version — so AS-OF reads in [copy_ts, this_ts)
        // resolve to the pre-update value instead of finding no version and
        // treating the row as not-yet-existing. Cheap common path: `covering_ts`
        // is a single relaxed atomic load when no markers exist.
        self.materialize_copy_marker_row(batch, table_name, row_id, timestamp)?;

        // W3.2: this funnel is INSERT-only (fresh row_id via `insert_tuple_fast`),
        // so there is never a prior flagged version of this row to materialize —
        // the flagged-latest materialize is a mutation-path concern only.
        let elide = allow_elide && self.elide_latest_version();

        let reverse_ts = u64::MAX - timestamp;
        let index_key = format!("v_idx:{}:{}:{:020}", table_name, row_id, reverse_ts);

        let version_bytes = if elide {
            // W3.2: elide the `v:` copy. Write only the flagged `v_idx:` event;
            // its value is served from `data:` (written by the caller in this same
            // batch) until the row's first mutation materializes the real `v:`.
            batch.put(index_key.as_bytes(), encode_version_index_value(timestamp, true));
            (index_key.len() + 8) as u64
        } else {
            let version_key = format!("v:{}:{}:{}", table_name, row_id, timestamp);
            // `value` arrives as PLAINTEXT from every caller; this funnel is the
            // boundary for the `v:` copy, so it seals here — unless the caller
            // already sealed this exact plaintext for the `data:` key in this
            // same batch and handed the result down (see `sealed_value`).
            let stored: Cow<'_, [u8]> = match sealed_value {
                Some(sealed) => Cow::Borrowed(sealed),
                None => self.seal_stored(value)?,
            };
            batch.put(version_key.as_bytes(), stored.as_ref());
            batch.put(index_key.as_bytes(), encode_version_index_value(timestamp, false));
            (version_key.len() + stored.len() + index_key.len() + 8) as u64
        };

        // W3.2: version-chain bytes for the autocommit INSERT path (fast single
        // INSERT via `insert_tuple_fast`). With elision ON the `v:` copy is gone,
        // so this collapses to the `v_idx:` event size — the §7 re-measurement.
        if crate::write_volume::enabled() {
            crate::write_volume::add(crate::write_volume::Category::Version, version_bytes);
        }

        if self.persist_metadata {
            // R1.4: snapshot: alone — txn_map:/scn_map: were write-only
            // (recover_snapshots rebuilds every in-memory map from the
            // snapshot: entries; nothing ever read the other two), costing
            // two keys of write amplification per autocommit statement.
            let snapshot_key = format!("snapshot:{}", metadata.timestamp);
            let snapshot_value = bincode::serialize(&metadata)
                .map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;
            batch.put(snapshot_key.as_bytes(), snapshot_value);
        }

        Ok(())
    }

    fn finish_version_snapshot(
        &self,
        table_name: &str,
        row_id: u64,
        timestamp: u64,
        txn_id: TransactionId,
        scn: Scn,
        metadata: SnapshotMetadata,
    ) -> Result<SnapshotMetadata> {
        self.snapshots.write().insert(timestamp, metadata.clone());
        self.txn_to_timestamp.write().insert(txn_id, timestamp);
        self.scn_to_timestamp.write().insert(scn, timestamp);
        self.invalidate_cache_for_row(table_name, row_id);

        if self.gc_config.auto_gc_enabled {
            if let Err(e) = self.gc_if_needed() {
                eprintln!("Warning: Snapshot GC failed: {}", e);
            }
        }

        Ok(metadata)
    }

    /// Invalidate cached snapshot reads after a new version is written.
    ///
    /// W2.2(c): O(1) — bumps the table's cache generation instead of the former
    /// linear scan over up to `max_entries` (1000) LRU keys. Stale entries are
    /// lazily rejected on the next `read_at_snapshot`. The bump is per-table (a
    /// safe over-approximation of the former per-(table,row) removal): it can
    /// only cost extra recomputation, never serve stale data. `row_id` is no
    /// longer needed but the signature is kept for the existing call sites.
    fn invalidate_cache_for_row(&self, table_name: &str, _row_id: u64) {
        if !self.cache_config.enabled {
            return;
        }
        self.bump_table_cache_generation(table_name);
    }

    /// W2.2(c): current snapshot-cache generation for a table (0 if never written).
    fn table_cache_generation(&self, table_name: &str) -> u64 {
        self.snapshot_cache_gen
            .get(table_name)
            .map(|g| g.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// W2.2(c): advance a table's snapshot-cache generation, invalidating every
    /// entry cached at an earlier generation on its next read.
    fn bump_table_cache_generation(&self, table_name: &str) {
        self.snapshot_cache_gen
            .entry(table_name.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Create reverse timestamp index for O(log N) lookups
    ///
    /// Index structure: v_idx:{table}:{row_id}:{reverse_ts} -> {actual_ts}
    /// Reverse timestamp allows RocksDB to find "latest before X" efficiently.
    fn create_reverse_timestamp_index(&self, table_name: &str, row_id: u64, timestamp: u64) -> Result<()> {
        let reverse_ts = u64::MAX - timestamp;
        let index_key = format!("v_idx:{}:{}:{:020}", table_name, row_id, reverse_ts);

        // Store the actual timestamp as the value (8 bytes, big-endian)
        let timestamp_bytes = timestamp.to_be_bytes();

        self.db
            .put(index_key.as_bytes(), timestamp_bytes)
            .map_err(|e| Error::storage(format!("Failed to create reverse index: {}", e)))
    }

    /// Persist snapshot metadata to disk
    ///
    /// R1.4: one `snapshot:` put. The former `txn_map:`/`scn_map:` mappings
    /// were write-only — recovery rebuilds every in-memory map from the
    /// `snapshot:` entries alone — so each commit paid two extra RocksDB
    /// write calls for nothing. GC still deletes legacy keys from databases
    /// written by older versions.
    fn persist_snapshot_metadata(&self, metadata: &SnapshotMetadata) -> Result<()> {
        let key = format!("snapshot:{}", metadata.timestamp);
        let value =
            bincode::serialize(metadata).map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;

        self.db
            .put(key.as_bytes(), value)
            .map_err(|e| Error::storage(format!("Failed to persist metadata: {}", e)))
    }

    /// Garbage collect old snapshots
    ///
    /// Removes snapshots that are:
    /// - Older than min_retention_seconds
    /// - Beyond max_snapshots limit
    /// - Marked as gc_eligible
    pub fn gc_old_snapshots(&self) -> Result<usize> {
        let now = Utc::now().timestamp() as u64;
        let min_retention = self.gc_config.min_retention_seconds;

        let mut snapshots = self.snapshots.write();
        let mut to_remove = Vec::new();

        // Find snapshots eligible for GC
        for (ts, metadata) in snapshots.iter() {
            if !metadata.gc_eligible {
                continue;
            }

            // W2.2(b): wall-clock age from epoch-micros metadata (seconds granularity).
            let age = now.saturating_sub(metadata.wall_clock_unix_secs() as u64);
            if age > min_retention {
                to_remove.push(*ts);
            }
        }

        // If we're still over the limit, remove oldest eligible snapshots
        if snapshots.len() - to_remove.len() > self.gc_config.max_snapshots {
            let mut eligible: Vec<_> = snapshots
                .iter()
                .filter(|(_, m)| m.gc_eligible && !to_remove.contains(&m.timestamp))
                .map(|(ts, m)| (*ts, m.clone()))
                .collect();

            eligible.sort_by_key(|(ts, _)| *ts);

            let excess = (snapshots.len() - to_remove.len()).saturating_sub(self.gc_config.max_snapshots);
            for (ts, _) in eligible.iter().take(excess) {
                to_remove.push(*ts);
            }
        }

        // Remove snapshots
        let count = to_remove.len();
        let mut delete_batch = WriteBatch::default();
        for ts in &to_remove {
            if let Some(metadata) = snapshots.remove(ts) {
                // Remove from mappings
                self.txn_to_timestamp.write().remove(&metadata.transaction_id);
                self.scn_to_timestamp.write().remove(&metadata.scn);

                if self.persist_metadata {
                    let snap_key = format!("snapshot:{}", ts);
                    let txn_key = format!("txn_map:{}", metadata.transaction_id);
                    let scn_key = format!("scn_map:{}", metadata.scn);

                    delete_batch.delete(snap_key.as_bytes());
                    delete_batch.delete(txn_key.as_bytes());
                    delete_batch.delete(scn_key.as_bytes());
                }

                // Note: We don't delete the versioned data (v:*) here
                // That would require a separate GC pass to avoid breaking
                // any in-flight queries
            }
        }

        if count > 0 && self.persist_metadata {
            self.write_batch(delete_batch, "Failed to delete old snapshots")?;
        }

        Ok(count)
    }

    /// Run GC if needed
    fn gc_if_needed(&self) -> Result<()> {
        let snapshot_count = self.snapshots.read().len();
        let slack = self.gc_config.max_snapshots.clamp(1, 1000);
        let trigger = self.gc_config.max_snapshots.saturating_add(slack);
        if snapshot_count > trigger {
            self.gc_old_snapshots()?;
        }
        Ok(())
    }

    /// Get snapshot metadata
    pub fn get_snapshot_metadata(&self, timestamp: u64) -> Option<SnapshotMetadata> {
        self.snapshots.read().get(&timestamp).cloned()
    }

    /// Get current SCN
    pub fn current_scn(&self) -> Scn {
        self.current_scn.load(Ordering::Relaxed)
    }

    /// Get current transaction ID
    pub fn current_transaction_id(&self) -> TransactionId {
        self.current_txn_id.load(Ordering::Relaxed)
    }

    /// Get snapshot count
    pub fn snapshot_count(&self) -> usize {
        self.snapshots.read().len()
    }

    /// List all snapshots
    pub fn list_snapshots(&self) -> Result<Vec<SnapshotMetadata>> {
        let snapshots = self.snapshots.read();
        let mut result: Vec<_> = snapshots.values().cloned().collect();
        result.sort_by_key(|s| s.timestamp);
        Ok(result)
    }

    /// Load existing snapshots from disk (for recovery)
    pub fn recover_snapshots(&self) -> Result<usize> {
        let mut count = 0;
        let iter = self.db.iterator(rocksdb::IteratorMode::Start);

        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error during recovery: {}", e)))?;

            // Item #2: rebuild the in-memory COPY marker index from the durable
            // `vmeta:` records in the same scan (crash-safe resume — a marker
            // outlives the process, so AS-OF visibility survives restart).
            if key.starts_with(crate::storage::copy_marker::VMETA_PREFIX.as_bytes()) {
                self.copy_markers.load_record(&key, &value);
                continue;
            }

            if let Ok(key_str) = std::str::from_utf8(&key) {
                if key_str.starts_with("snapshot:") {
                    // W2.2(b): tolerate both epoch-micros and legacy RFC3339 layouts.
                    if let Some(metadata) = deserialize_snapshot_metadata(&value) {
                        // Restore in-memory state
                        self.snapshots.write().insert(metadata.timestamp, metadata.clone());
                        self.txn_to_timestamp
                            .write()
                            .insert(metadata.transaction_id, metadata.timestamp);
                        self.scn_to_timestamp.write().insert(metadata.scn, metadata.timestamp);

                        // Update counters. W2.2(d): fetch_max is the atomic
                        // equivalent of the former `if id >= *counter { *counter = id + 1 }`.
                        self.current_scn.fetch_max(metadata.scn + 1, Ordering::Relaxed);
                        self.current_txn_id
                            .fetch_max(metadata.transaction_id + 1, Ordering::Relaxed);

                        count += 1;
                    }
                }
            }
        }

        Ok(count)
    }

    /// Get cache statistics
    ///
    /// Returns (current_size, max_capacity) of the snapshot cache
    pub fn cache_stats(&self) -> (usize, usize) {
        let cache = self.snapshot_cache.lock();
        (cache.len(), cache.cap().get())
    }

    /// Clear the snapshot cache
    ///
    /// Useful for testing or manual cache management
    pub fn clear_cache(&self) {
        self.snapshot_cache.lock().clear();
    }

    /// Calculate approximate size of a snapshot in bytes
    ///
    /// This estimates the storage footprint by counting version keys
    /// that exist at the snapshot's timestamp.
    pub fn calculate_snapshot_size(&self, timestamp: u64) -> Result<u64> {
        let mut total_size: u64 = 0;
        let prefix = format!("v:");

        // Iterate through all version keys
        let iter = self.db.iterator(rocksdb::IteratorMode::From(
            prefix.as_bytes(),
            rocksdb::Direction::Forward,
        ));

        for item in iter {
            let (key, value) =
                item.map_err(|e| Error::storage(format!("Iterator error during size calculation: {}", e)))?;

            if let Ok(key_str) = std::str::from_utf8(&key) {
                // Version keys: v:{table}:{row_id}:{timestamp}
                if key_str.starts_with("v:") {
                    // Parse timestamp from key
                    if let Some(ts_str) = key_str.rsplit(':').next() {
                        if let Ok(ts) = ts_str.parse::<u64>() {
                            // Count versions <= snapshot timestamp
                            if ts <= timestamp {
                                total_size += key.len() as u64 + value.len() as u64;
                            }
                        }
                    }
                }
            }

            // Stop if we've moved past version keys
            if !key.starts_with(b"v:") {
                break;
            }
        }

        Ok(total_size)
    }

    /// Scan all versions of all rows in a table between two timestamps
    ///
    /// Returns a vector of (row_id, timestamp, value_bytes) for each version
    /// within the specified range [start_ts, end_ts].
    ///
    /// Used for VERSIONS BETWEEN queries.
    pub fn scan_versions_between(
        &self,
        table_name: &str,
        start_ts: u64,
        end_ts: u64,
    ) -> Result<Vec<(u64, u64, Vec<u8>)>> {
        let mut versions = Vec::new();
        let prefix = format!("v:{}:", table_name);

        // Iterate through all version keys for this table
        let iter = self.db.iterator(rocksdb::IteratorMode::From(
            prefix.as_bytes(),
            rocksdb::Direction::Forward,
        ));

        for item in iter {
            let (key, value) =
                item.map_err(|e| Error::storage(format!("Iterator error during version scan: {}", e)))?;

            // Stop if we've moved past this table's version keys
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }

            if let Ok(key_str) = std::str::from_utf8(&key) {
                // Parse key: v:{table}:{row_id}:{timestamp}
                let parts: Vec<&str> = key_str.split(':').collect();
                if let (Some(p2), Some(p3)) = (parts.get(2), parts.get(3)) {
                    if let (Ok(row_id), Ok(ts)) = (p2.parse::<u64>(), p3.parse::<u64>()) {
                        // Check if timestamp is within range
                        if ts >= start_ts && ts <= end_ts {
                            // VERSIONS BETWEEN hands these straight to
                            // `bincode::deserialize` in the executor, so decode
                            // here — this is a `v:` value like any other.
                            let decoded = tde::open_owned(self.key_manager.as_deref(), &key, value.to_vec())?;
                            versions.push((row_id, ts, decoded));
                        }
                    }
                }
            }
        }

        // Sort by row_id first, then by timestamp descending (newest first)
        versions.sort_by(|a, b| {
            match a.0.cmp(&b.0) {
                std::cmp::Ordering::Equal => b.1.cmp(&a.1), // Descending timestamp
                other => other,
            }
        });

        Ok(versions)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Config;
    use tempfile::tempdir;

    fn create_test_db() -> (Arc<DB>, tempfile::TempDir) {
        let temp_dir = tempdir().unwrap();
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, temp_dir.path()).unwrap();
        (Arc::new(db), temp_dir)
    }

    #[test]
    fn test_snapshot_registration() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db);

        let metadata = manager.register_snapshot(100).unwrap();
        assert_eq!(metadata.timestamp, 100);
        assert_eq!(metadata.transaction_id, 1);
        assert_eq!(metadata.scn, 1);
    }

    #[test]
    fn test_resolve_transaction() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db);

        let metadata = manager.register_snapshot(100).unwrap();
        let txn_id = metadata.transaction_id;

        let resolved = manager.resolve_transaction(txn_id).unwrap();
        assert_eq!(resolved, 100);
    }

    #[test]
    fn test_resolve_scn() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db);

        let metadata = manager.register_snapshot(100).unwrap();
        let scn = metadata.scn;

        let resolved = manager.resolve_scn(scn).unwrap();
        assert_eq!(resolved, 100);
    }

    #[test]
    fn test_non_durable_snapshot_manager_keeps_metadata_in_memory_only() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new_non_durable(Arc::clone(&db));

        let metadata = manager
            .write_version_and_register_snapshot("users", 1, 100, b"value_at_100", Some(42), false)
            .unwrap();

        assert_eq!(manager.get_snapshot_metadata(100).unwrap().transaction_id, 42);
        assert_eq!(manager.resolve_transaction(42).unwrap(), 100);
        assert_eq!(manager.resolve_scn(metadata.scn).unwrap(), 100);
        assert!(manager.read_at_snapshot("users", 1, 100).unwrap().is_some());

        assert!(db.get(b"v:users:1:100").unwrap().is_some());
        assert!(db.get(b"v_idx:users:1:18446744073709551515").unwrap().is_some());
        assert!(db.get(b"snapshot:100").unwrap().is_none());
        assert!(db.get(b"txn_map:42").unwrap().is_none());
        assert!(db
            .get(format!("scn_map:{}", metadata.scn).as_bytes())
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_version_write_and_read() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db);

        // Write versions at different timestamps
        let value1 = b"value_at_100".to_vec();
        let value2 = b"value_at_200".to_vec();

        manager.write_version("users", 1, 100, &value1).unwrap();
        manager.write_version("users", 1, 200, &value2).unwrap();

        // Read at timestamp 150 should get value1
        let result = manager.read_at_snapshot("users", 1, 150).unwrap();
        assert_eq!(result, Some(value1));

        // Read at timestamp 250 should get value2
        let result = manager.read_at_snapshot("users", 1, 250).unwrap();
        assert_eq!(result, Some(value2));

        // Read at timestamp 50 should get nothing
        let result = manager.read_at_snapshot("users", 1, 50).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_snapshot_gc() {
        let (db, _temp) = create_test_db();
        let gc_config = GcConfig {
            min_retention_seconds: 0, // Allow immediate GC for testing
            max_snapshots: 5,
            auto_gc_enabled: false, // Manual GC for testing
        };
        let manager = SnapshotManager::with_gc_config(db, gc_config);

        // Create 10 snapshots
        for i in 1..=10 {
            manager.register_snapshot(i * 100).unwrap();
        }

        assert_eq!(manager.snapshot_count(), 10);

        // Run GC - should keep only 5 newest
        let removed = manager.gc_old_snapshots().unwrap();
        assert_eq!(removed, 5);
        assert_eq!(manager.snapshot_count(), 5);
    }

    #[test]
    fn test_snapshot_recovery() {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path();

        // Create snapshots and close
        {
            let mut opts = rocksdb::Options::default();
            opts.create_if_missing(true);
            let db = Arc::new(DB::open(&opts, db_path).unwrap());
            let manager = SnapshotManager::new(db);

            manager.register_snapshot(100).unwrap();
            manager.register_snapshot(200).unwrap();
        }

        // Reopen and recover
        {
            let mut opts = rocksdb::Options::default();
            opts.create_if_missing(true);
            let db = Arc::new(DB::open(&opts, db_path).unwrap());
            let manager = SnapshotManager::new(db);

            let count = manager.recover_snapshots().unwrap();
            assert_eq!(count, 2);
            assert_eq!(manager.snapshot_count(), 2);
        }
    }

    #[test]
    fn test_snapshot_metadata_legacy_rfc3339_fallback() {
        // W2.2(b): databases written before this change stored `wall_clock_time`
        // as an RFC3339 String. The forever-fallback deserializer must still read
        // those, and the current epoch-micros layout must round-trip through the
        // same reader. Flips on pre-change code: `deserialize_snapshot_metadata`
        // and `wall_clock_micros` did not exist.
        #[derive(serde::Serialize)]
        struct OldSnapshotMetadata {
            timestamp: u64,
            transaction_id: u64,
            scn: u64,
            wall_clock_time: String,
            active_transactions: u64,
            gc_eligible: bool,
        }

        let rfc = "2020-01-02T03:04:05.123456+00:00";
        let old = OldSnapshotMetadata {
            timestamp: 100,
            transaction_id: 7,
            scn: 3,
            wall_clock_time: rfc.to_string(),
            active_transactions: 0,
            gc_eligible: true,
        };
        let legacy_bytes = bincode::serialize(&old).unwrap();

        let decoded = deserialize_snapshot_metadata(&legacy_bytes).expect("legacy RFC3339 metadata must decode");
        assert_eq!(decoded.timestamp, 100);
        assert_eq!(decoded.transaction_id, 7);
        assert_eq!(decoded.scn, 3);
        assert!(decoded.gc_eligible);
        let expected_micros = DateTime::parse_from_rfc3339(rfc).unwrap().timestamp_micros();
        assert_eq!(decoded.wall_clock_micros, expected_micros);
        assert_eq!(decoded.wall_clock_unix_secs(), expected_micros / 1_000_000);

        let current = SnapshotMetadata::new(200, 9, 4);
        let current_bytes = bincode::serialize(&current).unwrap();
        let redecoded = deserialize_snapshot_metadata(&current_bytes).expect("current metadata must decode");
        assert_eq!(redecoded.timestamp, 200);
        assert_eq!(redecoded.transaction_id, 9);
        assert_eq!(redecoded.scn, 4);
        assert_eq!(redecoded.wall_clock_micros, current.wall_clock_micros);
    }

    #[test]
    fn test_resolve_timestamp_via_reconstructed_rfc3339() {
        // W2.2(b): the RFC3339 string display/API surfaces reconstruct from
        // epoch-micros must still resolve back to the same snapshot.
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db);
        let metadata = manager.register_snapshot(500).unwrap();
        let ts_str = metadata.wall_clock_rfc3339();
        assert_eq!(manager.resolve_timestamp(&ts_str).unwrap(), 500);
    }

    #[test]
    fn test_snapshot_cache_invalidated_on_new_version() {
        // W2.2(c): the per-table generation bump must invalidate a cached read
        // once a newer version is written. Flips if the generation is not bumped
        // (the stale cached value `A` would be served instead of `B`).
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db);

        manager.write_version("t", 1, 100, b"A").unwrap();
        // Populate the cache for the open-ended snapshot.
        assert_eq!(manager.read_at_snapshot("t", 1, u64::MAX).unwrap(), Some(b"A".to_vec()));

        // A newer version bumps the table's cache generation.
        manager.write_version("t", 1, 200, b"B").unwrap();

        // The previously cached entry is now stale and must be recomputed to B.
        assert_eq!(manager.read_at_snapshot("t", 1, u64::MAX).unwrap(), Some(b"B".to_vec()));
    }

    // ----- W3.2: single-copy latest version (elide_latest_version) -----

    #[test]
    fn test_w3_2_encode_decode_flag_roundtrip_and_legacy_fallback() {
        // The flag round-trips, a flag-clear encoding is byte-identical to the
        // legacy raw big-endian ts, and a raw (never-flagged) value decodes as
        // CLEAR — the forever-fallback. Flips on pre-W3.2 code (helpers absent).
        for ts in [1u64, 100, 1_700_000_000_000_000, (1u64 << 62) - 1] {
            let elided = encode_version_index_value(ts, true);
            let full = encode_version_index_value(ts, false);
            assert_eq!(full, ts.to_be_bytes(), "flag-clear encoding IS the legacy layout");
            assert_ne!(elided, full, "the flag must change the bytes");
            assert_eq!(decode_version_index_value(&elided), Some((ts, true)));
            assert_eq!(decode_version_index_value(&full), Some((ts, false)));
            assert_eq!(
                decode_version_index_value(&ts.to_be_bytes()),
                Some((ts, false)),
                "a legacy raw value decodes as flag CLEAR"
            );
        }
        assert_eq!(decode_version_index_value(&[0u8; 4]), None, "a short value is rejected");
    }

    #[test]
    fn test_w3_2_elided_insert_omits_v_and_reads_from_data() {
        // With elision on, a versioned INSERT writes NO `v:` copy — only a flagged
        // `v_idx:` event — and AS-OF reads resolve via `data:`. Flips on pre-W3.2
        // code, which always writes a full `v:` (version_keys would be 1, not 0).
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(Arc::clone(&db));
        manager.configure_elision(true);

        manager
            .write_data_version_and_register_snapshot(b"data:t:1", b"A", "t", 1, 100, b"A", Some(1), None, true)
            .unwrap();

        assert!(db.get(b"v:t:1:100").unwrap().is_none(), "the `v:` copy must be elided");
        let stats = crate::storage::version_gc::version_storage_stats(&db).unwrap();
        assert_eq!(stats.version_keys, 0, "no `v:` key ⇒ nothing for version-GC to reclaim");
        assert_eq!(stats.version_index_keys, 1, "the `v_idx:` event survives");

        assert_eq!(manager.read_at_snapshot("t", 1, 150).unwrap(), Some(b"A".to_vec()));
        assert_eq!(
            manager.read_at_snapshot("t", 1, 50).unwrap(),
            None,
            "not visible before the insert ts"
        );
    }

    #[test]
    fn test_w3_2_materialize_on_first_overwrite_preserves_as_of_history() {
        // The first mutation materializes the elided insert version from `data:`
        // and clears the flag BEFORE `data:` is overwritten, so AS-OF at the
        // insert ts keeps returning the pre-overwrite value.
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(Arc::clone(&db));
        manager.configure_elision(true);
        manager
            .write_data_version_and_register_snapshot(b"data:t:1", b"A", "t", 1, 100, b"A", Some(1), None, true)
            .unwrap();

        // Mutation-path order: materialize first, then overwrite `data:`.
        manager.materialize_copy_marker_row_durable("t", 1).unwrap();
        db.put(b"data:t:1", b"B").unwrap();

        assert_eq!(
            db.get(b"v:t:1:100").unwrap().as_deref(),
            Some(b"A".as_slice()),
            "insert `v:` materialized"
        );
        assert_eq!(
            manager.read_at_snapshot("t", 1, 150).unwrap(),
            Some(b"A".to_vec()),
            "AS-OF survives overwrite"
        );

        // Idempotent: a second materialize is a no-op (newest event now clear).
        manager.materialize_copy_marker_row_durable("t", 1).unwrap();
        assert_eq!(manager.read_at_snapshot("t", 1, 150).unwrap(), Some(b"A".to_vec()));
    }

    #[test]
    fn test_w3_2_mixed_flagged_and_full_rows_resolve_per_row() {
        // A flagged (elided) row and a full-`v:` row in the same table resolve by
        // their own per-row flag — the transition is per-row, not global.
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(Arc::clone(&db));
        manager.configure_elision(true);

        manager
            .write_data_version_and_register_snapshot(b"data:t:1", b"one", "t", 1, 100, b"one", Some(1), None, true)
            .unwrap();
        // Row 2 with allow_elide=false (the side-storage / legacy path): full `v:`.
        manager
            .write_data_version_and_register_snapshot(b"data:t:2", b"two", "t", 2, 100, b"two", Some(2), None, false)
            .unwrap();

        assert!(db.get(b"v:t:1:100").unwrap().is_none(), "row 1 elided");
        assert_eq!(
            db.get(b"v:t:2:100").unwrap().as_deref(),
            Some(b"two".as_slice()),
            "row 2 full `v:`"
        );
        assert_eq!(manager.read_at_snapshot("t", 1, 150).unwrap(), Some(b"one".to_vec()));
        assert_eq!(manager.read_at_snapshot("t", 2, 150).unwrap(), Some(b"two".to_vec()));
    }

    #[test]
    fn test_w3_2_legacy_unflagged_value_reads_v_forever_fallback() {
        // A pre-W3.2 database wrote `v_idx:` values as the raw big-endian ts (high
        // bit clear) alongside a full `v:`. The new decoder must read those via
        // `v:` forever — never as an elided `data:` read.
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(Arc::clone(&db));
        db.put(b"v:t:1:100", b"legacy").unwrap();
        db.put(
            format!("v_idx:t:1:{:020}", u64::MAX - 100).as_bytes(),
            100u64.to_be_bytes(),
        )
        .unwrap();
        // A DIFFERENT current `data:` proves `v:` (not `data:`) is read.
        db.put(b"data:t:1", b"current").unwrap();

        assert_eq!(
            manager.read_at_snapshot("t", 1, 150).unwrap(),
            Some(b"legacy".to_vec()),
            "an unflagged legacy value must resolve via `v:`, not `data:`"
        );
    }

    #[test]
    fn test_w3_2_elision_sentinel_arms_gate_across_reopen() {
        // The durable sentinel makes a reopen with elision OFF still materialize a
        // row a prior session left flagged — without it, `maybe_elided_rows` would
        // be false and the overwrite would silently destroy the AS-OF value.
        let temp = tempdir().unwrap();
        let path = temp.path();
        {
            let mut opts = rocksdb::Options::default();
            opts.create_if_missing(true);
            let db = Arc::new(DB::open(&opts, path).unwrap());
            let manager = SnapshotManager::new(Arc::clone(&db));
            manager.configure_elision(true); // session 1: elision ON
            manager
                .write_data_version_and_register_snapshot(b"data:t:1", b"A", "t", 1, 100, b"A", Some(1), None, true)
                .unwrap();
            assert!(db.get(ELIDE_SENTINEL_KEY).unwrap().is_some(), "sentinel persisted");
        }
        {
            let mut opts = rocksdb::Options::default();
            opts.create_if_missing(true);
            let db = Arc::new(DB::open(&opts, path).unwrap());
            let manager = SnapshotManager::new(Arc::clone(&db));
            manager.configure_elision(false); // session 2: elision OFF
            assert!(
                manager.maybe_elided_rows(),
                "the sentinel must re-arm the materialize gate"
            );

            manager.materialize_copy_marker_row_durable("t", 1).unwrap();
            db.put(b"data:t:1", b"B").unwrap();
            assert_eq!(
                manager.read_at_snapshot("t", 1, 150).unwrap(),
                Some(b"A".to_vec()),
                "AS-OF at the insert ts must survive the overwrite via the materialized `v:`"
            );
        }
    }
}
