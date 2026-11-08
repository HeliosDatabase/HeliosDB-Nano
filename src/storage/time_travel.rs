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
use super::write_gate::WriteGate;
use crate::crypto::KeyManager;
use crate::{Error, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use lru::LruCache;
use parking_lot::{Mutex, RwLock};
use rocksdb::{ReadOptions, WriteBatch, WriteOptions, DB};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

/// System Change Number (Oracle-compatible)
pub type Scn = u64;

/// Transaction ID
pub type TransactionId = u64;

/// A commit's pending key, if present: outer `None` means read the committed
/// store, inner `None` means this commit deletes the key. Used only to decode a
/// NEW physical row; old-row materialization always reads committed sidecars.
pub(crate) type PendingHistoryRead<'a> = dyn Fn(&[u8]) -> Option<Option<Vec<u8>>> + 'a;

/// The engine's current-row decoder, without owning the engine itself. This is
/// a WRITE-TIME interface: its columnar reads are valid only while the caller
/// still holds the old sidecars intact, or supplies the new commit's overlay.
/// It must never be used to resolve a historical `ColumnarRef`.
pub(crate) trait CurrentRowHistoryCodec: Send + Sync {
    fn forget_committed_dictionaries(&self);
    fn dictionary_writer_lock(&self) -> parking_lot::ReentrantMutexGuard<'_, ()>;
    fn validate_staged_dictionary(&self, key: &[u8], proposed: &[u8]) -> Result<()>;
    fn table_uses_side_storage(&self, table: &str) -> Result<bool>;
    /// A retained tuple is ALREADY in today's layout. Only immutable dictionary
    /// IDs and CAS hashes may be resolved; historical columnar slots never are.
    fn logical_retained_tuple(&self, table: &str, row: u64, tuple: &mut crate::Tuple) -> Result<()>;

    /// `None` preserves the caller's bytes (the row is already logical).
    fn logical_row(
        &self,
        table: &str,
        row: u64,
        plain: &[u8],
        pending: Option<&PendingHistoryRead<'_>>,
    ) -> Result<Option<Vec<u8>>>;
}

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

/// Sprinter 6bc4a4040eb7 (B4): the trailing byte of a DELETE's `v_idx:` event,
/// after the 8-byte big-endian commit timestamp. A delete event has no `v:` key: it
/// says the row is absent from this time on.
///
/// A trailing byte, not a flag in the timestamp: a binary that predates it decodes
/// the first eight bytes, finds no `v:` key and answers "absent", which is what the
/// event means, and every other reader of a `v_idx:` value already takes the first
/// eight bytes (`decode_version_index_value`).
const DELETE_EVENT_TAG: u8 = b'D';

/// The value of a delete event of commit `commit_ts`.
#[inline]
pub(crate) fn encode_delete_event(commit_ts: u64) -> [u8; 9] {
    let mut event = [DELETE_EVENT_TAG; 9];
    for (slot, byte) in event.iter_mut().zip(commit_ts.to_be_bytes()) {
        *slot = byte;
    }
    event
}

/// Spec R5.2: the timestamp of the pre-image a rewrite writes for a row with no
/// version at all (`SnapshotManager::stage_rewritten_row_version`), older than any
/// snapshot. Its CONTENT is the row as the rewrite found it, so its shape is that of
/// the rewrite's time, not of this timestamp (phase B4's anchor rule asks).
pub(crate) const REWRITE_PRE_IMAGE_TS: u64 = 0;

/// Is this `v_idx:` value a delete event?
#[inline]
pub(crate) fn is_delete_event(value: &[u8]) -> bool {
    value.len() == 9 && value.get(8) == Some(&DELETE_EVENT_TAG)
}

/// Sprinter 6bc4a4040eb7 (B4 fix pass): read options for a walk that stays inside
/// `prefix`. Total order, and an upper bound at the prefix's end: without one, finding
/// the first key at or after a seek steps through every deleted key past the prefix
/// before it gives up, and a history purge or a branch reclaim leaves long runs of them
/// (until compaction) right after a table's or a level's keys. A per-row probe paid that
/// run once per row. No range deletes: they cut prefix-mode walks short
/// (`tests/prefix_mode_scans.rs`).
pub(crate) fn bounded_read_options(prefix: &[u8]) -> ReadOptions {
    let mut read = ReadOptions::default();
    read.set_total_order_seek(true);
    if let Some(end) = prefix_end(prefix) {
        read.set_iterate_upper_bound(end);
    }
    read
}

/// The first key past every key that starts with `prefix`; `None` for none.
pub(crate) fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if let Some(next) = last.checked_add(1) {
            end.push(next);
            return Some(end);
        }
    }
    None
}

/// The `v_idx:` key of an event of row `row_id` of `table` at `commit_ts`.
pub(crate) fn version_index_key(table: &str, row_id: u64, commit_ts: u64) -> String {
    format!("v_idx:{}:{}:{:020}", table, row_id, u64::MAX - commit_ts)
}

/// The key family of the `snapshot:{timestamp}` records.
const SNAPSHOT_RECORD_PREFIX: &[u8] = b"snapshot:";

/// The snapshot index: beside every `snapshot:{timestamp}` record, in the same
/// write, `scn_map:@{wall_secs:020}:{timestamp:020}` holding the snapshot's SCN
/// and transaction id (two big-endian u64). The record keys are the timestamp in
/// decimal, which neither sorts by number nor names the wall-clock second, SCN
/// or transaction id a lookup starts from, so a lookup the in-memory map could
/// not answer walked every record (about 1 ms per 1000 commits, without bound
/// under infinite retention). On this index `AS OF TIMESTAMP` and a `VERSIONS
/// BETWEEN` bound are one seek. `AS OF SCN` / `AS OF TRANSACTION` use the
/// ID-ordered indexes below: wall clock and commit registration order do not
/// establish any ordering of their IDs.
///
/// `scn_map:` is an existing, classified family (`KeyClass::SnapshotMap`: the
/// legacy id maps R1.4 stopped writing, `scn_map:{scn}` in decimal): a physical
/// standby replicates it and derives nothing from it, and recovery skips it. The
/// `@` keeps the index apart from the legacy keys, which begin with a digit.
const SNAPSHOT_INDEX_PREFIX: &[u8] = b"scn_map:@";

/// `scn_map:@~` (after every index key): every `snapshot:` record at or below
/// this timestamp (u64 BE) has its index key. Records above it are given one at
/// open (`recover_snapshots_with_policy`): those of a store an older binary
/// wrote, and those since the last GC moved it.
const SNAPSHOT_INDEX_MARKER: &[u8] = b"scn_map:@~";

/// The index key of a snapshot of wall-clock second `wall_secs` and `timestamp`.
fn snapshot_index_key(wall_secs: u64, timestamp: u64) -> String {
    format!("scn_map:@{wall_secs:020}:{timestamp:020}")
}

/// The index key of `metadata` (a second before the epoch indexes as 0).
fn snapshot_index_key_of(metadata: &SnapshotMetadata) -> String {
    snapshot_index_key(
        u64::try_from(metadata.wall_clock_unix_secs()).unwrap_or(0),
        metadata.timestamp,
    )
}

/// The index value of `metadata`: its SCN, then its transaction id.
fn snapshot_index_value(metadata: &SnapshotMetadata) -> [u8; 16] {
    let mut value = [0u8; 16];
    for (slot, byte) in value.iter_mut().zip(
        metadata
            .scn
            .to_be_bytes()
            .into_iter()
            .chain(metadata.transaction_id.to_be_bytes()),
    ) {
        *slot = byte;
    }
    value
}

/// Read options confined to the index (`SNAPSHOT_INDEX_PREFIX`): total order,
/// both bounds, so a seek or step never leaves it (`tests/prefix_mode_scans.rs`).
fn index_read_options() -> ReadOptions {
    let mut read = bounded_read_options(SNAPSHOT_INDEX_PREFIX);
    read.set_iterate_lower_bound(SNAPSHOT_INDEX_PREFIX.to_vec());
    // The wall-clock keys start with a fixed-width decimal number. Keep marker
    // and ID-index subspaces out of these iterators, including a forward lookup
    // beyond the newest wall time (which must not walk every ID key on a miss).
    read.set_iterate_upper_bound(b"scn_map:@:".to_vec());
    read
}

/// Separate completeness marker for the exact ID indexes. Older builds may have
/// a wall-clock index marker without any ID keys. Open backfills them before this
/// marker is written; a read-only physical image without it falls back to records.
const SNAPSHOT_ID_INDEX_MARKER: &[u8] = b"scn_map:@id-v1";

/// An ID and timestamp are both encoded at fixed width, so one reverse seek gives
/// its newest retained snapshot, regardless of wall-clock rollback, concurrent
/// registration order, or how many snapshots reuse a transaction ID. A key per
/// occurrence means GC deletes only that snapshot's mapping, without a read/modify/
/// write race against a newer registration. The keys stay in the existing replicated
/// `SnapshotMap` family and ride the snapshot record's batch.
#[derive(Clone, Copy)]
enum SnapshotId {
    Scn(Scn),
    Transaction(TransactionId),
}

impl SnapshotId {
    fn prefix(self) -> String {
        match self {
            Self::Scn(id) => format!("scn_map:@s:{id:020}:"),
            Self::Transaction(id) => format!("scn_map:@t:{id:020}:"),
        }
    }

    fn key(self, timestamp: u64) -> String {
        format!("{}{timestamp:020}", self.prefix())
    }
}

fn put_snapshot_id_indexes(batch: &mut WriteBatch, metadata: &SnapshotMetadata) {
    batch.put(SnapshotId::Scn(metadata.scn).key(metadata.timestamp).as_bytes(), []);
    batch.put(
        SnapshotId::Transaction(metadata.transaction_id)
            .key(metadata.timestamp)
            .as_bytes(),
        [],
    );
}

fn delete_snapshot_indexes(batch: &mut WriteBatch, metadata: &SnapshotMetadata) {
    batch.delete(snapshot_index_key_of(metadata).as_bytes());
    batch.delete(SnapshotId::Scn(metadata.scn).key(metadata.timestamp).as_bytes());
    batch.delete(
        SnapshotId::Transaction(metadata.transaction_id)
            .key(metadata.timestamp)
            .as_bytes(),
    );
}

/// One snapshot index entry ([`SNAPSHOT_INDEX_PREFIX`]).
#[derive(Debug, Clone, Copy)]
struct IndexEntry {
    wall_secs: u64,
    timestamp: u64,
}

impl IndexEntry {
    /// The entry `key` / `value` hold, or `None` for anything else in the family
    /// (the marker).
    fn parse(key: &[u8], value: &[u8]) -> Option<Self> {
        // Retain the value-format check even though time lookups need only the key.
        if value.len() != 16 {
            return None;
        }
        let rest = key.strip_prefix(SNAPSHOT_INDEX_PREFIX)?;
        if rest.len() != 41 || rest.get(20) != Some(&b':') {
            return None;
        }
        let number = |digits: &[u8]| -> Option<u64> { std::str::from_utf8(digits).ok()?.parse().ok() };
        Some(Self {
            wall_secs: number(rest.get(..20)?)?,
            timestamp: number(rest.get(21..)?)?,
        })
    }

    fn wall_secs_i64(&self) -> i64 {
        i64::try_from(self.wall_secs).unwrap_or(i64::MAX)
    }
}

/// The snapshots the in-memory map has let go of by count, or left out at
/// open-time recovery, while their `snapshot:` records stay in the store
/// (`SnapshotManager::evict_from_memory`, `recover_snapshots_with_policy`).
///
/// The map keeps the newest `max_snapshots` (plus slack) so it stays bounded on
/// a busy store; it used to drop the records with them, so `AS OF` reached only
/// the last ~1000 snapshots, however recent the anchor. A record now lives
/// until the version-GC low watermark passes it (`gc_old_snapshots`). A lookup
/// the map cannot answer reads the snapshot index ([`SNAPSHOT_INDEX_PREFIX`]),
/// and these watermarks say when it cannot: every record the store holds and
/// the map does not is at or below `through_ts` and `max_wall_secs`.
#[derive(Default)]
struct EvictedSnapshots {
    /// Whether any snapshot was let go of by count.
    any: AtomicBool,
    /// Whether the store may hold records without index keys that the index
    /// cannot be trusted to cover: a physical image of a primary that never
    /// indexed (no marker), or an open that could not write the missing keys.
    /// Lookups then walk the records, as they did before the index.
    unindexed: AtomicBool,
    /// The ID indexes may be absent even when the older wall-clock index exists.
    ids_unindexed: AtomicBool,
    /// Index entries and records read by lookups the map could not answer: what
    /// the tests count to show a lookup's cost does not grow with history.
    records_read: AtomicU64,
    /// The newest timestamp let go of.
    through_ts: AtomicU64,
    /// The latest wall-clock second (`wall_clock_unix_secs`) let go of.
    max_wall_secs: AtomicI64,
    /// Where the next sweep of expired records resumes; `None` = the start of
    /// the family (`SnapshotManager::sweep_expired_records`).
    sweep_cursor: Mutex<Option<Vec<u8>>>,
}

/// The wall-clock span of the snapshots a failed `AS OF TIMESTAMP` lookup saw,
/// for its error.
#[derive(Default, Clone, Copy)]
struct SnapshotSpan {
    count: usize,
    earliest: Option<i64>,
    latest: Option<i64>,
    /// The span came from the index, which does not count: the error then
    /// names no count.
    uncounted: bool,
}

impl SnapshotSpan {
    fn add(&mut self, wall_secs: i64) {
        self.count += 1;
        self.earliest = Some(self.earliest.map_or(wall_secs, |e| e.min(wall_secs)));
        self.latest = Some(self.latest.map_or(wall_secs, |l| l.max(wall_secs)));
    }

    /// `self` (the in-memory map) widened by `stored` (the store's records,
    /// a superset of the map's on a persistent store).
    fn widened(self, stored: Self) -> Self {
        let pick = |a: Option<i64>, b: Option<i64>, f: fn(i64, i64) -> i64| match (a, b) {
            (Some(a), Some(b)) => Some(f(a, b)),
            (a, b) => a.or(b),
        };
        Self {
            count: self.count.max(stored.count),
            earliest: pick(self.earliest, stored.earliest, i64::min),
            latest: pick(self.latest, stored.latest, i64::max),
            uncounted: self.uncounted || stored.uncounted,
        }
    }
}

/// Spec R5.4: a row's version and `snapshot:` record staged into a caller's write
/// group (`SnapshotManager::stage_data_version_snapshot`), to be finished once the
/// group is written (`SnapshotManager::finish_staged_version`).
#[derive(Debug)]
pub struct StagedVersion {
    table_name: String,
    row_id: u64,
    timestamp: u64,
    txn_id: TransactionId,
    scn: Scn,
    metadata: SnapshotMetadata,
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

/// Spec R5.5: where a snapshot read found a row's image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSource {
    /// A version (`v:`) the row's commit at this timestamp wrote: the layout of
    /// that time (`storage::table_layout`).
    Version(u64),
    /// The row's `data:` image: an elided latest version, a COPY-marker row, a row
    /// with no version at all. Today's layout.
    Current,
}

/// A cached snapshot read: the generation it was read at, and what it found.
type CachedSnapshotRead = (u64, Option<(ImageSource, Vec<u8>)>);

/// Snapshot Manager
///
/// Manages historical snapshots for time-travel queries.
pub struct SnapshotManager {
    /// Database handle. Written only through `gate`.
    db: Arc<DB>,
    /// The write gate of the engine that owns `db`.
    gate: Arc<WriteGate>,
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
    /// The newest timestamp registered as a snapshot.
    newest_snapshot_ts: AtomicU64,
    /// Current transaction ID counter (W2.2(d): lock-free `AtomicU64`, see above).
    current_txn_id: Arc<AtomicU64>,
    /// Snapshot read cache for performance. W2.2(c): each entry carries the
    /// per-table generation it was computed at (`snapshot_cache_gen`); a stale
    /// entry is rejected on read and overwritten, replacing the former
    /// per-write O(cache-size) linear scan of `invalidate_cache_for_row`.
    snapshot_cache: Arc<Mutex<LruCache<SnapshotCacheKey, CachedSnapshotRead>>>,
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
    /// in process and do not need it for recovery, and neither does a physical
    /// image: its `snapshot:` keys are the primary's, and a snapshot
    /// registered by a read-only COMMIT on it must stay in memory (see
    /// [`Self::set_persistence`]).
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
    /// Shares only the engine's DB/schema/dictionary handles, never an
    /// `Arc<StorageEngine>`, so installing it cannot create an ownership cycle.
    current_row_history_codec: Option<Arc<dyn CurrentRowHistoryCodec>>,
    /// The snapshots the in-memory map let go of by count, whose records the
    /// store keeps for lookups the map cannot answer.
    evicted: EvictedSnapshots,
    /// Spec R5.5: each table's layout record (`meta:table_layout:{t}`) once read, a
    /// table with none too ([`Self::table_layout`]).
    table_layouts: dashmap::DashMap<String, Option<Arc<super::table_layout::TableLayout>>>,
    /// Spec R5.5: moved by every invalidation of [`Self::table_layouts`], before its
    /// entry goes: a load that read the record before a change and lands after it is
    /// refused.
    table_layouts_epoch: AtomicU64,
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
    /// Minimum retention period (seconds): a floor. A snapshot is removed
    /// only once the version-GC low watermark has passed it (the configured
    /// `storage.version_retention`) AND it is older than this.
    pub min_retention_seconds: u64,
    /// Maximum number of snapshots the in-memory map keeps (it is let grow to
    /// twice this, at most 1000 more, before GC runs); the records of the
    /// older ones stay in the store, where `AS OF` finds them.
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
    /// Create a new snapshot manager. `gate` is the write gate of the engine
    /// that owns `db` (a handle the caller owns outright: `WriteGate::open()`).
    pub fn new(db: Arc<DB>, gate: Arc<WriteGate>) -> Self {
        let cache_config = CacheConfig::default();
        let cache_size = NonZeroUsize::new(cache_config.max_entries)
            .unwrap_or_else(|| NonZeroUsize::new(1000).unwrap_or(NonZeroUsize::MIN));

        Self {
            db,
            gate,
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            txn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            scn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            current_scn: Arc::new(AtomicU64::new(1)),
            newest_snapshot_ts: AtomicU64::new(0),
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
            current_row_history_codec: None,
            evicted: EvictedSnapshots::default(),
            table_layouts: dashmap::DashMap::new(),
            table_layouts_epoch: AtomicU64::new(0),
        }
    }

    /// Create a snapshot manager for memory-only databases.
    pub fn new_non_durable(db: Arc<DB>, gate: Arc<WriteGate>) -> Self {
        let mut manager = Self::new(db, gate);
        manager.non_durable_writes = true;
        manager.persist_metadata = false;
        manager
    }

    /// Create a new snapshot manager with custom GC config
    pub fn with_gc_config(db: Arc<DB>, gate: Arc<WriteGate>, gc_config: GcConfig) -> Self {
        let cache_config = CacheConfig::default();
        let cache_size = NonZeroUsize::new(cache_config.max_entries)
            .unwrap_or_else(|| NonZeroUsize::new(1000).unwrap_or(NonZeroUsize::MIN));

        Self {
            db,
            gate,
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            txn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            scn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            current_scn: Arc::new(AtomicU64::new(1)),
            newest_snapshot_ts: AtomicU64::new(0),
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
            current_row_history_codec: None,
            evicted: EvictedSnapshots::default(),
            table_layouts: dashmap::DashMap::new(),
            table_layouts_epoch: AtomicU64::new(0),
        }
    }

    /// Create a new snapshot manager with custom cache and GC config
    pub fn with_config(db: Arc<DB>, gate: Arc<WriteGate>, cache_config: CacheConfig, gc_config: GcConfig) -> Self {
        let cache_size = NonZeroUsize::new(cache_config.max_entries)
            .unwrap_or_else(|| NonZeroUsize::new(1000).unwrap_or(NonZeroUsize::MIN));

        Self {
            db,
            gate,
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            txn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            scn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            current_scn: Arc::new(AtomicU64::new(1)),
            newest_snapshot_ts: AtomicU64::new(0),
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
            current_row_history_codec: None,
            evicted: EvictedSnapshots::default(),
            table_layouts: dashmap::DashMap::new(),
            table_layouts_epoch: AtomicU64::new(0),
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

    pub(crate) fn set_current_row_history_codec(&mut self, codec: Arc<dyn CurrentRowHistoryCodec>) {
        self.current_row_history_codec = Some(codec);
    }

    pub(crate) fn forget_committed_dictionaries(&self) {
        if let Some(codec) = &self.current_row_history_codec {
            codec.forget_committed_dictionaries();
        }
    }

    pub(crate) fn dictionary_writer_lock(&self) -> Option<parking_lot::ReentrantMutexGuard<'_, ()>> {
        self.current_row_history_codec
            .as_ref()
            .map(|codec| codec.dictionary_writer_lock())
    }

    pub(crate) fn validate_staged_dictionary(&self, key: &[u8], proposed: &[u8]) -> Result<()> {
        match &self.current_row_history_codec {
            Some(codec) => codec.validate_staged_dictionary(key, proposed),
            None => super::dictionary::DictionaryManager::validate_staged(&self.db, key, proposed),
        }
    }

    /// A current row's logical plaintext for a NEW version. The optional lookup
    /// sees the commit's pending sidecar values; old preimages must pass `None`.
    /// Inline rows keep their original bytes and are never serialized again.
    pub(crate) fn logical_current_row_for_history<'a>(
        &self,
        table: &str,
        row: u64,
        plain: &'a [u8],
        pending: Option<&PendingHistoryRead<'_>>,
    ) -> Result<Cow<'a, [u8]>> {
        match &self.current_row_history_codec {
            Some(codec) => match codec.logical_row(table, row, plain, pending)? {
                Some(logical) => Ok(Cow::Owned(logical)),
                None => Ok(Cow::Borrowed(plain)),
            },
            None => Ok(Cow::Borrowed(plain)),
        }
    }

    /// Preserve an OLD, still-current stored row before any sidecar mutation.
    /// Reuse inline ciphertext verbatim; only a decoded physical row is sealed
    /// again, once, as its logical image. Historical reads never call this.
    fn current_row_history_image<'a>(&self, table: &str, row: u64, stored: &'a [u8]) -> Result<Cow<'a, [u8]>> {
        if !self
            .current_row_history_codec
            .as_ref()
            .map(|codec| codec.table_uses_side_storage(table))
            .transpose()?
            .unwrap_or(false)
        {
            return Ok(Cow::Borrowed(stored));
        }
        let plain = tde::open_ref(
            self.key_manager.as_deref(),
            format!("data:{table}:{row}").as_bytes(),
            stored,
        )?;
        let logical = self.logical_current_row_for_history(table, row, plain.as_ref(), None)?;
        match logical {
            Cow::Borrowed(_) => Ok(Cow::Borrowed(stored)),
            Cow::Owned(logical) => Ok(Cow::Owned(self.seal_stored(&logical)?.into_owned())),
        }
    }

    /// Whether registering (and garbage-collecting) snapshots persists their
    /// `snapshot:` keys. `false` keeps the in-memory maps and counters exactly
    /// as before and writes nothing: the engine sets it for a physical
    /// snapshot or standby, whose store must change only through replication.
    ///
    /// Like [`Self::set_key_manager`], called before the manager is wrapped in
    /// an `Arc` and published.
    pub fn set_persistence(&mut self, enabled: bool) {
        self.persist_metadata = enabled;
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

    /// Normalize a decoded snapshot image AFTER bringing it into today's layout.
    /// A dropped physical field is therefore gone before it could refuse a read.
    /// Current-source images may use current sidecars; retained versions may use
    /// only immutable dictionary/CAS payloads and never mutable columnar slots.
    pub(crate) fn normalize_snapshot_tuple(
        &self,
        table: &str,
        row: u64,
        source: ImageSource,
        tuple: &mut crate::Tuple,
    ) -> Result<bool> {
        if !super::table_layout::has_physical_storage_refs(tuple) {
            return Ok(false);
        }
        let Some(codec) = &self.current_row_history_codec else {
            return super::table_layout::refuse_physical_version(table, row, tuple).map(|()| false);
        };
        match source {
            ImageSource::Version(_) => codec.logical_retained_tuple(table, row, tuple)?,
            ImageSource::Current => {
                let plain = bincode::serialize(tuple)
                    .map_err(|e| Error::storage(format!("snapshot current tuple does not encode: {e}")))?;
                if let Some(logical) = codec.logical_row(table, row, &plain, None)? {
                    *tuple = bincode::deserialize(&logical)
                        .map_err(|e| Error::storage(format!("logical snapshot tuple does not decode: {e}")))?;
                }
                super::table_layout::refuse_physical_version(table, row, tuple)?;
            }
        }
        Ok(true)
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
    /// Recover read-side elision state without installing a sentinel.
    pub(crate) fn configure_elision_read_only(&self) -> Result<()> {
        let sentinel = self
            .db
            .get(ELIDE_SENTINEL_KEY)
            .map_err(|e| Error::storage(format!("elision sentinel read failed: {e}")))?;
        if sentinel.as_deref().is_some_and(|bytes| bytes != [1u8]) {
            return Err(Error::storage("invalid elision sentinel"));
        }
        self.elide_latest_version.store(false, Ordering::Relaxed);
        self.maybe_elided_rows.store(sentinel.is_some(), Ordering::Relaxed);
        Ok(())
    }

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
                match self.gate.put(&self.db, ELIDE_SENTINEL_KEY, [1u8], None) {
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
            self.gate
                .write(&self.db, batch, Some(&opts))
                .map_err(|e| Error::storage(format!("{}: {}", context, e)))
        } else {
            self.gate
                .write(&self.db, batch, None)
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
        self.newest_snapshot_ts.fetch_max(timestamp, Ordering::AcqRel);

        // Store in-memory
        self.snapshots.write().insert(timestamp, metadata.clone());
        self.map_transaction_timestamp(txn_id, timestamp);
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

        // Find the closest snapshot <= target time: the latest wall-clock
        // second at or before it (W2.2(b): epoch-micros metadata, seconds
        // granularity, as before), the newest snapshot of that second. In the
        // in-memory map first; in the store's records when the map has let go
        // of one that could be closer (`memory_answers`).
        let target_secs = i64::try_from(target_time).unwrap_or(i64::MAX);
        let mut best: Option<(i64, u64)> = None;
        let mut span = SnapshotSpan::default();
        let memory_answers = {
            let snapshots = self.snapshots.read();
            for metadata in snapshots.values() {
                span.add(metadata.wall_clock_unix_secs());
                Self::closer_at_or_before(&mut best, metadata, target_secs);
            }
            self.memory_answers(best)
        };
        if !memory_answers {
            span = span.widened(self.stored_closest_at_or_before(&mut best, target_secs)?);
        }

        best.map(|(_, timestamp)| timestamp).ok_or_else(|| {
            // Say WHY: the instant the literal was read as, and what exists.
            // "No snapshot found for timestamp" gave the user nothing to act on.
            let fmt = |s: i64| {
                chrono::DateTime::from_timestamp(s, 0)
                    .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                    .unwrap_or_else(|| s.to_string())
            };
            let target = fmt(target_time as i64);
            // Snapshots below the version-GC low watermark are deleted with the
            // history they resolved to (`gc_old_snapshots`): say so.
            let collected = if self.gc_low_watermark() > 0 {
                " History older than storage.version_retention has been garbage collected."
            } else {
                ""
            };
            let counted = if span.uncounted {
                String::new()
            } else {
                format!(" ({} snapshots)", span.count)
            };
            match (span.earliest, span.latest) {
                (Some(lo), Some(hi)) => Error::query_execution(format!(
                    "No snapshot at or before {target} (literal '{ts_str}' interpreted as UTC unless it \
                     carries an offset); earliest snapshot {}, latest {}{counted}. Use an explicit \
                     offset such as '… -03:00', a later timestamp, or AS OF TRANSACTION <id>.{collected}",
                    fmt(lo),
                    fmt(hi),
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

                // The start is the earliest snapshot at or after the target, the
                // end the latest at or before it (W2.2(b): epoch-micros metadata,
                // seconds granularity, as before); in the store's records too when
                // the in-memory map has let go of one that could be it.
                let target_secs = i64::try_from(target_time).unwrap_or(i64::MAX);
                let mut best_match: Option<u64> = None;
                let (memory_empty, memory_answers) = {
                    let snapshots = self.snapshots.read();
                    for metadata in snapshots.values() {
                        Self::range_candidate(&mut best_match, metadata, target_secs, is_start);
                    }
                    (
                        snapshots.is_empty(),
                        self.memory_answers_range(best_match, target_secs, is_start),
                    )
                };
                if memory_empty && memory_answers {
                    // No snapshots - use boundary values for full range
                    return Ok(if is_start { 0 } else { u64::MAX });
                }
                if !memory_answers {
                    self.stored_range_candidate(&mut best_match, target_secs, is_start)?;
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

    /// Resolve transaction ID to snapshot timestamp: the newest snapshot that
    /// carries it, from the store's records when the in-memory map has let go
    /// of one that may be newer than the one it maps the id to.
    fn resolve_transaction(&self, txn_id: TransactionId) -> Result<u64> {
        let mut found = self.txn_to_timestamp.read().get(&txn_id).copied();
        if !self.memory_holds_every_snapshot_after(found) {
            let mut newer = |timestamp: u64| {
                if found.is_none_or(|ts| timestamp > ts) {
                    found = Some(timestamp);
                }
            };
            if self.id_index_trusted() {
                if let Some(timestamp) = self.newest_indexed_id(SnapshotId::Transaction(txn_id))? {
                    newer(timestamp);
                }
            } else {
                self.for_each_stored_snapshot(|metadata| {
                    if metadata.transaction_id == txn_id {
                        newer(metadata.timestamp);
                    }
                })?;
            }
        }
        found.ok_or_else(|| {
            Error::query_execution(format!("Transaction {txn_id} not found or has been garbage collected"))
        })
    }

    /// Resolve SCN to snapshot timestamp (an SCN names one snapshot; from the
    /// store's records when the in-memory map has let go of it).
    pub fn resolve_scn(&self, scn: Scn) -> Result<u64> {
        let mut found = self.scn_to_timestamp.read().get(&scn).copied();
        if found.is_none() && self.evicted.any.load(Ordering::Acquire) {
            if self.id_index_trusted() {
                found = self.newest_indexed_id(SnapshotId::Scn(scn))?;
            } else {
                self.for_each_stored_snapshot(|metadata| {
                    if metadata.scn == scn {
                        found = Some(metadata.timestamp);
                    }
                })?;
            }
        }
        found.ok_or_else(|| Error::query_execution(format!("SCN {scn} not found or has been garbage collected")))
    }

    /// `AS OF TIMESTAMP`'s choice: keep `metadata` in `best` (as `(wall-clock
    /// second, timestamp)`) when it is at or before `target_secs` and later than
    /// `best` — a later second, or the same second and a newer snapshot.
    fn closer_at_or_before(best: &mut Option<(i64, u64)>, metadata: &SnapshotMetadata, target_secs: i64) {
        let candidate = (metadata.wall_clock_unix_secs(), metadata.timestamp);
        if candidate.0 <= target_secs && best.is_none_or(|b| candidate > b) {
            *best = Some(candidate);
        }
    }

    /// `VERSIONS BETWEEN`'s choice: the earliest snapshot at or after
    /// `target_secs` for the start, the latest at or before it for the end.
    fn range_candidate(best: &mut Option<u64>, metadata: &SnapshotMetadata, target_secs: i64, is_start: bool) {
        let wall_secs = metadata.wall_clock_unix_secs();
        let timestamp = metadata.timestamp;
        let better = if is_start {
            wall_secs >= target_secs && best.is_none_or(|b| timestamp < b)
        } else {
            wall_secs <= target_secs && best.is_none_or(|b| timestamp > b)
        };
        if better {
            *best = Some(timestamp);
        }
    }

    /// Whether the in-memory map alone answers an `AS OF TIMESTAMP` lookup whose
    /// best in-memory candidate is `best`: nothing was let go of, or `best` is
    /// later, by wall clock and then by timestamp, than everything that was (so
    /// no record the map lacks can be closer to the target). Call it under the
    /// map's lock, after the walk: eviction moves the watermarks before it
    /// removes an entry.
    fn memory_answers(&self, best: Option<(i64, u64)>) -> bool {
        if !self.evicted.any.load(Ordering::Acquire) {
            return true;
        }
        let evicted = (
            self.evicted.max_wall_secs.load(Ordering::Acquire),
            self.evicted.through_ts.load(Ordering::Acquire),
        );
        best.is_some_and(|candidate| candidate > evicted)
    }

    /// Whether no snapshot the in-memory map let go of is newer than `found`.
    fn memory_holds_every_snapshot_after(&self, found: Option<u64>) -> bool {
        !self.evicted.any.load(Ordering::Acquire)
            || found.is_some_and(|ts| ts > self.evicted.through_ts.load(Ordering::Acquire))
    }

    /// [`Self::memory_answers`] for a `VERSIONS BETWEEN` bound: a start no
    /// evicted snapshot is late enough for, or an end newer than all of them.
    fn memory_answers_range(&self, best: Option<u64>, target_secs: i64, is_start: bool) -> bool {
        if is_start {
            !self.evicted.any.load(Ordering::Acquire)
                || self.evicted.max_wall_secs.load(Ordering::Acquire) < target_secs
        } else {
            self.memory_holds_every_snapshot_after(best)
        }
    }

    /// Whether the snapshot index covers every record (`EvictedSnapshots::unindexed`).
    fn index_trusted(&self) -> bool {
        !self.evicted.unindexed.load(Ordering::Acquire)
    }

    /// Whether every retained snapshot has both of its exact ID index keys.
    fn id_index_trusted(&self) -> bool {
        !self.evicted.ids_unindexed.load(Ordering::Acquire)
    }

    /// `AS OF TIMESTAMP` beyond the in-memory map: one seek of the index (the
    /// last entry at or before the end of `target_secs`: the latest second, the
    /// newest snapshot of it), or the walk of every record when the index is
    /// not trusted. Returns what it saw, for the error.
    fn stored_closest_at_or_before(&self, best: &mut Option<(i64, u64)>, target_secs: i64) -> Result<SnapshotSpan> {
        let mut stored = SnapshotSpan::default();
        if !self.index_trusted() {
            self.for_each_stored_snapshot(|metadata| {
                stored.add(metadata.wall_clock_unix_secs());
                Self::closer_at_or_before(best, metadata, target_secs);
            })?;
            return Ok(stored);
        }
        stored.uncounted = true;
        let target = u64::try_from(target_secs).unwrap_or(0);
        if let Some(entry) = self.index_entry_near(target, u64::MAX, false)? {
            let candidate = (entry.wall_secs_i64(), entry.timestamp);
            if entry.wall_secs <= target && best.is_none_or(|b| candidate > b) {
                *best = Some(candidate);
            }
        }
        if best.is_none() {
            // Nothing at or before it: what exists starts at the first entry.
            if let Some(first) = self.index_entry_near(0, 0, true)? {
                stored.earliest = Some(first.wall_secs_i64());
                stored.latest = Some(first.wall_secs_i64());
            }
        }
        Ok(stored)
    }

    /// A `VERSIONS BETWEEN` bound beyond the in-memory map: one seek of the
    /// index (the first entry at or after `target_secs` for the start, the last
    /// at or before it for the end), or the walk when the index is not trusted.
    fn stored_range_candidate(&self, best: &mut Option<u64>, target_secs: i64, is_start: bool) -> Result<()> {
        if !self.index_trusted() {
            return self.for_each_stored_snapshot(|metadata| {
                Self::range_candidate(best, metadata, target_secs, is_start);
            });
        }
        let target = u64::try_from(target_secs).unwrap_or(0);
        let entry = if is_start {
            self.index_entry_near(target, 0, true)?
                .filter(|entry| i64::try_from(entry.wall_secs).is_ok_and(|secs| secs >= target_secs))
        } else {
            self.index_entry_near(target, u64::MAX, false)?
                .filter(|entry| entry.wall_secs <= target)
        };
        if let Some(entry) = entry {
            let better = if is_start {
                best.is_none_or(|b| entry.timestamp < b)
            } else {
                best.is_none_or(|b| entry.timestamp > b)
            };
            if better {
                *best = Some(entry.timestamp);
            }
        }
        Ok(())
    }

    /// The index entry nearest the key of `(wall_secs, timestamp)`: the first at
    /// or after it (`forward`), or the last at or before it. One seek.
    fn index_entry_near(&self, wall_secs: u64, timestamp: u64, forward: bool) -> Result<Option<IndexEntry>> {
        let mut iter = self.db.raw_iterator_opt(index_read_options());
        let key = snapshot_index_key(wall_secs, timestamp);
        if forward {
            iter.seek(key.as_bytes());
        } else {
            iter.seek_for_prev(key.as_bytes());
        }
        self.index_entry_at(&mut iter, forward)
    }

    /// The entry the iterator is on, stepping (`forward` or back) past anything
    /// in the family that is not one (the marker).
    fn index_entry_at(&self, iter: &mut rocksdb::DBRawIterator<'_>, forward: bool) -> Result<Option<IndexEntry>> {
        while iter.valid() {
            self.evicted.records_read.fetch_add(1, Ordering::Relaxed);
            if let Some(entry) = Self::index_entry_under(iter) {
                return Ok(Some(entry));
            }
            if forward {
                iter.next();
            } else {
                iter.prev();
            }
        }
        iter.status()
            .map_err(|e| Error::storage(format!("Iterator error reading the snapshot index: {e}")))?;
        Ok(None)
    }

    /// The index entry under the iterator, if it is on one.
    fn index_entry_under(iter: &rocksdb::DBRawIterator<'_>) -> Option<IndexEntry> {
        iter.key()
            .zip(iter.value())
            .and_then(|(key, value)| IndexEntry::parse(key, value))
    }

    /// The newest retained occurrence of `id`: one seek, independent of history
    /// length and of how many times a transaction ID was reused. A point in wall-clock
    /// order cannot answer this query: wall time can move backwards and registrations
    /// can publish in a different order than the IDs were allocated.
    fn newest_indexed_id(&self, id: SnapshotId) -> Result<Option<u64>> {
        let prefix = id.prefix();
        let mut read = bounded_read_options(prefix.as_bytes());
        read.set_iterate_lower_bound(prefix.as_bytes().to_vec());
        let mut iter = self.db.raw_iterator_opt(read);
        iter.seek_for_prev(id.key(u64::MAX).as_bytes());
        let found = if let Some(key) = iter.key() {
            self.evicted.records_read.fetch_add(1, Ordering::Relaxed);
            let suffix = key
                .strip_prefix(prefix.as_bytes())
                .ok_or_else(|| Error::storage("snapshot ID index escaped its bounds"))?;
            if suffix.len() != 20 || !suffix.iter().all(u8::is_ascii_digit) {
                return Err(Error::storage("malformed snapshot ID index key"));
            }
            Some(
                std::str::from_utf8(suffix)
                    .ok()
                    .and_then(|text| text.parse::<u64>().ok())
                    .ok_or_else(|| Error::storage("malformed snapshot ID index timestamp"))?,
            )
        } else {
            None
        };
        iter.status()
            .map_err(|e| Error::storage(format!("Iterator error reading the snapshot ID index: {e}")))?;
        if let Some(timestamp) = found {
            // The completeness marker proves that writers supply an index key;
            // it cannot prove that an existing key still describes its record.
            // A stale or corrupt key must never select a different history.
            let key = format!("snapshot:{timestamp}");
            self.evicted.records_read.fetch_add(1, Ordering::Relaxed);
            let value = self
                .db
                .get(key.as_bytes())
                .map_err(|e| Error::storage(format!("snapshot ID index record read failed: {e}")))?
                .ok_or_else(|| Error::storage("snapshot ID index points to a missing snapshot record"))?;
            let metadata = Self::decode_snapshot_record(&key, &value, true)?
                .ok_or_else(|| Error::storage("snapshot ID index points to malformed snapshot metadata"))?;
            let matches_id = match id {
                SnapshotId::Scn(expected) => metadata.scn == expected,
                SnapshotId::Transaction(expected) => metadata.transaction_id == expected,
            };
            if metadata.timestamp != timestamp || !matches_id {
                return Err(Error::storage("snapshot ID index identity mismatch"));
            }
        }
        Ok(found)
    }

    /// Visit every `snapshot:` record in the store, decoded; one that does not
    /// decode is skipped, as the open-time recovery skips it. A bounded walk of
    /// the family (total order, an upper bound at its end; see
    /// `tests/prefix_mode_scans.rs`): the lookups' fallback while the snapshot
    /// index is not trusted (`EvictedSnapshots::unindexed`).
    fn for_each_stored_snapshot(&self, mut visit: impl FnMut(&SnapshotMetadata)) -> Result<()> {
        let iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(SNAPSHOT_RECORD_PREFIX, rocksdb::Direction::Forward),
            bounded_read_options(SNAPSHOT_RECORD_PREFIX),
        );
        for item in iter {
            let (key, value) =
                item.map_err(|e| Error::storage(format!("Iterator error reading snapshot records: {e}")))?;
            if !key.starts_with(SNAPSHOT_RECORD_PREFIX) {
                break;
            }
            self.evicted.records_read.fetch_add(1, Ordering::Relaxed);
            if let Some(metadata) = deserialize_snapshot_metadata(&value) {
                visit(&metadata);
            }
        }
        Ok(())
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

    /// Existing monotonic commit publication, for rare checkpoint race checks.
    /// Pruning retained snapshot metadata cannot move this ticket backwards.
    pub(crate) fn snapshot_publication_ticket(&self) -> u64 {
        self.newest_snapshot_ts.load(Ordering::Acquire)
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

        // Spec P1.1b: a total-order seek to the row's versions. This walked
        // the whole keyspace with prefix-mode options, which a range tombstone
        // anywhere before `v:` could cut short (see
        // `recover_snapshots_with_policy`), losing the versions after it.
        let iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
            bounded_read_options(prefix.as_bytes()),
        );
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }

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

        // Sprinter 6bc4a4040eb7 (B4): a delete event has no `v:` key, so this scan
        // cannot see one: the row is gone at any time at or after it.
        if self.newest_event_is_a_delete(table_name, row_id, snapshot_ts)? {
            return Ok(None);
        }

        // Return the best version found, decoded like every other version read.
        // The scan above keeps only the value, so the exact key is gone by here;
        // the placeholder keeps the real `v:` prefix because the decode rule and
        // the plaintext-passthrough diagnostic are both keyed on the keyspace.
        self.decode_stored_opt(b"v:<version value>", best_version.map(|(_, value)| value))
    }

    /// Is the newest `v_idx:` event of the row at or before `snapshot_ts` its delete?
    fn newest_event_is_a_delete(&self, table_name: &str, row_id: u64, snapshot_ts: u64) -> Result<bool> {
        let seek = version_index_key(table_name, row_id, snapshot_ts);
        let prefix = format!("v_idx:{}:{}:", table_name, row_id);
        let mut iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(seek.as_bytes(), rocksdb::Direction::Forward),
            bounded_read_options(prefix.as_bytes()),
        );
        match iter.next() {
            Some(item) => {
                let (key, value) = item.map_err(|e| Error::storage(format!("Version index read failed: {}", e)))?;
                Ok(key.starts_with(prefix.as_bytes()) && is_delete_event(&value))
            }
            None => Ok(false),
        }
    }

    /// Read a versioned value at a specific snapshot (optimized with reverse index and cache)
    ///
    /// This implements O(log N) time-travel queries using a reverse timestamp index.
    /// The reverse index uses `u64::MAX - timestamp` to enable efficient lookups.
    /// Additionally, uses an LRU cache for frequently accessed snapshots.
    ///
    /// The image is in the layout of the time it was written: a reader that decodes it
    /// under today's schema takes it through [`Self::bring_version_forward`] first, or
    /// reads it with [`Self::read_at_snapshot_in_layout`] (spec R5.5). These raw-byte
    /// APIs preserve the stored layout; legacy physical references are interpreted
    /// only at those decoded-row funnels, after dropped fields have been removed.
    pub fn read_at_snapshot(&self, table_name: &str, row_id: u64, snapshot_ts: u64) -> Result<Option<Vec<u8>>> {
        Ok(self
            .read_at_snapshot_sourced(table_name, row_id, snapshot_ts)?
            .map(|(_, image)| image))
    }

    /// Spec R5.5: [`Self::read_at_snapshot`], and where the image came from: the
    /// version of a commit (its timestamp), or the row's current image.
    pub(crate) fn read_at_snapshot_sourced(
        &self,
        table_name: &str,
        row_id: u64,
        snapshot_ts: u64,
    ) -> Result<Option<(ImageSource, Vec<u8>)>> {
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
        let result = match self.read_at_snapshot_uncached(table_name, row_id, snapshot_ts)? {
            Some((source, stored)) => self
                .decode_stored_opt(b"v:<version value>", Some(stored))?
                .map(|image| (source, image)),
            None => None,
        };

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
    fn read_at_snapshot_uncached(
        &self,
        table_name: &str,
        row_id: u64,
        snapshot_ts: u64,
    ) -> Result<Option<(ImageSource, Vec<u8>)>> {
        let current = |what: &str| -> Result<Option<(ImageSource, Vec<u8>)>> {
            let data_key = format!("data:{}:{}", table_name, row_id);
            self.db
                .get(data_key.as_bytes())
                .map_err(|e| Error::storage(format!("Failed to read {what}: {e}")))
                .map(|opt| opt.map(|v| (ImageSource::Current, v)))
        };
        // Use reverse timestamp index for O(log N) lookup
        // Reverse timestamp allows us to find the latest version <= snapshot_ts
        let reverse_ts = u64::MAX - snapshot_ts;

        // Seek to the reverse timestamp index
        // Index format: v_idx:{table}:{row_id}:{reverse_ts} -> {actual_ts}
        let seek_key = format!("v_idx:{}:{}:{:020}", table_name, row_id, reverse_ts);

        // Since we use reverse timestamps (larger actual_ts -> smaller reverse_ts),
        // we need to seek forward to find versions with actual_ts <= snapshot_ts
        // (which have reverse_ts >= our target reverse_ts)
        let expected_prefix = format!("v_idx:{}:{}:", table_name, row_id);
        let mut iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(seek_key.as_bytes(), rocksdb::Direction::Forward),
            bounded_read_options(expected_prefix.as_bytes()),
        );

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
                            // Sprinter 6bc4a4040eb7 (B4): the newest event at or
                            // before this snapshot is the row's delete.
                            if is_delete_event(&value) {
                                return Ok(None);
                            }
                            if elided {
                                // W3.2: the `v:` copy was elided — this is the
                                // latest version and its value is the current
                                // `data:` row (no later mutation materialized it,
                                // else the flag would be clear). Read `data:`.
                                return current("elided version data");
                            }
                            // Non-elided: fetch the actual versioned `v:` data.
                            return Ok(self
                                .get_version_by_exact_timestamp(table_name, row_id, actual_ts)?
                                .map(|image| (ImageSource::Version(actual_ts), image)));
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
        let any_iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(any_prefix.as_bytes(), rocksdb::Direction::Forward),
            bounded_read_options(any_prefix.as_bytes()),
        );
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
            return current("data key fallback");
        }

        // Versions exist but none at or before our snapshot — row was created
        // after our snapshot started. Not visible to this transaction.
        Ok(None)
    }

    /// Spec R5.5: `table`'s layout record (`storage::table_layout`), from memory after
    /// the first call; `None` when the table has none, which is every table no `DROP
    /// COLUMN`, rename or storage move has changed. Loaded under
    /// [`Self::table_layouts_epoch`]'s rule: a writer writes the record durably, then
    /// [`Self::invalidate_table_layout`]; a load that read the old record either lands
    /// before that and is removed, or after it and is refused.
    pub(crate) fn table_layout(&self, table_name: &str) -> Result<Option<Arc<super::table_layout::TableLayout>>> {
        if let Some(entry) = self.table_layouts.get(table_name) {
            return Ok(entry.value().clone());
        }
        let epoch = self.table_layouts_epoch.load(Ordering::Acquire);
        let key = super::table_layout::TableLayout::key(table_name);
        let raw = self
            .db
            .get(&key)
            .map_err(|e| Error::storage(format!("Failed to read the layout record of '{}': {}", table_name, e)))?;
        let loaded = match tde::open_opt(self.key_manager.as_deref(), &key, raw)? {
            Some(plain) => Some(Arc::new(super::table_layout::TableLayout::decode(table_name, &plain)?)),
            None => None,
        };
        if let dashmap::mapref::entry::Entry::Vacant(slot) = self.table_layouts.entry(table_name.to_string()) {
            if self.table_layouts_epoch.load(Ordering::Acquire) == epoch {
                slot.insert(loaded.clone());
            }
        }
        Ok(loaded)
    }

    /// Spec R5.5: forget `table`'s loaded layout record. After the durable write.
    pub(crate) fn invalidate_table_layout(&self, table_name: &str) {
        self.table_layouts_epoch.fetch_add(1, Ordering::AcqRel);
        self.table_layouts.remove(table_name);
    }

    /// Spec R5.5: forget every table's loaded layout record.
    pub(crate) fn clear_table_layouts(&self) {
        self.table_layouts_epoch.fetch_add(1, Ordering::AcqRel);
        self.table_layouts.clear();
    }

    /// Spec R5.5: the time whose layout a version of row `row_id` written at
    /// `written` has. A version's own timestamp, except the pre-image a rewrite or a
    /// first change writes for a row with no version at all
    /// ([`REWRITE_PRE_IMAGE_TS`]): its content is the row as that change found it,
    /// so its layout is the one just before the row's next event.
    pub(crate) fn version_layout_time(&self, table_name: &str, row_id: u64, written: u64) -> Result<u64> {
        if written != REWRITE_PRE_IMAGE_TS {
            return Ok(written);
        }
        let prefix = format!("v_idx:{}:{}:", table_name, row_id);
        let oldest = version_index_key(table_name, row_id, REWRITE_PRE_IMAGE_TS);
        // Newest first: the event just before the pre-image's key is the next one.
        let events = self.db.iterator_opt(
            rocksdb::IteratorMode::From(oldest.as_bytes(), rocksdb::Direction::Reverse),
            bounded_read_options(prefix.as_bytes()),
        );
        for item in events {
            let (key, value) = item.map_err(|e| Error::storage(format!("Version index read failed: {}", e)))?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            if *key == *oldest.as_bytes() {
                continue;
            }
            return Ok(decode_version_index_value(&value)
                .map_or(REWRITE_PRE_IMAGE_TS, |(event_ts, _)| event_ts.saturating_sub(1)));
        }
        Ok(REWRITE_PRE_IMAGE_TS)
    }

    /// Spec R5.5 with phase B4's rule: the cut that left row `row_id` with nothing to
    /// read at `snapshot_ts`, if one did. A rename (or a refresh's swap) moved the row
    /// here without its history; it existed at the snapshot (its id is at or under what
    /// the cut moved), has no event at or before it and one since: what it was then is
    /// lost, and leaving it out would show the table without it. `AS OF`, a
    /// transaction's reads and a branch's anchor all ask this, BEFORE their read and
    /// whatever it finds: the snapshot cache can still hold the image such a row read
    /// while it had no version at all (a transaction's commit does not move the cache's
    /// generation), and whether a read is refused must not depend on what is cached.
    pub(crate) fn history_lost_at(
        &self,
        layout: &super::table_layout::TableLayout,
        table_name: &str,
        row_id: u64,
        snapshot_ts: u64,
    ) -> Result<Option<super::table_layout::HistoryCut>> {
        let Some(cause) = layout.lost_history(row_id, snapshot_ts) else {
            return Ok(None);
        };
        let (at_or_before, changed_since) = self.row_events_around(table_name, row_id, snapshot_ts)?;
        Ok((at_or_before.is_none() && changed_since).then_some(cause))
    }

    /// [`Self::history_lost_at`] as `AS OF`'s and a transaction's refusal (0A000).
    pub(crate) fn refuse_lost_history(
        &self,
        layout: &super::table_layout::TableLayout,
        table_name: &str,
        row_id: u64,
        snapshot_ts: u64,
    ) -> Result<()> {
        match self.history_lost_at(layout, table_name, row_id, snapshot_ts)? {
            Some(cause) => Err(Error::history_before_layout_change(
                table_name,
                &super::table_layout::describe_cut(cause, table_name),
            )),
            None => Ok(()),
        }
    }

    /// Spec R5.5: take `tuple`, the image a read of `table` found at `source`, into
    /// today's layout: through every `DROP COLUMN` after the version it is, refused
    /// at a cut (`storage::table_layout`). A current image is in today's layout.
    pub(crate) fn bring_version_forward(
        &self,
        table_name: &str,
        row_id: u64,
        source: ImageSource,
        tuple: &mut crate::Tuple,
    ) -> Result<()> {
        if let ImageSource::Version(written) = source {
            if let Some(layout) = self.table_layout(table_name)? {
                if layout.changed_since(written) {
                    let time = self.version_layout_time(table_name, row_id, written)?;
                    layout.bring_forward(table_name, time, tuple)?;
                }
            }
        }
        self.normalize_snapshot_tuple(table_name, row_id, source, tuple)?;
        Ok(())
    }

    /// Spec R5.5: [`Self::read_at_snapshot`] for a reader that decodes the image under
    /// today's schema (`Transaction::get`): a version older than a `DROP COLUMN` is
    /// re-encoded in today's layout, `layout` being the table's record
    /// ([`Self::table_layout`]), read by the caller. A decode and an encode only for
    /// such a version.
    pub(crate) fn read_at_snapshot_in_layout(
        &self,
        table_name: &str,
        row_id: u64,
        snapshot_ts: u64,
        layout: Option<&super::table_layout::TableLayout>,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(layout) = layout {
            self.refuse_lost_history(layout, table_name, row_id, snapshot_ts)?;
        }
        let Some((source, image)) = self.read_at_snapshot_sourced(table_name, row_id, snapshot_ts)? else {
            return Ok(None);
        };
        let changed_layout = match (source, layout) {
            (ImageSource::Version(written), Some(layout)) => layout.changed_since(written),
            _ => false,
        };
        let side_storage = self
            .current_row_history_codec
            .as_ref()
            .map(|codec| codec.table_uses_side_storage(table_name))
            .transpose()?
            .unwrap_or(false);
        if !changed_layout && !side_storage {
            return Ok(Some(image));
        }
        let mut tuple: crate::Tuple = bincode::deserialize(&image)
            .map_err(|e| Error::storage(format!("Failed to deserialize a row version: {}", e)))?;
        if let (ImageSource::Version(written), Some(layout)) = (source, layout) {
            if changed_layout {
                let time = self.version_layout_time(table_name, row_id, written)?;
                layout.bring_forward(table_name, time, &mut tuple)?;
            }
        }
        let normalized = self.normalize_snapshot_tuple(table_name, row_id, source, &mut tuple)?;
        if !changed_layout && !normalized {
            return Ok(Some(image));
        }
        bincode::serialize(&tuple)
            .map(Some)
            .map_err(|e| Error::storage(format!("Failed to serialize a row version: {}", e)))
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
        self.gate
            .put(&self.db, key.as_bytes(), stored.as_ref(), None)
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
        let mut batch = WriteBatch::default();
        let staged = self.stage_data_version_snapshot(
            &mut batch,
            data_key,
            data_value,
            table_name,
            row_id,
            timestamp,
            version_value,
            lsn,
            allow_elide,
        )?;
        if let Some(opts) = write_options {
            self.gate
                .write(&self.db, batch, Some(opts))
                .map_err(|e| Error::storage(format!("Failed to write data/version snapshot batch: {}", e)))?;
        } else {
            self.write_batch(batch, "Failed to write data/version snapshot batch")?;
        }
        self.finish_staged_version(staged)
    }

    /// Spec R5.4 (F1.2, F1.3): what [`Self::write_data_version_and_register_snapshot`]
    /// writes, a new row's `data:` image, its version and the `snapshot:` record,
    /// staged into the caller's `batch`, so that the caller can add to the same write
    /// group what belongs to the row (its counter, its logical-WAL entry, its MV delta)
    /// and write it once. Finish with [`Self::finish_staged_version`] after the write.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn stage_data_version_snapshot(
        &self,
        batch: &mut WriteBatch,
        data_key: &[u8],
        data_value: &[u8],
        table_name: &str,
        row_id: u64,
        timestamp: u64,
        version_value: &[u8],
        lsn: Option<u64>,
        allow_elide: bool,
    ) -> Result<StagedVersion> {
        let (metadata, txn_id, scn) = self.allocate_snapshot_metadata(timestamp, lsn);
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
            batch,
            table_name,
            row_id,
            timestamp,
            version_value,
            sealed_version,
            &metadata,
            allow_elide && data_value == version_value,
        )?;
        Ok(StagedVersion {
            table_name: table_name.to_string(),
            row_id,
            timestamp,
            txn_id,
            scn,
            metadata,
        })
    }

    /// Spec R5.4: the batch [`Self::stage_data_version_snapshot`] staged into was
    /// written: the snapshot exists in memory too, and what readers cached from
    /// before the row no longer holds.
    pub(crate) fn finish_staged_version(&self, staged: StagedVersion) -> Result<SnapshotMetadata> {
        self.finish_version_snapshot(
            &staged.table_name,
            staged.row_id,
            staged.timestamp,
            staged.txn_id,
            staged.scn,
            staged.metadata,
        )
    }

    /// Sprinter 6bc4a4040eb7 (B4): a version-skipping fast DELETE, with its delete
    /// event and the snapshot an `AS OF` of a later time resolves to, in one batch.
    /// Without that snapshot, `AS OF` a time after the delete would resolve to the
    /// one before it and show the row again.
    // Spec R5.4: the fast DELETE stages its event in its own group now
    // (`StorageEngine::write_fast_row_change`); the unit tests below still drive this.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn delete_data_and_record_event(
        &self,
        data_key: &[u8],
        table_name: &str,
        row_id: u64,
        timestamp: u64,
        lsn: Option<u64>,
        write_options: Option<&WriteOptions>,
    ) -> Result<SnapshotMetadata> {
        let (metadata, txn_id, scn) = self.allocate_snapshot_metadata(timestamp, lsn);
        let mut batch = WriteBatch::default();
        batch.delete(data_key);
        batch.put(
            version_index_key(table_name, row_id, timestamp).as_bytes(),
            encode_delete_event(timestamp),
        );
        self.append_snapshot_metadata_to_batch(&mut batch, &metadata)?;
        match write_options {
            Some(options) => self
                .gate
                .write(&self.db, batch, Some(options))
                .map_err(|e| Error::storage(format!("Failed to write delete event batch: {}", e)))?,
            None => self.write_batch(batch, "Failed to write delete event batch")?,
        }
        self.finish_version_snapshot(table_name, row_id, timestamp, txn_id, scn, metadata)
    }

    /// Sprinter 6bc4a4040eb7 (B4 fix pass): row `row_id` of `table` around `ts`: its
    /// newest event at or before it (the event's time, and whether it is a delete), and
    /// whether it has a later one.
    pub(crate) fn row_events_around(
        &self,
        table_name: &str,
        row_id: u64,
        ts: u64,
    ) -> Result<(Option<(u64, bool)>, bool)> {
        let prefix = format!("v_idx:{}:{}:", table_name, row_id);
        let seek = version_index_key(table_name, row_id, ts);
        let first_from = |from: &[u8]| -> Result<Option<(Box<[u8]>, Box<[u8]>)>> {
            let read = bounded_read_options(prefix.as_bytes());
            let mut iter = self
                .db
                .iterator_opt(rocksdb::IteratorMode::From(from, rocksdb::Direction::Forward), read);
            match iter.next() {
                Some(item) => {
                    let (key, value) = item.map_err(|e| Error::storage(format!("Version index read failed: {}", e)))?;
                    Ok(key.starts_with(prefix.as_bytes()).then_some((key, value)))
                }
                None => Ok(None),
            }
        };
        let at_or_before = first_from(seek.as_bytes())?.and_then(|(_, value)| {
            decode_version_index_value(&value).map(|(event_ts, _)| (event_ts, is_delete_event(&value)))
        });
        // Newest first: a key before the seek position is a later event.
        let later = first_from(prefix.as_bytes())?.is_some_and(|(key, _)| &*key < seek.as_bytes());
        Ok((at_or_before, later))
    }

    /// Sprinter 6bc4a4040eb7 (B4): the rows of `table` whose newest event is a
    /// delete: what `AS OF` and a branch's anchor add to the current rows. A row
    /// deleted before delete events existed has none and stays out, which is what
    /// it always was: absent at every time.
    pub(crate) fn rows_with_delete_event(&self, table_name: &str) -> Result<Vec<u64>> {
        self.rows_deleted_after(table_name, 0)
    }

    /// The rows of `table` whose newest event is a delete at a timestamp after
    /// `after`: the rows a snapshot at `after` (a transaction's, or `AS OF`'s)
    /// adds to the current ones, because they existed then (review F7). A row
    /// deleted at or before it reads as absent there anyway, so it is left out.
    ///
    /// One step per row with history, not per event: past a row's newest event,
    /// a row with more of them is left with one seek.
    pub(crate) fn rows_deleted_after(&self, table_name: &str, after: u64) -> Result<Vec<u64>> {
        let prefix = format!("v_idx:{}:", table_name);
        let mut iter = self.db.raw_iterator_opt(bounded_read_options(prefix.as_bytes()));
        iter.seek(prefix.as_bytes());
        let mut rows = Vec::new();
        while iter.valid() {
            let (Some(key), Some(value)) = (iter.key(), iter.value()) else {
                break;
            };
            let Some(rest) = key.strip_prefix(prefix.as_bytes()) else {
                break;
            };
            // `{row_id}:{reverse_ts}`. A row's events are contiguous, newest first.
            // Anything else is a table whose name extends this one past a ':'.
            let Some(colon) = rest.iter().position(|byte| *byte == b':') else {
                iter.next();
                continue;
            };
            let deleted_after = is_delete_event(value)
                && decode_version_index_value(value).is_some_and(|(event_ts, _)| event_ts > after);
            if deleted_after {
                if let Some(row_id) = rest
                    .get(..colon)
                    .and_then(|row| std::str::from_utf8(row).ok())
                    .and_then(|text| text.parse().ok())
                {
                    rows.push(row_id);
                }
            }
            // The row's older events: step once, and seek past the rest of them
            // (the first key after `{prefix}{row}:`, its ':' raised to ';').
            let row_prefix = key.get(..prefix.len() + colon + 1).unwrap_or_default().to_vec();
            iter.next();
            if iter.key().is_some_and(|next| next.starts_with(&row_prefix)) {
                let mut past_row = row_prefix;
                if let Some(last) = past_row.last_mut() {
                    *last = b';';
                }
                iter.seek(&past_row);
            }
        }
        iter.status()
            .map_err(|e| Error::storage(format!("Version index scan failed: {e}")))?;
        Ok(rows)
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
        let mut iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
            bounded_read_options(prefix.as_bytes()),
        );
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
            let old_value = self.current_row_history_image(table_name, row_id, &old_value)?;
            // Capture logical values while the old current sidecars are intact.
            // Inline ciphertext is reused; a physical row is decoded and sealed
            // exactly once by current_row_history_image.
            let mut batch = WriteBatch::default();
            let version_key = format!("v:{}:{}:{}", table_name, row_id, marker_ts);
            batch.put(version_key.as_bytes(), old_value.as_ref());
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
        let mut iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
            bounded_read_options(prefix.as_bytes()),
        );
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
            let old_value = self.current_row_history_image(table_name, row_id, &old_value)?;
            let mut batch = WriteBatch::default();
            let version_key = format!("v:{}:{}:{}", table_name, row_id, event_ts);
            batch.put(version_key.as_bytes(), old_value.as_ref());
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
            let old_value = self.current_row_history_image(table_name, row_id, &old_value)?;
            let version_key = format!("v:{}:{}:{}", table_name, row_id, event_ts);
            batch.put(version_key.as_bytes(), old_value.as_ref());
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
            let old_value = self.current_row_history_image(table_name, row_id, &old_value)?;
            let version_key = format!("v:{}:{}:{}", table_name, row_id, marker_ts);
            batch.put(version_key.as_bytes(), old_value.as_ref());
            let reverse_ts = u64::MAX - marker_ts;
            let index_key = format!("v_idx:{}:{}:{:020}", table_name, row_id, reverse_ts);
            batch.put(index_key.as_bytes(), marker_ts.to_be_bytes());
        }
        Ok(())
    }

    /// Spec R5.2: stage the version of a row a schema statement rewrites in
    /// place, next to the `data:` image in `batch`, at the statement's one
    /// `timestamp`. `sealed` is that image as sealed for `data:`.
    ///
    /// Every snapshot-resolved read goes through the versions, so without this it
    /// keeps resolving the row's previous version and never sees the rewrite. What
    /// older snapshots read stays as it was: the version-eliding paths' history is
    /// materialized first, and a row with no version at all, which reads `data:` at
    /// every snapshot, gets its previous image as a version older than any snapshot
    /// (once a row has a version, a snapshot older than all of them does not see it).
    ///
    /// The `data:` reads return the image before the rewrite: `batch` has not been
    /// applied.
    pub(crate) fn stage_rewritten_row_version(
        &self,
        batch: &mut WriteBatch,
        table_name: &str,
        row_id: u64,
        timestamp: u64,
        sealed: &[u8],
    ) -> Result<()> {
        self.stage_row_pre_history(batch, table_name, row_id, timestamp)?;
        let version_key = format!("v:{}:{}:{}", table_name, row_id, timestamp);
        batch.put(version_key.as_bytes(), sealed);
        let index_key = format!("v_idx:{}:{}:{:020}", table_name, row_id, u64::MAX - timestamp);
        batch.put(index_key.as_bytes(), encode_version_index_value(timestamp, false));
        Ok(())
    }

    /// A rewrite's data row may contain references whose payload is mutable or
    /// purged by the rewrite. Its history instead carries logical row values. Both
    /// images are sealed before any side storage changes: `before` in the old layout,
    /// `after` in the new layout. COPY/elided/no-history pre-images use `before` too.
    pub(crate) fn stage_logical_rewritten_row_version(
        &self,
        batch: &mut WriteBatch,
        table_name: &str,
        row_id: u64,
        timestamp: u64,
        after: &[u8],
        before: &[u8],
    ) -> Result<()> {
        self.stage_row_pre_history_with_image(batch, table_name, row_id, timestamp, Some(before))?;
        batch.put(format!("v:{table_name}:{row_id}:{timestamp}").as_bytes(), after);
        batch.put(
            version_index_key(table_name, row_id, timestamp).as_bytes(),
            encode_version_index_value(timestamp, false),
        );
        Ok(())
    }

    /// Spec R5.4 (F1.3): stage, beside a fast UPDATE's or DELETE's `data:` change in
    /// `batch`, the row's event at `timestamp`: its new version (`Some(image)`, the
    /// version value as stored, i.e. sealed) or its delete event (`None`). Without the
    /// version a later transaction's scan and `AS OF` read the row as it was before
    /// the UPDATE. What older snapshots read stays as it was
    /// ([`Self::stage_rewritten_row_version`]).
    pub(crate) fn stage_row_change_history(
        &self,
        batch: &mut WriteBatch,
        table_name: &str,
        row_id: u64,
        timestamp: u64,
        image: Option<&[u8]>,
    ) -> Result<()> {
        match image {
            Some(sealed) => self.stage_rewritten_row_version(batch, table_name, row_id, timestamp, sealed),
            None => {
                self.stage_row_pre_history(batch, table_name, row_id, timestamp)?;
                batch.put(
                    version_index_key(table_name, row_id, timestamp).as_bytes(),
                    encode_delete_event(timestamp),
                );
                Ok(())
            }
        }
    }

    /// What a row's history needs in `batch` before its current image changes, so that
    /// a snapshot older than the change reads what it read before: a COPY-marker
    /// row's insert version, an elided latest version made real, and, for a row with
    /// no version at all (which reads `data:` at every snapshot), its current image as
    /// a version older than any snapshot. One seek of the row's version index when no
    /// COPY marker covers it.
    ///
    /// The `data:` reads return the image before the change: `batch` is not applied.
    // Explicit transactions use the same pre-image staging as autocommit writes:
    // otherwise the first transactional change to a non-versioned row loses what
    // snapshots from before the transaction must read.
    pub(crate) fn stage_row_pre_history(
        &self,
        batch: &mut WriteBatch,
        table_name: &str,
        row_id: u64,
        timestamp: u64,
    ) -> Result<()> {
        self.stage_row_pre_history_with_image(batch, table_name, row_id, timestamp, None)
    }

    fn stage_row_pre_history_with_image(
        &self,
        batch: &mut WriteBatch,
        table_name: &str,
        row_id: u64,
        timestamp: u64,
        before: Option<&[u8]>,
    ) -> Result<()> {
        if let Some(marker_ts) = self.copy_markers.covering_ts(table_name, row_id) {
            match before {
                Some(image) => {
                    if marker_ts < timestamp && !self.has_any_version_index(table_name, row_id)? {
                        self.stage_pre_image(batch, table_name, row_id, marker_ts, Some(image))?;
                    }
                    if self.maybe_elided_rows() {
                        if let Some((event_ts, _)) = self.find_flagged_latest_version(table_name, row_id)? {
                            self.stage_pre_image(batch, table_name, row_id, event_ts, Some(image))?;
                        }
                    }
                }
                None => {
                    self.materialize_copy_marker_row(batch, table_name, row_id, timestamp)?;
                    if self.maybe_elided_rows() {
                        self.materialize_elided_latest_version(batch, table_name, row_id)?;
                    }
                }
            }
            return Ok(());
        }
        let prefix = format!("v_idx:{}:{}:", table_name, row_id);
        let newest = {
            let mut iter = self.db.iterator_opt(
                rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
                bounded_read_options(prefix.as_bytes()),
            );
            match iter.next() {
                Some(item) => {
                    let (key, value) = item.map_err(|e| Error::storage(format!("version index seek failed: {}", e)))?;
                    if key.starts_with(prefix.as_bytes()) {
                        Some((key.to_vec(), decode_version_index_value(&value)))
                    } else {
                        None
                    }
                }
                None => None,
            }
        };
        match newest {
            None => self.stage_pre_image(batch, table_name, row_id, REWRITE_PRE_IMAGE_TS, before)?,
            // W3.2: the newest event is elided; its value is `data:` until now.
            Some((_, Some((event_ts, true)))) => {
                self.stage_pre_image(batch, table_name, row_id, event_ts, before)?;
            }
            Some(_) => {}
        }
        Ok(())
    }

    /// Materialize one prior event from the supplied logical image, or the
    /// pre-write data row for callers whose data row is already logical. Never
    /// double-seal the image: the supplied bytes and data: are both stored bytes.
    fn stage_pre_image(
        &self,
        batch: &mut WriteBatch,
        table_name: &str,
        row_id: u64,
        event_ts: u64,
        before: Option<&[u8]>,
    ) -> Result<()> {
        let image = match before {
            Some(image) => Some(Cow::Borrowed(image)),
            None => self
                .db
                .get(format!("data:{table_name}:{row_id}").as_bytes())
                .map_err(|e| Error::storage(format!("row history data read failed: {e}")))?
                .map(Cow::Owned),
        };
        if let Some(image) = image {
            let logical = if before.is_some() {
                Cow::Borrowed(image.as_ref())
            } else {
                self.current_row_history_image(table_name, row_id, image.as_ref())?
            };
            batch.put(
                format!("v:{table_name}:{row_id}:{event_ts}").as_bytes(),
                logical.as_ref(),
            );
            batch.put(
                version_index_key(table_name, row_id, event_ts).as_bytes(),
                encode_version_index_value(event_ts, false),
            );
        }
        Ok(())
    }

    /// Spec R5.4 (F1.5): stage the `snapshot:` record of `timestamp` into the batch of
    /// the write that commits at it, so the record is durable exactly when the write is
    /// (it used to be a write group of its own after every commit). `lsn` is the
    /// logical-WAL position `AS OF TRANSACTION` names it by, when there is one.
    /// Publish it with [`Self::publish_staged_snapshot`] once the batch is written.
    pub(crate) fn stage_snapshot_record(
        &self,
        batch: &mut WriteBatch,
        timestamp: u64,
        lsn: Option<u64>,
    ) -> Result<SnapshotMetadata> {
        let (metadata, _, _) = self.allocate_snapshot_metadata(timestamp, lsn);
        self.append_snapshot_metadata_to_batch(batch, &metadata)?;
        Ok(metadata)
    }

    /// Spec R5.4 (F1.5): the batch carrying a staged `snapshot:` record was written:
    /// `AS OF` resolves to it from now on, as after [`Self::register_snapshot`].
    pub(crate) fn publish_staged_snapshot(&self, metadata: SnapshotMetadata) {
        self.publish_snapshot_metadata(metadata);
        if self.gc_config.auto_gc_enabled {
            if let Err(e) = self.gc_if_needed() {
                tracing::warn!("snapshot GC failed: {}", e);
            }
        }
    }

    /// Stage the `snapshot:` record of a rewrite's one timestamp in its group, so
    /// `AS OF` resolves to it once it is durable. Publish it with
    /// [`Self::publish_rewrite_snapshot`] after the group is written.
    pub(crate) fn stage_rewrite_snapshot(&self, batch: &mut WriteBatch, timestamp: u64) -> Result<SnapshotMetadata> {
        let (metadata, _, _) = self.allocate_snapshot_metadata(timestamp, None);
        if self.persist_metadata {
            Self::put_snapshot_record(batch, &metadata)?;
        }
        Ok(metadata)
    }

    /// A rewrite's group is durable: its snapshot exists, and what a reader cached
    /// from before it no longer holds.
    pub(crate) fn publish_rewrite_snapshot(&self, table_name: &str, metadata: SnapshotMetadata) {
        self.publish_snapshot_metadata(metadata);
        self.invalidate_cached_versions(table_name);
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
        let logical = self.logical_current_row_for_history(table_name, row_id, value, None)?;
        let elide = allow_elide && self.elide_latest_version() && matches!(logical, Cow::Borrowed(_));
        // An already sealed copy is reusable only when normalization preserved
        // its plaintext. Physical row references always get a full logical v:.
        let sealed_value = if matches!(logical, Cow::Borrowed(_)) {
            sealed_value
        } else {
            None
        };
        let value = logical.as_ref();

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

        self.append_snapshot_metadata_to_batch(batch, metadata)
    }

    /// The `snapshot:` record of `metadata`, into `batch`.
    fn append_snapshot_metadata_to_batch(&self, batch: &mut WriteBatch, metadata: &SnapshotMetadata) -> Result<()> {
        if self.persist_metadata {
            // R1.4: the per-id `txn_map:` / `scn_map:` maps were write-only
            // (recovery rebuilds the in-memory maps from `snapshot:` alone) and
            // are not written. The one index key that is (review F16) is what a
            // lookup past the in-memory map seeks instead of walking every record.
            Self::put_snapshot_record(batch, metadata)?;
        }

        Ok(())
    }

    /// A snapshot's `snapshot:` record and its index key ([`SNAPSHOT_INDEX_PREFIX`]),
    /// into `batch`: every writer of a record goes through here, so no record is
    /// without its key.
    fn put_snapshot_record(batch: &mut WriteBatch, metadata: &SnapshotMetadata) -> Result<()> {
        let value =
            bincode::serialize(metadata).map_err(|e| Error::storage(format!("Failed to serialize metadata: {e}")))?;
        batch.put(format!("snapshot:{}", metadata.timestamp).as_bytes(), value);
        batch.put(
            snapshot_index_key_of(metadata).as_bytes(),
            snapshot_index_value(metadata),
        );
        put_snapshot_id_indexes(batch, metadata);
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
        self.map_transaction_timestamp(txn_id, timestamp);
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

        self.gate
            .put(&self.db, index_key.as_bytes(), timestamp_bytes, None)
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
        let mut batch = WriteBatch::default();
        Self::put_snapshot_record(&mut batch, metadata)?;
        self.gate
            .write(&self.db, batch, None)
            .map_err(|e| Error::storage(format!("Failed to persist metadata: {}", e)))
    }

    /// Garbage collect old snapshots
    ///
    /// A snapshot lives as long as the history it resolves to can be read:
    /// the configured `storage.version_retention`, which the version GC
    /// enforces through its low watermark. Among the snapshots marked
    /// `gc_eligible`:
    /// - one below the version-GC low watermark (every read it could resolve
    ///   is refused there, `check_gc_horizon`) and older than
    ///   `min_retention_seconds` (a floor) is removed, its `snapshot:` record
    ///   with it. With infinite retention (the default) the watermark never
    ///   moves and no record is removed: they grow by one per snapshot, about
    ///   60 bytes, until `version_retention` is set;
    /// - past `max_snapshots`, the oldest leave the in-memory map only: their
    ///   records stay in the store, and `AS OF` finds them there
    ///   (`EvictedSnapshots`). This used to delete the records too, so on a
    ///   busy store `AS OF` and `CREATE BRANCH … AS OF` reached only the last
    ///   ~1000 snapshots, an anchor seconds old included; and the age rule
    ///   used to delete them after a fixed hour whatever the retention;
    /// - a record of one the map let go of is deleted once the watermark has
    ///   passed it, by a sweep bounded per call (`sweep_expired_records`).
    ///   The version GC calls this after each cycle, so the sweep follows the
    ///   watermark on a quiet store too.
    ///
    /// Returns how many left the in-memory map.
    pub fn gc_old_snapshots(&self) -> Result<usize> {
        let now_secs = Utc::now().timestamp();
        let min_retention = i64::try_from(self.gc_config.min_retention_seconds).unwrap_or(i64::MAX);
        let watermark = self.gc_low_watermark();
        // W2.2(b): wall-clock age from epoch-micros metadata (seconds granularity).
        let expired = |metadata: &SnapshotMetadata| {
            metadata.gc_eligible
                && metadata.timestamp < watermark
                && now_secs.saturating_sub(metadata.wall_clock_unix_secs()) > min_retention
        };

        let mut snapshots = self.snapshots.write();
        let to_remove: Vec<u64> = snapshots
            .values()
            .filter(|&metadata| expired(metadata))
            .map(|metadata| metadata.timestamp)
            .collect();

        // If we're still over the limit, the oldest eligible leave memory only.
        let to_evict = self.oldest_beyond_limit(&snapshots, &to_remove);

        // Remove snapshots
        let count = to_remove.len() + to_evict.len();
        let mut delete_batch = WriteBatch::default();
        let mut removed = Vec::with_capacity(to_remove.len());
        for ts in &to_remove {
            if let Some(metadata) = snapshots.remove(ts) {
                if self.persist_metadata {
                    let snap_key = format!("snapshot:{}", ts);
                    // The legacy id-map records of databases written by older
                    // versions: nothing reads them (recovery rebuilds the maps
                    // from `snapshot:` alone), so they go with the snapshot.
                    let txn_key = format!("txn_map:{}", metadata.transaction_id);
                    let scn_key = format!("scn_map:{}", metadata.scn);

                    delete_batch.delete(snap_key.as_bytes());
                    delete_batch.delete(txn_key.as_bytes());
                    delete_batch.delete(scn_key.as_bytes());
                    delete_snapshot_indexes(&mut delete_batch, &metadata);
                }

                // Note: We don't delete the versioned data (v:*) here
                // That would require a separate GC pass to avoid breaking
                // any in-flight queries
                removed.push(metadata);
            }
        }
        // Remove from the id mappings only what names a removed snapshot
        // (spec P1.1b). This removed every removed snapshot's transaction-id
        // mapping unconditionally, but an id can name several snapshots
        // (`register_snapshot_with_lsn` takes the WAL LSN, and commits with
        // nothing logged between them share one): GC dropped the id of a
        // surviving snapshot, `AS OF TRANSACTION` stopped resolving it, and a
        // restart — which maps every id a surviving record carries — disagreed
        // with the running process.
        self.unmap_forgotten_snapshots(&snapshots, &removed);

        self.evict_from_memory(&mut snapshots, &to_evict)?;

        // A manager that writes its own records (not a physical image's, which
        // change only through replication) deletes those of evicted snapshots
        // once the watermark has passed them; before it moves, none can be.
        let swept = if (self.persist_metadata || self.non_durable_writes)
            && watermark > 0
            && self.evicted.any.load(Ordering::Acquire)
        {
            let budget = self.gc_config.max_snapshots.clamp(1, 1000).saturating_mul(2);
            self.sweep_expired_records(&snapshots, &mut delete_batch, expired, budget)?
        } else {
            false
        };

        // Every record this manager writes has its index key, so the index covers
        // every record up to the newest registered (`SNAPSHOT_INDEX_MARKER`): the
        // next open re-indexes only what was registered after this.
        if self.persist_metadata {
            let through = self.newest_snapshot_ts.load(Ordering::Acquire).to_be_bytes();
            delete_batch.put(SNAPSHOT_INDEX_MARKER, through);
            // GC runs only after open completed its ID backfill, or on a new store
            // whose registrations always stage all the keys together.
            if self.id_index_trusted() {
                delete_batch.put(SNAPSHOT_ID_INDEX_MARKER, through);
            }
        }
        if self.persist_metadata || swept {
            self.write_batch(delete_batch, "Failed to delete old snapshots")?;
        }

        Ok(count)
    }

    /// The oldest gc-eligible snapshots past `max_snapshots`, not counting the
    /// ones in `leaving` (on their way out already).
    fn oldest_beyond_limit(&self, snapshots: &HashMap<u64, SnapshotMetadata>, leaving: &[u64]) -> Vec<u64> {
        let excess = snapshots
            .len()
            .saturating_sub(leaving.len())
            .saturating_sub(self.gc_config.max_snapshots);
        if excess == 0 {
            return Vec::new();
        }
        let leaving: HashSet<u64> = leaving.iter().copied().collect();
        let mut eligible: Vec<u64> = snapshots
            .values()
            .filter(|metadata| metadata.gc_eligible && !leaving.contains(&metadata.timestamp))
            .map(|metadata| metadata.timestamp)
            .collect();
        eligible.sort_unstable();
        eligible.truncate(excess);
        eligible
    }

    /// Let the snapshots at `evict` go from the in-memory maps while their
    /// `snapshot:` records stay in the store, where the lookups the maps can no
    /// longer answer find them ([`EvictedSnapshots`]). A memory-only database
    /// writes no record as it registers a snapshot, so it writes these now.
    /// The watermarks move before any entry goes: a lookup that misses an
    /// entry must see them.
    fn evict_from_memory(&self, snapshots: &mut HashMap<u64, SnapshotMetadata>, evict: &[u64]) -> Result<()> {
        if evict.is_empty() {
            return Ok(());
        }
        if self.non_durable_writes && !self.persist_metadata {
            let mut batch = WriteBatch::default();
            for metadata in evict.iter().filter_map(|ts| snapshots.get(ts)) {
                Self::put_snapshot_record(&mut batch, metadata)?;
            }
            self.write_batch(batch, "Failed to keep the records of evicted snapshots")?;
        }
        for metadata in evict.iter().filter_map(|ts| snapshots.get(ts)) {
            self.note_evicted(metadata);
        }
        let evicted: Vec<SnapshotMetadata> = evict.iter().filter_map(|ts| snapshots.remove(ts)).collect();
        // The id maps follow the rule removal follows: an id that named an
        // evicted snapshot moves to its newest one still in memory. A lookup
        // checks that against the watermarks and walks the records when an
        // evicted snapshot may be newer (`resolve_transaction`).
        self.unmap_forgotten_snapshots(snapshots, &evicted);
        Ok(())
    }

    /// Move the eviction watermarks past `metadata`, a snapshot whose record
    /// the store keeps while the in-memory map does not hold it.
    fn note_evicted(&self, metadata: &SnapshotMetadata) {
        self.evicted.through_ts.fetch_max(metadata.timestamp, Ordering::AcqRel);
        self.evicted
            .max_wall_secs
            .fetch_max(metadata.wall_clock_unix_secs(), Ordering::AcqRel);
        self.evicted.any.store(true, Ordering::Release);
    }

    /// Whether the in-memory map has let go of snapshots whose records the
    /// store may still hold: then [`Self::list_snapshots`] (`\snapshots`,
    /// `pg_snapshots`) lists only the newest, and `AS OF` still reaches the
    /// others inside `storage.version_retention`.
    pub fn has_snapshots_beyond_memory(&self) -> bool {
        self.evicted.any.load(Ordering::Acquire)
    }

    /// Delete the store's `snapshot:` records that are `expired` (see
    /// [`Self::gc_old_snapshots`]) and not in the in-memory map: the records of
    /// snapshots the map let go of by count, which the pass over the map never
    /// sees. At most
    /// `budget` records a call, resuming where the last call stopped and
    /// starting over past the end of the family, so the cost follows the
    /// registrations that trigger GC and every record is revisited within
    /// about `records / budget` calls. Returns whether it staged a delete.
    fn sweep_expired_records(
        &self,
        in_memory: &HashMap<u64, SnapshotMetadata>,
        batch: &mut WriteBatch,
        expired: impl Fn(&SnapshotMetadata) -> bool,
        budget: usize,
    ) -> Result<bool> {
        let mut cursor = self.evicted.sweep_cursor.lock();
        let start = cursor.take().unwrap_or_else(|| SNAPSHOT_RECORD_PREFIX.to_vec());
        let iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(start.as_slice(), rocksdb::Direction::Forward),
            bounded_read_options(SNAPSHOT_RECORD_PREFIX),
        );
        let mut staged = false;
        for (visited, item) in iter.enumerate() {
            let (key, value) =
                item.map_err(|e| Error::storage(format!("Iterator error sweeping snapshot records: {e}")))?;
            if !key.starts_with(SNAPSHOT_RECORD_PREFIX) {
                break;
            }
            if visited == budget {
                *cursor = Some(key.to_vec());
                break;
            }
            let Some(metadata) = deserialize_snapshot_metadata(&value) else {
                continue;
            };
            if in_memory.contains_key(&metadata.timestamp) || !expired(&metadata) {
                continue;
            }
            batch.delete(&key);
            // The legacy id-map records go with it, as in the age pass, and its
            // index key.
            batch.delete(format!("txn_map:{}", metadata.transaction_id).as_bytes());
            batch.delete(format!("scn_map:{}", metadata.scn).as_bytes());
            delete_snapshot_indexes(batch, &metadata);
            staged = true;
        }
        Ok(staged)
    }

    /// The in-memory map size past which GC runs: `max_snapshots` plus a slack
    /// of as many again, at most 1000, so GC runs once per slack registrations.
    fn gc_trigger(&self) -> usize {
        let slack = self.gc_config.max_snapshots.clamp(1, 1000);
        self.gc_config.max_snapshots.saturating_add(slack)
    }

    /// Run GC if needed
    fn gc_if_needed(&self) -> Result<()> {
        let snapshot_count = self.snapshots.read().len();
        if snapshot_count > self.gc_trigger() {
            self.gc_old_snapshots()?;
        }
        Ok(())
    }

    /// Bring the in-memory map back to `max_snapshots` once it has outgrown
    /// the GC trigger, by count alone, and deleting nothing: as a physical
    /// standby ingests the primary's records, which change only through
    /// replication (the primary's GC deletes them).
    fn trim_memory_if_needed(&self) -> Result<()> {
        if !self.gc_config.auto_gc_enabled || self.snapshots.read().len() <= self.gc_trigger() {
            return Ok(());
        }
        let mut snapshots = self.snapshots.write();
        let evict = self.oldest_beyond_limit(&snapshots, &[]);
        self.evict_from_memory(&mut snapshots, &evict)
    }

    /// Get snapshot metadata
    pub fn get_snapshot_metadata(&self, timestamp: u64) -> Option<SnapshotMetadata> {
        self.snapshots.read().get(&timestamp).cloned()
    }

    /// Has a snapshot newer than `timestamp` been registered?
    pub fn has_snapshot_after(&self, timestamp: u64) -> bool {
        self.newest_snapshot_ts.load(Ordering::Acquire) > timestamp
    }

    /// Get current SCN
    pub fn current_scn(&self) -> Scn {
        self.current_scn.load(Ordering::Relaxed)
    }

    /// Get current transaction ID
    pub fn current_transaction_id(&self) -> TransactionId {
        self.current_txn_id.load(Ordering::Relaxed)
    }

    /// How many snapshots the in-memory map holds: all of them until it lets
    /// some go by count (`has_snapshots_beyond_memory`), the newest after.
    pub fn snapshot_count(&self) -> usize {
        self.snapshots.read().len()
    }

    /// List the snapshots the in-memory map holds, oldest first: every one
    /// until the map lets some go by count, then only the newest
    /// `max_snapshots` to twice that — what `\snapshots`, `pg_snapshots`,
    /// `pg_transaction_map` and `pg_scn_map` show, and say so
    /// ([`Self::has_snapshots_beyond_memory`]). `AS OF` reaches the older
    /// ones through their records.
    pub fn list_snapshots(&self) -> Result<Vec<SnapshotMetadata>> {
        let snapshots = self.snapshots.read();
        let mut result: Vec<_> = snapshots.values().cloned().collect();
        result.sort_by_key(|s| s.timestamp);
        Ok(result)
    }

    /// Load existing snapshots from disk (for recovery)
    pub fn recover_snapshots(&self) -> Result<usize> {
        self.recover_snapshots_with_policy(false)
    }

    pub(crate) fn recover_snapshots_strict(&self) -> Result<usize> {
        self.recover_snapshots_with_policy(true)
    }

    fn recover_snapshots_with_policy(&self, strict: bool) -> Result<usize> {
        let mut count = 0;
        // The map holds the newest `max_snapshots`; every older record stays
        // in the store, where a lookup the map cannot answer finds it. They
        // are collected here in a set of that bound, never all at once: with
        // infinite `storage.version_retention` the store keeps one record per
        // snapshot ever taken, millions on a busy store.
        let keep = if self.gc_config.auto_gc_enabled {
            self.gc_config.max_snapshots
        } else {
            usize::MAX
        };
        let mut newest: BTreeMap<u64, SnapshotMetadata> = BTreeMap::new();
        // Review F16: a record above the index marker gets its index key here — a
        // store an older binary wrote, and what was registered after the last GC
        // moved the marker. Blind puts: an existing key is rewritten unchanged.
        // A physical image cannot write: it trusts the primary's index when the
        // primary keeps one (its marker is there), and walks the records if not.
        let marker = self.index_marker()?;
        let id_marker = self.read_index_marker(SNAPSHOT_ID_INDEX_MARKER)?;
        let mut reindex = self.persist_metadata.then(WriteBatch::default);
        let mut reindexed = 0usize;
        let mut newest_ts = marker.unwrap_or(0);
        // A read-only empty image must remember that its source supplied no
        // completeness marker. It can later receive legacy snapshot records
        // without index keys through replication; finding no records at open
        // is not evidence that those future records will be indexed.
        let mut unindexed = !self.persist_metadata && marker.is_none();
        let mut ids_unindexed = !self.persist_metadata && id_marker.is_none();
        // A scan across every key prefix must be a total-order iterator. The
        // engine's store has a 5-byte prefix extractor, and a prefix-mode
        // iterator is defined only within one prefix: when it steps onto a key
        // a range tombstone covers, RocksDB re-seeks the older files to the
        // tombstone's end, and the prefix filter of a file holding no key with
        // that prefix invalidates the file's iterator. Every later key of that
        // file — `snapshot:` and `vmeta:` records among them — was then
        // silently missing from recovery.
        let mut read = ReadOptions::default();
        read.set_total_order_seek(true);
        let iter = self.db.iterator_opt(rocksdb::IteratorMode::Start, read);

        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error during recovery: {}", e)))?;

            // Item #2: rebuild the in-memory COPY marker index from the durable
            // `vmeta:` records in the same scan (crash-safe resume — a marker
            // outlives the process, so AS-OF visibility survives restart).
            if key.starts_with(crate::storage::copy_marker::VMETA_PREFIX.as_bytes()) {
                if strict {
                    self.copy_markers.load_record_strict(&key, &value)?;
                } else {
                    self.copy_markers.load_record(&key, &value);
                }
                continue;
            }

            if strict && key.starts_with(b"snapshot:") && std::str::from_utf8(&key).is_err() {
                return Err(Error::storage("malformed snapshot metadata key"));
            }
            if let Ok(key_str) = std::str::from_utf8(&key) {
                if key_str.starts_with("snapshot:") {
                    if let Some(metadata) = Self::decode_snapshot_record(key_str, &value, strict)? {
                        self.advance_counters(&metadata);
                        newest_ts = newest_ts.max(metadata.timestamp);
                        let needs_wall = marker.is_none_or(|indexed| metadata.timestamp > indexed);
                        let needs_ids = id_marker.is_none_or(|indexed| metadata.timestamp > indexed);
                        if needs_wall || needs_ids {
                            match reindex.as_mut() {
                                Some(batch) => {
                                    if needs_wall {
                                        batch.put(
                                            snapshot_index_key_of(&metadata).as_bytes(),
                                            snapshot_index_value(&metadata),
                                        );
                                    }
                                    if needs_ids {
                                        put_snapshot_id_indexes(batch, &metadata);
                                    }
                                    reindexed += 1;
                                }
                                None => {
                                    // Entries above a valid marker were created by
                                    // a writer which stages its index keys atomically.
                                    unindexed |= marker.is_none();
                                    ids_unindexed |= id_marker.is_none();
                                }
                            }
                            if reindexed >= 4096 {
                                reindexed = 0;
                                if let Some(batch) = reindex.as_mut() {
                                    let full = std::mem::take(batch);
                                    if self.write_batch(full, "Failed to index snapshot records").is_err() {
                                        reindex = None;
                                        unindexed = true;
                                        ids_unindexed = true;
                                    }
                                }
                            }
                        }
                        newest.insert(metadata.timestamp, metadata);
                        if newest.len() > keep {
                            if let Some((_, older)) = newest.pop_first() {
                                self.note_evicted(&older);
                            }
                        }
                        count += 1;
                    }
                }
            }
        }

        if let Some(mut batch) = reindex {
            batch.put(SNAPSHOT_INDEX_MARKER, newest_ts.to_be_bytes());
            batch.put(SNAPSHOT_ID_INDEX_MARKER, newest_ts.to_be_bytes());
            if self.write_batch(batch, "Failed to index snapshot records").is_err() {
                unindexed = true;
                ids_unindexed = true;
            }
        }
        if unindexed {
            self.evicted.unindexed.store(true, Ordering::Release);
        }
        if ids_unindexed {
            self.evicted.ids_unindexed.store(true, Ordering::Release);
        }

        // Oldest first, so an id several snapshots carry maps to its newest.
        for metadata in newest.into_values() {
            self.publish_snapshot_metadata(metadata);
        }
        Ok(count)
    }

    /// The index marker (`SNAPSHOT_INDEX_MARKER`), if the store has one.
    fn index_marker(&self) -> Result<Option<u64>> {
        self.read_index_marker(SNAPSHOT_INDEX_MARKER)
    }

    fn read_index_marker(&self, key: &[u8]) -> Result<Option<u64>> {
        let raw = self
            .db
            .get(key)
            .map_err(|e| Error::storage(format!("snapshot index marker read failed: {e}")))?;
        Ok(raw
            .as_deref()
            .and_then(|bytes| bytes.get(..8))
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_be_bytes))
    }

    /// Decode one `snapshot:{ts}` record. The lenient open-time scan skips an
    /// undecodable value (`Ok(None)`); `strict` — a physical image, whose
    /// records are the primary's and must not be silently dropped — refuses
    /// it, and also refuses an exhausted counter and a key that does not name
    /// the record's own timestamp.
    ///
    /// The per-record decode of [`Self::recover_snapshots_with_policy`],
    /// factored out so a physical standby ingests a replicated record by the
    /// same rule ([`Self::ingest_snapshot_record`]).
    fn decode_snapshot_record(key: &str, value: &[u8], strict: bool) -> Result<Option<SnapshotMetadata>> {
        // W2.2(b): tolerate both epoch-micros and legacy RFC3339 layouts.
        let decoded = deserialize_snapshot_metadata(value);
        if strict && decoded.is_none() {
            return Err(Error::storage("malformed snapshot metadata"));
        }
        let Some(metadata) = decoded else {
            return Ok(None);
        };
        if strict {
            if metadata.scn == u64::MAX || metadata.transaction_id == u64::MAX || metadata.timestamp == u64::MAX {
                return Err(Error::storage("snapshot metadata sequence exhausted"));
            }
            if key != format!("snapshot:{}", metadata.timestamp) {
                return Err(Error::storage("snapshot metadata identity mismatch"));
            }
        }
        Ok(Some(metadata))
    }

    /// Publish one recovered or replicated snapshot into the in-memory maps
    /// and advance the SCN / transaction-id counters past it. The publish half
    /// of [`Self::recover_snapshots_with_policy`].
    fn publish_snapshot_metadata(&self, metadata: SnapshotMetadata) {
        // Restore in-memory state
        self.snapshots.write().insert(metadata.timestamp, metadata.clone());
        self.map_transaction_timestamp(metadata.transaction_id, metadata.timestamp);
        self.scn_to_timestamp.write().insert(metadata.scn, metadata.timestamp);
        self.advance_counters(&metadata);
    }

    /// Completion order need not equal timestamp order. Reused IDs always name
    /// their newest snapshot, including before anything has left the memory maps.
    fn map_transaction_timestamp(&self, id: TransactionId, timestamp: u64) {
        self.txn_to_timestamp
            .write()
            .entry(id)
            .and_modify(|mapped| *mapped = (*mapped).max(timestamp))
            .or_insert(timestamp);
    }

    /// Advance the SCN / transaction-id counters and the newest timestamp past
    /// a recovered or replicated snapshot, held in memory or not.
    fn advance_counters(&self, metadata: &SnapshotMetadata) {
        // W2.2(d): fetch_max is the atomic
        // equivalent of the former `if id >= *counter { *counter = id + 1 }`.
        self.current_scn.fetch_max(metadata.scn + 1, Ordering::Relaxed);
        self.newest_snapshot_ts.fetch_max(metadata.timestamp, Ordering::AcqRel);
        self.current_txn_id
            .fetch_max(metadata.transaction_id + 1, Ordering::Relaxed);
    }

    /// A physical standby ingests one replicated `snapshot:` record as its
    /// open-time strict recovery would have (`recover_snapshots_strict`), so
    /// `AS OF` resolution and the SCN / transaction-id counters follow the
    /// primary without a reopen. Returns the snapshot's timestamp, which the
    /// caller raises the engine's MVCC timestamp to: a reader that takes its
    /// snapshot after this must see the commit the record describes.
    #[cfg_attr(not(feature = "ha-tier1"), allow(dead_code))]
    pub(crate) fn ingest_snapshot_record(&self, key: &[u8], value: &[u8]) -> Result<u64> {
        let key = std::str::from_utf8(key).map_err(|_| Error::storage("malformed snapshot metadata key"))?;
        let metadata = Self::decode_snapshot_record(key, value, true)?
            .ok_or_else(|| Error::storage("malformed snapshot metadata"))?;
        let timestamp = metadata.timestamp;
        self.publish_snapshot_metadata(metadata);
        // The primary keeps every record inside retention; the map keeps the
        // newest, as the primary's does.
        self.trim_memory_if_needed()?;
        Ok(timestamp)
    }

    /// Forget the snapshots at `timestamps` after their `snapshot:` records
    /// were deleted on the primary (snapshot GC): the in-memory half of
    /// [`Self::gc_old_snapshots`], without the write, leaving the maps as a
    /// fresh recovery of the surviving records builds them. Returns how many
    /// were known. The id maps follow [`Self::unmap_forgotten_snapshots`], the
    /// rule GC applies too.
    #[cfg_attr(not(feature = "ha-tier1"), allow(dead_code))]
    pub(crate) fn forget_snapshots(&self, timestamps: &[u64]) -> usize {
        let mut snapshots = self.snapshots.write();
        let forgotten: Vec<SnapshotMetadata> = timestamps
            .iter()
            .filter_map(|timestamp| snapshots.remove(timestamp))
            .collect();
        self.unmap_forgotten_snapshots(&snapshots, &forgotten);
        forgotten.len()
    }

    /// The id-map half of forgetting snapshots, shared by GC
    /// ([`Self::gc_old_snapshots`]) and a physical standby's
    /// [`Self::forget_snapshots`] (factored out of the latter, spec P1.1b).
    /// `survivors` is the snapshot map with `forgotten` already removed.
    ///
    /// A transaction id can name several snapshots (`register_snapshot_with_lsn`
    /// takes the WAL LSN, and commits with nothing logged between them share
    /// one), and recovery maps every id a surviving record still carries. So
    /// a mapping is removed only when it names a forgotten snapshot, and an id
    /// left unmapped that way moves to its newest surviving snapshot, the one
    /// the latest registration under it would have mapped. SCNs are never
    /// shared, but are held to the same rule.
    /// The counters stay where they are: they only move forward, and GC runs
    /// as a snapshot is registered and keeps that one, whose ids are the
    /// highest and are what recovery derives the counters from. The lock order
    /// is GC's: the snapshot map (held by the caller), then one id map at a
    /// time.
    fn unmap_forgotten_snapshots(&self, survivors: &HashMap<u64, SnapshotMetadata>, forgotten: &[SnapshotMetadata]) {
        let mut unmapped = HashSet::new();
        {
            let mut by_txn = self.txn_to_timestamp.write();
            for metadata in forgotten {
                if by_txn.get(&metadata.transaction_id) == Some(&metadata.timestamp) {
                    by_txn.remove(&metadata.transaction_id);
                    unmapped.insert(metadata.transaction_id);
                }
            }
        }
        {
            let mut by_scn = self.scn_to_timestamp.write();
            for metadata in forgotten {
                if by_scn.get(&metadata.scn) == Some(&metadata.timestamp) {
                    by_scn.remove(&metadata.scn);
                }
            }
        }
        if !unmapped.is_empty() {
            let mut by_txn = self.txn_to_timestamp.write();
            for metadata in survivors.values() {
                if unmapped.contains(&metadata.transaction_id) {
                    let mapped = by_txn.entry(metadata.transaction_id).or_insert(metadata.timestamp);
                    *mapped = (*mapped).max(metadata.timestamp);
                }
            }
        }
    }

    /// Load one replicated `vmeta:` COPY marker into the in-memory interval
    /// index, as the open-time scan does (`CopyMarkers::load_record`: a
    /// malformed record is skipped, exactly as the primary skips it on reopen).
    #[cfg_attr(not(feature = "ha-tier1"), allow(dead_code))]
    pub(crate) fn ingest_copy_marker(&self, key: &[u8], value: &[u8]) {
        self.copy_markers.load_record(key, value);
    }

    /// Forget the COPY marker a deleted `vmeta:` record described
    /// (`CopyMarkers::remove_record`).
    #[cfg_attr(not(feature = "ha-tier1"), allow(dead_code))]
    pub(crate) fn forget_copy_marker(&self, key: &[u8]) -> bool {
        self.copy_markers.remove_record(key)
    }

    /// Invalidate every cached `read_at_snapshot` answer for `table_name`.
    ///
    /// The primary bumps a table's cache generation on its own version writes
    /// (`invalidate_cache_for_row`). A physical standby's versions arrive
    /// through the applier instead, stamped with the PRIMARY's commit
    /// timestamps, which may be at or below a snapshot timestamp a standby
    /// reader already cached an answer for — so the applier bumps the
    /// generation for every table a batch's row or version keys touched.
    #[cfg_attr(not(feature = "ha-tier1"), allow(dead_code))]
    pub(crate) fn invalidate_cached_versions(&self, table_name: &str) {
        self.invalidate_cache_for_row(table_name, 0);
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

        // Iterate through all version keys. `v:` is shorter than the store's
        // 5-byte prefix extractor, so this scan crosses one extractor prefix
        // per table and row: it must be total-order (spec P1.1b), or a range
        // tombstone on the way drops older files from it.
        let iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
            bounded_read_options(prefix.as_bytes()),
        );

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

        // Iterate through all version keys for this table. For a one-letter
        // table name the prefix (`v:t:`) is shorter than the store's 5-byte
        // extractor, and the scan crosses one extractor prefix per row: it
        // must be total-order (spec P1.1b), as `calculate_snapshot_size` is.
        let iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
            bounded_read_options(prefix.as_bytes()),
        );

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
        let manager = SnapshotManager::new(db, WriteGate::open());

        let metadata = manager.register_snapshot(100).unwrap();
        assert_eq!(metadata.timestamp, 100);
        assert_eq!(metadata.transaction_id, 1);
        assert_eq!(metadata.scn, 1);
    }

    #[test]
    fn test_resolve_transaction() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db, WriteGate::open());

        let metadata = manager.register_snapshot(100).unwrap();
        let txn_id = metadata.transaction_id;

        let resolved = manager.resolve_transaction(txn_id).unwrap();
        assert_eq!(resolved, 100);
    }

    #[test]
    fn test_resolve_scn() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db, WriteGate::open());

        let metadata = manager.register_snapshot(100).unwrap();
        let scn = metadata.scn;

        let resolved = manager.resolve_scn(scn).unwrap();
        assert_eq!(resolved, 100);
    }

    #[test]
    fn test_non_durable_snapshot_manager_keeps_metadata_in_memory_only() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new_non_durable(Arc::clone(&db), WriteGate::open());

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
        let manager = SnapshotManager::new(db, WriteGate::open());

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
        let manager = SnapshotManager::with_gc_config(db, WriteGate::open(), gc_config);

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
            let manager = SnapshotManager::new(db, WriteGate::open());

            manager.register_snapshot(100).unwrap();
            manager.register_snapshot(200).unwrap();
        }

        // Reopen and recover
        {
            let mut opts = rocksdb::Options::default();
            opts.create_if_missing(true);
            let db = Arc::new(DB::open(&opts, db_path).unwrap());
            let manager = SnapshotManager::new(db, WriteGate::open());

            let count = manager.recover_snapshots().unwrap();
            assert_eq!(count, 2);
            assert_eq!(manager.snapshot_count(), 2);
        }
    }

    /// A store opened the way the engine opens its own in the two respects
    /// that decide how a full scan behaves (a 5-byte prefix extractor over a
    /// full-filter block table, `StorageEngine` open), with compaction off so
    /// each flush stays a file of its own.
    fn open_prefix_extracted_db(path: &std::path::Path) -> Arc<DB> {
        let mut block = rocksdb::BlockBasedOptions::default();
        block.set_bloom_filter(14.0, false);
        block.set_whole_key_filtering(true);
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(5));
        opts.set_block_based_table_factory(&block);
        opts.set_disable_auto_compactions(true);
        Arc::new(DB::open(&opts, path).unwrap())
    }

    #[test]
    fn recovery_reads_every_record_past_a_range_tombstone() {
        let dir = tempdir().unwrap();
        let db = open_prefix_extracted_db(dir.path());
        let gate = WriteGate::open();
        let manager = SnapshotManager::new(Arc::clone(&db), Arc::clone(&gate));
        // File 1: a snapshot record, and no `data:` key.
        manager.register_snapshot(2).unwrap();
        db.flush().unwrap();
        // File 2: rows, and a later snapshot record.
        let mut rows = WriteBatch::default();
        for row in 1..=50 {
            rows.put(format!("data:t:{row}"), b"row");
        }
        gate.write(&db, rows, None).unwrap();
        manager.register_snapshot(3).unwrap();
        db.flush().unwrap();
        // File 3: a range tombstone over the rows. Stepping onto a covered row
        // re-seeks the older files to `data:t;`, and file 1 holds no `data:`
        // key: a prefix-mode scan lost the rest of file 1, `snapshot:2` with it.
        let mut range = WriteBatch::default();
        range.delete_range(b"data:t:".as_slice(), b"data:t;".as_slice());
        gate.write(&db, range, None).unwrap();
        db.flush().unwrap();

        let recovered = SnapshotManager::new(Arc::clone(&db), WriteGate::open());
        assert_eq!(recovered.recover_snapshots_strict().unwrap(), 2);
        let timestamps: Vec<u64> = recovered
            .list_snapshots()
            .unwrap()
            .iter()
            .map(|s| s.timestamp)
            .collect();
        assert_eq!(timestamps, vec![2, 3]);
        assert_eq!(recovered.resolve_transaction(1).unwrap(), 2);
        assert_eq!(recovered.current_scn(), 3);
        assert_eq!(recovered.current_transaction_id(), 3);
    }

    #[test]
    fn forgetting_snapshots_leaves_the_maps_a_recovery_of_the_survivors_builds() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db, WriteGate::open());
        // Two snapshots under one transaction id (a shared WAL LSN), one alone.
        manager.register_snapshot_with_lsn(100, 7).unwrap();
        manager.register_snapshot_with_lsn(200, 7).unwrap();
        let alone = manager.register_snapshot(300).unwrap();
        let shared_scn = manager.get_snapshot_metadata(200).unwrap().scn;
        let (scn, txn) = (manager.current_scn(), manager.current_transaction_id());

        // Forgetting the snapshot the shared id maps to moves the id to the
        // survivor; an unknown timestamp changes nothing.
        assert_eq!(manager.forget_snapshots(&[200, 999]), 1);
        assert_eq!(manager.resolve_transaction(7).unwrap(), 100);
        assert!(manager.resolve_scn(shared_scn).is_err());
        assert_eq!(manager.resolve_transaction(alone.transaction_id).unwrap(), 300);
        assert_eq!(manager.snapshot_count(), 2);

        // Forgetting the other one leaves the id mapped to nothing; the
        // counters never move back.
        assert_eq!(manager.forget_snapshots(&[100]), 1);
        assert!(manager.resolve_transaction(7).is_err());
        assert_eq!(manager.resolve_transaction(alone.transaction_id).unwrap(), 300);
        assert_eq!((manager.current_scn(), manager.current_transaction_id()), (scn, txn));
    }

    /// Spec P1.1b: snapshot GC keeps a shared transaction id mapped while a
    /// snapshot that carries it survives, and leaves exactly the maps a
    /// recovery of the surviving records builds. Red before the fix: GC removed
    /// every removed snapshot's id mapping unconditionally, so `AS OF
    /// TRANSACTION 7` failed in the running process while a restart resolved it.
    #[test]
    fn gc_keeps_a_shared_transaction_id_mapped_to_its_surviving_snapshot() {
        let gc_config = || GcConfig {
            // Nothing is old enough to go by age: only the `max_snapshots`
            // excess is collected.
            min_retention_seconds: 3600,
            max_snapshots: 1,
            auto_gc_enabled: false,
        };
        let recovered_mapping = |db: &Arc<DB>| {
            let recovered = SnapshotManager::new(Arc::clone(db), WriteGate::open());
            recovered.recover_snapshots().unwrap();
            recovered.resolve_transaction(7).ok()
        };

        // The survivor is the snapshot the id maps to: GC removes the older
        // one and must not take the mapping with it.
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::with_gc_config(Arc::clone(&db), WriteGate::open(), gc_config());
        manager.register_snapshot_with_lsn(100, 7).unwrap();
        manager.register_snapshot_with_lsn(200, 7).unwrap();
        assert_eq!(manager.gc_old_snapshots().unwrap(), 1);
        assert!(manager.get_snapshot_metadata(100).is_none());
        assert_eq!(manager.resolve_transaction(7).unwrap(), 200);
        assert_eq!(recovered_mapping(&db), Some(200));

        // The id maps to the snapshot GC removes while an older one carrying
        // it is pinned: the mapping moves to the survivor. Removed because the
        // version-GC watermark passed it (and it is past the age floor): an
        // excess by count only leaves memory now, its record kept for `AS OF`
        // (`count_eviction_keeps_every_record_findable`).
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::with_gc_config(Arc::clone(&db), WriteGate::open(), gc_config());
        manager.register_snapshot_with_lsn(100, 7).unwrap();
        manager.register_snapshot_with_lsn(200, 7).unwrap();
        manager.snapshots.write().get_mut(&100).unwrap().gc_eligible = false;
        manager.snapshots.write().get_mut(&200).unwrap().wall_clock_micros = 0;
        manager.set_gc_low_watermark(201);
        let shared_scn = manager.get_snapshot_metadata(200).unwrap().scn;
        assert_eq!(manager.gc_old_snapshots().unwrap(), 1);
        assert!(manager.get_snapshot_metadata(200).is_none());
        assert_eq!(manager.resolve_transaction(7).unwrap(), 100);
        assert!(
            manager.resolve_scn(shared_scn).is_err(),
            "the removed snapshot's SCN goes with it"
        );
        assert_eq!(recovered_mapping(&db), Some(100));
    }

    /// `count` snapshots at timestamps 100, 200, …, the i-th with SCN and
    /// transaction id `i`, registered `1000 - 10 * i` seconds ago: entries in
    /// the in-memory map, and records in the store when the manager persists
    /// them. Returns their wall-clock seconds, oldest first.
    fn seed_dated_snapshots(manager: &SnapshotManager, count: u64) -> Vec<i64> {
        let now = Utc::now().timestamp();
        (1..=count)
            .map(|i| {
                let secs = now - 1000 + i64::try_from(i).unwrap() * 10;
                let mut metadata = SnapshotMetadata::new(i * 100, i, i);
                metadata.wall_clock_micros = secs * 1_000_000;
                if manager.persist_metadata {
                    manager.persist_snapshot_metadata(&metadata).unwrap();
                }
                manager.publish_snapshot_metadata(metadata);
                secs
            })
            .collect()
    }

    fn utc(secs: i64) -> String {
        DateTime::from_timestamp(secs, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    /// Review item P2 (4.42.0): GC past `max_snapshots` deleted the records
    /// with the in-memory entries, so `AS OF` (and `CREATE BRANCH … AS OF`)
    /// reached only the newest ~1000 snapshots, an anchor seconds old
    /// included. Red before the fix: every lookup of snapshot 3 below failed.
    #[test]
    fn count_eviction_keeps_every_record_findable() {
        use crate::sql::logical_plan::AsOfClause;
        let (db, _temp) = create_test_db();
        let config = || GcConfig {
            min_retention_seconds: 3600,
            max_snapshots: 2,
            auto_gc_enabled: false,
        };
        let manager = SnapshotManager::with_gc_config(Arc::clone(&db), WriteGate::open(), config());
        let walls = seed_dated_snapshots(&manager, 10);
        let wall = |i: usize| *walls.get(i - 1).unwrap();
        assert_eq!(manager.gc_old_snapshots().unwrap(), 8);
        assert_eq!(manager.snapshot_count(), 2);
        assert!(
            db.get(b"snapshot:300").unwrap().is_some(),
            "an evicted snapshot's record stays"
        );

        let check = |manager: &SnapshotManager, when: &str| {
            // At snapshot 3's second, and between it and snapshot 4's.
            assert_eq!(manager.resolve_timestamp(&utc(wall(3))).unwrap(), 300, "{when}");
            assert_eq!(manager.resolve_timestamp(&utc(wall(3) + 5)).unwrap(), 300, "{when}");
            assert_eq!(manager.resolve_transaction(3).unwrap(), 300, "{when}");
            assert_eq!(manager.resolve_scn(3).unwrap(), 300, "{when}");
            let at = |secs: i64| AsOfClause::Timestamp(utc(secs));
            assert_eq!(
                manager.resolve_timestamp_for_range(&at(wall(3) - 5), true).unwrap(),
                300,
                "{when}"
            );
            assert_eq!(
                manager.resolve_timestamp_for_range(&at(wall(3) + 5), false).unwrap(),
                300,
                "{when}"
            );
            // The newest, from the map.
            assert_eq!(manager.resolve_timestamp(&utc(wall(10))).unwrap(), 1000, "{when}");
            // Before every snapshot: the error it always was, naming the first.
            let err = manager.resolve_timestamp(&utc(wall(1) - 100)).unwrap_err().to_string();
            assert!(err.contains("No snapshot at or before"), "{when}: {err}");
            assert!(err.contains("earliest snapshot"), "{when}: {err}");
        };
        check(&manager, "running");

        // A restart loads every record, and the map keeps the newest.
        let recovered = SnapshotManager::with_gc_config(
            Arc::clone(&db),
            WriteGate::open(),
            GcConfig {
                auto_gc_enabled: true,
                ..config()
            },
        );
        assert_eq!(recovered.recover_snapshots().unwrap(), 10);
        assert_eq!(recovered.snapshot_count(), 2);
        check(&recovered, "recovered");
    }

    /// A record lives as long as the history it resolves to: while the
    /// version-GC watermark has not passed it, no age deletes it (infinite
    /// `storage.version_retention`, the default, keeps them all; the fixed
    /// hour that used to apply is gone); once it has, the bounded sweep (four
    /// records a call here) deletes the evicted snapshots' records below it,
    /// and the rest stay.
    #[test]
    fn records_of_evicted_snapshots_are_swept_once_the_watermark_passes_them() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::with_gc_config(
            Arc::clone(&db),
            WriteGate::open(),
            GcConfig {
                // Every snapshot here is past the age floor.
                min_retention_seconds: 0,
                max_snapshots: 2,
                auto_gc_enabled: false,
            },
        );
        let walls = seed_dated_snapshots(&manager, 10);
        let wall = |i: usize| *walls.get(i - 1).unwrap();
        let stored = || -> Vec<u64> {
            (1..=10_u64)
                .filter(|i| db.get(format!("snapshot:{}", i * 100)).unwrap().is_some())
                .collect()
        };
        // Ten records at four a call: a pass is three calls, and a call may
        // start mid-pass, so six calls make sure.
        assert_eq!(manager.gc_old_snapshots().unwrap(), 8);
        for _ in 0..6 {
            assert_eq!(manager.gc_old_snapshots().unwrap(), 0);
        }
        assert_eq!(
            stored(),
            (1..=10).collect::<Vec<u64>>(),
            "no watermark: every record stays"
        );
        assert_eq!(manager.resolve_timestamp(&utc(wall(2))).unwrap(), 200);

        // The version GC moved its watermark to snapshot 5: 1-4 resolve only
        // to reads it refuses.
        manager.set_gc_low_watermark(500);
        for _ in 0..6 {
            assert_eq!(manager.gc_old_snapshots().unwrap(), 0);
        }
        assert_eq!(stored(), (5..=10).collect::<Vec<u64>>());
        for i in 1..=10_usize {
            let key = snapshot_index_key(u64::try_from(wall(i)).unwrap(), u64::try_from(i).unwrap() * 100);
            assert_eq!(db.get(key.as_bytes()).unwrap().is_some(), i >= 5, "index key of {i}");
        }

        let err = manager.resolve_timestamp(&utc(wall(2))).unwrap_err().to_string();
        assert!(err.contains("No snapshot at or before"), "{err}");
        assert!(err.contains("storage.version_retention"), "{err}");
        assert_eq!(manager.resolve_timestamp(&utc(wall(5))).unwrap(), 500);
    }

    /// Open-time recovery holds no more than `max_snapshots` in memory however
    /// many records the store keeps, and still advances every counter past
    /// all of them.
    #[test]
    fn recovery_loads_only_the_newest_records() {
        let (db, _temp) = create_test_db();
        let writer = SnapshotManager::new(Arc::clone(&db), WriteGate::open());
        seed_dated_snapshots(&writer, 10);
        let recovered = SnapshotManager::with_gc_config(
            Arc::clone(&db),
            WriteGate::open(),
            GcConfig {
                min_retention_seconds: 3600,
                max_snapshots: 3,
                auto_gc_enabled: true,
            },
        );
        assert_eq!(recovered.recover_snapshots().unwrap(), 10);
        let held: Vec<u64> = recovered
            .list_snapshots()
            .unwrap()
            .iter()
            .map(|s| s.timestamp)
            .collect();
        assert_eq!(held, vec![800, 900, 1000]);
        assert!(recovered.has_snapshots_beyond_memory());
        assert_eq!(recovered.current_scn(), 11);
        assert_eq!(recovered.current_transaction_id(), 11);
        assert_eq!(recovered.max_snapshot_timestamp(), Some(1000));
        assert_eq!(recovered.resolve_transaction(4).unwrap(), 400);
    }

    /// Review F16: a lookup past the in-memory map reads the snapshot index, a
    /// bounded number of entries whatever the history: one entry for a time
    /// (`AS OF TIMESTAMP`, a `VERSIONS BETWEEN` bound) or an exact ID-ordered
    /// seek for an SCN or a transaction id. Red before the fix: every one of
    /// these walked all 5000 records.
    #[test]
    fn lookups_past_the_map_read_a_bounded_number_of_entries() {
        use crate::sql::logical_plan::AsOfClause;
        const RECORDS: u64 = 5000;
        const SEARCH_BOUND: u64 = 2;
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::with_gc_config(
            Arc::clone(&db),
            WriteGate::open(),
            GcConfig {
                min_retention_seconds: 3600,
                max_snapshots: 10,
                auto_gc_enabled: false,
            },
        );
        let start = Utc::now().timestamp() - 2 * i64::try_from(RECORDS).unwrap();
        let wall = |i: u64| start + i64::try_from(i).unwrap();
        for i in 1..=RECORDS {
            let mut metadata = SnapshotMetadata::new(i * 10, 7_000 + i, i);
            metadata.wall_clock_micros = wall(i) * 1_000_000;
            manager.persist_snapshot_metadata(&metadata).unwrap();
            manager.publish_snapshot_metadata(metadata);
        }
        manager.gc_old_snapshots().unwrap();
        assert_eq!(manager.snapshot_count(), 10);

        let read = |lookup: &dyn Fn() -> u64| -> (u64, u64) {
            manager.evicted.records_read.store(0, Ordering::Relaxed);
            let answer = lookup();
            (answer, manager.evicted.records_read.load(Ordering::Relaxed))
        };
        for i in [1, 777, 2500, 4900] {
            let at = AsOfClause::Timestamp(utc(wall(i)));
            let cases: [(&str, &dyn Fn() -> u64, u64); 5] = [
                ("TIMESTAMP", &|| manager.resolve_timestamp(&utc(wall(i))).unwrap(), 1),
                ("SCN", &|| manager.resolve_scn(i).unwrap(), SEARCH_BOUND),
                (
                    "TRANSACTION",
                    &|| manager.resolve_transaction(7_000 + i).unwrap(),
                    SEARCH_BOUND,
                ),
                (
                    "range start",
                    &|| manager.resolve_timestamp_for_range(&at, true).unwrap(),
                    1,
                ),
                (
                    "range end",
                    &|| manager.resolve_timestamp_for_range(&at, false).unwrap(),
                    1,
                ),
            ];
            for (lookup, run, bound) in cases {
                let (timestamp, entries) = read(run);
                assert_eq!(timestamp, i * 10, "{lookup} of snapshot {i}");
                assert!(entries <= bound, "{lookup} of snapshot {i} read {entries} entries");
            }
        }
    }

    /// A store whose records have no index keys (an older binary's, or records
    /// registered after the last marker) is indexed at open. A physical image,
    /// which writes nothing, walks the records while its primary has never
    /// indexed.
    #[test]
    fn records_without_index_keys_are_indexed_at_open_or_walked() {
        let (db, _temp) = create_test_db();
        let writer = SnapshotManager::new(Arc::clone(&db), WriteGate::open());
        let walls = seed_dated_snapshots(&writer, 10);
        let wall = |i: usize| *walls.get(i - 1).unwrap();
        let index_key = |i: u64| {
            let metadata = writer.get_snapshot_metadata(i * 100).unwrap();
            snapshot_index_key_of(&metadata)
        };
        for i in 1..=10_u64 {
            db.delete(index_key(i).as_bytes()).unwrap();
        }
        let config = || GcConfig {
            min_retention_seconds: 3600,
            max_snapshots: 2,
            auto_gc_enabled: true,
        };

        let mut image = SnapshotManager::with_gc_config(Arc::clone(&db), WriteGate::open(), config());
        image.set_persistence(false);
        assert_eq!(image.recover_snapshots_strict().unwrap(), 10);
        assert!(!image.index_trusted());
        assert_eq!(image.resolve_timestamp(&utc(wall(3))).unwrap(), 300);
        assert_eq!(image.resolve_scn(3).unwrap(), 300);
        assert!(
            db.get(SNAPSHOT_INDEX_MARKER).unwrap().is_none(),
            "an image writes nothing"
        );

        let primary = SnapshotManager::with_gc_config(Arc::clone(&db), WriteGate::open(), config());
        assert_eq!(primary.recover_snapshots().unwrap(), 10);
        assert!(primary.index_trusted());
        assert_eq!(primary.index_marker().unwrap(), Some(1000));
        for i in 1..=10_u64 {
            assert!(db.get(index_key(i).as_bytes()).unwrap().is_some(), "index key of {i}");
        }
        primary.evicted.records_read.store(0, Ordering::Relaxed);
        assert_eq!(primary.resolve_timestamp(&utc(wall(3))).unwrap(), 300);
        assert!(primary.evicted.records_read.load(Ordering::Relaxed) <= 1);
        assert_eq!(primary.resolve_scn(3).unwrap(), 300);
    }

    /// The wall clock steps backwards while IDs and MVCC timestamps advance. Every
    /// retained ID must still resolve after it leaves memory and after recovery.
    /// The former wall-ordered binary search missed SCNs 1 and 1200 here.
    #[test]
    fn exact_snapshot_ids_survive_clock_rollback_and_memory_eviction() {
        const RECORDS: u64 = 2000;
        let (db, _temp) = create_test_db();
        let config = |auto_gc_enabled| GcConfig {
            min_retention_seconds: 3600,
            max_snapshots: 10,
            auto_gc_enabled,
        };
        let manager = SnapshotManager::with_gc_config(Arc::clone(&db), WriteGate::open(), config(false));
        let wall = Utc::now().timestamp() - 100;
        for i in 1..=RECORDS {
            let mut metadata = SnapshotMetadata::new(i * 10, 7000 + i, i);
            metadata.wall_clock_micros = (wall - if i > 1000 { 1 } else { 0 }) * 1_000_000;
            manager.persist_snapshot_metadata(&metadata).unwrap();
            manager.publish_snapshot_metadata(metadata);
        }
        manager.gc_old_snapshots().unwrap();
        assert_eq!(manager.snapshot_count(), 10);
        let check = |manager: &SnapshotManager| {
            for i in 1..=RECORDS {
                manager.evicted.records_read.store(0, Ordering::Relaxed);
                assert_eq!(manager.resolve_scn(i).unwrap(), i * 10, "SCN {i}");
                assert!(manager.evicted.records_read.load(Ordering::Relaxed) <= 2);
                manager.evicted.records_read.store(0, Ordering::Relaxed);
                assert_eq!(
                    manager.resolve_transaction(7000 + i).unwrap(),
                    i * 10,
                    "transaction {}",
                    7000 + i
                );
                assert!(manager.evicted.records_read.load(Ordering::Relaxed) <= 2);
            }
        };
        check(&manager);
        let recovered = SnapshotManager::with_gc_config(Arc::clone(&db), WriteGate::open(), config(true));
        assert_eq!(
            recovered.recover_snapshots().unwrap(),
            usize::try_from(RECORDS).unwrap()
        );
        assert_eq!(recovered.snapshot_count(), 10);
        check(&recovered);
    }

    /// The numeric wall-clock subspace ends before the ID keys. A future range
    /// start is absent with no scan of unrelated SCN/transaction index entries.
    #[test]
    fn wall_clock_lookup_misses_do_not_walk_the_id_indexes() {
        use crate::storage::keyspace::{classify, KeyClass};
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(Arc::clone(&db), WriteGate::open());
        let walls = seed_dated_snapshots(&manager, 600);
        for id in [SnapshotId::Scn(1), SnapshotId::Transaction(1)] {
            assert_eq!(classify(id.key(100).as_bytes()).class, KeyClass::SnapshotMap);
        }
        assert_eq!(classify(SNAPSHOT_ID_INDEX_MARKER).class, KeyClass::SnapshotMap);
        manager.evicted.records_read.store(0, Ordering::Relaxed);
        let beyond = u64::try_from(*walls.last().unwrap() + 100).unwrap();
        assert!(manager.index_entry_near(beyond, 0, true).unwrap().is_none());
        assert_eq!(manager.evicted.records_read.load(Ordering::Relaxed), 0);
    }

    /// One transaction/LSN can label arbitrarily many snapshots. Publication may
    /// finish out of timestamp order, and the exact index must find its newest
    /// occurrence even after every occurrence has left the memory maps.
    #[test]
    fn reused_transaction_id_names_its_newest_snapshot_without_a_search_window() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::with_gc_config(
            Arc::clone(&db),
            WriteGate::open(),
            GcConfig {
                min_retention_seconds: 3600,
                max_snapshots: 10,
                auto_gc_enabled: false,
            },
        );
        let wall = Utc::now().timestamp() - 100;
        for i in (1..=1500_u64).rev() {
            let mut metadata = SnapshotMetadata::new(i, 42, i);
            metadata.wall_clock_micros = wall * 1_000_000;
            manager.persist_snapshot_metadata(&metadata).unwrap();
            manager.publish_snapshot_metadata(metadata);
        }
        assert_eq!(
            manager.resolve_transaction(42).unwrap(),
            1500,
            "latest timestamp, not last publisher"
        );
        for i in 1501..=1520_u64 {
            let mut metadata = SnapshotMetadata::new(i, 10000 + i, i);
            metadata.wall_clock_micros = (wall + 1) * 1_000_000;
            manager.persist_snapshot_metadata(&metadata).unwrap();
            manager.publish_snapshot_metadata(metadata);
        }
        manager.gc_old_snapshots().unwrap();
        assert!(
            !manager.txn_to_timestamp.read().contains_key(&42),
            "every occurrence left memory"
        );
        manager.evicted.records_read.store(0, Ordering::Relaxed);
        assert_eq!(manager.resolve_transaction(42).unwrap(), 1500);
        assert_eq!(manager.evicted.records_read.load(Ordering::Relaxed), 2);
        let recovered = SnapshotManager::with_gc_config(
            Arc::clone(&db),
            WriteGate::open(),
            GcConfig {
                min_retention_seconds: 3600,
                max_snapshots: 10,
                auto_gc_enabled: true,
            },
        );
        recovered.recover_snapshots().unwrap();
        assert_eq!(recovered.resolve_transaction(42).unwrap(), 1500);
    }

    /// Upgrade a store with the earlier wall-clock index and its completeness
    /// marker, but no ID indexes. A physical image cannot backfill; it uses exact
    /// record fallback without touching the store until a primary adds ID keys.
    #[test]
    fn exact_id_indexes_backfill_even_when_the_wall_clock_index_was_complete() {
        let (db, _temp) = create_test_db();
        let writer = SnapshotManager::new(Arc::clone(&db), WriteGate::open());
        seed_dated_snapshots(&writer, 10);
        writer.gc_old_snapshots().unwrap();
        assert!(writer.index_marker().unwrap().is_some());
        for metadata in writer.list_snapshots().unwrap() {
            db.delete(SnapshotId::Scn(metadata.scn).key(metadata.timestamp).as_bytes())
                .unwrap();
            db.delete(
                SnapshotId::Transaction(metadata.transaction_id)
                    .key(metadata.timestamp)
                    .as_bytes(),
            )
            .unwrap();
        }
        db.delete(SNAPSHOT_ID_INDEX_MARKER).unwrap();
        let config = || GcConfig {
            min_retention_seconds: 3600,
            max_snapshots: 2,
            auto_gc_enabled: true,
        };
        let mut image = SnapshotManager::with_gc_config(Arc::clone(&db), WriteGate::open(), config());
        image.set_persistence(false);
        let sequence = db.latest_sequence_number();
        image.recover_snapshots_strict().unwrap();
        assert!(image.index_trusted(), "old wall-clock index is complete");
        assert!(!image.id_index_trusted(), "ID marker is separate");
        assert_eq!(image.resolve_scn(3).unwrap(), 300);
        assert_eq!(image.resolve_transaction(3).unwrap(), 300);
        assert_eq!(db.latest_sequence_number(), sequence, "read-only image never backfills");
        let upgraded = SnapshotManager::with_gc_config(Arc::clone(&db), WriteGate::open(), config());
        upgraded.recover_snapshots().unwrap();
        assert!(upgraded.id_index_trusted());
        assert_eq!(
            upgraded.read_index_marker(SNAPSHOT_ID_INDEX_MARKER).unwrap(),
            Some(1000)
        );
        for i in 1..=10_u64 {
            assert!(db.get(SnapshotId::Scn(i).key(i * 100).as_bytes()).unwrap().is_some());
            assert!(db
                .get(SnapshotId::Transaction(i).key(i * 100).as_bytes())
                .unwrap()
                .is_some());
        }
        upgraded.evicted.records_read.store(0, Ordering::Relaxed);
        assert_eq!(upgraded.resolve_scn(3).unwrap(), 300);
        assert_eq!(upgraded.evicted.records_read.load(Ordering::Relaxed), 2);
    }

    /// A syntactically valid index key still has to name a real snapshot
    /// carrying that ID. Corruption must be an explicit refusal, never a
    /// different history or an ordinary missing-ID result.
    #[test]
    fn exact_id_index_hits_refuse_stale_missing_and_corrupt_records() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::with_gc_config(
            Arc::clone(&db),
            WriteGate::open(),
            GcConfig {
                min_retention_seconds: 3600,
                max_snapshots: 2,
                auto_gc_enabled: false,
            },
        );
        seed_dated_snapshots(&manager, 10);
        manager.gc_old_snapshots().unwrap();
        assert!(manager.id_index_trusted());
        assert!(!manager.scn_to_timestamp.read().contains_key(&3));
        assert!(!manager.txn_to_timestamp.read().contains_key(&3));
        assert_eq!(manager.resolve_scn(3).unwrap(), 300);
        for id in [SnapshotId::Scn(3), SnapshotId::Transaction(3)] {
            // Both timestamps and the key shape are valid, but snapshot 400
            // belongs to ID 4. The newer stale key would win the reverse seek.
            let stale = id.key(400);
            db.put(stale.as_bytes(), []).unwrap();
            let error = match id {
                SnapshotId::Scn(value) => manager.resolve_scn(value).unwrap_err(),
                SnapshotId::Transaction(value) => manager.resolve_transaction(value).unwrap_err(),
            };
            assert!(
                error.to_string().contains("snapshot ID index identity mismatch"),
                "{error}"
            );
            db.delete(stale.as_bytes()).unwrap();
        }
        db.delete(b"snapshot:300").unwrap();
        for result in [manager.resolve_scn(3), manager.resolve_transaction(3)] {
            let error = result.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("snapshot ID index points to a missing snapshot record"),
                "{error}"
            );
        }
        db.put(b"snapshot:300", b"invalid metadata").unwrap();
        for result in [manager.resolve_scn(3), manager.resolve_transaction(3)] {
            let error = result.unwrap_err();
            assert!(error.to_string().contains("malformed snapshot metadata"), "{error}");
        }
        // IDs still in the hot maps require no disk lookup.
        manager.evicted.records_read.store(0, Ordering::Relaxed);
        assert_eq!(manager.resolve_scn(10).unwrap(), 1000);
        assert_eq!(manager.resolve_transaction(10).unwrap(), 1000);
        assert_eq!(manager.evicted.records_read.load(Ordering::Relaxed), 0);
    }

    /// A standby bootstrapped from an empty legacy image has no index marker
    /// and no snapshot records yet. Subsequent legacy snapshot records still
    /// need exact record fallback after the bounded memory maps evict them.
    #[test]
    fn an_empty_read_only_image_keeps_legacy_streamed_snapshots_findable() {
        let (db, _temp) = create_test_db();
        let mut image = SnapshotManager::with_gc_config(
            Arc::clone(&db),
            WriteGate::open(),
            GcConfig {
                min_retention_seconds: 3600,
                max_snapshots: 2,
                auto_gc_enabled: true,
            },
        );
        image.set_persistence(false);
        let empty_sequence = db.latest_sequence_number();
        assert_eq!(image.recover_snapshots_strict().unwrap(), 0);
        assert!(!image.index_trusted(), "an empty legacy image has no wall-index proof");
        assert!(!image.id_index_trusted(), "an empty legacy image has no ID-index proof");
        assert_eq!(db.latest_sequence_number(), empty_sequence);

        let wall = Utc::now().timestamp() - 100;
        for id in 1..=20_u64 {
            let mut metadata = SnapshotMetadata::new(id * 100, id + 1000, id);
            metadata.wall_clock_micros = (wall + i64::try_from(id).unwrap()) * 1_000_000;
            let key = format!("snapshot:{}", metadata.timestamp);
            let value = bincode::serialize(&metadata).unwrap();
            // The physical source supplies just the legacy snapshot record,
            // without either lookup index. Publication itself must not write.
            db.put(key.as_bytes(), &value).unwrap();
            let applied_sequence = db.latest_sequence_number();
            image.ingest_snapshot_record(key.as_bytes(), &value).unwrap();
            assert_eq!(db.latest_sequence_number(), applied_sequence);
        }
        assert!(
            image.has_snapshots_beyond_memory(),
            "the test must exercise record fallback"
        );
        assert!(image.snapshot_count() <= 4, "the in-memory map remains bounded");
        let sequence = db.latest_sequence_number();
        for id in 1..=20_u64 {
            assert_eq!(image.resolve_scn(id).unwrap(), id * 100);
            assert_eq!(image.resolve_transaction(id + 1000).unwrap(), id * 100);
        }
        assert_eq!(image.resolve_timestamp(&utc(wall + 3)).unwrap(), 300);
        assert_eq!(
            db.latest_sequence_number(),
            sequence,
            "fallback never backfills the image"
        );
    }

    /// Each occurrence owns its own ID index keys. Expiring one snapshot never
    /// removes another snapshot that carries the same transaction ID.
    #[test]
    fn exact_id_indexes_follow_snapshot_gc_and_survive_a_read_only_reopen() {
        let (db, _temp) = create_test_db();
        let config = || GcConfig {
            min_retention_seconds: 0,
            max_snapshots: 2,
            auto_gc_enabled: false,
        };
        let manager = SnapshotManager::with_gc_config(Arc::clone(&db), WriteGate::open(), config());
        let wall = Utc::now().timestamp() - 100;
        for i in 1..=12_u64 {
            let mut metadata = SnapshotMetadata::new(i * 10, if i <= 3 { 42 } else { i }, i);
            metadata.wall_clock_micros = wall * 1_000_000;
            manager.persist_snapshot_metadata(&metadata).unwrap();
            manager.publish_snapshot_metadata(metadata);
        }
        manager.gc_old_snapshots().unwrap();
        manager.set_gc_low_watermark(25);
        for _ in 0..10 {
            manager.gc_old_snapshots().unwrap();
        }
        assert_eq!(manager.resolve_transaction(42).unwrap(), 30);
        for i in 1..=12_u64 {
            assert_eq!(
                db.get(SnapshotId::Scn(i).key(i * 10).as_bytes()).unwrap().is_some(),
                i >= 3
            );
            let id = if i <= 3 { 42 } else { i };
            assert_eq!(
                db.get(SnapshotId::Transaction(id).key(i * 10).as_bytes())
                    .unwrap()
                    .is_some(),
                i >= 3
            );
        }
        let mut image = SnapshotManager::with_gc_config(Arc::clone(&db), WriteGate::open(), config());
        image.set_persistence(false);
        let sequence = db.latest_sequence_number();
        image.recover_snapshots_strict().unwrap();
        assert_eq!(image.resolve_transaction(42).unwrap(), 30);
        assert_eq!(db.latest_sequence_number(), sequence);
        manager.set_gc_low_watermark(35);
        for _ in 0..10 {
            manager.gc_old_snapshots().unwrap();
        }
        assert!(manager.resolve_transaction(42).is_err());
        assert!(manager.resolve_scn(3).is_err());
        assert_eq!(manager.resolve_transaction(12).unwrap(), 120);
    }

    /// A memory-only store writes no record as it registers a snapshot; it
    /// writes the ones its map lets go of, so `AS OF` still finds them.
    #[test]
    fn a_memory_only_store_writes_the_records_its_map_lets_go_of() {
        let (db, _temp) = create_test_db();
        let mut manager = SnapshotManager::new_non_durable(Arc::clone(&db), WriteGate::open());
        manager.gc_config = GcConfig {
            min_retention_seconds: 3600,
            max_snapshots: 2,
            auto_gc_enabled: false,
        };
        let walls = seed_dated_snapshots(&manager, 10);
        let wall = |i: usize| *walls.get(i - 1).unwrap();
        assert!(db.get(b"snapshot:300").unwrap().is_none());
        assert_eq!(manager.gc_old_snapshots().unwrap(), 8);
        assert!(db.get(b"snapshot:300").unwrap().is_some());
        assert!(
            db.get(b"snapshot:1000").unwrap().is_none(),
            "still in memory: no record"
        );
        assert_eq!(manager.resolve_timestamp(&utc(wall(3))).unwrap(), 300);
        assert_eq!(manager.resolve_transaction(3).unwrap(), 300);
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
        let manager = SnapshotManager::new(db, WriteGate::open());
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
        let manager = SnapshotManager::new(db, WriteGate::open());

        manager.write_version("t", 1, 100, b"A").unwrap();
        // Populate the cache for the open-ended snapshot.
        assert_eq!(manager.read_at_snapshot("t", 1, u64::MAX).unwrap(), Some(b"A".to_vec()));

        // A newer version bumps the table's cache generation.
        manager.write_version("t", 1, 200, b"B").unwrap();

        // The previously cached entry is now stale and must be recomputed to B.
        assert_eq!(manager.read_at_snapshot("t", 1, u64::MAX).unwrap(), Some(b"B".to_vec()));
    }

    // ----- W3.2: single-copy latest version (elide_latest_version) -----

    /// Sprinter 6bc4a4040eb7 (B4): a delete event makes the row absent from its
    /// time on, leaves earlier reads as they were, is decoded as an ordinary event
    /// by everything else, and names the rows `AS OF` adds.
    #[test]
    fn a_delete_event_ends_the_row_at_its_time() {
        let (db, _dir) = create_test_db();
        let manager = SnapshotManager::new(Arc::clone(&db), WriteGate::open());
        manager.write_version("t", 1, 10, b"ten").unwrap();
        manager.write_version("t", 2, 10, b"other").unwrap();
        manager.write_version("t", 3, 10, b"old").unwrap();
        assert!(manager.rows_with_delete_event("t").unwrap().is_empty());
        assert!(manager
            .delete_data_and_record_event(b"data:t:1", "t", 1, 20, None, None)
            .is_ok());
        // Deleted, and put back by a later event: its newest event is no delete.
        manager
            .delete_data_and_record_event(b"data:t:3", "t", 3, 20, None, None)
            .unwrap();
        manager.write_version("t", 3, 30, b"new").unwrap();

        assert_eq!(
            manager.read_at_snapshot("t", 1, 19).unwrap().as_deref(),
            Some(b"ten".as_slice())
        );
        assert_eq!(manager.read_at_snapshot("t", 1, 20).unwrap(), None);
        assert_eq!(manager.read_at_snapshot("t", 1, 99).unwrap(), None);
        assert_eq!(manager.read_at_snapshot("t", 1, 5).unwrap(), None, "not there yet");
        assert_eq!(
            manager.read_at_snapshot("t", 2, 99).unwrap().as_deref(),
            Some(b"other".as_slice())
        );
        assert_eq!(manager.read_at_snapshot("t", 3, 25).unwrap(), None);
        assert_eq!(
            manager.read_at_snapshot("t", 3, 30).unwrap().as_deref(),
            Some(b"new".as_slice())
        );
        assert_eq!(
            manager.row_events_around("t", 1, 19).unwrap(),
            (Some((10, false)), true)
        );
        assert_eq!(
            manager.row_events_around("t", 1, 20).unwrap(),
            (Some((20, true)), false)
        );
        assert_eq!(manager.row_events_around("t", 1, 5).unwrap(), (None, true));
        assert_eq!(manager.row_events_around("t", 9, 5).unwrap(), (None, false));
        // The linear reader, which only sees `v:` keys, says the same.
        assert_eq!(
            manager.read_at_snapshot_linear("t", 1, 19).unwrap().as_deref(),
            Some(b"ten".as_slice())
        );
        assert_eq!(manager.read_at_snapshot_linear("t", 1, 20).unwrap(), None);
        assert_eq!(manager.rows_with_delete_event("t").unwrap(), vec![1]);
        assert!(manager.rows_with_delete_event("t2").unwrap().is_empty());
        // Nothing else takes the event for a version: no `v:` key, and the range
        // scan of versions has no entry for it.
        let versions = manager.scan_versions_between("t", 0, 99).unwrap();
        assert!(versions.iter().all(|(_, ts, _)| *ts != 20));
        // What a build without this byte would read: the timestamp, no flag.
        let event = encode_delete_event(20);
        assert_eq!(decode_version_index_value(&event), Some((20, false)));
        assert!(is_delete_event(&event));
        assert!(!is_delete_event(&encode_version_index_value(20, false)));
    }

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
        let manager = SnapshotManager::new(Arc::clone(&db), WriteGate::open());
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
        let manager = SnapshotManager::new(Arc::clone(&db), WriteGate::open());
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
        let manager = SnapshotManager::new(Arc::clone(&db), WriteGate::open());
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
        let manager = SnapshotManager::new(Arc::clone(&db), WriteGate::open());
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
            let manager = SnapshotManager::new(Arc::clone(&db), WriteGate::open());
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
            let manager = SnapshotManager::new(Arc::clone(&db), WriteGate::open());
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
