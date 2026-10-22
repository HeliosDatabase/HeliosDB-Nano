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

use super::checkpoint_transfer::{validate_primary_source, OnlineCheckpointExport, TransferLimits};
use super::ha_state::{ha_state, StandbyHandle, StandbyRegistration, StandbyState, SyncMode};
use super::physical::{poll_source, PhysicalError, PhysicalPoll, MAX_BATCH_BYTES};
use super::physical_wire::{
    Heartbeat, Hello, ObservedSession, OpenRequest, PeerRole, PhysicalSource, Refusal, RefuseCode, Result,
    SessionObserver, SourceSessionInfo, WireError, WireSyncMode,
};
use async_trait::async_trait;
use rocksdb::DB;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
};
use uuid::Uuid;

fn source_error(error: impl std::fmt::Display) -> WireError {
    WireError::Source(error.to_string())
}

fn not_serving(reason: &str) -> WireError {
    WireError::Refused(Refusal::new(RefuseCode::SourceNotServing, reason))
}

/// A poll's failure as the receiver must hear it: a WAL it no longer has, a
/// hole, a receiver ahead of it — each a refusal the receiver needs recovery
/// for. Anything else is a source-side failure (retried).
fn poll_error(db: &DB, error: PhysicalError) -> WireError {
    match error {
        PhysicalError::Pruned { wanted, oldest } => WireError::Refused(Refusal::new(
            RefuseCode::WalPruned {
                oldest: oldest.unwrap_or_else(|| db.latest_sequence_number().saturating_add(1)),
            },
            format!(
                "the receiver needs sequence {wanted}; this primary's physical WAL no longer holds it (retention \
                 [replication.physical] wal_retention_secs / wal_retention_size_mb): rebuild the standby from a \
                 new, empty data directory"
            ),
        )),
        PhysicalError::Gap { expected, observed } => WireError::Refused(Refusal::new(
            RefuseCode::Gap {
                expected,
                have_from: observed,
                have_to: db.latest_sequence_number(),
            },
            "this primary's physical WAL is not contiguous from the receiver's cursor",
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
        other => source_error(other),
    }
}

/// One shared budget per source endpoint, retained by all its providers.
/// A canceled wire session does not release capacity while its blocking job is
/// still running. Constructing a separate limiter for each session is invalid.
#[derive(Clone)]
pub struct SourceWorkLimiter(Arc<SourceWorkState>);
struct SourceWorkState {
    slots: Arc<tokio::sync::Semaphore>,
    max_jobs: u32,
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
            draining: AtomicBool::new(false),
        })))
    }
    /// Terminal admission stop. Await after joining wire sessions so every
    /// canceled/finished session's actual blocking work releases its resources
    /// before the source DB owner exits. A timeout is not a completed drain.
    pub async fn drain(&self) -> Result<()> {
        self.0.draining.store(true, Ordering::Release);
        let all = self
            .0
            .slots
            .clone()
            .acquire_many_owned(self.0.max_jobs)
            .await
            .map_err(source_error)?;
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
}

struct PrimarySession {
    db: Arc<DB>,
    history: Uuid,
    storage_identity: String,
    info: SourceSessionInfo,
    export: Option<Arc<OnlineCheckpointExport>>,
    work: SourceWorkLimiter,
}

/// Open one source session. Resume does not inspect/create `export_parent`,
/// allocate a checkpoint, or reset a receiver cursor. Its first bounded poll
/// proves a contiguous prefix and durable source barrier; later polls recheck
/// continuity independently. Missing WAL always fails rather than reseeding.
///
/// A snapshot lease retains files, not WAL. The caller owns listener admission,
/// simultaneous export caps, retention policy and disk budgets.
pub async fn open_primary_session(
    db: Arc<DB>,
    expected_history: Uuid,
    export_parent: PathBuf,
    limits: TransferLimits,
    request: OpenRequest,
    work: SourceWorkLimiter,
) -> Result<Arc<dyn PhysicalSource>> {
    let session_work = work.clone();
    work.run(move || {
        let storage_identity = validate_primary_source(&db, expected_history).map_err(source_error)?;
        let (info, export) = match request {
            OpenRequest::Snapshot => {
                let export = Arc::new(
                    OnlineCheckpointExport::create(&db, expected_history, &export_parent, limits)
                        .map_err(source_error)?,
                );
                (SourceSessionInfo::Snapshot(export.manifest().clone()), Some(export))
            }
            OpenRequest::Resume { history, after } => {
                if history != expected_history {
                    return Err(poll_error(&db, PhysicalError::HistoryMismatch));
                }
                // The first bounded poll proves the receiver's cursor is still
                // served: retained, contiguous, not ahead of this primary.
                let initial = poll_source(&db, history, after, MAX_BATCH_BYTES).map_err(|e| poll_error(&db, e))?;
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
        if validate_primary_source(&db, expected_history).map_err(source_error)? != storage_identity {
            return Err(source_error("primary storage identity changed during session creation"));
        }
        Ok(Arc::new(PrimarySession {
            db,
            history: expected_history,
            storage_identity,
            info,
            export,
            work: session_work,
        }) as Arc<dyn PhysicalSource>)
    })
    .await
}

#[async_trait]
impl PhysicalSource for PrimarySession {
    fn session_info(&self) -> &SourceSessionInfo {
        &self.info
    }

    async fn read_chunk(&self, file_index: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
        let export = self
            .export
            .clone()
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
        self.work
            .run(move || {
                if validate_primary_source(&db, history).map_err(source_error)? != storage_identity {
                    return Err(source_error("primary storage identity changed before poll"));
                }
                let poll = poll_source(&db, history, after, byte_limit).map_err(|e| poll_error(&db, e))?;
                if validate_primary_source(&db, history).map_err(source_error)? != storage_identity {
                    return Err(source_error("primary storage identity changed during poll"));
                }
                Ok(poll)
            })
            .await
    }
}

/// Spec P2.1 §2: the primary's standby registry as its replication sessions
/// see it — `ha_state()`'s, the one the commit path's sync wait reads. Every
/// admitted session registers its standby from the `Hello` and records each
/// heartbeat (= acknowledgement) into it; the session's end unregisters it.
pub struct StandbyRegistryObserver {
    db: Weak<DB>,
}

impl StandbyRegistryObserver {
    /// `db` is the primary's store (its physical sequence is the barrier).
    pub fn new(db: &Arc<DB>) -> Self {
        Self { db: Arc::downgrade(db) }
    }
}

/// One admitted session, registered while it lives.
struct RegisteredSession(StandbyHandle);

impl ObservedSession for RegisteredSession {
    fn heartbeat(&mut self, heartbeat: &Heartbeat) {
        self.0
            .record_ack(heartbeat.applied_sequence, heartbeat.durable_sequence);
    }
}

impl SessionObserver for StandbyRegistryObserver {
    fn barrier(&self) -> u64 {
        self.db.upgrade().map_or(0, |db| db.latest_sequence_number())
    }

    fn admit(&self, peer: Option<SocketAddr>, hello: &Hello) -> Box<dyn ObservedSession> {
        // A resumed session follows this primary from a durable cursor; a
        // snapshot session is still installing its copy. Only a serving
        // standby that follows counts toward `min_acks`.
        let following = hello.cursor.is_some();
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
        )
        .await
        .unwrap();
        std::fs::write(db.path().join(PRIMARY_HISTORY_FILE), b"{}").unwrap();
        assert!(source.poll(1, MAX_BATCH_BYTES).await.is_err());
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
}
