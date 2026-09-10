//! GH #30 — a parameterized `INSERT … RETURNING` inside `BEGIN … ROLLBACK`
//! (the extended-protocol shape Prisma / node-pg send) must NOT be persisted.
//!
//! Intended file name: `tests/gh_issue_30.rs`.
//!
//! # What this adds over `tests/prisma_p0_extended_returning_txn.rs`
//!
//! That file already pins the core contract for
//! `EmbeddedDatabase::execute_params_returning_for_session`. It does NOT cover
//! three things the issue explicitly reports:
//!
//!   1. the issue's literal shape — a `UUID PRIMARY KEY`, not an `INT` one;
//!   2. the issue's second, louder symptom: *"a retry of the same statement
//!      raises a duplicate primary key error"*. That is a statement about the
//!      UNIQUE/PK index, not about row visibility. The PK ART index is
//!      maintained EAGERLY for in-transaction inserts and unwound from an undo
//!      log on rollback, so a row can be correctly invisible after `ROLLBACK`
//!      while the index entry that rejects the retry survives. Only a retry
//!      proves the index was unwound;
//!   3. `ON CONFLICT DO NOTHING` / a second `RETURNING` statement in the same
//!      aborted transaction.
//!
//! The wire half of the proof (that Parse/Bind/Execute actually routes to the
//! `_for_session` entry point rather than the session-less twin) lives in the
//! companion `wire_30.rs` snippet for
//! `src/protocol/postgres/wire_tests.rs` — an embedded test cannot see a
//! mis-wired protocol handler.
//!
//! # Expected outcome on the tree as of `e6ed61f` (v4.31.1)
//!
//! ALL tests in this file are expected to PASS: the session-transaction slot is
//! threaded as `Some(&txn)` at `src/lib.rs:18853`, and a session whose slot has
//! vanished is an ERROR (`src/lib.rs:18841-18845`), never a silent autocommit.
//! They are permanent regression tests for that wiring.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::session::SessionId;
use heliosdb_nano::{EmbeddedDatabase, Value};

const PROBE_ID: &str = "a1b2c3d4-0000-4000-8000-000000000001";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The issue's table, verbatim: `("id" UUID PRIMARY KEY, "v" INTEGER)`.
fn uuid_db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    db.execute("CREATE TABLE rbprobe (id UUID PRIMARY KEY, v INTEGER)")
        .expect("create rbprobe");
    db
}

fn int_db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    db.execute("CREATE TABLE ints (id INT PRIMARY KEY, v INTEGER)")
        .expect("create ints");
    db
}

fn session(db: &EmbeddedDatabase, name: &str) -> SessionId {
    db.create_wire_session(name).expect("wire session")
}

/// Number of rows in `table` visible to `sid` — a real row scan, deliberately
/// NOT `SELECT count(*)`: COUNT(*) can be answered from the primary-key ART
/// index, which is maintained eagerly for in-transaction writes and is
/// therefore not a witness for what a row read can see.
fn row_count(db: &EmbeddedDatabase, sid: SessionId, table: &str) -> usize {
    let (rows, _cols) = db
        .query_with_columns_for_session(sid, &format!("SELECT id FROM {table}"))
        .expect("scan");
    rows.len()
}

// ---------------------------------------------------------------------------
// 0. POSITIVE CONTROL — the harness itself runs and can observe a write.
// ---------------------------------------------------------------------------

/// Nothing about transactions: a plain autocommit insert must be visible.
/// If THIS ever fails, every other assertion in the file is meaningless.
#[test]
fn positive_control_the_harness_can_see_a_committed_row() {
    let db = uuid_db();
    let sid = session(&db, "control");

    assert_eq!(row_count(&db, sid, "rbprobe"), 0, "vacuity: the table starts empty");

    let (affected, rows) = db
        .execute_params_returning_for_session(
            sid,
            "INSERT INTO rbprobe (id, v) VALUES ($1, $2) RETURNING id",
            &[Value::String(PROBE_ID.into()), Value::Int4(1)],
        )
        .expect("autocommit insert returning");
    assert_eq!(affected, 1, "one row inserted");
    assert_eq!(rows.len(), 1, "RETURNING must emit exactly one row");
    assert_eq!(
        row_count(&db, sid, "rbprobe"),
        1,
        "an autocommit RETURNING insert must be visible"
    );

    db.destroy_session(sid).expect("destroy");
}

// ---------------------------------------------------------------------------
// 1. The issue's literal reproducer — UUID primary key.
// ---------------------------------------------------------------------------

/// `BEGIN; INSERT INTO rbprobe (id,v) VALUES ($1,$2) RETURNING id; ROLLBACK;`
/// then `SELECT count(*) WHERE id = $1` must be 0.
#[test]
fn uuid_pk_params_returning_insert_is_undone_by_rollback() {
    let db = uuid_db();
    let sid = session(&db, "prisma");

    db.begin_transaction_for_session(sid).expect("begin");
    let (affected, rows) = db
        .execute_params_returning_for_session(
            sid,
            "INSERT INTO rbprobe (id, v) VALUES ($1, $2) RETURNING id",
            &[Value::String(PROBE_ID.into()), Value::Int4(1)],
        )
        .expect("insert returning");
    assert_eq!(affected, 1, "vacuity: the INSERT must actually have inserted a row");
    assert_eq!(rows.len(), 1, "vacuity: RETURNING must have produced a row");

    db.rollback_transaction_for_session(sid).expect("rollback");

    assert_eq!(
        row_count(&db, sid, "rbprobe"),
        0,
        "*** GH#30: a parameterized UUID-PK INSERT … RETURNING escaped the session \
         transaction — ROLLBACK did not undo it ***"
    );
    db.destroy_session(sid).expect("destroy");
}

/// The issue's SECOND symptom, and the one no existing test covers: after the
/// `ROLLBACK`, retrying the identical statement must SUCCEED.
///
/// A row can be invisible to a scan and still occupy the primary-key ART index
/// (that index is written eagerly inside a transaction and unwound from an undo
/// log at rollback). If the undo did not run, this retry fails with a duplicate
/// primary key — exactly what the issue reports — even though the row-count
/// assertion above passes.
#[test]
fn retrying_the_same_pk_after_rollback_must_not_be_a_duplicate_key_error() {
    let db = uuid_db();
    let sid = session(&db, "prisma");

    db.begin_transaction_for_session(sid).expect("begin");
    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO rbprobe (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::String(PROBE_ID.into()), Value::Int4(1)],
    )
    .expect("first insert returning");
    db.rollback_transaction_for_session(sid).expect("rollback");

    let retry = db.execute_params_returning_for_session(
        sid,
        "INSERT INTO rbprobe (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::String(PROBE_ID.into()), Value::Int4(1)],
    );
    let (affected, rows) = retry.unwrap_or_else(|e| {
        panic!(
            "*** GH#30: retrying the rolled-back INSERT was rejected — the rolled-back row \
             still occupies the primary-key index: {e} ***"
        )
    });
    assert_eq!(affected, 1, "the retry must insert one row");
    assert_eq!(rows.len(), 1, "the retry must return one row");
    assert_eq!(
        row_count(&db, sid, "rbprobe"),
        1,
        "after the retry the row must exist exactly once"
    );

    db.destroy_session(sid).expect("destroy");
}

/// The same retry contract on an INT primary key, so a UUID-specific coercion
/// quirk cannot be mistaken for the transaction bug.
#[test]
fn retrying_the_same_int_pk_after_rollback_must_not_be_a_duplicate_key_error() {
    let db = int_db();
    let sid = session(&db, "prisma-int");

    db.begin_transaction_for_session(sid).expect("begin");
    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO ints (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::Int4(1), Value::Int4(10)],
    )
    .expect("first insert returning");
    db.rollback_transaction_for_session(sid).expect("rollback");
    assert_eq!(row_count(&db, sid, "ints"), 0, "the rolled-back row must be gone");

    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO ints (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::Int4(1), Value::Int4(20)],
    )
    .unwrap_or_else(|e| panic!("*** GH#30: INT-PK retry after ROLLBACK rejected: {e} ***"));

    db.destroy_session(sid).expect("destroy");
}

// ---------------------------------------------------------------------------
// 2. Multi-statement Prisma `$transaction` shape — the impact the issue names.
// ---------------------------------------------------------------------------

/// "create parent + child": two parameterized `RETURNING` writes in one
/// transaction, aborted. NEITHER may survive — a fix that threads the
/// transaction only for the first statement of a transaction would pass the
/// single-statement tests above and fail here.
#[test]
fn two_params_returning_writes_in_one_aborted_transaction_both_vanish() {
    let db = uuid_db();
    let sid = session(&db, "prisma-multi");

    db.begin_transaction_for_session(sid).expect("begin");
    for (id, v) in [
        ("a1b2c3d4-0000-4000-8000-000000000001", 1),
        ("a1b2c3d4-0000-4000-8000-000000000002", 2),
    ] {
        db.execute_params_returning_for_session(
            sid,
            "INSERT INTO rbprobe (id, v) VALUES ($1, $2) RETURNING id",
            &[Value::String(id.into()), Value::Int4(v)],
        )
        .expect("insert returning");
    }
    assert_eq!(
        row_count(&db, sid, "rbprobe"),
        2,
        "vacuity: the inserting session must see its own two uncommitted rows"
    );

    db.rollback_transaction_for_session(sid).expect("rollback");

    assert_eq!(
        row_count(&db, sid, "rbprobe"),
        0,
        "*** GH#30: a multi-statement parameterized RETURNING transaction was not atomic ***"
    );
    db.destroy_session(sid).expect("destroy");
}

/// `INSERT … ON CONFLICT DO NOTHING RETURNING` — Prisma's `upsert` shape. Its
/// arbiter probe reads the ART index, so it is the arm most likely to bypass a
/// transaction-threaded write path.
#[test]
fn params_returning_on_conflict_do_nothing_is_undone_by_rollback() {
    let db = int_db();
    let sid = session(&db, "prisma-upsert");
    db.execute("INSERT INTO ints VALUES (1, 100)").expect("seed");

    db.begin_transaction_for_session(sid).expect("begin");
    // Conflicting row → DO NOTHING.
    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO ints (id, v) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING RETURNING id",
        &[Value::Int4(1), Value::Int4(999)],
    )
    .expect("conflicting upsert");
    // Non-conflicting row → inserted.
    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO ints (id, v) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING RETURNING id",
        &[Value::Int4(2), Value::Int4(200)],
    )
    .expect("new upsert");
    db.rollback_transaction_for_session(sid).expect("rollback");

    assert_eq!(
        row_count(&db, sid, "ints"),
        1,
        "*** GH#30: an ON CONFLICT DO NOTHING … RETURNING row survived ROLLBACK ***"
    );
    db.destroy_session(sid).expect("destroy");
}

// ---------------------------------------------------------------------------
// 3. The other executor family, and the commit half — regression guards, so a
//    fix cannot be "make the write a no-op" or "break the text family".
// ---------------------------------------------------------------------------

/// Text family (`execute_returning_for_session`, the psql simple-protocol
/// route) must keep honouring the same transaction. Passes before and after.
#[test]
fn text_family_returning_is_undone_by_rollback() {
    let db = uuid_db();
    let sid = session(&db, "psql");

    db.begin_transaction_for_session(sid).expect("begin");
    db.execute_returning_for_session(
        sid,
        &format!("INSERT INTO rbprobe (id, v) VALUES ('{PROBE_ID}', 1) RETURNING id"),
    )
    .expect("text insert returning");
    db.rollback_transaction_for_session(sid).expect("rollback");

    assert_eq!(
        row_count(&db, sid, "rbprobe"),
        0,
        "the text family must still honour the session transaction"
    );
    db.destroy_session(sid).expect("destroy");
}

/// Params family WITHOUT `RETURNING` (`execute_params_for_session`) — the third
/// route. Passes before and after.
#[test]
fn params_family_without_returning_is_undone_by_rollback() {
    let db = uuid_db();
    let sid = session(&db, "params");

    db.begin_transaction_for_session(sid).expect("begin");
    db.execute_params_for_session(
        sid,
        "INSERT INTO rbprobe (id, v) VALUES ($1, $2)",
        &[Value::String(PROBE_ID.into()), Value::Int4(1)],
    )
    .expect("params insert");
    db.rollback_transaction_for_session(sid).expect("rollback");

    assert_eq!(
        row_count(&db, sid, "rbprobe"),
        0,
        "params-without-RETURNING must roll back"
    );
    db.destroy_session(sid).expect("destroy");
}

/// COMMIT must still persist — the fix must not turn the write into a no-op.
#[test]
fn params_returning_insert_persists_on_commit() {
    let db = uuid_db();
    let sid = session(&db, "prisma-commit");

    db.begin_transaction_for_session(sid).expect("begin");
    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO rbprobe (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::String(PROBE_ID.into()), Value::Int4(7)],
    )
    .expect("insert returning");
    db.commit_transaction_for_session(sid).expect("commit");

    assert_eq!(
        row_count(&db, sid, "rbprobe"),
        1,
        "a COMMITTED parameterized RETURNING insert must persist"
    );
    // And a second session sees it.
    let other = session(&db, "observer");
    assert_eq!(
        row_count(&db, other, "rbprobe"),
        1,
        "committed rows are globally visible"
    );
    db.destroy_session(other).expect("destroy other");
    db.destroy_session(sid).expect("destroy");
}

/// Isolation: another session must NOT see the row before COMMIT. This is the
/// half that proves the write went into the transaction's write set rather than
/// straight to storage — an autocommit leak is visible here even if `ROLLBACK`
/// happened to clean up afterwards.
#[test]
fn params_returning_insert_is_invisible_to_another_session_until_commit() {
    let db = uuid_db();
    let writer = session(&db, "writer");
    let reader = session(&db, "reader");

    db.begin_transaction_for_session(writer).expect("begin");
    db.execute_params_returning_for_session(
        writer,
        "INSERT INTO rbprobe (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::String(PROBE_ID.into()), Value::Int4(1)],
    )
    .expect("insert returning");

    assert_eq!(
        row_count(&db, reader, "rbprobe"),
        0,
        "*** GH#30: an uncommitted parameterized RETURNING insert was visible to another \
         session — it autocommitted ***"
    );

    db.rollback_transaction_for_session(writer).expect("rollback");
    db.destroy_session(writer).expect("destroy writer");
    db.destroy_session(reader).expect("destroy reader");
}

// ---------------------------------------------------------------------------
// 4. ADVERSARIAL-REVIEW ADDITIONS (2026-09-08).
//
// The retry-after-ROLLBACK assertions above only exercise the
// `ArtUndoOp::RemoveInserted` arm of the per-session undo log
// (`EmbeddedDatabase::push_art_undo`, src/lib.rs:3136; replayed by
// `finish_session_art_undo(sid, true)` from
// `rollback_transaction_for_session`, src/lib.rs:17846/17853). The two other
// arms — `RestoreUpdated` and the partial `ROLLBACK TO SAVEPOINT` drain
// (`rollback_art_undo_to`, src/lib.rs:3195) — are what a parameterized
// `UPDATE … RETURNING` and Prisma's `$transaction` + savepoint shapes hit, and
// nothing in the repo asserts that a UNIQUE/PK key freed by one of those is
// actually re-usable afterwards. Both are expected to PASS on the current tree.
// ---------------------------------------------------------------------------

/// `ROLLBACK TO SAVEPOINT` must free the primary key the discarded statement
/// took, not merely hide its row. `rollback_art_undo_to` drains only the undo
/// entries appended after the savepoint mark, so an off-by-one there leaves the
/// key occupied while `SELECT` correctly shows the row gone — the same
/// "invisible row, 23505 on retry" pair the issue reports, one level down.
#[test]
fn savepoint_rollback_frees_the_primary_key_it_took() {
    let db = int_db();
    let sid = session(&db, "prisma-savepoint");

    db.begin_transaction_for_session(sid).expect("begin");
    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO ints (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::Int4(1), Value::Int4(10)],
    )
    .expect("pre-savepoint insert");
    db.execute_for_session(sid, "SAVEPOINT sp1").expect("savepoint");
    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO ints (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::Int4(2), Value::Int4(20)],
    )
    .expect("post-savepoint insert");
    db.execute_for_session(sid, "ROLLBACK TO SAVEPOINT sp1")
        .expect("rollback to savepoint");

    assert_eq!(
        row_count(&db, sid, "ints"),
        1,
        "vacuity: ROLLBACK TO SAVEPOINT must have removed the second row"
    );

    // The key `2` must be free again, and the key `1` must still be TAKEN —
    // a drain that overshot the savepoint mark would free both.
    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO ints (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::Int4(2), Value::Int4(22)],
    )
    .unwrap_or_else(|e| panic!("*** GH#30: the key freed by ROLLBACK TO SAVEPOINT was still occupied: {e} ***"));
    let overshoot = db.execute_params_returning_for_session(
        sid,
        "INSERT INTO ints (id, v) VALUES ($1, $2) RETURNING id",
        &[Value::Int4(1), Value::Int4(11)],
    );
    assert!(
        overshoot.is_err(),
        "the pre-savepoint key must STILL be taken — ROLLBACK TO SAVEPOINT drained too far"
    );

    db.commit_transaction_for_session(sid).expect("commit");
    assert_eq!(row_count(&db, sid, "ints"), 2, "ids 1 and 2 must both be committed");
    db.destroy_session(sid).expect("destroy");
}

/// A parameterized `UPDATE … RETURNING` that MOVES a UNIQUE value takes a new
/// index key and frees the old one eagerly (`ArtUndoOp::RestoreUpdated`). After
/// ROLLBACK both keys must be back where they started: the old value re-usable
/// by nobody else (it is the row's own value again) and the new value free.
///
/// This is the arm the issue's INSERT-only reproducer cannot reach, and Prisma's
/// `update({ where: { slug } , data: { slug } })` sends exactly this shape.
#[test]
fn params_returning_update_of_a_unique_value_is_undone_by_rollback() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    db.execute("CREATE TABLE slugs (id INT PRIMARY KEY, slug TEXT UNIQUE)")
        .expect("create slugs");
    db.execute("INSERT INTO slugs VALUES (1, 'alpha')").expect("seed 1");
    db.execute("INSERT INTO slugs VALUES (2, 'beta')").expect("seed 2");
    let sid = session(&db, "prisma-unique-move");

    db.begin_transaction_for_session(sid).expect("begin");
    let (affected, rows) = db
        .execute_params_returning_for_session(
            sid,
            "UPDATE slugs SET slug = $1 WHERE id = $2 RETURNING slug",
            &[Value::String("gamma".into()), Value::Int4(1)],
        )
        .expect("update returning");
    assert_eq!(affected, 1, "vacuity: the UPDATE must have matched a row");
    assert_eq!(rows.len(), 1, "vacuity: RETURNING must have produced a row");

    db.rollback_transaction_for_session(sid).expect("rollback");

    // 1. The row's value is back.
    let (restored, _cols) = db
        .query_with_columns_for_session(sid, "SELECT slug FROM slugs WHERE id = 1")
        .expect("read back");
    assert_eq!(restored.len(), 1, "the row must still exist after ROLLBACK");
    assert_eq!(
        restored[0].values[0],
        Value::String("alpha".into()),
        "*** GH#30: a rolled-back parameterized UPDATE … RETURNING kept its new value ***"
    );

    // 2. The value the rolled-back UPDATE reserved must be FREE for someone else.
    db.execute_params_for_session(
        sid,
        "UPDATE slugs SET slug = $1 WHERE id = $2",
        &[Value::String("gamma".into()), Value::Int4(2)],
    )
    .unwrap_or_else(|e| panic!("*** GH#30: the UNIQUE key a rolled-back UPDATE reserved is still occupied: {e} ***"));

    // 3. And the restored value must still be ENFORCED — the undo must not have
    //    dropped `alpha` out of the index altogether (fail-open would be worse
    //    than the bug).
    let dup = db.execute_params_for_session(
        sid,
        "INSERT INTO slugs (id, slug) VALUES ($1, $2)",
        &[Value::Int4(3), Value::String("alpha".into())],
    );
    assert!(
        dup.is_err(),
        "*** GH#30: the ROLLBACK un-indexed the restored UNIQUE value — the constraint is now \
         enforced by nothing ***"
    );

    db.destroy_session(sid).expect("destroy");
}
