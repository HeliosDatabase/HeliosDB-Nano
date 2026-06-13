//! R4.3: Watermark-based MVCC version garbage collection.
//!
//! Every versioned INSERT/UPDATE/DELETE writes immortal version history
//! (`v:{table}:{row_id}:{ts}` values plus `v_idx:{table}:{row_id}:{rev_ts}`
//! reverse-index entries). Nothing ever collected them, so disk grows
//! without bound and aged databases pay for ever-longer dead version
//! chains. This module reclaims versions that no consumer can ever read
//! again, under a retention contract.
//!
//! ## Horizon (safe watermark)
//!
//! A GC cycle computes `horizon = min(of all of these)`:
//!
//! 1. **Retention**: the newest logical timestamp known (via persisted
//!    time anchors and snapshot-metadata wall clocks) to be older than
//!    `now - storage.version_retention`. No mapping old enough → no GC.
//! 2. **Pinned snapshots** (`WriteConflictRegistry::min_pinned_snapshot`):
//!    every open transaction pins its snapshot; statement-scoped `AS OF`
//!    readers and in-flight `CREATE BRANCH ... AS OF` pin theirs.
//! 3. **Branch anchors**: every active non-main branch pins its
//!    `created_from_snapshot`. Branch reads today overlay *current* main
//!    data, but merge-base resolution and `AS OF`-anchored branches hold
//!    the anchor timestamp, so a live branch conservatively pins history
//!    at its anchor.
//!
//! ## Chain-preservation rule
//!
//! For each row the collector deletes only versions `ts < horizon` for
//! which a NEWER version with `ts' <= horizon` exists — i.e. it always
//! keeps the newest version at-or-below the horizon (the version visible
//! at the horizon and at every `t >= horizon` until the next version),
//! plus everything above the horizon. It never deletes the only/latest
//! version of a row, and reconstruction at any `t >= horizon` is exact.
//! Deleted rows have no tombstone version (pre-existing schema), so their
//! final pre-delete version is also retained.
//!
//! ## Beyond-horizon semantics
//!
//! The highest horizon ever applied is persisted (`vgc:low_watermark`)
//! BEFORE the cycle's deletes (crash-safe direction: after a crash the
//! watermark may exceed what was actually deleted, so reads keep erroring
//! rather than seeing a half-pruned chain; deletes are idempotent and the
//! next cycle finishes the job). `AS OF` / `VERSIONS BETWEEN` starts /
//! branch anchors below the watermark fail with a clear error.
//!
//! ## Time anchors
//!
//! Version timestamps are logical. The retention window is wall-clock.
//! The collector maintains `vgc:anchor:{unix_secs:020} -> logical_ts`
//! records (one per cycle, pruned behind the cutoff) plus the snapshot
//! metadata wall clocks as the wallclock→logical mapping. Anchors stay
//! valid across restarts because the engine seeds its timestamp counter
//! from recovered snapshot metadata and the persisted watermark.

use crate::{Error, Result};
use parking_lot::{Mutex, RwLock};
use rocksdb::{ReadOptions, WriteBatch, DB};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

use super::branch::BranchManager;
use super::conflict::WriteConflictRegistry;
use super::time_travel::SnapshotManager;

/// Persisted low watermark: highest GC horizon ever applied (u64 BE).
pub const LOW_WATERMARK_KEY: &[u8] = b"vgc:low_watermark";
/// Persisted wallclock→logical-ts anchors: `vgc:anchor:{unix_secs:020}`.
pub const ANCHOR_PREFIX: &[u8] = b"vgc:anchor:";

/// Apply collected deletes in batches of this many keys.
const DELETE_BATCH_KEYS: usize = 1024;
/// Per-cycle scan bound multiplier (keys scanned vs. max deletions) so a
/// cycle over a mostly-live keyspace still terminates promptly.
const SCAN_BOUND_FACTOR: usize = 8;
/// In-memory time-anchor history bound.
const MAX_MEMORY_ANCHORS: usize = 4096;

/// Version-GC runtime configuration (resolved from `StorageConfig`).
#[derive(Debug, Clone)]
pub struct VersionGcConfig {
    /// Retention window in seconds. `None` = infinite = GC disabled.
    pub retention_secs: Option<u64>,
    /// Background cycle interval in seconds. `0` = no background worker.
    pub interval_secs: u64,
    /// Max versions reclaimed per cycle (latency bound).
    pub max_versions_per_cycle: usize,
}

/// Result of one bounded GC cycle.
#[derive(Debug, Clone, Default)]
pub struct VersionGcCycleStats {
    /// `v:` version values deleted.
    pub versions_deleted: u64,
    /// `v_idx:` reverse-index entries deleted.
    pub index_entries_deleted: u64,
    /// Version keys scanned.
    pub keys_scanned: u64,
    /// True when the cycle reached the end of the `v:` keyspace.
    pub finished_pass: bool,
    /// Horizon used for this cycle (None = retention not yet resolvable).
    pub horizon: Option<u64>,
}

/// Counts/sizes of the MVCC version keyspaces (diagnostics + tests).
#[derive(Debug, Clone, Default)]
pub struct VersionStorageStats {
    pub version_keys: u64,
    pub version_index_keys: u64,
    pub approx_bytes: u64,
}

/// Shared version-GC state and collector. One per `StorageEngine`.
pub struct VersionGc {
    db: Arc<DB>,
    snapshot_manager: Arc<SnapshotManager>,
    conflict_registry: Arc<WriteConflictRegistry>,
    branch_manager: Arc<RwLock<Option<Arc<BranchManager>>>>,
    /// Engine logical timestamp counter (read-only here, for anchors).
    timestamp: Arc<RwLock<u64>>,
    config: VersionGcConfig,
    /// Resume key for the next bounded cycle (None = start of `v:`).
    cursor: Mutex<Option<Vec<u8>>>,
    /// One collector at a time (background cycle vs. VACUUM VERSIONS).
    gc_lock: Mutex<()>,
    /// In-memory (unix_secs, logical_ts) anchors recorded this process.
    memory_anchors: Mutex<Vec<(i64, u64)>>,
}

impl VersionGc {
    pub fn new(
        db: Arc<DB>,
        snapshot_manager: Arc<SnapshotManager>,
        conflict_registry: Arc<WriteConflictRegistry>,
        branch_manager: Arc<RwLock<Option<Arc<BranchManager>>>>,
        timestamp: Arc<RwLock<u64>>,
        config: VersionGcConfig,
    ) -> Self {
        // Recover the persisted low watermark into the snapshot manager so
        // beyond-horizon reads error from the first statement after reopen,
        // and keep the engine timestamp counter at or above it.
        if let Ok(Some(raw)) = db.get(LOW_WATERMARK_KEY) {
            if let Some(bytes) = raw.get(0..8).and_then(|b| <[u8; 8]>::try_from(b).ok()) {
                let watermark = u64::from_be_bytes(bytes);
                snapshot_manager.set_gc_low_watermark(watermark);
                let mut ts = timestamp.write();
                if *ts < watermark {
                    *ts = watermark;
                }
            }
        }

        Self {
            db,
            snapshot_manager,
            conflict_registry,
            branch_manager,
            timestamp,
            config,
            cursor: Mutex::new(None),
            gc_lock: Mutex::new(()),
            memory_anchors: Mutex::new(Vec::new()),
        }
    }

    pub fn config(&self) -> &VersionGcConfig {
        &self.config
    }

    /// Record a wallclock→logical-ts anchor (in memory + persisted).
    pub fn record_time_anchor(&self) {
        let now_secs = chrono::Utc::now().timestamp();
        let ts = *self.timestamp.read();

        {
            let mut anchors = self.memory_anchors.lock();
            anchors.push((now_secs, ts));
            if anchors.len() > MAX_MEMORY_ANCHORS {
                let excess = anchors.len() - MAX_MEMORY_ANCHORS;
                anchors.drain(0..excess);
            }
        }

        let key = anchor_key(now_secs);
        if let Err(e) = self.db.put(&key, ts.to_be_bytes()) {
            warn!("version-gc: failed to persist time anchor: {}", e);
        }
    }

    /// One bounded GC cycle. Returns what it did; safe to call anytime
    /// (no-op when retention is infinite or not yet resolvable).
    pub fn run_cycle(&self) -> Result<VersionGcCycleStats> {
        let _guard = self.gc_lock.lock();
        self.record_time_anchor();
        self.run_cycle_locked()
    }

    /// `VACUUM VERSIONS`: restart from the beginning of the version
    /// keyspace and loop bounded cycles until a full pass completes.
    /// Returns the total number of version values reclaimed.
    pub fn vacuum(&self) -> Result<u64> {
        let _guard = self.gc_lock.lock();
        self.record_time_anchor();
        *self.cursor.lock() = None;

        let mut total = 0u64;
        loop {
            let stats = self.run_cycle_locked()?;
            total += stats.versions_deleted;
            if stats.finished_pass || stats.horizon.is_none() {
                break;
            }
        }
        Ok(total)
    }

    fn run_cycle_locked(&self) -> Result<VersionGcCycleStats> {
        let Some(retention_secs) = self.config.retention_secs else {
            return Ok(VersionGcCycleStats {
                finished_pass: true,
                ..Default::default()
            });
        };

        // 1. Retention horizon: newest logical ts provably older than the
        //    wall-clock cutoff. Nothing datable that far back → no GC.
        let cutoff_secs = chrono::Utc::now().timestamp() - retention_secs as i64;
        let Some(retention_ts) = self.retention_horizon(cutoff_secs)? else {
            return Ok(VersionGcCycleStats {
                finished_pass: true,
                ..Default::default()
            });
        };

        // 2. Clamp by pinned snapshots and branch anchors.
        let horizon = self.clamp_horizon(retention_ts)?;

        // 3. Publish the watermark BEFORE deleting (see module docs: after
        //    a crash, "error too eagerly" is safe; "read a pruned chain"
        //    is not). Monotonic.
        if horizon > self.snapshot_manager.gc_low_watermark() {
            self.db
                .put(LOW_WATERMARK_KEY, horizon.to_be_bytes())
                .map_err(|e| Error::storage(format!("version-gc: failed to persist watermark: {}", e)))?;
            self.snapshot_manager.set_gc_low_watermark(horizon);
        }

        // 4. Second look (closes the pin race): a reader pins BEFORE
        //    validating against the watermark, and the watermark store
        //    above precedes this re-read — so either we see the pin here,
        //    or the reader sees the new watermark and errors. Same for
        //    CREATE BRANCH (it pins its anchor while persisting).
        let delete_horizon = self.clamp_horizon(horizon)?;

        // 5. Collect.
        let mut stats = self.collect_below(delete_horizon)?;
        stats.horizon = Some(delete_horizon);

        if stats.versions_deleted > 0 {
            debug!(
                horizon = delete_horizon,
                deleted = stats.versions_deleted,
                scanned = stats.keys_scanned,
                finished = stats.finished_pass,
                "version-gc cycle"
            );
        }
        Ok(stats)
    }

    /// Newest logical timestamp known to be older than `cutoff_secs`
    /// (wallclock), from snapshot metadata + in-memory + persisted anchors.
    /// Also prunes persisted anchors strictly behind the chosen one.
    fn retention_horizon(&self, cutoff_secs: i64) -> Result<Option<u64>> {
        let mut best = self
            .snapshot_manager
            .max_snapshot_ts_at_or_before_wallclock(cutoff_secs);

        {
            let anchors = self.memory_anchors.lock();
            for (secs, ts) in anchors.iter() {
                if *secs <= cutoff_secs {
                    best = Some(best.map_or(*ts, |b| b.max(*ts)));
                }
            }
        }

        // Persisted anchors (survive restarts; keys sort by unix seconds).
        let mut stale_anchor_keys: Vec<Vec<u8>> = Vec::new();
        let mut best_anchor: Option<(Vec<u8>, u64)> = None;
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(ANCHOR_PREFIX, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("version-gc anchor scan: {}", e)))?;
            if !key.starts_with(ANCHOR_PREFIX) {
                break;
            }
            let Some(secs) = parse_anchor_secs(&key) else { continue };
            if secs > cutoff_secs {
                break;
            }
            let Some(bytes) = value.get(0..8).and_then(|b| <[u8; 8]>::try_from(b).ok()) else {
                continue;
            };
            let ts = u64::from_be_bytes(bytes);
            if let Some((prev_key, _)) = best_anchor.take() {
                stale_anchor_keys.push(prev_key);
            }
            best_anchor = Some((key.to_vec(), ts));
        }
        if let Some((_, ts)) = &best_anchor {
            best = Some(best.map_or(*ts, |b| b.max(*ts)));
        }
        if !stale_anchor_keys.is_empty() {
            let mut batch = WriteBatch::default();
            for key in stale_anchor_keys {
                batch.delete(&key);
            }
            self.db
                .write(batch)
                .map_err(|e| Error::storage(format!("version-gc anchor prune: {}", e)))?;
        }

        Ok(best)
    }

    /// min(candidate, pinned snapshots, active branch anchors).
    fn clamp_horizon(&self, candidate: u64) -> Result<u64> {
        let mut horizon = candidate;

        if let Some(pin_min) = self.conflict_registry.min_pinned_snapshot() {
            horizon = horizon.min(pin_min);
        }

        // Active non-main branches pin their creation anchor.
        let manager = self.branch_manager.read().clone();
        if let Some(mgr) = manager {
            for branch in mgr.list_branches()? {
                if branch.name == "main" || branch.created_from_snapshot == 0 {
                    continue;
                }
                horizon = horizon.min(branch.created_from_snapshot);
            }
        }

        Ok(horizon)
    }

    /// Scan the `v:` keyspace from the cursor and delete, per row, every
    /// version strictly older than the newest version at-or-below
    /// `horizon` (plus its `v_idx:` twin). Bounded by
    /// `max_versions_per_cycle` deletions / a scan budget.
    fn collect_below(&self, horizon: u64) -> Result<VersionGcCycleStats> {
        let mut stats = VersionGcCycleStats::default();
        let max_deletes = self.config.max_versions_per_cycle.max(1);
        let scan_budget = max_deletes.saturating_mul(SCAN_BOUND_FACTOR).max(65_536);

        let start_key: Vec<u8> = self.cursor.lock().clone().unwrap_or_else(|| b"v:".to_vec());

        // total_order_seek: the column family uses a fixed 5-byte prefix
        // extractor; a cross-prefix scan over the whole `v:` range needs
        // total-order iteration.
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(&start_key, rocksdb::Direction::Forward),
            read_opts,
        );

        let mut batch = WriteBatch::default();
        let mut batch_keys = 0usize;
        // Current row group: full `v:{table}:{row_id}:` prefix + its parsed
        // version timestamps. A row's versions are contiguous (identical
        // prefix) even though decimal timestamps don't sort numerically.
        let mut group_prefix: Vec<u8> = Vec::new();
        let mut group_versions: Vec<u64> = Vec::new();
        let mut next_cursor: Option<Vec<u8>> = None;
        let mut finished_pass = true;

        let flush = |batch: &mut WriteBatch, batch_keys: &mut usize, db: &DB| -> Result<()> {
            if *batch_keys > 0 {
                let owned = std::mem::take(batch);
                db.write(owned)
                    .map_err(|e| Error::storage(format!("version-gc delete batch: {}", e)))?;
                *batch_keys = 0;
            }
            Ok(())
        };

        let index_key_for = |prefix: &[u8], ts: u64| -> Vec<u8> {
            // v_idx:{table}:{row}:{(MAX-ts):020}
            let mut ikey = Vec::with_capacity(prefix.len() + 26);
            ikey.extend_from_slice(b"v_idx:");
            ikey.extend_from_slice(&prefix[2..]);
            ikey.extend_from_slice(format!("{:020}", u64::MAX - ts).as_bytes());
            ikey
        };

        let process_group_fn = |prefix: &[u8],
                                versions: &mut Vec<u64>,
                                batch: &mut WriteBatch,
                                batch_keys: &mut usize,
                                stats: &mut VersionGcCycleStats|
         -> Result<()> {
            if versions.len() > 1 {
                if let Some(keep) = versions.iter().copied().filter(|ts| *ts <= horizon).max() {
                    // C15 interaction: the autocommit UPDATE/DELETE paths
                    // write `v:` versions WITHOUT `v_idx:` entries, and
                    // `read_at_snapshot` resolves through `v_idx:` only.
                    // If the newest version <= horizon is un-indexed, also
                    // keep the newest INDEXED version <= horizon so indexed
                    // reads at t >= horizon return exactly what they did
                    // before GC. One bloom-backed point lookup per kept
                    // version; a walk only when the newest is un-indexed.
                    let mut keep_indexed: Option<u64> = None;
                    let keep_has_index = self
                        .db
                        .get(index_key_for(prefix, keep))
                        .map_err(|e| Error::storage(format!("version-gc index probe: {}", e)))?
                        .is_some();
                    if !keep_has_index {
                        let mut candidates: Vec<u64> = versions
                            .iter()
                            .copied()
                            .filter(|ts| *ts <= horizon && *ts != keep)
                            .collect();
                        candidates.sort_unstable_by(|a, b| b.cmp(a)); // newest first
                        for ts in candidates {
                            let indexed = self
                                .db
                                .get(index_key_for(prefix, ts))
                                .map_err(|e| Error::storage(format!("version-gc index probe: {}", e)))?
                                .is_some();
                            if indexed {
                                keep_indexed = Some(ts);
                                break;
                            }
                        }
                    }

                    for ts in versions.iter().copied() {
                        if ts <= horizon && ts != keep && Some(ts) != keep_indexed {
                            // v:{table}:{row}:{ts}
                            let mut vkey = Vec::with_capacity(prefix.len() + 20);
                            vkey.extend_from_slice(prefix);
                            vkey.extend_from_slice(ts.to_string().as_bytes());
                            batch.delete(&vkey);
                            batch.delete(index_key_for(prefix, ts));
                            stats.versions_deleted += 1;
                            stats.index_entries_deleted += 1;
                            *batch_keys += 2;
                        }
                    }
                    if *batch_keys >= DELETE_BATCH_KEYS {
                        flush(batch, batch_keys, &self.db)?;
                    }
                }
            }
            versions.clear();
            Ok(())
        };

        for item in iter {
            let (key, _value) = item.map_err(|e| Error::storage(format!("version-gc scan: {}", e)))?;
            if !key.starts_with(b"v:") {
                // End of the `v:` range (`v_idx:` sorts after `v:`).
                break;
            }

            // Split `v:{table}:{row_id}:{ts}` at the LAST ':'.
            let Some(last_colon) = key.iter().rposition(|b| *b == b':') else {
                continue;
            };
            let prefix = &key[..=last_colon];
            let Some(ts) = std::str::from_utf8(&key[last_colon + 1..])
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
            else {
                continue; // never touch keys we can't parse
            };

            if prefix != group_prefix.as_slice() {
                process_group_fn(
                    &group_prefix,
                    &mut group_versions,
                    &mut batch,
                    &mut batch_keys,
                    &mut stats,
                )?;
                // Bound checks happen at group boundaries so a row's chain
                // is always evaluated as a whole.
                if stats.versions_deleted >= max_deletes as u64 || stats.keys_scanned >= scan_budget as u64 {
                    next_cursor = Some(key.to_vec());
                    finished_pass = false;
                    break;
                }
                group_prefix = prefix.to_vec();
            }
            group_versions.push(ts);
            stats.keys_scanned += 1;
        }

        if finished_pass {
            process_group_fn(
                &group_prefix,
                &mut group_versions,
                &mut batch,
                &mut batch_keys,
                &mut stats,
            )?;
        }
        flush(&mut batch, &mut batch_keys, &self.db)?;

        *self.cursor.lock() = next_cursor;
        stats.finished_pass = finished_pass;
        Ok(stats)
    }
}

/// Count `v:` / `v_idx:` keys and their approximate byte footprint.
pub fn version_storage_stats(db: &DB) -> Result<VersionStorageStats> {
    let mut stats = VersionStorageStats::default();
    for (prefix, counter) in [(b"v:".as_slice(), 0usize), (b"v_idx:".as_slice(), 1usize)] {
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = db.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("version stats scan: {}", e)))?;
            if !key.starts_with(prefix) {
                break;
            }
            // The `v:` range scan must not double-count `v_idx:` (it
            // doesn't: `v_idx:` does not start with `v:`).
            if counter == 0 {
                stats.version_keys += 1;
            } else {
                stats.version_index_keys += 1;
            }
            stats.approx_bytes += (key.len() + value.len()) as u64;
        }
    }
    Ok(stats)
}

fn anchor_key(unix_secs: i64) -> Vec<u8> {
    let mut key = Vec::with_capacity(ANCHOR_PREFIX.len() + 20);
    key.extend_from_slice(ANCHOR_PREFIX);
    key.extend_from_slice(format!("{:020}", unix_secs.max(0)).as_bytes());
    key
}

fn parse_anchor_secs(key: &[u8]) -> Option<i64> {
    std::str::from_utf8(key.get(ANCHOR_PREFIX.len()..)?)
        .ok()?
        .parse::<i64>()
        .ok()
}

/// Background collector thread. Polls shutdown every 100ms so engine drop
/// joins promptly; runs one bounded cycle per interval.
pub struct VersionGcWorker {
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl VersionGcWorker {
    pub fn start(gc: Arc<VersionGc>, interval_secs: u64) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);

        let handle = std::thread::Builder::new()
            .name("version-gc".into())
            .spawn(move || {
                info!(interval_secs, "version-gc worker started");
                // Record an anchor immediately so the retention clock
                // starts at process start, not after the first interval.
                gc.record_time_anchor();
                let slices_per_interval = interval_secs.saturating_mul(10).max(1);
                'outer: loop {
                    for _ in 0..slices_per_interval {
                        if shutdown_clone.load(Ordering::Relaxed) {
                            break 'outer;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    if shutdown_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Err(e) = gc.run_cycle() {
                        warn!("version-gc cycle failed: {}", e);
                    }
                }
                debug!("version-gc worker stopped");
            })
            .ok();

        Self { shutdown, handle }
    }

    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for VersionGcWorker {
    fn drop(&mut self) {
        self.stop();
    }
}
