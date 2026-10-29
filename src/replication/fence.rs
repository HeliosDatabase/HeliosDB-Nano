//! Fencing a superseded primary (spec P5.1 §3, §11, plan
//! `PHYSICAL_STANDBY_REPLICATION.md` §D7, `REPLICATION_4_41_1_DESIGN.md`
//! §3.4).
//!
//! # Why
//!
//! A promotion makes a standby the primary of its dataset at a higher epoch.
//! The old primary must stop writing first, or both nodes accept writes and
//! the dataset forks — the split brain no proxy can repair afterwards. The
//! strongest form of fencing is the graceful one: the promoting standby asks
//! the old primary on its own replication port (`Fence`), the old primary
//! stops writing, makes everything it committed durable, and answers with
//! its final sequence (`Fenced`); the standby tails it through that sequence
//! before it converts its store, so nothing committed is lost. The second
//! form is wire-level: a primary that hears a `Hello` of a higher epoch from
//! any node — the old primary restarted after a forced promotion, say —
//! learns it is superseded and fences itself the same way.
//!
//! # What a fence does ([`PrimaryFence`], one per primary process)
//!
//! 1. Closes the write gate (`WriteGate::fence`): every write is refused from
//!    here on with the fence's text, SQLSTATE 25006 on the wire — and marks
//!    the node read-only (`ha_state`) and `fencing`
//!    (`standby_posture::note_fencing`, spec §11.D): `/ready` 503,
//!    `heliosdb_require_ready()` 55000, and the PostgreSQL wire ends every
//!    session opened before the fence (FATAL 57P01): pushed at once through
//!    the session registry (spec P5.2 §1), and — the backstop, for a session
//!    mid-statement — at its next statement.
//! 2. Waits for the writes that were already past the gate, fsyncs the WAL
//!    and reads the store's sequence: the final sequence.
//! 3. Records the fence durably — [`FENCED_FILE`] in the data directory,
//!    written whole or not at all (`history::write_new_record_in`) — so the
//!    node cannot come back writable: every later open of the directory, in
//!    any build and through any entry point, is fenced from its first second
//!    (`StorageEngine::open`, spec §11.A), and it is rebuilt as a standby by
//!    `recover --from-primary --replace`.
//! 4. Marks the node `fenced` (`standby_posture::note_fenced`).
//! 5. Asks the archiver to archive THROUGH the final sequence and seal its
//!    open segment (spec §11.B), waiting a bounded time for it: what the old
//!    primary committed is then in sealed segments that `recover
//!    --from-archive` and other nodes' archive fills read. The archiver keeps
//!    running, idle — nothing more is written. The replication port keeps
//!    serving resume sessions: the promoting standby tails it, and its other
//!    standbys may finish what they were applying.
//! 6. Logs one ERROR line naming the epoch, the fencing promotion and the
//!    final sequence.
//!
//! If step 2 or 3 fails the fence is INCOMPLETE (spec §11.D): writes stay
//! refused, the node stays `fencing` and not ready with "fence incomplete"
//! in its reason, sessions are still ended lazily; the promotion that asked
//! retries (or is forced). No record was written then, so a restart of the
//! node undoes the incomplete fence — the log says so.
//!
//! The whole fence runs on one blocking thread under one lock, so a session
//! that gives up waiting cannot cancel it half-way, and two fences cannot
//! interleave. The first fence wins: a repeated fence by the same promotion
//! gets the same answer (a promotion re-run after a crash between the fence
//! and its own record), a fence by another one is refused naming the first —
//! a second promotion of the same epoch would fork the dataset.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use parking_lot::Mutex;
use tracing::{error, info, warn};
use uuid::Uuid;

use super::archiver::ArchiveService;
use super::history::{read_record_in, write_new_record_in, HistoryError};
use super::physical_wire::{Fenced, Refusal, RefuseCode, SourceFence};
use crate::standby_posture::{
    format_unix_secs, note_fence_incomplete, note_fenced, note_fencing, primary_fenced_write_message, FenceStatus,
};
use crate::storage::replication_records::fenced_rebuild_hint;
pub use crate::storage::replication_records::{FencedRecord, FENCED_FILE};
use crate::storage::StorageEngine;

const FORMAT: u32 = crate::storage::replication_records::RECORD_FORMAT;
/// How long a fence waits for the writes that were past the gate when it
/// closed (a write stuck on a stalled disk): the fence is then incomplete,
/// the gate stays closed, and the promoting standby retries or forces.
/// Public: an online promotion waits for a fence's answer at least this
/// long plus [`ARCHIVE_SEAL_TIMEOUT`] and one frame (spec P5.2 §8.B).
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Spec P5.1 §11.B: how long a fence waits for the archiver to archive
/// through the final sequence and seal. Past it the fence still answers —
/// the archive is the old primary's copy of what the promoted standby already
/// tails — and a WARNING says how far the archive got.
pub const ARCHIVE_SEAL_TIMEOUT: Duration = Duration::from_secs(15);

/// The archiver of a primary, shared by the fence (which asks it to seal
/// through the final sequence) and the replication port (which stops it at
/// shutdown).
pub type SharedArchive = Arc<Mutex<Option<ArchiveService>>>;

/// The fence record of the data directory `dir`, if it carries one. A file
/// that is there but does not decode is an error: it still means the
/// directory was fenced.
pub fn read_fenced_record(dir: &Path) -> Result<Option<FencedRecord>, String> {
    match read_record_in::<FencedRecord>(dir, FENCED_FILE) {
        Ok(Some(record)) => record.validate().map(|()| Some(record)),
        Ok(None) => Ok(None),
        Err(error) => Err(format!(
            "{} carries {FENCED_FILE}, which cannot be read ({error}): it was fenced",
            dir.display()
        )),
    }
}

/// Spec P5.1 §11.A: the refusal of a STANDBY-role start over a fenced
/// primary's directory (a primary-role start serves it fenced instead).
pub fn fenced_start_refusal(dir: &Path, record: &FencedRecord) -> String {
    format!(
        "this directory is a primary that was fenced at {} by epoch {} (promotion {}; its last sequence is {}); it \
         is not a standby's and cannot be started as one; {}",
        format_unix_secs(record.fenced_at),
        record.fenced_by_epoch,
        record.by_node,
        record.final_sequence,
        fenced_rebuild_hint(dir)
    )
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Spec P5.2 §1: push FATAL 57P01 to every PostgreSQL session opened before
/// the fence by `by_epoch` at `at` (the session registry's termination), and
/// — on the async runtime, without holding the caller — drain them, closing
/// those that do not go. Sessions opened after the fence are no targets:
/// they were told `default_transaction_read_only = on` and are served reads.
fn push_session_termination(by_epoch: u64, at: u64) {
    use crate::protocol::postgres::sessions::{registry, TerminateCause, TERMINATE_DRAIN_TIMEOUT};
    let message = crate::standby_posture::fence_termination_message(by_epoch, at);
    let (generation, targets) = registry().push(TerminateCause::Fence, message);
    if targets.is_empty() {
        return;
    }
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            let drained = registry().drain(generation, targets, TERMINATE_DRAIN_TIMEOUT).await;
            info!(
                "replication: the fence ended {} PostgreSQL session(s) opened before it ({} closed by force, {} \
                 still open)",
                drained.targeted, drained.aborted, drained.remaining
            );
        });
    }
}

/// The fence that was applied, and by whom.
#[derive(Clone, Copy, Debug)]
struct Applied {
    fenced: Fenced,
    by_node: Uuid,
    at: u64,
}

impl Applied {
    fn of(record: &FencedRecord) -> Self {
        Self {
            fenced: Fenced {
                epoch: record.fenced_by_epoch,
                final_sequence: record.final_sequence,
            },
            by_node: record.by_node,
            at: record.fenced_at,
        }
    }
}

/// Spec P5.1 §11.A: a fenced primary's directory was opened (its engine's
/// write gate is fenced from the first second): note the fence for every
/// surface of the process — read-only, posture `fenced`, `/ready` 503,
/// `heliosdb_require_ready()` 55000 — before any listener accepts a
/// session. Returns the record; `None` when `dir` carries none.
pub fn note_fenced_at_open(engine: &StorageEngine, dir: &Path) -> Result<Option<FencedRecord>, String> {
    let Some(record) = read_fenced_record(dir)? else {
        return Ok(None);
    };
    if !engine.write_gate().is_fenced() {
        // The engine read the same file at open; a record that appeared
        // since is honoured too.
        engine
            .write_gate()
            .fence(primary_fenced_write_message(record.fenced_by_epoch, record.fenced_at));
    }
    super::ha_state::ha_state().set_read_only(true);
    note_fenced(FenceStatus {
        epoch: record.epoch,
        by_epoch: record.fenced_by_epoch,
        by_node: record.by_node,
        at: record.fenced_at,
        final_sequence: Some(record.final_sequence),
        incomplete: None,
    });
    Ok(Some(record))
}

struct Inner {
    /// The primary's engine: its write gate, its WAL, its sequence. Weak, so
    /// the fence never keeps a stopped primary's store open.
    engine: Weak<StorageEngine>,
    data_dir: PathBuf,
    history: Uuid,
    /// The primary's own epoch (its store's, read at open).
    epoch: u64,
    archive: SharedArchive,
    /// The fence applied, under the lock every fence runs under.
    applied: Mutex<Option<Applied>>,
}

/// A primary's fence (module documentation): the [`SourceFence`] of its
/// replication port.
#[derive(Clone)]
pub struct PrimaryFence {
    inner: Arc<Inner>,
}

impl PrimaryFence {
    /// The fence of the primary serving `engine` from `data_dir`, for
    /// `history`; `archive` is the archiver it seals (empty without one). A
    /// directory fenced before (its record, spec §11.A) starts fenced: a
    /// repeated fence by the same promotion is answered from the record, any
    /// other refused naming it.
    pub fn new(engine: &Arc<StorageEngine>, data_dir: &Path, history: Uuid, archive: SharedArchive) -> Self {
        let applied = match read_fenced_record(data_dir) {
            Ok(Some(record)) if record.history == history => Some(Applied::of(&record)),
            Ok(Some(record)) => {
                warn!(
                    "replication: {} records a fence of history {}, not this primary's {history}; it is ignored \
                     (the write gate stays fenced)",
                    data_dir.join(FENCED_FILE).display(),
                    record.history
                );
                None
            }
            Ok(None) => None,
            // The engine refused to open an unreadable record; one that went
            // bad since is answered by the next fence's read.
            Err(error) => {
                warn!("replication: {error}");
                None
            }
        };
        Self {
            inner: Arc::new(Inner {
                engine: Arc::downgrade(engine),
                data_dir: data_dir.to_path_buf(),
                history,
                epoch: engine.replication_epoch(),
                archive,
                applied: Mutex::new(applied),
            }),
        }
    }

    /// The archiver this fence seals, for the port's shutdown.
    pub fn archive(&self) -> &SharedArchive {
        &self.inner.archive
    }

    /// Whether this primary was fenced.
    pub fn is_fenced(&self) -> bool {
        self.inner.applied.lock().is_some()
    }
}

impl Inner {
    /// The answer to a fence by `epoch` from `node` once one was applied.
    fn answer(&self, applied: &Applied, epoch: u64, node: Uuid) -> Result<Fenced, Refusal> {
        if applied.by_node == node && epoch <= applied.fenced.epoch {
            return Ok(applied.fenced);
        }
        Err(Refusal::new(
            RefuseCode::Version,
            format!(
                "this primary was already fenced by epoch {} (node {}) at {}, at its sequence {}: a primary at epoch \
                 {} exists; recover the receiver from it instead",
                applied.fenced.epoch,
                applied.by_node,
                format_unix_secs(applied.at),
                applied.fenced.final_sequence,
                applied.fenced.epoch
            ),
        ))
    }

    /// Spec P5.1 §11.D: the fence could not complete (`why`): writes stay
    /// refused, the posture says "fence incomplete", and — no record having
    /// been written — a restart of this node undoes it.
    fn incomplete(&self, epoch: u64, why: String) -> Refusal {
        note_fence_incomplete(why.clone());
        error!(
            "replication: the fence of this primary by epoch {epoch} is INCOMPLETE: {why}. Writes stay refused and \
             this node is not ready; the promotion that asked retries the fence (or promotes with --force). No fence \
             record ({}) was written, so a restart of this node undoes the incomplete fence",
            self.data_dir.join(FENCED_FILE).display()
        );
        Refusal::new(RefuseCode::SourceNotServing, format!("fence incomplete: {why}"))
    }

    /// Spec P5.1 §11.B: everything through `final_sequence` archived and
    /// sealed, the archiver left running. Best effort, bounded: the fence
    /// answers either way, and says how far the archive got.
    fn seal_archive(&self, final_sequence: u64) {
        let sealer = self.archive.lock().as_ref().map(ArchiveService::sealer);
        let Some(sealer) = sealer else {
            return;
        };
        match sealer.seal_through(final_sequence, ARCHIVE_SEAL_TIMEOUT) {
            Ok(sealed) => info!(
                "archive: the fenced primary's WAL archive is sealed through sequence {sealed} (its final sequence \
                 {final_sequence}); the archiver keeps running, idle"
            ),
            Err(reason) => warn!(
                "archive: the fenced primary's WAL archive may end before its final sequence {final_sequence}: \
                 {reason}; a recovery from this archive may stop short of it"
            ),
        }
    }

    /// The fence itself (module documentation, steps 1-6), on a blocking
    /// thread, under the lock.
    fn fence_blocking(&self, epoch: u64, node: Uuid) -> Result<Fenced, Refusal> {
        let mut applied = self.applied.lock();
        if let Some(applied) = applied.as_ref() {
            return self.answer(applied, epoch, node);
        }
        if epoch <= self.epoch {
            return Err(Refusal::new(
                RefuseCode::Version,
                format!(
                    "a fence must name a higher epoch than this source's {}; it named {epoch}",
                    self.epoch
                ),
            ));
        }
        let not_serving = |reason: String| Refusal::new(RefuseCode::SourceNotServing, reason);
        let engine = self
            .engine
            .upgrade()
            .ok_or_else(|| not_serving("the primary is shutting down: its store is closed".to_string()))?;
        // A fence this process recorded before a failure later in an earlier
        // attempt (same promotion, same epoch) is taken up again.
        let at = match read_fenced_record(&self.data_dir).map_err(not_serving)? {
            Some(record) if record.by_node == node && record.fenced_by_epoch == epoch => record.fenced_at,
            Some(record) => {
                return Err(Refusal::new(
                    RefuseCode::Version,
                    format!(
                        "this primary's directory records a fence by epoch {} (node {}): a primary at epoch {} exists",
                        record.fenced_by_epoch, record.by_node, record.fenced_by_epoch
                    ),
                ))
            }
            None => unix_secs(),
        };
        // Step 1: the gate. From here on no write is admitted, and every
        // surface says `fencing` (spec §11.D).
        engine.write_gate().fence(primary_fenced_write_message(epoch, at));
        super::ha_state::ha_state().set_read_only(true);
        note_fencing(self.epoch, epoch, node, at);
        // Spec P5.2 §1: every PostgreSQL session opened before the fence is
        // ended NOW (FATAL 57P01, pushed), not only at its next statement,
        // which stays the backstop — an idle pooled connection must not keep
        // believing it talks to the primary. Not waited for here (this thread
        // holds the fence's lock); the drain closes what does not go.
        push_session_termination(epoch, at);
        // Step 2: the writes already past the gate, made durable.
        if !engine.write_gate().wait_for_writes(DRAIN_TIMEOUT) {
            return Err(self.incomplete(
                epoch,
                format!(
                    "a write admitted before the fence did not finish within {} s (a stalled disk?)",
                    DRAIN_TIMEOUT.as_secs()
                ),
            ));
        }
        let db = engine.db();
        if let Err(error) = db.flush_wal(true) {
            return Err(self.incomplete(epoch, format!("the WAL could not be fsynced after the fence: {error}")));
        }
        let final_sequence = db.latest_sequence_number();
        // Step 3: durable.
        let record = FencedRecord {
            format: FORMAT,
            history: self.history,
            epoch: self.epoch,
            fenced_by_epoch: epoch,
            fenced_at: at,
            by_node: node,
            final_sequence,
        };
        match write_new_record_in(&self.data_dir, FENCED_FILE, &record) {
            Ok(()) => {}
            // The earlier attempt's record (checked above): same fence.
            Err(HistoryError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(self.incomplete(
                    epoch,
                    format!(
                        "the fence could not be recorded in {} ({error})",
                        self.data_dir.join(FENCED_FILE).display()
                    ),
                ))
            }
        }
        // Step 4.
        note_fenced(FenceStatus {
            epoch: self.epoch,
            by_epoch: epoch,
            by_node: node,
            at,
            final_sequence: Some(final_sequence),
            incomplete: None,
        });
        // Nothing is persisted on this engine's behalf any more: its close
        // would only be refused by the gate.
        engine.set_index_snapshots_on_close(false);
        engine.set_row_counter_flush_on_close(false);
        // Step 5: the archive holds everything through the final sequence.
        self.seal_archive(final_sequence);
        // Step 6.
        error!(
            "replication: this primary (history {}, epoch {}) was FENCED by epoch {epoch} (promotion {node}) at its \
             sequence {final_sequence}: a standby was promoted. Every write is refused from now on; reads are \
             served. {}",
            self.history,
            self.epoch,
            fenced_rebuild_hint(&self.data_dir)
        );
        let fenced = Fenced { epoch, final_sequence };
        *applied = Some(Applied {
            fenced,
            by_node: node,
            at,
        });
        Ok(fenced)
    }
}

#[async_trait]
impl SourceFence for PrimaryFence {
    async fn fence(&self, epoch: u64, node: Uuid) -> Result<Fenced, Refusal> {
        let inner = Arc::clone(&self.inner);
        // One blocking job: a session that stops waiting cannot cancel it.
        tokio::task::spawn_blocking(move || inner.fence_blocking(epoch, node))
            .await
            .unwrap_or_else(|error| {
                Err(Refusal::new(
                    RefuseCode::SourceNotServing,
                    format!("the fence's worker failed: {error}"),
                ))
            })
    }

    /// Spec P5.1 §11.E: fenced — or being fenced (the gate closed) — this
    /// source bootstraps nobody: a snapshot or a fast-forward of it would be
    /// a copy of a superseded primary.
    fn fenced(&self) -> Option<String> {
        if let Some(applied) = *self.inner.applied.lock() {
            return Some(format!(
                "fenced at {} by epoch {}: bootstrap from the new primary",
                format_unix_secs(applied.at),
                applied.fenced.epoch
            ));
        }
        let gate_fenced = self
            .inner
            .engine
            .upgrade()
            .is_some_and(|engine| engine.write_gate().is_fenced());
        gate_fenced.then(|| "fenced (the fence is not complete): bootstrap from the new primary".to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn record(dir: &Path) -> FencedRecord {
        let record = FencedRecord {
            format: FORMAT,
            history: Uuid::new_v4(),
            epoch: 1,
            fenced_by_epoch: 2,
            fenced_at: 1_761_000_000,
            by_node: Uuid::new_v4(),
            final_sequence: 4_242,
        };
        write_new_record_in(dir, FENCED_FILE, &record).unwrap();
        record
    }

    #[test]
    fn a_fence_record_round_trips_and_names_the_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_fenced_record(dir.path()).unwrap(), None);
        let written = record(dir.path());
        let read = read_fenced_record(dir.path()).unwrap().unwrap();
        assert_eq!(read, written);
        let refusal = fenced_start_refusal(dir.path(), &read);
        assert!(
            refusal.contains("fenced at")
                && refusal.contains("by epoch 2")
                && refusal.contains("cannot be started as one"),
            "{refusal}"
        );
        assert!(
            refusal.contains("heliosdb-nano recover --from-primary") && refusal.contains("--replace"),
            "{refusal}"
        );
        // Written whole, never over another: a second record is refused.
        assert!(write_new_record_in(dir.path(), FENCED_FILE, &written).is_err());
        assert_eq!(read_fenced_record(dir.path()).unwrap(), Some(written));
    }

    #[test]
    fn an_unreadable_fence_record_still_means_fenced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FENCED_FILE), b"{not json").unwrap();
        let error = read_fenced_record(dir.path()).unwrap_err();
        assert!(error.contains("it was fenced"), "{error}");
    }

    /// Spec P5.1 §11.A, §11.C: a primary whose directory carries a fence
    /// record starts fenced — the record's promotion is answered the same
    /// `Fenced` (from the record, nothing re-done), another promotion of the
    /// same epoch is refused naming the first, and the source bootstraps
    /// nobody. The process-wide posture is not touched: `fence_blocking`
    /// returns at the record's answer before any step.
    #[test]
    fn a_directory_fenced_before_starts_fenced_and_answers_its_promotion() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let written = record(dir.path());
        let engine = Arc::new(StorageEngine::open(&store, &crate::Config::default()).unwrap());
        let fence = PrimaryFence::new(&engine, dir.path(), written.history, Arc::new(Mutex::new(None)));
        assert!(fence.is_fenced());
        let answered = fence.inner.fence_blocking(2, written.by_node).unwrap();
        assert_eq!(
            answered,
            Fenced {
                epoch: 2,
                final_sequence: written.final_sequence
            }
        );
        let other = fence.inner.fence_blocking(2, Uuid::new_v4()).unwrap_err();
        assert_eq!(other.code, RefuseCode::Version);
        assert!(other.reason.contains("already fenced by epoch 2"), "{other}");
        assert!(SourceFence::fenced(&fence).is_some_and(|why| why.contains("bootstrap from the new primary")));
    }
}
