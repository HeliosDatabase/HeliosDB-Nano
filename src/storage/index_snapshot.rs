//! Durable index snapshots (R4.2).
//!
//! ART trees and standard HNSW graphs are memory-only structures that were
//! historically rebuilt from a full table scan on every open (94.5s for the
//! 678k x 768-dim incident dataset). This module defines the on-disk snapshot
//! format that lets a reopen skip that rebuild entirely.
//!
//! # Design: checkpoint snapshots with crash-invalidated validity markers
//!
//! Two artifact kinds are persisted, each with a *validity marker* written in
//! the same RocksDB `WriteBatch`:
//!
//! - **ART entries** — `idxsnap:{table}` holds every `(encoded_key, row_ids)`
//!   pair of every ART index registered on the table ([`ArtTableSnapshot`]),
//!   `idxsnapv:{table}` is its marker.
//! - **HNSW graphs** — the graph itself is dumped through `hnsw_rs`'s
//!   `file_dump` into `{data_dir}/hnsw_snapshots/{basename}.hnsw.{graph,data}`,
//!   and `vecsnap:{index}` holds the row-id mappings + construction params
//!   ([`VectorGraphSidecar`]); `vecsnapv:{index}` is its marker.
//!
//! # Checkpoint moments (chosen + documented)
//!
//! Snapshots are written at **clean shutdown** (`EmbeddedDatabase` drop/close
//! with no open transaction) and on the explicit
//! `StorageEngine::persist_index_snapshots()` checkpoint API. They are *not*
//! written per-mutation: the DML hot path stays untouched except for one
//! relaxed atomic load.
//!
//! # Crash correctness (no WAL-tail replay needed)
//!
//! The invariant is: *a validity marker exists if and only if the snapshot
//! exactly matches the index-relevant on-disk state*. It is maintained by
//! deleting all markers (durably, via RocksDB delete) on the **first**
//! index-mutating operation after a checkpoint — the index managers carry a
//! `snapshot_clean` flag and an invalidation hook wired by the engine. Index
//! registration at open also counts as a mutation, so markers are effectively
//! consumed by the open that reads them.
//!
//! Consequently:
//! - clean shutdown → reopen: markers valid, snapshot loaded, **no scan**;
//! - crash after post-checkpoint mutations: markers already deleted before
//!   the first mutation hit the data keyspace → next open falls back to the
//!   full scan rebuild (the pre-R4.2 behaviour, always correct);
//! - crash before any mutation: snapshot still matches state, loading it is
//!   correct whether or not the marker survived.
//!
//! This trades "fast open after a crash" (rare) for zero hot-path overhead
//! and no logical-WAL coupling; WAL-tail replay into the index layer can be
//! layered on later without changing the format.
//!
//! # Versioning / migration
//!
//! [`ArtTableSnapshot::key_encoding_version`] records the ART key encoding the
//! entries were produced with ([`ART_KEY_ENCODING_VERSION`], v2 = the R4.4
//! order-preserving encoding: sign-flipped big-endian integers, total-order
//! floats, escaped composite separators). A snapshot whose versions do not
//! match the running binary is ignored and the table is rebuilt from rows —
//! pre-R4.2 data directories simply have no snapshots and migrate by
//! rebuilding once, exactly as every open did before.

use serde::{Deserialize, Serialize};

/// Snapshot container format version.
pub const INDEX_SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// ART key encoding version the snapshot entries were written with.
///
/// v1: raw `to_be_bytes` integers (not order-preserving for negatives).
/// v2: order-preserving encoding (R4.4) — sign-flipped big-endian integers,
///     total-order floats, `0x00 -> 0x00 0xFF` escaping inside composite
///     string/byte values.
pub const ART_KEY_ENCODING_VERSION: u32 = 2;

/// RocksDB key prefixes for snapshot blobs and their validity markers.
pub const ART_SNAPSHOT_PREFIX: &str = "idxsnap:";
pub const ART_SNAPSHOT_MARKER_PREFIX: &str = "idxsnapv:";
pub const VEC_SNAPSHOT_PREFIX: &str = "vecsnap:";
pub const VEC_SNAPSHOT_MARKER_PREFIX: &str = "vecsnapv:";

/// Subdirectory of the data dir holding `hnsw_rs` graph dumps.
pub const HNSW_SNAPSHOT_DIR: &str = "hnsw_snapshots";

pub fn art_snapshot_key(table: &str) -> Vec<u8> {
    format!("{ART_SNAPSHOT_PREFIX}{table}").into_bytes()
}

pub fn art_snapshot_marker_key(table: &str) -> Vec<u8> {
    format!("{ART_SNAPSHOT_MARKER_PREFIX}{table}").into_bytes()
}

pub fn vec_snapshot_key(index: &str) -> Vec<u8> {
    format!("{VEC_SNAPSHOT_PREFIX}{index}").into_bytes()
}

pub fn vec_snapshot_marker_key(index: &str) -> Vec<u8> {
    format!("{VEC_SNAPSHOT_MARKER_PREFIX}{index}").into_bytes()
}

/// Validity marker payload. Written in the same `WriteBatch` as the snapshot
/// blob; its presence (with matching versions) is what makes a snapshot
/// trustworthy at open.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMarker {
    pub format_version: u32,
    pub key_encoding_version: u32,
    /// Unix seconds at checkpoint time (diagnostics only).
    pub checkpointed_at_secs: u64,
}

impl SnapshotMarker {
    pub fn current() -> Self {
        Self {
            format_version: INDEX_SNAPSHOT_FORMAT_VERSION,
            key_encoding_version: ART_KEY_ENCODING_VERSION,
            checkpointed_at_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }

    pub fn matches_current(&self) -> bool {
        self.format_version == INDEX_SNAPSHOT_FORMAT_VERSION
            && self.key_encoding_version == ART_KEY_ENCODING_VERSION
    }
}

/// One ART index's serialized entries: `(encoded key, row ids)` pairs in
/// iteration order. Multi-value (non-unique) keys carry all row ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtIndexSnapshot {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    /// `ArtIndexType` tag: 0 = PrimaryKey, 1 = ForeignKey, 2 = Unique, 3 = Manual.
    pub index_type: u8,
    pub entries: Vec<(Vec<u8>, Vec<u64>)>,
}

/// All ART indexes of one table, snapshotted together so the open path can
/// take a single load-or-scan decision per table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtTableSnapshot {
    pub format_version: u32,
    pub key_encoding_version: u32,
    pub indexes: Vec<ArtIndexSnapshot>,
}

/// Sidecar for a dumped standard HNSW graph: everything needed to
/// reconstruct the in-memory wrapper around the reloaded `hnsw_rs` graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorGraphSidecar {
    pub format_version: u32,
    pub index_name: String,
    pub table_name: String,
    pub column_name: String,
    pub dimension: usize,
    /// "l2" | "cosine" | "ip"
    pub metric: String,
    pub m: usize,
    pub ef_construction: usize,
    /// Graph dump basename inside [`HNSW_SNAPSHOT_DIR`].
    pub basename: String,
    /// hnsw-internal id -> row id (physical, tombstones included).
    pub id_mapping: Vec<u64>,
    /// Live `(row_id, hnsw_id)` pairs (tombstones excluded).
    pub reverse_mapping: Vec<(u64, u64)>,
}

/// What `persist_index_snapshots` wrote (diagnostics / tests).
#[derive(Debug, Clone, Default)]
pub struct IndexSnapshotPersistReport {
    /// False when persistence was skipped entirely (in-memory engine).
    pub persisted: bool,
    pub art_tables: u64,
    pub art_entries: u64,
    pub vector_graphs: u64,
    /// Vector indexes that could not be dumped (left unmarked → rebuilt on open).
    pub vector_dump_failures: u64,
    pub elapsed_ms: f64,
}

/// How the last open populated the in-memory indexes (diagnostics / tests).
#[derive(Debug, Clone, Default)]
pub struct IndexOpenReport {
    pub tables_total: u64,
    /// Tables whose ART indexes were loaded from a snapshot (no row scan).
    pub tables_from_snapshot: u64,
    /// Tables rebuilt by scanning rows (no/invalid/mismatched snapshot).
    pub tables_scanned: u64,
    /// Rows replayed through `on_insert` on the scan path.
    pub rows_scanned: u64,
    /// `(key, row_id)` entries bulk-loaded from snapshots.
    pub entries_loaded: u64,
    /// HNSW indexes reloaded from a graph dump (no rebuild).
    pub vector_reloaded: u64,
    /// HNSW indexes rebuilt from a table scan.
    pub vector_rebuilt: u64,
    /// Persistent (RocksDB-backed) vector indexes reopened in place.
    pub vector_reopened_persistent: u64,
    /// Vector indexes that failed to rebuild/reload and are now degraded.
    pub vector_degraded: u64,
    pub elapsed_ms: f64,
}
