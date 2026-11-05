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
//! # HNSW graph dump files (Sprinter 746342a0bfbb)
//!
//! - **Names.** Each checkpoint dumps a graph under a fresh basename,
//!   `{stable index id:016x}-{generation:016x}` ([`graph_dump_basename`]): never
//!   the index name, whose filesystem-safe form was lossy (`a.b` and `a_b` both
//!   became `a_b` and shared one dump). The sidecar records the basename.
//! - **Atomic switch.** The dump is written into a staging directory, fsynced
//!   and renamed into [`HNSW_SNAPSHOT_DIR`]; then the sidecar and its marker are
//!   written in ONE RocksDB batch; then the previous dump's files are removed.
//!   A crash at any point leaves either the old sidecar with its files or the
//!   new one with its files.
//! - **Unchanged indexes are not dumped again.** An index that has not changed
//!   since the dump its sidecar describes (or since the reload from it) keeps
//!   that dump; the checkpoint only re-arms its marker, which the first
//!   mutation of ANY vector index deletes.
//! - **Sweep at open.** Every dump file no valid sidecar references, and every
//!   leftover staging directory, is deleted ([`sweep_graph_dumps`]).
//! - **At-rest encryption.** hnsw_rs reads and writes plaintext files only, so
//!   an encrypted store writes NO graph dump: its HNSW indexes are rebuilt
//!   from the sealed rows at every open (open time grows with index size; each
//!   rebuild is logged with its duration), and dump files an unencrypted run
//!   left behind (and their sidecars) are deleted at the first encrypted open.
//!   Deleting unlinks them; it does not erase their bytes from free disk blocks.
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
/// v3: the v2 encoding, bumped in 4.41.1 to retire v2 snapshots. Earlier trees
///     dropped the row ids of keys that end at an inner node whenever that node
///     was resized, so a v2 snapshot can be missing entries; it is ignored and
///     the table's indexes are rebuilt from rows once.
pub const ART_KEY_ENCODING_VERSION: u32 = 3;

/// RocksDB key prefixes for snapshot blobs and their validity markers.
pub const ART_SNAPSHOT_PREFIX: &str = "idxsnap:";
pub const ART_SNAPSHOT_MARKER_PREFIX: &str = "idxsnapv:";
pub const VEC_SNAPSHOT_PREFIX: &str = "vecsnap:";
pub const VEC_SNAPSHOT_MARKER_PREFIX: &str = "vecsnapv:";

/// Subdirectory of the data dir holding `hnsw_rs` graph dumps.
pub const HNSW_SNAPSHOT_DIR: &str = "hnsw_snapshots";
/// The two files of one graph dump: `{basename}{suffix}`.
pub const HNSW_GRAPH_SUFFIX: &str = crate::vector::hnsw_index::GRAPH_FILE_SUFFIX;
pub const HNSW_DATA_SUFFIX: &str = crate::vector::hnsw_index::DATA_FILE_SUFFIX;

/// Sprinter 746342a0bfbb: the basename of one checkpoint's dump of the index
/// whose stable id is `index_id`. `generation` makes it unique per checkpoint,
/// so a new dump never overwrites the files the current sidecar references.
pub fn graph_dump_basename(index_id: u64, generation: u64) -> String {
    format!("{index_id:016x}-{generation:016x}")
}

/// Whether both files of the dump `basename` are present in `dir`.
pub fn graph_dump_files_present(dir: &std::path::Path, basename: &str) -> bool {
    [HNSW_GRAPH_SUFFIX, HNSW_DATA_SUFFIX]
        .iter()
        .all(|suffix| dir.join(format!("{basename}{suffix}")).is_file())
}

/// Remove both files of the dump `basename` from `dir`; a missing file is fine.
pub fn remove_graph_dump(dir: &std::path::Path, basename: &str) -> std::io::Result<()> {
    for suffix in [HNSW_GRAPH_SUFFIX, HNSW_DATA_SUFFIX] {
        match std::fs::remove_file(dir.join(format!("{basename}{suffix}"))) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Sprinter 746342a0bfbb: delete from `dir` every graph dump file whose
/// basename is not in `keep`, and every leftover dump staging directory.
/// Anything else in `dir` is left alone. Returns how many entries were
/// removed; a failure to remove one is warned and the sweep goes on.
pub fn sweep_graph_dumps(dir: &std::path::Path, keep: &std::collections::HashSet<&str>) -> u64 {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            tracing::warn!("HNSW snapshot sweep: cannot list {}: {}", dir.display(), e);
            return 0;
        }
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let (is_dir, is_file) = match entry.file_type() {
            Ok(file_type) => (file_type.is_dir(), file_type.is_file()),
            Err(_) => continue,
        };
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let result = if is_dir && name.starts_with(crate::vector::hnsw_index::DUMP_STAGING_PREFIX) {
            std::fs::remove_dir_all(&path)
        } else {
            let basename = name
                .strip_suffix(HNSW_GRAPH_SUFFIX)
                .or_else(|| name.strip_suffix(HNSW_DATA_SUFFIX));
            match basename {
                Some(basename) if is_file && !keep.contains(basename) => std::fs::remove_file(&path),
                _ => continue,
            }
        };
        match result {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!("HNSW snapshot sweep: cannot remove {}: {}", path.display(), e),
        }
    }
    removed
}

/// SHA-256 of one graph dump file (spec P2.2: recorded in the sidecar, so a
/// dump that is not the one the sidecar describes — another checkpoint's,
/// a truncated or missing one — is never loaded).
pub fn dump_file_digest(path: &std::path::Path) -> std::io::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(buffer.get(..count).unwrap_or_default());
    }
    Ok(hash.finalize().into())
}

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
        self.format_version == INDEX_SNAPSHOT_FORMAT_VERSION && self.key_encoding_version == ART_KEY_ENCODING_VERSION
    }

    /// Marker check for HNSW graph snapshots. The ART key encoding does not
    /// apply to them, so retiring ART snapshots must not force a graph rebuild.
    pub fn matches_current_format(&self) -> bool {
        self.format_version == INDEX_SNAPSHOT_FORMAT_VERSION
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
    /// Spec P2.2 (plan §D5): SHA-256 of `{basename}.hnsw.graph` and
    /// `{basename}.hnsw.data` as dumped. Hydration refuses a dump whose files
    /// are missing or differ and rebuilds the index from rows instead: the
    /// dump files live outside RocksDB, so nothing else ties them to this
    /// sidecar. (A sidecar written before this field fails to decode and is
    /// rebuilt from rows too.)
    pub graph_sha256: [u8; 32],
    pub data_sha256: [u8; 32],
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
    /// Sprinter 746342a0bfbb: indexes unchanged since the dump their sidecar
    /// describes — that dump was kept and only its marker re-armed.
    pub vector_unchanged: u64,
    /// Standard indexes deliberately not dumped: an empty graph (hnsw_rs cannot
    /// dump one; it rebuilds from no rows), or every index of an encrypted
    /// store (see the module docs).
    pub vector_skipped: u64,
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
    /// Sprinter 746342a0bfbb: graph dump files and staging directories in
    /// `hnsw_snapshots/` that no valid sidecar referenced, deleted at open.
    pub vector_dump_files_swept: u64,
    pub elapsed_ms: f64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The sweep removes unreferenced dump files and staging directories and
    /// nothing else.
    #[test]
    fn the_sweep_removes_only_unreferenced_dumps_and_staging_directories() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "kept.hnsw.graph",
            "kept.hnsw.data",
            "stale.hnsw.graph",
            "stale.hnsw.data",
            "idx_d_docs_emb-4242.hnsw.data",
            "notes.txt",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let staging = dir
            .path()
            .join(format!("{}kept-1-2-0", crate::vector::hnsw_index::DUMP_STAGING_PREFIX));
        std::fs::create_dir(&staging).unwrap();
        std::fs::write(staging.join("kept.hnsw.graph"), b"partial").unwrap();
        // A directory that merely looks like a dump file is not a dump.
        std::fs::create_dir(dir.path().join("odd.hnsw.graph")).unwrap();

        let keep: std::collections::HashSet<&str> = ["kept"].into_iter().collect();
        assert_eq!(sweep_graph_dumps(dir.path(), &keep), 4);
        assert_eq!(
            names(dir.path()),
            vec!["kept.hnsw.data", "kept.hnsw.graph", "notes.txt", "odd.hnsw.graph"]
        );
        assert_eq!(sweep_graph_dumps(&dir.path().join("absent"), &keep), 0);
    }

    #[test]
    fn dump_basenames_are_unique_per_index_and_checkpoint_and_removable() {
        let a = graph_dump_basename(0xABCD, 1);
        assert_eq!(a, "000000000000abcd-0000000000000001");
        assert_ne!(a, graph_dump_basename(0xABCD, 2));
        assert_ne!(a, graph_dump_basename(0xABCE, 1));

        let dir = tempfile::tempdir().unwrap();
        assert!(!graph_dump_files_present(dir.path(), &a));
        std::fs::write(dir.path().join(format!("{a}{HNSW_GRAPH_SUFFIX}")), b"g").unwrap();
        assert!(!graph_dump_files_present(dir.path(), &a), "one file is not a dump");
        std::fs::write(dir.path().join(format!("{a}{HNSW_DATA_SUFFIX}")), b"d").unwrap();
        assert!(graph_dump_files_present(dir.path(), &a));
        remove_graph_dump(dir.path(), &a).unwrap();
        assert!(names(dir.path()).is_empty());
        remove_graph_dump(dir.path(), &a).expect("removing an absent dump is fine");
    }
}
