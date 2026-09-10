//! GH #26 — `pg_advisory_lock` / `pg_advisory_unlock` family (blocks `prisma migrate`).
//!
//! Compile as `tests/gh_issue_26.rs`.
//!
//! The family was reported missing on 3.58.1 (`unknown function`) and reported
//! implemented for 4.31.0. This file is the *audit*: it walks the ENTIRE
//! surface the issue names — `pg_advisory_lock`, `pg_advisory_unlock`,
//! `pg_advisory_unlock_all`, `pg_try_advisory_lock`, `pg_advisory_xact_lock`,
//! `pg_try_advisory_xact_lock`, the `(int, int)` overload as well as the
//! `bigint` one, session-scope release at DISCONNECT and transaction-scope
//! release at COMMIT/ROLLBACK — on BOTH DML executor families:
//!
//! * TEXT family   — `query_with_columns_for_session` / `execute_for_session`
//!                   → `execute_in_transaction_inner` (psql simple protocol).
//! * PARAMS family — `query_params_for_session` / `execute_params_for_session`
//!                   → `execute_plan_with_params_inner` (node-pg / Prisma
//!                   extended protocol, and the REST layer).
//!
//! `tests/prisma_p0_advisory_locks.rs` already covers the core semantics. What
//! this file adds is the sub-cases that file does NOT reach: the `(int, int)`
//! overload for EVERY spelling (that file only tries `pg_try_advisory_lock`),
//! transaction-scope release of a pair-keyed lock, `pg_advisory_unlock_all`
//! across both key kinds, argument-arity/typing rejections, and a per-function
//! reachability sweep on both families.
//!
//! Implementation under audit: `src/advisory_lock.rs` (manager + `evaluate`),
//! dispatch at `src/sql/evaluator.rs:1082-1087`, owner installation at
//! `src/lib.rs:18366` (`advisory_context_guard`), session release at
//! `src/lib.rs:17471`, transaction release at `src/lib.rs:17703` (COMMIT) and
//! `src/lib.rs:17821` (ROLLBACK).
//!
//! Keys are unique per test: the lock table is process-global by design
//! (`advisory_lock::manager()`), and tests in this binary run concurrently.
//! This file owns the 91_000_0xx / (7, 91_000_0xx) key space.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::sync::Arc;

fn db() -> Arc<EmbeddedDatabase> {
    Arc::new(EmbeddedDatabase::new_in_memory().expect("in-memory database"))
}

fn session(db: &EmbeddedDatabase) -> SessionId {
    db.create_session("gh26", IsolationLevel::ReadCommitted)
        .expect("session")
}

/// Which executor family a sub-case is exercising.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    /// `db.query_with_columns_for_session` — psql simple protocol.
    Text,
    /// `db.query_params_for_session` — extended protocol / REST.
    Params,
}

impl Family {
    fn name(self) -> &'static str {
        match self {
            Family::Text => "text",
            Family::Params => "params",
        }
    }
}

const BOTH: [Family; 2] = [Family::Text, Family::Params];

/// Run a one-row, one-column SELECT on the requested family and return the value.
fn scalar(db: &EmbeddedDatabase, sid: SessionId, sql: &str, family: Family) -> Value {
    let rows = match family {
        Family::Text => {
            db.query_with_columns_for_session(sid, sql)
                .unwrap_or_else(|e| panic!("[{}] {sql}: {e}", family.name()))
                .0
        }
        Family::Params => db
            .query_params_for_session(sid, sql, &[])
            .unwrap_or_else(|e| panic!("[{}] {sql}: {e}", family.name())),
    };
    assert_eq!(rows.len(), 1, "[{}] {sql} must return one row", family.name());
    assert_eq!(
        rows[0].values.len(),
        1,
        "[{}] {sql} must return one column",
        family.name()
    );
    rows[0].values[0].clone()
}

/// Same, but returns the error instead of panicking.
fn scalar_err(db: &EmbeddedDatabase, sid: SessionId, sql: &str, family: Family) -> Option<String> {
    let res = match family {
        Family::Text => db.query_with_columns_for_session(sid, sql).map(|(r, _)| r),
        Family::Params => db.query_params_for_session(sid, sql, &[]),
    };
    res.err().map(|e| e.to_string())
}

fn boolean(v: &Value, what: &str) -> bool {
    match v {
        Value::Boolean(b) => *b,
        other => panic!("{what} must return boolean, got {other:?}"),
    }
}

/// `pg_try_advisory_lock(<key spelling>)` on `family`.
fn try_lock(db: &EmbeddedDatabase, sid: SessionId, key: &str, family: Family) -> bool {
    let sql = format!("SELECT pg_try_advisory_lock({key})");
    boolean(&scalar(db, sid, &sql, family), &sql)
}

/// `pg_advisory_unlock(<key spelling>)` on `family`.
fn unlock(db: &EmbeddedDatabase, sid: SessionId, key: &str, family: Family) -> bool {
    let sql = format!("SELECT pg_advisory_unlock({key})");
    boolean(&scalar(db, sid, &sql, family), &sql)
}

// ===========================================================================
// 0. POSITIVE CONTROL — proves this file's harness actually runs.
//
// Passes before AND after any advisory-lock change. If this fails, nothing
// else in the file means anything.
// ===========================================================================

#[test]
fn positive_control_the_harness_runs_on_both_families() {
    let db = db();
    let a = session(&db);

    // Both scalar funnels answer a trivial expression.
    assert_eq!(scalar(&db, a, "SELECT 1", Family::Text), Value::Int4(1));
    assert_eq!(scalar(&db, a, "SELECT 1", Family::Params), Value::Int4(1));

    // Both DML funnels write, and the write is visible.
    db.execute_for_session(a, "CREATE TABLE gh26_control (id INT PRIMARY KEY, v INT)")
        .expect("create table");
    db.execute_for_session(a, "INSERT INTO gh26_control VALUES (1, 10)")
        .expect("text-family insert");
    db.execute_params_for_session(
        a,
        "INSERT INTO gh26_control VALUES ($1, $2)",
        &[Value::Int4(2), Value::Int4(20)],
    )
    .expect("params-family insert");
    let rows = db
        .query_with_columns_for_session(a, "SELECT id FROM gh26_control ORDER BY id")
        .expect("select")
        .0;
    assert_eq!(rows.len(), 2, "both executor families must have written a row");

    // A genuinely unknown function still errors, so an "it works" assertion
    // below cannot be satisfied by a permissive fallback.
    let err = scalar_err(&db, a, "SELECT pg_no_such_function_at_all(1)", Family::Text)
        .expect("an unknown function must error");
    assert!(
        err.contains("Unknown scalar function"),
        "the unknown-function arm must still fire, got: {err}"
    );

    db.destroy_session(a).unwrap();
}

// ===========================================================================
// 1. EVERY function in the family exists, on BOTH executor families.
//
// This is the literal issue: on 3.58.1 each of these raised
// `Unknown scalar function: pg_advisory_…` (42883 on the wire).
// ===========================================================================

#[test]
fn every_function_in_the_family_is_served_on_both_executor_families() {
    for family in BOTH {
        let db = db();
        let a = session(&db);
        let fam = family.name();
        // One distinct key per spelling so nothing collides inside the sweep.
        let base: i64 = if family == Family::Text { 91_000_100 } else { 91_000_120 };

        let calls = [
            format!("SELECT pg_advisory_lock({})", base),
            format!("SELECT pg_try_advisory_lock({})", base + 1),
            format!("SELECT pg_advisory_xact_lock({})", base + 2),
            format!("SELECT pg_try_advisory_xact_lock({})", base + 3),
            format!("SELECT pg_advisory_unlock({})", base),
            "SELECT pg_advisory_unlock_all()".to_string(),
        ];
        for sql in &calls {
            if let Some(err) = scalar_err(&db, a, sql, family) {
                panic!("[{fam}] *** {sql} IS NOT IMPLEMENTED *** — got: {err}");
            }
        }

        // ... and the schema-qualified spelling PostgreSQL also accepts
        // (`pg_catalog.` is stripped at src/sql/evaluator.rs:643).
        let sql = format!("SELECT pg_catalog.pg_try_advisory_lock({})", base + 4);
        if let Some(err) = scalar_err(&db, a, &sql, family) {
            panic!("[{fam}] *** {sql} rejected *** — got: {err}");
        }

        db.destroy_session(a).unwrap();
    }
}

/// The literal Prisma Migrate sequence, on both families, with the key Prisma
/// actually uses.
#[test]
fn the_prisma_migrate_sequence_serialises_two_connections() {
    for family in BOTH {
        let db = db();
        let migrator = session(&db);
        let rival = session(&db);
        let fam = family.name();
        // Prisma's own advisory key; distinct db per family so the process-global
        // table cannot make the two iterations interfere.
        const KEY: &str = "72707369";

        assert_eq!(
            scalar(&db, migrator, &format!("SELECT pg_advisory_lock({KEY})"), family),
            Value::Null,
            "[{fam}] pg_advisory_lock() is a void function"
        );
        assert!(
            !try_lock(&db, rival, KEY, family),
            "[{fam}] *** a second connection was also granted the migration lock ***"
        );
        assert!(
            unlock(&db, migrator, KEY, family),
            "[{fam}] the holder's pg_advisory_unlock() must return true"
        );
        assert!(
            try_lock(&db, rival, KEY, family),
            "[{fam}] the key must be free after the holder released it"
        );

        db.destroy_session(migrator).unwrap();
        db.destroy_session(rival).unwrap();
    }
}

// ===========================================================================
// 2. The (int, int) overload — for EVERY spelling, not just the try-lock.
//
// `tests/prisma_p0_advisory_locks.rs` exercises the pair form only through
// `pg_try_advisory_lock`. PostgreSQL offers the two-int32 overload on every
// member of the family; `key_from_args` (src/advisory_lock.rs:789) is the
// single place that decides.
// ===========================================================================

#[test]
fn the_int_pair_overload_is_served_by_every_spelling_on_both_families() {
    for family in BOTH {
        let db = db();
        let a = session(&db);
        let b = session(&db);
        let fam = family.name();
        let base: i32 = if family == Family::Text { 91_000_200 } else { 91_000_240 };

        // pg_advisory_lock(int, int) — void, and exclusive.
        let k1 = format!("7, {base}");
        assert_eq!(
            scalar(&db, a, &format!("SELECT pg_advisory_lock({k1})"), family),
            Value::Null,
            "[{fam}] pg_advisory_lock(int,int) must return void"
        );
        assert!(
            !try_lock(&db, b, &k1, family),
            "[{fam}] *** pg_advisory_lock(int,int) excluded nobody ***"
        );
        assert!(
            unlock(&db, a, &k1, family),
            "[{fam}] pg_advisory_unlock(int,int) must release the holder's lock"
        );
        assert!(try_lock(&db, b, &k1, family), "[{fam}] the pair key must be free again");
        assert!(unlock(&db, b, &k1, family));

        // pg_try_advisory_lock(int, int).
        let k2 = format!("7, {}", base + 1);
        assert!(try_lock(&db, a, &k2, family), "[{fam}] pg_try_advisory_lock(int,int)");
        assert!(
            !try_lock(&db, b, &k2, family),
            "[{fam}] *** pg_try_advisory_lock(int,int) excluded nobody ***"
        );

        // pg_advisory_xact_lock(int, int) inside an explicit transaction.
        let k3 = format!("7, {}", base + 2);
        db.begin_transaction_for_session(a).unwrap();
        assert_eq!(
            scalar(&db, a, &format!("SELECT pg_advisory_xact_lock({k3})"), family),
            Value::Null,
            "[{fam}] pg_advisory_xact_lock(int,int) must return void"
        );
        assert!(
            !try_lock(&db, b, &k3, family),
            "[{fam}] *** the pair-keyed xact lock excluded nobody ***"
        );

        // pg_try_advisory_xact_lock(int, int) in the same transaction.
        let k4 = format!("7, {}", base + 3);
        assert!(
            boolean(
                &scalar(&db, a, &format!("SELECT pg_try_advisory_xact_lock({k4})"), family),
                "pg_try_advisory_xact_lock(int,int)"
            ),
            "[{fam}] pg_try_advisory_xact_lock(int,int) must acquire"
        );
        assert!(!try_lock(&db, b, &k4, family));

        // COMMIT must release BOTH transaction-scope pair keys.
        db.commit_transaction_for_session(a).unwrap();
        assert!(
            try_lock(&db, b, &k3, family),
            "[{fam}] *** COMMIT did not release pg_advisory_xact_lock(int,int) ***"
        );
        assert!(
            try_lock(&db, b, &k4, family),
            "[{fam}] *** COMMIT did not release pg_try_advisory_xact_lock(int,int) ***"
        );

        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }
}

/// `pg_advisory_lock(k)` and `pg_advisory_lock(0, k)` are DIFFERENT locks —
/// PostgreSQL separates them by the lock tag's `objsubid`. Both directions.
#[test]
fn the_bigint_and_pair_key_spaces_never_collide() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const N: &str = "91000300";

    // bigint held by A → the (0, N) pair is still free for B.
    assert!(try_lock(&db, a, N, Family::Text));
    assert!(
        try_lock(&db, b, &format!("0, {N}"), Family::Text),
        "*** (0, k) collided with the bigint key k ***"
    );
    // ... and the pair itself is exclusive.
    assert!(!try_lock(&db, a, &format!("0, {N}"), Family::Text));

    // The reverse direction, on the params family, with a different key.
    const M: &str = "91000301";
    assert!(try_lock(&db, a, &format!("0, {M}"), Family::Params));
    assert!(
        try_lock(&db, b, M, Family::Params),
        "*** the bigint key k collided with the (0, k) pair ***"
    );

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

// ===========================================================================
// 3. pg_advisory_unlock_all()
// ===========================================================================

/// `pg_advisory_unlock_all()` drops every SESSION-level hold of the caller —
/// both key kinds, counters and all — and leaves other sessions' locks alone.
#[test]
fn unlock_all_releases_both_key_kinds_and_only_the_callers_locks() {
    for family in BOTH {
        let db = db();
        let a = session(&db);
        let b = session(&db);
        let fam = family.name();
        let base: i64 = if family == Family::Text { 91_000_400 } else { 91_000_420 };

        let big = format!("{base}");
        let pair = format!("7, {}", base + 1);
        let other = format!("{}", base + 2);

        assert!(try_lock(&db, a, &big, family));
        assert!(try_lock(&db, a, &big, family), "[{fam}] re-entrant hold");
        assert!(try_lock(&db, a, &pair, family));
        // A lock owned by a DIFFERENT session must survive A's unlock_all.
        assert!(try_lock(&db, b, &other, family));

        assert_eq!(
            scalar(&db, a, "SELECT pg_advisory_unlock_all()", family),
            Value::Null,
            "[{fam}] pg_advisory_unlock_all() returns void"
        );

        assert!(
            try_lock(&db, b, &big, family),
            "[{fam}] *** unlock_all left the bigint key held (counter not zeroed) ***"
        );
        assert!(
            try_lock(&db, b, &pair, family),
            "[{fam}] *** unlock_all left the (int,int) key held ***"
        );
        assert!(
            !try_lock(&db, a, &other, family),
            "[{fam}] *** unlock_all released another session's lock ***"
        );

        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }
}

/// `pg_advisory_unlock_all()` must NOT release transaction-scope holds
/// (PostgreSQL parity: those end only with the transaction).
#[test]
fn unlock_all_leaves_transaction_scope_holds_alone() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const SESSION_KEY: &str = "91000500";
    const XACT_KEY: &str = "91000501";

    db.begin_transaction_for_session(a).unwrap();
    assert!(try_lock(&db, a, SESSION_KEY, Family::Text));
    scalar(
        &db,
        a,
        &format!("SELECT pg_advisory_xact_lock({XACT_KEY})"),
        Family::Text,
    );

    scalar(&db, a, "SELECT pg_advisory_unlock_all()", Family::Text);

    assert!(
        try_lock(&db, b, SESSION_KEY, Family::Text),
        "unlock_all must release the session-scope hold"
    );
    assert!(
        !try_lock(&db, b, XACT_KEY, Family::Text),
        "*** unlock_all released a TRANSACTION-scope hold ***"
    );
    db.commit_transaction_for_session(a).unwrap();
    assert!(try_lock(&db, b, XACT_KEY, Family::Text), "COMMIT ends the xact hold");

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

// ===========================================================================
// 4. Transaction-scope release at COMMIT and at ROLLBACK, both families.
// ===========================================================================

#[test]
fn xact_locks_end_at_commit_and_at_rollback_on_both_families() {
    for family in BOTH {
        for (finish, offset) in [("COMMIT", 0_i64), ("ROLLBACK", 1)] {
            let db = db();
            let a = session(&db);
            let b = session(&db);
            let fam = family.name();
            let base: i64 = if family == Family::Text { 91_000_600 } else { 91_000_620 };
            let key = format!("{}", base + offset);

            db.begin_transaction_for_session(a).unwrap();
            scalar(&db, a, &format!("SELECT pg_advisory_xact_lock({key})"), family);
            assert!(
                !try_lock(&db, b, &key, family),
                "[{fam}] held for the duration of the transaction"
            );

            // `pg_advisory_unlock` must NOT be able to release a xact hold.
            assert!(
                !unlock(&db, a, &key, family),
                "[{fam}] *** pg_advisory_unlock released a transaction-level lock ***"
            );
            assert!(
                !try_lock(&db, b, &key, family),
                "[{fam}] still held after the failed unlock"
            );

            if finish == "COMMIT" {
                db.commit_transaction_for_session(a).unwrap();
            } else {
                db.rollback_transaction_for_session(a).unwrap();
            }
            assert!(
                try_lock(&db, b, &key, family),
                "[{fam}] *** {finish} did not release the transaction-level advisory lock ***"
            );

            db.destroy_session(a).unwrap();
            db.destroy_session(b).unwrap();
        }
    }
}

/// In autocommit the statement IS the transaction, so a transaction-scope lock
/// taken there dies with the statement — and only the keys THAT statement took.
#[test]
fn an_autocommit_xact_lock_dies_with_its_own_statement_only() {
    for family in BOTH {
        let db = db();
        let a = session(&db);
        let b = session(&db);
        let fam = family.name();
        let base: i64 = if family == Family::Text { 91_000_700 } else { 91_000_720 };
        let transient = format!("{base}");
        let kept = format!("{}", base + 1);

        // A session-scope lock A keeps across the autocommit statement.
        assert!(try_lock(&db, a, &kept, family));

        scalar(&db, a, &format!("SELECT pg_advisory_xact_lock({transient})"), family);
        assert!(
            try_lock(&db, b, &transient, family),
            "[{fam}] *** an autocommit xact lock outlived its statement ***"
        );
        assert!(
            !try_lock(&db, b, &kept, family),
            "[{fam}] *** the statement-scoped sweep released an unrelated session lock ***"
        );

        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }
}

// ===========================================================================
// 5. Release at DISCONNECT.
//
// `destroy_session` is the ONE funnel the PG handler's `Drop` uses
// (src/protocol/postgres/handler.rs:354-359) and the MySQL handler's
// (src/protocol/mysql/handler.rs:899-903), so it stands in for Terminate,
// a dropped socket and the error path alike.
// ===========================================================================

#[test]
fn disconnect_releases_every_lock_kind_the_connection_held() {
    for family in BOTH {
        let db = db();
        let a = session(&db);
        let b = session(&db);
        let fam = family.name();
        let base: i64 = if family == Family::Text { 91_000_800 } else { 91_000_820 };

        let session_big = format!("{base}");
        let session_pair = format!("7, {}", base + 1);
        let xact_big = format!("{}", base + 2);

        assert!(try_lock(&db, a, &session_big, family));
        assert!(try_lock(&db, a, &session_big, family), "[{fam}] held twice");
        assert!(try_lock(&db, a, &session_pair, family));
        db.begin_transaction_for_session(a).unwrap();
        scalar(&db, a, &format!("SELECT pg_advisory_xact_lock({xact_big})"), family);

        // The connection goes away with a transaction still open.
        db.destroy_session(a).unwrap();

        assert!(
            try_lock(&db, b, &session_big, family),
            "[{fam}] *** a session-scope bigint lock outlived the connection ***"
        );
        assert!(
            try_lock(&db, b, &session_pair, family),
            "[{fam}] *** a session-scope (int,int) lock outlived the connection ***"
        );
        assert!(
            try_lock(&db, b, &xact_big, family),
            "[{fam}] *** a transaction-scope lock outlived the connection ***"
        );

        db.destroy_session(b).unwrap();
    }
}

// ===========================================================================
// 6. The DML executor families themselves (INSERT … pg_try_advisory_lock(k)).
//
// A SELECT of the function is served by the query funnels; a DML statement
// whose value list calls it is served by `execute_in_transaction_inner`
// (text) and `execute_plan_with_params_inner` (params). Both must attribute
// the lock to the SESSION running the statement.
// ===========================================================================

#[test]
fn both_dml_executor_families_take_and_own_the_lock() {
    let db = db();
    let a = session(&db);
    let b = session(&db);
    const TEXT_KEY: i64 = 91_000_900;
    const PARAMS_KEY: i64 = 91_000_901;
    const TEXT_PAIR: i64 = 91_000_902;

    db.execute_for_session(a, "CREATE TABLE gh26_dml (id INT PRIMARY KEY, got BOOLEAN)")
        .unwrap();

    db.execute_for_session(
        a,
        &format!("INSERT INTO gh26_dml VALUES (1, pg_try_advisory_lock({TEXT_KEY}))"),
    )
    .expect("text-family DML must serve the advisory function");
    assert!(
        !try_lock(&db, b, &TEXT_KEY.to_string(), Family::Text),
        "*** the text DML family did not actually take the lock ***"
    );

    db.execute_params_for_session(
        a,
        &format!("INSERT INTO gh26_dml VALUES ($1, pg_try_advisory_lock({PARAMS_KEY}))"),
        &[Value::Int4(2)],
    )
    .expect("params-family DML must serve the advisory function");
    assert!(
        !try_lock(&db, b, &PARAMS_KEY.to_string(), Family::Text),
        "*** the params DML family did not actually take the lock ***"
    );

    // The (int, int) overload through DML too.
    db.execute_params_for_session(
        a,
        &format!("INSERT INTO gh26_dml VALUES ($1, pg_try_advisory_lock(7, {TEXT_PAIR}))"),
        &[Value::Int4(3)],
    )
    .expect("params-family DML must serve the (int,int) overload");
    assert!(!try_lock(&db, b, &format!("7, {TEXT_PAIR}"), Family::Text));

    let rows = db
        .query_with_columns_for_session(a, "SELECT got FROM gh26_dml ORDER BY id")
        .unwrap()
        .0;
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(row.values[0], Value::Boolean(true), "every acquisition succeeded");
    }

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

// ===========================================================================
// 7. Fail-closed argument handling.
//
// A wrong-arity or NULL call must be an ERROR, never a silent lock on some
// other key: a migration runner that believes it serialised when it did not
// is the failure this whole family exists to prevent.
// ===========================================================================

#[test]
fn malformed_calls_are_rejected_on_both_families() {
    for family in BOTH {
        let db = db();
        let a = session(&db);
        let fam = family.name();

        for sql in [
            "SELECT pg_advisory_lock(1, 2, 3)",
            "SELECT pg_try_advisory_lock(1, 2, 3)",
            "SELECT pg_advisory_unlock(1, 2, 3)",
            "SELECT pg_advisory_unlock_all(1)",
            "SELECT pg_advisory_lock(NULL)",
            "SELECT pg_try_advisory_lock(NULL, 1)",
            "SELECT pg_advisory_lock('not-a-number')",
        ] {
            let err = scalar_err(&db, a, sql, family)
                .unwrap_or_else(|| panic!("[{fam}] *** {sql} was ACCEPTED *** — it must be an error"));
            assert!(
                !err.contains("Unknown scalar function"),
                "[{fam}] {sql} must be an argument error, not a missing-function error: {err}"
            );
        }

        // An out-of-range (int, int) component is an error, not a silent wrap.
        let err = scalar_err(&db, a, "SELECT pg_try_advisory_lock(1, 9223372036854775807)", family)
            .unwrap_or_else(|| panic!("[{fam}] an out-of-int32-range pair component must be rejected"));
        assert!(
            err.to_ascii_lowercase().contains("out of range") || err.to_ascii_lowercase().contains("integer"),
            "[{fam}] expected a range/type error, got: {err}"
        );

        db.destroy_session(a).unwrap();
    }
}

/// The `_shared` variants are deliberately NOT implemented. They must keep
/// erroring rather than be served as EXCLUSIVE locks — two writers that each
/// asked for a shared lock would otherwise both believe they were serialised.
#[test]
fn shared_variants_stay_unimplemented_rather_than_faked() {
    for family in BOTH {
        let db = db();
        let a = session(&db);
        let fam = family.name();
        for sql in [
            "SELECT pg_advisory_lock_shared(1)",
            "SELECT pg_try_advisory_lock_shared(1)",
            "SELECT pg_advisory_unlock_shared(1)",
            "SELECT pg_advisory_xact_lock_shared(1)",
            "SELECT pg_try_advisory_xact_lock_shared(1)",
        ] {
            let err = scalar_err(&db, a, sql, family).unwrap_or_else(|| {
                panic!("[{fam}] *** {sql} was SERVED *** — a shared lock must not become exclusive")
            });
            assert!(
                err.contains("Unknown scalar function"),
                "[{fam}] {sql} must report an unknown function (42883), got: {err}"
            );
        }
        db.destroy_session(a).unwrap();
    }
}

// ===========================================================================
// 8. Observability — `pg_advisory_locks` answers "who holds 72707369?".
// ===========================================================================

#[test]
fn the_pg_advisory_locks_view_reports_both_key_kinds() {
    let db = db();
    let a = session(&db);
    const BIG: i64 = 91_001_000;
    const PAIR_KEY2: i32 = 91_001_001;

    assert!(try_lock(&db, a, &BIG.to_string(), Family::Text));
    assert!(try_lock(&db, a, &format!("7, {PAIR_KEY2}"), Family::Text));

    let (rows, cols) = db
        .query_with_columns_for_session(
            a,
            "SELECT key_kind, classid, objid, objsubid, session_id, session_locks, xact_locks, mode \
             FROM pg_advisory_locks",
        )
        .expect("pg_advisory_locks must be queryable");
    assert!(cols.len() >= 7, "columns: {cols:?}");

    // The table is process-global, so other tests contribute rows: pick ours.
    let big_row = rows
        .iter()
        .find(|r| r.values[0] == Value::String("bigint".to_string()) && r.values[2] == Value::Int8(BIG))
        .unwrap_or_else(|| panic!("the bigint key {BIG} must be listed; rows: {rows:?}"));
    assert_eq!(big_row.values[4], Value::Int8(a.0 as i64), "attributed to the holder");

    // Match on BOTH components: the lock table is process-global and every
    // other test in this binary also uses classid 7, so a classid-only probe
    // would pick up someone else's row and then fail the ownership assertion.
    let pair_row = rows
        .iter()
        .find(|r| {
            r.values[0] == Value::String("int_pair".to_string())
                && r.values[1] == Value::Int4(7)
                && r.values[2] == Value::Int8(i64::from(PAIR_KEY2))
        })
        .unwrap_or_else(|| panic!("the (7, {PAIR_KEY2}) key must be listed; rows: {rows:?}"));
    assert_eq!(pair_row.values[4], Value::Int8(a.0 as i64));

    db.destroy_session(a).unwrap();
}

// ===========================================================================
// 9. The result cache must never answer an advisory call.
// ===========================================================================

#[test]
fn identical_advisory_sql_is_never_served_from_the_result_cache() {
    for family in BOTH {
        let db = db();
        let a = session(&db);
        let b = session(&db);
        let fam = family.name();
        let key = if family == Family::Text { "91001100" } else { "91001101" };

        assert!(try_lock(&db, a, key, family));
        // Byte-identical SQL, run repeatedly by B.
        assert!(!try_lock(&db, b, key, family));
        assert!(!try_lock(&db, b, key, family));
        assert!(unlock(&db, a, key, family));
        assert!(
            try_lock(&db, b, key, family),
            "[{fam}] *** a cached `false` was served after the holder released ***"
        );

        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }
}

// ===========================================================================
// 10. Blocking acquisition still terminates.
// ===========================================================================

/// A blocked `pg_advisory_lock` is granted the moment the holder releases,
/// on both families.
#[test]
fn a_blocked_lock_is_granted_when_the_holder_releases() {
    for family in BOTH {
        let db = db();
        let a = session(&db);
        let b = session(&db);
        let fam = family.name();
        let key: i64 = if family == Family::Text { 91_001_200 } else { 91_001_201 };

        assert!(try_lock(&db, a, &key.to_string(), family));

        let waiter_db = Arc::clone(&db);
        let waiter =
            std::thread::spawn(move || scalar(&waiter_db, b, &format!("SELECT pg_advisory_lock({key})"), family));

        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(!waiter.is_finished(), "[{fam}] the waiter must still be blocked");
        assert!(unlock(&db, a, &key.to_string(), family));

        let granted = waiter.join().expect("[waiter] must not panic");
        assert_eq!(granted, Value::Null, "[{fam}] the granted lock returns void");
        assert!(!try_lock(&db, a, &key.to_string(), family), "[{fam}] B holds it now");

        db.destroy_session(a).unwrap();
        db.destroy_session(b).unwrap();
    }
}

/// A blocked waiter is cut loose by `statement_timeout` (57014
/// `query_canceled`) instead of hanging the connection forever.
#[test]
fn a_blocked_lock_honours_statement_timeout() {
    use heliosdb_nano::config::Config;

    let mut config = Config::in_memory();
    config.storage.statement_timeout_ms = Some(200);
    let db = Arc::new(EmbeddedDatabase::with_config(config).expect("db"));
    let a = session(&db);
    let b = session(&db);
    const KEY: i64 = 91_001_300;

    assert!(try_lock(&db, a, &KEY.to_string(), Family::Text));
    let started = std::time::Instant::now();
    let err = db
        .query_params_for_session(b, &format!("SELECT pg_advisory_lock({KEY})"), &[])
        .expect_err("a blocked advisory lock must time out, not hang");
    assert!(
        matches!(err, heliosdb_nano::Error::QueryTimeout(_)),
        "statement_timeout must surface as QueryTimeout (57014), got {err:?}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

// ===========================================================================
// 11. Interface coverage (repo quality gate 5).
//
// The only threshold this feature introduces is the per-session key cap. It
// must be a config knob (`[locks] max_advisory_locks_per_session`), not a
// hardcoded constant, and it must fail CLOSED — refuse a NEW key, never evict
// and never refuse a re-acquisition of a key already held.
// ===========================================================================

#[test]
fn the_per_session_cap_is_configurable_and_fails_closed() {
    use heliosdb_nano::config::Config;

    let mut config = Config::in_memory();
    config.locks.max_advisory_locks_per_session = 2;
    let db = Arc::new(EmbeddedDatabase::with_config(config).expect("db"));
    let a = session(&db);
    const BASE: i64 = 91_001_400;

    assert!(try_lock(&db, a, &BASE.to_string(), Family::Text));
    assert!(try_lock(&db, a, &(BASE + 1).to_string(), Family::Text));
    assert!(
        try_lock(&db, a, &BASE.to_string(), Family::Text),
        "re-entry on a held key must never be refused by the cap"
    );

    let err = scalar_err(
        &db,
        a,
        &format!("SELECT pg_try_advisory_lock({})", BASE + 2),
        Family::Text,
    )
    .expect("*** a third distinct key was GRANTED past the cap ***");
    assert!(err.contains("out of advisory lock slots"), "got: {err}");
    assert!(
        err.contains("max_advisory_locks_per_session"),
        "the error must name the config knob so an operator can raise it: {err}"
    );

    // The cap is per SESSION, not per process.
    let b = session(&db);
    assert!(
        try_lock(&db, b, &(BASE + 2).to_string(), Family::Text),
        "a different session must have its own budget"
    );

    // 0 = unlimited must still be expressible.
    let mut unlimited = Config::in_memory();
    unlimited.locks.max_advisory_locks_per_session = 0;
    let db2 = Arc::new(EmbeddedDatabase::with_config(unlimited).expect("db"));
    let c = db2.create_session("gh26", IsolationLevel::ReadCommitted).unwrap();
    for n in 0..8_i64 {
        assert!(
            try_lock(&db2, c, &(91_001_500 + n).to_string(), Family::Text),
            "0 must mean unlimited"
        );
    }
    db2.destroy_session(c).unwrap();

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

// ===========================================================================
// 12. The session-LESS surfaces still fail closed.
//
// `db.query()` / `db.execute()` (REST/BaaS, MCP, the REPL, embedded) carry no
// connection identity. The session-scope half must be REFUSED there rather
// than granting a lock that excludes nobody and that nothing can release.
// ===========================================================================

#[test]
fn a_sessionless_caller_cannot_strand_the_migration_key() {
    let db = db();
    const KEY: i64 = 91_001_600;

    // Whatever the session-less paths decide to do with these…
    let _ = db.query_with_columns(&format!("SELECT pg_advisory_lock({KEY})"));
    let _ = db.query_params(&format!("SELECT pg_try_advisory_lock({KEY})"), &[]);
    let _ = db.query_with_columns(&format!("SELECT pg_try_advisory_lock(7, {KEY})"));

    // … a real connection must still be able to take both key kinds.
    let a = session(&db);
    assert!(
        try_lock(&db, a, &KEY.to_string(), Family::Text),
        "*** a session-less statement stranded the bigint migration key ***"
    );
    assert!(
        try_lock(&db, a, &format!("7, {KEY}"), Family::Text),
        "*** a session-less statement stranded the (int,int) key ***"
    );
    db.destroy_session(a).unwrap();
}
