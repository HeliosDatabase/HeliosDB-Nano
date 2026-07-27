//! `ON DELETE CASCADE` / `ON DELETE SET NULL` must be atomic with the statement
//! that triggered them.
//!
//! `cascade_delete_referencing_rows` and `set_null_referencing_rows` used to
//! open — and immediately commit — their own autocommit transaction, ignoring
//! any enclosing `BEGIN`. So `BEGIN; DELETE FROM parent; ROLLBACK;` restored the
//! parent row (the outer transaction rolled back) while the child rows stayed
//! permanently deleted, or permanently NULLed: a silent atomicity violation that
//! never surfaced as an error. Both helpers now take the caller's transaction
//! and stage into it.
//!
//! Both DML executor families are covered, because referential actions became
//! reachable from the params family in v4.6.3 (see `constraint_parity_tests.rs`):
//!   * "text family"   — `db.execute()`        → `execute_in_transaction_inner`
//!   * "params family" — `db.execute_params()` → `execute_plan_with_params_inner`
//!
//! The autocommit tests are the unchanged-behavior pins: with no enclosing
//! transaction the params family still reaches the helpers' self-contained
//! "own transaction, commit immediately" branch, which must not have changed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};
use tempfile::TempDir;

fn count(db: &EmbeddedDatabase, sql: &str) -> usize {
    db.query(sql, &[]).unwrap().len()
}

/// Fetch a single scalar (row 0, column 0), or `None` when no row matched.
fn scalar(db: &EmbeddedDatabase, sql: &str) -> Option<Value> {
    db.query(sql, &[]).unwrap().first().and_then(|row| row.get(0).cloned())
}

fn parents(db: &EmbeddedDatabase) -> usize {
    count(db, "SELECT id FROM p")
}

fn children(db: &EmbeddedDatabase) -> usize {
    count(db, "SELECT id FROM c")
}

/// The FK column of child row 10, or `None` if that row is gone.
fn child_fk(db: &EmbeddedDatabase) -> Option<Value> {
    scalar(db, "SELECT p_id FROM c WHERE id = 10")
}

/// `p(id)` + `c(id, p_id REFERENCES p(id) ON DELETE CASCADE)`, seeded with
/// `p = (1)` and `c = (10, 1)`.
fn setup_cascade(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE c (id INT PRIMARY KEY, p_id INT REFERENCES p(id) ON DELETE CASCADE)")
        .unwrap();
    db.execute("INSERT INTO p VALUES (1)").unwrap();
    db.execute("INSERT INTO c VALUES (10, 1)").unwrap();
}

/// Same shape as `setup_cascade` but with `ON DELETE SET NULL`.
fn setup_set_null(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE c (id INT PRIMARY KEY, p_id INT REFERENCES p(id) ON DELETE SET NULL)")
        .unwrap();
    db.execute("INSERT INTO p VALUES (1)").unwrap();
    db.execute("INSERT INTO c VALUES (10, 1)").unwrap();
}

// ---------------------------------------------------------------------------
// ROLLBACK must undo the referential action too
// ---------------------------------------------------------------------------

/// The headline case: BEGIN; DELETE parent; ROLLBACK → the child row is back.
/// Before the fix the parent came back and the child stayed deleted.
#[test]
fn headline_cascade_delete_rolls_back_with_parent_text_family() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_cascade(&db);

    db.execute("BEGIN").unwrap();
    db.execute("DELETE FROM p WHERE id = 1").unwrap();
    db.execute("ROLLBACK").unwrap();

    assert_eq!(parents(&db), 1, "ROLLBACK must restore the parent row");
    assert_eq!(children(&db), 1, "cascade must roll back with the parent");
    assert_eq!(child_fk(&db), Some(Value::Int4(1)), "FK value restored");
}

/// Same, driven through the params family (referential actions became reachable
/// from it in v4.6.3). Mixing `execute` and `execute_params` inside one explicit
/// transaction is intentional: both must observe the same global-slot
/// transaction.
#[test]
fn cascade_delete_rolls_back_with_parent_params_family() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_cascade(&db);

    db.execute("BEGIN").unwrap();
    db.execute_params("DELETE FROM p WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    db.execute("ROLLBACK").unwrap();

    assert_eq!(parents(&db), 1, "ROLLBACK must restore the parent row");
    assert_eq!(children(&db), 1, "params cascade must roll back too");
    assert_eq!(child_fk(&db), Some(Value::Int4(1)), "FK value restored");
}

#[test]
fn set_null_delete_rolls_back_with_parent_and_fk_value_text_family() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_set_null(&db);

    db.execute("BEGIN").unwrap();
    db.execute("DELETE FROM p WHERE id = 1").unwrap();
    db.execute("ROLLBACK").unwrap();

    assert_eq!(parents(&db), 1, "ROLLBACK must restore the parent row");
    assert_eq!(children(&db), 1, "SET NULL never deletes the child row");
    assert_eq!(child_fk(&db), Some(Value::Int4(1)), "NULL must be undone");
}

#[test]
fn set_null_delete_rolls_back_params_family() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_set_null(&db);

    db.execute("BEGIN").unwrap();
    db.execute_params("DELETE FROM p WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    db.execute("ROLLBACK").unwrap();

    assert_eq!(parents(&db), 1, "ROLLBACK must restore the parent row");
    assert_eq!(children(&db), 1, "SET NULL never deletes the child row");
    assert_eq!(child_fk(&db), Some(Value::Int4(1)), "NULL must be undone");
}

// ---------------------------------------------------------------------------
// COMMIT must still make the referential action permanent
// ---------------------------------------------------------------------------

#[test]
fn cascade_delete_commits_normally_text_family() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_cascade(&db);

    db.execute("BEGIN").unwrap();
    db.execute("DELETE FROM p WHERE id = 1").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(parents(&db), 0, "COMMIT keeps the parent delete");
    assert_eq!(children(&db), 0, "COMMIT keeps the cascaded delete");
}

#[test]
fn cascade_delete_commits_normally_params_family() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_cascade(&db);

    db.execute("BEGIN").unwrap();
    db.execute_params("DELETE FROM p WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(parents(&db), 0, "COMMIT keeps the parent delete");
    assert_eq!(children(&db), 0, "COMMIT keeps the cascaded delete");
}

#[test]
fn set_null_delete_commits_normally_text_family() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_set_null(&db);

    db.execute("BEGIN").unwrap();
    db.execute("DELETE FROM p WHERE id = 1").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(parents(&db), 0, "COMMIT keeps the parent delete");
    assert_eq!(children(&db), 1, "SET NULL never deletes the child row");
    assert_eq!(child_fk(&db), Some(Value::Null), "COMMIT keeps the NULL");
}

// ---------------------------------------------------------------------------
// Autocommit — the helpers' self-contained branch, unchanged
// ---------------------------------------------------------------------------

/// No enclosing transaction: the params family reaches the helpers with
/// `active_txn = None`, i.e. the original "own transaction, commit immediately"
/// branch. It must still self-commit — no explicit COMMIT is issued here.
#[test]
fn cascade_delete_autocommit_params_still_commits_immediately() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_cascade(&db);

    db.execute_params("DELETE FROM p WHERE id = $1", &[Value::Int4(1)])
        .unwrap();

    assert_eq!(parents(&db), 0, "autocommit delete is immediate");
    assert_eq!(children(&db), 0, "autocommit cascade is immediate");
}

#[test]
fn set_null_delete_autocommit_params_still_commits_immediately() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_set_null(&db);

    db.execute_params("DELETE FROM p WHERE id = $1", &[Value::Int4(1)])
        .unwrap();

    assert_eq!(parents(&db), 0, "autocommit delete is immediate");
    assert_eq!(children(&db), 1, "SET NULL never deletes the child row");
    assert_eq!(child_fk(&db), Some(Value::Null), "SET NULL is immediate");
}

/// The text family has no `active_txn = None` path — every statement, autocommit
/// included, runs inside a per-statement transaction — so this now exercises the
/// NEW in-transaction branch with an implicit transaction that commits at
/// statement end. It must be indistinguishable from the old behavior.
#[test]
fn cascade_delete_autocommit_text_family_still_commits_immediately() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_cascade(&db);

    db.execute("DELETE FROM p WHERE id = 1").unwrap();

    assert_eq!(parents(&db), 0, "autocommit delete is immediate");
    assert_eq!(children(&db), 0, "autocommit cascade is immediate");
}

#[test]
fn set_null_delete_autocommit_text_family_still_commits_immediately() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_set_null(&db);

    db.execute("DELETE FROM p WHERE id = 1").unwrap();

    assert_eq!(parents(&db), 0, "autocommit delete is immediate");
    assert_eq!(child_fk(&db), Some(Value::Null), "SET NULL is immediate");
}

// ---------------------------------------------------------------------------
// Index (ART) state after an undone cascade
// ---------------------------------------------------------------------------

/// The cascade's ART maintenance is applied eagerly and undone through the
/// transaction's undo log (`ArtUndoOp::RestoreDeleted`). After a ROLLBACK the
/// indexes must describe the restored rows exactly — modelled on
/// `in_txn_insert_then_rollback_clears_art` in `fk_in_txn_perf_regression.rs`.
#[test]
fn cascade_rollback_leaves_no_dangling_art_entry() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_cascade(&db);

    db.execute("BEGIN").unwrap();
    db.execute("DELETE FROM p WHERE id = 1").unwrap();
    db.execute("ROLLBACK").unwrap();

    // The FK target must still resolve through the parent's PK index.
    db.execute("INSERT INTO c VALUES (20, 1)")
        .expect("the parent PK must still be indexed after the undone cascade");
    // And the child's PK index must not have lost row 10: reinserting it has
    // to be a duplicate-key violation.
    let dup = db.execute("INSERT INTO c VALUES (10, 1)");
    assert!(dup.is_err(), "restored child must still hold its PK slot");

    // A second, committed cascade must remove BOTH child rows: the undo must
    // have left the bookkeeping whole, not half-restored.
    db.execute("DELETE FROM p WHERE id = 1").unwrap();
    assert_eq!(children(&db), 0, "second cascade must clear both rows");
    assert_eq!(parents(&db), 0, "second parent delete must commit");
}

// ---------------------------------------------------------------------------
// Savepoint-bounded rollback
// ---------------------------------------------------------------------------

/// `ROLLBACK TO SAVEPOINT` replays only the undo entries staged after the
/// savepoint, so exactly the post-savepoint cascade must be undone. SAVEPOINT
/// and ROLLBACK TO SAVEPOINT go through `execute_returning` — see the header of
/// `savepoint_hardening_tests.rs`.
#[test]
fn cascade_rollback_to_savepoint_undoes_only_post_savepoint_cascade() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE c (id INT PRIMARY KEY, p_id INT REFERENCES p(id) ON DELETE CASCADE)")
        .unwrap();
    db.execute("INSERT INTO p VALUES (1), (2)").unwrap();
    db.execute("INSERT INTO c VALUES (10, 1), (20, 2)").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("DELETE FROM p WHERE id = 1").unwrap(); // cascades away c.10
    db.execute_returning("SAVEPOINT sp").unwrap();
    db.execute("DELETE FROM p WHERE id = 2").unwrap(); // cascades away c.20
    db.execute_returning("ROLLBACK TO SAVEPOINT sp").unwrap();
    db.execute("COMMIT").unwrap();

    let p1 = count(&db, "SELECT id FROM p WHERE id = 1");
    let c10 = count(&db, "SELECT id FROM c WHERE id = 10");
    let p2 = count(&db, "SELECT id FROM p WHERE id = 2");
    let c20 = count(&db, "SELECT id FROM c WHERE id = 20");
    assert_eq!(p1, 0, "pre-savepoint parent delete stays committed");
    assert_eq!(c10, 0, "pre-savepoint cascade stays committed");
    assert_eq!(p2, 1, "post-savepoint parent delete is undone");
    assert_eq!(c20, 1, "post-savepoint cascade is undone with it");

    let fk20 = scalar(&db, "SELECT p_id FROM c WHERE id = 20");
    assert_eq!(fk20, Some(Value::Int4(2)), "restored FK value intact");
}

// ---------------------------------------------------------------------------
// Logical WAL: a rolled-back referential action must leave no phantom entry
// ---------------------------------------------------------------------------

/// `log_data_delete` appends to the logical WAL at call time and no code path
/// ever writes a compensating abort record, so a cascade staged into an explicit
/// transaction must NOT log — otherwise the entry outlives the ROLLBACK and the
/// crash-recovery replay that runs at every open deletes the restored child row
/// all over again.
///
/// There is no public accessor for raw WAL entries from an integration test
/// (`wal_entries_for_tests` is `#[cfg(test)] pub(crate)`), so this asserts the
/// observable consequence instead: reopen the on-disk database and check that
/// the child row survived the replay.
#[test]
fn cascade_rollback_leaves_no_phantom_wal_delete_after_reopen() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        setup_cascade(&db);

        db.execute("BEGIN").unwrap();
        db.execute("DELETE FROM p WHERE id = 1").unwrap();
        db.execute("ROLLBACK").unwrap();

        assert_eq!(children(&db), 1, "child is back before the close");
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    assert_eq!(parents(&db), 1, "parent must survive the WAL replay");
    assert_eq!(children(&db), 1, "no phantom WAL Delete for the child");
    assert_eq!(child_fk(&db), Some(Value::Int4(1)), "FK value intact");
}

/// Mirror of the above for SET NULL: a phantom WAL Update would re-apply the
/// nulled tuple on replay.
#[test]
fn set_null_rollback_leaves_no_phantom_wal_update_after_reopen() {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        setup_set_null(&db);

        db.execute("BEGIN").unwrap();
        db.execute("DELETE FROM p WHERE id = 1").unwrap();
        db.execute("ROLLBACK").unwrap();
    }

    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    assert_eq!(parents(&db), 1, "parent must survive the WAL replay");
    assert_eq!(child_fk(&db), Some(Value::Int4(1)), "no phantom Update");
}
