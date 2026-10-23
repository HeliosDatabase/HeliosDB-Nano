//! The WAL archiver (spec P3.1 §3-§4, plan §D4): every write group a node
//! applies is sealed into archived segments ([`super::archive_segment`]) and
//! kept for the recovery window, bounded by disk.
//!
//! # Both roles
//!
//! * A **primary** archives its own store: a thread started with its
//!   replication port tails the store's WAL through the same
//!   [`physical::poll_source`] every standby session uses, from the last
//!   sequence its archive holds.
//! * A **standby** archives every batch its receiver applied
//!   ([`ArchiveFeed::offer`], called right after a successful apply), so its
//!   frames are byte-for-byte the primary's for the same sequences.
//!
//! # Never in the write path
//!
//! Nothing here runs in a commit, a `WriteGate` or a transaction: the
//! archiver and its pruner run on their own thread, and every error they meet
//! is logged and lived with. The archive lags, records a gap, or shortens the
//! recovery window — the store never waits for it. A primary's archiver
//! re-polls at once only while it is behind (a poll stopped at its byte
//! limit); a poll that reached the store's barrier waits `poll_ms`, so the WAL
//! fsync a poll makes (`poll_source` flushes the WAL) runs at most once per
//! interval on a busy primary, and not at all on an idle one: the archiver's
//! own [`physical::FlushMemo`] (spec P4.1d) skips it while the barrier has not
//! moved. RocksDB's own WAL retention (`[replication.physical]
//! wal_retention_secs`) covers the archiver's lag. A standby's applier hands
//! batches over a queue bounded in bytes
//! ([`MAX_QUEUED_BYTES`]); when the archiver falls that far behind, batches
//! are left out of the archive (a gap, WARN) rather than slowing the applier.
//!
//! # Retention: the recovery window
//!
//! Every `prune_interval_secs` and after each seal ([`prune`]): (1) sealed
//! segments whose newest write group was observed before `now - window` are
//! pruned; (2) oldest first while the archive holds more than `max_bytes`;
//! (3) oldest first while the filesystem's free space is below the disk
//! reserve, re-measured after each removal. Steps 2 and 3 prune inside the
//! window — a **shortening**: WARN once per episode
//! (`recovery window shortened to …`), `pruned_inside_window_total` counts
//! it, and the episode ends (INFO once) when everything it pruned would have
//! expired anyway. The newest sealed segment and corrupt files are never
//! pruned; a segment a connected standby still needs is skipped by steps 2
//! and 3, and pruned by step 1 beyond the window (that standby then needs
//! recovery).

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use rocksdb::DB;
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};
use uuid::Uuid;

use super::archive_catalog::{
    measured_free_bytes, reserve_bytes, ArchiveCatalog, ArchiveGap, ArchiveStatus, PartialPolicy,
};
use super::archive_segment::{
    create_private_dir, parse_partial_name, repair_partial, segment_dir, set_aside_corrupt, sync_dir, unix_now,
    OpenSegment, SegmentError, SegmentHeader, SegmentInfo, SegmentResult, FLAG_SEALED, FOOTER_LEN, FRAME_META,
    HEADER_LEN, KEY_CHECK_KEY,
};
use super::physical::{self, PhysicalBatch, PhysicalError};
use crate::config::{ArchiveConfig, DiskReserve};
use crate::crypto::KeyManager;

/// The lock file a running archiver holds (`flock`) in its archive
/// directory: one archiver per directory, and no offline `archive prune`
/// under a running node.
pub const LOCK_FILE: &str = "archiver.lock";

/// How far (in bytes) a standby's archiver may fall behind its applier
/// before batches are left out of the archive instead of queued.
pub const MAX_QUEUED_BYTES: u64 = 256 * 1024 * 1024;

/// How far a primary's archiver reads its store's WAL per poll: a coalescing
/// target (spec P4.1c), so a larger write group still arrives whole, alone.
/// A poll holds at most this much, or that one larger group.
const ARCHIVE_POLL_BYTES: usize = 64 * 1024 * 1024;

/// How a node's segments are cut and fsynced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentLimits {
    /// A segment is sealed before it would grow past this, and as soon as it
    /// reaches it. A soft target (spec P4.1c): a single write group larger
    /// than it gets a segment of its own, never refused for its size.
    pub max_bytes: u64,
    /// A non-empty segment older than this many seconds is sealed.
    pub max_secs: u64,
    /// The open segment is fsynced each time this much was appended.
    pub fsync_bytes: u64,
}

/// `[replication.archive]`, resolved (spec P3.1 §3).
#[derive(Clone, Debug)]
pub struct ArchiveSettings {
    pub dir: PathBuf,
    pub limits: SegmentLimits,
    pub poll_interval: Duration,
    pub recovery_window_secs: u64,
    pub max_bytes: Option<u64>,
    pub disk_reserve: DiskReserve,
    pub prune_interval: Duration,
}

impl ArchiveSettings {
    /// `config`'s values with the resolved archive directory `dir`
    /// ([`ArchiveConfig::resolve_dir`]).
    pub fn from_config(config: &ArchiveConfig, dir: PathBuf) -> crate::Result<Self> {
        config.validate()?;
        Ok(Self {
            dir,
            limits: SegmentLimits {
                max_bytes: config.segment_max_bytes_value()?,
                max_secs: config.segment_max_secs,
                fsync_bytes: config.fsync_bytes_value()?,
            },
            poll_interval: Duration::from_millis(config.poll_ms),
            recovery_window_secs: config.recovery_window_secs()?,
            max_bytes: config.max_bytes_value()?,
            disk_reserve: config.disk_reserve_value()?,
            prune_interval: Duration::from_secs(config.prune_interval_secs),
        })
    }
}

/// The dataset key of a node configured by `config` (`[encryption]`), for a
/// standby's archive: its frames are sealed with the key the primary's are.
pub fn dataset_key(config: &crate::Config) -> crate::Result<Option<Arc<KeyManager>>> {
    if !config.encryption.enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(KeyManager::from_source(&config.encryption.key_source)?)))
}

/// SHA-256 of the store's key-verification sentinel; zeros when it has none.
fn key_check_of(store: &DB) -> [u8; 32] {
    match store.get(KEY_CHECK_KEY) {
        Ok(Some(value)) => Sha256::digest(&value).into(),
        _ => [0; 32],
    }
}

/// Human seconds for a log line: `42s`, `5m 3s`, `2h 10m`, `7d 0h`.
fn format_secs(secs: u64) -> String {
    let (days, hours, minutes, seconds) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60, secs % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// What [`SegmentWriter::set_aside`] did with a segment whose write failed.
#[derive(Debug)]
pub enum SetAside {
    /// Its intact frames were sealed.
    Repaired(Box<SegmentInfo>),
    /// It held no complete frame and was removed.
    Empty,
    /// It could not be repaired and was renamed `.corrupt`.
    Corrupt { path: PathBuf, reason: String },
}

/// Appends write groups to one history and epoch's segments, rolling them by
/// size and age (spec P3.1 §3). Used by both roles.
pub struct SegmentWriter {
    dir: PathBuf,
    history: Uuid,
    epoch: u64,
    node_id: Uuid,
    key_manager: Option<Arc<KeyManager>>,
    key_check: [u8; 32],
    limits: SegmentLimits,
    open: Option<OpenSegment>,
    /// An open segment whose seal failed, waiting for [`Self::set_aside`].
    unsealed: Option<PathBuf>,
    /// The last sequence written, sealed or open.
    cursor: Option<u64>,
}

impl SegmentWriter {
    /// A writer into `<root>/<history>/<epoch>`, continuing after `cursor`
    /// (the last sequence the archive holds there). Frames are sealed when a
    /// dataset key is given.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        root: &Path,
        history: Uuid,
        epoch: u64,
        node_id: Uuid,
        key_manager: Option<Arc<KeyManager>>,
        key_check: [u8; 32],
        limits: SegmentLimits,
        cursor: Option<u64>,
    ) -> Self {
        Self {
            dir: segment_dir(root, history, epoch),
            history,
            epoch,
            node_id,
            key_manager,
            key_check,
            limits,
            open: None,
            unsealed: None,
            cursor,
        }
    }

    pub fn history(&self) -> Uuid {
        self.history
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The last sequence written.
    pub fn cursor(&self) -> Option<u64> {
        self.cursor
    }

    /// Continue after `cursor` (after a gap: seal the open segment first, or
    /// the next append seals it, the interval being discontinuous).
    pub fn set_cursor(&mut self, cursor: Option<u64>) {
        self.cursor = cursor;
    }

    /// The key check new segments carry.
    pub fn set_key_check(&mut self, key_check: [u8; 32]) {
        self.key_check = key_check;
    }

    pub fn key_check(&self) -> [u8; 32] {
        self.key_check
    }

    pub fn sealed_frames(&self) -> bool {
        self.key_manager.is_some()
    }

    /// Append `batch`, observed at `now`. A write group the archive already
    /// holds is skipped; one that does not continue the open segment, or
    /// would grow it past `max_bytes`, seals it first, and a segment that
    /// reached `max_bytes` with it is sealed right after it. Spec P4.1c:
    /// `max_bytes` is a soft target and a group is never split, so one larger
    /// than it gets a segment of its own — sealed before it when the open
    /// segment holds anything, appended, sealed after it — and size never
    /// refuses a group. Returns the segments sealed by this call. On an error
    /// the open segment must be set aside ([`Self::set_aside`]).
    pub fn append(&mut self, batch: &PhysicalBatch, now: i64) -> SegmentResult<Vec<SegmentInfo>> {
        if let Some(cursor) = self.cursor {
            if batch.last_sequence() <= cursor {
                return Ok(Vec::new());
            }
            if batch.first_sequence() <= cursor {
                return Err(SegmentError {
                    path: self.dir.clone(),
                    reason: format!(
                        "write group {}..{} overlaps the archive, which holds up to {cursor}",
                        batch.first_sequence(),
                        batch.last_sequence()
                    ),
                });
            }
        }
        let frame = OpenSegment::frame_bytes(batch, self.sealed_frames());
        let limit = self.limits.max_bytes;
        let roll = self.open.as_ref().is_some_and(|open| {
            open.batch_count() > 0
                && (open.last_sequence().saturating_add(1) != batch.first_sequence()
                    || HEADER_LEN as u64 + open.body_bytes() + frame + FOOTER_LEN as u64 > limit)
        });
        let mut sealed = Vec::new();
        if roll {
            sealed.extend(self.seal()?);
        }
        if self.open.is_none() {
            let header = SegmentHeader {
                flags: if self.sealed_frames() { FLAG_SEALED } else { 0 },
                history: self.history,
                epoch: self.epoch,
                first_sequence: batch.first_sequence(),
                created_at: now,
                node_id: self.node_id,
                key_check: self.key_check,
            };
            self.open = Some(OpenSegment::create(&self.dir, header)?);
        }
        let key = self.key_manager.as_deref();
        let mut full = false;
        if let Some(open) = self.open.as_mut() {
            open.append(batch, now, key)?;
            self.cursor = Some(batch.last_sequence());
            full = HEADER_LEN as u64 + open.body_bytes() + FOOTER_LEN as u64 >= limit;
            // A full segment is sealed (and fsynced) below.
            if !full && open.unsynced_bytes() >= self.limits.fsync_bytes {
                open.sync()?;
            }
        }
        if full {
            sealed.extend(self.seal()?);
        }
        Ok(sealed)
    }

    /// Seal the open segment when it is non-empty and older than `max_secs`.
    pub fn roll_if_due(&mut self, now: i64) -> SegmentResult<Option<SegmentInfo>> {
        let max_secs = i64::try_from(self.limits.max_secs).unwrap_or(i64::MAX);
        let due = self
            .open
            .as_ref()
            .is_some_and(|open| open.batch_count() > 0 && now.saturating_sub(open.header().created_at) >= max_secs);
        if due {
            self.seal()
        } else {
            Ok(None)
        }
    }

    /// Seal the open segment (an empty one is removed instead).
    pub fn seal(&mut self) -> SegmentResult<Option<SegmentInfo>> {
        let Some(open) = self.open.take() else {
            return Ok(None);
        };
        let path = open.path().to_path_buf();
        if open.batch_count() == 0 && !open.failed() {
            drop(open);
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }
        match open.seal() {
            Ok(info) => Ok(Some(info)),
            Err(error) => {
                self.unsealed = Some(path);
                Err(error)
            }
        }
    }

    /// After a failed write or seal: repair what the open segment holds
    /// ([`repair_partial`]) or rename it `.corrupt`, and continue after what
    /// the archive then holds. `None` when there was nothing to set aside.
    pub fn set_aside(&mut self) -> Option<SetAside> {
        let path = match self.open.take() {
            Some(open) => {
                let path = open.path().to_path_buf();
                drop(open);
                path
            }
            None => self.unsealed.take()?,
        };
        let before = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(parse_partial_name)
            .map(|first| first.saturating_sub(1));
        Some(match repair_partial(&path, self.key_manager.as_deref()) {
            Ok(Some(info)) => {
                self.cursor = Some(info.last_sequence());
                SetAside::Repaired(Box::new(info))
            }
            Ok(None) => {
                self.cursor = before.or(self.cursor);
                SetAside::Empty
            }
            Err(error) => {
                let moved = set_aside_corrupt(&path).unwrap_or(path);
                self.cursor = before.or(self.cursor);
                SetAside::Corrupt {
                    path: moved,
                    reason: error.reason,
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Retention.
// ---------------------------------------------------------------------------

/// Why the pruner removed a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PruneReason {
    /// Beyond the recovery window (expected, no warning).
    Expired,
    /// The archive held more than `max_bytes` (inside the window).
    OverMaxBytes,
    /// Free space was below the disk reserve (inside the window).
    DiskReserve,
}

impl PruneReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Expired => "beyond the recovery window",
            Self::OverMaxBytes => "over max_bytes",
            Self::DiskReserve => "below the disk reserve",
        }
    }

    /// Steps 2 and 3 remove segments the window still covers.
    pub fn inside_window(&self) -> bool {
        !matches!(self, Self::Expired)
    }
}

/// One file the pruner may consider.
#[derive(Clone, Debug)]
pub struct PruneCandidate {
    pub history: Uuid,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub bytes: u64,
    pub last_observed_at: i64,
    /// A sealed, available segment. Corrupt files are never pruned.
    pub prunable: bool,
}

/// The retention policy at one moment.
#[derive(Clone, Copy, Debug)]
pub struct PrunePolicy {
    pub now: i64,
    pub window_secs: u64,
    pub max_bytes: Option<u64>,
    pub reserve_bytes: u64,
}

/// Where the pruner acts: removing a candidate, and measuring free space
/// (again after each removal, for the reserve).
pub trait PruneTarget {
    /// Free bytes on the archive's filesystem now; `None` when unknown (the
    /// reserve step then stops: unknown is not full).
    fn free_bytes(&mut self) -> Option<u64>;
    /// Remove candidate `index`; false when it could not be.
    fn remove(&mut self, index: usize) -> bool;
}

struct Pruning<'a> {
    candidates: &'a [PruneCandidate],
    removed: Vec<bool>,
    total: u64,
    decisions: Vec<(usize, PruneReason)>,
}

impl Pruning<'_> {
    fn take(&mut self, index: usize, reason: PruneReason, target: &mut dyn PruneTarget) {
        if let Some(flag) = self.removed.get_mut(index) {
            // Marked even when the removal fails, so it is not tried again.
            *flag = true;
        }
        if target.remove(index) {
            let bytes = self.candidates.get(index).map_or(0, |candidate| candidate.bytes);
            self.total = self.total.saturating_sub(bytes);
            self.decisions.push((index, reason));
        }
    }
}

/// Apply the retention policy (module documentation) to `candidates`, whose
/// files total `total_bytes`. `standbys`: (history, applied sequence) of the
/// connected standbys. Returns what was removed, in removal order.
pub fn prune(
    candidates: &[PruneCandidate],
    total_bytes: u64,
    policy: &PrunePolicy,
    standbys: &[(Uuid, u64)],
    target: &mut dyn PruneTarget,
) -> Vec<(usize, PruneReason)> {
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by_key(|index| {
        candidates
            .get(*index)
            .map(|candidate| (candidate.last_observed_at, candidate.first_sequence))
    });
    let newest = order
        .iter()
        .rev()
        .copied()
        .find(|index| candidates.get(*index).is_some_and(|candidate| candidate.prunable));
    let horizon = policy
        .now
        .saturating_sub(i64::try_from(policy.window_secs).unwrap_or(i64::MAX));
    let eligible = |index: usize, removed: &[bool]| {
        Some(index) != newest
            && !removed.get(index).copied().unwrap_or(true)
            && candidates.get(index).is_some_and(|candidate| candidate.prunable)
    };
    let needed = |index: usize| {
        candidates.get(index).is_some_and(|candidate| {
            standbys
                .iter()
                .any(|(history, applied)| *history == candidate.history && *applied < candidate.last_sequence)
        })
    };
    let mut state = Pruning {
        candidates,
        removed: vec![false; candidates.len()],
        total: total_bytes,
        decisions: Vec::new(),
    };
    // 1. Expired, whoever still needs them.
    for &index in &order {
        let expired = candidates
            .get(index)
            .is_some_and(|candidate| candidate.last_observed_at < horizon);
        if expired && eligible(index, &state.removed) {
            state.take(index, PruneReason::Expired, target);
        }
    }
    // 2. Oldest first while over max_bytes.
    if let Some(max) = policy.max_bytes {
        for &index in &order {
            if state.total <= max {
                break;
            }
            if eligible(index, &state.removed) && !needed(index) {
                state.take(index, PruneReason::OverMaxBytes, target);
            }
        }
    }
    // 3. Oldest first while free space is below the reserve, measured again
    //    after each removal.
    loop {
        match target.free_bytes() {
            Some(free) if free < policy.reserve_bytes => {}
            _ => break,
        }
        let Some(index) = order
            .iter()
            .copied()
            .find(|index| eligible(*index, &state.removed) && !needed(*index))
        else {
            break;
        };
        state.take(index, PruneReason::DiskReserve, target);
    }
    state.decisions
}

/// The catalog's files as prune candidates (paths in the same order).
fn candidates_of(catalog: &ArchiveCatalog) -> (Vec<PathBuf>, Vec<PruneCandidate>) {
    catalog
        .entries()
        .map(|entry| {
            (
                entry.path.clone(),
                PruneCandidate {
                    history: entry.history.unwrap_or_else(Uuid::nil),
                    first_sequence: entry.first_sequence.unwrap_or(0),
                    last_sequence: entry.last_sequence.unwrap_or(0),
                    bytes: entry.file_bytes,
                    last_observed_at: entry.info.as_ref().map_or(i64::MAX, SegmentInfo::last_observed_at),
                    prunable: entry.is_available(),
                },
            )
        })
        .unzip()
}

/// The pruner's target in an archive directory: removes files, measures the
/// filesystem, and fsyncs the directories it removed from.
struct DirectoryTarget<'a> {
    dir: &'a Path,
    paths: &'a [PathBuf],
    touched: BTreeSet<PathBuf>,
}

impl PruneTarget for DirectoryTarget<'_> {
    fn free_bytes(&mut self) -> Option<u64> {
        measured_free_bytes(self.dir)
    }

    fn remove(&mut self, index: usize) -> bool {
        let Some(path) = self.paths.get(index) else {
            return false;
        };
        match std::fs::remove_file(path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    self.touched.insert(parent.to_path_buf());
                }
                true
            }
            Err(error) => {
                warn!("archive: cannot prune {}: {error}", path.display());
                false
            }
        }
    }
}

impl DirectoryTarget<'_> {
    fn sync(&self) {
        for dir in &self.touched {
            if let Err(error) = sync_dir(dir) {
                warn!("archive: cannot fsync {} after pruning: {error}", dir.display());
            }
        }
    }
}

/// A dry run: nothing is removed; free space grows by what would be.
struct Simulated {
    free: Option<u64>,
    bytes: Vec<u64>,
}

impl PruneTarget for Simulated {
    fn free_bytes(&mut self) -> Option<u64> {
        self.free
    }

    fn remove(&mut self, index: usize) -> bool {
        if let (Some(free), Some(bytes)) = (self.free.as_mut(), self.bytes.get(index)) {
            *free = free.saturating_add(*bytes);
        }
        true
    }
}

/// The standbys this primary serves: (history, applied sequence) of every
/// streaming one (spec P3.1 §4: their needs hold steps 2 and 3 back).
fn connected_standbys() -> Vec<(Uuid, u64)> {
    use super::ha_state::{ha_state, StandbyState};
    ha_state()
        .get_standbys()
        .into_iter()
        .filter(|standby| standby.state == StandbyState::Streaming)
        .filter_map(|standby| standby.history.map(|history| (history, standby.applied_seq)))
        .collect()
}

/// One retention pass of a running archiver: prune, update the catalog and
/// the counters, and say so when the window is shortened or restored.
fn prune_running(status: &ArchiveStatus, settings: &ArchiveSettings, now: i64) {
    let policy = PrunePolicy {
        now,
        window_secs: settings.recovery_window_secs,
        max_bytes: settings.max_bytes,
        reserve_bytes: reserve_bytes(settings.disk_reserve, &settings.dir),
    };
    let standbys = connected_standbys();
    let mut catalog = status.catalog();
    let (paths, candidates) = candidates_of(&catalog);
    let mut target = DirectoryTarget {
        dir: &settings.dir,
        paths: &paths,
        touched: BTreeSet::new(),
    };
    let decisions = prune(&candidates, catalog.bytes_retained(), &policy, &standbys, &mut target);
    let mut inside = 0_u64;
    let mut until = i64::MIN;
    let mut reasons = BTreeSet::new();
    for (index, reason) in &decisions {
        if let Some(path) = paths.get(*index) {
            catalog.remove(path);
        }
        if reason.inside_window() {
            inside += 1;
            until = until.max(
                candidates
                    .get(*index)
                    .map_or(i64::MIN, |candidate| candidate.last_observed_at),
            );
            reasons.insert(match reason {
                PruneReason::OverMaxBytes => format!(
                    "the archive held more than max_bytes ({} bytes)",
                    settings.max_bytes.unwrap_or(0)
                ),
                _ => format!(
                    "free space on the archive's filesystem was below the disk reserve ({} bytes; disk_reserve = {})",
                    policy.reserve_bytes, settings.disk_reserve
                ),
            });
        }
    }
    let effective = catalog
        .oldest_observed_at()
        .map_or(0, |oldest| u64::try_from(now.saturating_sub(oldest)).unwrap_or(0));
    drop(catalog);
    target.sync();
    tracing::debug!(
        "archive: prune pass at {now}: {} decision(s), {inside} inside the window, until {until}, candidates {}",
        decisions.len(),
        candidates.len()
    );
    if inside > 0 {
        if status.note_pruned_inside_window(inside, until, now) {
            warn!(
                "recovery window shortened to {}: {} (configured {}; {inside} segment(s) pruned inside it). The \
                 primary's writes are never held back for the archive: raise max_bytes, free disk space or move the \
                 archive ([replication.archive] dir) to keep the full window",
                format_secs(effective),
                reasons.into_iter().collect::<Vec<_>>().join("; "),
                format_secs(settings.recovery_window_secs)
            );
        }
    } else if status.close_restored_episode(now) {
        info!(
            "recovery window restored to {}: nothing inside it has been pruned since it was shortened",
            format_secs(settings.recovery_window_secs)
        );
    }
}

// ---------------------------------------------------------------------------
// The running archiver.
// ---------------------------------------------------------------------------

/// The archive directory's lock: held (`flock`, exclusive) by the one
/// archiver writing into it, and by an offline `archive prune`. The file
/// names the holder's pid.
pub struct ArchiveLock {
    _file: File,
}

impl ArchiveLock {
    /// Lock `dir`, which must exist; refused (`WouldBlock`) when another
    /// process holds it.
    pub fn acquire(dir: &Path) -> std::io::Result<Self> {
        let path = dir.join(LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: flock(2) on a descriptor this function owns, with
            // standard flags; no pointers.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let error = std::io::Error::last_os_error();
                let holder = std::fs::read_to_string(&path)
                    .ok()
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty());
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!(
                        "{} is held by {} ({error})",
                        path.display(),
                        holder.map_or_else(|| "another process".to_string(), |pid| format!("process {pid}"))
                    ),
                ));
            }
        }
        file.set_len(0)?;
        file.write_all(format!("{}\n", std::process::id()).as_bytes())?;
        Ok(Self { _file: file })
    }
}

/// Interruptible sleep and the stop flag of an archiver thread.
#[derive(Default)]
struct StopSignal {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl StopSignal {
    fn stop(&self) {
        *self.stopped.lock() = true;
        self.wake.notify_all();
    }

    fn is_stopped(&self) -> bool {
        *self.stopped.lock()
    }

    /// Sleep up to `timeout`; true when stopped.
    fn wait(&self, timeout: Duration) -> bool {
        let mut stopped = self.stopped.lock();
        if !*stopped {
            let _ = self.wake.wait_for(&mut stopped, timeout);
        }
        *stopped
    }
}

/// A running archiver. [`Self::shutdown`] stops it after it sealed its open
/// segment; dropping it only asks it to stop.
pub struct ArchiveService {
    stop: Arc<StopSignal>,
    thread: Option<std::thread::JoinHandle<()>>,
    status: Arc<ArchiveStatus>,
}

impl ArchiveService {
    pub fn status(&self) -> &Arc<ArchiveStatus> {
        &self.status
    }

    /// Stop the archiver and wait for it (blocking: call it off the async
    /// runtime). Its open segment is sealed first.
    pub fn shutdown(mut self) {
        self.stop.stop();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                error!("archive: the WAL archiver thread panicked");
            }
        }
    }
}

impl Drop for ArchiveService {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

/// Warn once per distinct complaint a minute: a full disk is reported, not
/// repeated every poll.
#[derive(Default)]
struct Complaint {
    last: Option<(String, Instant)>,
}

impl Complaint {
    fn note(&mut self, message: &str) {
        let repeat = self
            .last
            .as_ref()
            .is_some_and(|(last, at)| last == message && at.elapsed() < Duration::from_secs(60));
        if !repeat {
            warn!("{message}");
            self.last = Some((message.to_string(), Instant::now()));
        }
    }

    fn clear(&mut self) {
        self.last = None;
    }
}

/// The archive's state on one thread: its writer, the lock, the cadence.
struct Session {
    role: &'static str,
    settings: ArchiveSettings,
    status: Arc<ArchiveStatus>,
    writer: SegmentWriter,
    _lock: ArchiveLock,
    next_prune: Instant,
    complaint: Complaint,
}

fn give_up(status: &ArchiveStatus, reason: String) -> Option<Session> {
    warn!("archive: the WAL archive is off on this node: {reason}");
    status.disable(reason);
    None
}

impl Session {
    /// Create and lock the directory, scan it (repairing open segments) and
    /// continue after the last sequence it holds for `history`. `None` (the
    /// archive disabled, with the reason in its status) when that fails.
    #[allow(clippy::too_many_arguments)]
    fn open(
        role: &'static str,
        settings: ArchiveSettings,
        status: Arc<ArchiveStatus>,
        history: Uuid,
        node_id: Uuid,
        key_manager: Option<Arc<KeyManager>>,
        key_check: [u8; 32],
    ) -> Option<Self> {
        let dir = settings.dir.clone();
        if let Err(error) = create_private_dir(&dir) {
            return give_up(
                &status,
                format!("cannot create the archive directory {}: {error}", dir.display()),
            );
        }
        let lock = match ArchiveLock::acquire(&dir) {
            Ok(lock) => lock,
            Err(error) => {
                return give_up(
                    &status,
                    format!(
                        "the archive directory {} is in use: {error}; give each node its own [replication.archive] dir",
                        dir.display()
                    ),
                )
            }
        };
        let (catalog, report) = match ArchiveCatalog::scan(&dir, PartialPolicy::Repair(key_manager.as_deref())) {
            Ok(scanned) => scanned,
            Err(error) => {
                return give_up(
                    &status,
                    format!("cannot scan the archive directory {}: {error}", dir.display()),
                )
            }
        };
        for info in &report.repaired {
            info!(
                "archive: repaired the open segment {}: {} write groups, sequences {}..{}, sealed",
                info.path.display(),
                info.batch_count(),
                info.first_sequence(),
                info.last_sequence()
            );
        }
        for path in &report.removed {
            info!(
                "archive: removed the open segment {}: it held no complete write group",
                path.display()
            );
        }
        for (path, reason) in &report.set_aside {
            warn!(
                "archive: an open segment could not be repaired and was set aside as {} ({reason})",
                path.display()
            );
        }
        let epoch = crate::standby_posture::EPOCH;
        let cursor = catalog.last_sequence(history, epoch);
        status.replace_catalog(catalog);
        if let Some(cursor) = cursor {
            status.note_archived_through(cursor);
        }
        let writer = SegmentWriter::new(
            &dir,
            history,
            epoch,
            node_id,
            key_manager,
            key_check,
            settings.limits,
            cursor,
        );
        Some(Self {
            role,
            next_prune: Instant::now(),
            complaint: Complaint::default(),
            settings,
            status,
            writer,
            _lock: lock,
        })
    }

    fn sealed(&mut self, info: SegmentInfo) {
        self.status.catalog().insert(info);
        // Prune right after a roll (spec P3.1 §4).
        self.next_prune = Instant::now();
    }

    /// Archive one write group; false when it could not be (the open
    /// segment is set aside and the archive continues after what it holds).
    fn append(&mut self, batch: &PhysicalBatch, now: i64) -> bool {
        match self.writer.append(batch, now) {
            Ok(sealed) => {
                for info in sealed {
                    self.sealed(info);
                }
                if let Some(cursor) = self.writer.cursor() {
                    self.status.note_archived_through(cursor);
                }
                self.complaint.clear();
                true
            }
            Err(error) => {
                self.complaint.note(&format!(
                    "archive: cannot archive write group {}..{}: {error}",
                    batch.first_sequence(),
                    batch.last_sequence()
                ));
                self.set_aside();
                false
            }
        }
    }

    fn set_aside(&mut self) {
        let outcome = self.writer.set_aside();
        if let Some(cursor) = self.writer.cursor() {
            self.status.note_archived_through(cursor);
        }
        match outcome {
            Some(SetAside::Repaired(info)) => {
                info!(
                    "archive: sealed the {} intact write groups of a segment whose write failed ({})",
                    info.batch_count(),
                    info.path.display()
                );
                self.sealed(*info);
            }
            Some(SetAside::Corrupt { path, reason }) => {
                warn!(
                    "archive: a segment whose write failed could not be repaired and was set aside as {} ({reason})",
                    path.display()
                );
                let filed = Some((self.writer.history(), self.writer.epoch()));
                self.status.catalog().insert_corrupt(&path, filed, reason);
            }
            Some(SetAside::Empty) | None => {}
        }
    }

    fn seal_open(&mut self) {
        match self.writer.seal() {
            Ok(Some(info)) => self.sealed(info),
            Ok(None) => {}
            Err(error) => {
                self.complaint.note(&format!("archive: cannot seal a segment: {error}"));
                self.set_aside();
            }
        }
    }

    /// The archive lacks `from..=to` (logged once, listed by
    /// `heliosdb_archive_gaps`).
    fn gap(&mut self, from: u64, to: u64, reason: &str) {
        if from > to {
            return;
        }
        warn!("archive gap {from}..{to}: {reason}");
        self.status.catalog().record_gap(ArchiveGap {
            history: self.writer.history(),
            epoch: self.writer.epoch(),
            from_sequence: from,
            to_sequence: to,
            reason: reason.to_string(),
        });
    }

    /// Continue after `to`; `from..=to` is missing.
    fn jump(&mut self, from: u64, to: u64, reason: &str) {
        self.seal_open();
        self.gap(from, to, reason);
        self.writer.set_cursor(Some(to));
        self.status.note_archived_through(to);
    }

    /// Roll by age, and prune when it is time (or right after a roll).
    fn maintain(&mut self, now: i64) {
        match self.writer.roll_if_due(now) {
            Ok(Some(info)) => self.sealed(info),
            Ok(None) => {}
            Err(error) => {
                self.complaint.note(&format!("archive: cannot seal a segment: {error}"));
                self.set_aside();
            }
        }
        if Instant::now() >= self.next_prune {
            prune_running(&self.status, &self.settings, now);
            self.next_prune = Instant::now() + self.settings.prune_interval;
        }
    }

    fn finish(mut self) {
        self.seal_open();
        info!(
            "archive: the {} archiver stopped; the archive holds up to sequence {}",
            self.role,
            self.writer
                .cursor()
                .map_or_else(|| "none".to_string(), |cursor| cursor.to_string())
        );
    }
}

/// Where a primary's archive continues after `cursor` (its last sequence)
/// when the store is at `barrier`: right after it while the store's WAL
/// still holds the next write group; else at the oldest write group the WAL
/// holds, the interval in between recorded as a gap the store no longer
/// holds (spec P3.1 §3). The archiver never waits for it.
fn resume_point(store: &DB, session: &mut Session, cursor: u64, barrier: u64) -> u64 {
    if cursor >= barrier {
        return cursor;
    }
    match physical::wal_retains_after(store, cursor) {
        Ok(true) => cursor,
        Ok(false) => {
            let resume = match physical::oldest_retained_sequence(store) {
                Ok(Some(oldest)) => oldest.saturating_sub(1),
                _ => barrier,
            }
            .clamp(cursor, barrier);
            session.gap(cursor + 1, resume, "the store no longer holds it");
            resume
        }
        Err(error) => {
            warn!(
                "archive: cannot tell whether the store's WAL still holds sequence {}: {error}",
                cursor + 1
            );
            cursor
        }
    }
}

fn backoff(poll: Duration, failures: u32) -> Duration {
    poll.saturating_mul(2_u32.saturating_pow(failures.min(5)))
        .min(Duration::from_secs(30))
}

/// A primary's archiver thread (spec P3.1 §3).
#[allow(clippy::too_many_arguments)]
fn run_primary(
    settings: ArchiveSettings,
    store: Arc<DB>,
    history: Uuid,
    node_id: Uuid,
    key_manager: Option<Arc<KeyManager>>,
    status: Arc<ArchiveStatus>,
    stop: Arc<StopSignal>,
) {
    let key_check = if key_manager.is_some() {
        key_check_of(&store)
    } else {
        [0; 32]
    };
    let poll = settings.poll_interval;
    let Some(mut session) = Session::open(
        "primary",
        settings,
        Arc::clone(&status),
        history,
        node_id,
        key_manager,
        key_check,
    ) else {
        return;
    };
    let barrier = store.latest_sequence_number();
    let start = match session.writer.cursor() {
        None => {
            info!(
                "archive: the archive of history {history} is empty; it covers this primary from sequence {} on \
                 (a snapshot covers the history before it)",
                barrier.saturating_add(1)
            );
            barrier
        }
        Some(cursor) if cursor > barrier => {
            let reason = format!(
                "the archive in {} holds sequences up to {cursor}, past this store's {barrier}: it belongs to another \
                 copy of this dataset; move it aside",
                session.settings.dir.display()
            );
            error!("archive: the WAL archive is off on this node: {reason}");
            status.disable(reason);
            return;
        }
        Some(cursor) => resume_point(&store, &mut session, cursor, barrier),
    };
    session.writer.set_cursor(Some(start));
    status.note_archived_through(start);
    info!(
        "archive: primary archiving history {history} into {} after sequence {start}",
        session.settings.dir.display()
    );
    let mut failures = 0_u32;
    // Spec P4.1d: the archiver's own memo of the barrier its polls fsynced,
    // so an idle store is fsynced once per barrier, not every `poll_ms`.
    let flushed = physical::FlushMemo::new();
    while !stop.is_stopped() {
        let now = unix_now();
        let cursor = session.writer.cursor().unwrap_or(start);
        let mut behind = false;
        let mut failed = false;
        match physical::poll_source(&store, history, cursor, ARCHIVE_POLL_BYTES, &flushed) {
            Ok(polled) => {
                for batch in &polled.batches {
                    if !session.append(batch, now) {
                        failed = true;
                        break;
                    }
                }
                behind = !failed && !polled.batches.is_empty() && polled.through < polled.durable_barrier;
            }
            Err(PhysicalError::Pruned { wanted, oldest }) => {
                let resume = oldest.map_or_else(|| store.latest_sequence_number(), |oldest| oldest.saturating_sub(1));
                session.jump(wanted, resume.max(cursor), "the store no longer holds it");
            }
            Err(PhysicalError::Gap { observed, .. }) => {
                // The poll's groups before the discontinuity were not
                // returned either: the gap starts where the archive ends.
                session.jump(
                    cursor.saturating_add(1),
                    observed.saturating_sub(1).max(cursor),
                    "the store's WAL is discontinuous here",
                );
            }
            Err(PhysicalError::Ahead { after, barrier }) => {
                let reason = format!(
                    "the archive is at sequence {after}, past this store's {barrier}: the store was replaced by an \
                     older copy of this dataset; move the archive aside"
                );
                error!("archive: the WAL archive is off on this node: {reason}");
                status.disable(reason);
                break;
            }
            Err(error) => {
                session.complaint.note(&format!(
                    "archive: cannot read the store's WAL after sequence {cursor}: {error}"
                ));
                failed = true;
            }
        }
        session.maintain(unix_now());
        failures = if failed { failures.saturating_add(1) } else { 0 };
        let wait = if behind {
            Duration::ZERO
        } else if failures > 0 {
            backoff(poll, failures)
        } else {
            poll
        };
        if !wait.is_zero() && stop.wait(wait) {
            break;
        }
    }
    session.finish();
}

/// What a standby's applier and its archiver share.
struct FeedState {
    queued_bytes: AtomicU64,
    dropped: AtomicU64,
    closed: AtomicBool,
    sealed: bool,
    key_check: OnceLock<[u8; 32]>,
}

fn queued_size(batch: &PhysicalBatch) -> u64 {
    FRAME_META as u64 + batch.data().len() as u64
}

impl FeedState {
    fn received(&self, batch: &PhysicalBatch) {
        let bytes = queued_size(batch);
        let _ = self
            .queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                Some(queued.saturating_sub(bytes))
            });
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

/// A standby's hand-off from its applier to its archiver (spec P3.1 §3):
/// called by the receiver right after a batch was applied.
#[derive(Clone)]
pub struct ArchiveFeed {
    state: Arc<FeedState>,
    sender: Arc<Mutex<mpsc::Sender<PhysicalBatch>>>,
}

impl ArchiveFeed {
    /// Queue `batch`, applied to `store`, for the archiver — without
    /// waiting: when the archiver is [`MAX_QUEUED_BYTES`] behind, or gone,
    /// the batch is left out of the archive (it then records a gap) and
    /// false is returned. The applier never waits for the archive.
    pub fn offer(&self, batch: &PhysicalBatch, store: &DB) -> bool {
        let state = &self.state;
        if state.closed.load(Ordering::Acquire) {
            return false;
        }
        if state.sealed && state.key_check.get().is_none() {
            let _ = state.key_check.set(key_check_of(store));
        }
        let bytes = queued_size(batch);
        let queued = state.queued_bytes.load(Ordering::Acquire);
        if queued > 0 && queued.saturating_add(bytes) > MAX_QUEUED_BYTES {
            let dropped = state.dropped.fetch_add(1, Ordering::AcqRel) + 1;
            if dropped.is_power_of_two() {
                warn!(
                    "archive: the archiver is {queued} bytes behind this standby's applier; write group {}..{} is \
                     not archived ({dropped} so far): the archive will show a gap",
                    batch.first_sequence(),
                    batch.last_sequence()
                );
            }
            return false;
        }
        state.queued_bytes.fetch_add(bytes, Ordering::AcqRel);
        if self.sender.lock().send(batch.clone()).is_err() {
            state.received(batch);
            state.close();
            return false;
        }
        true
    }

    /// Batches left out of the archive so far.
    pub fn dropped(&self) -> u64 {
        self.state.dropped.load(Ordering::Acquire)
    }
}

/// A standby's archiver thread (spec P3.1 §3).
#[allow(clippy::too_many_arguments)]
fn run_standby(
    settings: ArchiveSettings,
    history: Uuid,
    node_id: Uuid,
    key_manager: Option<Arc<KeyManager>>,
    batches: mpsc::Receiver<PhysicalBatch>,
    feed: Arc<FeedState>,
    status: Arc<ArchiveStatus>,
    stop: Arc<StopSignal>,
) {
    let tick = settings.poll_interval.min(Duration::from_secs(1));
    let Some(mut session) = Session::open(
        "standby",
        settings,
        Arc::clone(&status),
        history,
        node_id,
        key_manager,
        [0; 32],
    ) else {
        feed.close();
        return;
    };
    info!(
        "archive: standby archiving every applied write group of history {history} into {}",
        session.settings.dir.display()
    );
    while !stop.is_stopped() {
        match batches.recv_timeout(tick) {
            Ok(batch) => archive_applied(&mut session, &feed, &batch),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        session.maintain(unix_now());
    }
    // What the applier handed over before the stop was applied: archive it.
    while let Ok(batch) = batches.try_recv() {
        archive_applied(&mut session, &feed, &batch);
    }
    feed.close();
    session.finish();
}

/// Archive one batch a standby applied; a discontinuity (batches the
/// applier applied but the archive never got) is recorded as a gap first.
fn archive_applied(session: &mut Session, feed: &FeedState, batch: &PhysicalBatch) {
    feed.received(batch);
    if session.writer.key_check() == [0; 32] {
        if let Some(check) = feed.key_check.get() {
            session.writer.set_key_check(*check);
        }
    }
    if let Some(cursor) = session.writer.cursor() {
        if batch.first_sequence() > cursor.saturating_add(1) {
            session.jump(
                cursor + 1,
                batch.first_sequence() - 1,
                "applied on this standby but not archived",
            );
        }
    }
    session.append(batch, unix_now());
}

fn install_status(settings: &ArchiveSettings) -> Arc<ArchiveStatus> {
    let status = Arc::new(ArchiveStatus::new(
        settings.dir.clone(),
        settings.recovery_window_secs,
        settings.max_bytes,
        settings.disk_reserve,
    ));
    ArchiveStatus::install(Arc::clone(&status));
    status
}

fn spawn_archiver(
    status: Arc<ArchiveStatus>,
    body: impl FnOnce(Arc<ArchiveStatus>, Arc<StopSignal>) + Send + 'static,
) -> ArchiveService {
    let stop = Arc::new(StopSignal::default());
    let spawned = std::thread::Builder::new().name("nano-wal-archiver".into()).spawn({
        let status = Arc::clone(&status);
        let stop = Arc::clone(&stop);
        move || body(status, stop)
    });
    match spawned {
        Ok(thread) => ArchiveService {
            stop,
            thread: Some(thread),
            status,
        },
        Err(error) => {
            let reason = format!("cannot start the archiver thread: {error}");
            warn!("archive: the WAL archive is off on this node: {reason}");
            status.disable(reason);
            ArchiveService {
                stop,
                thread: None,
                status,
            }
        }
    }
}

/// Start a primary's archiver over `storage` (its store, and its dataset key
/// when encrypted), for `history`. It never fails the primary: a directory
/// it cannot use disables the archive, with the reason in
/// `heliosdb_recovery_window` and the log.
pub fn start_primary(
    settings: ArchiveSettings,
    storage: &crate::storage::StorageEngine,
    history: Uuid,
    node_id: Uuid,
) -> ArchiveService {
    let status = install_status(&settings);
    let key_manager = storage.key_manager_arc();
    let store = storage.db();
    spawn_archiver(status, move |status, stop| {
        run_primary(settings, store, history, node_id, key_manager, status, stop);
    })
}

/// Start a standby's archiver for `history` (its binding); the receiver
/// hands it every applied batch through the returned feed.
pub fn start_standby(
    settings: ArchiveSettings,
    history: Uuid,
    node_id: Uuid,
    key_manager: Option<Arc<KeyManager>>,
) -> (ArchiveService, ArchiveFeed) {
    let status = install_status(&settings);
    let (sender, receiver) = mpsc::channel();
    let state = Arc::new(FeedState {
        queued_bytes: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
        closed: AtomicBool::new(false),
        sealed: key_manager.is_some(),
        key_check: OnceLock::new(),
    });
    let feed = ArchiveFeed {
        state: Arc::clone(&state),
        sender: Arc::new(Mutex::new(sender)),
    };
    let service = spawn_archiver(status, move |status, stop| {
        run_standby(settings, history, node_id, key_manager, receiver, state, status, stop);
    });
    if service.thread.is_none() {
        feed.state.close();
    }
    (service, feed)
}

/// One file an offline prune removed (or would remove).
#[derive(Clone, Debug)]
pub struct OfflinePruned {
    pub path: PathBuf,
    pub reason: PruneReason,
    pub bytes: u64,
}

/// `heliosdb-nano archive prune`: apply `settings`' retention policy to its
/// directory while no node runs on it (spec P3.1 §5). Refused while a node's
/// archiver holds the directory's lock. Without a running primary no standby
/// cursor is known, so standby needs do not hold steps 2 and 3 back. A dry
/// run changes nothing and takes no lock.
pub fn prune_offline(settings: &ArchiveSettings, dry_run: bool) -> crate::Result<Vec<OfflinePruned>> {
    let dir = &settings.dir;
    let _lock = if dry_run {
        None
    } else {
        Some(ArchiveLock::acquire(dir).map_err(|error| {
            crate::Error::config(format!(
                "archive prune refused: cannot lock {} ({error}); a running node archives into it and prunes it \
                 itself — stop that node first",
                dir.display()
            ))
        })?)
    };
    let (catalog, _) = ArchiveCatalog::scan(dir, PartialPolicy::Leave)
        .map_err(|error| crate::Error::io(format!("cannot scan the archive directory {}: {error}", dir.display())))?;
    let (paths, candidates) = candidates_of(&catalog);
    let policy = PrunePolicy {
        now: unix_now(),
        window_secs: settings.recovery_window_secs,
        max_bytes: settings.max_bytes,
        reserve_bytes: reserve_bytes(settings.disk_reserve, dir),
    };
    let decisions = if dry_run {
        let mut target = Simulated {
            free: measured_free_bytes(dir),
            bytes: candidates.iter().map(|candidate| candidate.bytes).collect(),
        };
        prune(&candidates, catalog.bytes_retained(), &policy, &[], &mut target)
    } else {
        let mut target = DirectoryTarget {
            dir,
            paths: &paths,
            touched: BTreeSet::new(),
        };
        let decisions = prune(&candidates, catalog.bytes_retained(), &policy, &[], &mut target);
        target.sync();
        decisions
    };
    Ok(decisions
        .into_iter()
        .filter_map(|(index, reason)| {
            Some(OfflinePruned {
                path: paths.get(index)?.clone(),
                reason,
                bytes: candidates.get(index)?.bytes,
            })
        })
        .collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::replication::archive_segment::{tests::batches, SegmentReader};
    use rocksdb::{Options, WriteBatch};
    use tempfile::TempDir;

    const LIMITS: SegmentLimits = SegmentLimits {
        max_bytes: 1 << 20,
        max_secs: 300,
        fsync_bytes: 4096,
    };

    fn segments(root: &Path) -> Vec<SegmentInfo> {
        let (catalog, _) = ArchiveCatalog::scan(root, PartialPolicy::Leave).unwrap();
        catalog.entries().filter_map(|entry| entry.info.clone()).collect()
    }

    fn assert_contiguous(segments: &[SegmentInfo]) {
        for pair in segments.windows(2) {
            assert_eq!(pair[0].last_sequence() + 1, pair[1].first_sequence(), "{pair:?}");
        }
    }

    /// Spec P3.1 §3: a segment is sealed before it would pass
    /// `segment_max_bytes` (a frame is never split) and once it is older than
    /// `segment_max_secs`.
    #[test]
    fn a_writer_rolls_by_size_and_by_age() {
        let root = TempDir::new().unwrap();
        let (history, batches) = batches(12, 4);
        let frame = OpenSegment::frame_bytes(&batches[0], false);
        let limits = SegmentLimits {
            // Three frames fit, the fourth does not.
            max_bytes: HEADER_LEN as u64 + FOOTER_LEN as u64 + 3 * frame + frame / 2,
            ..LIMITS
        };
        let mut writer = SegmentWriter::new(root.path(), history, 1, Uuid::new_v4(), None, [0; 32], limits, None);
        let mut sealed = Vec::new();
        for batch in &batches[..10] {
            sealed.extend(writer.append(batch, 1_000).unwrap());
        }
        assert_eq!(sealed.len(), 3, "10 groups, 3 per segment: 3 sealed, 1 open");
        assert!(sealed
            .iter()
            .all(|info| info.batch_count() == 3 && info.file_bytes <= limits.max_bytes));
        assert_eq!(writer.roll_if_due(1_000 + 299).unwrap(), None, "not old enough");
        let by_age = writer.roll_if_due(1_000 + 300).unwrap().expect("sealed by age");
        assert_eq!(by_age.batch_count(), 1);
        assert_eq!(writer.roll_if_due(2_000).unwrap(), None, "nothing open");
        // A group the archive already holds is skipped.
        assert!(writer.append(&batches[3], 2_000).unwrap().is_empty());
        let mut all = segments(root.path());
        all.sort_by_key(SegmentInfo::first_sequence);
        assert_eq!(all.len(), 4);
        assert_contiguous(&all);
        assert_eq!(all[0].first_sequence(), batches[0].first_sequence());
        assert_eq!(all[3].last_sequence(), batches[9].last_sequence());
        // A frame larger than the limit gets a segment of its own, sealed
        // as soon as it holds it (spec P4.1c).
        let tiny = SegmentLimits { max_bytes: 1, ..LIMITS };
        let mut writer = SegmentWriter::new(
            root.path(),
            history,
            2,
            Uuid::new_v4(),
            None,
            [0; 32],
            tiny,
            Some(batches[9].last_sequence()),
        );
        for batch in &batches[10..12] {
            let rolled = writer.append(batch, 3_000).unwrap();
            assert_eq!(rolled.len(), 1);
            assert_eq!(rolled[0].batch_count(), 1);
            assert_eq!(rolled[0].first_sequence(), batch.first_sequence());
        }
        assert_eq!(writer.seal().unwrap(), None, "nothing left open");
    }

    /// A small write group, one of 100 MiB (100 puts of 1 MiB values) and
    /// another small one, as the archiver sees them.
    fn small_large_small() -> (Uuid, Vec<PhysicalBatch>) {
        let root = TempDir::new().unwrap();
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_wal_ttl_seconds(3600);
        let db = DB::open(&options, root.path().join("source")).unwrap();
        db.put(b"data:small:1", b"one").unwrap();
        let mut large = WriteBatch::default();
        for index in 0..100_u32 {
            large.put(
                format!("data:large:{index:03}"),
                vec![u8::try_from(index % 251).unwrap(); 1 << 20],
            );
        }
        db.write(large).unwrap();
        db.put(b"data:small:2", b"two").unwrap();
        let history = Uuid::new_v4();
        let polled =
            physical::poll_source(&db, history, 0, physical::MAX_BATCH_BYTES, &physical::FlushMemo::new()).unwrap();
        assert_eq!(polled.batches.len(), 3);
        (history, polled.batches)
    }

    /// Spec P4.1c §3: `segment_max_bytes` is a soft target. A 100 MiB write
    /// group after a small one gets a segment of its own — the open segment
    /// sealed before it, its own sealed right after it, never refused for its
    /// size — and reads back equal and deep-verified; with sealed (encrypted)
    /// frames too.
    #[test]
    fn a_write_group_larger_than_a_segment_gets_a_segment_of_its_own() {
        let (history, groups) = small_large_small();
        for key in [None, Some(Arc::new(KeyManager::generate_random()))] {
            let root = TempDir::new().unwrap();
            let limits = SegmentLimits {
                max_bytes: 1 << 20,
                ..LIMITS
            };
            let mut writer = SegmentWriter::new(
                root.path(),
                history,
                1,
                Uuid::new_v4(),
                key.clone(),
                [0; 32],
                limits,
                None,
            );
            assert!(
                writer.append(&groups[0], 1_000).unwrap().is_empty(),
                "a small group: the segment stays open"
            );
            let sealed = writer.append(&groups[1], 1_000).unwrap();
            assert_eq!(sealed.len(), 2, "sealed before the large group and after it");
            assert_eq!(
                (
                    sealed[0].first_sequence(),
                    sealed[0].last_sequence(),
                    sealed[0].batch_count()
                ),
                (groups[0].first_sequence(), groups[0].last_sequence(), 1)
            );
            let own = &sealed[1];
            assert_eq!(
                (own.first_sequence(), own.last_sequence(), own.batch_count()),
                (groups[1].first_sequence(), groups[1].last_sequence(), 1)
            );
            assert!(own.file_bytes > 100 << 20, "{} bytes", own.file_bytes);
            assert_eq!(own.sealed_frames(), key.is_some());
            own.verify_deep(key.as_deref()).unwrap();
            let read: Vec<PhysicalBatch> = own.frames(key.as_deref()).unwrap().map(Result::unwrap).collect();
            assert_eq!(read.len(), 1);
            assert_eq!(read[0].digest(), groups[1].digest());
            assert_eq!(read[0].data(), groups[1].data());
            // The next small group opens a new segment.
            assert!(writer.append(&groups[2], 1_000).unwrap().is_empty());
            let last = writer.seal().unwrap().unwrap();
            assert_eq!(last.first_sequence(), groups[2].first_sequence());
            let mut all = segments(root.path());
            all.sort_by_key(SegmentInfo::first_sequence);
            assert_eq!(all.len(), 3);
            assert_contiguous(&all);
            for segment in &all {
                segment.verify_deep(key.as_deref()).unwrap();
            }
        }
    }

    fn candidate(index: u64, observed: i64, bytes: u64) -> PruneCandidate {
        PruneCandidate {
            history: Uuid::nil(),
            first_sequence: index * 10 + 1,
            last_sequence: index * 10 + 10,
            bytes,
            last_observed_at: observed,
            prunable: true,
        }
    }

    /// A filesystem whose free space grows by what is removed.
    struct Disk {
        free: Option<u64>,
        sizes: Vec<u64>,
        removed: Vec<usize>,
    }

    impl PruneTarget for Disk {
        fn free_bytes(&mut self) -> Option<u64> {
            self.free
        }

        fn remove(&mut self, index: usize) -> bool {
            if let Some(free) = self.free.as_mut() {
                *free += self.sizes[index];
            }
            self.removed.push(index);
            true
        }
    }

    fn run(
        candidates: &[PruneCandidate],
        policy: PrunePolicy,
        standbys: &[(Uuid, u64)],
        free: Option<u64>,
    ) -> Vec<(usize, PruneReason)> {
        let total = candidates.iter().map(|candidate| candidate.bytes).sum();
        let mut disk = Disk {
            free,
            sizes: candidates.iter().map(|candidate| candidate.bytes).collect(),
            removed: Vec::new(),
        };
        let decisions = prune(candidates, total, &policy, standbys, &mut disk);
        assert_eq!(
            disk.removed,
            decisions.iter().map(|(index, _)| *index).collect::<Vec<_>>()
        );
        decisions
    }

    const POLICY: PrunePolicy = PrunePolicy {
        now: 10_000,
        window_secs: 1_000,
        max_bytes: None,
        reserve_bytes: 0,
    };

    /// Spec P3.1 §4, the decision table.
    #[test]
    fn the_retention_decision_table() {
        use PruneReason::{DiskReserve as Reserve, Expired, OverMaxBytes};
        // Expired beyond the window, oldest first; inside it, kept.
        let segments = [
            candidate(0, 8_000, 10),
            candidate(1, 8_500, 10),
            candidate(2, 9_500, 10),
            candidate(3, 9_900, 10),
        ];
        assert_eq!(run(&segments, POLICY, &[], None), [(0, Expired), (1, Expired)]);

        // The newest sealed segment is never pruned, expired or not.
        let old = [candidate(0, 100, 10), candidate(1, 200, 10)];
        assert_eq!(run(&old, POLICY, &[], None), [(0, Expired)]);

        // A corrupt file is never pruned.
        let mut corrupt = [candidate(0, 100, 10), candidate(1, 200, 10), candidate(2, 9_900, 10)];
        corrupt[0].prunable = false;
        assert_eq!(run(&corrupt, POLICY, &[], None), [(1, Expired)]);

        // Over max_bytes: oldest first until under, inside the window.
        let inside = [
            candidate(0, 9_100, 40),
            candidate(1, 9_200, 40),
            candidate(2, 9_300, 40),
            candidate(3, 9_400, 40),
        ];
        let capped = PrunePolicy {
            max_bytes: Some(90),
            ..POLICY
        };
        assert_eq!(run(&inside, capped, &[], None), [(0, OverMaxBytes), (1, OverMaxBytes)]);

        // Under the reserve: oldest first, free space measured again each time.
        let reserved = PrunePolicy {
            reserve_bytes: 100,
            ..POLICY
        };
        assert_eq!(
            run(&inside, reserved, &[], Some(25)),
            [(0, Reserve), (1, Reserve)],
            "25 + 40 + 40 >= 100"
        );
        assert!(
            run(&inside, reserved, &[], None).is_empty(),
            "unknown free space is not full"
        );

        // A standby still needing a segment holds steps 2 and 3 back inside
        // the window...
        let standby = [(Uuid::nil(), 15)];
        assert_eq!(
            run(&inside, capped, &standby, None),
            [(0, OverMaxBytes)],
            "segment 0 (1..=10) is applied; segment 1 (11..=20) is still needed, and so is every later one"
        );
        // ...but not step 1 beyond it.
        assert_eq!(
            run(&segments, POLICY, &[(Uuid::nil(), 0)], None),
            [(0, Expired), (1, Expired)]
        );
        // A standby of another history needs nothing here.
        assert_eq!(
            run(&inside, capped, &[(Uuid::new_v4(), 0)], None),
            [(0, OverMaxBytes), (1, OverMaxBytes)]
        );
    }

    fn source(path: &Path) -> Arc<DB> {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_wal_ttl_seconds(3600);
        Arc::new(DB::open(&options, path).unwrap())
    }

    fn put_groups(store: &DB, range: std::ops::Range<u32>) {
        for index in range {
            let mut batch = WriteBatch::default();
            batch.put(format!("data:t:{index}"), b"row");
            store.write(batch).unwrap();
        }
    }

    fn settings(root: &Path) -> ArchiveSettings {
        ArchiveSettings {
            dir: root.to_path_buf(),
            limits: LIMITS,
            poll_interval: Duration::from_millis(10),
            recovery_window_secs: 7 * 86_400,
            max_bytes: None,
            disk_reserve: DiskReserve::Bytes(0),
            prune_interval: Duration::from_secs(60),
        }
    }

    /// A primary archiver thread over a test store.
    struct Running {
        status: Arc<ArchiveStatus>,
        stop: Arc<StopSignal>,
        thread: std::thread::JoinHandle<()>,
    }

    impl Running {
        fn start(store: &Arc<DB>, root: &Path, history: Uuid) -> Self {
            let settings = settings(root);
            let status = Arc::new(ArchiveStatus::new(
                settings.dir.clone(),
                settings.recovery_window_secs,
                None,
                settings.disk_reserve,
            ));
            let stop = Arc::new(StopSignal::default());
            let thread = std::thread::spawn({
                let (store, status, stop) = (Arc::clone(store), Arc::clone(&status), Arc::clone(&stop));
                move || run_primary(settings, store, history, Uuid::new_v4(), None, status, stop)
            });
            Self { status, stop, thread }
        }

        /// Wait until the archive holds `through`.
        fn wait(&self, through: u64) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while self.status.archived_through() < through {
                assert!(
                    Instant::now() < deadline,
                    "archived through {}",
                    self.status.archived_through()
                );
                assert!(
                    self.status.disabled_reason().is_none(),
                    "{:?}",
                    self.status.disabled_reason()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// Stop it (it seals its open segment first).
        fn stop(self) -> Arc<ArchiveStatus> {
            self.stop.stop();
            self.thread.join().unwrap();
            self.status
        }
    }

    /// Spec P3.1 §3: an empty archive covers the primary from "now"; a
    /// restart continues right after the archive's last sequence (no gap);
    /// a store that no longer holds what the archive lacks is a gap, named,
    /// and the archiver continues from what the store holds.
    #[test]
    fn a_primary_archiver_continues_after_its_last_segment_and_names_what_the_store_lost() {
        let root = TempDir::new().unwrap();
        let archive = root.path().join("archive");
        let store_path = root.path().join("store");
        let history = Uuid::new_v4();
        {
            let store = source(&store_path);
            put_groups(&store, 0..3);
            // Empty archive: it starts at the barrier (3); nothing before it.
            let running = Running::start(&store, &archive, history);
            running.wait(3);
            put_groups(&store, 3..6);
            running.wait(6);
            let status = running.stop();
            let archived = segments(&archive);
            assert_eq!(archived.len(), 1);
            assert_eq!((archived[0].first_sequence(), archived[0].last_sequence()), (4, 6));
            assert!(status.gaps().is_empty());
            // Written while it is stopped: a restart continues at 7, no gap.
            put_groups(&store, 6..8);
            let running = Running::start(&store, &archive, history);
            running.wait(8);
            let status = running.stop();
            let mut archived = segments(&archive);
            archived.sort_by_key(SegmentInfo::first_sequence);
            assert_eq!(archived.len(), 2);
            assert_contiguous(&archived);
            assert!(status.gaps().is_empty(), "{:?}", status.gaps());
            // Written while no archiver runs, then flushed out of the WAL.
            put_groups(&store, 8..10);
            store.flush().unwrap();
        }
        // What WAL retention does to the flushed WAL (as in physical.rs's
        // pruned-WAL test): every archived file and every log but the newest.
        let wal_archive = store_path.join("archive");
        if wal_archive.is_dir() {
            for entry in std::fs::read_dir(&wal_archive).unwrap() {
                std::fs::remove_file(entry.unwrap().path()).unwrap();
            }
        }
        let mut logs: Vec<PathBuf> = std::fs::read_dir(&store_path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
            .collect();
        logs.sort();
        logs.pop();
        for log in logs {
            std::fs::remove_file(log).unwrap();
        }
        let store = source(&store_path);
        put_groups(&store, 10..11);
        let running = Running::start(&store, &archive, history);
        running.wait(11);
        let status = running.stop();
        let gaps = status.gaps();
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!((gaps[0].from_sequence, gaps[0].to_sequence), (9, 10));
        assert_eq!(gaps[0].reason, "the store no longer holds it");
        let mut archived = segments(&archive);
        archived.sort_by_key(SegmentInfo::first_sequence);
        assert_eq!(archived.last().unwrap().last_sequence(), 11);
        for info in &archived {
            SegmentReader::open(&info.path).unwrap().verify_deep(None).unwrap();
        }
    }

    /// Spec P3.1 §5: one archiver per directory, and no offline prune under
    /// it; a dry run changes nothing.
    #[test]
    fn the_directory_lock_refuses_a_second_archiver_and_an_offline_prune() {
        let root = TempDir::new().unwrap();
        let (history, batches) = batches(6, 1);
        let mut writer = SegmentWriter::new(
            root.path(),
            history,
            1,
            Uuid::new_v4(),
            None,
            [0; 32],
            SegmentLimits { max_bytes: 1, ..LIMITS },
            None,
        );
        for batch in &batches {
            // Observed now: inside the 7-day window, so only max_bytes prunes.
            writer.append(batch, unix_now()).unwrap();
        }
        writer.seal().unwrap();
        assert_eq!(segments(root.path()).len(), 6);
        let policy = ArchiveSettings {
            max_bytes: Some(1),
            ..settings(root.path())
        };
        let held = ArchiveLock::acquire(root.path()).unwrap();
        let refused = prune_offline(&policy, false).unwrap_err().to_string();
        assert!(refused.contains("archive prune refused"), "{refused}");
        assert!(
            refused.contains(&std::process::id().to_string()),
            "names the holder: {refused}"
        );
        assert!(ArchiveLock::acquire(root.path()).is_err(), "one archiver per directory");
        let would = prune_offline(&policy, true).unwrap();
        assert_eq!(would.len(), 5, "all but the newest");
        assert!(would.iter().all(|pruned| pruned.reason == PruneReason::OverMaxBytes));
        assert_eq!(segments(root.path()).len(), 6, "a dry run changes nothing");
        drop(held);
        let pruned = prune_offline(&policy, false).unwrap();
        assert_eq!(pruned.len(), 5);
        let left = segments(root.path());
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].last_sequence(), batches[5].last_sequence());
    }

    /// Spec P3.1 §3: the standby's hand-off never waits: past
    /// `MAX_QUEUED_BYTES` behind, batches are left out (counted).
    #[test]
    fn a_standby_feed_never_waits_for_its_archiver() {
        let root = TempDir::new().unwrap();
        let store = source(&root.path().join("store"));
        let (sender, receiver) = mpsc::channel();
        let state = Arc::new(FeedState {
            queued_bytes: AtomicU64::new(MAX_QUEUED_BYTES - 10),
            dropped: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            sealed: false,
            key_check: OnceLock::new(),
        });
        let feed = ArchiveFeed {
            state: Arc::clone(&state),
            sender: Arc::new(Mutex::new(sender)),
        };
        let (_, batches) = batches(2, 1);
        assert!(!feed.offer(&batches[0], &store), "over the bound: left out");
        assert_eq!(feed.dropped(), 1);
        state.queued_bytes.store(0, Ordering::Release);
        assert!(feed.offer(&batches[0], &store));
        assert_eq!(
            receiver.try_recv().unwrap().first_sequence(),
            batches[0].first_sequence()
        );
        drop(receiver);
        assert!(!feed.offer(&batches[1], &store), "the archiver is gone");
        assert!(!feed.offer(&batches[1], &store));
    }
}
