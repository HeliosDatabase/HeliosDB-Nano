//! The archive catalog (spec P3.1 §5): what an archive directory holds, for
//! the archiver, the pruner, the `archive` CLI and the SQL views.
//!
//! # No catalog in RocksDB
//!
//! The plan (§D4) sketched `arch:seg:*` keys in the node-local column family.
//! They are not needed: segments are self-describing (header and footer name
//! their history, epoch, interval, counts, times and hashes) and their names
//! sort in sequence order, so the catalog is rebuilt by scanning the
//! directory — at start, by the CLI, on another host after an operator copied
//! the files — and kept in memory while the archiver runs. A primary has no
//! node-local column family to keep it in anyway (§D3.1), and a catalog that
//! could disagree with the files would be one more thing to repair.
//!
//! # States
//!
//! `available`: header and footer verify. `corrupt`: they do not (or an open
//! segment could not be repaired and was set aside); a corrupt file is
//! listed, counted, and never pruned — removing it is the operator's call.
//!
//! # Gaps
//!
//! Holes between consecutive available segments of one history and epoch,
//! the interval of a corrupt segment, and the intervals the archiver itself
//! recorded as missing (the store no longer held them when it looked).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard, RwLock};
use uuid::Uuid;

use super::archive_segment::{
    parse_partial_name, parse_segment_name, repair_partial, set_aside_corrupt, unix_now, SegmentInfo, SegmentReader,
    CORRUPT_SUFFIX, PARTIAL_SUFFIX, SEGMENT_SUFFIX,
};
use crate::config::DiskReserve;
use crate::crypto::KeyManager;

/// `internal-tests` builds only: the free bytes the archive reports and
/// prunes by, instead of `statvfs` — the disk-reserve path, testable without
/// filling a disk.
pub const TEST_FREE_BYTES_ENV: &str = "HELIOSDB_NANO_TEST_ARCHIVE_FREE_BYTES";

/// Free bytes on the archive's filesystem; `None` when they cannot be
/// measured (the pruner then never prunes for the reserve: unknown is not
/// full).
pub fn measured_free_bytes(dir: &Path) -> Option<u64> {
    #[cfg(feature = "internal-tests")]
    {
        if let Some(free) = std::env::var(TEST_FREE_BYTES_ENV)
            .ok()
            .and_then(|value| value.trim().parse().ok())
        {
            return Some(free);
        }
    }
    crate::storage::disk::free_bytes(dir).ok()
}

/// The disk reserve in bytes on `dir`'s filesystem (a percentage of a
/// filesystem whose size cannot be measured is 0).
pub fn reserve_bytes(reserve: DiskReserve, dir: &Path) -> u64 {
    match reserve {
        DiskReserve::Percent(_) => crate::storage::disk::total_bytes(dir).map_or(0, |total| reserve.bytes_of(total)),
        DiskReserve::Bytes(bytes) => bytes,
    }
}

/// Whether a listed file can be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryState {
    Available,
    Corrupt(String),
}

/// One file of the archive.
#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub path: PathBuf,
    pub file_bytes: u64,
    /// From the header when it verifies, else from the directory it is filed in.
    pub history: Option<Uuid>,
    pub epoch: Option<u64>,
    /// From the footer when it verifies, else from the file name.
    pub first_sequence: Option<u64>,
    pub last_sequence: Option<u64>,
    /// `Some` exactly when the entry is available.
    pub info: Option<SegmentInfo>,
    pub state: EntryState,
}

impl CatalogEntry {
    pub fn available(info: SegmentInfo) -> Self {
        Self {
            path: info.path.clone(),
            file_bytes: info.file_bytes,
            history: Some(info.history()),
            epoch: Some(info.epoch()),
            first_sequence: Some(info.first_sequence()),
            last_sequence: Some(info.last_sequence()),
            info: Some(info),
            state: EntryState::Available,
        }
    }

    fn corrupt(path: &Path, filed: Option<(Uuid, u64)>, reason: String) -> Self {
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        let named = parse_segment_name(name.strip_suffix(CORRUPT_SUFFIX).unwrap_or(name));
        Self {
            path: path.to_path_buf(),
            file_bytes: std::fs::metadata(path).map_or(0, |metadata| metadata.len()),
            history: filed.map(|(history, _)| history),
            epoch: filed.map(|(_, epoch)| epoch),
            first_sequence: named.map(|(first, _)| first),
            last_sequence: named.map(|(_, last)| last),
            info: None,
            state: EntryState::Corrupt(reason),
        }
    }

    pub fn is_available(&self) -> bool {
        self.state == EntryState::Available
    }

    /// `available` or `corrupt`.
    pub fn state_name(&self) -> &'static str {
        match self.state {
            EntryState::Available => "available",
            EntryState::Corrupt(_) => "corrupt",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match &self.state {
            EntryState::Available => None,
            EntryState::Corrupt(reason) => Some(reason),
        }
    }
}

/// A sequence interval the archive does not hold.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArchiveGap {
    pub history: Uuid,
    pub epoch: u64,
    pub from_sequence: u64,
    pub to_sequence: u64,
    pub reason: String,
}

/// What a scan did to the open segments it found.
#[derive(Debug, Default)]
pub struct ScanReport {
    /// Open segments repaired and sealed.
    pub repaired: Vec<SegmentInfo>,
    /// Open segments that held no complete frame, removed.
    pub removed: Vec<PathBuf>,
    /// Open segments that could not be repaired, set aside as `.corrupt`:
    /// (where they went, why).
    pub set_aside: Vec<(PathBuf, String)>,
}

/// How a scan treats open segments (`*.hseg.partial`).
#[derive(Clone, Copy)]
pub enum PartialPolicy<'k> {
    /// List them, touch nothing (the CLI: a daemon may be writing them).
    Leave,
    /// Repair and seal them (the archiver at start, which owns the directory).
    Repair(Option<&'k KeyManager>),
}

/// The files of one archive directory, keyed by path.
#[derive(Debug, Default)]
pub struct ArchiveCatalog {
    entries: BTreeMap<PathBuf, CatalogEntry>,
    open: Vec<PathBuf>,
    recorded: Vec<ArchiveGap>,
}

fn read_dir_sorted(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = std::fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    Ok(paths)
}

impl ArchiveCatalog {
    /// Scan `root`: `<root>/<history>/<epoch>/*` (the archiver's layout) and
    /// segment files directly in `root` (an operator's copy of segments from
    /// another host). Other files and directories are ignored. A segment
    /// filed under a history or epoch its header does not name is corrupt.
    pub fn scan(root: &Path, partials: PartialPolicy<'_>) -> std::io::Result<(Self, ScanReport)> {
        let mut catalog = Self::default();
        let mut report = ScanReport::default();
        for path in read_dir_sorted(root)? {
            if path.is_file() {
                catalog.consider(&path, None, partials, &mut report);
                continue;
            }
            let Some(history) = dir_name(&path).and_then(|name| Uuid::parse_str(name).ok()) else {
                continue;
            };
            for epoch_path in read_dir_sorted(&path)? {
                let Some(epoch) = dir_name(&epoch_path).and_then(|name| name.parse::<u64>().ok()) else {
                    continue;
                };
                if !epoch_path.is_dir() {
                    continue;
                }
                for file in read_dir_sorted(&epoch_path)? {
                    if file.is_file() {
                        catalog.consider(&file, Some((history, epoch)), partials, &mut report);
                    }
                }
            }
        }
        Ok((catalog, report))
    }

    fn consider(
        &mut self,
        path: &Path,
        filed: Option<(Uuid, u64)>,
        partials: PartialPolicy<'_>,
        report: &mut ScanReport,
    ) {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return;
        };
        if name.ends_with(SEGMENT_SUFFIX) {
            let entry = match SegmentReader::open_expecting(path, filed) {
                Ok(info) => CatalogEntry::available(info),
                Err(error) => CatalogEntry::corrupt(path, filed, error.reason),
            };
            self.entries.insert(path.to_path_buf(), entry);
        } else if name.ends_with(PARTIAL_SUFFIX) && parse_partial_name(name).is_some() {
            match partials {
                PartialPolicy::Leave => self.open.push(path.to_path_buf()),
                PartialPolicy::Repair(key) => match repair_partial(path, key) {
                    Ok(Some(info)) => {
                        report.repaired.push(info.clone());
                        self.insert(info);
                    }
                    Ok(None) => report.removed.push(path.to_path_buf()),
                    Err(error) => {
                        let moved = set_aside_corrupt(path).unwrap_or_else(|_| path.to_path_buf());
                        let reason = format!("an open segment that could not be repaired: {}", error.reason);
                        report.set_aside.push((moved.clone(), reason.clone()));
                        self.entries
                            .insert(moved.clone(), CatalogEntry::corrupt(&moved, filed, reason));
                    }
                },
            }
        } else if name.ends_with(CORRUPT_SUFFIX) {
            self.entries.insert(
                path.to_path_buf(),
                CatalogEntry::corrupt(
                    path,
                    filed,
                    "set aside after a failed repair or write; inspect it and remove it by hand".to_string(),
                ),
            );
        }
    }

    /// A segment the archiver just sealed.
    pub fn insert(&mut self, info: SegmentInfo) {
        self.entries.insert(info.path.clone(), CatalogEntry::available(info));
    }

    /// A file the pruner removed.
    pub fn remove(&mut self, path: &Path) -> Option<CatalogEntry> {
        self.entries.remove(path)
    }

    /// A corrupt file the archiver set aside while running.
    pub fn insert_corrupt(&mut self, path: &Path, filed: Option<(Uuid, u64)>, reason: String) {
        self.entries
            .insert(path.to_path_buf(), CatalogEntry::corrupt(path, filed, reason));
    }

    /// An interval the archiver knows it does not hold.
    pub fn record_gap(&mut self, gap: ArchiveGap) {
        if !self.recorded.contains(&gap) {
            self.recorded.push(gap);
        }
    }

    /// Every listed file, in path order (which is sequence order within one
    /// history and epoch).
    pub fn entries(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.entries.values()
    }

    /// Open segments a [`PartialPolicy::Leave`] scan saw.
    pub fn open_segments(&self) -> &[PathBuf] {
        &self.open
    }

    pub fn segment_count(&self) -> u64 {
        self.entries.len() as u64
    }

    /// Bytes of every listed file, corrupt ones included (they take the space).
    pub fn bytes_retained(&self) -> u64 {
        self.entries.values().map(|entry| entry.file_bytes).sum()
    }

    /// The last sequence the archive holds for `history` / `epoch`.
    pub fn last_sequence(&self, history: Uuid, epoch: u64) -> Option<u64> {
        self.available()
            .filter(|info| info.history() == history && info.epoch() == epoch)
            .map(SegmentInfo::last_sequence)
            .max()
    }

    fn available(&self) -> impl Iterator<Item = &SegmentInfo> {
        self.entries.values().filter_map(|entry| entry.info.as_ref())
    }

    /// When the oldest retained write group was observed.
    pub fn oldest_observed_at(&self) -> Option<i64> {
        self.available().map(SegmentInfo::first_observed_at).min()
    }

    /// When the newest segment was sealed (its file's last write; its last
    /// observation when that is unknown).
    pub fn newest_sealed_at(&self) -> Option<i64> {
        self.available()
            .map(|info| info.sealed_at.unwrap_or(info.last_observed_at()))
            .max()
    }

    /// The intervals the archive lacks: holes between consecutive available
    /// segments of one history and epoch, corrupt segments' intervals, and
    /// the gaps the archiver recorded (those that fell before the oldest
    /// retained segment were pruned with it and are not listed).
    pub fn gaps(&self) -> Vec<ArchiveGap> {
        let mut by_stream: BTreeMap<(Uuid, u64), Vec<&SegmentInfo>> = BTreeMap::new();
        for info in self.available() {
            by_stream.entry((info.history(), info.epoch())).or_default().push(info);
        }
        let mut gaps: Vec<ArchiveGap> = Vec::new();
        for ((history, epoch), segments) in &mut by_stream {
            segments.sort_by_key(|info| info.first_sequence());
            for pair in segments.windows(2) {
                let (Some(before), Some(after)) = (pair.first(), pair.get(1)) else {
                    continue;
                };
                let from = before.last_sequence().saturating_add(1);
                if after.first_sequence() > from {
                    gaps.push(ArchiveGap {
                        history: *history,
                        epoch: *epoch,
                        from_sequence: from,
                        to_sequence: after.first_sequence() - 1,
                        reason: "no segment holds it".to_string(),
                    });
                }
            }
        }
        for entry in self.entries.values() {
            if let (EntryState::Corrupt(reason), Some(history), Some(epoch), Some(first), Some(last)) = (
                &entry.state,
                entry.history,
                entry.epoch,
                entry.first_sequence,
                entry.last_sequence,
            ) {
                // A corrupt file whose interval an available segment holds
                // (a stray copy, a misfiled segment) leaves no hole.
                let covered = by_stream.get(&(history, epoch)).is_some_and(|segments| {
                    segments
                        .iter()
                        .any(|info| info.first_sequence() <= first && last <= info.last_sequence())
                });
                if covered {
                    continue;
                }
                let name = entry.path.file_name().map(|name| name.to_string_lossy().into_owned());
                let reason = format!("segment {} is corrupt: {reason}", name.unwrap_or_default());
                // The hole a corrupt file sits in is split around it: its own
                // interval is named for what it is.
                let mut around = Vec::new();
                gaps.retain(|gap| {
                    let holds = gap.history == history
                        && gap.epoch == epoch
                        && gap.from_sequence <= first
                        && last <= gap.to_sequence;
                    if holds {
                        if gap.from_sequence < first {
                            around.push(ArchiveGap {
                                to_sequence: first - 1,
                                ..gap.clone()
                            });
                        }
                        if last < gap.to_sequence {
                            around.push(ArchiveGap {
                                from_sequence: last.saturating_add(1),
                                ..gap.clone()
                            });
                        }
                    }
                    !holds
                });
                gaps.extend(around);
                gaps.push(ArchiveGap {
                    history,
                    epoch,
                    from_sequence: first,
                    to_sequence: last,
                    reason,
                });
            }
        }
        for recorded in &self.recorded {
            let oldest = by_stream
                .get(&(recorded.history, recorded.epoch))
                .and_then(|segments| segments.first())
                .map(|info| info.first_sequence());
            if oldest.is_some_and(|oldest| recorded.to_sequence < oldest) {
                continue;
            }
            match gaps.iter_mut().find(|gap| {
                gap.history == recorded.history
                    && gap.epoch == recorded.epoch
                    && gap.from_sequence == recorded.from_sequence
                    && gap.to_sequence == recorded.to_sequence
            }) {
                // The archiver knows why better than the scan does.
                Some(gap) => gap.reason.clone_from(&recorded.reason),
                None => gaps.push(recorded.clone()),
            }
        }
        gaps.sort();
        gaps
    }
}

fn dir_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|name| name.to_str())
}

/// What one node's archive is doing, for the views and `/health`: installed
/// process-wide by the archiver it belongs to (a server process is one node,
/// like `ha_state()`).
pub struct ArchiveStatus {
    dir: PathBuf,
    configured_window_secs: u64,
    max_bytes: Option<u64>,
    disk_reserve: DiskReserve,
    catalog: Mutex<ArchiveCatalog>,
    pruned_inside_window_total: AtomicU64,
    /// The last sequence the archive holds or has skipped (a gap), the open
    /// segment included.
    archived_through: AtomicU64,
    /// While `now - window <= until`, the window is shortened: segments
    /// observed up to `until` were pruned although the window still covered
    /// them.
    shortened_until: Mutex<Option<i64>>,
    disabled: Mutex<Option<String>>,
}

static INSTALLED: RwLock<Option<Arc<ArchiveStatus>>> = RwLock::new(None);

/// One row of `heliosdb_archived_wal`.
#[derive(Clone, Debug)]
pub struct ArchivedWalRow {
    pub history: Option<Uuid>,
    pub epoch: Option<u64>,
    pub first_sequence: Option<u64>,
    pub last_sequence: Option<u64>,
    pub batches: Option<u64>,
    pub bytes: u64,
    pub first_observed_at: Option<i64>,
    pub last_observed_at: Option<i64>,
    pub sealed_at: Option<i64>,
    pub state: &'static str,
    pub path: String,
    pub sha256: Option<String>,
}

/// The one row of `heliosdb_recovery_window`.
#[derive(Clone, Debug)]
pub struct RecoveryWindowRow {
    pub enabled: bool,
    pub archive_dir: Option<String>,
    pub configured_window_secs: u64,
    pub effective_window_secs: u64,
    pub oldest_retained_at: Option<i64>,
    pub newest_sealed_at: Option<i64>,
    pub segments: u64,
    pub bytes_retained: u64,
    pub max_bytes: Option<u64>,
    pub disk_reserve_bytes: u64,
    pub free_bytes: Option<u64>,
    pub pruned_inside_window_total: u64,
    pub shortened: bool,
    /// `ok`, `shortened`, `gap` or `disabled`.
    pub state: &'static str,
    /// Why the archive is disabled, when it is.
    pub reason: Option<String>,
}

impl RecoveryWindowRow {
    /// No archive on this node.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            archive_dir: None,
            configured_window_secs: 0,
            effective_window_secs: 0,
            oldest_retained_at: None,
            newest_sealed_at: None,
            segments: 0,
            bytes_retained: 0,
            max_bytes: None,
            disk_reserve_bytes: 0,
            free_bytes: None,
            pruned_inside_window_total: 0,
            shortened: false,
            state: "disabled",
            reason: None,
        }
    }
}

impl ArchiveStatus {
    pub fn new(dir: PathBuf, configured_window_secs: u64, max_bytes: Option<u64>, disk_reserve: DiskReserve) -> Self {
        Self {
            dir,
            configured_window_secs,
            max_bytes,
            disk_reserve,
            catalog: Mutex::new(ArchiveCatalog::default()),
            pruned_inside_window_total: AtomicU64::new(0),
            archived_through: AtomicU64::new(0),
            shortened_until: Mutex::new(None),
            disabled: Mutex::new(None),
        }
    }

    /// Make `status` this process's archive status.
    pub fn install(status: Arc<Self>) {
        *INSTALLED.write() = Some(status);
    }

    /// This process's archive status, when an archiver was started.
    pub fn installed() -> Option<Arc<Self>> {
        INSTALLED.read().clone()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn configured_window_secs(&self) -> u64 {
        self.configured_window_secs
    }

    /// The archiver gave up (it never stops the node): views and `/health`
    /// report the archive disabled, with this reason.
    pub fn disable(&self, reason: String) {
        *self.disabled.lock() = Some(reason);
    }

    pub fn disabled_reason(&self) -> Option<String> {
        self.disabled.lock().clone()
    }

    pub fn catalog(&self) -> MutexGuard<'_, ArchiveCatalog> {
        self.catalog.lock()
    }

    pub fn replace_catalog(&self, catalog: ArchiveCatalog) {
        *self.catalog.lock() = catalog;
    }

    pub fn pruned_inside_window_total(&self) -> u64 {
        self.pruned_inside_window_total.load(Ordering::Acquire)
    }

    /// The archiver's position: everything up to this sequence is archived
    /// (sealed or in the open segment) or recorded as a gap.
    pub fn note_archived_through(&self, sequence: u64) {
        self.archived_through.store(sequence, Ordering::Release);
    }

    pub fn archived_through(&self) -> u64 {
        self.archived_through.load(Ordering::Acquire)
    }

    /// The pruner removed `count` segments inside the window, the newest of
    /// them observed at `until`. Returns whether this opens a new shortening
    /// episode (the window was not shortened before).
    pub fn note_pruned_inside_window(&self, count: u64, until: i64, now: i64) -> bool {
        self.pruned_inside_window_total.fetch_add(count, Ordering::AcqRel);
        let mut shortened = self.shortened_until.lock();
        let was = self.shortened_locked(*shortened, now);
        *shortened = Some(shortened.map_or(until, |previous| previous.max(until)));
        !was
    }

    /// Close a shortening episode that has ended — everything pruned inside
    /// the window would have expired by `now` — returning true exactly once
    /// for it (the caller logs the restored window).
    pub fn close_restored_episode(&self, now: i64) -> bool {
        let mut shortened = self.shortened_until.lock();
        if shortened.is_some() && !self.shortened_locked(*shortened, now) {
            *shortened = None;
            return true;
        }
        false
    }

    fn shortened_locked(&self, until: Option<i64>, now: i64) -> bool {
        let horizon = now.saturating_sub(i64::try_from(self.configured_window_secs).unwrap_or(i64::MAX));
        until.is_some_and(|until| horizon <= until)
    }

    pub fn shortened(&self, now: i64) -> bool {
        self.shortened_locked(*self.shortened_until.lock(), now)
    }

    /// `heliosdb_archived_wal`: one row per listed file.
    pub fn archived_wal_rows(&self) -> Vec<ArchivedWalRow> {
        self.catalog()
            .entries()
            .map(|entry| {
                let info = entry.info.as_ref();
                ArchivedWalRow {
                    history: entry.history,
                    epoch: entry.epoch,
                    first_sequence: entry.first_sequence,
                    last_sequence: entry.last_sequence,
                    batches: info.map(SegmentInfo::batch_count),
                    bytes: entry.file_bytes,
                    first_observed_at: info.map(SegmentInfo::first_observed_at),
                    last_observed_at: info.map(SegmentInfo::last_observed_at),
                    sealed_at: info.and_then(|info| info.sealed_at),
                    state: entry.state_name(),
                    path: entry.path.display().to_string(),
                    sha256: info.map(SegmentInfo::sha256_hex),
                }
            })
            .collect()
    }

    pub fn gaps(&self) -> Vec<ArchiveGap> {
        self.catalog().gaps()
    }

    /// `heliosdb_recovery_window` at `now`.
    pub fn recovery_window_row(&self, now: i64) -> RecoveryWindowRow {
        let disabled = self.disabled_reason();
        let (segments, bytes_retained, oldest, newest, gaps) = {
            let catalog = self.catalog();
            (
                catalog.segment_count(),
                catalog.bytes_retained(),
                catalog.oldest_observed_at(),
                catalog.newest_sealed_at(),
                catalog.gaps().len(),
            )
        };
        let shortened = self.shortened(now);
        let state = if disabled.is_some() {
            "disabled"
        } else if shortened {
            "shortened"
        } else if gaps > 0 {
            "gap"
        } else {
            "ok"
        };
        RecoveryWindowRow {
            enabled: disabled.is_none(),
            archive_dir: Some(self.dir.display().to_string()),
            configured_window_secs: self.configured_window_secs,
            effective_window_secs: oldest.map_or(0, |oldest| u64::try_from(now.saturating_sub(oldest)).unwrap_or(0)),
            oldest_retained_at: oldest,
            newest_sealed_at: newest,
            segments,
            bytes_retained,
            max_bytes: self.max_bytes,
            disk_reserve_bytes: reserve_bytes(self.disk_reserve, &self.dir),
            free_bytes: measured_free_bytes(&self.dir),
            pruned_inside_window_total: self.pruned_inside_window_total(),
            shortened,
            state,
            reason: disabled,
        }
    }
}

/// `heliosdb_archived_wal` of this process (empty without an archive).
pub fn archived_wal_rows() -> Vec<ArchivedWalRow> {
    ArchiveStatus::installed().map_or_else(Vec::new, |status| status.archived_wal_rows())
}

/// `heliosdb_archive_gaps` of this process (empty without an archive).
pub fn archive_gap_rows() -> Vec<ArchiveGap> {
    ArchiveStatus::installed().map_or_else(Vec::new, |status| status.gaps())
}

/// `heliosdb_recovery_window` of this process (`disabled` without an archive).
pub fn recovery_window_row() -> RecoveryWindowRow {
    ArchiveStatus::installed().map_or_else(RecoveryWindowRow::disabled, |status| {
        status.recovery_window_row(unix_now())
    })
}

/// `/health`'s `archive` object (spec P3.1 §5): a sibling of the frozen
/// `replication` block, never inside it.
pub fn health_json() -> serde_json::Value {
    let row = recovery_window_row();
    let gaps = archive_gap_rows().len();
    serde_json::json!({
        "enabled": row.enabled,
        "segments": row.segments,
        "bytes_retained": row.bytes_retained,
        "effective_window_secs": row.effective_window_secs,
        "shortened": row.shortened,
        "gaps": gaps,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::replication::archive_segment::{
        partial_file_name, segment_dir, tests::batches, unix_now, OpenSegment, SegmentHeader,
    };
    use crate::replication::physical::PhysicalBatch;
    use tempfile::TempDir;

    fn write(dir: &Path, batches: &[PhysicalBatch]) -> SegmentInfo {
        let header = SegmentHeader {
            flags: 0,
            history: batches[0].history(),
            epoch: 1,
            first_sequence: batches[0].first_sequence(),
            created_at: unix_now(),
            node_id: Uuid::new_v4(),
            key_check: [0; 32],
        };
        let mut open = OpenSegment::create(dir, header).unwrap();
        for batch in batches {
            open.append(batch, unix_now(), None).unwrap();
        }
        open.seal().unwrap()
    }

    /// Spec P3.1 §5: a scan lists every segment, a corrupt one as corrupt
    /// (never dropped), finds the hole a missing segment leaves, and repairs
    /// an open segment only when it owns the directory.
    #[test]
    fn a_scan_lists_segments_flags_corrupt_ones_and_finds_holes() {
        let root = TempDir::new().unwrap();
        let (history, batches) = batches(8, 1);
        let dir = segment_dir(root.path(), history, 1);
        let first = write(&dir, &batches[0..2]);
        let missing = write(&dir, &batches[2..4]);
        let corrupt = write(&dir, &batches[4..6]);
        std::fs::remove_file(&missing.path).unwrap();
        let mut bytes = std::fs::read(&corrupt.path).unwrap();
        bytes[50] ^= 1;
        std::fs::write(&corrupt.path, bytes).unwrap();
        // An open segment holding the last two groups.
        let mut open = OpenSegment::create(
            &dir,
            SegmentHeader {
                flags: 0,
                history,
                epoch: 1,
                first_sequence: batches[6].first_sequence(),
                created_at: unix_now(),
                node_id: Uuid::new_v4(),
                key_check: [0; 32],
            },
        )
        .unwrap();
        open.append(&batches[6], unix_now(), None).unwrap();
        open.append(&batches[7], unix_now(), None).unwrap();
        drop(open);
        // A file that is not the archive's is ignored.
        std::fs::write(root.path().join("archiver.lock"), "1\n").unwrap();

        let (listed, report) = ArchiveCatalog::scan(root.path(), PartialPolicy::Leave).unwrap();
        assert!(report.repaired.is_empty());
        assert_eq!(listed.open_segments().len(), 1);
        assert_eq!(listed.segment_count(), 2);
        let states: Vec<&str> = listed.entries().map(CatalogEntry::state_name).collect();
        assert_eq!(states, ["available", "corrupt"]);
        let reason = listed.entries().nth(1).unwrap().reason().unwrap().to_string();
        assert!(reason.contains("header hash"), "{reason}");

        let (repaired, report) = ArchiveCatalog::scan(root.path(), PartialPolicy::Repair(None)).unwrap();
        assert_eq!(report.repaired.len(), 1);
        assert!(!dir.join(partial_file_name(batches[6].first_sequence())).exists());
        assert_eq!(repaired.last_sequence(history, 1), Some(batches[7].last_sequence()));
        let gaps = repaired.gaps();
        // The removed segment's interval, and the corrupt one's.
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        assert_eq!(
            (gaps[0].from_sequence, gaps[0].to_sequence),
            (batches[2].first_sequence(), batches[3].last_sequence())
        );
        assert!(gaps[0].reason.contains("no segment holds it"));
        assert_eq!(
            (gaps[1].from_sequence, gaps[1].to_sequence),
            (batches[4].first_sequence(), batches[5].last_sequence())
        );
        assert!(gaps[1].reason.contains("is corrupt"));
        assert_eq!(repaired.bytes_retained(), {
            let entries: Vec<u64> = repaired.entries().map(|entry| entry.file_bytes).collect();
            entries.iter().sum::<u64>()
        });
        assert!(first.path.exists());
    }

    /// A segment filed under another history is corrupt, by name.
    #[test]
    fn a_segment_filed_under_another_history_is_refused() {
        let root = TempDir::new().unwrap();
        let (_, ours) = batches(2, 1);
        let (other, theirs) = batches(3, 1);
        let ours_dir = segment_dir(root.path(), ours[0].history(), 1);
        write(&ours_dir, &ours);
        let stray = write(&segment_dir(root.path(), other, 1), &theirs);
        let misfiled = ours_dir.join(stray.path.file_name().unwrap());
        assert!(!misfiled.exists(), "another interval: no name collision");
        std::fs::rename(&stray.path, &misfiled).unwrap();
        let (catalog, _) = ArchiveCatalog::scan(root.path(), PartialPolicy::Leave).unwrap();
        let refused: Vec<&CatalogEntry> = catalog.entries().filter(|entry| !entry.is_available()).collect();
        assert_eq!(refused.len(), 1);
        let reason = refused[0].reason().unwrap();
        assert!(reason.contains("other history"), "{reason}");
        assert_eq!(refused[0].history, Some(ours[0].history()), "filed under ours");
    }

    /// A gap the archiver recorded is listed with its reason until the
    /// segments around it are pruned.
    #[test]
    fn recorded_gaps_carry_their_reason_until_pruned() {
        let root = TempDir::new().unwrap();
        let (history, batches) = batches(4, 1);
        let dir = segment_dir(root.path(), history, 1);
        let mut catalog = ArchiveCatalog::default();
        let older = write(&dir, &batches[0..1]);
        catalog.insert(older.clone());
        catalog.insert(write(&dir, &batches[3..4]));
        catalog.record_gap(ArchiveGap {
            history,
            epoch: 1,
            from_sequence: batches[1].first_sequence(),
            to_sequence: batches[2].last_sequence(),
            reason: "the store no longer holds it".to_string(),
        });
        let gaps = catalog.gaps();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].reason, "the store no longer holds it");
        catalog.remove(&older.path);
        assert!(catalog.gaps().is_empty(), "pruned with the segment before it");
    }

    /// The shortening episode: opened once, kept while the pruned segments
    /// would still be inside the window, closed once.
    #[test]
    fn a_shortening_episode_opens_once_and_closes_once() {
        let status = ArchiveStatus::new(PathBuf::from("/nonexistent"), 100, None, DiskReserve::Percent(10));
        assert!(!status.shortened(1_000));
        assert!(status.note_pruned_inside_window(2, 950, 1_000), "a new episode");
        assert!(!status.note_pruned_inside_window(1, 960, 1_010), "the same episode");
        assert_eq!(status.pruned_inside_window_total(), 3);
        assert!(status.shortened(1_050));
        assert!(!status.close_restored_episode(1_050));
        assert!(status.close_restored_episode(1_061), "960 + 100 has passed");
        assert!(!status.close_restored_episode(1_062));
        assert!(!status.shortened(1_062));
        assert!(
            status.note_pruned_inside_window(1, 1_100, 1_150),
            "a later episode opens again"
        );
        let row = status.recovery_window_row(1_150);
        assert_eq!(row.state, "shortened");
        assert!(row.shortened && row.enabled);
        status.disable("test".to_string());
        assert_eq!(status.recovery_window_row(1_150).state, "disabled");
    }
}
