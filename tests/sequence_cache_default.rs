//! R-D1: default sequence CACHE.
//!
//! A sequence created without an explicit CACHE clause now reserves a block of
//! DEFAULT_SEQUENCE_CACHE (32) values per durable high-water fsync (was 1),
//! turning a nextval-bound insert loop from ~one fsync per value into ~one per
//! 32. Consecutive nextvals within a session stay dense (the block is handed
//! out in order); the crash/restart gap this permits matches PostgreSQL's own
//! sequence durability granularity.

use heliosdb_nano::{EmbeddedDatabase, Value};

/// The sequence store's persistence handle is PROCESS-GLOBAL and last-writer-
/// wins (`sql::sequences::install_persistence` runs at every EmbeddedDatabase
/// construction; `nextval` upgrades the global Weak at call time). Under the
/// default multi-threaded harness a sibling test's engine can overwrite the
/// global and then drop, so this test's nextval upgrades a dead Weak →
/// "nextval requires storage context" (observed 2026-07-16 under
/// RUST_TEST_THREADS=4). Serialize the tests in this binary until the handle
/// is per-instance (filed: unify SERIAL/sequence store, per-db handle).
static SEQ_GLOBAL_HANDLE: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SEQ_GLOBAL_HANDLE.lock().unwrap_or_else(|p| p.into_inner())
}

fn nextval(db: &EmbeddedDatabase, seq: &str) -> i64 {
    let rows = db.query(&format!("SELECT nextval('{seq}')"), &[]).unwrap();
    match &rows[0].values[0] {
        Value::Int8(n) => *n,
        Value::Int4(n) => i64::from(*n),
        other => panic!("nextval returned {other:?}"),
    }
}

#[test]
fn default_cache_is_dense_within_session() {
    let _serial = serial();
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SEQUENCE s_default").unwrap();
    // Consecutive nextvals in one session are still gapless 1,2,3,… even though
    // the underlying block now reserves 32 at a time.
    let vals: Vec<i64> = (0..40).map(|_| nextval(&db, "s_default")).collect();
    assert_eq!(vals, (1..=40).collect::<Vec<_>>());
}

#[test]
fn explicit_cache_is_honored() {
    let _serial = serial();
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SEQUENCE s_explicit CACHE 1").unwrap();
    // Explicit CACHE 1 still produces dense values within a session.
    assert_eq!(nextval(&db, "s_explicit"), 1);
    assert_eq!(nextval(&db, "s_explicit"), 2);
    assert_eq!(nextval(&db, "s_explicit"), 3);
}

#[test]
fn default_cache_survives_restart_without_reuse() {
    let _serial = serial();
    // The core no-duplicate invariant: after reopening the data dir, nextval
    // must never re-hand a value already served, regardless of the larger
    // default block (a crash may skip forward, but never backward).
    let dir = tempfile::tempdir().unwrap();
    let first_after_reopen;
    let last_before;
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        db.execute("CREATE SEQUENCE s_restart").unwrap();
        last_before = nextval(&db, "s_restart"); // 1
        assert_eq!(last_before, 1);
    }
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        first_after_reopen = nextval(&db, "s_restart");
    }
    assert!(
        first_after_reopen > last_before,
        "nextval after reopen ({first_after_reopen}) must be strictly past the last served value ({last_before}) — no duplicate"
    );
}
