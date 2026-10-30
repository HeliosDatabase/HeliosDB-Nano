//! A statement never stalls its server (sprinter ccebc95977f1).
//!
//! # Why
//!
//! The server's runtime is Tokio's multi-thread scheduler, on which exactly
//! one parked worker owns the I/O and timer driver. When client bytes wake
//! that worker, it runs the woken connection task from its LIFO slot and wakes
//! no other worker for a single task. Statement execution is synchronous
//! inside that task, so for the whole statement nobody polled epoll or the
//! timers: new connections were not accepted, other sessions' messages were
//! not read, the physical source's frame I/O was not served (a standby flapped
//! `reconnecting` behind a statement longer than 30 s) and no timer fired.
//!
//! `tokio::task::block_in_place` is the fix: it moves the worker's queued
//! tasks — the LIFO slot included — to the run queue, hands the worker's core
//! (and with it the driver) to a replacement thread, and runs the closure on
//! THIS thread. The closure staying on the calling thread is what keeps the
//! engine's thread-local state valid (`SESSION_SCOPED_STATE`, the
//! per-statement guards, `temp_namespace_guard`, `write_volume::stmt_scope`)
//! and the `replication_wait` task-local readable — which is why a statement
//! is never handed to `spawn_blocking`.
//!
//! # Two entry points
//!
//! * [`off_worker`] — always: the bounded blocking waits that must not pin a
//!   worker whatever the caller (advisory-lock waits, the synchronous-commit
//!   replication wait).
//! * [`offload_statement`] — where a statement actually EXECUTES: the
//!   execution entry points of `EmbeddedDatabase`, COPY, COMMIT/ROLLBACK, a
//!   catalog interception that matched, the REST/MCP requests that reach the
//!   engine without SQL. Never the cheap steps around them (the result-cache
//!   probe, Parse/Describe planning, `BEGIN`, a per-statement savepoint), and
//!   never a BOUNDED read: the hot read funnels (`query_with_schema`,
//!   `query_params_inner_unguarded`) answer a primary-key point read — one
//!   probe, no user code — on the worker and offload only their general path.
//!   A handoff costs microseconds (the worker's core moves to another thread),
//!   which a point read cannot afford and a long statement does not notice.
//!   So an executed statement pays at most ONE handoff; what runs inside an
//!   offloaded call nests for free. Gated by a process-wide switch that is
//!   OFF by default, so an embedder's runtime sees no change, and that the
//!   server turns ON before its listeners start ([`enable_for_server`]) unless
//!   `HELIOSDB_NANO_STATEMENT_OFFLOAD` says off. An embedder that wants it
//!   calls [`set_statement_offload`].
//!
//! # Never panics
//!
//! `block_in_place` panics on a `current_thread` runtime, and on a
//! multi-thread runtime's `block_on` thread while it polls a
//! `tokio::task::LocalSet`. The first is recognised up front (the runtime
//! flavour) and runs `f` inline. The second cannot be recognised through
//! Tokio's public API, so that refusal — raised before `f` runs, and
//! recognised by Tokio's own message — is caught and `f` runs inline: off a
//! worker thread `block_in_place` has no worker to hand over, so nothing is
//! lost. Every other panic unwinds unchanged (a panic of `f`, or a failure
//! after Tokio may already have handed the worker's core over). The
//! process's panic hook still prints each refusal. Under `panic = "abort"`
//! the refusal aborts the process instead: only an embedder can meet it (the
//! server polls no `LocalSet`), and for an embedder the per-statement switch
//! is off by default — the always-on waits above behaved the same before.
//!
//! Nested calls are nearly free: inside an outer `block_in_place` the
//! worker's core is already handed over, and the inner call costs a few
//! thread-local reads.

use std::sync::atomic::{AtomicBool, Ordering};

/// The environment variable that sets the server's switch: `off`, `0`,
/// `false`, `no` or `disabled` keep statements on the worker that serves them
/// (an A/B and emergency switch); `on`, `1`, `true`, `yes`, `enabled` or unset
/// offload them. Case-insensitive; any other value warns and stays on.
pub const STATEMENT_OFFLOAD_ENV: &str = "HELIOSDB_NANO_STATEMENT_OFFLOAD";

/// What Tokio panics with when `block_in_place` is refused ("Never panics").
const BLOCK_IN_PLACE_REFUSED: &str = "can call blocking only when running on the multi-threaded runtime";

/// `internal-tests`: how many times work has left the async worker — an
/// outermost `block_in_place` that ran its closure; nested calls do not count
/// — so a test can tell a bounded read (none) from a general one (one).
#[cfg(feature = "internal-tests")]
static HANDOFFS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "internal-tests")]
thread_local! {
    /// Inside a counted handoff on this thread.
    static IN_HANDOFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `internal-tests`: the handoffs counted so far in this process.
#[cfg(feature = "internal-tests")]
pub fn handoffs() -> u64 {
    HANDOFFS.load(Ordering::Relaxed)
}

/// `internal-tests`: counts one handoff unless it is nested in another.
#[cfg(feature = "internal-tests")]
struct CountedHandoff(bool);

#[cfg(feature = "internal-tests")]
impl CountedHandoff {
    fn enter() -> Self {
        let outermost = !IN_HANDOFF.with(|inside| inside.replace(true));
        if outermost {
            HANDOFFS.fetch_add(1, Ordering::Relaxed);
        }
        Self(outermost)
    }
}

#[cfg(feature = "internal-tests")]
impl Drop for CountedHandoff {
    fn drop(&mut self) {
        if self.0 {
            IN_HANDOFF.with(|inside| inside.set(false));
        }
    }
}

/// The process-wide per-statement switch (module documentation): OFF until
/// the server ([`enable_for_server`]) or an embedder
/// ([`set_statement_offload`]) turns it on.
static STATEMENT_OFFLOAD: AtomicBool = AtomicBool::new(false);

/// Turn [`offload_statement`] on or off for the whole process.
pub fn set_statement_offload(on: bool) {
    STATEMENT_OFFLOAD.store(on, Ordering::Relaxed);
}

/// Is [`offload_statement`] on?
pub fn statement_offload() -> bool {
    STATEMENT_OFFLOAD.load(Ordering::Relaxed)
}

/// The server's setting, made once before its listeners start: on, unless
/// [`STATEMENT_OFFLOAD_ENV`] says off — which is logged, because every long
/// statement then stalls the server again. A value it does not recognise is
/// logged too, and leaves the offload on.
pub fn enable_for_server() {
    let on = match std::env::var_os(STATEMENT_OFFLOAD_ENV) {
        None => true,
        Some(value) => match value.to_str().and_then(parse_switch) {
            Some(on) => on,
            None => {
                tracing::warn!(
                    "{STATEMENT_OFFLOAD_ENV}={value:?} is not off/0/false/no/disabled or \
                     on/1/true/yes/enabled; statements stay off the async worker (sprinter ccebc95977f1)"
                );
                true
            }
        },
    };
    if !on {
        tracing::warn!(
            "{STATEMENT_OFFLOAD_ENV} is off: statements run on the async worker that serves them, so a long \
             statement delays new connections, other sessions and replication I/O until it ends \
             (sprinter ccebc95977f1)"
        );
    }
    set_statement_offload(on);
}

/// [`STATEMENT_OFFLOAD_ENV`]'s spellings; `None` for any other value.
fn parse_switch(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" | "0" | "false" | "no" | "disabled" => Some(false),
        "on" | "1" | "true" | "yes" | "enabled" => Some(true),
        _ => None,
    }
}

/// Run one statement's synchronous work: [`off_worker`] while the
/// process-wide switch is on, inline otherwise. One relaxed load when off.
#[inline]
pub fn offload_statement<T>(f: impl FnOnce() -> T) -> T {
    offload_if(STATEMENT_OFFLOAD.load(Ordering::Relaxed), f)
}

/// [`offload_statement`] with the switch given (the unit tests use it, so
/// they never flip the process-wide switch under concurrently running tests).
#[inline]
fn offload_if<T>(on: bool, f: impl FnOnce() -> T) -> T {
    if on {
        off_worker(f)
    } else {
        f()
    }
}

/// Run `f` on this thread without stranding the tasks of the async worker it
/// runs on: on a multi-thread runtime, `tokio::task::block_in_place(f)`;
/// otherwise (no runtime, a `current_thread` runtime, a blocking-pool thread)
/// `f()` inline. Never panics of its own (module documentation); any other
/// panic, `f`'s own included, unwinds unchanged.
pub fn off_worker<T>(f: impl FnOnce() -> T) -> T {
    let multi_thread = tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| matches!(handle.runtime_flavor(), tokio::runtime::RuntimeFlavor::MultiThread));
    if !multi_thread {
        return f();
    }
    let mut pending = Some(f);
    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tokio::task::block_in_place(|| {
            #[cfg(feature = "internal-tests")]
            let _counted = CountedHandoff::enter();
            pending.take().map(|f| f())
        })
    }));
    match attempt {
        Ok(Some(value)) => value,
        Err(payload) => match pending.take() {
            // Refused before it ran `f` (a `LocalSet` polled by a
            // multi-thread runtime's `block_on`): run it here. Asked again
            // next time — a refusal is no property of the thread.
            Some(f) if is_block_in_place_refusal(payload.as_ref()) => {
                tracing::debug!("block_in_place refused here; the statement runs inline (sprinter ccebc95977f1)");
                f()
            }
            // `f` panicked, or Tokio failed after it may have handed the
            // worker's core over: unwind on exactly as without this helper.
            _ => std::panic::resume_unwind(payload),
        },
        // `block_in_place` returns only after running its closure, which takes `f`.
        Ok(None) => unreachable!("block_in_place returned without running its closure"),
    }
}

/// Is `payload` Tokio's `block_in_place` refusal, and nothing else?
fn is_block_in_place_refusal(payload: &(dyn std::any::Any + Send)) -> bool {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&'static str>().copied())
        .is_some_and(|message| message.contains(BLOCK_IN_PLACE_REFUSED))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn multi_thread_one_worker() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    /// On the ONLY worker of a multi-thread runtime: spawn a probe task (it
    /// lands in this worker's LIFO slot, where no other thread can steal it),
    /// then run a statement that waits up to `wait` for the probe through
    /// `offload_if(on, …)`. Returns whether the probe ran WHILE the statement
    /// was still running, and the statement's thread against the caller's.
    fn probe_runs_during_statement(on: bool, wait: Duration) -> (bool, bool) {
        let runtime = multi_thread_one_worker();
        runtime.block_on(async move {
            tokio::spawn(async move {
                let ran = Arc::new(AtomicBool::new(false));
                let probe = {
                    let ran = Arc::clone(&ran);
                    tokio::spawn(async move { ran.store(true, Ordering::SeqCst) })
                };
                let caller = std::thread::current().id();
                let (seen, same_thread) = offload_if(on, || {
                    let deadline = Instant::now() + wait;
                    while !ran.load(Ordering::SeqCst) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    (ran.load(Ordering::SeqCst), std::thread::current().id() == caller)
                });
                probe.await.unwrap();
                (seen, same_thread)
            })
            .await
            .unwrap()
        })
    }

    #[test]
    fn outside_any_runtime_it_runs_inline() {
        let caller = std::thread::current().id();
        assert_eq!(off_worker(|| std::thread::current().id()), caller);
        assert_eq!(offload_if(true, || std::thread::current().id()), caller);
        assert_eq!(offload_if(false, || 7), 7);
    }

    #[test]
    fn on_a_current_thread_runtime_it_runs_inline_and_never_panics() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let caller = std::thread::current().id();
        let (thread, value) = runtime.block_on(async { offload_if(true, || (std::thread::current().id(), 42)) });
        assert_eq!((thread, value), (caller, 42));
        // A spawned task of a current-thread runtime, too.
        let value = runtime.block_on(async { tokio::spawn(async { off_worker(|| 5) }).await.unwrap() });
        assert_eq!(value, 5);
    }

    /// The defect itself, and the fix: with the switch on, a task queued on
    /// the only worker runs while the statement holds that worker — and the
    /// statement still ran on the caller's own thread (its thread-locals).
    #[test]
    fn switch_on_a_queued_task_runs_while_the_statement_holds_the_only_worker() {
        let (seen, same_thread) = probe_runs_during_statement(true, Duration::from_secs(10));
        assert!(seen, "the probe never ran while the statement held the worker");
        assert!(same_thread, "the statement must run on the calling thread");
    }

    /// Switch off: the statement runs inline, so the queued task waits for it
    /// — the stall, which proves the test above can fail.
    #[test]
    fn switch_off_the_statement_runs_inline_and_the_queued_task_waits() {
        let (seen, same_thread) = probe_runs_during_statement(false, Duration::from_millis(300));
        assert!(!seen, "with the switch off the statement runs inline on the worker");
        assert!(same_thread);
    }

    #[test]
    fn nested_calls_are_fine_on_a_worker_and_off_it() {
        assert_eq!(offload_if(true, || offload_if(true, || off_worker(|| 3))), 3);
        let runtime = multi_thread_one_worker();
        let value = runtime.block_on(async {
            tokio::spawn(async { offload_if(true, || offload_if(true, || off_worker(|| 4))) })
                .await
                .unwrap()
        });
        assert_eq!(value, 4);
        // On the multi-thread runtime's `block_on` thread (no worker core).
        assert_eq!(runtime.block_on(async { off_worker(|| off_worker(|| 6)) }), 6);
    }

    /// A `LocalSet` polled by a multi-thread runtime's `block_on`, where
    /// `block_in_place` refuses: the helper runs `f` inline and returns, no
    /// panic reaches the caller (the panic hook prints each refusal) — and a
    /// refusal is asked again each time, never remembered.
    #[test]
    fn inside_a_local_set_on_a_multi_thread_block_on_it_runs_inline() {
        let runtime = multi_thread_one_worker();
        let local = tokio::task::LocalSet::new();
        let caller = std::thread::current().id();
        let (first, second) = runtime.block_on(local.run_until(async {
            let first = off_worker(|| std::thread::current().id());
            let second = tokio::task::spawn_local(async { offload_if(true, || std::thread::current().id()) })
                .await
                .unwrap();
            (first, second)
        }));
        assert_eq!((first, second), (caller, caller));
    }

    /// A panic of `f` unwinds unchanged through the helper, on a worker.
    #[test]
    fn a_panic_of_the_statement_unwinds_unchanged() {
        let runtime = multi_thread_one_worker();
        let message = runtime.block_on(async {
            tokio::spawn(async {
                let caught = std::panic::catch_unwind(|| off_worker(|| -> u8 { panic!("statement panicked") }));
                let payload = caught.expect_err("the panic must reach the caller");
                payload.downcast_ref::<&str>().map(|text| (*text).to_string())
            })
            .await
            .unwrap()
        });
        assert_eq!(message.as_deref(), Some("statement panicked"));
    }

    /// Only Tokio's refusal is taken for one: a panic that merely resembles
    /// a failure of `block_in_place` unwinds.
    #[test]
    fn only_the_refusal_message_counts_as_a_refusal() {
        let refusal: Box<dyn std::any::Any + Send> = Box::new(BLOCK_IN_PLACE_REFUSED.to_string());
        assert!(is_block_in_place_refusal(refusal.as_ref()));
        let literal: Box<dyn std::any::Any + Send> = Box::new(BLOCK_IN_PLACE_REFUSED);
        assert!(is_block_in_place_refusal(literal.as_ref()));
        let other: Box<dyn std::any::Any + Send> = Box::new("failed to spawn a thread".to_string());
        assert!(!is_block_in_place_refusal(other.as_ref()));
        let opaque: Box<dyn std::any::Any + Send> = Box::new(7_u8);
        assert!(!is_block_in_place_refusal(opaque.as_ref()));
    }

    #[test]
    fn the_switch_reads_every_documented_spelling() {
        for off in ["off", "OFF", " 0 ", "false", "No", "disabled"] {
            assert_eq!(parse_switch(off), Some(false), "{off:?}");
        }
        for on in ["on", "1", "TRUE", "yes", "Enabled"] {
            assert_eq!(parse_switch(on), Some(true), "{on:?}");
        }
        for unknown in ["", "maybe", "offf", "2"] {
            assert_eq!(parse_switch(unknown), None, "{unknown:?}");
        }
    }
}
