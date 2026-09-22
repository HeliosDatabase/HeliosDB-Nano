//! Two open databases in one process are two SERVERS, not two views of one —
//! sprinter `564e9ac1d762` (advisory locks) and `32ed4b9e0002`
//! (`pg_stat_activity`).
//!
//! Both items are children of `98c88a573a7d` (per-session / per-engine state
//! parked in process globals) and both have the same shape: a `static` table
//! that every `EmbeddedDatabase` in the process shares, read back with no
//! notion of WHICH database is asking.
//!
//! # What fails before the fix
//!
//! * `advisory_lock::manager()` is one `LazyLock<AdvisoryLockManager>` keyed by
//!   the client's integer ALONE (`src/advisory_lock.rs:544`,
//!   `AdvisoryKey::BigInt(i64)` at `src/advisory_lock.rs:107`). Every ORM
//!   hardcodes the same migration key — Prisma's is `72707369` — so database
//!   A's migration lock blocks database B's for as long as A holds it, and the
//!   wait is a condvar wait with no bound unless `statement_timeout` is set.
//! * `session::scoped::live_backends()` is one
//!   `DashMap<i32, Weak<SessionScopedState>>` (`src/session/scoped.rs:136`)
//!   scanned by `execute_pg_stat_activity` (`src/sql/phase3/system_views.rs:3842`)
//!   with no filter at all, so database A's `pg_stat_activity` lists database
//!   B's backends — `usename`, `application_name`, `client_addr`, `client_port`.
//!
//! # What this file pins
//!
//! Isolation in BOTH directions. Every "A does not see / is not blocked by B"
//! assertion is paired with a control proving the surface still WORKS within
//! one database — the failure mode of a careless fix is a lock that excludes
//! nobody and a view that lists nothing, and either of those would pass a
//! one-sided test.
//!
//! Keys are unique per test: the lock table is shared by every test in this
//! binary and they run concurrently.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};
use std::sync::Arc;

fn db() -> Arc<EmbeddedDatabase> {
    Arc::new(EmbeddedDatabase::new_in_memory().expect("in-memory database"))
}

fn session(db: &EmbeddedDatabase, user: &str) -> SessionId {
    db.create_session(user, IsolationLevel::ReadCommitted).expect("session")
}

/// One statement on the TEXT (simple-query) family, which is the entry point
/// both wire handlers use — and, critically, one that installs the session's
/// per-statement context, so the advisory owner and the scanning backend are
/// both resolvable.
fn rows(db: &EmbeddedDatabase, sid: SessionId, sql: &str) -> Vec<Tuple> {
    let (rows, _cols) = db
        .query_with_columns_for_session(sid, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows
}

fn scalar(db: &EmbeddedDatabase, sid: SessionId, sql: &str) -> Value {
    let rows = rows(db, sid, sql);
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

fn try_lock(db: &EmbeddedDatabase, sid: SessionId, key: i64) -> bool {
    expect_bool(
        &scalar(db, sid, &format!("SELECT pg_try_advisory_lock({key})")),
        "pg_try_advisory_lock",
    )
}

fn lock(db: &EmbeddedDatabase, sid: SessionId, key: i64) {
    let _ = scalar(db, sid, &format!("SELECT pg_advisory_lock({key})"));
}

fn unlock(db: &EmbeddedDatabase, sid: SessionId, key: i64) -> bool {
    expect_bool(
        &scalar(db, sid, &format!("SELECT pg_advisory_unlock({key})")),
        "pg_advisory_unlock",
    )
}

fn as_i64(value: &Value) -> i64 {
    match value {
        Value::Int2(v) => i64::from(*v),
        Value::Int4(v) => i64::from(*v),
        Value::Int8(v) => *v,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => panic!("expected text or NULL, got {other:?}"),
    }
}

// ===========================================================================
// Item 564e9ac1d762 — advisory locks are not scoped per database
// ===========================================================================

/// THE REPRO, verbatim from the item: two in-memory databases, Prisma's own
/// hardcoded migration key on both.
///
/// Before the fix this returns `false` — database B's `prisma migrate` waits on
/// database A's migration, forever, because the two share
/// `advisory_lock::manager()`'s single `HashMap<AdvisoryKey, Holder>` and
/// `AdvisoryKey` is the client's integer and nothing else.
#[test]
fn prisma_migration_lock_in_one_database_does_not_block_another() {
    const PRISMA_KEY: i64 = 72_707_369;

    let a = db();
    let b = db();
    let a_session = session(&a, "prisma_a");
    let b_session = session(&b, "prisma_b");

    lock(&a, a_session, PRISMA_KEY);

    assert!(
        try_lock(&b, b_session, PRISMA_KEY),
        "database B could not take Prisma's migration key {PRISMA_KEY} while database A held it: \
         two EmbeddedDatabases share one advisory-lock table, so every ORM's hardcoded migration \
         key cross-blocks"
    );

    assert!(unlock(&a, a_session, PRISMA_KEY));
    assert!(unlock(&b, b_session, PRISMA_KEY));
}

/// CONTROL, and the half a careless fix breaks: scoping the key per database
/// must not stop it excluding a second connection to the SAME database. A lock
/// that serialises nothing is strictly worse than no lock at all — it is
/// exactly the silent failure a migration runner cannot survive.
#[test]
fn advisory_lock_still_excludes_a_second_session_of_the_same_database() {
    const KEY: i64 = -770_000_001;

    let db = db();
    let holder = session(&db, "holder");
    let waiter = session(&db, "waiter");

    assert!(try_lock(&db, holder, KEY), "the first acquisition must succeed");
    assert!(
        !try_lock(&db, waiter, KEY),
        "a second session of the SAME database took a key that was already held — \
         the per-database scope broke mutual exclusion instead of narrowing it"
    );

    assert!(unlock(&db, holder, KEY));
    assert!(
        try_lock(&db, waiter, KEY),
        "the key was not released to the same database after unlock"
    );
    assert!(unlock(&db, waiter, KEY));
}

/// Releasing in A must not release B. Two databases holding "the same" key hold
/// two different locks, so A's `pg_advisory_unlock` may not hand B's away.
#[test]
fn releasing_in_one_database_does_not_release_the_other() {
    const KEY: i64 = -770_000_002;

    let a = db();
    let b = db();
    let a_session = session(&a, "a");
    let b_session = session(&b, "b");
    let b_other = session(&b, "b2");

    assert!(try_lock(&a, a_session, KEY));
    assert!(try_lock(&b, b_session, KEY), "B must be able to hold its own {KEY}");

    assert!(unlock(&a, a_session, KEY), "A must release its own hold");

    assert!(
        !try_lock(&b, b_other, KEY),
        "unlocking in database A released database B's hold on the same key"
    );
    assert!(unlock(&b, b_session, KEY));
}

/// `pg_advisory_locks` is the view an operator reads to answer "the migration
/// is stuck on 72707369 — who has it?". It must answer for THIS database, not
/// leak another database's holder session ids.
#[test]
fn pg_advisory_locks_view_lists_only_this_databases_holders() {
    const KEY: i64 = -770_000_003;

    let a = db();
    let b = db();
    let a_session = session(&a, "a");
    let b_session = session(&b, "b");

    assert!(try_lock(&a, a_session, KEY));

    let held_in = |handle: &EmbeddedDatabase, sid: SessionId| -> Vec<i64> {
        rows(
            handle,
            sid,
            &format!("SELECT objid FROM pg_advisory_locks WHERE objid = {KEY}"),
        )
        .iter()
        .map(|t| as_i64(&t.values[0]))
        .collect()
    };

    assert_eq!(
        held_in(&a, a_session),
        vec![KEY],
        "the holding database's own pg_advisory_locks must still list the key"
    );
    assert!(
        held_in(&b, b_session).is_empty(),
        "database B's pg_advisory_locks lists database A's holder"
    );

    assert!(unlock(&a, a_session, KEY));
}

// ===========================================================================
// Item 32ed4b9e0002 — pg_stat_activity lists every database's backends
// ===========================================================================

/// Read `pg_stat_activity` as `sid`, one row per live backend as that session
/// sees it.
fn stat_activity(db: &EmbeddedDatabase, sid: SessionId) -> Vec<(i64, Option<String>, Option<String>)> {
    rows(
        db,
        sid,
        "SELECT pid, usename, application_name FROM pg_stat_activity ORDER BY pid",
    )
    .iter()
    .map(|t| (as_i64(&t.values[0]), as_text(&t.values[1]), as_text(&t.values[2])))
    .collect()
}

/// THE REPRO, verbatim from the item: two `EmbeddedDatabase`s, a session on
/// each with a distinct `application_name`, then scan on A.
///
/// Before the fix B's pid, login name and application name are all listed to a
/// principal that has no connection to database B at all.
#[test]
fn pg_stat_activity_does_not_disclose_another_databases_backends() {
    let a = db();
    let b = db();
    let a_session = session(&a, "alice");
    let b_session = session(&b, "bob");
    a.set_session_application_name(a_session, "tenant-a-app").unwrap();
    b.set_session_application_name(b_session, "tenant-b-secret-app")
        .unwrap();

    let b_pid = b.session_backend_pid(b_session).unwrap();
    // Make B's backend live and identified before A scans.
    let _ = scalar(&b, b_session, "SELECT pg_backend_pid()");

    let seen = stat_activity(&a, a_session);

    assert!(
        !seen.iter().any(|(pid, _, _)| *pid == i64::from(b_pid)),
        "database A's pg_stat_activity lists database B's backend {b_pid}: {seen:?}"
    );
    assert!(
        !seen.iter().any(|(_, user, _)| user.as_deref() == Some("bob")),
        "database A's pg_stat_activity discloses database B's usename: {seen:?}"
    );
    assert!(
        !seen
            .iter()
            .any(|(_, _, app)| app.as_deref() == Some("tenant-b-secret-app")),
        "database A's pg_stat_activity discloses database B's application_name: {seen:?}"
    );

    a.destroy_session(a_session).unwrap();
    b.destroy_session(b_session).unwrap();
}

/// CONTROL: the view must not be emptied by the filter. A session still sees
/// its OWN row — the `... WHERE pid = pg_backend_pid()` self-join every pool,
/// health check and test suite writes — and the sibling backends of its own
/// database.
#[test]
fn pg_stat_activity_still_lists_this_databases_backends() {
    let db = db();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");

    let alice_pid = i64::from(db.session_backend_pid(alice).unwrap());
    let bob_pid = i64::from(db.session_backend_pid(bob).unwrap());

    let self_join = as_i64(&scalar(
        &db,
        alice,
        "SELECT count(*) FROM pg_stat_activity WHERE pid = pg_backend_pid()",
    ));
    assert_eq!(
        self_join, 1,
        "the scanning backend is missing from its own pg_stat_activity"
    );

    let seen = stat_activity(&db, alice);
    assert!(
        seen.iter().any(|(pid, _, _)| *pid == alice_pid),
        "own backend missing: {seen:?}"
    );
    assert!(
        seen.iter().any(|(pid, _, _)| *pid == bob_pid),
        "a sibling backend of the SAME database is missing — the filter emptied the view \
         instead of scoping it: {seen:?}"
    );

    db.destroy_session(alice).unwrap();
    db.destroy_session(bob).unwrap();
}

/// A destroyed session must leave the view — the `Weak`-registry property the
/// engine scoping must not disturb.
#[test]
fn a_destroyed_session_leaves_this_databases_pg_stat_activity() {
    let db = db();
    let watcher = session(&db, "watcher");
    let transient = session(&db, "transient");
    let transient_pid = i64::from(db.session_backend_pid(transient).unwrap());

    assert!(
        stat_activity(&db, watcher)
            .iter()
            .any(|(pid, _, _)| *pid == transient_pid),
        "the transient backend was never listed"
    );

    db.destroy_session(transient).unwrap();

    assert!(
        !stat_activity(&db, watcher)
            .iter()
            .any(|(pid, _, _)| *pid == transient_pid),
        "a destroyed session is still listed in pg_stat_activity"
    );
    db.destroy_session(watcher).unwrap();
}

/// PostgreSQL's own contract for the rows that DO belong to this server: every
/// backend is listed, but a backend owned by a different login role shows NULL
/// in the columns that disclose what that role is doing — `client_addr`,
/// `client_port`, `state`, `query` and the backend's timestamps
/// (`pg_stat_get_activity`'s unprivileged branch).
#[test]
fn pg_stat_activity_masks_another_roles_activity_columns() {
    let db = db();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    db.set_session_client_address(alice, Some("10.0.0.1"), 5001).unwrap();
    db.set_session_client_address(bob, Some("10.0.0.2"), 5002).unwrap();

    let alice_pid = i64::from(db.session_backend_pid(alice).unwrap());
    let bob_pid = i64::from(db.session_backend_pid(bob).unwrap());

    let scanned = rows(
        &db,
        alice,
        "SELECT pid, client_addr, client_port, state FROM pg_stat_activity ORDER BY pid",
    );

    let row_for = |pid: i64| -> &Tuple {
        scanned
            .iter()
            .find(|t| as_i64(&t.values[0]) == pid)
            .unwrap_or_else(|| panic!("pid {pid} missing from pg_stat_activity: {scanned:?}"))
    };

    // Own row: everything visible, exactly as before.
    let mine = row_for(alice_pid);
    assert_eq!(
        as_text(&mine.values[1]).as_deref(),
        Some("10.0.0.1"),
        "a session lost sight of its OWN client_addr"
    );
    assert_eq!(as_i64(&mine.values[2]), 5001, "a session lost its OWN client_port");
    assert_eq!(
        as_text(&mine.values[3]).as_deref(),
        Some("active"),
        "the scanning backend must report itself active"
    );

    // Another role's row: listed (PostgreSQL does not hide it) but masked.
    let theirs = row_for(bob_pid);
    assert_eq!(
        as_text(&theirs.values[1]),
        None,
        "another role's client_addr is disclosed: {theirs:?}"
    );
    assert!(
        matches!(theirs.values[2], Value::Null),
        "another role's client_port is disclosed: {theirs:?}"
    );
    assert_eq!(
        as_text(&theirs.values[3]),
        None,
        "another role's state is disclosed: {theirs:?}"
    );

    db.destroy_session(alice).unwrap();
    db.destroy_session(bob).unwrap();
}

/// The masking predicate is the ROLE, not the pid: two sessions opened under
/// the same login name are the same principal and must see each other in full,
/// the way `HasPrivsOfRole` decides it in PostgreSQL.
#[test]
fn pg_stat_activity_does_not_mask_the_same_roles_other_backend() {
    let db = db();
    let first = session(&db, "reporter");
    let second = session(&db, "reporter");
    db.set_session_client_address(second, Some("10.0.0.7"), 5007).unwrap();
    let second_pid = i64::from(db.session_backend_pid(second).unwrap());

    let scanned = rows(&db, first, "SELECT pid, client_addr FROM pg_stat_activity ORDER BY pid");
    let sibling = scanned
        .iter()
        .find(|t| as_i64(&t.values[0]) == second_pid)
        .unwrap_or_else(|| panic!("same-role sibling missing: {scanned:?}"));

    assert_eq!(
        as_text(&sibling.values[1]).as_deref(),
        Some("10.0.0.7"),
        "the same login role's other backend was masked — masking must follow the role, \
         not the backend pid"
    );

    db.destroy_session(first).unwrap();
    db.destroy_session(second).unwrap();
}
