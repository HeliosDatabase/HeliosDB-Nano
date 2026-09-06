//! Prisma P0 spec 03 — the `pg_advisory_lock` family.
//!
//! Prisma Migrate serialises every migration run with
//! `SELECT pg_advisory_lock(72707369)` and releases it with
//! `SELECT pg_advisory_unlock(72707369)`. Before this change both raised
//! `Unknown scalar function` (SQLSTATE 42883 on the wire), so the HeliosDB
//! Partner Portal could not run a single migration against Nano.
//!
//! Every test here FAILS on the unfixed tree with
//! `Query execution error: Unknown scalar function: pg_advisory_…`.
//!
//! Two sessions stand in for two connections: advisory locks are owned by the
//! session (`EmbeddedDatabase::create_session` /
//! `EmbeddedDatabase::create_wire_session`), and the lock table is
//! process-global, exactly as PostgreSQL's shared-memory lock table is.
//!
//! Keys are unique per test: the table is process-global by design and the
//! tests in this binary run concurrently.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::sync::Arc;

fn db() -> Arc<EmbeddedDatabase> {
    Arc::new(EmbeddedDatabase::new_in_memory().expect("in-memory database"))
}

fn session(db: &EmbeddedDatabase) -> SessionId {
    db.create_session("advisory", IsolationLevel::ReadCommitted)
        .expect("session")
}

/// Run a single-value SELECT on the TEXT (simple-query) family — the same
/// entry point the PostgreSQL simple-query handler uses.
fn scalar_text(db: &EmbeddedDatabase, sid: SessionId, sql: &str) -> Value {
    let (rows, _cols) = db
        .query_with_columns_for_session(sid, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(rows.len(), 1, "{sql} must return exactly one row");
    assert_eq!(rows[0].values.len(), 1, "{sql} must return exactly one column");
    rows[0].values[0].clone()
}

/// Run a single-value SELECT on the PARAMS (extended-protocol / REST) family.
fn scalar_params(db: &EmbeddedDatabase, sid: SessionId, sql: &str) -> Value {
    let rows = db
        .query_params_for_session(sid, sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(rows.len(), 1, "{sql} must return exactly one row");
    assert_eq!(rows[0].values.len(), 1, "{sql} must return exactly one column");
    rows[0].values[0].clone()
}

fn expect_bool(value: &Value, what: &str) -> bool {
    match value {
        Value::Boolean(b) => *b,
        other => panic!("{what} must return boolean, got {other:?}"),
    }
}

fn try_lock_text(db: &EmbeddedDatabase, sid: SessionId, key: i64) -> bool {
    let v = scalar_text(db, sid, &format!("SELECT pg_try_advisory_lock({key})"));
    expect_bool(&v, "pg_try_advisory_lock")
}

fn try_lock_params(db: &EmbeddedDatabase, sid: SessionId, key: i64) -> bool {
    let v = scalar_params(db, sid, &format!("SELECT pg_try_advisory_lock({key})"));
    expect_bool(&v, "pg_try_advisory_lock")
}

fn unlock_text(db: &EmbeddedDatabase, sid: SessionId, key: i64) -> bool {
    let v = scalar_text(db, sid, &format!("SELECT pg_advisory_unlock({key})"));
    expect_bool(&v, "pg_advisory_unlock")
}

// ---------------------------------------------------------------------------
// The literal Prisma Migrate sequence
// ---------------------------------------------------------------------------

/// The exact statements Prisma Migrate issues, on both executor families.
/// `pg_advisory_lock` is a `void` function: one row, one column, NULL-typed.
#[test]
fn prisma_migrate_lock_sequence_works_on_both_families() {
    let db = db();
    let text = session(&db);
    let params = session(&db);
    const KEY: i64 = 72_707_369;

    // TEXT family (psycopg2 / simple protocol).
    let acquired = scalar_text(&db, text, &format!("SELECT pg_advisory_lock({KEY})"));
    assert_eq!(acquired, Value::Null, "pg_advisory_lock() returns void");
    assert!(
        !try_lock_params(&db, params, KEY),
        "a second connection must not get the migration lock"
    );
    assert!(unlock_text(&db, text, KEY), "the holder's unlock must return true");

    // PARAMS family (psycopg3 / extended protocol / REST).
    let acquired = scalar_params(&db, params, &format!("SELECT pg_advisory_lock({KEY})"));
    assert_eq!(acquired, Value::Null, "pg_advisory_lock() returns void");
    assert!(
        !try_lock_text(&db, text, KEY),
        "a second connection must not get the migration lock"
    );
    let released = scalar_params(&db, params, &format!("SELECT pg_advisory_unlock({KEY})"));
    assert!(expect_bool(&released, "pg_advisory_unlock"));

    assert!(try_lock_text(&db, text, KEY), "the key is free again");
    db.destroy_session(text).unwrap();
    db.destroy_session(params).unwrap();
}

// ---------------------------------------------------------------------------
// Core semantics
// ---------------------------------------------------------------------------

/// lock → try from the other session is false → unlock → try is true.
#[test]
fn lock_excludes_other_session_until_unlocked() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const KEY: i64 = 86_100_001;

    assert_eq!(
        scalar_text(&db, a, &format!("SELECT pg_advisory_lock({KEY})")),
        Value::Null
    );
    assert!(!try_lock_text(&db, b, KEY), "B must not acquire A's lock");
    assert!(unlock_text(&db, a, KEY));
    assert!(try_lock_text(&db, b, KEY), "B acquires once A released");

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

/// Re-entrant for the same session: N locks need N unlocks.
#[test]
fn reentrancy_counter_requires_matching_unlocks() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const KEY: i64 = 86_100_002;

    assert!(try_lock_text(&db, a, KEY));
    assert!(try_lock_text(&db, a, KEY), "re-entry by the owner always succeeds");
    assert!(try_lock_text(&db, a, KEY));

    assert!(unlock_text(&db, a, KEY));
    assert!(unlock_text(&db, a, KEY));
    assert!(!try_lock_text(&db, b, KEY), "two of three unlocks is still held");
    assert!(unlock_text(&db, a, KEY));
    assert!(try_lock_text(&db, b, KEY), "the third unlock frees it");

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

/// `pg_advisory_unlock` of a lock you do not own returns false (PostgreSQL
/// also emits a WARNING; we log at WARN).
#[test]
fn unlock_of_a_lock_you_do_not_own_returns_false() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const HELD: i64 = 86_100_003;
    const NEVER_HELD: i64 = 86_100_004;

    assert!(try_lock_text(&db, a, HELD));
    assert!(!unlock_text(&db, b, HELD), "B does not hold this lock");
    assert!(!unlock_text(&db, b, NEVER_HELD), "nobody holds this lock");
    // A still holds it — the failed unlock must not have released anything.
    assert!(!try_lock_text(&db, b, HELD));

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

/// `pg_advisory_unlock_all()` drops every session-level lock this session holds.
#[test]
fn unlock_all_releases_every_session_lock() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const K1: i64 = 86_100_005;
    const K2: i64 = 86_100_006;

    assert!(try_lock_text(&db, a, K1));
    assert!(try_lock_text(&db, a, K2));
    assert!(try_lock_text(&db, a, K2), "held twice");

    let out = scalar_text(&db, a, "SELECT pg_advisory_unlock_all()");
    assert_eq!(out, Value::Null, "pg_advisory_unlock_all() returns void");

    assert!(try_lock_text(&db, b, K1), "K1 released");
    assert!(try_lock_text(&db, b, K2), "K2 released, counter and all");

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

/// The `(int, int)` overload is a DIFFERENT lock from the `bigint` one, as in
/// PostgreSQL (which distinguishes them by the lock tag's `objsubid`).
#[test]
fn int_pair_key_is_a_distinct_namespace_from_the_bigint_key() {
    let db = db();
    let a = session(&db);
    let b = session(&db);

    assert!(try_lock_text(&db, a, 6_100_007));
    let pair = scalar_text(&db, b, "SELECT pg_try_advisory_lock(0, 6100007)");
    assert!(
        expect_bool(&pair, "pg_try_advisory_lock(int,int)"),
        "(0, k) must not collide with the bigint key k"
    );
    // ... and the pair form is itself exclusive.
    let a_pair = scalar_text(&db, a, "SELECT pg_try_advisory_lock(0, 6100007)");
    assert!(!expect_bool(&a_pair, "pg_try_advisory_lock(int,int)"));

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

// ---------------------------------------------------------------------------
// Transaction-scoped locks
// ---------------------------------------------------------------------------

/// `pg_advisory_xact_lock` is released automatically at COMMIT.
#[test]
fn xact_lock_is_released_at_commit() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const KEY: i64 = 86_100_008;

    db.begin_transaction_for_session(a).unwrap();
    let out = scalar_text(&db, a, &format!("SELECT pg_advisory_xact_lock({KEY})"));
    assert_eq!(out, Value::Null, "pg_advisory_xact_lock() returns void");
    assert!(!try_lock_text(&db, b, KEY), "held for the duration of A's transaction");

    db.commit_transaction_for_session(a).unwrap();
    assert!(try_lock_text(&db, b, KEY), "COMMIT must release the xact lock");

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

/// ... and at ROLLBACK.
#[test]
fn xact_lock_is_released_at_rollback() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const KEY: i64 = 86_100_009;

    db.begin_transaction_for_session(a).unwrap();
    let held = scalar_params(&db, a, &format!("SELECT pg_try_advisory_xact_lock({KEY})"));
    assert!(expect_bool(&held, "pg_try_advisory_xact_lock"));
    assert!(!try_lock_text(&db, b, KEY));

    db.rollback_transaction_for_session(a).unwrap();
    assert!(try_lock_text(&db, b, KEY), "ROLLBACK must release the xact lock");

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

/// A transaction-level lock cannot be released with `pg_advisory_unlock`
/// (PostgreSQL: "you don't own a lock of type ExclusiveLock" → false).
#[test]
fn xact_lock_cannot_be_unlocked_explicitly() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const KEY: i64 = 86_100_010;

    db.begin_transaction_for_session(a).unwrap();
    scalar_text(&db, a, &format!("SELECT pg_advisory_xact_lock({KEY})"));
    assert!(
        !unlock_text(&db, a, KEY),
        "pg_advisory_unlock must not release a transaction-level lock"
    );
    assert!(!try_lock_text(&db, b, KEY), "still held");
    db.commit_transaction_for_session(a).unwrap();
    assert!(try_lock_text(&db, b, KEY));

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

/// In autocommit the statement IS the transaction, so an xact lock taken there
/// ends with the statement (PostgreSQL parity).
#[test]
fn xact_lock_in_autocommit_ends_with_the_statement() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const KEY: i64 = 86_100_011;

    scalar_text(&db, a, &format!("SELECT pg_advisory_xact_lock({KEY})"));
    assert!(
        try_lock_text(&db, b, KEY),
        "an autocommit xact lock must not outlive its statement"
    );

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

// ---------------------------------------------------------------------------
// Ownership lifetime
// ---------------------------------------------------------------------------

/// Locks die with the connection. `destroy_session` is the ONE funnel the PG
/// handler's `Drop` uses, so this covers Terminate, a dropped socket and the
/// error path alike.
#[test]
fn session_teardown_releases_every_lock_the_session_held() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const SESSION_KEY: i64 = 86_100_012;
    const XACT_KEY: i64 = 86_100_013;

    assert!(try_lock_text(&db, a, SESSION_KEY));
    db.begin_transaction_for_session(a).unwrap();
    scalar_text(&db, a, &format!("SELECT pg_advisory_xact_lock({XACT_KEY})"));

    db.destroy_session(a).unwrap();

    assert!(try_lock_text(&db, b, SESSION_KEY), "session lock released at teardown");
    assert!(try_lock_text(&db, b, XACT_KEY), "xact lock released at teardown");

    db.destroy_session(b).unwrap();
}

/// A blocking `pg_advisory_lock` is granted as soon as the holder releases.
#[test]
fn blocking_lock_is_granted_when_the_holder_releases() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const KEY: i64 = 86_100_014;

    assert!(try_lock_text(&db, a, KEY));

    let waiter_db = Arc::clone(&db);
    let waiter = std::thread::spawn(move || {
        // Blocks until A unlocks; returns the void row.
        scalar_text(&waiter_db, b, &format!("SELECT pg_advisory_lock({KEY})"))
    });

    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(!waiter.is_finished(), "the waiter must still be blocked");
    assert!(unlock_text(&db, a, KEY));

    let granted = waiter.join().expect("waiter thread must not panic");
    assert_eq!(granted, Value::Null);

    // B now holds it.
    assert!(!try_lock_text(&db, a, KEY));
    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

/// A blocking wait honours `statement_timeout` and reports PostgreSQL's own
/// `57014 query_canceled` (`Error::QueryTimeout`) instead of hanging forever.
#[test]
fn blocking_lock_honours_statement_timeout() {
    use heliosdb_nano::config::Config;

    let mut config = Config::in_memory();
    config.storage.statement_timeout_ms = Some(200);
    let db = Arc::new(EmbeddedDatabase::with_config(config).expect("db"));
    let a = session(&db);
    let b = session(&db);
    const KEY: i64 = 86_100_015;

    assert!(try_lock_text(&db, a, KEY));
    let started = std::time::Instant::now();
    let err = db
        .query_with_columns_for_session(b, &format!("SELECT pg_advisory_lock({KEY})"))
        .expect_err("a blocked advisory lock must time out, not hang");
    assert!(
        matches!(err, heliosdb_nano::Error::QueryTimeout(_)),
        "statement_timeout must surface as QueryTimeout (57014), got {err:?}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

// ---------------------------------------------------------------------------
// Both DML executor families
// ---------------------------------------------------------------------------

/// The advisory functions are reachable from the DML executors too, and each
/// family attributes the lock to the SESSION running the statement.
///
/// Text family: `execute_for_session` → `execute_in_transaction_inner`.
/// Params family: `execute_params_for_session` → `execute_plan_with_params_inner`.
#[test]
fn advisory_locks_are_reachable_from_both_dml_executor_families() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const TEXT_KEY: i64 = 86_100_016;
    const PARAMS_KEY: i64 = 86_100_017;

    db.execute_for_session(a, "CREATE TABLE adv (id INT PRIMARY KEY, got BOOLEAN)")
        .unwrap();

    // TEXT family.
    db.execute_for_session(
        a,
        &format!("INSERT INTO adv VALUES (1, pg_try_advisory_lock({TEXT_KEY}))"),
    )
    .unwrap();
    assert!(
        !try_lock_text(&db, b, TEXT_KEY),
        "the text DML family must actually take the lock"
    );

    // PARAMS family.
    db.execute_params_for_session(
        a,
        &format!("INSERT INTO adv VALUES ($1, pg_try_advisory_lock({PARAMS_KEY}))"),
        &[Value::Int4(2)],
    )
    .unwrap();
    assert!(
        !try_lock_text(&db, b, PARAMS_KEY),
        "the params DML family must actually take the lock"
    );

    // Both rows recorded a successful acquisition.
    let rows = db
        .query_with_columns_for_session(a, "SELECT got FROM adv ORDER BY id")
        .unwrap()
        .0;
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.values[0], Value::Boolean(true));
    }

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

// ---------------------------------------------------------------------------
// Observability + caching
// ---------------------------------------------------------------------------

/// `pg_advisory_locks` shows who holds what, so an operator can answer "a
/// migration is hanging on 72707369 — who has it?".
#[test]
fn pg_advisory_locks_view_reports_the_holder() {
    let db = db();
    let a = session(&db);
    const KEY: i64 = 86_100_018;

    assert!(try_lock_text(&db, a, KEY));
    assert!(try_lock_text(&db, a, KEY));

    let (rows, cols) = db
        .query_with_columns_for_session(
            a,
            "SELECT key_kind, objid, objsubid, session_id, session_locks, xact_locks, mode \
             FROM pg_advisory_locks",
        )
        .expect("pg_advisory_locks must be queryable");
    assert_eq!(cols.len(), 7, "columns: {cols:?}");
    // The table is process-global, so other tests in this binary have rows here
    // too — pick ours out rather than asserting on the row count.
    let mine = rows
        .iter()
        .find(|r| r.values[1] == Value::Int8(KEY))
        .unwrap_or_else(|| panic!("pg_advisory_locks must list the held key {KEY}; rows: {rows:?}"));
    assert_eq!(mine.values[0], Value::String("bigint".to_string()));
    assert_eq!(mine.values[2], Value::Int4(1), "objsubid 1 = the bigint overload");
    assert_eq!(mine.values[3], Value::Int8(a.0 as i64), "attributed to the holder");
    assert_eq!(mine.values[4], Value::Int8(2), "held twice");
    assert_eq!(mine.values[5], Value::Int8(0), "no transaction-level holds");
    assert_eq!(mine.values[6], Value::String("ExclusiveLock".to_string()));

    db.destroy_session(a).unwrap();
}

/// The advisory functions mutate server state, so their results must never be
/// served from the result cache: the SAME SQL run twice must reflect the real
/// lock state both times.
#[test]
fn advisory_results_are_never_served_from_the_result_cache() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const KEY: i64 = 86_100_019;

    assert!(try_lock_text(&db, a, KEY));
    // Identical SQL text, run repeatedly by B: false while A holds it...
    assert!(!try_lock_text(&db, b, KEY));
    assert!(!try_lock_text(&db, b, KEY));
    assert!(unlock_text(&db, a, KEY));
    // ... and true immediately after A releases, not a cached `false`.
    assert!(try_lock_text(&db, b, KEY), "a cached result would still say false");

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

// ---------------------------------------------------------------------------
// Fail closed
// ---------------------------------------------------------------------------

/// Shared advisory locks are OUT of scope. They must keep erroring rather than
/// be silently served as EXCLUSIVE locks, which would let two writers that each
/// asked for a shared lock believe they were serialised.
#[test]
fn shared_variants_are_rejected_not_silently_served_as_exclusive() {
    let db = db();
    let a = session(&db);

    for sql in [
        "SELECT pg_advisory_lock_shared(1)",
        "SELECT pg_try_advisory_lock_shared(1)",
        "SELECT pg_advisory_unlock_shared(1)",
        "SELECT pg_advisory_xact_lock_shared(1)",
    ] {
        let err = db
            .query_with_columns_for_session(a, sql)
            .expect_err("shared advisory locks are not implemented and must not be faked");
        assert!(
            err.to_string().contains("Unknown scalar function"),
            "{sql} must report an unknown function (42883), got: {err}"
        );
    }

    db.destroy_session(a).unwrap();
}

/// A NULL key is rejected rather than silently locking key 0.
#[test]
fn null_key_is_rejected() {
    let db = db();
    let a = session(&db);
    let err = db
        .query_with_columns_for_session(a, "SELECT pg_advisory_lock(NULL)")
        .expect_err("a NULL key must not be treated as a key");
    assert!(err.to_string().contains("NULL"), "got: {err}");
    db.destroy_session(a).unwrap();
}

/// The per-session quota (`[locks] max_advisory_locks_per_session`) refuses a
/// NEW key once the session is at its cap — it never evicts, and it never
/// refuses a re-acquisition of a key the session already holds.
#[test]
fn per_session_quota_fails_closed() {
    use heliosdb_nano::config::Config;

    let mut config = Config::in_memory();
    config.locks.max_advisory_locks_per_session = 2;
    let db = Arc::new(EmbeddedDatabase::with_config(config).expect("db"));
    let a = session(&db);
    const BASE: i64 = 86_100_020;

    assert!(try_lock_text(&db, a, BASE));
    assert!(try_lock_text(&db, a, BASE + 1));
    // Re-entry on a held key is always allowed.
    assert!(try_lock_text(&db, a, BASE));

    let err = db
        .query_with_columns_for_session(a, &format!("SELECT pg_try_advisory_lock({})", BASE + 2))
        .expect_err("a third distinct key must be refused, not granted");
    assert!(err.to_string().contains("out of advisory lock slots"), "got: {err}");

    db.destroy_session(a).unwrap();
}

// ---------------------------------------------------------------------------
// Session-LESS surfaces (REST/BaaS, MCP, the embedded funnel, the REPL)
//
// `db.query()` / `db.execute()` carry no connection identity. Every concurrent
// HTTP request, MCP tool call and embedded thread shares ONE
// `EmbeddedDatabase`, so attributing their locks to the handle gave two
// unrelated clients the SAME lock — re-entrant for the shared owner, so both
// were told `true` — and released it only when the last handle dropped. These
// tests pin the fail-closed contract that replaced it.
// ---------------------------------------------------------------------------

/// Marker every session-less refusal carries (`0A000` on the wire).
const NEEDS_A_SESSION: &str = "advisory locks require a client session";

/// The single value a one-column, one-row answer produced, or `None` when the
/// call was refused (or returned nothing).
fn answered(value: Option<Value>) -> bool {
    value == Some(Value::Boolean(true))
}

fn sessionless_text(db: &EmbeddedDatabase, sql: &str) -> Option<Value> {
    db.query_with_columns(sql)
        .ok()
        .and_then(|(rows, _cols)| rows.first().map(|r| r.values[0].clone()))
}

fn sessionless_params(db: &EmbeddedDatabase, sql: &str) -> Option<Value> {
    db.query_params(sql, &[])
        .ok()
        .and_then(|rows| rows.first().map(|r| r.values[0].clone()))
}

/// The whole defect in two statements: two unrelated session-less callers of
/// one handle must never both be told they hold the migration key.
///
/// FAILS on the unfixed tree — one shared `embedded_session_id` makes the
/// second acquisition re-entrant (`advisory_lock.rs`: `Some(current) if current
/// == owner => bump`), so both calls return `true` and both believe they have
/// serialised the migration.
#[test]
fn two_sessionless_callers_are_never_both_told_they_hold_the_key() {
    let db = db();
    const KEY: i64 = 86_100_034;

    // Two clients, one handle — exactly two concurrent `/mcp` or `/rest/v1`
    // requests, one on each executor family.
    let first = sessionless_text(&db, &format!("SELECT pg_try_advisory_lock({KEY})"));
    let second = sessionless_params(&db, &format!("SELECT pg_try_advisory_lock({KEY})"));

    assert!(
        !(answered(first.clone()) && answered(second.clone())),
        "*** two session-less callers were both granted advisory key {KEY} \
         ({first:?} / {second:?}) — the lock excludes nobody ***"
    );
}

/// The session-scope family is REFUSED on a session-less path — on the query
/// paths AND on both DML executor families — rather than granting a lock that
/// excludes nobody and that nothing can release.
///
/// FAILS on the unfixed tree: every call returns `Ok`.
#[test]
fn sessionless_surfaces_are_refused_the_session_scope_family() {
    let db = db();
    const KEY: i64 = 86_100_031;

    // TEXT query family (`db.query_with_columns` — the REPL, the REST executor).
    let err = db
        .query_with_columns(&format!("SELECT pg_try_advisory_lock({KEY})"))
        .expect_err("a session-less caller must not be granted a session-level advisory lock");
    assert!(err.to_string().contains(NEEDS_A_SESSION), "got: {err}");

    // PARAMS query family (`db.query_params` — REST/BaaS, the MCP query tool).
    let err = db
        .query_params(&format!("SELECT pg_advisory_lock({KEY})"), &[])
        .expect_err("a session-less caller must not be granted a session-level advisory lock");
    assert!(err.to_string().contains(NEEDS_A_SESSION), "got: {err}");

    // The unlocks too: reporting "released" for locks that live somewhere else
    // is the same lie in the other direction.
    for sql in [
        format!("SELECT pg_advisory_unlock({KEY})"),
        "SELECT pg_advisory_unlock_all()".to_string(),
    ] {
        let err = db
            .query_with_columns(&sql)
            .err()
            .unwrap_or_else(|| panic!("{sql} must be refused on a session-less path"));
        assert!(err.to_string().contains(NEEDS_A_SESSION), "{sql}: {err}");
    }

    // Both DML executor families funnel through the same guard.
    db.execute("CREATE TABLE adv_sessionless (id INT PRIMARY KEY, got BOOLEAN)")
        .unwrap();
    let err = db
        .execute(&format!(
            "INSERT INTO adv_sessionless VALUES (1, pg_try_advisory_lock({KEY}))"
        ))
        .expect_err("the TEXT DML family must refuse it too");
    assert!(err.to_string().contains(NEEDS_A_SESSION), "text DML: {err}");
    let err = db
        .execute_params(
            &format!("INSERT INTO adv_sessionless VALUES ($1, pg_try_advisory_lock({KEY}))"),
            &[Value::Int4(2)],
        )
        .expect_err("the PARAMS DML family must refuse it too");
    assert!(err.to_string().contains(NEEDS_A_SESSION), "params DML: {err}");
}

/// Nothing a session-less statement does can strand the migration key: a real
/// connection can always take it afterwards.
///
/// FAILS on the unfixed tree — the session-less `pg_advisory_lock` succeeds
/// under the handle-wide owner, `destroy_session` never matches that owner and
/// `Drop` only fires for the last handle, so the session's
/// `pg_try_advisory_lock` returns false. A Prisma `pg_advisory_lock(72707369)`
/// on the wire would then block forever on a key no connection can release.
#[test]
fn a_sessionless_statement_cannot_strand_the_migration_key() {
    let db = db();
    const KEY: i64 = 86_100_032;

    // Whatever the session-less path decides to do with these...
    let _ = db.query_with_columns(&format!("SELECT pg_advisory_lock({KEY})"));
    let _ = db.query_params(&format!("SELECT pg_try_advisory_lock({KEY})"), &[]);

    // ... a real connection must still be able to take the key.
    let a = session(&db);
    assert!(
        try_lock_text(&db, a, KEY),
        "*** a session-less statement stranded advisory key {KEY} in the process-global table ***"
    );
    db.destroy_session(a).unwrap();
}

/// The TRANSACTION-scope half IS served session-lessly — in autocommit the
/// statement is the transaction, exactly as in PostgreSQL — and it ends with
/// that statement even while an unrelated embedded `BEGIN` is open on the same
/// handle.
///
/// FAILS on the unfixed tree: the session-less context read
/// `in_explicit_transaction` from the handle-global `global_txn_active`, so
/// while ANY caller sat inside an embedded `BEGIN`, every other caller's
/// autocommit transaction-scope lock was treated as transaction-scoped and
/// leaked until that unrelated transaction committed — the session's
/// `pg_try_advisory_lock` then returns false.
#[test]
fn sessionless_xact_lock_ends_with_its_statement_not_with_someone_elses_begin() {
    let db = db();
    const KEY: i64 = 86_100_033;

    // An unrelated caller of this handle opens the global-slot transaction.
    db.execute("BEGIN").unwrap();

    let granted = sessionless_text(&db, &format!("SELECT pg_try_advisory_xact_lock({KEY})"));
    assert_eq!(
        granted,
        Some(Value::Boolean(true)),
        "a transaction-scope advisory lock IS available session-lessly, got {granted:?}"
    );

    let a = session(&db);
    assert!(
        try_lock_text(&db, a, KEY),
        "*** a session-less autocommit xact lock was pinned by an unrelated embedded BEGIN ***"
    );

    db.execute("COMMIT").unwrap();
    db.destroy_session(a).unwrap();
}
