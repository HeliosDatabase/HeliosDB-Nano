//! Locking + tenant-limit hygiene (sprinter batch F).
//!
//! Two defects, both of them a limit that does not hold for the reason the
//! code claims it does:
//!
//! 1. **F1 / 68b70030ba28 — `LockGuard` had no re-entrancy refcount.** After
//!    the lock manager was made re-entrant for the holder (a self-deadlock
//!    fix), one transaction can hold N guards on one resource — but
//!    `LockGuard::drop` released the holder on the FIRST drop. Nothing
//!    observable broke on `main` because every guard is parked in
//!    `Transaction::acquired_locks` until commit/rollback and they drop
//!    together; any early-drop path (statement-scoped locks, a savepoint
//!    rollback releasing a subset) would have turned it into a live fail-open,
//!    handing a row to another transaction while the first still held it. The
//!    lock-manager-level proof lives in `src/storage/lock_manager.rs`'s unit
//!    tests (they can reach the acquisition counts directly); the test below
//!    is the same failure reproduced through the public API.
//!
//! 2. **F3 / c837352dabef (SECURITY) — tenant `max_qps` was a LIFETIME quota.**
//!    `check_quota("qps")` compared `queries_this_window < max_qps` and
//!    `record_query` only ever incremented that counter.
//!    `TenantManager::reset_qps_window` existed, and so did two drivers for it
//!    (`EmbeddedDatabase::start_qps_reset_task` and `reset_all_qps_windows`) —
//!    but a caller census found ZERO callers of either anywhere in `src/`,
//!    `tests/`, `benches/` or `examples/`. The window therefore never reset in
//!    a running process: a tenant got `max_qps` statements for the LIFE of the
//!    process and was refused every statement afterwards. The window is now
//!    evaluated lazily against a configured length inside the quota check, so
//!    it holds on the embedded path too (no tokio runtime, no background task
//!    to starve or forget).
//!
//! F2 (fab4dd25bf29, the flaky `test_deadlock_detection_simple`) is a unit test
//! by nature and was fixed in place in `src/storage/lock_manager.rs`.
//!
//! No test here sleeps for a full second: the QPS window length is a parameter
//! (`[resource_quotas].tenant_qps_window_ms`, default 1000 ms), so the refill
//! is observed with a short window and a sleep of a few tens of milliseconds.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::config::ResourceQuotaConfig;
use heliosdb_nano::storage::{LockManager, LockType};
use heliosdb_nano::tenant::{IsolationMode, ResourceLimits, TenantManager};
use heliosdb_nano::EmbeddedDatabase;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// F1 — a partially released re-entrant lock must stay held
// ---------------------------------------------------------------------------

/// The early drop, through the public lock-manager API: txn 7 takes the row
/// twice and releases once. Pre-fix, that single release emptied `holders` and
/// reaped the lock entry, so `is_locked` went false and txn 8 was GRANTED a row
/// txn 7 was still holding a live guard for.
#[test]
fn f1_partial_release_of_a_re_entrant_lock_keeps_the_row_locked() {
    // A short timeout so the contending acquire fails fast instead of spinning.
    let manager = Arc::new(LockManager::new(150));

    let outer = manager
        .acquire_lock("data:accounts:1", 7, LockType::Write)
        .expect("first acquire");
    let inner = manager
        .acquire_lock("data:accounts:1", 7, LockType::Write)
        .expect("re-entrant acquire must be granted to the holder");

    // The statement / savepoint scope ends and releases ONE of the two guards.
    drop(inner);

    assert!(
        manager.is_locked("data:accounts:1"),
        "*** the row was released while its transaction still held a guard ***"
    );
    assert_eq!(manager.get_lock_holders("data:accounts:1"), vec![7]);
    assert!(
        manager.acquire_lock("data:accounts:1", 8, LockType::Write).is_err(),
        "*** another transaction was granted a row txn 7 still holds ***"
    );

    // The last release is the one that frees it.
    drop(outer);
    assert!(!manager.is_locked("data:accounts:1"));
    let other = manager
        .acquire_lock("data:accounts:1", 8, LockType::Write)
        .expect("fully released rows are available again");
    drop(other);
}

// ---------------------------------------------------------------------------
// F3 — max_qps is a RATE, not a lifetime quota
// ---------------------------------------------------------------------------

fn tenant_with_qps(manager: &TenantManager, max_qps: usize) -> heliosdb_nano::tenant::TenantId {
    let tenant = manager.register_tenant("batch-f".to_string(), IsolationMode::SharedSchema);
    manager
        .update_resource_limits(
            tenant.id,
            ResourceLimits {
                max_storage_bytes: 100_000_000,
                max_connections: 50,
                max_qps,
            },
        )
        .expect("limits");
    tenant.id
}

/// THE PROOF. A tenant that exhausts its budget is throttled, and is served
/// again once the window elapses — with no background task running and without
/// anyone calling `reset_qps_window`.
///
/// Against the pre-fix behaviour (nothing ever resets `queries_this_window`)
/// this fails at the `*** lifetime quota ***` assertion in the middle: the
/// tenant stays refused forever, for the life of the process.
#[test]
fn f3_qps_budget_refills_when_the_window_elapses() {
    // The window is switched between a long and a short length so that EVERY
    // assertion is deterministic under an arbitrarily slow runner: the budget is
    // spent inside an hour-long window (no stall can refill it), the window is
    // then shortened and slept past (a stall only makes MORE time elapse), and
    // the refilled window is frozen again before it is re-tested. The only
    // timing assumption left is that `sleep(60ms)` sleeps at least 40 ms.
    let manager = TenantManager::with_qps_window(Duration::from_secs(3600));
    let tenant_id = tenant_with_qps(&manager, 2);

    // Window 1: the budget is spent, then enforced.
    assert!(manager.record_query(tenant_id).is_ok(), "query 1 of 2");
    assert!(manager.record_query(tenant_id).is_ok(), "query 2 of 2");
    let refused = manager.record_query(tenant_id).expect_err("query 3 exceeds max_qps");
    assert!(refused.contains("rate limit exceeded"), "unexpected error: {refused}");

    // Let the window expire. 60 ms — deliberately well under a second, which is
    // only possible because the window length is a parameter.
    manager.set_qps_window(Duration::from_millis(40));
    std::thread::sleep(Duration::from_millis(60));

    // Window 2: served again. No background task, no manual `reset_qps_window`.
    assert!(
        manager.record_query(tenant_id).is_ok(),
        "*** max_qps behaved as a lifetime quota: the window never reset ***"
    );

    // Freeze the window that the query above just opened, so the remaining
    // assertions cannot be perturbed by a slow runner.
    manager.set_qps_window(Duration::from_secs(3600));
    assert!(manager.record_query(tenant_id).is_ok(), "the full budget is back");
    assert!(
        manager.record_query(tenant_id).is_err(),
        "the refilled budget is still bounded by max_qps"
    );

    // The counter reflects the CURRENT window, not the lifetime total, while
    // `total_queries` keeps counting everything served.
    let tracking = manager.get_quota_tracking(tenant_id).expect("tracking");
    assert_eq!(tracking.queries_this_window, 2);
    assert_eq!(tracking.total_queries, 4);
    assert_eq!(tracking.qps_hwm, 2);
}

/// The other half of "it is a rate": the window must not roll early, or
/// `max_qps` would be unenforceable. A long window keeps the tenant refused.
#[test]
fn f3_budget_is_not_refilled_before_the_window_elapses() {
    let manager = TenantManager::with_qps_window(Duration::from_secs(3600));
    let tenant_id = tenant_with_qps(&manager, 2);

    assert!(manager.record_query(tenant_id).is_ok());
    assert!(manager.record_query(tenant_id).is_ok());
    for attempt in 0..5 {
        assert!(
            manager.record_query(tenant_id).is_err(),
            "attempt {attempt} must stay refused inside the same window"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    // `check_quota` must agree with `record_query` — it is the same window.
    assert!(!manager.check_quota(tenant_id, "qps"));
}

/// `check_quota` is the public predicate, so it rolls the window too: a caller
/// that only ever asks "may this tenant run a query?" must not be told "no"
/// forever after one exhausted window.
#[test]
fn f3_check_quota_sees_the_new_window() {
    let manager = TenantManager::with_qps_window(Duration::from_secs(3600));
    let tenant_id = tenant_with_qps(&manager, 1);

    assert!(manager.check_quota(tenant_id, "qps"));
    manager.record_query(tenant_id).expect("the single query in the budget");
    assert!(!manager.check_quota(tenant_id, "qps"), "budget spent");

    manager.set_qps_window(Duration::from_millis(40));
    std::thread::sleep(Duration::from_millis(60));
    assert!(
        manager.check_quota(tenant_id, "qps"),
        "a new window restores the budget"
    );
}

/// The manual reset path is public API with doc examples and must keep working
/// unchanged — this is the shape the three pre-existing test sites rely on
/// (`src/tenant/mod.rs`, `tests/multi_tenancy_integration.rs`,
/// `tests/multi_tenancy_tests.rs`).
#[test]
fn f3_manual_reset_still_works_inside_a_long_window() {
    let manager = TenantManager::with_qps_window(Duration::from_secs(3600));
    let tenant_id = tenant_with_qps(&manager, 2);

    manager.record_query(tenant_id).unwrap();
    manager.record_query(tenant_id).unwrap();
    assert!(manager.record_query(tenant_id).is_err());

    manager.reset_qps_window(tenant_id).expect("manual reset");
    assert!(
        manager.record_query(tenant_id).is_ok(),
        "an explicit reset opens a new window immediately"
    );
}

/// `EmbeddedDatabase::reset_all_qps_windows` had zero callers in the tree; it
/// stays public API (removing it would be a breaking change), so it gets a
/// caller here that proves it still drives every tenant's window.
#[test]
fn f3_reset_all_qps_windows_still_drives_every_tenant() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    // A long window so the ONLY thing that can refill the budget is the call
    // under test.
    db.tenant_manager.set_qps_window(Duration::from_secs(3600));

    let a = tenant_with_qps(&db.tenant_manager, 1);
    let b = tenant_with_qps(&db.tenant_manager, 1);
    db.tenant_manager.record_query(a).unwrap();
    db.tenant_manager.record_query(b).unwrap();
    assert!(db.tenant_manager.record_query(a).is_err());
    assert!(db.tenant_manager.record_query(b).is_err());

    db.reset_all_qps_windows();

    assert!(db.tenant_manager.record_query(a).is_ok());
    assert!(db.tenant_manager.record_query(b).is_ok());
}

/// The window length is a config parameter and it reaches the component that
/// enforces it — the failure mode this whole item is about was a knob that
/// existed but was never wired to anything.
#[test]
fn f3_window_length_comes_from_config() {
    let cfg = ResourceQuotaConfig {
        tenant_qps_window_ms: 250,
        ..ResourceQuotaConfig::default()
    };
    assert_eq!(
        TenantManager::from_quota_config(&cfg).qps_window(),
        Duration::from_millis(250)
    );

    // The default is one second — the cadence the (never-started) background
    // task used — and an `EmbeddedDatabase` really gets it.
    assert_eq!(ResourceQuotaConfig::default().tenant_qps_window_ms, 1000);
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    assert_eq!(db.tenant_manager.qps_window(), Duration::from_secs(1));

    // A zero-length window would refill the budget on every query; config
    // validation rejects it and the constructor clamps it.
    let invalid = ResourceQuotaConfig {
        tenant_qps_window_ms: 0,
        ..ResourceQuotaConfig::default()
    };
    assert!(invalid.validate().is_err(), "a 0 ms window must not validate");
    assert_eq!(
        TenantManager::with_qps_window(Duration::from_millis(0)).qps_window(),
        Duration::from_millis(1),
        "a programmatic 0 ms window is clamped, not honoured"
    );
}
