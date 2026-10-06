//! EXTERNAL DRAFT: opt-in admission for complete synchronous source operations.
//!
//! This module alone DOES NOT certify Nano SQL consistency. Every mutating path,
//! including background work and error cleanup, must participate before its
//! barriers may be advertised as serving boundaries. Raw DB access bypasses the
//! protocol. Create one coordinator per engine at construction; never late-enable
//! it while previously untracked operations or background workers exist.
//!
//! Permits/fences are !Send and !Sync and must never cross an await. Reentrancy is
//! per OS thread, not task: using these guards across await is incorrect even on
//! a local executor. The timeout bounds admission/drain waiting only; an owned
//! filesystem/checkpoint/fsync operation cannot be forcibly canceled.

use super::checkpoint_transfer::{
    validate_primary_source, CapturedCheckpoint, OnlineCheckpointExport, TransferError,
    TransferLimits,
};
use rocksdb::DB;
use std::{
    collections::HashMap,
    marker::PhantomData,
    path::Path,
    rc::Rc,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread::{self, ThreadId},
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("physical source fence drain timed out")]
    Timeout,
    #[error("physical source fence timeout must be positive and at most one hour")]
    InvalidTimeout,
    #[error("physical source cannot fence its own admitted operation or nested fence")]
    SelfFence,
    #[error("physical source operation cannot start inside its capture fence")]
    OperationInsideFence,
    #[error("physical source coordinator poisoned: {0}")]
    Poisoned(String),
    #[error("physical source operation nesting exhausted")]
    DepthOverflow,
    #[error("physical source barrier belongs to another database or history")]
    WrongSource,
    #[error("physical source changed while exclusively fenced; untracked mutator")]
    UntrackedMutation,
    #[error("physical source checkpoint cut differs from its fenced source boundary")]
    CheckpointCutMismatch,
    #[error("physical source storage: {0}")]
    Storage(#[from] rocksdb::Error),
    #[error("physical source checkpoint: {0}")]
    Transfer(#[from] TransferError),
}
pub type Result<T> = std::result::Result<T, CoordinatorError>;

#[derive(Default)]
struct Admission {
    operations: HashMap<ThreadId, usize>,
    waiting_fences: usize,
    fence_owner: Option<ThreadId>,
    poison: Option<String>,
    admission_broken: bool,
}
struct Inner {
    db: Arc<DB>,
    state: Mutex<Admission>,
    changed: Condvar,
}
impl Inner {
    fn lock(&self) -> MutexGuard<'_, Admission> {
        match self.state.lock() {
            Ok(state) => state,
            Err(error) => {
                let mut state = error.into_inner();
                state.admission_broken = true;
                state
                    .poison
                    .get_or_insert_with(|| "admission mutex poisoned".into());
                state
            }
        }
    }
}
fn healthy(state: &Admission) -> Result<()> {
    match &state.poison {
        Some(reason) => Err(CoordinatorError::Poisoned(reason.clone())),
        None => Ok(()),
    }
}

/// Store `Option<Arc<PhysicalSourceCoordinator>>` on the engine. `None` is the
/// ordinary default and read-only mode, so disabled operations need no lock.
/// Constructing multiple coordinators for the same DB violates this protocol.
pub struct PhysicalSourceCoordinator {
    inner: Arc<Inner>,
}
impl PhysicalSourceCoordinator {
    pub fn new(db: Arc<DB>) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                state: Mutex::new(Admission::default()),
                changed: Condvar::new(),
            }),
        }
    }

    /// Admit one whole synchronous semantic operation, including metadata and
    /// failure cleanup. Nested admissions bypass waiting fences only when this
    /// exact OS thread already owns an operation on this coordinator.
    pub fn operation(&self) -> Result<PhysicalOperationPermit> {
        let owner = thread::current().id();
        let mut state = self.inner.lock();
        loop {
            if state.admission_broken {
                return Err(CoordinatorError::Poisoned(
                    "operation admission accounting broken".into(),
                ));
            }
            if state.fence_owner == Some(owner) {
                return Err(CoordinatorError::OperationInsideFence);
            }
            if let Some(depth) = state.operations.get_mut(&owner) {
                *depth = depth
                    .checked_add(1)
                    .ok_or(CoordinatorError::DepthOverflow)?;
                break;
            }
            if state.fence_owner.is_none() && state.waiting_fences == 0 {
                state.operations.insert(owner, 1);
                break;
            }
            state = match self.inner.changed.wait(state) {
                Ok(state) => state,
                Err(error) => {
                    let mut state = error.into_inner();
                    state.admission_broken = true;
                    state
                        .poison
                        .get_or_insert_with(|| "admission mutex poisoned".into());
                    state
                }
            };
        }
        drop(state);
        Ok(PhysicalOperationPermit {
            inner: self.inner.clone(),
            owner,
            _not_send_or_sync: PhantomData,
        })
    }

    pub fn fence(&self, timeout: Duration) -> Result<PhysicalSourceFence> {
        if timeout.is_zero() || timeout > Duration::from_secs(3600) {
            return Err(CoordinatorError::InvalidTimeout);
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(CoordinatorError::InvalidTimeout)?;
        let owner = thread::current().id();
        let mut state = self.inner.lock();
        healthy(&state)?;
        if state.operations.contains_key(&owner) || state.fence_owner == Some(owner) {
            return Err(CoordinatorError::SelfFence);
        }
        state.waiting_fences = state
            .waiting_fences
            .checked_add(1)
            .ok_or(CoordinatorError::DepthOverflow)?;
        // No user code executes while waiting. Every normal/error exit below
        // removes exactly this waiter before waking operations/other fences.
        let outcome = loop {
            if let Err(error) = healthy(&state) {
                break Err(error);
            }
            if Instant::now() >= deadline {
                break Err(CoordinatorError::Timeout);
            }
            if state.operations.is_empty() && state.fence_owner.is_none() {
                state.fence_owner = Some(owner);
                break Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            state = match self.inner.changed.wait_timeout(state, remaining) {
                Ok((state, _)) => state,
                Err(error) => {
                    let (mut state, _) = error.into_inner();
                    state.admission_broken = true;
                    state
                        .poison
                        .get_or_insert_with(|| "admission mutex poisoned".into());
                    state
                }
            };
        };
        state.waiting_fences -= 1;
        self.inner.changed.notify_all();
        drop(state);
        outcome?;
        Ok(PhysicalSourceFence {
            inner: self.inner.clone(),
            owner,
            _not_send_or_sync: PhantomData,
        })
    }

    /// Sticky certification refusal after possible partial semantic writes.
    /// Ordinary primary admission continues; only certification is disabled.
    /// Do not call for every SQL error: no-write errors and rolled-back errors
    /// are valid boundaries. Their classification belongs to audited callers.
    /// Reopening/recovery is required; there is deliberately no reset method.
    pub fn poison_certification(&self, reason: impl Into<String>) {
        let mut state = self.inner.lock();
        state.poison.get_or_insert_with(|| reason.into());
        self.inner.changed.notify_all();
    }

    /// Arm immediately before the first semantic durable write, after ordinary
    /// prewrite validation. This retains its own nested operation admission.
    /// Complete only after all related writes and cleanup form a valid boundary.
    /// An error/panic/drop while armed disables future certification, not SQL.
    pub fn begin_durable_phase(&self, reason: &'static str) -> Result<PhysicalDurablePhase> {
        Ok(PhysicalDurablePhase {
            operation: self.operation()?,
            completed: false,
            reason,
        })
    }

    pub fn capture_barrier(
        &self,
        history: Uuid,
        timeout: Duration,
    ) -> Result<CommittedPhysicalBarrier> {
        self.fence(timeout)?.capture_barrier(history)
    }

    /// Capture files and exact sequence under the fence; `CapturedCheckpoint`
    /// owns the private lease and hashes files only when the caller later calls
    /// finish(), after this method has released admission. No WAL pin is implied.
    pub fn capture_checkpoint(
        &self,
        history: Uuid,
        export_parent: &Path,
        limits: TransferLimits,
        timeout: Duration,
    ) -> Result<(CapturedCheckpoint, CommittedPhysicalBarrier)> {
        self.fence(timeout)?
            .capture_checkpoint(history, export_parent, limits)
    }
}

/// The last same-thread permit releases admission, regardless of guard drop
/// order. Do not keep this value across async suspension or user idle time.
pub struct PhysicalOperationPermit {
    inner: Arc<Inner>,
    owner: ThreadId,
    _not_send_or_sync: PhantomData<Rc<()>>,
}
impl Drop for PhysicalOperationPermit {
    fn drop(&mut self) {
        let mut state = self.inner.lock();
        if thread::panicking() {
            state
                .poison
                .get_or_insert_with(|| "semantic operation unwound".into());
        }
        match state.operations.get_mut(&self.owner) {
            Some(depth) if *depth > 1 => *depth -= 1,
            Some(_) => {
                state.operations.remove(&self.owner);
            }
            None => {
                state.admission_broken = true;
                state
                    .poison
                    .get_or_insert_with(|| "operation admission accounting lost".into());
            }
        }
        self.inner.changed.notify_all();
    }
}

/// A semantic write phase that may have partially persisted on failure.
/// The owned operation remains admitted until certification is marked uncertain.
pub struct PhysicalDurablePhase {
    operation: PhysicalOperationPermit,
    completed: bool,
    reason: &'static str,
}
impl PhysicalDurablePhase {
    pub fn complete(mut self) {
        self.completed = true;
    }
}
impl Drop for PhysicalDurablePhase {
    fn drop(&mut self) {
        if !self.completed {
            let mut state = self.operation.inner.lock();
            state.poison.get_or_insert_with(|| self.reason.to_string());
            self.operation.inner.changed.notify_all();
        }
        // Rust drops operation AFTER this body: no fence can cross the failure
        // before its uncertainty has been recorded.
    }
}

pub struct PhysicalSourceFence {
    inner: Arc<Inner>,
    owner: ThreadId,
    _not_send_or_sync: PhantomData<Rc<()>>,
}
impl PhysicalSourceFence {
    fn check_healthy(&self) -> Result<()> {
        let state = self.inner.lock();
        healthy(&state)?;
        if state.fence_owner != Some(self.owner) || !state.operations.is_empty() {
            return Err(CoordinatorError::UntrackedMutation);
        }
        Ok(())
    }
    fn verify_sequence(&self, expected: u64) -> Result<()> {
        self.check_healthy()?;
        if self.inner.db.latest_sequence_number() != expected {
            let mut state = self.inner.lock();
            state
                .poison
                .get_or_insert_with(|| "raw DB changed during exclusive source capture".into());
            self.inner.changed.notify_all();
            return Err(CoordinatorError::UntrackedMutation);
        }
        Ok(())
    }
    pub fn capture_barrier(&self, history: Uuid) -> Result<CommittedPhysicalBarrier> {
        self.check_healthy()?;
        let identity = validate_primary_source(&self.inner.db, history)?;
        let sequence = self.inner.db.latest_sequence_number();
        self.inner.db.flush_wal(true)?;
        self.verify_sequence(sequence)?;
        if validate_primary_source(&self.inner.db, history)? != identity {
            return Err(CoordinatorError::WrongSource);
        }
        Ok(CommittedPhysicalBarrier {
            db: self.inner.db.clone(),
            history,
            source_sequence: sequence,
        })
    }
    pub fn capture_checkpoint(
        &self,
        history: Uuid,
        export_parent: &Path,
        limits: TransferLimits,
    ) -> Result<(CapturedCheckpoint, CommittedPhysicalBarrier)> {
        let boundary = self.capture_barrier(history)?;
        let captured =
            OnlineCheckpointExport::capture(&self.inner.db, history, export_parent, limits)?;
        self.verify_sequence(boundary.source_sequence)?;
        if captured.history() != history
            || captured.checkpoint_sequence() != boundary.source_sequence
        {
            return Err(CoordinatorError::CheckpointCutMismatch);
        }
        // Capture validates durable history/IDENTITY before and after the raw
        // checkpoint. Full file hashing deliberately happens in finish().
        Ok((captured, boundary))
    }
}
impl Drop for PhysicalSourceFence {
    fn drop(&mut self) {
        let mut state = self.inner.lock();
        if thread::panicking() {
            state
                .poison
                .get_or_insert_with(|| "source capture unwound".into());
        }
        if state.fence_owner == Some(self.owner) {
            state.fence_owner = None;
        } else {
            state.admission_broken = true;
            state
                .poison
                .get_or_insert_with(|| "fence admission accounting lost".into());
        }
        self.inner.changed.notify_all();
    }
}

/// An in-process capability, not a deserializable wire integer. This certifies
/// only a fully wired source admission protocol; a partially wired engine must
/// never expose it as SQL-ready. Source retention remains independent.
#[derive(Clone)]
pub struct CommittedPhysicalBarrier {
    db: Arc<DB>,
    history: Uuid,
    source_sequence: u64,
}
impl CommittedPhysicalBarrier {
    pub fn source_sequence(&self) -> u64 {
        self.source_sequence
    }
    pub fn history(&self) -> Uuid {
        self.history
    }
    pub(crate) fn checked_sequence(&self, db: &DB, history: Uuid) -> Result<u64> {
        if !std::ptr::eq(self.db.as_ref(), db) || self.history != history {
            return Err(CoordinatorError::WrongSource);
        }
        Ok(self.source_sequence)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::super::history::load_or_create_primary_history;
    use super::*;
    use rocksdb::Options;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    // Join even if an assertion unwinds before the explicit join below. Channel
    // fixtures have bounded receive waits; a kernel I/O stall still cannot be
    // forcibly canceled. TempDir destruction always follows worker ownership.
    struct JoinedThread<T>(Option<thread::JoinHandle<T>>);
    impl<T: Send + 'static> JoinedThread<T> {
        fn spawn(work: impl FnOnce() -> T + Send + 'static) -> Self {
            Self(Some(thread::spawn(work)))
        }
        fn join(mut self) -> thread::Result<T> {
            self.0.take().expect("owned fixture worker").join()
        }
    }
    impl<T> Drop for JoinedThread<T> {
        fn drop(&mut self) {
            if let Some(worker) = self.0.take() {
                let _ = worker.join();
            }
        }
    }

    fn fixture() -> (
        tempfile::TempDir,
        Arc<DB>,
        Arc<PhysicalSourceCoordinator>,
        Uuid,
    ) {
        let root = tempfile::tempdir().unwrap();
        let mut options = Options::default();
        options.create_if_missing(true);
        let db = Arc::new(DB::open(&options, root.path().join("source")).unwrap());
        let history = load_or_create_primary_history(db.path()).unwrap();
        let coordinator = Arc::new(PhysicalSourceCoordinator::new(db.clone()));
        (root, db, coordinator, history)
    }
    fn wait_pending(coordinator: &PhysicalSourceCoordinator) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while coordinator.inner.lock().waiting_fences == 0 {
            assert!(Instant::now() < deadline, "fence was not queued");
            thread::yield_now();
        }
    }

    #[test]
    fn nested_operation_bypasses_pending_fence_and_last_guard_controls_admission() {
        let (_root, _db, coordinator, _) = fixture();
        let outer = coordinator.operation().unwrap();
        let next = coordinator.clone();
        let (captured_tx, captured_rx) = mpsc::channel();
        let waiter = JoinedThread::spawn(move || {
            let result = next.fence(Duration::from_secs(2));
            let success = result.is_ok();
            let _ = captured_tx.send(success);
        });
        wait_pending(&coordinator);
        let inner = coordinator.operation().unwrap();
        drop(outer); // Deliberately reverse ordinary lexical guard order.
        let before = captured_rx.recv_timeout(Duration::from_millis(30));
        drop(inner);
        let after = captured_rx.recv_timeout(Duration::from_secs(2));
        let joined = waiter.join();
        assert!(
            before.is_err(),
            "fence crossed the remaining nested operation"
        );
        assert_eq!(after.unwrap(), true);
        assert!(joined.is_ok());
    }

    #[test]
    fn queued_fence_precedes_new_thread_operations_but_not_existing_nested_calls() {
        let (_root, _db, coordinator, _) = fixture();
        let permit = coordinator.operation().unwrap();
        let next = coordinator.clone();
        let (fenced_tx, fenced_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let capture = JoinedThread::spawn(move || {
            let fence = next.fence(Duration::from_secs(2)).unwrap();
            let _ = fenced_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
            drop(fence);
        });
        wait_pending(&coordinator);
        let next = coordinator.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let operation = JoinedThread::spawn(move || {
            let result = next.operation();
            let _ = entered_tx.send(result.is_ok());
        });
        let nested = coordinator.operation().unwrap();
        drop(nested);
        drop(permit);
        let fenced = fenced_rx.recv_timeout(Duration::from_secs(2));
        let entered_early = entered_rx.recv_timeout(Duration::from_millis(30));
        let _ = release_tx.send(());
        let entered = entered_rx.recv_timeout(Duration::from_secs(2));
        let capture_result = capture.join();
        let operation_result = operation.join();
        assert!(fenced.is_ok() && entered_early.is_err());
        assert_eq!(entered.unwrap(), true);
        assert!(capture_result.is_ok() && operation_result.is_ok());
    }

    #[test]
    fn timed_out_waiter_unblocks_new_operations_and_self_fences_are_rejected() {
        let (_root, _db, coordinator, _) = fixture();
        let permit = coordinator.operation().unwrap();
        assert!(matches!(
            coordinator.fence(Duration::from_millis(20)),
            Err(CoordinatorError::SelfFence)
        ));
        let next = coordinator.clone();
        let waiter = JoinedThread::spawn(move || {
            matches!(
                next.fence(Duration::from_millis(30)),
                Err(CoordinatorError::Timeout)
            )
        });
        assert!(waiter.join().unwrap());
        assert_eq!(coordinator.inner.lock().waiting_fences, 0);
        let next = coordinator.clone();
        assert!(JoinedThread::spawn(move || next.operation().is_ok())
            .join()
            .unwrap());
        drop(permit);
        let fence = coordinator.fence(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            coordinator.operation(),
            Err(CoordinatorError::OperationInsideFence)
        ));
        assert!(matches!(
            coordinator.fence(Duration::from_secs(1)),
            Err(CoordinatorError::SelfFence)
        ));
        drop(fence);
        assert!(matches!(
            coordinator.fence(Duration::ZERO),
            Err(CoordinatorError::InvalidTimeout)
        ));
    }

    #[test]
    fn independent_coordinators_do_not_share_thread_depth_or_fences() {
        let (_first_root, _first_db, first, _) = fixture();
        let (_second_root, _second_db, second, _) = fixture();
        let permit = first.operation().unwrap();
        let fence = second.fence(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            first.fence(Duration::from_millis(10)),
            Err(CoordinatorError::SelfFence)
        ));
        drop(fence);
        drop(permit);
        assert!(first.fence(Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn panic_and_explicit_uncertain_failure_poison_future_capture() {
        let (_root, _db, coordinator, history) = fixture();
        let next = coordinator.clone();
        let failed = JoinedThread::spawn(move || {
            let _permit = next.operation().unwrap();
            panic!("injected semantic failure after possible writes");
        })
        .join();
        assert!(failed.is_err());
        assert!(
            coordinator.operation().is_ok(),
            "optional certification poison must not disable primary writes"
        );
        assert!(matches!(
            coordinator.capture_barrier(history, Duration::from_secs(1)),
            Err(CoordinatorError::Poisoned(_))
        ));
        let (_other_root, _other_db, other, other_history) = fixture();
        other.poison_certification("partial durable DDL error");
        assert!(other.operation().is_ok());
        assert!(matches!(
            other.capture_barrier(other_history, Duration::from_secs(1)),
            Err(CoordinatorError::Poisoned(_))
        ));
    }

    #[test]
    fn capture_waits_for_both_batches_of_one_semantic_operation() {
        let (root, db, coordinator, history) = fixture();
        let (first_tx, first_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let writer_coord = coordinator.clone();
        let writer_db = db.clone();
        let writer = JoinedThread::spawn(move || {
            let _permit = writer_coord.operation().unwrap();
            writer_db.put(b"row", b"committed row").unwrap();
            let _ = first_tx.send(());
            let _ = finish_rx.recv_timeout(Duration::from_secs(2));
            writer_db.put(b"commit-metadata", b"complete").unwrap();
        });
        let first = first_rx.recv_timeout(Duration::from_secs(2));
        let next = coordinator.clone();
        let parent = root.path().to_path_buf();
        let capture = JoinedThread::spawn(move || {
            next.capture_checkpoint(
                history,
                &parent,
                TransferLimits::default(),
                Duration::from_secs(3),
            )
        });
        wait_pending(&coordinator);
        let _ = finish_tx.send(());
        let writer_result = writer.join();
        let (captured, barrier) = capture.join().unwrap().unwrap();
        assert!(first.is_ok() && writer_result.is_ok());
        assert_eq!(barrier.source_sequence(), 2);
        assert_eq!(captured.checkpoint_sequence(), 2);
        // Admissions are released BEFORE file hashing/publication. This write
        // must neither block nor enter the already captured checkpoint.
        let operation = coordinator.operation().unwrap();
        db.put(b"later", b"outside cut").unwrap();
        drop(operation);
        let export = captured.finish().unwrap();
        assert_eq!(export.manifest().checkpoint_sequence, 2);
        assert_eq!(barrier.checked_sequence(&db, history).unwrap(), 2);
        assert_eq!(db.latest_sequence_number(), 3);
    }

    #[test]
    fn committed_capability_rejects_other_database_and_history() {
        let (_root, db, coordinator, history) = fixture();
        let (_other_root, other_db, _other, _) = fixture();
        let barrier = coordinator
            .capture_barrier(history, Duration::from_secs(1))
            .unwrap();
        assert_eq!(barrier.checked_sequence(&db, history).unwrap(), 0);
        assert!(matches!(
            barrier.checked_sequence(&other_db, history),
            Err(CoordinatorError::WrongSource)
        ));
        assert!(matches!(
            barrier.checked_sequence(&db, Uuid::new_v4()),
            Err(CoordinatorError::WrongSource)
        ));
        assert!(coordinator
            .capture_barrier(Uuid::nil(), Duration::from_secs(1))
            .is_err());
    }

    #[test]
    fn concurrent_fences_never_overlap_and_release_admission() {
        let (_root, _db, coordinator, _) = fixture();
        let active = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let next = coordinator.clone();
            let active = active.clone();
            workers.push(JoinedThread::spawn(move || {
                for _ in 0..10 {
                    let _fence = next.fence(Duration::from_secs(2)).unwrap();
                    assert!(!active.swap(true, Ordering::SeqCst));
                    thread::yield_now();
                    assert!(active.swap(false, Ordering::SeqCst));
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(coordinator.operation().is_ok());
    }
    #[test]
    fn detected_untracked_raw_write_disables_certification_but_not_primary_admission() {
        let (_root, db, coordinator, history) = fixture();
        let fence = coordinator.fence(Duration::from_secs(1)).unwrap();
        let expected = db.latest_sequence_number();
        // Deliberate protocol violation: bypasses all source operation permits.
        db.put(b"bypass", b"must not certify").unwrap();
        assert!(matches!(
            fence.verify_sequence(expected),
            Err(CoordinatorError::UntrackedMutation)
        ));
        drop(fence);
        assert!(matches!(
            coordinator.capture_barrier(history, Duration::from_secs(1)),
            Err(CoordinatorError::Poisoned(_))
        ));
        let permit = coordinator.operation().unwrap();
        db.put(b"primary-still-writable", b"yes").unwrap();
        drop(permit);
        assert_eq!(
            db.get(b"primary-still-writable").unwrap(),
            Some(b"yes".to_vec())
        );
    }
    #[test]
    fn durable_phase_success_allows_capture_and_failure_marks_before_release() {
        let (_root, db, coordinator, history) = fixture();
        let outer = coordinator.operation().unwrap();
        let phase = coordinator
            .begin_durable_phase("test semantic partial failure")
            .unwrap();
        db.put(b"complete", b"one").unwrap();
        phase.complete();
        drop(outer);
        assert_eq!(
            coordinator
                .capture_barrier(history, Duration::from_secs(1))
                .unwrap()
                .source_sequence(),
            1
        );
        let phase = coordinator
            .begin_durable_phase("test semantic partial failure")
            .unwrap();
        db.put(b"partial", b"two").unwrap();
        let next = coordinator.clone();
        let waiter = JoinedThread::spawn(move || {
            matches!(
                next.capture_barrier(history, Duration::from_secs(2)),
                Err(CoordinatorError::Poisoned(_))
            )
        });
        wait_pending(&coordinator);
        drop(phase);
        assert!(
            waiter.join().unwrap(),
            "waiting capture crossed an uncertain phase"
        );
        assert!(
            coordinator.operation().is_ok(),
            "primary admission must remain available"
        );
    }

    #[test]
    fn unarmed_prewrite_error_does_not_disable_certification() {
        let (_root, _db, coordinator, history) = fixture();
        fn ordinary_error(coordinator: &PhysicalSourceCoordinator) -> Result<()> {
            let _operation = coordinator.operation()?;
            // No durable phase was armed because prewrite validation refused.
            Err(CoordinatorError::WrongSource)
        }
        assert!(ordinary_error(&coordinator).is_err());
        assert!(coordinator
            .capture_barrier(history, Duration::from_secs(1))
            .is_ok());
    }
}
