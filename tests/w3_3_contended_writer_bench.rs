//! W3.3 — contended-writer microbench: documents TODAY's pessimistic same-row
//! write-conflict behavior (the lock-acquire spin the design doc
//! `docs/plans/PERF_STABILITY_2026_07/W3_3_DESIGN.md` proposes to replace with
//! statement-level retry).
//!
//! Two writers contend for the same row. The holder keeps an open transaction's
//! write lock; the waiter's UPDATE blocks in `LockManager::acquire_lock`
//! (`storage/lock_manager.rs`) and — because a same-row waiter can NEVER be
//! granted the lock while the holder stays open (the in-code futility note on
//! `with_default_timeout`) — the wait always runs to the full timeout and
//! returns the typed `Error::WriteConflict` (PostgreSQL SQLSTATE 40001). This
//! is the path no existing test covers: `conflict_detection_tests.rs` commits
//! the first writer BEFORE the second writes, so its conflict surfaces at
//! COMMIT via the optimistic registry, never at the lock.
//!
//! `#[ignore]`d: it deliberately spends ~timeout per conflict, so the
//! coordinator runs it explicitly:
//!   cargo test --test w3_3_contended_writer_bench -- --ignored --nocapture
//! Bounded: 2 rows, a short 300ms spin timeout, 8 conflicts ⇒ well under 30s.

use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{EmbeddedDatabase, Error};
use std::time::Instant;

const CONFLICTS: usize = 8;
/// Bound the futile spin so the whole run stays well under 30s. Production
/// default is 1000 (`NANO_LOCK_TIMEOUT_MS`, `with_default_timeout`).
const TIMEOUT_MS: u64 = 300;

#[test]
#[ignore = "contended-writer microbench: spends ~TIMEOUT_MS per same-row conflict; run explicitly with --ignored --nocapture"]
fn contended_writer_spin_documents_today() {
    // The LockManager reads this at construction; set it before the db is built.
    std::env::set_var("NANO_LOCK_TIMEOUT_MS", TIMEOUT_MS.to_string());

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE contended (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("INSERT INTO contended (id, v) VALUES (1, 100)").unwrap();
    db.execute("INSERT INTO contended (id, v) VALUES (2, 200)").unwrap();

    let mut latencies_ms: Vec<u128> = Vec::with_capacity(CONFLICTS);
    let mut observed_timeout_ms: u64 = 0;

    for i in 0..CONFLICTS {
        let holder = db
            .create_session(&format!("holder_{i}"), IsolationLevel::ReadCommitted)
            .unwrap();
        let waiter = db
            .create_session(&format!("waiter_{i}"), IsolationLevel::ReadCommitted)
            .unwrap();

        // Holder opens a transaction and takes the row's write lock; it never
        // commits, so the lock is held for the whole conflict window.
        db.begin_transaction_for_session(holder).unwrap();
        db.execute_in_session(holder, "UPDATE contended SET v = v + 1 WHERE id = 1")
            .unwrap();

        // Waiter tries the same row: blocks in acquire_lock, then times out.
        db.begin_transaction_for_session(waiter).unwrap();
        let start = Instant::now();
        let err = db
            .execute_in_session(waiter, &format!("UPDATE contended SET v = {i} WHERE id = 1"))
            .expect_err("same-row waiter must fail with a write conflict");
        latencies_ms.push(start.elapsed().as_millis());

        match err {
            Error::WriteConflict {
                table,
                row,
                holder_txn,
                waiter_txn,
                waited_ms,
            } => {
                assert_eq!(table, "contended", "conflict must name the table");
                assert!(!row.is_empty(), "conflict must carry a row identity");
                assert_ne!(
                    holder_txn, waiter_txn,
                    "holder and waiter must be distinct transactions"
                );
                assert_eq!(waited_ms, TIMEOUT_MS, "conflict reports the spin timeout");
                observed_timeout_ms = waited_ms;
            }
            other => panic!("expected typed WriteConflict, got: {other}"),
        }

        // Release the holder's write lock so the next iteration starts clean.
        db.rollback_transaction_for_session(holder).unwrap();
        let _ = db.destroy_session(holder);
        let _ = db.destroy_session(waiter);
    }

    latencies_ms.sort_unstable();
    let n = latencies_ms.len();
    let min = latencies_ms[0];
    let max = latencies_ms[n - 1];
    let median = latencies_ms[n / 2];
    let mean = latencies_ms.iter().sum::<u128>() / n as u128;

    println!("=== W3.3 contended-writer microbench (today's pessimistic spin) ===");
    println!("conflicts        : {n}");
    println!("spin timeout (ms): {observed_timeout_ms}   (production default 1000)");
    println!("latency ms       : min={min} median={median} mean={mean} max={max}");
    println!("all {n} conflicts resolved via full-timeout spin → typed WriteConflict (SQLSTATE 40001)");

    // Futility, measured: every conflict burns ~the whole timeout — it was never
    // granted early. Only the lower bound is load-bearing; an upper bound would
    // flake on this OOM-prone host (a scheduler/GC stall on one iteration can push
    // latency arbitrarily high without disproving futility), and the run is already
    // bounded <30s structurally by CONFLICTS × TIMEOUT_MS.
    let lo = (TIMEOUT_MS / 2) as u128;
    for (i, &ms) in latencies_ms.iter().enumerate() {
        assert!(
            ms >= lo,
            "conflict {i} latency {ms}ms below {lo} — spin should run ~to timeout, not be granted early"
        );
    }
}
