//! Recovering a physical standby from archived WAL (spec P3.2 §2-§3, plan
//! §D6 "from WAL"): Oracle's `RECOVER STANDBY DATABASE` and managed recovery
//! from archived logs.
//!
//! * [`recover_from_archive`] (`heliosdb-nano recover --from-archive`) brings
//!   a stopped standby forward from segments an operator gathered — copied
//!   from the primary, from another standby, from a backup — with or without
//!   a reachable primary.
//! * [`run_archive_standby`] (`start --replication-role standby --source
//!   archive`) keeps applying segments as they appear in its directories,
//!   never opening a replication connection, and serves reads meanwhile.
//!
//! # The same apply path as a live receiver
//!
//! Both open the standby's store through `ReopenedPhysicalStage::open` — the
//! marker, manifest, history and durable cursor proven as at every standby
//! start — and apply the write groups through `PhysicalApplier::apply_run`
//! over an engine opened in `PhysicalStandby` mode, exactly as a receiver
//! applies a poll from its primary (spec P4.1e, after P4.1d): a segment's
//! groups are gathered into runs of at most `APPLY_CHUNK_BYTES` (a larger
//! group is a run of its own), each run decoded, pre-read and published once
//! and written one RocksDB write per group, with the same cursor rules (a
//! group must start right after the cursor; an already applied one is
//! skipped by sequence), the same verbatim write with the cursor in the same
//! write group, the same derived-state publication. A standby recovered from
//! segments is therefore indistinguishable from one that streamed them.
//!
//! The groups are written deferred and the WAL is fsynced once per segment
//! (a receiver fsyncs once per poll), not once per group. A crash mid-segment
//! is still safe: each group and the cursor that accounts for it are one
//! RocksDB write, so the store reopens at a consistent cursor — the last one
//! fsynced, or a later one that reached the disk anyway — whose groups are
//! all applied and none of whose successors is; re-running the command
//! applies only what follows that cursor (idempotent, as above). A recovery
//! stopped by an error fsyncs what it applied when the stage is closed.
//!
//! # Verify before the first apply
//!
//! A segment carries deltas, not a base: once a corrupt group is applied the
//! standby cannot be trusted. `recover` therefore verifies EVERY segment of
//! its chain deep (`archive_chain::verify_chain`: file hash, every frame's
//! digest, contiguity, history, the group boundary at the cursor) before it
//! applies anything, and refuses — naming the file — a corrupt segment or one
//! of another dataset among the copies. The archive-only standby verifies each
//! segment deep before it applies it. Gathering runs changes none of it:
//! every frame is read and checked (its digest, its sequence against the
//! cursor, `--through`) before the run it joins is handed to the applier.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use super::archive_chain::{build_chain, verify_chain, verify_segment_from, RecoveryChain};
use super::archive_segment::{unix_now, SegmentInfo};
use super::archiver::{dataset_key, ArchiveFeed};
use super::checkpoint_transfer::{ReopenedPhysicalStage, TransferLimits};
use super::epoch::read_epoch;
use super::physical::{Durability, PhysicalBatch};
use super::physical_applier::{NoopDerivedStateListener, PhysicalApplier};
use super::physical_receiver::ServingAttach;
use super::physical_wire::APPLY_CHUNK_BYTES;
use crate::crypto::KeyManager;
use crate::standby_posture::PostureTracker;
use crate::storage::{StorageEngine, WriteGate};
use crate::Config;

/// Why a recovery did not complete.
#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    /// Nothing was applied: the standby is where it was.
    #[error("recover refused: {0}")]
    Refused(String),
    /// Applying stopped part-way; every group before it is durable.
    #[error("recover stopped at sequence {reached}: {reason}; re-run the command to continue from there")]
    Failed { reached: u64, reason: String },
    /// Spec P4.1 §1: the standby needs a full copy of its primary that was
    /// not applied — refused by `--no-full-copy`, or completed beside an
    /// occupied data directory without `--replace`. The reason names what to
    /// do. `recover` exits 3.
    #[error("{0}")]
    FullCopyPending(String),
}

/// What [`recover_from_archive`] does.
pub struct RecoverOptions<'a> {
    /// The installed physical standby to advance.
    pub data_dir: &'a Path,
    /// The dataset history its marker names.
    pub history: Uuid,
    /// The directories segments are gathered from.
    pub sources: &'a [PathBuf],
    /// Stop after the last write group that ends at or before it.
    pub through: Option<u64>,
    /// The node's configuration: storage options, and the dataset key of an
    /// encrypted dataset (its segments' frames are sealed with it).
    pub config: &'a Config,
    pub limits: TransferLimits,
}

/// What a recovery did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverReport {
    /// The standby's cursor before.
    pub started_at: u64,
    /// The standby's durable cursor after.
    pub reached: u64,
    /// Write groups applied.
    pub applied: u64,
    /// Write groups the standby already held (skipped by sequence).
    pub skipped: u64,
    /// Segments the chain drew from.
    pub segments: usize,
    /// The newest sequence any usable segment holds.
    pub newest: Option<u64>,
    /// The first interval no segment holds, short of `--through` or of the
    /// newest segment.
    pub missing: Option<(u64, u64)>,
    /// The archive bytes the recovery drew on: the files of the segments of
    /// its chain.
    pub archive_bytes: u64,
    /// How long the recovery took, verification included.
    pub elapsed: Duration,
}

fn refused(reason: impl Into<String>) -> RecoverError {
    RecoverError::Refused(reason.into())
}

/// Advance an installed, STOPPED physical standby from archived segments
/// (spec P3.2 §2). See the module documentation. `progress` receives one line
/// per segment (and the notes of the chain). The caller holds no handle on
/// the directory; the stage open takes RocksDB's lock, so a running node
/// refuses it.
pub fn recover_from_archive(
    options: &RecoverOptions<'_>,
    progress: &mut dyn FnMut(&str),
) -> Result<RecoverReport, RecoverError> {
    let started = Instant::now();
    let key =
        dataset_key(options.config).map_err(|error| refused(format!("the dataset key cannot be loaded: {error}")))?;
    let stage = ReopenedPhysicalStage::open(options.data_dir, options.history, options.limits, options.config)
        .map_err(|error| {
            refused(format!(
                "cannot open the physical standby at {}: {error}",
                options.data_dir.display()
            ))
        })?;
    let started_at = stage.cursor().source_sequence();
    let outcome = recover_stage(&stage, options, key.as_deref(), started_at, progress);
    // The engine and its applier are gone: the stage holds the only handle.
    let closed = stage.close();
    let mut report = outcome?;
    report.elapsed = started.elapsed();
    match closed {
        Ok(closed) if closed.cursor.source_sequence() == report.reached => Ok(report),
        Ok(closed) => Err(RecoverError::Failed {
            reached: closed.cursor.source_sequence(),
            reason: format!(
                "the standby's durable cursor is {}, not the {} its writer reported",
                closed.cursor.source_sequence(),
                report.reached
            ),
        }),
        Err(error) => Err(RecoverError::Failed {
            reached: report.reached,
            reason: format!("the standby's store could not be closed cleanly: {error}"),
        }),
    }
}

/// [`recover_from_archive`] over an opened stage.
fn recover_stage(
    stage: &ReopenedPhysicalStage,
    options: &RecoverOptions<'_>,
    key: Option<&KeyManager>,
    started_at: u64,
    progress: &mut dyn FnMut(&str),
) -> Result<RecoverReport, RecoverError> {
    // Spec P5.1: the chain is the standby's own epoch's (its store's).
    let epoch = read_epoch(stage.writer().db())
        .map_err(|error| refused(format!("the standby's replication epoch cannot be read: {error}")))?;
    let chain = build_chain(options.sources, options.history, epoch, started_at, options.through)
        .map_err(|error| refused(error.to_string()))?;
    for note in &chain.notes {
        progress(&format!("recover: {note}"));
    }
    if !chain.refused.is_empty() {
        return Err(refused(format!(
            "{} file(s) in the given directories cannot be used, so nothing was applied:\n  {}",
            chain.refused.len(),
            chain.refused.join("\n  ")
        )));
    }
    let mut report = RecoverReport {
        started_at,
        reached: started_at,
        applied: 0,
        skipped: 0,
        segments: chain.segments.len(),
        newest: chain.newest,
        missing: chain.missing,
        archive_bytes: chain.segments.iter().map(|segment| segment.file_bytes).sum(),
        elapsed: Duration::ZERO,
    };
    if chain.segments.is_empty() {
        return Ok(report);
    }
    progress(&format!(
        "recover: the standby is at sequence {started_at}; {} segment(s) continue it to {}; verifying every one \
         before anything is applied",
        chain.segments.len(),
        chain.reached
    ));
    verify_chain(&chain, started_at, key).map_err(|reason| refused(format!("{reason}; nothing was applied")))?;
    progress(&format!("recover: {} segment(s) verified", chain.segments.len()));

    // The applier a live receiver uses, over an engine opened on the stage's
    // writer, hydrated first (the derived state the publication maintains).
    let writer = Arc::clone(stage.writer());
    let engine = StorageEngine::open_physical_standby(
        options.data_dir,
        options.config,
        Arc::clone(&writer),
        WriteGate::closed("recovery from archived WAL"),
    )
    .map_err(|error| refused(format!("cannot open the standby's engine: {error}")))?;
    let applier = PhysicalApplier::new(
        Arc::new(engine),
        Arc::clone(&writer),
        Arc::new(NoopDerivedStateListener),
    )
    .map_err(|error| refused(format!("cannot build the standby's applier: {error}")))?;
    applier
        .hydrate()
        .map_err(|error| refused(format!("cannot hydrate the standby's derived state: {error}")))?;
    let applied = apply_chain(stage, &applier, &chain, options.through, key, progress, &mut report);
    report.reached = writer.cursor().source_sequence();
    drop(applier);
    applied.map(|()| report)
}

/// Spec P4.1e: a segment's verified groups gathered into runs for
/// [`PhysicalApplier::apply_run`], as a receiver gathers a poll's (spec
/// P4.1d): at most [`APPLY_CHUNK_BYTES`] of raw groups per run, a larger
/// group a run of its own.
#[derive(Default)]
struct Runs {
    run: Vec<PhysicalBatch>,
    bytes: usize,
}

impl Runs {
    /// Add `batch`. Returns the run it closed when it would have taken that
    /// run past the chunk: that run is to be applied before `batch`.
    fn push(&mut self, batch: PhysicalBatch) -> Option<Vec<PhysicalBatch>> {
        let size = batch.data().len();
        let full = if !self.run.is_empty() && self.bytes.saturating_add(size) > APPLY_CHUNK_BYTES {
            Some(self.take())
        } else {
            None
        };
        self.bytes = self.bytes.saturating_add(size);
        self.run.push(batch);
        full
    }

    /// The run gathered so far (possibly empty), leaving none.
    fn take(&mut self) -> Vec<PhysicalBatch> {
        self.bytes = 0;
        std::mem::take(&mut self.run)
    }

    /// The last sequence of the gathered groups, if any.
    fn last_sequence(&self) -> Option<u64> {
        self.run.last().map(PhysicalBatch::last_sequence)
    }
}

/// Hand one run of `info`'s verified groups to `applier`, deferred, after the
/// stage check a receiver makes once per run (spec P4.1d). An empty run does
/// nothing.
fn apply_segment_run(
    stage: &ReopenedPhysicalStage,
    applier: &PhysicalApplier,
    info: &SegmentInfo,
    run: &[PhysicalBatch],
) -> Result<(), String> {
    let (Some(first), Some(last)) = (run.first(), run.last()) else {
        return Ok(());
    };
    stage.verify().map_err(|error| error.to_string())?;
    applier
        .apply_run(run, Durability::Deferred)
        .map(|_| ())
        .map_err(|error| {
            format!(
                "segment {}: write groups {}..{}: {error}",
                info.path.display(),
                first.first_sequence(),
                last.last_sequence()
            )
        })
}

/// Apply `chain`'s groups past the cursor through `applier`, one segment at a
/// time, in runs (spec P4.1e), fsyncing the WAL once per segment; groups the
/// standby already holds are skipped by sequence, and none that ends past
/// `through` is applied.
fn apply_chain(
    stage: &ReopenedPhysicalStage,
    applier: &PhysicalApplier,
    chain: &RecoveryChain,
    through: Option<u64>,
    key: Option<&KeyManager>,
    progress: &mut dyn FnMut(&str),
    report: &mut RecoverReport,
) -> Result<(), RecoverError> {
    let failed = |reason: String| RecoverError::Failed {
        reached: applier.writer().cursor().source_sequence(),
        reason,
    };
    for info in &chain.segments {
        let (mut applied, mut skipped) = (0_u64, 0_u64);
        let mut stopped = false;
        let mut runs = Runs::default();
        for frame in info.frames(key).map_err(|error| failed(error.to_string()))? {
            let batch = frame.map_err(|error| failed(error.to_string()))?;
            // What the store holds once the gathered run is applied.
            let held = runs
                .last_sequence()
                .unwrap_or_else(|| applier.writer().cursor().source_sequence());
            if batch.last_sequence() <= held {
                skipped += 1;
                continue;
            }
            if through.is_some_and(|through| batch.last_sequence() > through) {
                stopped = true;
                break;
            }
            if let Some(full) = runs.push(batch) {
                apply_segment_run(stage, applier, info, &full).map_err(&failed)?;
                applied += full.len() as u64;
            }
        }
        let last = runs.take();
        apply_segment_run(stage, applier, info, &last).map_err(&failed)?;
        applied += last.len() as u64;
        if applied > 0 {
            // One fsync for the segment's groups (module documentation).
            applier
                .writer()
                .sync()
                .map_err(|error| failed(format!("segment {}: {error}", info.path.display())))?;
        }
        report.applied += applied;
        report.skipped += skipped;
        progress(&format!(
            "recover: {}: sequences {}..{}, {applied} write group(s) applied, {skipped} already held; the standby is \
             at {}",
            info.path.display(),
            info.first_sequence(),
            info.last_sequence(),
            applier.writer().cursor().source_sequence()
        ));
        if stopped {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Archive-only standby (spec P3.2 §3).
// ---------------------------------------------------------------------------

/// What an archive-only standby follows.
pub struct ArchiveStandbyOptions {
    /// The installed physical standby it serves.
    pub data_dir: PathBuf,
    /// The dataset history its marker names.
    pub history: Uuid,
    /// The directories segments appear in (operator copies, NFS, rsync).
    pub sources: Vec<PathBuf>,
    /// The node's configuration: storage options and the dataset key.
    pub config: Config,
    pub limits: TransferLimits,
    /// How often the directories are scanned for new segments.
    pub poll_interval: Duration,
}

/// The directories, as the posture and the log name them.
pub fn describe_sources(sources: &[PathBuf]) -> String {
    sources
        .iter()
        .map(|source| source.display().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Run an archive-only standby until `stop` is set (spec P3.2 §3): blocking,
/// on a thread of its own. Opens the installed standby, builds its serving
/// side with `attach` (as a following receiver does), then applies every
/// segment that appears in its directories past its cursor — each verified
/// deep first — and reports its posture through `tracker`: `recovering`
/// (mode `archive`) while it is current, `catching_up` when the segment
/// holding its cursor is older than `max_standby_lag`, `needs_recovery` on a
/// gap or an unusable segment, which it leaves by itself when the directories
/// change. Every applied group is offered to `archive` (its own WAL archive,
/// when it keeps one). Returns `Err` only when it cannot continue at all: the
/// store cannot be opened or served, or a write left it uncertain.
pub fn run_archive_standby(
    options: ArchiveStandbyOptions,
    attach: ServingAttach,
    tracker: &PostureTracker,
    archive: Option<ArchiveFeed>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let key = dataset_key(&options.config).map_err(|error| format!("the dataset key cannot be loaded: {error}"))?;
    let stage = ReopenedPhysicalStage::open(&options.data_dir, options.history, options.limits, &options.config)
        .map_err(|error| {
            format!(
                "cannot open the physical standby at {}: {error}",
                options.data_dir.display()
            )
        })?;
    let applier = attach(stage.path(), stage.writer())?;
    let sources = describe_sources(&options.sources);
    tracing::info!(
        "recover: archive-only standby {} following segments in {sources} from sequence {}",
        options.data_dir.display(),
        stage.cursor().source_sequence()
    );
    let mut follower = Follower {
        stage: &stage,
        applier: &applier,
        key: key.as_deref(),
        archive: archive.as_ref(),
        history: options.history,
        sources: &options.sources,
        tracker,
        stop,
        verified: std::collections::HashSet::new(),
        last_blocked: None,
    };
    let result = loop {
        if stop.load(Ordering::Acquire) {
            break Ok(());
        }
        if let Err(reason) = follower.round() {
            break Err(reason);
        }
        // An idle moment: what the applier deferred runs now.
        if let Err(error) = applier.run_pending_rebuilds() {
            tracing::warn!("recover: deferred derived-state work failed; it stays pending: {error}");
        }
        let deadline = Instant::now() + options.poll_interval;
        while Instant::now() < deadline && !stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(50).min(options.poll_interval));
        }
    };
    // The serving database still shares the store: it closes with the
    // process. What was applied deferred is made durable first.
    if !applier.writer().is_poisoned() {
        if let Err(error) = applier.writer().sync() {
            tracing::error!("recover: could not fsync the applied write groups before stopping: {error}");
        }
    }
    tracing::info!(
        "recover: archive-only standby stopped at sequence {}",
        applier.writer().cursor().source_sequence()
    );
    result
}

/// One archive-only standby's state between rounds.
struct Follower<'a> {
    stage: &'a ReopenedPhysicalStage,
    applier: &'a PhysicalApplier,
    key: Option<&'a KeyManager>,
    archive: Option<&'a ArchiveFeed>,
    history: Uuid,
    sources: &'a [PathBuf],
    tracker: &'a PostureTracker,
    stop: &'a AtomicBool,
    /// Segments verified deep already (by path and file hash): a segment
    /// applied part-way is not verified again at the next round.
    verified: std::collections::HashSet<(PathBuf, [u8; 32])>,
    /// The last reason the standby could not continue, logged once.
    last_blocked: Option<String>,
}

impl Follower<'_> {
    fn cursor(&self) -> u64 {
        self.applier.writer().cursor().source_sequence()
    }

    /// Say once why the standby cannot continue (posture `needs_recovery`).
    fn blocked(&mut self, reason: String) {
        if self.last_blocked.as_ref() != Some(&reason) {
            tracing::warn!("recover: archive-only standby cannot continue: {reason}");
            self.last_blocked = Some(reason.clone());
        }
        self.tracker.archive_blocked(reason);
    }

    /// The posture after a round that could continue.
    fn current(&mut self, chain: &RecoveryChain) {
        if self.last_blocked.take().is_some() {
            tracing::info!("recover: archive-only standby continues at sequence {}", self.cursor());
        }
        let cursor = self.cursor();
        self.tracker.archive_progress(
            cursor,
            chain.newest.unwrap_or(cursor).max(cursor),
            chain.observed_at(cursor),
            unix_now(),
        );
    }

    /// One scan of the directories and the segments it found applied.
    fn round(&mut self) -> Result<(), String> {
        let cursor = self.cursor();
        // Spec P5.1: the standby's own epoch, as its store holds it now.
        let epoch = match read_epoch(self.applier.writer().db()) {
            Ok(epoch) => epoch,
            Err(error) => {
                self.blocked(format!("the standby's replication epoch cannot be read: {error}"));
                return Ok(());
            }
        };
        let chain = match build_chain(self.sources, self.history, epoch, cursor, None) {
            Ok(chain) => chain,
            Err(error) => {
                self.blocked(error.to_string());
                return Ok(());
            }
        };
        if let Some(reason) = chain.refused.first() {
            self.blocked(format!(
                "{reason}{}; move it out of the archive source to continue",
                if chain.refused.len() > 1 {
                    format!(" (and {} more)", chain.refused.len() - 1)
                } else {
                    String::new()
                }
            ));
            return Ok(());
        }
        let mut next = cursor.saturating_add(1);
        for info in &chain.segments {
            if self.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let identity = (info.path.clone(), info.footer.file_sha256);
            if !self.verified.contains(&identity) {
                if let Err(reason) = verify_segment_from(info, next, self.key) {
                    self.blocked(format!("{reason}; it was not applied"));
                    return Ok(());
                }
            }
            // Idempotent: a segment verified once (same path and hash) is
            // not read deep again on the next pass.
            self.verified.insert(identity);
            self.apply_segment(info)?;
            next = info.last_sequence().saturating_add(1);
            self.current(&chain);
        }
        match chain.missing {
            Some((from, to)) => self.blocked(format!(
                "archive gap {from}..{to}: no segment in {} holds it (the standby is at {}; it continues once the \
                 segment appears)",
                describe_sources(self.sources),
                self.cursor()
            )),
            None => self.current(&chain),
        }
        Ok(())
    }

    /// Apply one verified segment's groups past the cursor in runs (spec
    /// P4.1e; deferred, one fsync for the segment), offering each group to
    /// the standby's own archive once its run is applied. A stop between
    /// frames leaves the run gathered so far unapplied (the next start
    /// applies it from the cursor).
    fn apply_segment(&self, info: &SegmentInfo) -> Result<(), String> {
        let mut applied = 0_u64;
        let mut runs = Runs::default();
        let frames = info.frames(self.key).map_err(|error| error.to_string())?;
        let mut stopped = false;
        for frame in frames {
            if self.stop.load(Ordering::Acquire) {
                stopped = true;
                break;
            }
            let batch = frame.map_err(|error| error.to_string())?;
            let held = runs.last_sequence().unwrap_or_else(|| self.cursor());
            if batch.last_sequence() <= held {
                continue;
            }
            if let Some(full) = runs.push(batch) {
                applied += self.apply_and_offer(info, &full)?;
            }
        }
        if !stopped {
            let last = runs.take();
            applied += self.apply_and_offer(info, &last)?;
        }
        if applied > 0 {
            self.applier.writer().sync().map_err(|error| error.to_string())?;
            tracing::info!(
                "recover: applied {applied} write group(s) of {} (sequences {}..{}); the standby is at {}",
                info.path.display(),
                info.first_sequence(),
                info.last_sequence(),
                self.cursor()
            );
        }
        Ok(())
    }

    /// Apply one run of a segment's groups, then offer each to the
    /// standby's own archive. Returns the groups applied.
    fn apply_and_offer(&self, info: &SegmentInfo, run: &[PhysicalBatch]) -> Result<u64, String> {
        apply_segment_run(self.stage, self.applier, info, run)?;
        if let Some(feed) = self.archive {
            for batch in run {
                feed.offer(batch, self.applier.writer().db());
            }
        }
        Ok(run.len() as u64)
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::replication::archive_segment::{segment_dir, OpenSegment, SegmentHeader, HEADER_LEN};
    use crate::replication::checkpoint_transfer::{OnlineCheckpointExport, ReceiverStaging};
    use crate::replication::history::load_or_create_primary_history;
    use crate::replication::physical::{
        poll_source, range_digest, FlushMemo, PhysicalBatch, PhysicalPoll, MAX_BATCH_BYTES,
    };
    use crate::EmbeddedDatabase;
    use rocksdb::{Options, DB};

    /// Seal `batches` into one segment of `archive`.
    fn seal(archive: &Path, history: Uuid, batches: &[PhysicalBatch]) -> SegmentInfo {
        let header = SegmentHeader {
            flags: 0,
            history,
            epoch: crate::replication::epoch::INITIAL_EPOCH,
            first_sequence: batches[0].first_sequence(),
            created_at: unix_now(),
            node_id: Uuid::new_v4(),
            key_check: [0; 32],
        };
        let mut open = OpenSegment::create(
            &segment_dir(archive, history, crate::replication::epoch::INITIAL_EPOCH),
            header,
        )
        .unwrap();
        for batch in batches {
            open.append(batch, unix_now(), None).unwrap();
        }
        open.seal().unwrap()
    }

    fn persistent(path: &Path) -> Config {
        let mut config = Config::default();
        config.storage.path = Some(path.to_path_buf());
        config.storage.memory_only = false;
        config
    }

    /// A physical standby of `raw` installed at `root/name` from a downloaded
    /// checkpoint (the production path: export, staging, verification,
    /// publication): its path and cut.
    fn install(raw: &DB, history: Uuid, root: &Path, config: &Config, name: &str) -> (PathBuf, u64) {
        let limits = TransferLimits::default();
        let export = OnlineCheckpointExport::create(raw, history, root, limits).unwrap();
        let target = root.join(name);
        let mut staging = ReceiverStaging::begin(&target, &export.manifest_bytes().unwrap(), history, limits).unwrap();
        for (index, file) in export.manifest().files.iter().enumerate() {
            let mut offset = 0_u64;
            while offset < file.size {
                let chunk = export.read_chunk(index as u32, offset, 64 * 1024).unwrap();
                staging.append_chunk(index as u32, offset, &chunk).unwrap();
                offset += chunk.len() as u64;
            }
        }
        let installed = staging
            .finish_download(config)
            .unwrap()
            .open(config)
            .unwrap()
            .publish()
            .unwrap();
        (installed.path, installed.cursor.source_sequence())
    }

    /// The applier of an engine serving `stage` (a standby's serving side,
    /// as `ServingAttach` builds it), hydrated.
    fn serving(stage: &ReopenedPhysicalStage) -> PhysicalApplier {
        let path = stage.path().to_path_buf();
        let engine = StorageEngine::open_physical_standby(
            &path,
            &persistent(&path),
            Arc::clone(stage.writer()),
            WriteGate::closed("test standby"),
        )
        .unwrap();
        let applier = PhysicalApplier::new(
            Arc::new(engine),
            Arc::clone(stage.writer()),
            Arc::new(NoopDerivedStateListener),
        )
        .unwrap();
        applier.hydrate().unwrap();
        applier
    }

    /// The rows of `t` a standby's engine serves, in row order.
    fn served_rows(applier: &PhysicalApplier) -> Vec<crate::Tuple> {
        let mut rows = applier.engine().scan_table("t").unwrap();
        rows.sort_by_key(|row| row.row_id);
        rows
    }

    /// A Nano primary with a table, and its physical standby installed from
    /// a downloaded checkpoint (the production path: export, staging,
    /// verification, publication).
    struct Fixture {
        primary: EmbeddedDatabase,
        raw: Arc<DB>,
        history: Uuid,
        standby: PathBuf,
        cut: u64,
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let mut config = persistent(&root.path().join("primary"));
            config.replication.physical.enabled = true;
            let primary = EmbeddedDatabase::with_config(config.clone()).unwrap();
            primary.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
            primary.execute("INSERT INTO t VALUES (0, 'seed')").unwrap();
            let raw = primary.storage.db();
            let history = load_or_create_primary_history(raw.path()).unwrap();
            let (standby, cut) = install(&raw, history, root.path(), &config, "standby");
            Self {
                primary,
                raw,
                history,
                standby,
                cut,
                root,
            }
        }

        /// Another physical standby of the primary as it is now, installed
        /// beside the first: its path.
        fn install(&self, name: &str) -> PathBuf {
            let mut config = persistent(&self.root.path().join("primary"));
            config.replication.physical.enabled = true;
            let (path, cut) = install(&self.raw, self.history, self.root.path(), &config, name);
            assert_eq!(cut, self.cut, "installed at the first standby's cut");
            path
        }

        /// Insert `ids`, one write group each at least, and return the
        /// primary's groups since the standby's cut.
        fn write(&self, ids: std::ops::Range<i32>) -> PhysicalPoll {
            for id in ids {
                self.primary
                    .execute(&format!("INSERT INTO t VALUES ({id}, 'row {id}')"))
                    .unwrap();
            }
            poll_source(&self.raw, self.history, self.cut, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap()
        }
    }

    /// Spec P3.2 §2, §5: recovery applies the chain through the live apply
    /// path to the primary's exact default column family, and re-running it
    /// skips every group already applied (a no-op).
    #[test]
    fn recovery_reaches_the_primarys_state_and_a_rerun_skips_applied_groups() {
        let fixture = Fixture::new();
        let polled = fixture.write(1..9);
        assert!(polled.batches.len() >= 8, "{} groups", polled.batches.len());
        let half = polled.batches.len() / 2;
        let archive = fixture.root.path().join("copies");
        seal(&archive, fixture.history, &polled.batches[..half]);
        seal(&archive, fixture.history, &polled.batches[half..]);
        let config = persistent(&fixture.standby);
        let sources = vec![archive.clone()];
        let options = RecoverOptions {
            data_dir: &fixture.standby,
            history: fixture.history,
            sources: &sources,
            through: None,
            config: &config,
            limits: TransferLimits::default(),
        };
        // Stopped part-way (`--through`, or a crash: every group is durable).
        let midway = polled.batches[half - 1].last_sequence();
        let mut lines = Vec::new();
        let report = recover_from_archive(
            &RecoverOptions {
                through: Some(midway),
                ..options
            },
            &mut |line| lines.push(line.to_string()),
        )
        .unwrap();
        assert_eq!(report.started_at, fixture.cut);
        assert_eq!(report.reached, midway);
        assert_eq!(report.applied, half as u64);
        assert_eq!(report.missing, None);
        assert!(lines.iter().any(|line| line.contains("verified")), "{lines:?}");

        // Re-run from segments that start before the cursor (one segment of
        // the whole run): the groups already applied are skipped by sequence.
        let whole = fixture.root.path().join("whole");
        seal(&whole, fixture.history, &polled.batches);
        let whole_sources = vec![whole];
        let report = recover_from_archive(
            &RecoverOptions {
                sources: &whole_sources,
                ..options
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(report.started_at, midway);
        assert_eq!(report.reached, polled.through);
        assert_eq!(report.skipped, half as u64);
        assert_eq!(report.applied, (polled.batches.len() - half) as u64);

        assert_eq!(
            fixture.raw.latest_sequence_number(),
            polled.through,
            "precondition: the primary wrote nothing since"
        );
        let standby = DB::open_for_read_only(&Options::default(), &fixture.standby, false).unwrap();
        assert_eq!(
            range_digest(&standby, None, b"").unwrap(),
            range_digest(&fixture.raw, None, b"").unwrap(),
            "the primary's default column family at the same sequence"
        );
        drop(standby);

        // Idempotent: a re-run applies nothing.
        let again = recover_from_archive(&options, &mut |_| {}).unwrap();
        assert_eq!(again.applied, 0);
        assert_eq!(again.reached, polled.through);
    }

    /// Spec P3.2 §2: a corrupt segment anywhere in the chain is named and
    /// nothing is applied; a hole stops the chain short and names the range.
    #[test]
    fn a_corrupt_segment_applies_nothing_and_a_hole_names_the_missing_range() {
        let fixture = Fixture::new();
        let polled = fixture.write(1..10);
        let groups = &polled.batches;
        assert!(groups.len() >= 9, "{} groups", groups.len());
        let archive = fixture.root.path().join("copies");
        seal(&archive, fixture.history, &groups[..3]);
        let middle = seal(&archive, fixture.history, &groups[3..6]);
        seal(&archive, fixture.history, &groups[6..]);
        let mut bytes = std::fs::read(&middle.path).unwrap();
        bytes[HEADER_LEN + 25] ^= 0x33;
        std::fs::write(&middle.path, &bytes).unwrap();
        let config = persistent(&fixture.standby);
        let sources = vec![archive.clone()];
        let options = RecoverOptions {
            data_dir: &fixture.standby,
            history: fixture.history,
            sources: &sources,
            through: None,
            config: &config,
            limits: TransferLimits::default(),
        };
        let error = recover_from_archive(&options, &mut |_| {}).unwrap_err();
        assert!(matches!(error, RecoverError::Refused(_)), "{error}");
        let text = error.to_string();
        assert!(
            text.contains(&middle.path.display().to_string()) && text.contains("nothing was applied"),
            "{text}"
        );
        // Nothing moved: an empty chain reports the cursor.
        let probe = recover_from_archive(
            &RecoverOptions {
                sources: &[],
                ..options
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(probe.reached, fixture.cut);

        // The damaged segment removed: a hole, applied up to it.
        std::fs::remove_file(&middle.path).unwrap();
        let report = recover_from_archive(&options, &mut |_| {}).unwrap();
        assert_eq!(report.reached, groups[2].last_sequence(), "applied up to the hole");
        assert_eq!(
            report.missing,
            Some((groups[3].first_sequence(), groups[5].last_sequence()))
        );
    }

    /// Spec P4.1e §3: a segment of 3 000 small write groups is recovered in
    /// runs with ONE fsync of the standby's WAL — it was one per group — to
    /// the rows and the default column family of a standby that followed the
    /// same groups live (`apply_run`, as a receiver applies a poll).
    #[test]
    fn a_segment_of_small_groups_is_recovered_with_one_fsync_to_a_live_standbys_state() {
        let fixture = Fixture::new();
        let live_path = fixture.install("live");
        // Single-row inserts until the primary wrote 3 000 groups past the cut.
        let mut polled = fixture.write(1..1);
        let mut next = 1;
        while polled.batches.len() < 3_000 {
            assert!(next < 20_000, "{} groups from {next} inserts", polled.batches.len());
            polled = fixture.write(next..next + 200);
            next += 200;
        }
        let groups = polled.batches.len() as u64;
        let archive = fixture.root.path().join("one-segment");
        let segment = seal(&archive, fixture.history, &polled.batches);
        assert_eq!(segment.batch_count(), groups, "one segment holds them all");

        // Recovered, through the module's own path, over a stage whose
        // writer counts its fsyncs.
        let config = persistent(&fixture.standby);
        let limits = TransferLimits::default();
        let stage = ReopenedPhysicalStage::open(&fixture.standby, fixture.history, limits, &config).unwrap();
        let sources = vec![archive];
        let options = RecoverOptions {
            data_dir: &fixture.standby,
            history: fixture.history,
            sources: &sources,
            through: None,
            config: &config,
            limits,
        };
        let before = stage.writer().fsyncs();
        let report = recover_stage(&stage, &options, None, fixture.cut, &mut |_| {}).unwrap();
        assert_eq!(report.applied, groups);
        assert_eq!(report.reached, polled.through);
        assert_eq!(
            stage.writer().fsyncs() - before,
            1,
            "one fsync for the segment, not one per group ({groups} groups)"
        );

        // Live: the same groups as a receiver applies a poll.
        let live = ReopenedPhysicalStage::open(&live_path, fixture.history, limits, &persistent(&live_path)).unwrap();
        {
            let applier = serving(&live);
            applier.apply_run(&polled.batches, Durability::Deferred).unwrap();
            live.writer().sync().unwrap();
            let recovered = serving(&stage);
            let rows = served_rows(&recovered);
            assert!(rows.len() > 1, "{} rows", rows.len());
            assert_eq!(rows, served_rows(&applier), "the rows a live standby serves");
        }
        assert_eq!(
            fixture.raw.latest_sequence_number(),
            polled.through,
            "precondition: the primary wrote nothing since"
        );
        let recovered_digest = range_digest(stage.writer().db(), None, b"").unwrap();
        assert_eq!(
            recovered_digest,
            range_digest(live.writer().db(), None, b"").unwrap(),
            "the default column family of the standby that followed live"
        );
        assert_eq!(recovered_digest, range_digest(&fixture.raw, None, b"").unwrap());
        assert_eq!(stage.close().unwrap().cursor.source_sequence(), polled.through);
        live.close().unwrap();
    }
}
