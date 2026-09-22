//! sprinter 37a5968e7698 + afed6c8e8d1d — savepoint OWNERSHIP and REACHABILITY.
//!
//! Two items, one mechanism: where savepoint state lives, and who can reach it.
//!
//! # What was broken (proof-first citations, against the tree before the fix)
//!
//! * `EmbeddedDatabase.savepoints` (`src/lib.rs:1180`) was ONE
//!   `Arc<RwLock<Vec<SavepointState>>>` per database handle — shared by the
//!   global `BEGIN` slot, by the RAII `begin_transaction()` handle and by EVERY
//!   wire session at once. Both `RELEASE` arms (`src/lib.rs:9649` text family,
//!   `src/lib.rs:19340` params family) and both `ROLLBACK TO` arms
//!   (`src/lib.rs:9658`, `src/lib.rs:19350`) resolved the name with
//!   `rposition` over that ONE stack, so connection B could release — or roll
//!   back to — a savepoint connection A had established, and B's write set was
//!   then restored from A's snapshot.
//! * Nothing cleared the stack at the end of a transaction. `abort_global_slot_locked`
//!   said so in as many words (`src/lib.rs:4863`: "ROLLBACK deliberately does not
//!   clear `self.savepoints` — the savepoint stack is process-wide"), and only
//!   `Drop` (`src/lib.rs:1476`) ever emptied it. So
//!   `BEGIN; SAVEPOINT s; ROLLBACK; BEGIN; ROLLBACK TO SAVEPOINT s` succeeded and
//!   injected the dead transaction's write-set snapshot into the live one.
//!   PostgreSQL destroys savepoints at COMMIT and at ROLLBACK.
//! * On the params/extended family, `execute_plan_with_params_inner`
//!   (`src/lib.rs:18010`) refused `Savepoint` / `ReleaseSavepoint` /
//!   `RollbackToSavepoint` outright with "transaction control statements must go
//!   through the session API" whenever a session transaction was attached — which
//!   is every Parse/Bind/Execute statement a driver sends inside `BEGIN`. So
//!   Prisma's nested transactions and JDBC savepoints could not run at all.
//!
//! # What this file pins
//!
//! Savepoints are per-TRANSACTION (PostgreSQL semantics), which makes them
//! per-connection for free. Both statement families — the text family
//! (`execute_for_session`, what a simple `Query` message reaches) and the
//! bound-params family (`execute_params_for_session` /
//! `execute_params_returning_for_session`, the exact entry points
//! `handler_extended::handle_execute_extended` calls) — share ONE stack per
//! transaction and neither can see another connection's.
//!
//! No ports are bound here: these drive the engine's session API directly, which
//! is what both wire protocols call. The wire-level reachability of
//! Parse/Bind/Execute is pinned in `src/protocol/postgres/wire_tests.rs`.

use heliosdb_nano::session::SessionId;
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::sync::Arc;

/// An in-memory database with the standard two-column test table.
fn db() -> Arc<EmbeddedDatabase> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("in-memory database"));
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .expect("CREATE TABLE");
    db
}

/// A fresh wire session — one per simulated connection.
fn conn(db: &EmbeddedDatabase) -> SessionId {
    db.create_wire_session("savepoint_scoping").expect("wire session")
}

/// Every `id` in `t` this session can see, sorted.
///
/// Deliberately a row-returning scan rather than `SELECT count(*)`: the
/// primary-key ART index is maintained EAGERLY for in-transaction inserts (with
/// a rollback undo log), so a count is not a witness for what a row read — or
/// another connection — can actually see. Same helper shape as
/// `tests/prisma_p0_extended_returning_txn.rs`.
fn ids(db: &EmbeddedDatabase, sid: SessionId) -> Vec<i64> {
    let (rows, _cols) = db
        .query_with_columns_for_session(sid, "SELECT id FROM t")
        .expect("SELECT id FROM t");
    let mut out: Vec<i64> = rows
        .iter()
        .map(|r| match r.values.first() {
            Some(Value::Int4(n)) => i64::from(*n),
            Some(Value::Int8(n)) => *n,
            Some(Value::Int2(n)) => i64::from(*n),
            other => panic!("id must be an integer, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

/// Run a statement on the TEXT family (what a simple `Query` message reaches).
fn text(db: &EmbeddedDatabase, sid: SessionId, sql: &str) -> heliosdb_nano::Result<u64> {
    db.execute_for_session(sid, sql)
}

/// Run a statement on the BOUND-PARAMS family — the entry point
/// `handle_execute_extended` calls for every non-row-returning Execute
/// (`src/protocol/postgres/handler_extended.rs`, the `execute_params_for_session`
/// arm). Passing an empty parameter list is exactly what a driver that prepared a
/// parameterless statement sends.
fn params(db: &EmbeddedDatabase, sid: SessionId, sql: &str, args: &[Value]) -> heliosdb_nano::Result<u64> {
    db.execute_params_for_session(sid, sql, args)
}

fn err_text(e: &heliosdb_nano::Error) -> String {
    e.to_string()
}

/// Assert that `sql` fails **because the savepoint does not exist here** —
/// PostgreSQL's `3B001` wording — and not for some other reason.
///
/// The non-vacuity guard this file needs: a failed statement ABORTS the block
/// (HDB-008), and every later statement in that block is refused with `25P02`
/// before it reaches the savepoint code at all. A bare `is_err()` on a second
/// probe would therefore pass even if savepoints were still shared. Each probe
/// runs in its own fresh block AND checks the message.
#[track_caller]
fn assert_no_such_savepoint(result: heliosdb_nano::Result<u64>, name: &str, what: &str) {
    let e = match result {
        Err(e) => e,
        Ok(n) => panic!("{what}: expected `savepoint \"{name}\" does not exist`, got Ok({n})"),
    };
    let msg = err_text(&e);
    assert!(
        msg.contains(&format!(r#"savepoint "{name}" does not exist"#)),
        "{what}: expected `savepoint \"{name}\" does not exist` (3B001), got `{msg}` — \
         a different error means the statement never reached the savepoint code"
    );
}

// ============================================================================
// 1. Cross-connection isolation (37a5968e7698 / afed6c8e8d1d)
// ============================================================================

/// A second connection cannot SEE another connection's savepoint.
///
/// Before the fix this passed through `src/lib.rs:9649` / `:9658`, whose
/// `rposition` ran over the ONE process-wide `self.savepoints`, so B found A's
/// `sp_a` and both statements returned `Ok`.
#[test]
fn another_connections_savepoint_is_invisible() {
    let db = db();
    let a = conn(&db);
    let b = conn(&db);

    text(&db, a, "BEGIN").expect("A BEGIN");
    text(&db, a, "SAVEPOINT sp_a").expect("A SAVEPOINT");

    // Both statements, on both families, each in its own fresh block on B.
    for sql in ["RELEASE SAVEPOINT sp_a", "ROLLBACK TO SAVEPOINT sp_a"] {
        text(&db, b, "BEGIN").expect("B BEGIN");
        assert_no_such_savepoint(text(&db, b, sql), "sp_a", &format!("B, text family: `{sql}`"));
        text(&db, b, "ROLLBACK").expect("B ROLLBACK");

        text(&db, b, "BEGIN").expect("B BEGIN");
        assert_no_such_savepoint(params(&db, b, sql, &[]), "sp_a", &format!("B, params family: `{sql}`"));
        text(&db, b, "ROLLBACK").expect("B ROLLBACK");
    }

    // A's savepoint is untouched by everything B just tried.
    text(&db, a, "ROLLBACK TO SAVEPOINT sp_a").expect("A's own savepoint still resolves");
    text(&db, a, "ROLLBACK").expect("A ROLLBACK");
}

/// The corruption the item reports: B's `ROLLBACK TO` a name only A established
/// used to restore B's write set from A's snapshot, discarding B's own staged
/// row. The isolation property is that B's work survives.
#[test]
fn another_connections_rollback_to_cannot_discard_my_writes() {
    let db = db();
    let a = conn(&db);
    let b = conn(&db);

    text(&db, a, "BEGIN").expect("A BEGIN");
    text(&db, a, "INSERT INTO t VALUES (1, 'a-before')").expect("A insert 1");
    text(&db, a, "SAVEPOINT sp_a").expect("A SAVEPOINT");
    text(&db, a, "INSERT INTO t VALUES (2, 'a-after')").expect("A insert 2");

    text(&db, b, "BEGIN").expect("B BEGIN");
    text(&db, b, "INSERT INTO t VALUES (3, 'b-only')").expect("B insert 3");
    // B takes a savepoint of its OWN first, so the refused hijack below — which
    // aborts B's block under HDB-008 like any other failed statement — can be
    // recovered through it. That is also what makes the row assertion possible:
    // a read inside an aborted block is refused with 25P02.
    text(&db, b, "SAVEPOINT b_own").expect("B SAVEPOINT");
    assert_no_such_savepoint(
        text(&db, b, "ROLLBACK TO SAVEPOINT sp_a"),
        "sp_a",
        "B's ROLLBACK TO a savepoint it never established",
    );
    text(&db, b, "ROLLBACK TO SAVEPOINT b_own").expect("B recovers through its own savepoint");
    assert_eq!(
        ids(&db, b),
        vec![3],
        "B's own staged row must survive the refused ROLLBACK TO"
    );

    text(&db, b, "COMMIT").expect("B COMMIT");
    text(&db, a, "COMMIT").expect("A COMMIT");

    let c = conn(&db);
    assert_eq!(
        ids(&db, c),
        vec![1, 2, 3],
        "every committed row from both connections must be present"
    );
}

/// Two connections may hold savepoints with the SAME NAME at the same time and
/// neither may resolve to the other's.
#[test]
fn identically_named_savepoints_on_two_connections_do_not_collide() {
    let db = db();
    let a = conn(&db);
    let b = conn(&db);

    text(&db, a, "BEGIN").expect("A BEGIN");
    text(&db, b, "BEGIN").expect("B BEGIN");
    text(&db, a, "SAVEPOINT sp").expect("A SAVEPOINT sp");
    text(&db, b, "SAVEPOINT sp").expect("B SAVEPOINT sp");

    text(&db, a, "INSERT INTO t VALUES (10, 'a')").expect("A insert");
    text(&db, b, "INSERT INTO t VALUES (20, 'b')").expect("B insert");

    // A rolls back to ITS `sp`. B's row must be untouched.
    text(&db, a, "ROLLBACK TO SAVEPOINT sp").expect("A ROLLBACK TO sp");
    assert_eq!(ids(&db, a), Vec::<i64>::new(), "A's row is undone");
    assert_eq!(ids(&db, b), vec![20], "B's row is NOT undone by A's rollback");

    // B releasing ITS `sp` must not disturb A's still-live one.
    text(&db, b, "RELEASE SAVEPOINT sp").expect("B RELEASE sp");
    text(&db, a, "ROLLBACK TO SAVEPOINT sp").expect("A's sp is still established");

    text(&db, a, "COMMIT").expect("A COMMIT");
    text(&db, b, "COMMIT").expect("B COMMIT");

    let c = conn(&db);
    assert_eq!(ids(&db, c), vec![20], "only B's row was ever committed");
}

// ============================================================================
// 2. Savepoints die with their transaction (37a5968e7698)
// ============================================================================

/// PostgreSQL destroys every savepoint at COMMIT.
#[test]
fn savepoint_does_not_survive_its_transactions_commit() {
    let db = db();
    let s = conn(&db);

    text(&db, s, "BEGIN").expect("BEGIN");
    text(&db, s, "SAVEPOINT sp1").expect("SAVEPOINT");
    text(&db, s, "COMMIT").expect("COMMIT");

    // Each probe in its own fresh block — see `assert_no_such_savepoint`.
    for sql in ["RELEASE SAVEPOINT sp1", "ROLLBACK TO SAVEPOINT sp1"] {
        text(&db, s, "BEGIN").expect("BEGIN after the COMMIT");
        assert_no_such_savepoint(
            text(&db, s, sql),
            "sp1",
            &format!("`{sql}` after the establishing transaction COMMITted"),
        );
        text(&db, s, "ROLLBACK").expect("ROLLBACK");
    }
}

/// PostgreSQL destroys every savepoint at ROLLBACK.
///
/// The engine-level twin of `savepoint_hardening_tests::test_full_rollback_clears_the_savepoint_stack`,
/// which used to assert the bug ("KNOWN BUG: savepoint stack not cleared on
/// ROLLBACK; old savepoints leak") and has been INVERTED alongside this file.
#[test]
fn savepoint_does_not_survive_its_transactions_rollback() {
    let db = db();
    let s = conn(&db);

    text(&db, s, "BEGIN").expect("BEGIN");
    text(&db, s, "SAVEPOINT sp1").expect("SAVEPOINT sp1");
    text(&db, s, "SAVEPOINT sp2").expect("SAVEPOINT sp2");
    text(&db, s, "ROLLBACK").expect("ROLLBACK");

    for (sql, name) in [("RELEASE SAVEPOINT sp1", "sp1"), ("ROLLBACK TO SAVEPOINT sp2", "sp2")] {
        text(&db, s, "BEGIN").expect("BEGIN after the ROLLBACK");
        assert_no_such_savepoint(
            text(&db, s, sql),
            name,
            &format!("`{sql}` after the establishing transaction ROLLED BACK"),
        );
        text(&db, s, "ROLLBACK").expect("ROLLBACK");
    }
}

/// A stale savepoint must not be able to inject a dead transaction's write set
/// into a live one — the concrete harm behind the item's title.
#[test]
fn a_dead_transactions_snapshot_cannot_reach_a_live_transaction() {
    let db = db();
    let s = conn(&db);

    text(&db, s, "BEGIN").expect("BEGIN");
    text(&db, s, "INSERT INTO t VALUES (1, 'dead')").expect("insert into the doomed txn");
    text(&db, s, "SAVEPOINT sp1").expect("SAVEPOINT");
    text(&db, s, "ROLLBACK").expect("ROLLBACK");

    text(&db, s, "BEGIN").expect("second BEGIN");
    text(&db, s, "INSERT INTO t VALUES (2, 'live')").expect("insert into the live txn");
    // A live savepoint of this transaction's own, so the refused attempt below
    // (which aborts the block under HDB-008) can be recovered through it.
    text(&db, s, "SAVEPOINT live").expect("live SAVEPOINT");
    assert_no_such_savepoint(
        text(&db, s, "ROLLBACK TO SAVEPOINT sp1"),
        "sp1",
        "the dead transaction's savepoint must not be a rollback target",
    );
    text(&db, s, "ROLLBACK TO SAVEPOINT live").expect("recover through the live savepoint");
    assert_eq!(
        ids(&db, s),
        vec![2],
        "the live transaction's own row must still be staged"
    );
    text(&db, s, "COMMIT").expect("COMMIT");

    assert_eq!(
        ids(&db, conn(&db)),
        vec![2],
        "only the live transaction's row is committed"
    );
}

// ============================================================================
// 3. Unknown name → 3B001 invalid_savepoint_specification
// ============================================================================

/// PostgreSQL's wording and class for a name that does not exist in the CURRENT
/// transaction: `ERROR: savepoint "nope" does not exist` / `3B001`.
///
/// The SQLSTATE itself is pinned over the wire in
/// `src/protocol/postgres/wire_tests.rs`; the message shape is the anchor the
/// classifier keys on, so it is pinned here on every route that can emit it.
#[test]
fn unknown_savepoint_name_reports_postgres_wording() {
    let db = db();
    let s = conn(&db);

    // Each probe runs in a FRESH block: the failed statement aborts the block
    // under HDB-008, and a second statement sent into an aborted block is
    // refused with 25P02 before it ever reaches the savepoint code.
    for sql in ["ROLLBACK TO SAVEPOINT nope", "RELEASE SAVEPOINT nope"] {
        text(&db, s, "BEGIN").expect("BEGIN");
        let e = text(&db, s, sql).expect_err(sql);
        assert!(
            err_text(&e).contains(r#"savepoint "nope" does not exist"#),
            "`{sql}` (text family) must report PostgreSQL's wording, got {}",
            err_text(&e)
        );
        text(&db, s, "ROLLBACK").expect("ROLLBACK");

        text(&db, s, "BEGIN").expect("BEGIN");
        let e = params(&db, s, sql, &[]).expect_err(sql);
        assert!(
            err_text(&e).contains(r#"savepoint "nope" does not exist"#),
            "`{sql}` (params family) must report PostgreSQL's wording, got {}",
            err_text(&e)
        );
        text(&db, s, "ROLLBACK").expect("ROLLBACK");
    }
}

/// Releasing a savepoint destroys it and every savepoint taken after it, so
/// neither is a valid target afterwards.
#[test]
fn released_savepoint_is_gone_together_with_everything_after_it() {
    let db = db();
    let s = conn(&db);

    for (probe, name, why) in [
        (
            "ROLLBACK TO SAVEPOINT outer",
            "outer",
            "the released savepoint is destroyed",
        ),
        (
            "ROLLBACK TO SAVEPOINT inner",
            "inner",
            "a savepoint established after the released one is destroyed with it",
        ),
    ] {
        text(&db, s, "BEGIN").expect("BEGIN");
        text(&db, s, "SAVEPOINT outer").expect("SAVEPOINT outer");
        text(&db, s, "SAVEPOINT inner").expect("SAVEPOINT inner");
        text(&db, s, "RELEASE SAVEPOINT outer").expect("RELEASE outer");
        assert_no_such_savepoint(text(&db, s, probe), name, why);
        text(&db, s, "ROLLBACK").expect("ROLLBACK");
    }
}

// ============================================================================
// 4. Name reuse targets the MOST RECENT savepoint
// ============================================================================

/// PostgreSQL allows a savepoint name to be reused; `ROLLBACK TO` resolves the
/// most recent one, and the earlier one stays established (shadowed) until the
/// newer one is released.
#[test]
fn reused_savepoint_name_targets_the_most_recent() {
    let db = db();
    let s = conn(&db);

    text(&db, s, "BEGIN").expect("BEGIN");
    text(&db, s, "INSERT INTO t VALUES (1, 'first')").expect("insert 1");
    text(&db, s, "SAVEPOINT s").expect("SAVEPOINT s (outer)");
    text(&db, s, "INSERT INTO t VALUES (2, 'second')").expect("insert 2");
    text(&db, s, "SAVEPOINT s").expect("SAVEPOINT s (inner, same name)");
    text(&db, s, "INSERT INTO t VALUES (3, 'third')").expect("insert 3");

    text(&db, s, "ROLLBACK TO SAVEPOINT s").expect("ROLLBACK TO s");
    assert_eq!(
        ids(&db, s),
        vec![1, 2],
        "ROLLBACK TO a reused name must target the MOST RECENT savepoint"
    );

    // The inner `s` survives its own ROLLBACK TO (PostgreSQL keeps the target
    // established). Releasing it exposes the outer one of the same name.
    text(&db, s, "RELEASE SAVEPOINT s").expect("RELEASE the inner s");
    text(&db, s, "ROLLBACK TO SAVEPOINT s").expect("the outer s of the same name is still live");
    assert_eq!(
        ids(&db, s),
        vec![1],
        "the shadowed outer savepoint becomes the target once the inner one is released"
    );

    text(&db, s, "COMMIT").expect("COMMIT");
    assert_eq!(ids(&db, conn(&db)), vec![1]);
}

// ============================================================================
// 5. The bound-params / extended-protocol path (afed6c8e8d1d)
// ============================================================================

/// All three savepoint statements must be REACHABLE on the params family inside
/// a session transaction.
///
/// Before the fix every one of them returned
/// `transaction control statements must go through the session API`
/// from `src/lib.rs:18010`, because that guard lumped the savepoint family in
/// with BEGIN / COMMIT / ROLLBACK.
#[test]
fn params_family_reaches_all_three_savepoint_statements() {
    let db = db();
    let s = conn(&db);

    text(&db, s, "BEGIN").expect("BEGIN");
    params(&db, s, "SAVEPOINT sp1", &[]).expect("SAVEPOINT must be reachable on the params family");
    params(&db, s, "ROLLBACK TO SAVEPOINT sp1", &[])
        .expect("ROLLBACK TO SAVEPOINT must be reachable on the params family");
    params(&db, s, "RELEASE SAVEPOINT sp1", &[]).expect("RELEASE must be reachable on the params family");
    text(&db, s, "COMMIT").expect("COMMIT");
}

/// A write made with BOUND PARAMETERS is undone by a savepoint rollback issued
/// on the same family — the Prisma / JDBC nested-transaction shape end to end.
#[test]
fn bound_params_write_is_undone_by_a_params_rollback_to_savepoint() {
    let db = db();
    let s = conn(&db);

    text(&db, s, "BEGIN").expect("BEGIN");
    params(
        &db,
        s,
        "INSERT INTO t (id, v) VALUES ($1, $2)",
        &[Value::Int4(1), Value::String("keep".into())],
    )
    .expect("bound insert before the savepoint");
    params(&db, s, "SAVEPOINT sp1", &[]).expect("SAVEPOINT");
    params(
        &db,
        s,
        "INSERT INTO t (id, v) VALUES ($1, $2)",
        &[Value::Int4(2), Value::String("discard".into())],
    )
    .expect("bound insert after the savepoint");
    assert_eq!(ids(&db, s), vec![1, 2], "both rows are staged");

    params(&db, s, "ROLLBACK TO SAVEPOINT sp1", &[]).expect("ROLLBACK TO SAVEPOINT");
    assert_eq!(
        ids(&db, s),
        vec![1],
        "the post-savepoint bound-parameter insert must be undone"
    );

    text(&db, s, "COMMIT").expect("COMMIT");
    assert_eq!(ids(&db, conn(&db)), vec![1], "only the kept row is committed");
}

/// `INSERT … RETURNING` with bound parameters (the third extended-protocol
/// execution entry, `execute_params_returning_for_session`) is undone too.
#[test]
fn bound_params_returning_write_is_undone_by_rollback_to_savepoint() {
    let db = db();
    let s = conn(&db);

    text(&db, s, "BEGIN").expect("BEGIN");
    text(&db, s, "INSERT INTO t VALUES (1, 'keep')").expect("insert 1");
    params(&db, s, "SAVEPOINT sp1", &[]).expect("SAVEPOINT");

    let (_n, returned) = db
        .execute_params_returning_for_session(
            s,
            "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id",
            &[Value::Int4(2), Value::String("discard".into())],
        )
        .expect("bound RETURNING insert");
    assert_eq!(returned.len(), 1, "RETURNING must project the inserted row");

    params(&db, s, "ROLLBACK TO SAVEPOINT sp1", &[]).expect("ROLLBACK TO SAVEPOINT");
    text(&db, s, "COMMIT").expect("COMMIT");

    assert_eq!(
        ids(&db, conn(&db)),
        vec![1],
        "ROLLBACK TO SAVEPOINT must undo the parameterized RETURNING insert"
    );
}

/// The two families share ONE stack — the transaction's. A savepoint taken on
/// the text path is a valid target on the params path and vice versa, which is
/// what a driver that mixes simple `BEGIN` with prepared savepoints depends on.
#[test]
fn the_two_families_share_one_stack_per_transaction() {
    let db = db();
    let s = conn(&db);

    text(&db, s, "BEGIN").expect("BEGIN");
    text(&db, s, "INSERT INTO t VALUES (1, 'keep')").expect("insert 1");
    text(&db, s, "SAVEPOINT text_sp").expect("SAVEPOINT on the text family");
    params(
        &db,
        s,
        "INSERT INTO t (id, v) VALUES ($1, $2)",
        &[Value::Int4(2), Value::String("discard".into())],
    )
    .expect("bound insert");
    params(&db, s, "ROLLBACK TO SAVEPOINT text_sp", &[]).expect("params ROLLBACK TO a text-family savepoint");
    assert_eq!(ids(&db, s), vec![1]);

    params(&db, s, "SAVEPOINT params_sp", &[]).expect("SAVEPOINT on the params family");
    text(&db, s, "INSERT INTO t VALUES (3, 'discard')").expect("text insert");
    text(&db, s, "ROLLBACK TO SAVEPOINT params_sp").expect("text ROLLBACK TO a params-family savepoint");
    assert_eq!(ids(&db, s), vec![1]);

    text(&db, s, "COMMIT").expect("COMMIT");
    assert_eq!(ids(&db, conn(&db)), vec![1]);
}

/// Carving the savepoint family out of the `src/lib.rs:18010` guard must not
/// make a savepoint statement a silent no-op OUTSIDE a transaction: the params
/// family's "SAVEPOINT can only be used within a transaction" refusal (in the
/// pre-fix tree at `src/lib.rs:19326`) is unchanged.
#[test]
fn params_family_still_refuses_a_savepoint_outside_a_transaction() {
    let db = db();
    let s = conn(&db);

    let e = params(&db, s, "SAVEPOINT sp1", &[]).expect_err("SAVEPOINT outside a transaction");
    assert!(
        err_text(&e).to_ascii_lowercase().contains("transaction"),
        "SAVEPOINT with no transaction open must still be refused, got {}",
        err_text(&e)
    );
    assert!(
        params(&db, s, "ROLLBACK TO SAVEPOINT sp1", &[]).is_err(),
        "and nothing may have been recorded by the refused SAVEPOINT"
    );
}

// ============================================================================
// 6. The embedded global slot and the RAII handle keep their own stacks
// ============================================================================

/// The embedded `db.execute("BEGIN")` global slot is a transaction like any
/// other: its savepoints die with it, and a wire session cannot see them.
#[test]
fn the_global_slot_and_a_wire_session_do_not_share_a_stack() {
    let db = db();
    let s = conn(&db);

    db.execute("BEGIN").expect("global BEGIN");
    db.execute_returning("SAVEPOINT global_sp").expect("global SAVEPOINT");

    text(&db, s, "BEGIN").expect("session BEGIN");
    assert_no_such_savepoint(
        text(&db, s, "ROLLBACK TO SAVEPOINT global_sp"),
        "global_sp",
        "a wire session must not see the embedded global slot's savepoint",
    );
    text(&db, s, "ROLLBACK").expect("session ROLLBACK");

    db.execute("ROLLBACK").expect("global ROLLBACK");
    db.execute("BEGIN").expect("second global BEGIN");
    assert_no_such_savepoint(
        db.execute_returning("ROLLBACK TO SAVEPOINT global_sp").map(|(n, _)| n),
        "global_sp",
        "the global slot's savepoint must not survive its own ROLLBACK",
    );
    db.execute("ROLLBACK").expect("second global ROLLBACK");
}

/// A savepoint established inside the RAII `db.begin_transaction()` handle dies
/// with that handle and is invisible to a concurrent wire session.
#[test]
fn the_raii_transaction_handle_owns_its_savepoints() {
    let db = db();
    let s = conn(&db);

    {
        let tx = db.begin_transaction().expect("RAII transaction");
        tx.execute("INSERT INTO t VALUES (1, 'raii')").expect("insert");
        tx.execute("SAVEPOINT raii_sp").expect("SAVEPOINT on the RAII handle");
        tx.execute("INSERT INTO t VALUES (2, 'undone')").expect("insert 2");

        text(&db, s, "BEGIN").expect("session BEGIN");
        assert_no_such_savepoint(
            text(&db, s, "ROLLBACK TO SAVEPOINT raii_sp"),
            "raii_sp",
            "a wire session must not see the RAII handle's savepoint",
        );
        text(&db, s, "ROLLBACK").expect("session ROLLBACK");

        tx.execute("ROLLBACK TO SAVEPOINT raii_sp")
            .expect("the RAII handle's own savepoint resolves");
        tx.commit().expect("commit");
    }

    assert_eq!(
        ids(&db, conn(&db)),
        vec![1],
        "the post-savepoint row was undone; the pre-savepoint row committed"
    );
}
