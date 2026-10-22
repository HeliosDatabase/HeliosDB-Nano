//! Reading archived WAL back (spec P3.2): the segments P3.1 seals become a
//! source of write groups again.
//!
//! # Two readers, one rule
//!
//! * [`ArchiveTail`] serves a receiver's gap on the replication connection:
//!   when RocksDB's own WAL no longer holds the groups a reconnecting standby
//!   needs, the primary (or a cascading standby) reads them from its archive
//!   instead — the receiver sees ordinary `PollStart/Batch/PollEnd` frames
//!   (plan §D6, Oracle's FAL: fetch archive log).
//! * [`RecoveryChain`] gathers segments an operator copied into one or more
//!   directories — from any node of the same dataset history — into the one
//!   contiguous run that continues a standby's cursor (`heliosdb-nano
//!   recover`, the archive-only standby).
//!
//! Both read only what a segment proves about itself: its header and footer
//! (verified when it is listed), and every frame's own digest (verified when
//! it is read). A frame is one write group of the primary, numbered with the
//! primary's sequences, so the archives of every node of one history hold the
//! same groups at the same boundaries — whatever their segment boundaries —
//! and a chain may switch from one node's segments to another's at any group.
//! Only the current history and epoch are ever read (P5 makes the epoch real).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;

use super::archive_catalog::{ArchiveCatalog, ArchiveStatus, PartialPolicy};
use super::archive_segment::{SegmentError, SegmentInfo, CORRUPT_SUFFIX};
use super::physical::PhysicalBatch;
use crate::crypto::KeyManager;

/// The available segments of `catalog` that belong to `history` / `epoch`,
/// in sequence order (the longest first among segments starting together).
fn segments_of(catalog: &ArchiveCatalog, history: Uuid, epoch: u64) -> Vec<SegmentInfo> {
    let mut segments: Vec<SegmentInfo> = catalog
        .entries()
        .filter_map(|entry| entry.info.as_ref())
        .filter(|info| info.history() == history && info.epoch() == epoch)
        .cloned()
        .collect();
    sort_segments(&mut segments);
    segments
}

fn sort_segments(segments: &mut [SegmentInfo]) {
    segments.sort_by_key(|info| (info.first_sequence(), std::cmp::Reverse(info.last_sequence())));
}

/// Of `segments`, the one holding `sequence` that reaches furthest.
fn holding(segments: &[SegmentInfo], sequence: u64) -> Option<&SegmentInfo> {
    segments
        .iter()
        .filter(|info| info.first_sequence() <= sequence && sequence <= info.last_sequence())
        .max_by_key(|info| info.last_sequence())
}

/// What an archive holds for one history and epoch, as a source reads it.
#[derive(Clone, Debug, Default)]
pub struct ArchiveView {
    /// Sealed, available segments, in sequence order.
    pub segments: Vec<SegmentInfo>,
    /// Everything up to this sequence is archived, the segment still being
    /// written included (0 when unknown). What lies past the sealed segments
    /// is readable once its segment is sealed.
    pub archived_through: u64,
    /// Intervals the archive knows it lacks (holes between segments, corrupt
    /// segments, gaps its archiver recorded): no later seal fills them.
    pub gaps: Vec<(u64, u64)>,
    /// Why this node's archive cannot serve anything, when it cannot.
    pub unavailable: Option<String>,
}

type ViewSource = Arc<dyn Fn(Uuid, u64) -> ArchiveView + Send + Sync>;

/// Where a replication source finds the archived segments it serves a
/// receiver's gap from (spec P3.2 §1), and the dataset key that opens a
/// sealed one's frames. Read anew at every use: segments are sealed and
/// pruned while a session lives.
#[derive(Clone)]
pub struct ArchiveFill {
    view: ViewSource,
    key: Option<Arc<KeyManager>>,
}

impl std::fmt::Debug for ArchiveFill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveFill")
            .field("sealed_frames", &self.key.is_some())
            .finish_non_exhaustive()
    }
}

/// The view of `catalog` for `history` / `epoch`.
fn view_of(catalog: &ArchiveCatalog, history: Uuid, epoch: u64) -> ArchiveView {
    ArchiveView {
        segments: segments_of(catalog, history, epoch),
        archived_through: 0,
        gaps: catalog
            .gaps()
            .into_iter()
            .filter(|gap| gap.history == history && gap.epoch == epoch)
            .map(|gap| (gap.from_sequence, gap.to_sequence))
            .collect(),
        unavailable: None,
    }
}

impl ArchiveFill {
    /// This node's own archive: the catalog its archiver keeps in memory
    /// (`ArchiveStatus::installed`), so a poll reads no directory listing.
    pub fn installed(key: Option<Arc<KeyManager>>) -> Self {
        let view: ViewSource = Arc::new(|history: Uuid, epoch: u64| match ArchiveStatus::installed() {
            Some(status) => {
                let mut view = view_of(&status.catalog(), history, epoch);
                view.archived_through = status.archived_through();
                view.unavailable = status.disabled_reason();
                view
            }
            None => ArchiveView {
                unavailable: Some("this node runs no WAL archive".to_string()),
                ..ArchiveView::default()
            },
        });
        Self { view, key }
    }

    /// The segments of `dir`, scanned at every use (tests and tools; a scan
    /// reads every segment's header and footer).
    pub fn directory(dir: PathBuf, key: Option<Arc<KeyManager>>) -> Self {
        let view: ViewSource =
            Arc::new(
                move |history: Uuid, epoch: u64| match ArchiveCatalog::scan(&dir, PartialPolicy::Leave) {
                    Ok((catalog, _)) => view_of(&catalog, history, epoch),
                    Err(error) => ArchiveView {
                        unavailable: Some(format!("cannot read the archive directory {}: {error}", dir.display())),
                        ..ArchiveView::default()
                    },
                },
            );
        Self { view, key }
    }

    /// The archive as it is now, for `history` / `epoch`.
    pub fn tail(&self, history: Uuid, epoch: u64) -> ArchiveTail {
        ArchiveTail {
            view: (self.view)(history, epoch),
            key: self.key.clone(),
        }
    }
}

/// Why a tail read stopped.
#[derive(Debug, thiserror::Error)]
pub enum TailError {
    /// A segment (or one of its frames) failed its verification.
    #[error(transparent)]
    Segment(#[from] SegmentError),
    /// The receiver's cursor is not a group boundary of the archive.
    #[error(
        "segment {}: no write group starts at sequence {expected} (the group there starts at {found})",
        .path.display()
    )]
    Boundary { path: PathBuf, expected: u64, found: u64 },
    /// One write group is larger than a poll may carry.
    #[error("a write group of {bytes} bytes exceeds the poll byte limit of {limit}")]
    Oversized { bytes: usize, limit: usize },
}

/// One history's archived segments as a source serves a gap from them
/// ([`ArchiveFill::tail`]).
pub struct ArchiveTail {
    view: ArchiveView,
    key: Option<Arc<KeyManager>>,
}

impl ArchiveTail {
    /// A tail over known segments (tests and tools).
    pub fn from_segments(mut segments: Vec<SegmentInfo>, key: Option<Arc<KeyManager>>) -> Self {
        sort_segments(&mut segments);
        Self {
            view: ArchiveView {
                segments,
                ..ArchiveView::default()
            },
            key,
        }
    }

    /// The oldest sequence the archive holds (its floor), if any.
    pub fn floor(&self) -> Option<u64> {
        self.view.segments.first().map(SegmentInfo::first_sequence)
    }

    /// Everything up to this sequence is archived, the open segment included
    /// (0 when unknown).
    pub fn archived_through(&self) -> u64 {
        self.view.archived_through
    }

    /// Why the archive cannot serve anything, when it cannot.
    pub fn unavailable(&self) -> Option<&str> {
        self.view.unavailable.as_deref()
    }

    /// The first sequence of the earliest segment that starts after
    /// `sequence`: where the archive resumes past a hole.
    pub fn next_first_after(&self, sequence: u64) -> Option<u64> {
        self.view
            .segments
            .iter()
            .map(SegmentInfo::first_sequence)
            .find(|first| *first > sequence)
    }

    /// Whether the archive knows it lacks part of `from..=to` (a hole, a
    /// corrupt segment, a gap its archiver recorded): no later seal fills it.
    pub fn records_missing(&self, from: u64, to: u64) -> bool {
        self.view
            .gaps
            .iter()
            .any(|(gap_from, gap_to)| *gap_from <= to && from <= *gap_to)
    }

    /// How far the archive reaches without a hole from `from`: the last
    /// sequence of the unbroken run of segments that starts with one holding
    /// `from`; `None` when no segment holds it.
    pub fn reach(&self, from: u64) -> Option<u64> {
        let mut reached = None;
        let mut next = from;
        while let Some(info) = holding(&self.view.segments, next) {
            reached = Some(info.last_sequence());
            match info.last_sequence().checked_add(1) {
                Some(after) => next = after,
                None => break,
            }
        }
        reached
    }

    /// The write groups after `after`, in order and contiguous, as a source
    /// serves them: at most `byte_limit` bytes of raw groups (a single group
    /// larger than that is refused, as `poll_source` refuses it), none that
    /// starts after `until`, and only as far as the archive reaches without a
    /// hole. Every frame is verified as it is read (digest, history, first
    /// sequence), and the first one must start right after `after`.
    pub fn read(&self, after: u64, byte_limit: usize, until: u64) -> Result<Vec<PhysicalBatch>, TailError> {
        let mut batches = Vec::new();
        let mut bytes = 0_usize;
        let mut next = after.saturating_add(1);
        'segments: while next <= until {
            let Some(segment) = holding(&self.view.segments, next) else {
                break;
            };
            let before = next;
            for frame in segment.frames(self.key.as_deref())? {
                let batch = frame?;
                if batch.last_sequence() < next {
                    continue;
                }
                if batch.first_sequence() != next {
                    return Err(TailError::Boundary {
                        path: segment.path.clone(),
                        expected: next,
                        found: batch.first_sequence(),
                    });
                }
                let size = batch.data().len();
                if size > byte_limit {
                    return Err(TailError::Oversized {
                        bytes: size,
                        limit: byte_limit,
                    });
                }
                if bytes + size > byte_limit {
                    break 'segments;
                }
                bytes += size;
                next = batch.last_sequence().saturating_add(1);
                batches.push(batch);
                if next > until {
                    break 'segments;
                }
            }
            // A segment whose frames ended short of what its footer claims
            // was refused by its frame walk; never loop on one.
            if next == before {
                break;
            }
        }
        Ok(batches)
    }
}

/// The segments that continue a standby's cursor, gathered from one or more
/// directories (spec P3.2 §2-§3).
#[derive(Debug, Default)]
pub struct RecoveryChain {
    /// The segments to apply, in order; each holds the sequence after the
    /// previous one's last.
    pub segments: Vec<SegmentInfo>,
    /// Every usable segment found (this history and epoch, duplicates
    /// removed), in sequence order: the chain is drawn from these.
    pub found: Vec<SegmentInfo>,
    /// The last sequence the chain reaches (the cursor when it is empty).
    pub reached: u64,
    /// The newest sequence a usable segment holds.
    pub newest: Option<u64>,
    /// The first interval the chain cannot cover: up to the next segment
    /// found past it, or up to `through`.
    pub missing: Option<(u64, u64)>,
    /// Files the chain may not be built around, each named with its reason:
    /// a segment that does not verify, or one of another dataset placed
    /// among this one's.
    pub refused: Vec<String>,
    /// What was passed over, and why: duplicates, open segments, files set
    /// aside by an archiver, other datasets filed apart.
    pub notes: Vec<String>,
}

impl RecoveryChain {
    /// When the segment holding `sequence` was last observed (the archiver's
    /// clock, unix seconds): how current a standby at `sequence` is.
    pub fn observed_at(&self, sequence: u64) -> Option<i64> {
        holding(&self.found, sequence).map(SegmentInfo::last_observed_at)
    }
}

/// Gather the chain that continues `cursor` (up to `through`, else as far as
/// the segments go) from `roots`, for `history` / `epoch`.
///
/// Segments are matched by their own header and footer — history, epoch,
/// first and last sequence — wherever they were found: in an archiver's
/// `<history>/<epoch>/` layout or loose in a directory an operator copied
/// them into. Byte-identical copies (same SHA-256) count once. Overlapping
/// segments from different nodes are fine: the chain takes, at each step, the
/// segment that holds the next sequence and reaches furthest. A segment that
/// does not verify, or one of another dataset found loose or filed under this
/// one, is REFUSED (named): the caller applies nothing then. Segments of other
/// datasets filed under their own history directory, open segments and files
/// an archiver set aside as corrupt are passed over (noted).
pub fn build_chain(
    roots: &[PathBuf],
    history: Uuid,
    epoch: u64,
    cursor: u64,
    through: Option<u64>,
) -> std::io::Result<RecoveryChain> {
    let mut chain = RecoveryChain {
        reached: cursor,
        ..RecoveryChain::default()
    };
    let mut hashes: HashSet<[u8; 32]> = HashSet::new();
    for root in roots {
        let (catalog, _) = ArchiveCatalog::scan(root, PartialPolicy::Leave).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("cannot read the archive directory {}: {error}", root.display()),
            )
        })?;
        for entry in catalog.entries() {
            let path = entry.path.display();
            let loose = entry.path.parent() == Some(root.as_path());
            match &entry.info {
                Some(info) if info.history() == history && info.epoch() == epoch => {
                    if hashes.insert(info.footer.file_sha256) {
                        chain.found.push(info.clone());
                    } else {
                        chain.notes.push(format!(
                            "segment {path}: a copy of a segment already found (same SHA-256)"
                        ));
                    }
                }
                Some(info) if loose => chain.refused.push(format!(
                    "segment {path}: other dataset: it belongs to history {} epoch {}, this standby follows \
                     history {history} epoch {epoch}",
                    info.history(),
                    info.epoch()
                )),
                Some(info) => chain.notes.push(format!(
                    "segment {path}: skipped: it belongs to history {} epoch {}",
                    info.history(),
                    info.epoch()
                )),
                None => {
                    let set_aside = entry.path.to_string_lossy().ends_with(CORRUPT_SUFFIX);
                    let elsewhere = entry.history.is_some_and(|filed| filed != history)
                        || entry.epoch.is_some_and(|filed| filed != epoch);
                    let reason = entry.reason().unwrap_or("unreadable");
                    if set_aside {
                        chain.notes.push(format!(
                            "segment {path}: skipped: an archiver set it aside as corrupt ({reason})"
                        ));
                    } else if elsewhere {
                        chain.notes.push(format!(
                            "segment {path}: skipped: another dataset's, and corrupt ({reason})"
                        ));
                    } else {
                        chain.refused.push(format!("segment {path}: {reason}"));
                    }
                }
            }
        }
        for open in catalog.open_segments() {
            chain.notes.push(format!(
                "segment {}: skipped: an open segment, not sealed yet",
                open.display()
            ));
        }
    }
    sort_segments(&mut chain.found);
    chain.newest = chain.found.iter().map(SegmentInfo::last_sequence).max();
    let mut next = cursor.saturating_add(1);
    while through.is_none_or(|through| next <= through) {
        let Some(info) = holding(&chain.found, next) else {
            break;
        };
        chain.segments.push(info.clone());
        chain.reached = info.last_sequence();
        match info.last_sequence().checked_add(1) {
            Some(after) => next = after,
            None => break,
        }
    }
    let target = through.unwrap_or_else(|| chain.newest.unwrap_or(chain.reached));
    if chain.reached < target {
        let from = chain.reached + 1;
        let to = chain
            .found
            .iter()
            .map(SegmentInfo::first_sequence)
            .filter(|first| *first > from)
            .min()
            .map_or(target, |first| (first - 1).min(target));
        chain.missing = Some((from, to));
    }
    Ok(chain)
}

/// Verify a chain before anything of it is applied (spec P3.2 §2): every
/// segment deep — the whole file against its hash, every frame's digest,
/// contiguity, history — and that a write group starts exactly where the
/// chain needs it to (`cursor + 1` for the first segment, the previous
/// segment's last + 1 for the others). The first failure is returned, naming
/// its file.
pub fn verify_chain(chain: &RecoveryChain, cursor: u64, key: Option<&KeyManager>) -> Result<(), String> {
    let mut next = cursor.saturating_add(1);
    for info in &chain.segments {
        verify_segment_from(info, next, key)?;
        next = info.last_sequence().saturating_add(1);
    }
    Ok(())
}

/// [`verify_chain`] for one segment the chain needs from sequence `next` on.
pub fn verify_segment_from(info: &SegmentInfo, next: u64, key: Option<&KeyManager>) -> Result<(), String> {
    info.verify_deep(key).map_err(|error| error.to_string())?;
    let frames = info.frames(key).map_err(|error| error.to_string())?;
    for frame in frames {
        let batch = frame.map_err(|error| error.to_string())?;
        if batch.last_sequence() < next {
            continue;
        }
        if batch.first_sequence() != next {
            return Err(TailError::Boundary {
                path: info.path.clone(),
                expected: next,
                found: batch.first_sequence(),
            }
            .to_string());
        }
        return Ok(());
    }
    Err(format!(
        "segment {}: it holds no write group at sequence {next}",
        info.path.display()
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::replication::archive_segment::{segment_dir, tests::batches, unix_now, OpenSegment, SegmentHeader};
    use std::path::Path;
    use tempfile::TempDir;

    /// Seal `batches` into one segment of `dir` (written by `node`).
    fn seal(dir: &Path, batches: &[PhysicalBatch], node: Uuid) -> SegmentInfo {
        let header = SegmentHeader {
            flags: 0,
            history: batches[0].history(),
            epoch: 1,
            first_sequence: batches[0].first_sequence(),
            created_at: unix_now(),
            node_id: node,
            key_check: [0; 32],
        };
        let mut open = OpenSegment::create(dir, header).unwrap();
        for batch in batches {
            open.append(batch, unix_now(), None).unwrap();
        }
        open.seal().unwrap()
    }

    fn sequences(batches: &[PhysicalBatch]) -> Vec<(u64, u64)> {
        batches
            .iter()
            .map(|batch| (batch.first_sequence(), batch.last_sequence()))
            .collect()
    }

    /// Spec P3.2 §5: a tail serves a contiguous run of groups after the
    /// cursor, across segments, and stops at the WAL floor (`until`) and at
    /// the byte limit; a cursor inside a group is refused by name.
    #[test]
    fn a_tail_serves_a_chain_and_stops_at_the_wal_floor() {
        let root = TempDir::new().unwrap();
        let (history, groups) = batches(9, 2);
        let dir = segment_dir(root.path(), history, 1);
        let node = Uuid::new_v4();
        seal(&dir, &groups[0..3], node);
        seal(&dir, &groups[3..6], node);
        seal(&dir, &groups[6..9], node);
        let tail = ArchiveFill::directory(root.path().to_path_buf(), None).tail(history, 1);
        assert_eq!(tail.floor(), Some(groups[0].first_sequence()));
        assert_eq!(tail.reach(groups[1].first_sequence()), Some(groups[8].last_sequence()));
        assert_eq!(tail.reach(groups[8].last_sequence() + 1), None);

        // From group 1 on, up to the WAL floor at group 7's first sequence.
        let after = groups[0].last_sequence();
        let floor = groups[7].first_sequence();
        let served = tail.read(after, 1 << 20, floor - 1).unwrap();
        assert_eq!(
            sequences(&served),
            sequences(&groups[1..7]),
            "crosses segments, stops at the floor"
        );
        for (served, group) in served.iter().zip(&groups[1..7]) {
            assert_eq!(served.digest(), group.digest());
        }
        // The byte limit cuts the run at a group boundary.
        let one = groups[1].data().len();
        let limited = tail.read(after, one * 2, u64::MAX).unwrap();
        assert_eq!(sequences(&limited), sequences(&groups[1..3]));
        // A single group larger than the limit is refused, as by the WAL.
        assert!(matches!(
            tail.read(after, one - 1, u64::MAX),
            Err(TailError::Oversized { .. })
        ));
        // A cursor inside a group is not a boundary.
        let inside = groups[1].first_sequence();
        assert!(matches!(
            tail.read(inside, 1 << 20, u64::MAX),
            Err(TailError::Boundary { .. })
        ));
        // Past the archive: nothing.
        assert!(tail
            .read(groups[8].last_sequence(), 1 << 20, u64::MAX)
            .unwrap()
            .is_empty());
    }

    /// Spec P3.2 §5: a hole in the archive is where a tail stops reaching,
    /// so a source can name it; a chain names the missing range.
    #[test]
    fn a_hole_is_where_the_tail_stops_reaching_and_the_chain_names_it() {
        let root = TempDir::new().unwrap();
        let (history, groups) = batches(9, 1);
        let dir = segment_dir(root.path(), history, 1);
        let node = Uuid::new_v4();
        seal(&dir, &groups[0..3], node);
        let hole = seal(&dir, &groups[3..6], node);
        seal(&dir, &groups[6..9], node);
        std::fs::remove_file(&hole.path).unwrap();
        let tail = ArchiveFill::directory(root.path().to_path_buf(), None).tail(history, 1);
        assert_eq!(tail.reach(groups[0].first_sequence()), Some(groups[2].last_sequence()));
        let served = tail.read(groups[0].last_sequence(), 1 << 20, u64::MAX).unwrap();
        assert_eq!(sequences(&served), sequences(&groups[1..3]), "stops at the hole");

        let chain = build_chain(
            &[root.path().to_path_buf()],
            history,
            1,
            groups[0].last_sequence(),
            None,
        )
        .unwrap();
        assert!(chain.refused.is_empty(), "{:?}", chain.refused);
        assert_eq!(chain.reached, groups[2].last_sequence());
        assert_eq!(chain.newest, Some(groups[8].last_sequence()));
        assert_eq!(
            chain.missing,
            Some((groups[3].first_sequence(), groups[5].last_sequence())),
            "the missing range, up to the next segment found"
        );
        // Up to an explicit target past everything: the gap is named first.
        let through = groups[8].last_sequence() + 10;
        let chain = build_chain(
            &[root.path().to_path_buf()],
            history,
            1,
            groups[8].last_sequence(),
            Some(through),
        )
        .unwrap();
        assert!(chain.segments.is_empty());
        assert_eq!(chain.missing, Some((groups[8].last_sequence() + 1, through)));
    }

    /// Spec P3.2 §5: a chain across directories — the primary's archive and
    /// a standby's, cut at different boundaries, plus a byte-identical copy —
    /// continues the cursor without a hole, counts each copy once, and a
    /// segment of another dataset among the copies is refused by name.
    #[test]
    fn a_chain_spans_directories_dedupes_copies_and_refuses_a_foreign_segment() {
        let root = TempDir::new().unwrap();
        let (history, groups) = batches(10, 1);
        let primary = root.path().join("primary");
        let standby = root.path().join("standby");
        let copies = root.path().join("copies");
        std::fs::create_dir_all(&copies).unwrap();
        let primary_dir = segment_dir(&primary, history, 1);
        let standby_dir = segment_dir(&standby, history, 1);
        let first = seal(&primary_dir, &groups[0..4], Uuid::new_v4());
        seal(&standby_dir, &groups[2..7], Uuid::new_v4());
        seal(&primary_dir, &groups[7..10], Uuid::new_v4());
        // A byte-identical copy, loose in the operator's directory.
        std::fs::copy(&first.path, copies.join(first.path.file_name().unwrap())).unwrap();
        let roots = vec![primary.clone(), standby.clone(), copies.clone()];

        let chain = build_chain(&roots, history, 1, 0, None).unwrap();
        assert!(chain.refused.is_empty(), "{:?}", chain.refused);
        assert_eq!(chain.found.len(), 3, "the copy counts once");
        assert!(
            chain.notes.iter().any(|note| note.contains("same SHA-256")),
            "{:?}",
            chain.notes
        );
        assert_eq!(chain.segments.len(), 3);
        assert_eq!(chain.reached, groups[9].last_sequence());
        assert_eq!(chain.missing, None);
        verify_chain(&chain, 0, None).unwrap();
        // From the middle of the overlap: the standby's segment continues it.
        let middle = groups[3].last_sequence();
        let chain = build_chain(&roots, history, 1, middle, None).unwrap();
        assert_eq!(chain.segments[0].first_sequence(), groups[2].first_sequence());
        verify_chain(&chain, middle, None).unwrap();

        // A segment of another history, loose among the copies: refused.
        let (other, theirs) = batches(2, 1);
        let foreign = seal(&root.path().join("elsewhere"), &theirs, Uuid::new_v4());
        let stray = copies.join("foreign.hseg");
        std::fs::copy(&foreign.path, &stray).unwrap();
        let chain = build_chain(&roots, history, 1, 0, None).unwrap();
        assert_eq!(chain.refused.len(), 1, "{:?}", chain.refused);
        assert!(
            chain.refused[0].contains("foreign.hseg") && chain.refused[0].contains(&other.to_string()),
            "{}",
            chain.refused[0]
        );
        // Filed under its own history's directory, it is another dataset's
        // archive beside this one: passed over.
        std::fs::remove_file(&stray).unwrap();
        let filed = segment_dir(&copies, other, 1);
        std::fs::create_dir_all(&filed).unwrap();
        std::fs::copy(&foreign.path, filed.join(foreign.path.file_name().unwrap())).unwrap();
        let chain = build_chain(&roots, history, 1, 0, None).unwrap();
        assert!(chain.refused.is_empty(), "{:?}", chain.refused);
        assert!(
            chain.notes.iter().any(|note| note.contains("skipped")),
            "{:?}",
            chain.notes
        );
    }

    /// A segment whose body was damaged is refused by the deep verification
    /// a chain must pass before anything of it is applied, naming the file.
    #[test]
    fn a_damaged_segment_fails_the_chain_verification_by_name() {
        let root = TempDir::new().unwrap();
        let (history, groups) = batches(6, 1);
        let dir = segment_dir(root.path(), history, 1);
        seal(&dir, &groups[0..3], Uuid::new_v4());
        let damaged = seal(&dir, &groups[3..6], Uuid::new_v4());
        let mut bytes = std::fs::read(&damaged.path).unwrap();
        let index = crate::replication::archive_segment::HEADER_LEN + 30;
        bytes[index] ^= 0x5A;
        std::fs::write(&damaged.path, bytes).unwrap();
        let chain = build_chain(&[root.path().to_path_buf()], history, 1, 0, None).unwrap();
        assert_eq!(chain.segments.len(), 2, "header and footer still verify");
        let refused = verify_chain(&chain, 0, None).unwrap_err();
        assert!(
            refused.contains(&damaged.path.display().to_string()) && refused.contains("file hash"),
            "{refused}"
        );
    }
}
