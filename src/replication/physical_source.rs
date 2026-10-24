//! The live primary's side of a replication session: the physical
//! Snapshot/Resume provider, and the standby registry every session reports
//! to (spec P2.1 §2).
//!
//! The wire owner invokes the provider factory only AFTER authenticating the
//! receiver and accepting its `Hello`. Blocking jobs own every referenced
//! DB/checkpoint until completion. Cancellation never publishes a receiver or
//! modifies an existing destination. A provider that will not serve says why
//! ([`Refusal`]): a history it is not, WAL it no longer retains, a shutdown —
//! so the receiver can tell "come back later" from "rebuild me" instead of
//! reconnecting forever.
//!
//! Spec P2.2: a snapshot session's checkpoint export outlives the session
//! that opened it by an [`ExportLeases`] lease, keyed by snapshot id, so a
//! receiver whose download was interrupted opens the SAME snapshot again
//! (`OpenRequest::SnapshotOf`) and resumes it. A leased checkpoint is hard
//! links to the primary's SST files: it pins their disk space while it is
//! kept, which is why leases expire and are bounded in number.
//!
//! Spec P3.2 §1: a receiver whose next groups the store's WAL no longer holds
//! is served them from the WAL archive instead ([`ArchiveFill`], Oracle's FAL
//! gap fetch), on the same connection and in the same frames, until its
//! cursor reaches what the WAL still holds; the switch is re-checked at every
//! poll. Only when the archive cannot cover the gap either is it refused
//! `WalPruned`, naming the missing range and both floors.
//!
//! Spec P3.2 §4: the same sessions serve a CASCADING standby's store
//! ([`SourceKind::Cascade`]): its WAL with its node-local records stripped
//! (`physical::poll_source_cascade`), its exports made the primary's
//! (`OnlineCheckpointExport::create_cascaded`), in the primary's sequence
//! numbers.
//!
//! Spec P4.1 §3.2: a fast-forward session (`OpenRequest::FastForward`,
//! `recover --from-primary`) is served from a leased checkpoint export like
//! a snapshot — reused while its lease holds, so a reconnecting fast-forward
//! continues the same cut — through the export's leaf plan
//! (`fast_forward::FastForwardPlan`), planned once on a blocking thread of
//! the work limiter while the session answers "planning".

use super::archive_chain::{ArchiveFill, ArchiveTail, TailError};
use super::checkpoint_transfer::{
    validate_cascade_source, validate_primary_source, OnlineCheckpointExport, TransferLimits,
};
use super::fast_forward::{FastForwardPlan, PlanningGuard};
use super::ha_state::{ha_state, StandbyHandle, StandbyRegistration, StandbyState, SyncMode};
use super::physical::{
    poll_source, poll_source_cascade, receiver_source_barrier, wal_retains_after, CascadeHint, FlushMemo,
    PhysicalBatch, PhysicalError, PhysicalPoll,
};
use super::physical_wire::{
    valid_leaf_bytes, Heartbeat, Hello, LeafDigests, LeafReady, ObservedSession, OpenRequest, PeerRole, PhysicalSource,
    PollOrigin, Refusal, RefuseCode, Result, SessionObserver, SourceSessionInfo, WireError, WireSyncMode,
};
use crate::standby_posture::EPOCH;
use crate::Config;
use async_trait::async_trait;
use parking_lot::Mutex;
use rocksdb::DB;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

fn source_error(error: impl std::fmt::Display) -> WireError {
    WireError::Source(error.to_string())
}

fn not_serving(reason: &str) -> WireError {
    WireError::Refused(Refusal::new(RefuseCode::SourceNotServing, reason))
}

/// The byte limit of a poll that only proves a cursor is served (a resume's
/// first poll, a cascading source's retention check): the first write group
/// of a poll travels whatever its size (spec P4.1c), so one byte reads that
/// group and nothing more — never up to `MAX_BATCH_BYTES` of groups only to
/// be discarded.
const PROBE_POLL_BYTES: usize = 1;

/// What a replication port serves its sessions from.
#[derive(Clone)]
pub enum SourceKind {
    /// A primary's own store.
    Primary,
    /// Spec P3.2 §4: a physical standby's store, cascaded to a downstream
    /// standby (`[replication.physical] cascade = true`). `config` supplies
    /// the storage options a cascaded export's copy is opened with.
    Cascade { config: Arc<Config> },
}

impl SourceKind {
    /// The source's storage identity, revalidated against `history`.
    fn validate(&self, db: &DB, history: Uuid) -> Result<String> {
        match self {
            Self::Primary => validate_primary_source(db, history),
            Self::Cascade { .. } => validate_cascade_source(db, history),
        }
        .map_err(source_error)
    }

    /// A new checkpoint export of the source.
    fn export(
        &self,
        db: &DB,
        history: Uuid,
        export_parent: &std::path::Path,
        limits: TransferLimits,
    ) -> Result<OnlineCheckpointExport> {
        match self {
            Self::Primary => OnlineCheckpointExport::create(db, history, export_parent, limits),
            Self::Cascade { config } => {
                OnlineCheckpointExport::create_cascaded(db, history, export_parent, limits, config)
            }
        }
        .map_err(source_error)
    }

    /// The source's barrier, in the primary's sequence numbers: a primary's
    /// written sequence, a cascading standby's applied source sequence.
    fn barrier(&self, db: &DB) -> u64 {
        match self {
            Self::Primary => db.latest_sequence_number(),
            Self::Cascade { .. } => receiver_source_barrier(db).unwrap_or(0),
        }
    }

    /// One poll of the source's own WAL. `flushed` is the session's memo of
    /// the barrier its polls fsynced (spec P4.1d): the WAL is fsynced only
    /// when the barrier moved past it.
    fn poll(
        &self,
        db: &DB,
        history: Uuid,
        after: u64,
        byte_limit: usize,
        hint: &Mutex<CascadeHint>,
        flushed: &FlushMemo,
    ) -> std::result::Result<PhysicalPoll, PhysicalError> {
        match self {
            Self::Primary => poll_source(db, history, after, byte_limit, flushed),
            Self::Cascade { .. } => poll_source_cascade(db, history, after, byte_limit, &mut hint.lock(), flushed),
        }
    }

    /// Whether the source's WAL still holds the group after `after`. A
    /// cascading source probes with a poll, under the session's memo
    /// `flushed` (spec P4.1d).
    fn retains_after(
        &self,
        db: &DB,
        history: Uuid,
        after: u64,
        hint: &Mutex<CascadeHint>,
        flushed: &FlushMemo,
    ) -> bool {
        match self {
            Self::Primary => wal_retains_after(db, after).unwrap_or(false),
            Self::Cascade { .. } => self.poll(db, history, after, PROBE_POLL_BYTES, hint, flushed).is_ok(),
        }
    }

    /// The source, as a refusal names it.
    fn name(&self) -> &'static str {
        match self {
            Self::Primary => "this primary",
            Self::Cascade { .. } => "this cascading standby",
        }
    }

    /// The node's configuration, when the kind carries it: a fast-forward's
    /// leaf files are written with its storage options.
    fn storage_config(&self) -> Option<&Config> {
        match self {
            Self::Primary => None,
            Self::Cascade { config } => Some(config.as_ref()),
        }
    }
}

/// A poll's failure as the receiver must hear it: a WAL it no longer has, a
/// hole, a receiver ahead of it, a write group too large for any frame — each
/// a refusal the receiver needs recovery for. Anything else is a source-side failure (retried). `barrier` is the
/// source's, in the primary's sequence numbers. A pruned WAL reaches here only
/// when no archive was asked ([`poll_or_fill`] names both floors otherwise).
fn poll_error(barrier: u64, kind: &SourceKind, error: PhysicalError) -> WireError {
    match error {
        PhysicalError::Pruned { wanted, oldest } => pruned_refusal(kind, wanted, oldest, barrier, None),
        PhysicalError::Gap { expected, observed } => WireError::Refused(Refusal::new(
            RefuseCode::Gap {
                expected,
                have_from: observed,
                have_to: barrier,
            },
            format!(
                "{}'s physical WAL is not contiguous from the receiver's cursor",
                kind.name()
            ),
        )),
        PhysicalError::Ahead { after, barrier } => WireError::Refused(Refusal::new(
            RefuseCode::HistoryMismatch,
            format!(
                "the receiver is at sequence {after}, ahead of this primary's {barrier}: this primary is not the \
                 dataset the receiver followed (restored from an older copy?)"
            ),
        )),
        PhysicalError::HistoryMismatch => WireError::Refused(Refusal::new(
            RefuseCode::HistoryMismatch,
            "the receiver follows another dataset history",
        )),
        // Never servable, so never retried: the receiver needs recovery past
        // the group (a fast-forward or a full copy).
        PhysicalError::GroupTooLarge { first, bytes, limit } => WireError::Refused(Refusal::new(
            RefuseCode::GroupTooLarge {
                first,
                bytes: u64::try_from(bytes).unwrap_or(u64::MAX),
            },
            format!(
                "{}'s write group at sequence {first} is {bytes} bytes, more than the {limit} one replication frame \
                 can carry: it cannot be replicated; recover the standby past it (heliosdb-nano recover \
                 --from-primary) and load bulk data in smaller statements",
                kind.name()
            ),
        )),
        other => source_error(other),
    }
}

/// Spec P3.2 §1: the refusal of a receiver whose next groups are in neither
/// the source's RocksDB WAL nor its WAL archive. It names the first missing
/// range and both floors — where the archive starts and where the WAL starts
/// — so an operator knows which copied segments would bridge it.
fn pruned_refusal(
    kind: &SourceKind,
    wanted: u64,
    wal_floor: Option<u64>,
    barrier: u64,
    archive: Option<&ArchiveTail>,
) -> WireError {
    let oldest = wal_floor.unwrap_or_else(|| barrier.saturating_add(1));
    let until = oldest.saturating_sub(1).max(wanted);
    let from = archive
        .and_then(|tail| tail.reach(wanted))
        .map_or(wanted, |reach| reach.saturating_add(1));
    let to = archive
        .and_then(|tail| tail.next_first_after(from))
        .map_or(until, |first| first.saturating_sub(1).min(until));
    let wal = match wal_floor {
        Some(floor) => format!("from {floor}"),
        None => format!("nothing before {oldest}"),
    };
    let archived = match archive {
        None => "nothing (the WAL archive is off on this node)".to_string(),
        Some(tail) => match (tail.floor(), tail.unavailable()) {
            (Some(floor), _) => format!("from {floor}"),
            (None, Some(why)) => format!("nothing ({why})"),
            (None, None) => "nothing of this history".to_string(),
        },
    };
    WireError::Refused(Refusal::new(
        RefuseCode::WalPruned { oldest },
        format!(
            "the receiver needs sequence {wanted}; sequences {from}..{to} are in neither {source}'s RocksDB WAL nor \
             its WAL archive (the archive holds {archived}, RocksDB holds {wal}; retention: [replication.physical] \
             wal_retention_secs / wal_retention_size_mb, [replication.archive] recovery_window): rebuild the standby \
             from a new, empty data directory, or recover it from archived segments that cover the range \
             (heliosdb-nano recover --from-archive)",
            source = kind.name()
        ),
    ))
}

/// Spec P3.2 §1: a tail read's failure as the receiver must hear it. A
/// segment that does not verify, or a cursor that is not one of its group
/// boundaries, cannot be served from this archive: the receiver needs
/// recovery, told which file. (Spec P4.1c: a group larger than the poll's
/// limit is served alone, as from the WAL, never refused.)
fn tail_error(error: TailError, oldest: u64) -> WireError {
    match error {
        TailError::Segment(_) | TailError::Boundary { .. } => WireError::Refused(Refusal::new(
            RefuseCode::WalPruned { oldest },
            format!(
                "the WAL this receiver needs is pruned and the archive copy cannot be served: {error}; recover the \
                 standby from verified segments (heliosdb-nano recover --from-archive) or rebuild it"
            ),
        )),
    }
}

/// One poll as the session serves it (spec P3.2 §1): `attempt` is the poll of
/// the source's own WAL; when that WAL no longer holds the group after
/// `after` and the archive holds a contiguous run of this history's current
/// epoch from it to the WAL's floor, the groups come from the archive —
/// gathered up to `byte_limit` (a larger group alone, spec P4.1c), never past
/// the WAL's floor, so the next poll is
/// served from the WAL again once the cursor reaches it. Returns where the
/// groups came from.
///
/// The archive's groups past its sealed segments are still in the segment
/// being written: a receiver needing them is told to come back
/// (`SourceNotServing`, retried) rather than refused, unless the archive
/// records the range as missing.
#[allow(clippy::too_many_arguments)]
fn poll_or_fill(
    attempt: std::result::Result<PhysicalPoll, PhysicalError>,
    kind: &SourceKind,
    db: &DB,
    fill: Option<&ArchiveFill>,
    history: Uuid,
    after: u64,
    byte_limit: usize,
    flushed: &FlushMemo,
) -> Result<(PhysicalPoll, PollOrigin)> {
    let (wanted, wal_floor) = match attempt {
        Ok(poll) => return Ok((poll, PollOrigin::Wal)),
        Err(PhysicalError::Pruned { wanted, oldest }) => (wanted, oldest),
        Err(error) => return Err(poll_error(kind.barrier(db), kind, error)),
    };
    let barrier = kind.barrier(db);
    let Some(fill) = fill else {
        return Err(pruned_refusal(kind, wanted, wal_floor, barrier, None));
    };
    // As `poll_source` does: the barrier a poll reports is durable — the
    // barrier was sampled above, and the session's memo fsyncs once per
    // barrier (spec P4.1d).
    flushed.flush(db, barrier).map_err(source_error)?;
    let tail = fill.tail(history, EPOCH);
    let oldest = wal_floor.unwrap_or_else(|| barrier.saturating_add(1));
    // The archive serves up to the WAL's floor; the WAL the rest.
    let until = oldest.saturating_sub(1).min(barrier);
    let reach = tail.reach(wanted);
    if reach.is_none_or(|reach| reach < until) {
        let from = reach.map_or(wanted, |reach| reach.saturating_add(1));
        let unsealed = tail.archived_through() >= until
            && tail.next_first_after(from.saturating_sub(1)).is_none()
            && !tail.records_missing(from, until);
        if unsealed {
            return Err(not_serving(&format!(
                "sequences {from}..{until} are in this node's WAL archive but not in a sealed segment yet; the \
                 receiver is served once it is sealed"
            )));
        }
        return Err(pruned_refusal(kind, wanted, wal_floor, barrier, Some(&tail)));
    }
    let batches = tail
        .read(after, byte_limit, until)
        .map_err(|error| tail_error(error, oldest))?;
    let Some(through) = batches.last().map(PhysicalBatch::last_sequence) else {
        return Err(pruned_refusal(kind, wanted, wal_floor, barrier, Some(&tail)));
    };
    Ok((
        PhysicalPoll {
            batches,
            through,
            durable_barrier: barrier,
        },
        PollOrigin::Archive,
    ))
}

/// One shared budget per source endpoint, retained by all its providers.
/// A canceled wire session does not release capacity while its blocking job is
/// still running. Constructing a separate limiter for each session is invalid.
#[derive(Clone)]
pub struct SourceWorkLimiter(Arc<SourceWorkState>);
struct SourceWorkState {
    slots: Arc<tokio::sync::Semaphore>,
    max_jobs: u32,
    /// Spec P4.1 §3.2: a fast-forward's leaf planning runs under this ONE
    /// slot of its own, never a session's: a long plan cannot take the
    /// permits streaming standbys' polls need.
    planning: Arc<tokio::sync::Semaphore>,
    draining: AtomicBool,
}
// Field order is deliberate: if the async waiter is canceled, Tokio drops the
// result (including an export TempDir/DB Arc) BEFORE releasing its work permit.
struct SourceWorkOutput<T> {
    result: Option<Result<T>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
impl<T> SourceWorkOutput<T> {
    fn into_result(mut self) -> Result<T> {
        self.result
            .take()
            .unwrap_or_else(|| Err(source_error("source job output consumed twice")))
    }
}
impl SourceWorkLimiter {
    pub fn new(max_jobs: usize) -> Result<Self> {
        if !(1..=64).contains(&max_jobs) {
            return Err(source_error("source blocking job limit must be between 1 and 64"));
        }
        Ok(Self(Arc::new(SourceWorkState {
            slots: Arc::new(tokio::sync::Semaphore::new(max_jobs)),
            max_jobs: max_jobs as u32,
            planning: Arc::new(tokio::sync::Semaphore::new(1)),
            draining: AtomicBool::new(false),
        })))
    }
    /// Terminal admission stop. Await after joining wire sessions so every
    /// canceled/finished session's actual blocking work releases its resources
    /// before the source DB owner exits — a leaf planning under way included
    /// (told to stop, and joined). A timeout is not a completed drain.
    pub async fn drain(&self) -> Result<()> {
        self.0.draining.store(true, Ordering::Release);
        let all = self
            .0
            .slots
            .clone()
            .acquire_many_owned(self.0.max_jobs)
            .await
            .map_err(source_error)?;
        let planning = self.0.planning.clone().acquire_owned().await.map_err(source_error)?;
        drop(planning);
        drop(all);
        Ok(())
    }
    async fn run<T, F>(&self, job: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        // Draining = the primary is shutting down: the receiver comes back.
        if self.0.draining.load(Ordering::Acquire) {
            return Err(not_serving(
                "source work admission is draining: the primary is shutting down",
            ));
        }
        let permit = self
            .0
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| not_serving("source work admission closed: the primary is shutting down"))?;
        if self.0.draining.load(Ordering::Acquire) {
            return Err(not_serving(
                "source work admission is draining: the primary is shutting down",
            ));
        }
        tokio::task::spawn_blocking(move || {
            let permit = permit; // Also retained through result cleanup after cancellation.
            let result = job();
            SourceWorkOutput {
                result: Some(result),
                _permit: permit,
            }
        })
        .await
        .map_err(source_error)?
        .into_result()
    }

    /// Spec P4.1 §3.2: run a fast-forward's leaf planning (`guard`), which
    /// outlives the request that started it (its sessions ask for its
    /// outcome until it is ready), in a task of its own. The task waits for
    /// the source's ONE planning slot — never a session's work permit — and
    /// holds it on the blocking thread until the planning ends, so
    /// [`Self::drain`] joins it. The planning is told to stop early once the
    /// limiter drains or `abandoned` answers true. However the task ends —
    /// draining before it got the slot, the runtime shutting down, a panic —
    /// the guard is dropped with it and the plan is left failed, never
    /// planning.
    fn start_planning<A>(&self, guard: PlanningGuard, abandoned: A) -> Result<()>
    where
        A: Fn() -> bool + Send + 'static,
    {
        if self.0.draining.load(Ordering::Acquire) {
            drop(guard);
            return Err(not_serving(
                "source work admission is draining: the primary is shutting down",
            ));
        }
        let state = Arc::clone(&self.0);
        // Not awaited: the planning slot, held by the job, is what joins it.
        drop(tokio::spawn(async move {
            let Ok(permit) = state.planning.clone().acquire_owned().await else {
                return; // The guard drops: the plan fails.
            };
            if state.draining.load(Ordering::Acquire) {
                return;
            }
            let _ = tokio::task::spawn_blocking(move || {
                let _permit = permit; // Released only once the planning ended.
                let stop = || state.draining.load(Ordering::Acquire) || abandoned();
                guard.plan(&stop);
            })
            .await;
        }));
        Ok(())
    }
}

/// One leased export (see [`ExportLeases`]).
struct LeaseEntry {
    export: Arc<OnlineCheckpointExport>,
    /// Sessions serving it now.
    holders: usize,
    /// When it was leased (the order "oldest dropped" follows).
    leased_at: Instant,
    /// When its last session ended; `None` while one holds it.
    idle_since: Option<Instant>,
}

struct LeaseTable {
    lease: Duration,
    max_leases: usize,
    entries: Vec<LeaseEntry>,
}

impl LeaseTable {
    /// Remove every entry idle for longer than the lease; returns them (their
    /// checkpoint directories are removed when the last reference drops, so
    /// the caller drops them outside the lock, on a blocking thread).
    fn expire(&mut self, now: Instant) -> Vec<Arc<OnlineCheckpointExport>> {
        let lease = self.lease;
        let mut expired = Vec::new();
        self.entries.retain(|entry| {
            let keep = entry
                .idle_since
                .is_none_or(|idle| now.saturating_duration_since(idle) < lease);
            if !keep {
                expired.push(Arc::clone(&entry.export));
            }
            keep
        });
        expired
    }
}

/// Spec P2.2: the primary's leased checkpoint exports, shared by every
/// replication session of its port. A snapshot session's export is leased
/// for `lease` after the last session using it ends, keyed by snapshot id; a
/// receiver that reconnects with `OpenRequest::SnapshotOf` gets the same
/// export back while the lease holds. At most `max_leases` are kept (the
/// oldest is dropped first; a session still using a dropped export keeps it
/// until it ends). A leased checkpoint pins the SST files it hard-links.
#[derive(Clone)]
pub struct ExportLeases(Arc<Mutex<LeaseTable>>);

/// A session's hold on a leased export; its drop starts the lease.
struct LeaseHold {
    leases: ExportLeases,
    export: Arc<OnlineCheckpointExport>,
}

impl Drop for LeaseHold {
    fn drop(&mut self) {
        let snapshot = self.export.manifest().snapshot_id;
        let mut table = self.leases.0.lock();
        if let Some(entry) = table
            .entries
            .iter_mut()
            .find(|entry| entry.export.manifest().snapshot_id == snapshot)
        {
            entry.holders = entry.holders.saturating_sub(1);
            if entry.holders == 0 {
                entry.idle_since = Some(Instant::now());
            }
        }
    }
}

impl ExportLeases {
    /// `lease`: how long an export outlives its last session; `max_leases`
    /// (at least 1): how many are kept.
    pub fn new(lease: Duration, max_leases: usize) -> Self {
        Self(Arc::new(Mutex::new(LeaseTable {
            lease,
            max_leases: max_leases.max(1),
            entries: Vec::new(),
        })))
    }

    /// The snapshot ids leased now (diagnostics and tests).
    pub fn leased(&self) -> Vec<Uuid> {
        self.0
            .lock()
            .entries
            .iter()
            .map(|entry| entry.export.manifest().snapshot_id)
            .collect()
    }

    /// Drop every export whose lease ran out by `now`. The caller drops the
    /// returned exports — removing their checkpoint directories — off the
    /// async runtime.
    pub fn expire(&self, now: Instant) -> Vec<Arc<OnlineCheckpointExport>> {
        self.0.lock().expire(now)
    }

    /// Whether a session holds the leased export `snapshot` now. An export
    /// no longer in the table (its lease ran out, or it was dropped to stay
    /// within `max_leases`) counts as unheld.
    fn held(&self, snapshot: Uuid) -> bool {
        self.0
            .lock()
            .entries
            .iter()
            .any(|entry| entry.export.manifest().snapshot_id == snapshot && entry.holders > 0)
    }

    /// The leased exports, newest first, after expiring those whose lease
    /// ran out (returned second). The caller drops both off the lock.
    fn newest_first(&self, now: Instant) -> (Vec<Arc<OnlineCheckpointExport>>, Vec<Arc<OnlineCheckpointExport>>) {
        let mut table = self.0.lock();
        let expired = table.expire(now);
        let mut entries: Vec<(Instant, Arc<OnlineCheckpointExport>)> = table
            .entries
            .iter()
            .map(|entry| (entry.leased_at, Arc::clone(&entry.export)))
            .collect();
        drop(table);
        entries.sort_by_key(|(leased_at, _)| std::cmp::Reverse(*leased_at));
        (entries.into_iter().map(|(_, export)| export).collect(), expired)
    }

    /// The leased export `snapshot`, held for a new session, if its lease
    /// still holds.
    fn reuse(&self, snapshot: Uuid, now: Instant) -> (Option<LeaseHold>, Vec<Arc<OnlineCheckpointExport>>) {
        let mut table = self.0.lock();
        let expired = table.expire(now);
        let found = table
            .entries
            .iter_mut()
            .find(|entry| entry.export.manifest().snapshot_id == snapshot)
            .map(|entry| {
                entry.holders += 1;
                entry.idle_since = None;
                Arc::clone(&entry.export)
            });
        drop(table);
        let hold = found.map(|export| LeaseHold {
            leases: self.clone(),
            export,
        });
        (hold, expired)
    }

    /// Forget the export `snapshot` (it can no longer be served).
    fn forget(&self, snapshot: Uuid) -> Option<Arc<OnlineCheckpointExport>> {
        let mut table = self.0.lock();
        let position = table
            .entries
            .iter()
            .position(|entry| entry.export.manifest().snapshot_id == snapshot)?;
        Some(table.entries.remove(position).export)
    }

    /// Lease a new export, held by the session that created it. Returns the
    /// hold and the exports dropped to stay within `max_leases`.
    fn lease(
        &self,
        export: Arc<OnlineCheckpointExport>,
        now: Instant,
    ) -> (LeaseHold, Vec<Arc<OnlineCheckpointExport>>) {
        let mut table = self.0.lock();
        let mut dropped = table.expire(now);
        table.entries.push(LeaseEntry {
            export: Arc::clone(&export),
            holders: 1,
            leased_at: now,
            idle_since: None,
        });
        while table.entries.len() > table.max_leases {
            // Oldest first, preferring one no session uses.
            let victim = table
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.holders == 0)
                .min_by_key(|(_, entry)| entry.leased_at)
                .or_else(|| {
                    table
                        .entries
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, entry)| entry.leased_at)
                })
                .map(|(index, _)| index);
            match victim {
                Some(index) => dropped.push(table.entries.remove(index).export),
                None => break,
            }
        }
        drop(table);
        (
            LeaseHold {
                leases: self.clone(),
                export,
            },
            dropped,
        )
    }
}

struct SourceSession {
    db: Arc<DB>,
    history: Uuid,
    storage_identity: String,
    info: SourceSessionInfo,
    /// A snapshot session's export, held through its lease.
    export: Option<LeaseHold>,
    work: SourceWorkLimiter,
    kind: SourceKind,
    /// Spec P3.2 §1: where a gap the WAL no longer holds is served from.
    fill: Option<ArchiveFill>,
    /// A cascading session's positions in its store's own WAL.
    hint: Arc<Mutex<CascadeHint>>,
    /// Spec P4.1d: the barrier this session's polls last fsynced the store's
    /// WAL for. Its own: every session of the source keeps one, so a poll
    /// at a barrier the session already made durable fsyncs nothing.
    flushed: Arc<FlushMemo>,
    /// The last poll was served from the archive.
    from_archive: Arc<AtomicBool>,
    /// Spec P4.1: a fast-forward session's leaf plan (its export's).
    fast_forward: Option<Arc<FastForwardPlan>>,
}

/// A leased export the session may serve again: the same dataset and store,
/// its files intact, and the groups past its cut still served — from the WAL,
/// or from the archive (else the receiver would download it only to be
/// refused `WalPruned`).
/// `flushed` is the session's memo (spec P4.1d): a cascading source's probe
/// fsyncs only when its barrier moved.
fn reusable(
    db: &DB,
    kind: &SourceKind,
    fill: Option<&ArchiveFill>,
    export: &OnlineCheckpointExport,
    history: Uuid,
    storage_identity: &str,
    flushed: &FlushMemo,
) -> bool {
    let manifest = export.manifest();
    let cut = manifest.checkpoint_sequence;
    let continued = kind.retains_after(db, history, cut, &Mutex::new(CascadeHint::default()), flushed)
        || fill.is_some_and(|fill| {
            let tail = fill.tail(history, EPOCH);
            tail.reach(cut.saturating_add(1)).is_some()
        });
    manifest.history == history
        && manifest.storage_identity == storage_identity
        && export.manifest_bytes().is_ok()
        && continued
}

/// Open one source session. Resume does not inspect/create `export_parent`,
/// allocate a checkpoint, or reset a receiver cursor. Its first bounded poll
/// proves a contiguous prefix and durable source barrier; later polls recheck
/// continuity independently. Missing WAL always fails rather than reseeding.
///
/// A snapshot session serves the export `leases` still holds for an
/// `OpenRequest::SnapshotOf` (spec P2.2), or a new one, which it leases. A
/// snapshot lease retains files, not WAL. The caller owns listener admission,
/// simultaneous export caps, retention policy and disk budgets.
pub async fn open_primary_session(
    db: Arc<DB>,
    expected_history: Uuid,
    export_parent: PathBuf,
    limits: TransferLimits,
    request: OpenRequest,
    work: SourceWorkLimiter,
    leases: ExportLeases,
) -> Result<Arc<dyn PhysicalSource>> {
    open_source_session(
        db,
        expected_history,
        export_parent,
        limits,
        request,
        work,
        leases,
        SourceKind::Primary,
        None,
        None,
    )
    .await
}

/// [`open_primary_session`] of a primary that archives its WAL (spec P3.2
/// §1): a receiver whose next groups its RocksDB WAL no longer holds is
/// served them from `fill`.
#[allow(clippy::too_many_arguments)]
pub async fn open_primary_session_with(
    db: Arc<DB>,
    expected_history: Uuid,
    export_parent: PathBuf,
    limits: TransferLimits,
    request: OpenRequest,
    work: SourceWorkLimiter,
    leases: ExportLeases,
    fill: Option<ArchiveFill>,
) -> Result<Arc<dyn PhysicalSource>> {
    open_source_session(
        db,
        expected_history,
        export_parent,
        limits,
        request,
        work,
        leases,
        SourceKind::Primary,
        fill,
        None,
    )
    .await
}

/// [`open_primary_session_with`] of a primary whose node configuration is
/// `storage` (spec P4.1 §3.2): a fast-forward's leaf files are written with
/// its storage options — the engine's compression, filters and prefix
/// extractor — so an ingested leaf is laid out as the standby's own tables.
/// Without it they are written with the default configuration's.
#[allow(clippy::too_many_arguments)]
pub async fn open_primary_session_for(
    db: Arc<DB>,
    expected_history: Uuid,
    export_parent: PathBuf,
    limits: TransferLimits,
    request: OpenRequest,
    work: SourceWorkLimiter,
    leases: ExportLeases,
    fill: Option<ArchiveFill>,
    storage: Arc<Config>,
) -> Result<Arc<dyn PhysicalSource>> {
    open_source_session(
        db,
        expected_history,
        export_parent,
        limits,
        request,
        work,
        leases,
        SourceKind::Primary,
        fill,
        Some(storage),
    )
    .await
}

/// Spec P3.2 §4: a session of a CASCADING standby, serving its own store
/// (`db`, its writer's handle) to a downstream standby of the same history:
/// snapshots are cascaded exports (opened with `config`'s storage options),
/// polls strip the node-local records, and both are in the primary's sequence
/// numbers. `fill` is the standby's own WAL archive.
#[allow(clippy::too_many_arguments)]
pub async fn open_cascade_session(
    db: Arc<DB>,
    expected_history: Uuid,
    export_parent: PathBuf,
    limits: TransferLimits,
    request: OpenRequest,
    work: SourceWorkLimiter,
    leases: ExportLeases,
    fill: Option<ArchiveFill>,
    config: Arc<Config>,
) -> Result<Arc<dyn PhysicalSource>> {
    open_source_session(
        db,
        expected_history,
        export_parent,
        limits,
        request,
        work,
        leases,
        SourceKind::Cascade { config },
        fill,
        None,
    )
    .await
}

/// What [`leased_export`] needs of its session.
struct ExportRequest<'a> {
    db: &'a DB,
    kind: &'a SourceKind,
    fill: Option<&'a ArchiveFill>,
    leases: &'a ExportLeases,
    expected_history: Uuid,
    storage_identity: &'a str,
    export_parent: &'a std::path::Path,
    limits: TransferLimits,
    /// What a reused export serves, as the log names it.
    purpose: &'a str,
    /// A fast-forward of a receiver at this source sequence: with no
    /// preferred snapshot, any leased export it can use (reusable, its cut
    /// past the receiver) is served rather than a new checkpoint taken.
    fast_forward_after: Option<u64>,
    /// The session's memo of the barrier its polls fsynced (spec P4.1d).
    flushed: &'a FlushMemo,
}

/// Spec P4.1 §3.2: a fast-forward that names no snapshot takes the newest
/// leased export of its history that can still be continued from and whose
/// cut is past the receiver's `after` — one checkpoint, and one plan, shared
/// by the standbys recovering at once (and by a bootstrap copy's export) —
/// held for the session; `None` when there is none.
fn reuse_any_export(
    request: &ExportRequest<'_>,
    after: u64,
    released: &mut Vec<Arc<OnlineCheckpointExport>>,
) -> Option<LeaseHold> {
    let (candidates, expired) = request.leases.newest_first(Instant::now());
    released.extend(expired);
    for export in candidates {
        let manifest = export.manifest();
        let (snapshot, cut) = (manifest.snapshot_id, manifest.checkpoint_sequence);
        // Checked before a hold is taken, so a lease it passes over is left
        // as it was.
        if cut <= after
            || !reusable(
                request.db,
                request.kind,
                request.fill,
                &export,
                request.expected_history,
                request.storage_identity,
                request.flushed,
            )
        {
            continue;
        }
        let (reused, expired) = request.leases.reuse(snapshot, Instant::now());
        released.extend(expired);
        if let Some(hold) = reused {
            tracing::info!(
                "replication: serving the leased snapshot {snapshot} (cut {cut}) to {}",
                request.purpose
            );
            return Some(hold);
        }
    }
    None
}

/// The export a snapshot or fast-forward session serves: the leased export
/// `prefer` while its lease holds and it can still be continued from
/// ([`reusable`]); for a fast-forward naming none, any leased export it can
/// use ([`reuse_any_export`]); else a new one, which is leased. Exports
/// dropped by the lease table go to `released`, to be removed off its lock.
fn leased_export(
    request: &ExportRequest<'_>,
    prefer: Option<Uuid>,
    released: &mut Vec<Arc<OnlineCheckpointExport>>,
) -> Result<LeaseHold> {
    let reused = match prefer {
        Some(snapshot) => {
            let (reused, expired) = request.leases.reuse(snapshot, Instant::now());
            released.extend(expired);
            match reused {
                Some(reused)
                    if reusable(
                        request.db,
                        request.kind,
                        request.fill,
                        &reused.export,
                        request.expected_history,
                        request.storage_identity,
                        request.flushed,
                    ) =>
                {
                    tracing::info!(
                        "replication: serving the leased snapshot {snapshot} again ({})",
                        request.purpose
                    );
                    Some(reused)
                }
                Some(stale) => {
                    tracing::info!(
                        "replication: the leased snapshot {snapshot} can no longer be served (its WAL or files are \
                         gone); exporting a new one"
                    );
                    drop(stale);
                    released.extend(request.leases.forget(snapshot));
                    None
                }
                None => {
                    tracing::info!(
                        "replication: the snapshot {snapshot} a receiver resumes is no longer leased; exporting a new \
                         one"
                    );
                    None
                }
            }
        }
        None => request
            .fast_forward_after
            .and_then(|after| reuse_any_export(request, after, released)),
    };
    match reused {
        Some(hold) => Ok(hold),
        None => {
            let export = Arc::new(request.kind.export(
                request.db,
                request.expected_history,
                request.export_parent,
                request.limits,
            )?);
            let (hold, dropped) = request.leases.lease(export, Instant::now());
            released.extend(dropped);
            Ok(hold)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn open_source_session(
    db: Arc<DB>,
    expected_history: Uuid,
    export_parent: PathBuf,
    limits: TransferLimits,
    request: OpenRequest,
    work: SourceWorkLimiter,
    leases: ExportLeases,
    kind: SourceKind,
    fill: Option<ArchiveFill>,
    storage: Option<Arc<Config>>,
) -> Result<Arc<dyn PhysicalSource>> {
    let session_work = work.clone();
    let leases_for_planning = leases.clone();
    let session = work
        .run(move || {
            let storage_identity = kind.validate(&db, expected_history)?;
            let hint = Arc::new(Mutex::new(CascadeHint::default()));
            let flushed = Arc::new(FlushMemo::new());
            // Dropped (checkpoint directories removed) at the end of this
            // blocking job, never under the lease lock.
            let mut released = Vec::new();
            let mut fast_forward = None;
            let exports = ExportRequest {
                db: &db,
                kind: &kind,
                fill: fill.as_ref(),
                leases: &leases,
                expected_history,
                storage_identity: &storage_identity,
                export_parent: &export_parent,
                limits,
                purpose: "a resumed download",
                fast_forward_after: None,
                flushed: &flushed,
            };
            let (info, export) = match request {
                OpenRequest::Snapshot | OpenRequest::SnapshotOf { .. } => {
                    let prefer = match request {
                        OpenRequest::SnapshotOf { snapshot } => Some(snapshot),
                        _ => None,
                    };
                    let hold = leased_export(&exports, prefer, &mut released)?;
                    (SourceSessionInfo::Snapshot(hold.export.manifest().clone()), Some(hold))
                }
                OpenRequest::FastForward {
                    history,
                    after,
                    leaf_bytes,
                    prefer,
                } => {
                    if history != expected_history {
                        return Err(poll_error(kind.barrier(&db), &kind, PhysicalError::HistoryMismatch));
                    }
                    if !valid_leaf_bytes(leaf_bytes) {
                        return Err(WireError::Protocol(format!(
                        "a fast-forward asked for leaves of {leaf_bytes} bytes, outside the bounds both sides accept"
                    )));
                    }
                    let exports = ExportRequest {
                        purpose: if prefer.is_some() {
                            "a resumed fast-forward"
                        } else {
                            "a fast-forward"
                        },
                        fast_forward_after: Some(after),
                        ..exports
                    };
                    let hold = leased_export(&exports, prefer, &mut released)?;
                    let cut = hold.export.manifest().checkpoint_sequence;
                    if after >= cut {
                        return Err(WireError::Refused(Refusal::new(
                            RefuseCode::HistoryMismatch,
                            format!(
                            "the receiver at {after} is not behind the cut {cut}: {} is not the dataset it followed, \
                             or there is nothing to fast-forward",
                            kind.name()
                        ),
                        )));
                    }
                    let default_config;
                    let config = match (storage.as_deref(), kind.storage_config()) {
                        (Some(config), _) | (None, Some(config)) => config,
                        (None, None) => {
                            default_config = Config::default();
                            &default_config
                        }
                    };
                    let (plan, replaced) = hold.export.fast_forward_plan(leaf_bytes, config);
                    // A replaced plan (another leaf size's, or a failed one)
                    // is dropped here, off the export's lock; its leaf files
                    // go with its LAST reference, so a session still
                    // downloading them keeps them.
                    drop(replaced);
                    let info = SourceSessionInfo::FastForward {
                        session_id: Uuid::new_v4(),
                        snapshot_id: hold.export.manifest().snapshot_id,
                        history,
                        after,
                        sequence: cut,
                    };
                    fast_forward = Some(plan);
                    (info, Some(hold))
                }
                OpenRequest::Resume { history, after } => {
                    if history != expected_history {
                        return Err(poll_error(kind.barrier(&db), &kind, PhysicalError::HistoryMismatch));
                    }
                    // The first bounded poll proves the receiver's cursor is still
                    // served: retained (by the WAL, or by the archive), contiguous,
                    // not ahead of this source. One group proves it.
                    let attempt = kind.poll(&db, history, after, PROBE_POLL_BYTES, &hint, &flushed);
                    let (initial, _) = poll_or_fill(
                        attempt,
                        &kind,
                        &db,
                        fill.as_ref(),
                        history,
                        after,
                        PROBE_POLL_BYTES,
                        &flushed,
                    )?;
                    (
                        SourceSessionInfo::Resume {
                            session_id: Uuid::new_v4(),
                            history,
                            initial_after: after,
                            durable_barrier: initial.durable_barrier,
                        },
                        None,
                    )
                }
            };
            if kind.validate(&db, expected_history)? != storage_identity {
                return Err(source_error("source storage identity changed during session creation"));
            }
            drop(released);
            Ok(Arc::new(SourceSession {
                db,
                history: expected_history,
                storage_identity,
                info,
                export,
                work: session_work,
                kind,
                fill,
                hint,
                flushed,
                // Nothing served yet: the first poll says where it came from.
                from_archive: Arc::new(AtomicBool::new(false)),
                fast_forward,
            }))
        })
        .await?;
    // Spec P4.1 §3.2: the first fast-forward session of a plan starts its
    // planning, in a job that outlives this request and waits for the
    // source's planning slot; until it ends the session answers "planning".
    // It stops early when the source drains, or once no session holds the
    // plan's export any more (nobody would read the plan). Every way the
    // job can end without a plan leaves it failed (the guard), and the next
    // session gets a new one.
    if let Some(plan) = session.fast_forward.as_ref() {
        if let Some(guard) = plan.begin() {
            let snapshot = match &session.info {
                SourceSessionInfo::FastForward { snapshot_id, .. } => *snapshot_id,
                _ => return Err(source_error("a fast-forward plan on another kind of session")),
            };
            session
                .work
                .start_planning(guard, move || !leases_for_planning.held(snapshot))?;
        }
    }
    Ok(session as Arc<dyn PhysicalSource>)
}

#[async_trait]
impl PhysicalSource for SourceSession {
    fn session_info(&self) -> &SourceSessionInfo {
        &self.info
    }

    fn poll_origin(&self) -> PollOrigin {
        if self.from_archive.load(Ordering::Acquire) {
            PollOrigin::Archive
        } else {
            PollOrigin::Wal
        }
    }

    async fn read_chunk(&self, file_index: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
        let export = self
            .export
            .as_ref()
            .map(|hold| Arc::clone(&hold.export))
            .ok_or_else(|| source_error("resume sessions have no checkpoint chunks"))?;
        // Owning the Arc inside the blocking closure keeps immutable files alive
        // if the socket owner cancels or drops the surrounding session future.
        self.work
            .run(move || export.read_chunk(file_index, offset, length).map_err(source_error))
            .await
    }

    async fn poll(&self, after: u64, byte_limit: usize) -> Result<PhysicalPoll> {
        let db = self.db.clone();
        let history = self.history;
        let storage_identity = self.storage_identity.clone();
        let kind = self.kind.clone();
        let fill = self.fill.clone();
        let hint = Arc::clone(&self.hint);
        let flushed = Arc::clone(&self.flushed);
        let from_archive = Arc::clone(&self.from_archive);
        self.work
            .run(move || {
                if kind.validate(&db, history)? != storage_identity {
                    return Err(source_error("source storage identity changed before poll"));
                }
                // Re-checked at every poll: the WAL first, the archive only
                // for what the WAL no longer holds.
                let attempt = kind.poll(&db, history, after, byte_limit, &hint, &flushed);
                let (poll, origin) =
                    poll_or_fill(attempt, &kind, &db, fill.as_ref(), history, after, byte_limit, &flushed)?;
                if kind.validate(&db, history)? != storage_identity {
                    return Err(source_error("source storage identity changed during poll"));
                }
                let archived = origin == PollOrigin::Archive;
                if from_archive.swap(archived, Ordering::AcqRel) != archived {
                    if archived {
                        tracing::info!(
                            "replication: serving a receiver at sequence {after} from the WAL archive: {}'s RocksDB \
                             WAL no longer holds sequence {} (through {} this poll)",
                            kind.name(),
                            after.saturating_add(1),
                            poll.through
                        );
                    } else {
                        tracing::info!(
                            "replication: the receiver at sequence {after} is served from {}'s RocksDB WAL again",
                            kind.name()
                        );
                    }
                }
                Ok(poll)
            })
            .await
    }

    /// Spec P4.1 §3.2: from memory — the plan's page, a "planning" page
    /// while its blocking planning runs, or the refusal its failure gives.
    async fn leaf_digests(&self, from: u32, count: u32) -> Result<LeafDigests> {
        let plan = self.plan()?;
        let snapshot = self.fast_forward_snapshot()?;
        plan.page(snapshot, from, count).map_err(|reason| {
            WireError::Refused(Refusal::new(
                RefuseCode::SourceNotServing,
                format!("planning the fast-forward's leaves failed: {reason}"),
            ))
        })
    }

    async fn leaf(&self, index: u32) -> Result<LeafReady> {
        let plan = Arc::clone(self.plan()?);
        let snapshot = self.fast_forward_snapshot()?;
        self.work
            .run(move || plan.leaf(snapshot, index).map_err(source_error))
            .await
    }

    async fn read_leaf_chunk(&self, index: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
        let plan = Arc::clone(self.plan()?);
        self.work
            .run(move || plan.read_leaf_chunk(index, offset, length).map_err(source_error))
            .await
    }
}

impl SourceSession {
    /// A fast-forward session's leaf plan.
    fn plan(&self) -> Result<&Arc<FastForwardPlan>> {
        self.fast_forward
            .as_ref()
            .ok_or_else(|| source_error("only a fast-forward session has a leaf plan"))
    }

    fn fast_forward_snapshot(&self) -> Result<Uuid> {
        self.info
            .fast_forward_snapshot()
            .ok_or_else(|| source_error("only a fast-forward session has a leaf plan"))
    }
}

/// Spec P2.1 §2: the primary's standby registry as its replication sessions
/// see it — `ha_state()`'s, the one the commit path's sync wait reads. Every
/// admitted session registers its standby from the `Hello` and records each
/// heartbeat (= acknowledgement) into it; the session's end unregisters it.
///
/// Spec P3.2 §4: a cascading standby keeps the same registry of its
/// downstream standbys; its barrier is its applied source sequence.
pub struct StandbyRegistryObserver {
    barrier: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl StandbyRegistryObserver {
    /// `db` is the primary's store (its physical sequence is the barrier).
    pub fn new(db: &Arc<DB>) -> Self {
        let db = Arc::downgrade(db);
        Self::with_barrier(Box::new(move || {
            db.upgrade().map_or(0, |db| db.latest_sequence_number())
        }))
    }

    /// A registry whose barrier is read from `barrier` — a cascading
    /// standby's applied source sequence (spec P3.2 §4).
    pub fn with_barrier(barrier: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self { barrier }
    }
}

/// One admitted session, registered while it lives.
struct RegisteredSession(StandbyHandle);

impl ObservedSession for RegisteredSession {
    fn heartbeat(&mut self, heartbeat: &Heartbeat) {
        self.0
            .record_ack(heartbeat.applied_sequence, heartbeat.durable_sequence);
    }

    fn served(&mut self, origin: PollOrigin) {
        self.0.note_source(origin == PollOrigin::Archive);
    }
}

impl SessionObserver for StandbyRegistryObserver {
    fn barrier(&self) -> u64 {
        (self.barrier)()
    }

    fn admit(&self, peer: Option<SocketAddr>, hello: &Hello) -> Box<dyn ObservedSession> {
        // A resumed session follows this primary from a durable cursor; a
        // snapshot session is still installing its copy. Only a serving
        // standby that follows counts toward `min_acks`. Spec P4.1: an
        // offline receiver (`resync`, `recover --from-primary`, role
        // `Resync`) is rebuilding a copy, whatever cursor it announced: it is
        // listed as bootstrapping and never counts.
        let following = hello.cursor.is_some() && hello.role != PeerRole::Resync;
        Box::new(RegisteredSession(ha_state().register_standby(StandbyRegistration {
            node_id: hello.node_id,
            address: peer.map_or_else(|| "unknown".to_string(), |peer| peer.to_string()),
            history: hello.history,
            pg_port: hello.pg_port,
            sync_mode: match hello.sync_mode {
                WireSyncMode::Async => SyncMode::Async,
                WireSyncMode::SemiSync => SyncMode::SemiSync,
                WireSyncMode::Sync => SyncMode::Sync,
            },
            cursor: hello.cursor,
            state: if following {
                StandbyState::Streaming
            } else {
                StandbyState::Bootstrapping
            },
            counts_for_sync: following && hello.role == PeerRole::Standby,
        })))
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::replication::history::{load_or_create_primary_history, PRIMARY_HISTORY_FILE};
    use crate::replication::physical::MAX_BATCH_BYTES;
    use rocksdb::Options;

    fn primary(root: &std::path::Path) -> (Arc<DB>, Uuid) {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_wal_ttl_seconds(3600);
        let db = Arc::new(DB::open(&options, root.join("primary")).unwrap());
        db.put(b"one", b"before").unwrap();
        let history = load_or_create_primary_history(db.path()).unwrap();
        (db, history)
    }

    fn leases() -> ExportLeases {
        ExportLeases::new(Duration::from_secs(3600), 4)
    }

    /// Review P4.1d: a write group larger than one frame can carry reaches
    /// the receiver as a typed refusal it does not retry (it needs recovery
    /// past the group), naming the group's sequence and size — not as the
    /// "source not serving" a malformed batch would be, retried forever.
    #[test]
    fn a_group_too_large_for_a_frame_is_refused_by_name_and_never_retried() {
        let error = PhysicalError::GroupTooLarge {
            first: 41,
            bytes: MAX_BATCH_BYTES + 1,
            limit: MAX_BATCH_BYTES,
        };
        let WireError::Refused(refusal) = poll_error(100, &SourceKind::Primary, error) else {
            panic!("not a refusal");
        };
        assert_eq!(
            refusal.code,
            RefuseCode::GroupTooLarge {
                first: 41,
                bytes: (MAX_BATCH_BYTES + 1) as u64,
            }
        );
        assert!(refusal.code.needs_recovery() && !refusal.code.retryable());
        assert!(refusal.reason.contains("sequence 41"), "{}", refusal.reason);
    }

    #[tokio::test]
    async fn source_resume_keeps_older_cursor_without_creating_a_new_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let work = SourceWorkLimiter::new(2).unwrap();
        let snapshot = open_primary_session(
            db.clone(),
            history,
            root.path().to_path_buf(),
            TransferLimits::default(),
            OpenRequest::Snapshot,
            work.clone(),
            leases(),
        )
        .await
        .unwrap();
        let cut = match snapshot.session_info() {
            SourceSessionInfo::Snapshot(manifest) => {
                assert_eq!(manifest.history, history);
                assert_eq!(manifest.checkpoint_sequence, 1);
                manifest.checkpoint_sequence
            }
            _ => panic!("snapshot must return a checkpoint"),
        };
        db.put(b"two", b"after").unwrap();
        drop(snapshot);
        let absent_export_parent = root.path().join("must-not-be-created");
        let resumed = open_primary_session(
            db.clone(),
            history,
            absent_export_parent.clone(),
            TransferLimits::default(),
            OpenRequest::Resume { history, after: cut },
            work.clone(),
            leases(),
        )
        .await
        .unwrap();
        match resumed.session_info() {
            SourceSessionInfo::Resume {
                session_id,
                history: actual,
                initial_after,
                durable_barrier,
            } => {
                assert!(!session_id.is_nil());
                assert_eq!(*actual, history);
                assert_eq!(*initial_after, cut);
                assert_eq!(*durable_barrier, 2);
            }
            _ => panic!("resume must not create a checkpoint"),
        }
        assert!(!absent_export_parent.exists());
        assert!(resumed.read_chunk(0, 0, 1).await.is_err());
        let poll = resumed.poll(cut, MAX_BATCH_BYTES).await.unwrap();
        assert_eq!(poll.batches.len(), 1);
        assert_eq!(poll.batches[0].first_sequence(), 2);
        assert_eq!(poll.through, 2);
    }

    #[tokio::test]
    async fn source_resume_refuses_wrong_history_future_cursor_and_changed_metadata() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let work = SourceWorkLimiter::new(2).unwrap();
        // Spec P2.1: both are refusals the receiver needs recovery for — a
        // cursor ahead of the primary means the primary is not its dataset.
        for request in [
            OpenRequest::Resume {
                history: Uuid::new_v4(),
                after: 1,
            },
            OpenRequest::Resume { history, after: 2 },
        ] {
            let refused = open_primary_session(
                db.clone(),
                history,
                root.path().to_path_buf(),
                TransferLimits::default(),
                request,
                work.clone(),
                leases(),
            )
            .await;
            assert!(
                matches!(&refused, Err(WireError::Refused(refusal)) if refusal.code == RefuseCode::HistoryMismatch),
                "{request:?}: {:?}",
                refused.err()
            );
        }
        let source = open_primary_session(
            db.clone(),
            history,
            root.path().to_path_buf(),
            TransferLimits::default(),
            OpenRequest::Resume { history, after: 1 },
            work.clone(),
            leases(),
        )
        .await
        .unwrap();
        std::fs::write(db.path().join(PRIMARY_HISTORY_FILE), b"{}").unwrap();
        assert!(source.poll(1, MAX_BATCH_BYTES).await.is_err());
    }

    async fn snapshot(
        db: &Arc<DB>,
        history: Uuid,
        root: &std::path::Path,
        request: OpenRequest,
        work: &SourceWorkLimiter,
        leases: &ExportLeases,
    ) -> (Arc<dyn PhysicalSource>, Uuid) {
        let session = open_primary_session(
            db.clone(),
            history,
            root.to_path_buf(),
            TransferLimits::default(),
            request,
            work.clone(),
            leases.clone(),
        )
        .await
        .unwrap();
        let id = session.session_info().session_id();
        (session, id)
    }

    /// The checkpoint directories under the export parent.
    fn exports(root: &std::path::Path) -> usize {
        std::fs::read_dir(root)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("nano-checkpoint-")
            })
            .count()
    }

    /// Spec P2.2: a snapshot session's export outlives the session by its
    /// lease, and `SnapshotOf` gets the same export back; once the lease ran
    /// out the export (and its checkpoint directory) is gone and the same
    /// request gets a new snapshot.
    #[tokio::test]
    async fn a_leased_export_serves_the_same_snapshot_until_its_lease_expires() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let work = SourceWorkLimiter::new(2).unwrap();
        let leases = ExportLeases::new(Duration::from_secs(3600), 4);
        let (first, id) = snapshot(&db, history, root.path(), OpenRequest::Snapshot, &work, &leases).await;
        let manifest = first.session_info().manifest().unwrap().clone();
        let chunk = first.read_chunk(0, 0, 1).await.unwrap();
        drop(first);
        assert_eq!(leases.leased(), vec![id], "kept after its session ended");
        assert_eq!(exports(root.path()), 1);
        // A held export never expires, however late it is.
        let (again, again_id) = snapshot(
            &db,
            history,
            root.path(),
            OpenRequest::SnapshotOf { snapshot: id },
            &work,
            &leases,
        )
        .await;
        assert_eq!(again_id, id, "the reconnecting receiver gets the same snapshot");
        assert_eq!(
            serde_json::to_vec(again.session_info().manifest().unwrap()).unwrap(),
            serde_json::to_vec(&manifest).unwrap(),
            "byte for byte the same manifest"
        );
        assert_eq!(again.read_chunk(0, 0, 1).await.unwrap(), chunk);
        assert!(leases.expire(Instant::now() + Duration::from_secs(7200)).is_empty());
        assert_eq!(exports(root.path()), 1, "one export, shared");
        drop(again);
        let expired = leases.expire(Instant::now() + Duration::from_secs(7200));
        assert_eq!(expired.len(), 1);
        drop(expired);
        assert!(leases.leased().is_empty());
        assert_eq!(
            exports(root.path()),
            0,
            "an expired export's checkpoint directory is removed"
        );
        let (fresh, fresh_id) = snapshot(
            &db,
            history,
            root.path(),
            OpenRequest::SnapshotOf { snapshot: id },
            &work,
            &leases,
        )
        .await;
        assert_ne!(fresh_id, id, "an expired lease gets a new snapshot");
        assert_eq!(leases.leased(), vec![fresh_id]);
        drop(fresh);
    }

    /// At most `max_leases` exports are kept: the oldest goes first.
    #[tokio::test]
    async fn leased_exports_are_bounded_and_the_oldest_is_dropped() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let work = SourceWorkLimiter::new(2).unwrap();
        let leases = ExportLeases::new(Duration::from_secs(3600), 2);
        let mut ids = Vec::new();
        for _ in 0..3 {
            let (session, id) = snapshot(&db, history, root.path(), OpenRequest::Snapshot, &work, &leases).await;
            drop(session);
            ids.push(id);
        }
        assert_eq!(leases.leased(), ids[1..].to_vec());
        assert_eq!(exports(root.path()), 2, "the dropped export's directory is gone");
        // A lease of 0 keeps an export only while a session uses it.
        let none = ExportLeases::new(Duration::ZERO, 4);
        let (session, _) = snapshot(&db, history, root.path(), OpenRequest::Snapshot, &work, &none).await;
        assert!(none.expire(Instant::now()).is_empty(), "held");
        drop(session);
        assert_eq!(none.expire(Instant::now()).len(), 1);
    }

    #[tokio::test]
    async fn canceled_source_job_keeps_shared_capacity_until_actual_completion() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        assert!(SourceWorkLimiter::new(0).is_err());
        assert!(SourceWorkLimiter::new(65).is_err());
        let work = SourceWorkLimiter::new(1).unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_work = work.clone();
        let first = tokio::spawn(async move {
            first_work
                .run(move || {
                    let _ = started_tx.send(());
                    // Sender drop also releases this private fixture on panic.
                    let _ = release_rx.recv();
                    Ok(())
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let ran = Arc::new(AtomicBool::new(false));
        let observed = ran.clone();
        let second_work = work.clone();
        let mut second = tokio::spawn(async move {
            second_work
                .run(move || {
                    observed.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await
        });
        assert!(tokio::time::timeout(Duration::from_millis(30), &mut second)
            .await
            .is_err());
        assert!(!ran.load(Ordering::SeqCst));
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn source_drain_joins_canceled_jobs_and_permanently_refuses_new_work() {
        use std::time::Duration;
        let work = SourceWorkLimiter::new(1).unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_work = work.clone();
        let first = tokio::spawn(async move {
            first_work
                .run(move || {
                    let _ = started_tx.send(());
                    let _ = release_rx.recv();
                    Ok(())
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let draining = work.clone();
        let mut drain = tokio::spawn(async move { draining.drain().await });
        assert!(tokio::time::timeout(Duration::from_millis(30), &mut drain)
            .await
            .is_err());
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), drain)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(work
            .run(|| -> Result<()> { panic!("work ran after source drain") })
            .await
            .is_err());
        work.drain().await.unwrap(); // Repeated drain does not reopen admission.
    }

    #[tokio::test]
    async fn canceled_job_output_cleanup_remains_inside_the_work_budget() {
        use std::time::Duration;
        struct CleanupProbe {
            started: Option<tokio::sync::oneshot::Sender<()>>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl Drop for CleanupProbe {
            fn drop(&mut self) {
                if let Some(started) = self.started.take() {
                    let _ = started.send(());
                }
                let _ = self.release.recv();
            }
        }
        let work = SourceWorkLimiter::new(1).unwrap();
        let (job_started_tx, job_started_rx) = tokio::sync::oneshot::channel();
        let (job_finish_tx, job_finish_rx) = std::sync::mpsc::channel();
        let (cleanup_started_tx, cleanup_started_rx) = tokio::sync::oneshot::channel();
        let (cleanup_release_tx, cleanup_release_rx) = std::sync::mpsc::channel();
        let first_work = work.clone();
        let first = tokio::spawn(async move {
            first_work
                .run(move || {
                    let _ = job_started_tx.send(());
                    let _ = job_finish_rx.recv();
                    Ok(CleanupProbe {
                        started: Some(cleanup_started_tx),
                        release: cleanup_release_rx,
                    })
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), job_started_rx)
            .await
            .unwrap()
            .unwrap();
        first.abort();
        assert!(matches!(first.await, Err(error) if error.is_cancelled()));
        job_finish_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), cleanup_started_rx)
            .await
            .unwrap()
            .unwrap();
        let draining = work.clone();
        let mut drain = tokio::spawn(async move { draining.drain().await });
        assert!(tokio::time::timeout(Duration::from_millis(30), &mut drain)
            .await
            .is_err());
        cleanup_release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), drain)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    /// Seal `batches` into one segment of `archive` (the archiver's layout).
    fn archive_groups(archive: &std::path::Path, history: Uuid, batches: &[PhysicalBatch]) {
        use crate::replication::archive_segment::{segment_dir, unix_now, OpenSegment, SegmentHeader};
        let header = SegmentHeader {
            flags: 0,
            history,
            epoch: EPOCH,
            first_sequence: batches[0].first_sequence(),
            created_at: unix_now(),
            node_id: Uuid::new_v4(),
            key_check: [0; 32],
        };
        let mut open = OpenSegment::create(&segment_dir(archive, history, EPOCH), header).unwrap();
        for batch in batches {
            open.append(batch, unix_now(), None).unwrap();
        }
        open.seal().unwrap();
    }

    /// A primary whose groups 1..=7 were written, archived as `split` says,
    /// then flushed out of its WAL (what retention does), and one more group
    /// (8) written after: its WAL holds from 8 only.
    fn pruned_primary(root: &std::path::Path, split: &[std::ops::Range<usize>]) -> (Arc<DB>, Uuid) {
        let path = root.join("primary");
        let history = {
            let (db, history) = primary(root);
            for index in 0..6 {
                db.put(format!("k{index}"), b"v").unwrap();
            }
            let polled = poll_source(&db, history, 0, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
            assert_eq!(polled.through, 7);
            for range in split {
                archive_groups(&root.join("archive"), history, &polled.batches[range.clone()]);
            }
            db.flush().unwrap();
            history
        };
        // Every archived WAL file and every log but the newest goes.
        if let Ok(entries) = std::fs::read_dir(path.join("archive")) {
            for entry in entries {
                std::fs::remove_file(entry.unwrap().path()).unwrap();
            }
        }
        let mut logs: Vec<_> = std::fs::read_dir(&path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
            .collect();
        logs.sort();
        logs.pop();
        for log in logs {
            std::fs::remove_file(log).unwrap();
        }
        let mut options = Options::default();
        options.set_wal_ttl_seconds(3600);
        let db = Arc::new(DB::open(&options, &path).unwrap());
        db.put(b"after", b"purge").unwrap();
        assert_eq!(db.latest_sequence_number(), 8);
        (db, history)
    }

    async fn resume_at(
        db: &Arc<DB>,
        history: Uuid,
        root: &std::path::Path,
        after: u64,
        fill: Option<ArchiveFill>,
    ) -> Result<Arc<dyn PhysicalSource>> {
        open_primary_session_with(
            db.clone(),
            history,
            root.to_path_buf(),
            TransferLimits::default(),
            OpenRequest::Resume { history, after },
            SourceWorkLimiter::new(2).unwrap(),
            leases(),
            fill,
        )
        .await
    }

    /// Spec P3.2 §1: a receiver whose next groups the WAL no longer holds is
    /// served them from the archive, up to the WAL's floor, then from the WAL
    /// again; the session says where each poll came from.
    #[tokio::test]
    async fn a_pruned_wal_is_served_from_the_archive_up_to_the_wal_floor_then_from_the_wal() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = pruned_primary(root.path(), std::slice::from_ref(&(0..7)));
        let fill = ArchiveFill::directory(root.path().join("archive"), None);
        let session = resume_at(&db, history, root.path(), 2, Some(fill)).await.unwrap();
        match session.session_info() {
            SourceSessionInfo::Resume { durable_barrier, .. } => assert_eq!(*durable_barrier, 8),
            _ => panic!("a resume session"),
        }
        assert_eq!(session.poll_origin(), PollOrigin::Wal, "nothing served yet");
        let archived = session.poll(2, MAX_BATCH_BYTES).await.unwrap();
        assert_eq!(archived.batches.first().unwrap().first_sequence(), 3);
        assert_eq!(archived.through, 7, "the archive serves up to the WAL's floor");
        assert_eq!(archived.durable_barrier, 8);
        assert_eq!(session.poll_origin(), PollOrigin::Archive);
        let live = session.poll(7, MAX_BATCH_BYTES).await.unwrap();
        assert_eq!(live.batches.len(), 1);
        assert_eq!(live.batches[0].first_sequence(), 8);
        assert_eq!(session.poll_origin(), PollOrigin::Wal, "back on the WAL");
    }

    /// Spec P3.2 §1: without an archive, or with a hole in it, the receiver
    /// is refused `WalPruned`, naming the missing range and both floors.
    #[tokio::test]
    async fn a_gap_neither_the_wal_nor_the_archive_holds_is_refused_naming_both_floors() {
        let root = tempfile::tempdir().unwrap();
        // Groups 1..=3 and 6..=7 archived: 4..=5 are nowhere.
        let (db, history) = pruned_primary(root.path(), &[0..3, 5..7]);
        let refusal = |result: Result<Arc<dyn PhysicalSource>>| match result {
            Err(WireError::Refused(refusal)) => refusal,
            Err(other) => panic!("not a refusal: {other}"),
            Ok(_) => panic!("served"),
        };
        let off = refusal(resume_at(&db, history, root.path(), 2, None).await);
        assert_eq!(off.code, RefuseCode::WalPruned { oldest: 8 });
        assert!(off.reason.contains("sequences 3..7"), "{}", off.reason);
        assert!(off.reason.contains("RocksDB holds from 8"), "{}", off.reason);
        assert!(off.reason.contains("the WAL archive is off"), "{}", off.reason);
        assert!(off.reason.contains("rebuild the standby"), "{}", off.reason);

        let fill = ArchiveFill::directory(root.path().join("archive"), None);
        let hole = refusal(resume_at(&db, history, root.path(), 2, Some(fill.clone())).await);
        assert_eq!(hole.code, RefuseCode::WalPruned { oldest: 8 });
        assert!(hole.reason.contains("sequences 4..5"), "{}", hole.reason);
        assert!(hole.reason.contains("the archive holds from 1"), "{}", hole.reason);
        assert!(hole.reason.contains("RocksDB holds from 8"), "{}", hole.reason);
        // Past the hole, the archive covers the rest: served.
        let session = resume_at(&db, history, root.path(), 5, Some(fill)).await.unwrap();
        let archived = session.poll(5, MAX_BATCH_BYTES).await.unwrap();
        assert_eq!(archived.batches[0].first_sequence(), 6);
        assert_eq!(archived.through, 7);
    }

    async fn fast_forward_at(
        db: &Arc<DB>,
        history: Uuid,
        root: &std::path::Path,
        after: u64,
        prefer: Option<Uuid>,
        work: &SourceWorkLimiter,
        leases: &ExportLeases,
    ) -> Result<Arc<dyn PhysicalSource>> {
        open_primary_session(
            db.clone(),
            history,
            root.to_path_buf(),
            TransferLimits::default(),
            OpenRequest::FastForward {
                history,
                after,
                leaf_bytes: super::super::physical_wire::MIN_LEAF_BYTES,
                prefer,
            },
            work.clone(),
            leases.clone(),
        )
        .await
    }

    /// Spec P4.1 §3.2: a fast-forward session leases an export like a
    /// snapshot, answers "planning" until its plan is built on a blocking
    /// thread, then serves leaves whose files, ingested anywhere, hold the
    /// checkpoint's default column family exactly; a reconnect preferring
    /// the export gets the same cut; a receiver at the cut is refused.
    #[tokio::test]
    async fn a_fast_forward_session_plans_its_export_and_builds_its_leaf_files() {
        use super::super::physical::range_digest;
        use super::super::physical_wire::{LeafStatus, MAX_LEAVES};
        use sha2::Digest as _;
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        for index in 0..400_u32 {
            db.put(format!("row{index:05}"), vec![index as u8; 600]).unwrap();
        }
        let cut = db.latest_sequence_number();
        let work = SourceWorkLimiter::new(2).unwrap();
        let leases = leases();
        let refused = fast_forward_at(&db, history, root.path(), cut, None, &work, &leases).await;
        assert!(
            matches!(&refused, Err(WireError::Refused(refusal)) if refusal.code == RefuseCode::HistoryMismatch),
            "{:?}",
            refused.err()
        );
        let session = fast_forward_at(&db, history, root.path(), 1, None, &work, &leases)
            .await
            .unwrap();
        let snapshot_id = session.session_info().fast_forward_snapshot().unwrap();
        assert!(matches!(
            session.session_info(),
            SourceSessionInfo::FastForward { sequence, after: 1, .. } if *sequence == cut
        ));
        let deadline = Instant::now() + Duration::from_secs(30);
        let page = loop {
            let page = session.leaf_digests(0, MAX_LEAVES).await.unwrap();
            if page.status == LeafStatus::Ready {
                break page;
            }
            assert!(Instant::now() < deadline, "the plan never became ready");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(page.leaf_count >= 3, "{} leaves", page.leaf_count);
        assert_eq!(page.entries.len(), page.leaf_count as usize);
        assert!(session.leaf(page.leaf_count).await.is_err(), "past the plan");

        // Every leaf's file, fetched in chunks and ingested into a scratch
        // store, rebuilds the checkpoint's default column family.
        let scratch_path = root.path().join("scratch");
        let mut options = Options::default();
        options.create_if_missing(true);
        let scratch = DB::open(&options, &scratch_path).unwrap();
        for index in 0..page.leaf_count {
            let ready = session.leaf(index).await.unwrap();
            assert_eq!(ready.index, index);
            assert_eq!(session.leaf(index).await.unwrap(), ready, "built once");
            if ready.size == 0 {
                continue;
            }
            let mut bytes = Vec::new();
            while (bytes.len() as u64) < ready.size {
                let length = (ready.size - bytes.len() as u64).min(64 * 1024) as u32;
                bytes.extend(
                    session
                        .read_leaf_chunk(index, bytes.len() as u64, length)
                        .await
                        .unwrap(),
                );
            }
            assert_eq!(<[u8; 32]>::from(sha2::Sha256::digest(&bytes)), ready.sha256);
            let file = root.path().join(format!("fetched-{index}.sst"));
            std::fs::write(&file, &bytes).unwrap();
            scratch.ingest_external_file(vec![file]).unwrap();
        }
        assert_eq!(
            range_digest(&scratch, None, b"").unwrap(),
            range_digest(&db, None, b"").unwrap(),
            "the leaves hold the cut exactly (nothing was written since)"
        );

        // A reconnect preferring the export gets the same cut and plan.
        drop(session);
        let again = fast_forward_at(&db, history, root.path(), 1, Some(snapshot_id), &work, &leases)
            .await
            .unwrap();
        assert_eq!(again.session_info().fast_forward_snapshot(), Some(snapshot_id));
        let page_again = again.leaf_digests(0, MAX_LEAVES).await.unwrap();
        assert_eq!(page_again, page);
        // Its leaf requests are fast-forward only: a snapshot session has none.
        let (plain, _) = snapshot(&db, history, root.path(), OpenRequest::Snapshot, &work, &leases).await;
        assert!(plain.leaf_digests(0, 1).await.is_err());
    }
}
