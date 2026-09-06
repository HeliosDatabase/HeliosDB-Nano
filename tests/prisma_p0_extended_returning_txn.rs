//! Prisma P0 spec 04 — a parameterized `INSERT`/`UPDATE`/`DELETE … RETURNING`
//! must join the session's explicit transaction.
//!
//! Prisma binds every value and appends `RETURNING` to every create/update/
//! delete, so *every* write inside a `prisma.$transaction(...)` block arrives as
//! the extended-protocol shape exercised here. Until this change that shape —
//! and only that shape — escaped the transaction: the wire entry point
//! `EmbeddedDatabase::execute_params_returning_for_session` delegated straight
//! to the SESSION-LESS `execute_params_returning`, which resolves its
//! transaction from the GLOBAL `current_transaction` slot that a wire session
//! never uses. The write therefore went to storage immediately and
//! autocommitted: `ROLLBACK` did not undo it, another connection saw it before
//! `COMMIT`, and `ROLLBACK TO SAVEPOINT` could not take it back.
//!
//! Every other spelling honoured the transaction, which is why it survived so
//! long — the simple-protocol form (`execute_returning_for_session`) and the
//! non-`RETURNING` parameterized form (`execute_params_for_session`) are pinned
//! here too, so the fix cannot regress them.
//!
//! Sessions stand in for connections and are created through the public session
//! API (`create_wire_session` / `begin_transaction_for_session` / …), exactly as
//! the PostgreSQL handler creates them.
//!
//! Tests that FAIL on the unfixed tree are marked ***UNFIXED*** in their doc
//! comment, together with the value the unfixed tree produces.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::session::SessionId;
use heliosdb_nano::{EmbeddedDatabase, Value};

/// A table shaped like the spec's reproducer: `CREATE TABLE t (id INT PRIMARY
/// KEY, v TEXT)`.
fn db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .expect("create table");
    db
}

fn session(db: &EmbeddedDatabase, name: &str) -> SessionId {
    db.create_wire_session(name).expect("wire session")
}

/// Every `id` in `t` visible to this session, sorted.
///
/// A row-returning scan on purpose, NOT `SELECT count(*)`: COUNT(*) can be
/// answered from the primary-key ART index, and that index is maintained
/// EAGERLY for in-transaction inserts (with a rollback undo log), so it is not a
/// witness for what a row read — or another session — can actually see.
fn visible_ids(db: &EmbeddedDatabase, sid: SessionId) -> Vec<i64> {
    let (rows, _cols) = db
        .query_with_columns_for_session(sid, "SELECT id FROM t")
        .expect("select id");
    let mut ids: Vec<i64> = rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int4(n) => i64::from(n),
            Value::Int8(n) => n,
            Value::Int2(n) => i64::from(n),
            ref other => panic!("id must be an integer, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// `SELECT v FROM t WHERE id = <id>` on the session's own read path; `None` when
/// the row is not visible.
fn value_of(db: &EmbeddedDatabase, sid: SessionId, id: i32) -> Option<String> {
    let (rows, _cols) = db
        .query_with_columns_for_session(sid, &format!("SELECT v FROM t WHERE id = {id}"))
        .expect("select v");
    rows.first().map(|r| match r.values[0] {
        Value::String(ref s) => s.clone(),
        ref other => panic!("v must be text, got {other:?}"),
    })
}

/// Render one column of a RETURNING result as text.
fn returned_text(rows: &[heliosdb_nano::Tuple], col: usize) -> Vec<String> {
    rows.iter()
        .map(|r| match r.values[col] {
            Value::String(ref s) => s.clone(),
            Value::Int4(n) => n.to_string(),
            Value::Int8(n) => n.to_string(),
            ref other => panic!("unexpected RETURNING value {other:?}"),
        })
        .collect()
}

fn no_ids() -> Vec<i64> {
    Vec::new()
}

// ---------------------------------------------------------------------------
// The spec's reproducer, both outcomes
// ---------------------------------------------------------------------------

/// ***UNFIXED***: the row survives `ROLLBACK` — the visible ids are `[1]`, not
/// `[]`.
///
/// This is the literal sequence in spec 04: `BEGIN; INSERT INTO t (id, v)
/// VALUES ($1,$2) RETURNING id; ROLLBACK;`.
#[test]
fn params_returning_insert_rolls_back_with_the_session_transaction() {
    let db = db();
    let sid = session(&db, "prisma");

    db.begin_transaction_for_session(sid).expect("begin");
    let (affected, rows) = db
        .execute_params_returning_for_session(
            sid,
            "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id",
            &[Value::Int4(1), Value::String("alpha".into())],
        )
        .expect("insert returning");
    assert_eq!(affected, 1, "one row inserted");
    assert_eq!(returned_text(&rows, 0), vec!["1".to_string()], "RETURNING id");

    db.rollback_transaction_for_session(sid).expect("rollback");

    assert_eq!(
        visible_ids(&db, sid),
        no_ids(),
        "*** a parameterized INSERT … RETURNING escaped the session transaction: \
         ROLLBACK did not undo it ***"
    );
    db.destroy_session(sid).expect("destroy");
}

/// The COMMIT half: the same statement must PERSIST when the transaction
/// commits. (Passes on the unfixed tree — it autocommitted — and pins that the
/// fix did not turn the write into a no-op.)
#[test]
fn params_returning_insert_commits_with_the_session_transaction() {
    let db = db();
    let sid = session(&db, "prisma");

    db.begin_transaction_for_session(sid).expect("begin");
    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::Int4(1), Value::String("alpha".into())],
    )
    .expect("insert returning");
    db.commit_transaction_for_session(sid).expect("commit");

    assert_eq!(
        visible_ids(&db, sid),
        vec![1],
        "a committed RETURNING insert must persist"
    );
    assert_eq!(value_of(&db, sid, 1).as_deref(), Some("alpha"));
    db.destroy_session(sid).expect("destroy");
}

/// Read-your-writes: the row must be visible to its OWN session before COMMIT.
/// (Passes on the unfixed tree for the wrong reason — the row was already in
/// storage — and pins that staging it in the write set keeps it visible.)
#[test]
fn params_returning_insert_is_visible_to_its_own_session_before_commit() {
    let db = db();
    let sid = session(&db, "prisma");

    db.begin_transaction_for_session(sid).expect("begin");
    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::Int4(1), Value::String("alpha".into())],
    )
    .expect("insert returning");

    assert_eq!(
        visible_ids(&db, sid),
        vec![1],
        "the inserting session must see its own uncommitted RETURNING row"
    );
    db.rollback_transaction_for_session(sid).expect("rollback");
    db.destroy_session(sid).expect("destroy");
}

// ---------------------------------------------------------------------------
// UPDATE and DELETE — the other two shapes Prisma sends
// ---------------------------------------------------------------------------

/// ***UNFIXED***: `v` is still `'new'` after ROLLBACK instead of back at
/// `'old'`.
#[test]
fn params_returning_update_rolls_back_with_the_session_transaction() {
    let db = db();
    let sid = session(&db, "prisma");
    db.execute("INSERT INTO t (id, v) VALUES (1, 'old')").expect("seed");

    db.begin_transaction_for_session(sid).expect("begin");
    let (affected, rows) = db
        .execute_params_returning_for_session(
            sid,
            "UPDATE t SET v = $1 WHERE id = $2 RETURNING v",
            &[Value::String("new".into()), Value::Int4(1)],
        )
        .expect("update returning");
    assert_eq!(affected, 1, "one row updated");
    assert_eq!(
        returned_text(&rows, 0),
        vec!["new".to_string()],
        "RETURNING must show the POST-update value"
    );

    db.rollback_transaction_for_session(sid).expect("rollback");

    assert_eq!(
        value_of(&db, sid, 1).as_deref(),
        Some("old"),
        "*** a parameterized UPDATE … RETURNING escaped the session transaction ***"
    );
    db.destroy_session(sid).expect("destroy");
}

/// ***UNFIXED***: the row is gone after ROLLBACK — the visible ids are `[]`, not
/// `[1]`.
#[test]
fn params_returning_delete_rolls_back_with_the_session_transaction() {
    let db = db();
    let sid = session(&db, "prisma");
    db.execute("INSERT INTO t (id, v) VALUES (1, 'old')").expect("seed");

    db.begin_transaction_for_session(sid).expect("begin");
    let (affected, rows) = db
        .execute_params_returning_for_session(sid, "DELETE FROM t WHERE id = $1 RETURNING v", &[Value::Int4(1)])
        .expect("delete returning");
    assert_eq!(affected, 1, "one row deleted");
    assert_eq!(
        returned_text(&rows, 0),
        vec!["old".to_string()],
        "RETURNING the old row"
    );

    db.rollback_transaction_for_session(sid).expect("rollback");

    assert_eq!(
        visible_ids(&db, sid),
        vec![1],
        "*** a parameterized DELETE … RETURNING escaped the session transaction ***"
    );
    db.destroy_session(sid).expect("destroy");
}

// ---------------------------------------------------------------------------
// Isolation from other connections
// ---------------------------------------------------------------------------

/// ***UNFIXED***: the observer sees the row before COMMIT — its visible ids are
/// `[1]`, not `[]`.
#[test]
fn params_returning_insert_is_invisible_to_another_session_until_commit() {
    let db = db();
    let writer = session(&db, "writer");
    let observer = session(&db, "observer");

    db.begin_transaction_for_session(writer).expect("begin");
    db.execute_params_returning_for_session(
        writer,
        "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::Int4(1), Value::String("alpha".into())],
    )
    .expect("insert returning");

    assert_eq!(
        visible_ids(&db, observer),
        no_ids(),
        "*** another session saw an UNCOMMITTED parameterized RETURNING row ***"
    );

    db.commit_transaction_for_session(writer).expect("commit");
    assert_eq!(
        visible_ids(&db, observer),
        vec![1],
        "and must see it once the writer commits"
    );

    db.destroy_session(writer).expect("destroy writer");
    db.destroy_session(observer).expect("destroy observer");
}

// ---------------------------------------------------------------------------
// The statement must READ the transaction's uncommitted state, not merely write
// into it — that is what makes the RETURNING projection correct.
// ---------------------------------------------------------------------------

/// ***UNFIXED***: the parameterized UPDATE runs outside the transaction, so it
/// never sees the `'mid'` row staged by the previous statement — it matches
/// nothing and reports 0 rows instead of 1.
#[test]
fn params_returning_statement_reads_its_own_transactions_uncommitted_writes() {
    let db = db();
    let sid = session(&db, "prisma");
    db.execute("INSERT INTO t (id, v) VALUES (1, 'old')").expect("seed");

    db.begin_transaction_for_session(sid).expect("begin");
    // Staged in the session transaction by the TEXT family.
    db.execute_for_session(sid, "UPDATE t SET v = 'mid' WHERE id = 1")
        .expect("text update");

    // The PARAMS + RETURNING family must see that staged value.
    let (affected, rows) = db
        .execute_params_returning_for_session(
            sid,
            "UPDATE t SET v = $1 WHERE v = $2 RETURNING v",
            &[Value::String("new".into()), Value::String("mid".into())],
        )
        .expect("params update returning");
    assert_eq!(
        affected, 1,
        "*** the parameterized RETURNING statement did not see its own transaction's \
         uncommitted write ***"
    );
    assert_eq!(returned_text(&rows, 0), vec!["new".to_string()]);

    db.rollback_transaction_for_session(sid).expect("rollback");
    assert_eq!(value_of(&db, sid, 1).as_deref(), Some("old"), "both writes rolled back");
    db.destroy_session(sid).expect("destroy");
}

// ---------------------------------------------------------------------------
// Savepoints
// ---------------------------------------------------------------------------

/// ***UNFIXED***: `ROLLBACK TO SAVEPOINT` cannot take back a write that never
/// entered the transaction — the surviving ids are `[1, 2]`, not `[1]`.
#[test]
fn params_returning_insert_is_undone_by_rollback_to_savepoint() {
    let db = db();
    let sid = session(&db, "prisma");

    db.begin_transaction_for_session(sid).expect("begin");
    db.execute_for_session(sid, "INSERT INTO t (id, v) VALUES (1, 'keep')")
        .expect("pre-savepoint insert");
    db.execute_for_session(sid, "SAVEPOINT sp1").expect("savepoint");

    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::Int4(2), Value::String("discard".into())],
    )
    .expect("insert returning");

    db.execute_for_session(sid, "ROLLBACK TO SAVEPOINT sp1")
        .expect("rollback to savepoint");
    db.commit_transaction_for_session(sid).expect("commit");

    assert_eq!(
        visible_ids(&db, sid),
        vec![1],
        "*** ROLLBACK TO SAVEPOINT did not undo the parameterized RETURNING insert ***"
    );
    assert_eq!(value_of(&db, sid, 1).as_deref(), Some("keep"));
    db.destroy_session(sid).expect("destroy");
}

// ---------------------------------------------------------------------------
// Autocommit — outside a transaction nothing changes
// ---------------------------------------------------------------------------

/// With no open session transaction the statement still autocommits, exactly as
/// PostgreSQL does. (Passes on the unfixed tree; pins that the new
/// `session_transactions` branch did not swallow the autocommit path.)
#[test]
fn params_returning_outside_a_transaction_still_autocommits() {
    let db = db();
    let writer = session(&db, "writer");
    let observer = session(&db, "observer");

    let (affected, rows) = db
        .execute_params_returning_for_session(
            writer,
            "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id",
            &[Value::Int4(1), Value::String("alpha".into())],
        )
        .expect("autocommit insert returning");
    assert_eq!(affected, 1);
    assert_eq!(returned_text(&rows, 0), vec!["1".to_string()]);

    assert_eq!(
        visible_ids(&db, observer),
        vec![1],
        "an autocommit RETURNING insert must be visible to every session at once"
    );
    db.destroy_session(writer).expect("destroy writer");
    db.destroy_session(observer).expect("destroy observer");
}

// ---------------------------------------------------------------------------
// The interceptor prologue this entry point gained
// ---------------------------------------------------------------------------

/// ***UNFIXED***: `BEGIN` arriving on the params-`RETURNING` entry point was
/// routed at the GLOBAL transaction slot instead of the session's, so
/// `session_in_transaction` stays `false` and the statements that follow are
/// not in the session's transaction at all.
///
/// A driver that binds parameters reaches Execute for EVERY statement it sends,
/// so this entry point has to recognise the session-owned statement classes the
/// simple-protocol twin already recognised.
#[test]
fn params_returning_entry_point_routes_transaction_control_to_the_session() {
    let db = db();
    let sid = session(&db, "prisma");

    db.execute_params_returning_for_session(sid, "BEGIN", &[])
        .expect("begin through the params-returning entry point");
    assert!(
        db.session_in_transaction(sid),
        "*** BEGIN did not open the SESSION transaction ***"
    );

    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::Int4(1), Value::String("alpha".into())],
    )
    .expect("insert returning");

    db.execute_params_returning_for_session(sid, "COMMIT", &[])
        .expect("commit through the params-returning entry point");
    assert!(
        !db.session_in_transaction(sid),
        "*** COMMIT did not close the SESSION transaction ***"
    );
    assert_eq!(visible_ids(&db, sid), vec![1], "the committed row must persist");
    db.destroy_session(sid).expect("destroy");
}

// ---------------------------------------------------------------------------
// Pins: the shapes that already worked must keep working
// ---------------------------------------------------------------------------

/// The TEXT family's `RETURNING` (simple protocol) already honoured the session
/// transaction. Pinned so the params fix cannot be mistaken for a shared-code
/// change that broke it.
#[test]
fn text_family_returning_still_honours_the_session_transaction() {
    let db = db();
    let sid = session(&db, "psql");

    db.begin_transaction_for_session(sid).expect("begin");
    let (affected, rows) = db
        .execute_returning_for_session(sid, "INSERT INTO t (id, v) VALUES (1, 'alpha') RETURNING id")
        .expect("text insert returning");
    assert_eq!(affected, 1);
    assert_eq!(rows.len(), 1);
    db.rollback_transaction_for_session(sid).expect("rollback");

    assert_eq!(
        visible_ids(&db, sid),
        no_ids(),
        "the simple-protocol RETURNING form must still roll back"
    );
    db.destroy_session(sid).expect("destroy");
}

/// The PARAMS family WITHOUT `RETURNING` already honoured the session
/// transaction (`execute_params_for_session`). Pinned as the control for the
/// failing tests above: same family, same parameters, only `RETURNING` differs.
#[test]
fn params_family_without_returning_still_honours_the_session_transaction() {
    let db = db();
    let sid = session(&db, "prisma");

    db.begin_transaction_for_session(sid).expect("begin");
    let affected = db
        .execute_params_for_session(
            sid,
            "INSERT INTO t (id, v) VALUES ($1, $2)",
            &[Value::Int4(1), Value::String("alpha".into())],
        )
        .expect("params insert");
    assert_eq!(affected, 1);
    db.rollback_transaction_for_session(sid).expect("rollback");

    assert_eq!(
        visible_ids(&db, sid),
        no_ids(),
        "the non-RETURNING parameterized form must still roll back"
    );
    db.destroy_session(sid).expect("destroy");
}

// ---------------------------------------------------------------------------
// A transaction that writes the SAME ROW TWICE
//
// Joining the session transaction (above) exposed a second, older defect that
// had been unreachable from this shape while the statement autocommitted: the
// lock manager treated a transaction as conflicting with ITSELF. Row locks are
// held for the whole transaction, so a transaction's second write of a row its
// own first statement already wrote asked for a lock it already held; that
// request took the conflict path, wrote the self-edge `txn -> txn` into the
// wait-for graph, and the DFS cycle check reported
// `Deadlock: Deadlock detected for transaction N` — a transaction deadlocked
// against itself, with no second party in the system.
//
// Prisma emits this constantly (`$transaction` blocks that create-then-update,
// or update the same row from two `await`s), so it is not an exotic shape. The
// defect is family-independent — the pure-text case below fails identically on
// the unfixed tree — which is why the fix is in `LockManager`, not in either
// DML family.
// ---------------------------------------------------------------------------

/// ***UNFIXED***: the SECOND parameterized `UPDATE … RETURNING` fails with
/// `Transaction("Deadlock: Deadlock detected for transaction N")` — the
/// transaction deadlocked against its own row lock.
#[test]
fn params_returning_updates_the_same_row_twice_in_one_transaction() {
    let db = db();
    let sid = session(&db, "prisma");
    db.execute("INSERT INTO t (id, v) VALUES (1, 'old')").expect("seed");

    db.begin_transaction_for_session(sid).expect("begin");

    let (first, first_rows) = db
        .execute_params_returning_for_session(
            sid,
            "UPDATE t SET v = $1 WHERE id = $2 RETURNING v",
            &[Value::String("first".into()), Value::Int4(1)],
        )
        .expect("first params update returning");
    assert_eq!(first, 1);
    assert_eq!(returned_text(&first_rows, 0), vec!["first".to_string()]);

    // Same row, same transaction, same family — the lock this statement needs
    // is one the transaction is already holding.
    let (second, second_rows) = db
        .execute_params_returning_for_session(
            sid,
            "UPDATE t SET v = $1 WHERE id = $2 RETURNING v",
            &[Value::String("second".into()), Value::Int4(1)],
        )
        .expect("*** a transaction deadlocked against its own row lock ***");
    assert_eq!(second, 1, "the second update must match the same row");
    assert_eq!(
        returned_text(&second_rows, 0),
        vec!["second".to_string()],
        "RETURNING must show the value written by THIS statement"
    );
    assert_eq!(
        value_of(&db, sid, 1).as_deref(),
        Some("second"),
        "the transaction reads back its latest write"
    );

    db.rollback_transaction_for_session(sid).expect("rollback");
    assert_eq!(
        value_of(&db, sid, 1).as_deref(),
        Some("old"),
        "both writes roll back together"
    );
    db.destroy_session(sid).expect("destroy");
}

/// Prisma's create-then-update inside one `$transaction`: a parameterized
/// `INSERT … RETURNING` followed by a parameterized `UPDATE … RETURNING` of the
/// row it just created.
///
/// ***UNFIXED***: the `UPDATE` fails with the same self-deadlock — the `INSERT`
/// took the row's write lock a statement earlier.
#[test]
fn params_returning_insert_then_update_of_the_same_row_in_one_transaction() {
    let db = db();
    let sid = session(&db, "prisma");

    db.begin_transaction_for_session(sid).expect("begin");
    let (inserted, _) = db
        .execute_params_returning_for_session(
            sid,
            "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id",
            &[Value::Int4(1), Value::String("created".into())],
        )
        .expect("insert returning");
    assert_eq!(inserted, 1);

    let (updated, rows) = db
        .execute_params_returning_for_session(
            sid,
            "UPDATE t SET v = $1 WHERE id = $2 RETURNING v",
            &[Value::String("updated".into()), Value::Int4(1)],
        )
        .expect("*** the update deadlocked against the insert's own row lock ***");
    assert_eq!(updated, 1, "the update must find the row the insert staged");
    assert_eq!(returned_text(&rows, 0), vec!["updated".to_string()]);

    db.commit_transaction_for_session(sid).expect("commit");
    assert_eq!(visible_ids(&db, sid), vec![1]);
    assert_eq!(value_of(&db, sid, 1).as_deref(), Some("updated"));
    db.destroy_session(sid).expect("destroy");
}

/// The same-row-twice defect is NOT a params-family bug: two plain TEXT
/// statements deadlock identically, because the lock lives on the transaction,
/// not on the executor family. This test is the witness that the fix belongs in
/// `LockManager` — it fails on the unfixed tree without any parameterized or
/// `RETURNING` statement anywhere in it.
///
/// ***UNFIXED***: the second `UPDATE` fails with
/// `Transaction("Deadlock: Deadlock detected for transaction N")`.
#[test]
fn text_family_updates_the_same_row_twice_in_one_transaction() {
    let db = db();
    let sid = session(&db, "psql");
    db.execute("INSERT INTO t (id, v) VALUES (1, 'old')").expect("seed");

    db.begin_transaction_for_session(sid).expect("begin");
    let first = db
        .execute_for_session(sid, "UPDATE t SET v = 'first' WHERE id = 1")
        .expect("first text update");
    assert_eq!(first, 1);

    let second = db
        .execute_for_session(sid, "UPDATE t SET v = 'second' WHERE id = 1")
        .expect("*** a transaction deadlocked against its own row lock ***");
    assert_eq!(second, 1, "the second update must match the same row");
    assert_eq!(value_of(&db, sid, 1).as_deref(), Some("second"));

    db.rollback_transaction_for_session(sid).expect("rollback");
    assert_eq!(value_of(&db, sid, 1).as_deref(), Some("old"));
    db.destroy_session(sid).expect("destroy");
}
