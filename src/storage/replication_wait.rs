//! The commit path's synchronous-replication wait (spec P2.1 §2).
//!
//! # Why on the physical sequence
//!
//! A physical standby receives the primary's RocksDB write groups and
//! acknowledges the RocksDB sequence it has applied and made durable. So a
//! commit captures `db.latest_sequence_number()` right after its data write —
//! a conservative upper bound of its own write group, since a concurrent
//! writer can only raise it — and waits until enough standbys acknowledge
//! that number ([`barrier`], then [`wait`]). The logical WAL LSN the native
//! v3 channel acknowledged named another number space and is gone.
//!
//! # Why a failed wait is reported, not returned
//!
//! Under `sync_timeout_action = "fail"` the client must see an error for a
//! commit its standbys never acknowledged. That commit is already applied and
//! durable here, and every caller of `Transaction::commit` treats an error as
//! "nothing committed" — it replays its ART undo log and skips its post-commit
//! cache invalidation — so the commit path must not return one. It records
//! the failure instead, in the slot of the client request that ran it
//! ([`report_scope`], a task-local around one PostgreSQL wire message); the
//! wire turns it into the statement's error where it would have reported
//! success ([`take_unconfirmed`]). A commit with no client request around it
//! (an embedded caller, a background job) logs it.

use rocksdb::DB;
use std::cell::RefCell;
use std::future::Future;

tokio::task_local! {
    /// The unconfirmed-commit message of the client request being served.
    static UNCONFIRMED: RefCell<Option<String>>;
}

/// The physical sequence a commit must see acknowledged, read right after
/// its data write — `None` unless this node is a primary whose commits wait
/// for standbys, so every other node pays one atomic load.
pub(crate) fn barrier(db: &DB) -> Option<u64> {
    #[cfg(feature = "ha-tier1")]
    {
        crate::replication::ha_state::ha_state()
            .sync_wait_enabled()
            .then(|| db.latest_sequence_number())
    }
    #[cfg(not(feature = "ha-tier1"))]
    {
        let _ = db;
        None
    }
}

/// Wait (bounded by `[replication] sync_timeout_ms`) until enough standbys
/// acknowledge `barrier` (`HAStateRegistry::wait_for_sequence`). Never fails
/// the commit: an unconfirmed commit under `sync_timeout_action = "fail"` is
/// reported to the client request that ran it (module documentation).
pub(crate) fn wait(barrier: Option<u64>) {
    #[cfg(feature = "ha-tier1")]
    if let Some(sequence) = barrier {
        let outcome = blocking(|| crate::replication::ha_state::ha_state().wait_for_sequence(sequence));
        if let Err(unconfirmed) = outcome {
            report(unconfirmed.to_string());
        }
    }
    #[cfg(not(feature = "ha-tier1"))]
    {
        let _ = barrier;
    }
}

/// Run `f`, which may block for the sync timeout, without starving the async
/// runtime: on a multi-threaded runtime's worker the worker's other tasks —
/// among them the replication sessions whose acknowledgements `f` waits for —
/// move to another thread first.
#[cfg(feature = "ha-tier1")]
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if matches!(handle.runtime_flavor(), tokio::runtime::RuntimeFlavor::MultiThread) => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

#[cfg(feature = "ha-tier1")]
fn report(message: String) {
    let recorded = UNCONFIRMED.try_with(|slot| *slot.borrow_mut() = Some(message.clone()));
    if recorded.is_err() {
        tracing::error!("{message} (no client request to report it to; the commit stands)");
    }
}

/// Serve one client request (a PostgreSQL wire message) with its own
/// unconfirmed-commit slot. A slot never outlives its request, so a failure
/// is never reported to the next one.
pub async fn report_scope<F: Future>(future: F) -> F::Output {
    UNCONFIRMED.scope(RefCell::new(None), future).await
}

/// The unconfirmed commit the current request made, if any (taken: each is
/// reported once). Always `None` outside a [`report_scope`].
pub fn take_unconfirmed() -> Option<String> {
    UNCONFIRMED.try_with(|slot| slot.borrow_mut().take()).ok().flatten()
}

#[cfg(all(test, feature = "ha-tier1"))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_report_reaches_only_its_own_request() {
        assert_eq!(take_unconfirmed(), None, "no slot outside a request");
        let seen = report_scope(async {
            report("first".into());
            take_unconfirmed()
        })
        .await;
        assert_eq!(seen.as_deref(), Some("first"));
        let next = report_scope(async { take_unconfirmed() }).await;
        assert_eq!(next, None, "the next request starts clean");
        let once = report_scope(async {
            report("second".into());
            (take_unconfirmed(), take_unconfirmed())
        })
        .await;
        assert_eq!(once, (Some("second".into()), None), "reported once");
        // Outside any request it is logged, never stored.
        report("orphan".into());
        assert_eq!(take_unconfirmed(), None);
    }

    #[test]
    fn a_node_that_is_not_a_serving_primary_never_reads_a_barrier() {
        let directory = tempfile::tempdir().unwrap();
        let db = DB::open_default(directory.path()).unwrap();
        assert_eq!(barrier(&db), None);
        wait(None);
    }
}
