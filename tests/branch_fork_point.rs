//! sprinter 6bc4a4040eb7, phase B4 — a branch is a fork; dropping reclaims.
//!
//! Plan: `docs/plans/BRANCH_ISOLATION.md` (D5, D6). Spec:
//! `docs/plans/specs/B4_branch_fork_point.md`.
//!
//! A branch sees its parent as it was at its fork point: the `AS OF` anchor, or the
//! creation when there is none. What the parent does after that does not show through,
//! because the parent's write freezes what a live child sees into the child's overlay,
//! in the write gate. What a child only copied is no change of its own, so MERGE leaves
//! it. Dropping a branch, or a finished merge, deletes its keys.
//!
//! Every scenario that a client can spell two ways runs on BOTH DML executor families,
//! and ends by checking main. The races use handshakes, no sleeps: they either fail a
//! fixed interleaving or loop over a window.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{mpsc, Arc};

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::storage::branch_catalog::MergeFailSite;
use heliosdb_nano::storage::BranchState;
use heliosdb_nano::{
    Config, EmbeddedDatabase, Error, Tuple, Value, BRANCH_ANCHOR_UNREADABLE_MARKER, BRANCH_HAS_CHILD_MARKER,
    BRANCH_MERGE_SCHEMA_REFUSED_MARKER, BRANCH_STATEMENT_UNSUPPORTED_MARKER, BRANCH_WRITE_UNSUPPORTED_MARKER,
    TABLE_NAME_HOLDS_COLON_MARKER,
};

// ---------------------------------------------------------------------------
// The two executor families
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// `execute` / `query` with the parameters inlined as SQL literals.
    Text,
    /// `execute_params` / `query_params` with bound `$n`.
    Params,
}

const FAMILIES: [Family; 2] = [Family::Text, Family::Params];

fn literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Int4(v) => v.to_string(),
        Value::Int8(v) => v.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        other => panic!("no SQL literal for {other:?}"),
    }
}

fn inline(sql: &str, params: &[Value]) -> String {
    let mut out = sql.to_string();
    for (index, value) in params.iter().enumerate().rev() {
        out = out.replace(&format!("${}", index + 1), &literal(value));
    }
    out
}

fn exec(db: &EmbeddedDatabase, family: Family, sql: &str, params: &[Value]) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(&inline(sql, params)),
        Family::Params => db.execute_params(sql, params),
    }
}

fn ok(db: &EmbeddedDatabase, family: Family, sql: &str, params: &[Value]) -> u64 {
    exec(db, family, sql, params).unwrap_or_else(|e| panic!("[{family:?}] {sql} {params:?}: {e}"))
}

fn rows(db: &EmbeddedDatabase, family: Family, sql: &str) -> Vec<Tuple> {
    match family {
        Family::Text => db.query(sql, &[]),
        Family::Params => db.query_params(sql, &[]),
    }
    .unwrap_or_else(|e| panic!("[{family:?}] {sql}: {e}"))
}

fn on(db: &EmbeddedDatabase, branch: &str, family: Family, sql: &str, params: &[Value]) -> u64 {
    db.with_request_branch_scope(branch, || exec(db, family, sql, params))
        .unwrap_or_else(|e| panic!("[{family:?}] on {branch}: {sql} {params:?}: {e}"))
}

fn rows_on(db: &EmbeddedDatabase, branch: &str, family: Family, sql: &str) -> Vec<Tuple> {
    db.with_request_branch_scope(branch, || match family {
        Family::Text => db.query(sql, &[]),
        Family::Params => db.query_params(sql, &[]),
    })
    .unwrap_or_else(|e| panic!("[{family:?}] on {branch}: {sql}: {e}"))
}

fn int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Int2(v)) => i64::from(*v),
        Some(Value::Int4(v)) => i64::from(*v),
        Some(Value::Int8(v)) => *v,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        other => panic!("expected text, got {other:?}"),
    }
}

/// `(id, v)` rows, in id order.
fn pairs(found: &[Tuple]) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = found
        .iter()
        .map(|row| (int(row.values.first()), int(row.values.get(1))))
        .collect();
    out.sort_unstable();
    out
}

fn ids(found: &[Tuple]) -> Vec<i64> {
    let mut out: Vec<i64> = found.iter().map(|row| int(row.values.first())).collect();
    out.sort_unstable();
    out
}

fn i(v: i32) -> Value {
    Value::Int4(v)
}

// ---------------------------------------------------------------------------
// The raw keyspace
// ---------------------------------------------------------------------------

/// Every key under `prefix`, straight from RocksDB.
fn keys_with_prefix(db: &EmbeddedDatabase, prefix: &str) -> Vec<Vec<u8>> {
    let raw = db.storage.db();
    let mut read_opts = rocksdb::ReadOptions::default();
    read_opts.set_total_order_seek(true);
    let mut keys = Vec::new();
    for item in raw.iterator_opt(
        rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
        read_opts,
    ) {
        let (key, _) = item.unwrap();
        if !key.starts_with(prefix.as_bytes()) {
            break;
        }
        keys.push(key.to_vec());
    }
    keys
}

const LEVEL_FAMILIES: [&str; 6] = ["bdata:", "bdel:", "bfrz:", "bv:", "bmeta:", "bkey:"];

/// How many keys level `id` holds, in all of its families.
fn level_keys(db: &EmbeddedDatabase, id: u64) -> usize {
    LEVEL_FAMILIES
        .iter()
        .map(|family| keys_with_prefix(db, &format!("{family}{id}:")).len())
        .sum()
}

/// Every key of main's rows of `table`, with its stored value.
fn main_rows_state(db: &EmbeddedDatabase, table: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let prefix = format!("data:{table}:");
    let raw = db.storage.db();
    let mut read_opts = rocksdb::ReadOptions::default();
    read_opts.set_total_order_seek(true);
    let mut out = Vec::new();
    for item in raw.iterator_opt(
        rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
        read_opts,
    ) {
        let (key, value) = item.unwrap();
        if !key.starts_with(prefix.as_bytes()) {
            break;
        }
        out.push((key.to_vec(), value.to_vec()));
    }
    out
}

fn branch_id(db: &EmbeddedDatabase, name: &str) -> u64 {
    db.storage.get_branch(name).unwrap().branch_id
}

fn seed(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)").unwrap();
}

fn fork(db: &EmbeddedDatabase, name: &str) {
    db.execute(&format!("CREATE BRANCH {name} FROM main AS OF NOW"))
        .unwrap();
}

fn txn_session(db: &EmbeddedDatabase, name: &str) -> SessionId {
    db.create_session(name, IsolationLevel::ReadCommitted).unwrap()
}

// ---------------------------------------------------------------------------
// A branch does not see its parent's later writes
// ---------------------------------------------------------------------------

/// After the fork, main's INSERT, UPDATE and DELETE of rows the branch never touched do
/// not show on it, by any way of reading; what the branch does itself does; main sees its
/// own.
#[test]
fn mains_later_writes_do_not_show_on_a_branch() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        fork(&db, "r");

        ok(&db, family, "INSERT INTO t VALUES ($1, $2)", &[i(4), i(40)]);
        ok(&db, family, "UPDATE t SET v = $1 WHERE id = $2", &[i(11), i(1)]);
        ok(&db, family, "DELETE FROM t WHERE id = $1", &[i(2)]);

        let on_branch = rows_on(&db, "r", family, "SELECT id, v FROM t");
        assert_eq!(
            pairs(&on_branch),
            vec![(1, 10), (2, 20), (3, 30)],
            "[{family:?}] the scan"
        );
        for (id, v) in [(1, 10), (2, 20), (3, 30)] {
            let by_key = rows_on(&db, "r", family, &format!("SELECT id, v FROM t WHERE id = {id}"));
            assert_eq!(pairs(&by_key), vec![(id, v)], "[{family:?}] by key");
        }
        assert!(
            rows_on(&db, "r", family, "SELECT id FROM t WHERE id = 4").is_empty(),
            "[{family:?}] the new row, by key"
        );

        // The branch's own changes are still its own.
        on(&db, "r", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(33), i(3)]);
        on(&db, "r", family, "INSERT INTO t VALUES ($1, $2)", &[i(5), i(50)]);
        assert_eq!(
            pairs(&rows_on(&db, "r", family, "SELECT id, v FROM t")),
            vec![(1, 10), (2, 20), (3, 33), (5, 50)],
            "[{family:?}]"
        );
        // A key main took after the fork is free on the branch, and one it deleted
        // is still taken.
        on(&db, "r", family, "INSERT INTO t VALUES ($1, $2)", &[i(4), i(44)]);
        let err = db
            .with_request_branch_scope("r", || {
                exec(&db, family, "INSERT INTO t VALUES ($1, $2)", &[i(2), i(22)])
            })
            .expect_err("the branch still holds key 2");
        assert!(err.to_string().contains("Duplicate key"), "[{family:?}] {err}");

        assert_eq!(
            pairs(&rows(&db, family, "SELECT id, v FROM t")),
            vec![(1, 11), (3, 30), (4, 40)],
            "[{family:?}] main"
        );
    }
}

/// A row the branch changed itself is not overwritten by a later copy; a row main wrote
/// twice is frozen once, as it was at the fork.
#[test]
fn a_row_is_frozen_once_as_it_was_at_the_fork() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    fork(&db, "r");
    db.execute("UPDATE t SET v = 11 WHERE id = 1").unwrap();
    db.execute("UPDATE t SET v = 12 WHERE id = 1").unwrap();
    on(&db, "r", Family::Text, "UPDATE t SET v = 200 WHERE id = 2", &[]);
    db.execute("UPDATE t SET v = 21 WHERE id = 2").unwrap();
    assert_eq!(
        pairs(&rows_on(&db, "r", Family::Text, "SELECT id, v FROM t")),
        vec![(1, 10), (2, 200), (3, 30)]
    );
    let id = branch_id(&db, "r");
    assert_eq!(keys_with_prefix(&db, &format!("bfrz:{id}:t:")).len(), 1, "row 1 only");
}

/// A statement that writes a row twice in one batch: one copy, the pre-image.
#[test]
fn a_transaction_writing_a_row_twice_freezes_the_row_before_it() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    fork(&db, "r");
    let writer = txn_session(&db, "writer");
    db.begin_transaction_for_session(writer).unwrap();
    db.execute_for_session(writer, "UPDATE t SET v = 11 WHERE id = 1")
        .unwrap();
    db.execute_for_session(writer, "UPDATE t SET v = 12 WHERE id = 1")
        .unwrap();
    db.execute_for_session(writer, "DELETE FROM t WHERE id = 1").unwrap();
    db.execute_for_session(writer, "INSERT INTO t VALUES (9, 90)").unwrap();
    db.execute_for_session(writer, "UPDATE t SET v = 91 WHERE id = 9")
        .unwrap();
    db.commit_transaction_for_session(writer).unwrap();
    assert_eq!(
        pairs(&rows_on(&db, "r", Family::Text, "SELECT id, v FROM t")),
        vec![(1, 10), (2, 20), (3, 30)]
    );
    assert_eq!(
        pairs(&rows(&db, Family::Text, "SELECT id, v FROM t")),
        vec![(2, 20), (3, 30), (9, 91)]
    );
}

/// What the hook did for a transaction it did at its commit, reading the row then. A
/// transaction staged before the fork and committed after it: the state at the fork is
/// what the row still is, so that is the copy; and when another commit moved it after the
/// fork, the copy from that commit stays.
#[test]
fn a_transaction_staged_before_the_fork_and_committed_after_it() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        let writer = txn_session(&db, "writer");
        db.begin_transaction_for_session(writer).unwrap();
        db.execute_for_session(writer, "UPDATE t SET v = 11 WHERE id = 1")
            .unwrap();
        db.execute_for_session(writer, "DELETE FROM t WHERE id = 2").unwrap();
        db.execute_for_session(writer, "INSERT INTO t VALUES (4, 40)").unwrap();
        fork(&db, "r");
        // Another writer moves row 3 after the fork, while that one is open.
        ok(&db, family, "UPDATE t SET v = $1 WHERE id = $2", &[i(31), i(3)]);
        db.commit_transaction_for_session(writer).unwrap();
        db.destroy_session(writer).unwrap();

        assert_eq!(
            pairs(&rows_on(&db, "r", family, "SELECT id, v FROM t")),
            vec![(1, 10), (2, 20), (3, 30)],
            "[{family:?}] the commit is after the fork"
        );
        assert_eq!(
            pairs(&rows(&db, family, "SELECT id, v FROM t")),
            vec![(1, 11), (3, 31), (4, 40)],
            "[{family:?}] main"
        );
    }
}

/// A rollback writes nothing, so there is nothing to freeze.
#[test]
fn a_rolled_back_transaction_freezes_nothing() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    fork(&db, "r");
    let id = branch_id(&db, "r");
    let before = level_keys(&db, id);
    let writer = txn_session(&db, "writer");
    db.begin_transaction_for_session(writer).unwrap();
    db.execute_for_session(writer, "UPDATE t SET v = 11 WHERE id = 1")
        .unwrap();
    db.execute_for_session(writer, "DELETE FROM t WHERE id = 2").unwrap();
    db.rollback_transaction_for_session(writer).unwrap();
    assert_eq!(level_keys(&db, id), before);
    assert_eq!(
        pairs(&rows(&db, Family::Text, "SELECT id, v FROM t")),
        vec![(1, 10), (2, 20), (3, 30)]
    );
}

/// A table the child owns: main's batch on the name adds nothing to the child.
#[test]
fn a_batch_on_a_table_the_child_owns_freezes_nothing() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    fork(&db, "r");
    on(&db, "r", Family::Text, "ALTER TABLE t ADD COLUMN x INT DEFAULT 7", &[]);
    on(&db, "r", Family::Text, "CREATE TABLE own (id INT PRIMARY KEY)", &[]);
    let id = branch_id(&db, "r");
    let before = level_keys(&db, id);
    db.execute("INSERT INTO t VALUES (4, 40)").unwrap();
    db.execute("UPDATE t SET v = 11 WHERE id = 1").unwrap();
    db.execute("DELETE FROM t WHERE id = 2").unwrap();
    db.execute("CREATE TABLE own (id INT PRIMARY KEY, w INT)").unwrap();
    db.execute("INSERT INTO own VALUES (1, 1)").unwrap();
    assert_eq!(level_keys(&db, id), before, "the child reads neither from main");
    assert!(rows_on(&db, "r", Family::Text, "SELECT id FROM own").is_empty());
    assert_eq!(rows_on(&db, "r", Family::Text, "SELECT id FROM t").len(), 3);
}

// ---------------------------------------------------------------------------
// Merge: copies are no changes
// ---------------------------------------------------------------------------

/// A branch that changed nothing merges to nothing, whatever main did since.
#[test]
fn merging_a_branch_that_changed_nothing_changes_nothing() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        fork(&db, "r");
        ok(&db, family, "UPDATE t SET v = $1 WHERE id = $2", &[i(11), i(1)]);
        ok(&db, family, "DELETE FROM t WHERE id = $1", &[i(2)]);
        ok(&db, family, "INSERT INTO t VALUES ($1, $2)", &[i(4), i(40)]);
        assert!(level_keys(&db, branch_id(&db, "r")) > 0, "main's writes froze");
        let state = main_rows_state(&db, "t");
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(main_rows_state(&db, "t"), state, "[{family:?}]");
    }
}

/// The branch changed row A, main changed row B: main keeps its B, and takes the
/// branch's A. A row both changed is last-writer-wins, the branch being last.
#[test]
fn merging_keeps_what_main_changed_and_the_branch_did_not() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        fork(&db, "r");
        on(&db, "r", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(100), i(1)]);
        on(&db, "r", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(300), i(3)]);
        ok(&db, family, "UPDATE t SET v = $1 WHERE id = $2", &[i(21), i(2)]);
        ok(&db, family, "UPDATE t SET v = $1 WHERE id = $2", &[i(31), i(3)]);
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(
            pairs(&rows(&db, family, "SELECT id, v FROM t")),
            vec![(1, 100), (2, 21), (3, 300)],
            "[{family:?}]"
        );
    }
}

/// A table main created after the fork is not visible to the branch, so it cannot have
/// changed its rows, and the merge has nothing to refuse (it replaces B3's refusal).
#[test]
fn a_table_created_on_the_target_after_the_fork_is_invisible_and_merges() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        fork(&db, "r");
        db.execute("CREATE TABLE later (id INT PRIMARY KEY)").unwrap();
        ok(&db, family, "INSERT INTO later VALUES ($1)", &[i(1)]);
        db.with_request_branch_scope("r", || exec(&db, family, "INSERT INTO later VALUES ($1)", &[i(2)]))
            .expect_err("no such table on the branch");
        assert!(db
            .with_request_branch_scope("r", || db.query("SELECT id FROM later", &[]))
            .is_err());
        on(&db, "r", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(100), i(1)]);
        db.execute("MERGE BRANCH r INTO main").unwrap();
        assert_eq!(ids(&rows(&db, family, "SELECT id FROM later")), vec![1], "[{family:?}]");
        assert_eq!(
            pairs(&rows(&db, family, "SELECT id, v FROM t")),
            vec![(1, 100), (2, 20), (3, 30)],
            "[{family:?}]"
        );
    }
}

/// A sibling keeps its view while a merge lands on main, row and schema.
#[test]
fn a_merge_into_main_does_not_move_a_sibling() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    fork(&db, "r");
    fork(&db, "sibling");
    on(&db, "r", Family::Text, "UPDATE t SET v = 100 WHERE id = 1", &[]);
    on(&db, "r", Family::Text, "DELETE FROM t WHERE id = 2", &[]);
    on(&db, "r", Family::Text, "ALTER TABLE t ADD COLUMN x INT DEFAULT 7", &[]);
    db.execute("MERGE BRANCH r INTO main").unwrap();
    assert_eq!(
        rows(&db, Family::Text, "SELECT id, v, x FROM t").len(),
        2,
        "main took it"
    );
    assert_eq!(
        pairs(&rows_on(&db, "sibling", Family::Text, "SELECT id, v FROM t")),
        vec![(1, 10), (2, 20), (3, 30)]
    );
    assert!(db
        .with_request_branch_scope("sibling", || db.query("SELECT x FROM t", &[]))
        .is_err());
}

// ---------------------------------------------------------------------------
// The parent's DDL
// ---------------------------------------------------------------------------

/// Main's ADD COLUMN, DROP TABLE, CREATE TABLE and RENAME after the fork: the branch
/// keeps the old definition and rows, still sees the dropped table, does not see the new
/// one, and writes as it always could.
#[test]
fn mains_ddl_after_the_fork_does_not_show_on_a_branch() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        db.execute("CREATE TABLE gone (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO gone VALUES (1, 'a'), (2, 'b')").unwrap();
        db.execute("CREATE TABLE moved (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO moved VALUES (1)").unwrap();
        fork(&db, "r");

        db.execute("ALTER TABLE t ADD COLUMN x INT DEFAULT 42").unwrap();
        db.execute("DROP TABLE gone").unwrap();
        db.execute("CREATE TABLE fresh (id INT PRIMARY KEY)").unwrap();
        db.execute("ALTER TABLE moved RENAME TO renamed").unwrap();

        let columns = rows_on(&db, "r", family, "SELECT * FROM t");
        assert!(
            columns.iter().all(|row| row.values.len() == 2),
            "[{family:?}] {columns:?}"
        );
        assert_eq!(pairs(&columns), vec![(1, 10), (2, 20), (3, 30)], "[{family:?}]");
        assert!(db
            .with_request_branch_scope("r", || db.query("SELECT x FROM t", &[]))
            .is_err());
        assert_eq!(
            rows_on(&db, "r", family, "SELECT id FROM gone").len(),
            2,
            "[{family:?}]"
        );
        assert_eq!(
            rows_on(&db, "r", family, "SELECT id FROM moved").len(),
            1,
            "[{family:?}]"
        );
        for missing in ["fresh", "renamed"] {
            assert!(
                db.with_request_branch_scope("r", || db.query(&format!("SELECT * FROM {missing}"), &[]))
                    .is_err(),
                "[{family:?}] {missing}"
            );
        }

        // It can write them: its row ids do not collide with what it holds, though
        // main's counter of the dropped table is gone.
        on(&db, "r", family, "INSERT INTO gone VALUES ($1, 'c')", &[i(3)]);
        on(&db, "r", family, "INSERT INTO t VALUES ($1, $2)", &[i(5), i(50)]);
        assert_eq!(
            rows_on(&db, "r", family, "SELECT id FROM gone").len(),
            3,
            "[{family:?}]"
        );
        assert_eq!(
            rows(&db, family, "SELECT id, v, x FROM t").len(),
            3,
            "[{family:?}] main"
        );

        // The copies are no changes, and the branch's rows were written for the old
        // definition: MERGE says so, naming what moved.
        let err = db.execute("MERGE BRANCH r INTO main").unwrap_err().to_string();
        assert!(err.contains(BRANCH_MERGE_SCHEMA_REFUSED_MARKER), "[{family:?}] {err}");
    }
}

/// Main drops and creates a table of the name again, and writes it: the branch keeps the
/// old table's rows, its row ids colliding with the new ones' or not.
#[test]
fn main_recreating_a_table_does_not_reach_the_branchs_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    fork(&db, "r");
    db.execute("DROP TABLE t").unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO t VALUES (7, 70), (8, 80), (9, 90), (10, 100)")
        .unwrap();
    assert_eq!(
        pairs(&rows_on(&db, "r", Family::Text, "SELECT id, v FROM t")),
        vec![(1, 10), (2, 20), (3, 30)]
    );
    assert_eq!(rows(&db, Family::Text, "SELECT id FROM t").len(), 4);
}

/// `TRUNCATE` on main, and a key index on main that no longer holds the branch's rows.
#[test]
fn mains_truncate_does_not_show_on_a_branch() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        fork(&db, "r");
        ok(&db, family, "TRUNCATE TABLE t", &[]);
        assert_eq!(rows(&db, family, "SELECT id FROM t").len(), 0, "[{family:?}]");
        assert_eq!(rows_on(&db, "r", family, "SELECT id FROM t").len(), 3, "[{family:?}]");
        assert_eq!(
            pairs(&rows_on(&db, "r", family, "SELECT id, v FROM t WHERE id = 2")),
            vec![(2, 20)],
            "[{family:?}]"
        );
    }
}

/// A column main keeps out of its row (`STORAGE DICTIONARY`, `COLUMNAR`): the copy holds
/// the value, and it is the one before the write. A columnar slot is rewritten before
/// the row that refers to it.
#[test]
fn a_frozen_row_of_a_side_storage_table_holds_its_values() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute(
        "CREATE TABLE s (id INT PRIMARY KEY, c INT8 STORAGE COLUMNAR, d TEXT STORAGE DICTIONARY, x TEXT STORAGE \
         CONTENT_ADDRESSED)",
    )
    .unwrap();
    let long = "x".repeat(2048);
    db.execute(&format!(
        "INSERT INTO s VALUES (1, 100, 'one', '{long}'), (2, 200, 'two', 'short')"
    ))
    .unwrap();
    fork(&db, "r");
    let read = |found: &[Tuple]| -> Vec<(i64, i64, Option<String>)> {
        let mut out: Vec<_> = found
            .iter()
            .map(|row| (int(row.values.first()), int(row.values.get(1)), text(row.values.get(2))))
            .collect();
        out.sort();
        out
    };
    db.execute("UPDATE s SET c = 101, d = 'uno' WHERE id = 1").unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE s SET c = 201, d = 'dos' WHERE id = 2").unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(
        read(&rows_on(&db, "r", Family::Text, "SELECT id, c, d FROM s")),
        vec![(1, 100, Some("one".to_string())), (2, 200, Some("two".to_string()))]
    );
    let x = rows_on(&db, "r", Family::Text, "SELECT x FROM s WHERE id = 1");
    assert_eq!(text(x[0].values.first()).as_deref(), Some(long.as_str()));
    db.execute("DELETE FROM s WHERE id = 1").unwrap();
    assert_eq!(rows_on(&db, "r", Family::Text, "SELECT id FROM s").len(), 2);
    assert_eq!(
        read(&rows(&db, Family::Text, "SELECT id, c, d FROM s")),
        vec![(2, 201, Some("dos".to_string()))]
    );
}

// ---------------------------------------------------------------------------
// Chains
// ---------------------------------------------------------------------------

/// `b`'s changes after `c` forked from it do not show on `c`, and main's on neither.
#[test]
fn a_branch_of_a_branch_is_a_fork_of_it() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        fork(&db, "b");
        on(&db, "b", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(100), i(1)]);
        db.execute("CREATE BRANCH c FROM b AS OF NOW").unwrap();
        on(&db, "b", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(101), i(1)]);
        on(&db, "b", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(200), i(2)]);
        on(&db, "b", family, "DELETE FROM t WHERE id = $1", &[i(3)]);
        on(&db, "b", family, "INSERT INTO t VALUES ($1, $2)", &[i(4), i(400)]);
        ok(&db, family, "UPDATE t SET v = $1 WHERE id = $2", &[i(21), i(2)]);
        ok(&db, family, "INSERT INTO t VALUES ($1, $2)", &[i(5), i(50)]);

        assert_eq!(
            pairs(&rows_on(&db, "c", family, "SELECT id, v FROM t")),
            vec![(1, 100), (2, 20), (3, 30)],
            "[{family:?}] c"
        );
        assert_eq!(
            pairs(&rows_on(&db, "b", family, "SELECT id, v FROM t")),
            vec![(1, 101), (2, 200), (4, 400)],
            "[{family:?}] b"
        );
        assert_eq!(
            pairs(&rows(&db, family, "SELECT id, v FROM t")),
            vec![(1, 10), (2, 21), (3, 30), (5, 50)],
            "[{family:?}] main"
        );
    }
}

/// The two parents of a chain write one row: whichever comes first, `c` keeps the row as
/// it was at its fork, and `b` keeps its own.
#[test]
fn two_parents_writing_one_row_of_a_chain() {
    for main_first in [true, false] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        fork(&db, "b");
        db.execute("CREATE BRANCH c FROM b AS OF NOW").unwrap();
        if main_first {
            db.execute("UPDATE t SET v = 11 WHERE id = 1").unwrap();
            on(&db, "b", Family::Text, "UPDATE t SET v = 100 WHERE id = 1", &[]);
        } else {
            on(&db, "b", Family::Text, "UPDATE t SET v = 100 WHERE id = 1", &[]);
            db.execute("UPDATE t SET v = 11 WHERE id = 1").unwrap();
        }
        assert_eq!(
            pairs(&rows_on(&db, "c", Family::Text, "SELECT id, v FROM t WHERE id = 1")),
            vec![(1, 10)],
            "main first: {main_first}"
        );
        assert_eq!(
            pairs(&rows_on(&db, "b", Family::Text, "SELECT id, v FROM t WHERE id = 1")),
            vec![(1, 100)],
            "main first: {main_first}"
        );
        assert_eq!(
            pairs(&rows(&db, Family::Text, "SELECT id, v FROM t WHERE id = 1")),
            vec![(1, 11)],
            "main first: {main_first}"
        );
    }
}

/// DDL on a branch with live descendants: lifted. Each keeps what it saw, and they are
/// not told about it in any way.
#[test]
fn ddl_on_a_branch_with_a_live_child_leaves_the_child_alone() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        fork(&db, "b");
        on(
            &db,
            "b",
            family,
            "CREATE TABLE own (id INT PRIMARY KEY, code TEXT UNIQUE)",
            &[],
        );
        on(&db, "b", family, "INSERT INTO own VALUES ($1, 'a')", &[i(1)]);
        on(&db, "b", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(100), i(1)]);
        db.execute("CREATE BRANCH c FROM b AS OF NOW").unwrap();
        on(&db, "c", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(222), i(2)]);

        on(&db, "b", family, "ALTER TABLE t ADD COLUMN x INT DEFAULT 7", &[]);
        on(&db, "b", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(333), i(3)]);
        on(&db, "b", family, "DROP TABLE own", &[]);
        on(&db, "b", family, "CREATE TABLE fresh (id INT PRIMARY KEY)", &[]);

        let on_child = rows_on(&db, "c", family, "SELECT * FROM t");
        assert!(
            on_child.iter().all(|row| row.values.len() == 2),
            "[{family:?}] {on_child:?}"
        );
        assert_eq!(
            pairs(&on_child),
            vec![(1, 100), (2, 222), (3, 30)],
            "[{family:?}] what c saw, and its own"
        );
        assert_eq!(
            ids(&rows_on(&db, "c", family, "SELECT id FROM own")),
            vec![1],
            "[{family:?}]"
        );
        assert!(db
            .with_request_branch_scope("c", || db.query("SELECT id FROM fresh", &[]))
            .is_err());
        // Its constraints are the ones it saw, over the rows it holds now.
        let dup = db
            .with_request_branch_scope("c", || exec(&db, family, "INSERT INTO own VALUES ($1, 'a')", &[i(2)]))
            .expect_err("code 'a' is held on c");
        assert!(dup.to_string().contains("Duplicate key"), "[{family:?}] {dup}");
        on(&db, "c", family, "INSERT INTO own VALUES ($1, 'z')", &[i(2)]);

        assert_eq!(
            rows_on(&db, "b", family, "SELECT id, v, x FROM t").len(),
            3,
            "[{family:?}]"
        );
        assert!(db
            .with_request_branch_scope("b", || db.query("SELECT id FROM own", &[]))
            .is_err());
        assert_eq!(rows(&db, family, "SELECT id FROM t").len(), 3, "[{family:?}] main");
    }
}

/// The child of an isolated table of its parent keeps a key the parent moved: the copy
/// carries its key-index entry, or a probe would miss it.
#[test]
fn a_frozen_row_of_an_isolated_table_keeps_its_key() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        fork(&db, "b");
        on(
            &db,
            "b",
            family,
            "CREATE TABLE own (id INT PRIMARY KEY, code INT UNIQUE)",
            &[],
        );
        on(&db, "b", family, "INSERT INTO own VALUES ($1, $2)", &[i(1), i(10)]);
        on(&db, "b", family, "INSERT INTO own VALUES ($1, $2)", &[i(2), i(20)]);
        db.execute("CREATE BRANCH c FROM b AS OF NOW").unwrap();
        on(
            &db,
            "b",
            family,
            "UPDATE own SET code = $1 WHERE id = $2",
            &[i(11), i(1)],
        );
        on(&db, "b", family, "DELETE FROM own WHERE id = $1", &[i(2)]);
        let c = branch_id(&db, "c");
        assert_eq!(
            keys_with_prefix(&db, &format!("bfrz:{c}:own:")).len(),
            2,
            "[{family:?}]"
        );
        for held in [10, 20] {
            let dup = db
                .with_request_branch_scope("c", || {
                    exec(&db, family, "INSERT INTO own VALUES ($1, $2)", &[i(9), i(held)])
                })
                .expect_err("c holds the key");
            assert!(dup.to_string().contains("Duplicate key"), "[{family:?}] {dup}");
        }
        on(&db, "c", family, "INSERT INTO own VALUES ($1, $2)", &[i(9), i(11)]);
        assert_eq!(
            pairs(&rows_on(&db, "c", family, "SELECT id, code FROM own")),
            vec![(1, 10), (2, 20), (9, 11)],
            "[{family:?}]"
        );
    }
}

/// b -> c -> d, c merged into b: d still reads through c. b's rows and DDL leave it
/// alone, c's keys stay until d is gone, and then they go.
#[test]
fn a_merged_level_between_a_parent_and_its_descendant() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        fork(&db, "b");
        db.execute("CREATE BRANCH c FROM b AS OF NOW").unwrap();
        on(&db, "c", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(100), i(1)]);
        db.execute("CREATE BRANCH d FROM c AS OF NOW").unwrap();
        db.execute("MERGE BRANCH c INTO b").unwrap();
        assert!(matches!(
            db.storage.get_branch("c").unwrap().state,
            BranchState::Merged { .. }
        ));
        let c = branch_id(&db, "c");
        let held = level_keys(&db, c);
        assert!(held > 0, "[{family:?}] d reads through it");

        // A row of b: frozen into d, through the merged level.
        on(&db, "b", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(21), i(2)]);
        // A row c changed: d reads c's, and b's write of it changes nothing there.
        on(&db, "b", family, "UPDATE t SET v = $1 WHERE id = $2", &[i(11), i(1)]);
        ok(&db, family, "UPDATE t SET v = $1 WHERE id = $2", &[i(31), i(3)]);
        assert_eq!(
            pairs(&rows_on(&db, "d", family, "SELECT id, v FROM t")),
            vec![(1, 100), (2, 20), (3, 30)],
            "[{family:?}] rows"
        );

        // The DDL refusal is lifted through the merged level too.
        on(&db, "b", family, "ALTER TABLE t ADD COLUMN x INT DEFAULT 7", &[]);
        let on_d = rows_on(&db, "d", family, "SELECT * FROM t");
        assert!(on_d.iter().all(|row| row.values.len() == 2), "[{family:?}] {on_d:?}");
        assert_eq!(pairs(&on_d), vec![(1, 100), (2, 20), (3, 30)], "[{family:?}] DDL");
        assert_eq!(
            level_keys(&db, c),
            held,
            "[{family:?}] nothing goes into a merged level"
        );

        db.execute("DROP BRANCH d").unwrap();
        assert_eq!(level_keys(&db, c), 0, "[{family:?}] its last reader is gone");
        assert_eq!(
            rows_on(&db, "b", family, "SELECT id, v, x FROM t").len(),
            3,
            "[{family:?}]"
        );
    }
}

/// A schema-changing merge into a branch with another live child: lifted; the other
/// child keeps what it saw.
#[test]
fn a_schema_changing_merge_into_a_branch_with_another_child() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    fork(&db, "b");
    db.execute("CREATE BRANCH c1 FROM b AS OF NOW").unwrap();
    db.execute("CREATE BRANCH c2 FROM b AS OF NOW").unwrap();
    on(&db, "c1", Family::Text, "ALTER TABLE t ADD COLUMN x INT DEFAULT 7", &[]);
    on(&db, "c1", Family::Text, "UPDATE t SET v = 100 WHERE id = 1", &[]);
    db.execute("MERGE BRANCH c1 INTO b").unwrap();
    assert_eq!(rows_on(&db, "b", Family::Text, "SELECT id, v, x FROM t").len(), 3);
    assert_eq!(
        pairs(&rows_on(&db, "b", Family::Text, "SELECT id, v FROM t WHERE id = 1")),
        vec![(1, 100)]
    );
    let on_sibling = rows_on(&db, "c2", Family::Text, "SELECT * FROM t");
    assert!(on_sibling.iter().all(|row| row.values.len() == 2), "{on_sibling:?}");
    assert_eq!(pairs(&on_sibling), vec![(1, 10), (2, 20), (3, 30)]);
}

// ---------------------------------------------------------------------------
// AS OF
// ---------------------------------------------------------------------------

fn last_scn(db: &EmbeddedDatabase) -> u64 {
    db.storage.snapshot_manager().current_scn().saturating_sub(1)
}

/// Writes after the anchor, by every route: `(1, 10)` updated, `(2, 20)` deleted, `(4,
/// 40)` inserted; `(3, 30)` left alone. In a transaction, which versions its updates.
fn move_main_after_an_anchor(db: &EmbeddedDatabase, family: Family) {
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE t SET v = 11 WHERE id = 1").unwrap();
    db.execute("COMMIT").unwrap();
    ok(db, family, "DELETE FROM t WHERE id = $1", &[i(2)]);
    ok(db, family, "INSERT INTO t VALUES ($1, $2)", &[i(4), i(40)]);
}

/// A branch anchored in the past sees the rows inserted, updated and deleted after the
/// anchor as they were then; main's later writes do not show either.
#[test]
fn a_branch_anchored_in_the_past_sees_main_as_it_was() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        let anchor = last_scn(&db);
        move_main_after_an_anchor(&db, family);
        db.execute(&format!("CREATE BRANCH rewind FROM main AS OF SCN {anchor}"))
            .unwrap();
        assert_eq!(
            pairs(&rows_on(&db, "rewind", family, "SELECT id, v FROM t")),
            vec![(1, 10), (2, 20), (3, 30)],
            "[{family:?}]"
        );
        ok(&db, family, "DELETE FROM t WHERE id = $1", &[i(3)]);
        ok(&db, family, "INSERT INTO t VALUES ($1, $2)", &[i(5), i(50)]);
        assert_eq!(
            pairs(&rows_on(&db, "rewind", family, "SELECT id, v FROM t")),
            vec![(1, 10), (2, 20), (3, 30)],
            "[{family:?}] later too"
        );
        // Nothing it copied is a change.
        let state = main_rows_state(&db, "t");
        db.execute("MERGE BRANCH rewind INTO main").unwrap();
        assert_eq!(main_rows_state(&db, "t"), state, "[{family:?}]");
    }
}

/// With time travel off there is no history to anchor at: refused, and nothing happened.
#[test]
fn an_anchor_is_refused_with_time_travel_off() {
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = false;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    seed(&db);
    let refused = db
        .storage
        .create_branch_at_snapshot("r", Some("main"), Some(1), Default::default());
    assert!(refused.is_err(), "{refused:?}");
    assert!(db.storage.get_branch("r").is_err());
    let refused = db.execute("CREATE BRANCH r FROM main AS OF TIMESTAMP '2999-01-01 00:00:00'");
    assert!(refused.is_err(), "{refused:?}");
    assert!(db.storage.get_branch("r").is_err());
    // AS OF NOW needs no history.
    fork(&db, "r");
    db.execute("UPDATE t SET v = 11 WHERE id = 1").unwrap();
    assert_eq!(
        pairs(&rows_on(&db, "r", Family::Text, "SELECT id, v FROM t WHERE id = 1")),
        vec![(1, 10)]
    );
}

/// A branch keeps no history, so a branch of it cannot be anchored in the past.
#[test]
fn an_anchor_needs_main_as_its_parent() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    fork(&db, "b");
    let anchor = last_scn(&db);
    let err = db
        .execute(&format!("CREATE BRANCH c FROM b AS OF SCN {anchor}"))
        .unwrap_err()
        .to_string();
    assert!(err.contains(BRANCH_WRITE_UNSUPPORTED_MARKER), "{err}");
    assert!(db.storage.get_branch("c").is_err());
}

/// Stopped half way through its anchor: no such branch, its keys go, and the name is free.
#[test]
fn a_materialization_that_fails_leaves_no_branch() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    let anchor = last_scn(&db);
    move_main_after_an_anchor(&db, Family::Text);
    db.storage.debug_set_materialize_chunk_rows(1);
    db.storage.debug_fail_merge_at(MergeFailSite::Materialize, 0);
    let refused = db.execute(&format!("CREATE BRANCH rewind FROM main AS OF SCN {anchor}"));
    db.storage.debug_set_materialize_chunk_rows(0);
    db.storage.debug_clear_merge_failpoint();
    assert!(refused.is_err());
    assert!(db.storage.get_branch("rewind").is_err());
    assert!(
        db.with_request_branch_scope("rewind", || db.query("SELECT id FROM t", &[]))
            .is_err(),
        "nothing to bind"
    );
    for family in LEVEL_FAMILIES {
        assert!(
            keys_with_prefix(&db, family).is_empty(),
            "{family}: a key was left behind"
        );
    }
    db.execute(&format!("CREATE BRANCH rewind FROM main AS OF SCN {anchor}"))
        .unwrap();
    assert_eq!(
        pairs(&rows_on(&db, "rewind", Family::Text, "SELECT id, v FROM t")),
        vec![(1, 10), (2, 20), (3, 30)]
    );
}

// ---------------------------------------------------------------------------
// AS OF on main: delete events
// ---------------------------------------------------------------------------

/// A row deleted after the time an `AS OF` asks about is there, by every way a row is
/// deleted; and not at any time at or after its delete, nor before it existed.
#[test]
fn as_of_returns_a_row_deleted_after_it() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let before_any = last_scn(&db);
        db.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30), (4, 40)")
            .unwrap();
        let at = |scn: u64| -> Vec<(i64, i64)> {
            pairs(&rows(
                &db,
                Family::Text,
                &format!("SELECT id, v FROM t AS OF SCN {scn}"),
            ))
        };
        let inserted = last_scn(&db);

        // The fast path by primary key.
        ok(&db, family, "DELETE FROM t WHERE id = $1", &[i(1)]);
        let first = last_scn(&db);
        // A transaction's commit.
        db.execute("BEGIN").unwrap();
        db.execute("DELETE FROM t WHERE id = 2").unwrap();
        db.execute("COMMIT").unwrap();
        let second = last_scn(&db);
        // The planner's path, which is not by key.
        ok(&db, family, "DELETE FROM t WHERE v = $1", &[i(30)]);
        let third = last_scn(&db);

        assert_eq!(at(inserted), vec![(1, 10), (2, 20), (3, 30), (4, 40)], "[{family:?}]");
        assert_eq!(at(first), vec![(2, 20), (3, 30), (4, 40)], "[{family:?}]");
        assert_eq!(at(second), vec![(3, 30), (4, 40)], "[{family:?}]");
        assert_eq!(at(third), vec![(4, 40)], "[{family:?}]");
        assert_eq!(
            pairs(&rows(&db, family, "SELECT id, v FROM t")),
            vec![(4, 40)],
            "[{family:?}]"
        );
        // Before the rows existed: there is a version index, and nothing in it yet.
        if before_any > 0 {
            assert!(at(before_any).is_empty(), "[{family:?}]");
        }
    }
}

/// `TRUNCATE` is a delete of every row, at one time.
#[test]
fn as_of_before_a_truncate_returns_the_rows() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        let before = last_scn(&db);
        ok(&db, family, "TRUNCATE TABLE t", &[]);
        let after = last_scn(&db);
        assert_eq!(
            pairs(&rows(
                &db,
                Family::Text,
                &format!("SELECT id, v FROM t AS OF SCN {before}")
            )),
            vec![(1, 10), (2, 20), (3, 30)],
            "[{family:?}]"
        );
        assert!(
            rows(&db, Family::Text, &format!("SELECT id, v FROM t AS OF SCN {after}")).is_empty(),
            "[{family:?}]"
        );
    }
}

/// The delete events change `AS OF`, and nothing else an ordinary user reads: a transaction
/// that holds an older snapshot scans the rows that exist now, as it always did, and its
/// writes are built on that.
#[test]
fn a_transactions_snapshot_scan_reads_what_it_always_read() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    let reader = db.create_session("reader", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(reader).unwrap();
    assert_eq!(db.query_in_session(reader, "SELECT id FROM t", &[]).unwrap().len(), 3);
    db.execute("DELETE FROM t WHERE id = 1").unwrap();
    assert_eq!(db.query_in_session(reader, "SELECT id FROM t", &[]).unwrap().len(), 2);
    db.rollback_transaction_for_session(reader).unwrap();
    db.destroy_session(reader).unwrap();
    assert_eq!(rows(&db, Family::Text, "SELECT id FROM t").len(), 2);
}

/// A row in a table's history that has no delete event was deleted before there were any:
/// absent at every time, as it always was. Simulated by taking the event out.
#[test]
fn a_row_deleted_before_delete_events_stays_absent() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    let anchor = last_scn(&db);
    db.execute("DELETE FROM t WHERE id = 1").unwrap();
    let raw = db.storage.db();
    let mut events = 0;
    for key in keys_with_prefix(&db, "v_idx:t:") {
        if raw.get(&key).unwrap().is_some_and(|value| value.len() == 9) {
            raw.delete(&key).unwrap();
            events += 1;
        }
    }
    assert_eq!(events, 1, "the delete wrote one event");
    assert_eq!(
        pairs(&rows(
            &db,
            Family::Text,
            &format!("SELECT id, v FROM t AS OF SCN {anchor}")
        )),
        vec![(2, 20), (3, 30)]
    );
    // And a branch anchored there does not bring it back.
    db.execute(&format!("CREATE BRANCH rewind FROM main AS OF SCN {anchor}"))
        .unwrap();
    assert_eq!(
        pairs(&rows_on(&db, "rewind", Family::Text, "SELECT id, v FROM t")),
        vec![(2, 20), (3, 30)]
    );
}

/// `AS OF` before a merge's deletion shows the row, through the merge's commit. And a table
/// of the name created later does not inherit the events.
#[test]
fn as_of_after_a_merge_and_after_a_drop() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    fork(&db, "r");
    on(&db, "r", Family::Text, "DELETE FROM t WHERE id = 2", &[]);
    let before = last_scn(&db);
    db.execute("MERGE BRANCH r INTO main").unwrap();
    let after = last_scn(&db);
    let at = |scn: u64| {
        pairs(&rows(
            &db,
            Family::Text,
            &format!("SELECT id, v FROM t AS OF SCN {scn}"),
        ))
    };
    assert_eq!(at(before), vec![(1, 10), (2, 20), (3, 30)]);
    assert_eq!(at(after), vec![(1, 10), (3, 30)]);

    // A table of the name, created later, gets none of the old one's rows back, though
    // its row ids start over: the drop took the history.
    db.execute("DROP TABLE t").unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO t VALUES (9, 90)").unwrap();
    assert!(at(before).iter().all(|(id, _)| *id == 9), "{:?}", at(before));
}

/// A branch session has no history to answer `AS OF` from, and main's would show what
/// main did after the fork.
#[test]
fn as_of_on_a_branch_is_refused() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    let anchor = last_scn(&db);
    fork(&db, "r");
    let err = db
        .with_request_branch_scope("r", || db.query(&format!("SELECT id FROM t AS OF SCN {anchor}"), &[]))
        .expect_err("refused")
        .to_string();
    // 0A000 on both wires, like the marker below.
    assert!(err.contains(BRANCH_STATEMENT_UNSUPPORTED_MARKER), "{err}");
}

// ---------------------------------------------------------------------------
// COUNT(*) AS OF: the row counter knows now only
// ---------------------------------------------------------------------------

/// `COUNT(*) … AS OF` is the rows as of then, on both families, and is refused on a
/// branch like every other `AS OF`. The row counter's fast path used to answer both with
/// the count of now.
#[test]
fn count_star_as_of_counts_the_rows_as_of() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        let anchor = last_scn(&db);
        ok(&db, family, "DELETE FROM t WHERE id = $1", &[i(1)]);
        ok(&db, family, "INSERT INTO t VALUES ($1, $2)", &[i(4), i(40)]);
        ok(&db, family, "INSERT INTO t VALUES ($1, $2)", &[i(5), i(50)]);
        let count = |sql: &str| int(rows(&db, family, sql)[0].values.first());
        assert_eq!(
            count(&format!("SELECT COUNT(*) FROM t AS OF SCN {anchor}")),
            3,
            "[{family:?}]"
        );
        assert_eq!(
            count(&format!("SELECT COUNT(1) FROM t AS OF SCN {anchor}")),
            3,
            "[{family:?}]"
        );
        assert_eq!(count("SELECT COUNT(*) FROM t"), 4, "[{family:?}] now");
        fork(&db, "r");
        for sql in [
            format!("SELECT COUNT(*) FROM t AS OF SCN {anchor}"),
            format!("SELECT COUNT(1) FROM t AS OF SCN {anchor}"),
            format!("SELECT MAX(id) FROM t AS OF SCN {anchor}"),
        ] {
            let err = db
                .with_request_branch_scope("r", || match family {
                    Family::Text => db.query(&sql, &[]),
                    Family::Params => db.query_params(&sql, &[]),
                })
                .expect_err("refused on a branch")
                .to_string();
            assert!(
                err.contains(BRANCH_STATEMENT_UNSUPPORTED_MARKER),
                "[{family:?}] {sql}: {err}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// A schema statement on main is wholly before a fork, or wholly after it
// ---------------------------------------------------------------------------

const REWRITTEN_ROWS: i64 = 4_000;

fn load_rewritten(db: &EmbeddedDatabase, table: &str) {
    db.execute(&format!("CREATE TABLE {table} (id INT8 PRIMARY KEY, a INT8, b INT8)"))
        .unwrap();
    let mut id = 1;
    while id <= REWRITTEN_ROWS {
        let last = (id + 999).min(REWRITTEN_ROWS);
        let values: Vec<String> = (id..=last)
            .map(|row| format!("({row}, {}, {})", 100_000 + row, 200_000 + row))
            .collect();
        db.execute(&format!("INSERT INTO {table} VALUES {}", values.join(",")))
            .unwrap();
        id = last + 1;
    }
}

fn columns_on(db: &EmbeddedDatabase, branch: &str, table: &str) -> Vec<String> {
    rows_on(
        db,
        branch,
        Family::Text,
        "SELECT table_name, column_name FROM information_schema.columns",
    )
    .iter()
    .filter(|row| text(row.values.first()).as_deref() == Some(table))
    .filter_map(|row| text(row.values.get(1)))
    .collect()
}

/// Each row the branch holds against the definition it holds: `a` is `100000 + id`, `b`
/// is `200000 + id`, `c` is 7. A row of one side of the statement under the definition
/// of the other is off in some column.
fn assert_rows_match_definition(db: &EmbeddedDatabase, branch: &str, table: &str, context: &str) {
    let columns = columns_on(db, branch, table);
    assert!(!columns.is_empty(), "{context}: {branch} has no {table}");
    let found = rows_on(db, branch, Family::Text, &format!("SELECT * FROM {table}"));
    assert_eq!(found.len() as i64, REWRITTEN_ROWS, "{context}");
    for row in &found {
        assert_eq!(row.values.len(), columns.len(), "{context}: {columns:?} {row:?}");
        let id = int(row.values.first());
        for (name, value) in columns.iter().zip(row.values.iter()) {
            let expected = match name.as_str() {
                "a" => Some(100_000 + id),
                "b" => Some(200_000 + id),
                "c" => Some(7),
                _ => None,
            };
            if let Some(expected) = expected {
                assert_eq!(
                    int(Some(value)),
                    expected,
                    "{context}: {name} of row {id}, under {columns:?}"
                );
            }
        }
    }
}

/// The review's F1: `CREATE BRANCH` overlapping one rewriting `ALTER`. The hook copied
/// the old rows the statement had not reached while the child took the definition from
/// after it. Started by a handshake: the `ALTER` has changed the catalog and is in its
/// rows when `CREATE BRANCH` starts. Or both at once, the other way round.
#[test]
fn a_rewriting_alter_is_wholly_before_or_after_a_fork() {
    // Spec R5.2: a stable default rewrites no row, so the "add" case takes a volatile
    // one (always 7) to keep a rewrite racing the fork.
    for (kind, alter) in [
        ("drop", "DROP COLUMN a"),
        ("add", "ADD COLUMN c INT8 DEFAULT (CASE WHEN random() >= 0 THEN 7 END)"),
    ] {
        for trial in 0..4 {
            let handshake = trial % 2 == 0;
            let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
            let table = format!("r_{kind}_{trial}");
            load_rewritten(&db, &table);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let running = {
                let (db, table, barrier) = (Arc::clone(&db), table.clone(), Arc::clone(&barrier));
                let sql = format!("ALTER TABLE {table} {alter}");
                std::thread::spawn(move || {
                    barrier.wait();
                    db.execute(&sql).unwrap();
                })
            };
            barrier.wait();
            if handshake {
                let before = 3;
                while db.storage.catalog().get_table_schema(&table).unwrap().columns.len() == before
                    && !running.is_finished()
                {
                    std::hint::spin_loop();
                }
            }
            let name = format!("fork_{kind}_{trial}");
            db.execute(&format!("CREATE BRANCH {name} FROM main AS OF NOW"))
                .unwrap();
            running.join().unwrap();
            let context = format!("{kind} trial {trial}");
            assert_rows_match_definition(&db, &name, &table, &context);

            // What it holds is what a merge would carry.
            on(
                &db,
                &name,
                Family::Text,
                &format!("UPDATE {table} SET b = b WHERE id = 2500"),
                &[],
            );
            if db.execute(&format!("MERGE BRANCH {name} INTO main")).is_ok() {
                let merged = rows(&db, Family::Text, &format!("SELECT b FROM {table} WHERE id = 2500"));
                assert_eq!(
                    int(merged[0].values.first()),
                    202_500,
                    "{context}: main after the merge"
                );
            }
        }
    }
}

/// Statements keep coming while branches are created: every fork lands on one side of
/// each of them.
#[test]
fn forks_among_a_stream_of_alters_keep_definition_and_rows_together() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    load_rewritten(&db, "streamed");
    let stop = Arc::new(AtomicBool::new(false));
    let altering = {
        let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut added = 0;
            while !stop.load(Ordering::SeqCst) {
                added += 1;
                db.execute(&format!(
                    "ALTER TABLE streamed ADD COLUMN x{added} INT8 DEFAULT {added}"
                ))
                .unwrap();
            }
        })
    };
    for round in 0..6 {
        let name = format!("s{round}");
        db.execute(&format!("CREATE BRANCH {name} FROM main AS OF NOW"))
            .unwrap();
        // Read twice around later statements: the fork point does not move either.
        let columns = columns_on(&db, &name, "streamed");
        let found = rows_on(&db, &name, Family::Text, "SELECT * FROM streamed");
        assert_eq!(found.len() as i64, REWRITTEN_ROWS);
        for row in &found {
            assert_eq!(row.values.len(), columns.len(), "round {round}");
            for (name, value) in columns.iter().zip(row.values.iter()) {
                if let Some(added) = name.strip_prefix('x') {
                    let added: i64 = added.parse().unwrap();
                    assert_eq!(int(Some(value)), added, "round {round}: {name} of {row:?}");
                }
            }
        }
        db.execute(&format!("DROP BRANCH {name}")).unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    altering.join().unwrap();
}

// ---------------------------------------------------------------------------
// A commit over the chunk size holds its stripes through the apply
// ---------------------------------------------------------------------------

/// Just over `FREEZE_CHUNK_ROWS`: its first chunk's stripes used to be free by the time
/// the batch applied.
const OVER_A_CHUNK: i64 = 10_020;

fn load_big(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE big (id INT8 PRIMARY KEY, v INT8)").unwrap();
    let mut id = 1;
    while id <= OVER_A_CHUNK {
        let last = (id + 1_999).min(OVER_A_CHUNK);
        let values: Vec<String> = (id..=last).map(|row| format!("({row}, {row})")).collect();
        db.execute(&format!("INSERT INTO big VALUES {}", values.join(",")))
            .unwrap();
        id = last + 1;
    }
}

/// Run `body` while writers change random rows of `big` on `level`. Returns how many
/// writes they made.
fn with_writers_on(db: &Arc<EmbeddedDatabase>, level: &'static str, body: impl FnOnce()) -> u64 {
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..4u64)
        .map(|writer| {
            let (db, stop) = (Arc::clone(db), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut state = writer.wrapping_mul(0x9E37_79B9).wrapping_add(17);
                let mut written = 0u64;
                let mut run = || {
                    while !stop.load(Ordering::SeqCst) {
                        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                        let row = (state >> 33) % (OVER_A_CHUNK as u64) + 1;
                        db.execute(&format!("UPDATE big SET v = -7 WHERE id = {row}")).unwrap();
                        written += 1;
                    }
                };
                if level == "main" {
                    run();
                } else {
                    db.with_request_branch_scope(level, || -> heliosdb_nano::Result<()> {
                        run();
                        Ok(())
                    })
                    .unwrap();
                }
                written
            })
        })
        .collect();
    body();
    stop.store(true, Ordering::SeqCst);
    writers.into_iter().map(|writer| writer.join().unwrap()).sum()
}

fn assert_every_row_moved(db: &EmbeddedDatabase, branch: &str) {
    let found = pairs(&rows_on(db, branch, Family::Text, "SELECT id, v FROM big"));
    assert_eq!(found.len() as i64, OVER_A_CHUNK);
    let wrong: Vec<&(i64, i64)> = found.iter().filter(|(id, v)| *v != id + 1_000_000).collect();
    assert!(
        wrong.is_empty(),
        "{} rows lost the commit, e.g. {:?}",
        wrong.len(),
        wrong.first()
    );
}

/// One statement on the child, on both families.
#[test]
fn a_statement_over_the_chunk_size_racing_its_parent_keeps_every_row() {
    for family in FAMILIES {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        load_big(&db);
        fork(&db, "c");
        let written = with_writers_on(&db, "main", || {
            on(&db, "c", family, "UPDATE big SET v = id + 1000000", &[]);
        });
        assert!(written > 0, "the race needs writers");
        assert_every_row_moved(&db, "c");
        db.execute("MERGE BRANCH c INTO main").unwrap();
        let found = pairs(&rows(&db, Family::Text, "SELECT id, v FROM big"));
        assert!(
            found.iter().all(|(id, v)| *v == id + 1_000_000),
            "[{family:?}] a merge carries each row"
        );
    }
}

/// A transaction's commit, which the review lost 4 rows per trial to.
#[test]
fn a_commit_over_the_chunk_size_racing_its_parent_keeps_every_row() {
    for trial in 0..3 {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        load_big(&db);
        fork(&db, "c");
        let session = txn_session(&db, "writer");
        db.execute_for_session(session, "USE BRANCH c").unwrap();
        db.begin_transaction_for_session(session).unwrap();
        db.execute_for_session(session, "UPDATE big SET v = id + 1000000")
            .unwrap();
        let written = with_writers_on(&db, "main", || {
            db.commit_transaction_for_session(session).unwrap();
        });
        assert!(written > 0, "trial {trial}: the race needs writers");
        db.destroy_session(session).unwrap();
        assert_every_row_moved(&db, "c");
        db.execute("MERGE BRANCH c INTO main").unwrap();
        let found = pairs(&rows(&db, Family::Text, "SELECT id, v FROM big"));
        assert!(found.iter().all(|(id, v)| *v == id + 1_000_000), "trial {trial}");
    }
}

/// The same a level down: the parent is a branch.
#[test]
fn a_statement_over_the_chunk_size_racing_a_parent_branch_keeps_every_row() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    load_big(&db);
    fork(&db, "p");
    db.with_request_branch_scope("p", || db.execute("CREATE BRANCH c FROM p AS OF NOW"))
        .unwrap();
    let written = with_writers_on(&db, "p", || {
        on(&db, "c", Family::Text, "UPDATE big SET v = id + 1000000", &[]);
    });
    assert!(written > 0, "the race needs writers");
    assert_every_row_moved(&db, "c");
    db.execute("MERGE BRANCH c INTO p").unwrap();
    assert_every_row_moved(&db, "p");
}

/// `DELETE` of every row: the child read none and a merge left some on main, whose
/// tombstones a parent's copy had marked.
#[test]
fn a_delete_over_the_chunk_size_racing_its_parent_merges_every_row() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    load_big(&db);
    fork(&db, "c");
    let written = with_writers_on(&db, "main", || {
        on(&db, "c", Family::Text, "DELETE FROM big", &[]);
    });
    assert!(written > 0, "the race needs writers");
    assert!(rows_on(&db, "c", Family::Text, "SELECT id FROM big").is_empty());
    db.execute("MERGE BRANCH c INTO main").unwrap();
    assert_eq!(pairs(&rows(&db, Family::Text, "SELECT id, v FROM big")), Vec::new());
}

// ---------------------------------------------------------------------------
// An anchor whose rows cannot be read under the definition now is refused
// ---------------------------------------------------------------------------

fn shaped(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE r (id INT PRIMARY KEY, a INT, b INT, s TEXT)")
        .unwrap();
    db.execute("INSERT INTO r VALUES (1, 100, 200, 'x'), (2, 101, 201, 'y'), (3, 102, 202, 'z')")
        .unwrap();
    // A commit of another table moves the time, whatever `r` is doing.
    db.execute("CREATE TABLE tick (id INT PRIMARY KEY)").unwrap();
}

fn tick(db: &EmbeddedDatabase, id: i32) {
    db.execute(&format!("INSERT INTO tick VALUES ({id})")).unwrap();
}

fn anchor_refusal(db: &EmbeddedDatabase, anchor: u64) -> String {
    let err = db
        .execute(&format!("CREATE BRANCH refused FROM main AS OF SCN {anchor}"))
        .expect_err("refused")
        .to_string();
    assert!(db.storage.get_branch("refused").is_err(), "nothing was created");
    err
}

/// The review's F3, and the statements like it: a branch anchored before them is refused
/// (0A000 by its marker), naming the table and saying why. Before, it held `a`'s values
/// under `b`, and a merge carried them to main.
#[test]
fn an_anchor_before_a_change_old_versions_cannot_be_read_under_is_refused() {
    for (statement, named, why) in [
        ("ALTER TABLE r DROP COLUMN a", "\"r\"", "column dropped"),
        ("ALTER TABLE r RENAME COLUMN a TO z", "\"r\"", "renamed"),
        ("ALTER TABLE r RENAME TO r2", "\"r2\"", "was renamed"),
        (
            "ALTER TABLE r ALTER COLUMN s SET STORAGE DICTIONARY",
            "\"r\"",
            "other storage",
        ),
    ] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        shaped(&db);
        tick(&db, 1);
        let anchor = last_scn(&db);
        db.execute(statement).unwrap();
        let err = anchor_refusal(&db, anchor);
        assert!(
            err.contains(BRANCH_ANCHOR_UNREADABLE_MARKER) && err.contains(named) && err.contains(why),
            "{statement}: {err}"
        );
        assert!(
            !err.contains(BRANCH_WRITE_UNSUPPORTED_MARKER),
            "nothing ran on a branch: {err}"
        );
        // A branch of now is fine.
        fork(&db, "now");
    }
}

/// What an old version CAN be read under stays as it was: added columns are NULL on the
/// branch, and a table dropped or created since is what it was.
#[test]
fn an_anchor_before_an_added_column_or_a_new_table_still_works() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    shaped(&db);
    tick(&db, 1);
    let anchor = last_scn(&db);
    db.execute("ALTER TABLE r ADD COLUMN added INT").unwrap();
    db.execute("CREATE TABLE later (id INT PRIMARY KEY)").unwrap();
    db.execute(&format!("CREATE BRANCH ok1 FROM main AS OF SCN {anchor}"))
        .unwrap();
    assert_eq!(rows_on(&db, "ok1", Family::Text, "SELECT id FROM r").len(), 3);
    db.execute("ALTER TABLE r RENAME COLUMN added TO added_too").unwrap();
    assert!(db
        .execute(&format!("CREATE BRANCH no FROM main AS OF SCN {anchor}"))
        .is_err());
    db.execute("DROP TABLE r").unwrap();
    db.execute("CREATE TABLE r (id INT PRIMARY KEY)").unwrap();
    db.execute(&format!("CREATE BRANCH ok2 FROM main AS OF SCN {anchor}"))
        .expect("a drop takes the record with the table");
}

/// A later anchor reads a version from before the statement too, for a row not written
/// since. The row is what it is now if nothing changed it after the anchor; if something
/// did, what it was cannot be read, and the anchor is refused.
#[test]
fn an_anchor_after_such_a_change_holds_the_rows_as_they_were_or_is_refused() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    shaped(&db);
    db.execute("ALTER TABLE r DROP COLUMN a").unwrap();
    // Versioned after the statement, so readable at any later anchor.
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE r SET b = 222 WHERE id = 2").unwrap();
    db.execute("COMMIT").unwrap();
    tick(&db, 1);
    let anchor = last_scn(&db);
    let b_on = |branch: &str| {
        let mut found: Vec<(i64, i64)> = rows_on(&db, branch, Family::Text, "SELECT id, b FROM r")
            .iter()
            .map(|row| (int(row.values.first()), int(row.values.get(1))))
            .collect();
        found.sort_unstable();
        found
    };
    let at_anchor = vec![(1, 200), (2, 222), (3, 202)];

    db.execute(&format!("CREATE BRANCH unmoved FROM main AS OF SCN {anchor}"))
        .unwrap();
    assert_eq!(b_on("unmoved"), at_anchor);

    // Changed after the anchor, with a readable version at it; created after it.
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE r SET b = 333 WHERE id = 2").unwrap();
    db.execute("COMMIT").unwrap();
    db.execute("INSERT INTO r VALUES (4, 204, 'w')").unwrap();
    db.execute(&format!("CREATE BRANCH later_rows FROM main AS OF SCN {anchor}"))
        .unwrap();
    assert_eq!(b_on("later_rows"), at_anchor);

    // Changed after the anchor, whose version at it is from before the statement.
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE r SET b = 444 WHERE id = 1").unwrap();
    db.execute("COMMIT").unwrap();
    let err = anchor_refusal(&db, anchor);
    assert!(err.contains("\"r\""), "{err}");
    // Past that write it reads again.
    tick(&db, 2);
    db.execute(&format!("CREATE BRANCH past FROM main AS OF SCN {}", last_scn(&db)))
        .unwrap();
    assert_eq!(b_on("past"), vec![(1, 444), (2, 333), (3, 202), (4, 204)]);
}

/// A rename leaves a table's versions behind: a row written since has none at an anchor
/// between. One that existed at the rename is refused, one created since is not there.
#[test]
fn an_anchor_after_a_rename_tells_rows_it_left_behind_from_rows_created_since() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    shaped(&db);
    db.execute("ALTER TABLE r RENAME TO r2").unwrap();
    tick(&db, 1);
    let anchor = last_scn(&db);
    db.execute("INSERT INTO r2 VALUES (4, 103, 203, 'w')").unwrap();
    db.execute(&format!("CREATE BRANCH created FROM main AS OF SCN {anchor}"))
        .unwrap();
    assert_eq!(
        ids(&rows_on(&db, "created", Family::Text, "SELECT id FROM r2")),
        vec![1, 2, 3]
    );
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE r2 SET b = 999 WHERE id = 1").unwrap();
    db.execute("COMMIT").unwrap();
    let err = anchor_refusal(&db, anchor);
    assert!(err.contains("\"r2\"") && err.contains("was renamed"), "{err}");
}

/// The record is durable, with the statement, and no key of an unknown family.
#[test]
fn a_shape_record_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let anchor = {
        let db = on_disk(dir.path());
        shaped(&db);
        tick(&db, 1);
        let anchor = db.storage.current_timestamp();
        db.execute("ALTER TABLE r DROP COLUMN a").unwrap();
        assert_eq!(keys_with_prefix(&db, "meta:table_shape:r").len(), 1);
        anchor
    };
    let db = on_disk(dir.path());
    let refused = db
        .storage
        .create_branch_at_snapshot("x", Some("main"), Some(anchor), Default::default())
        .expect_err("refused")
        .to_string();
    assert!(
        refused.contains(BRANCH_ANCHOR_UNREADABLE_MARKER) && refused.contains("\"r\""),
        "{refused}"
    );
    // A drop takes it.
    db.execute("DROP TABLE r").unwrap();
    assert!(keys_with_prefix(&db, "meta:table_shape:r").is_empty());
}

// ---------------------------------------------------------------------------
// A table's history leaves with its name
// ---------------------------------------------------------------------------

fn no_history_of(db: &EmbeddedDatabase, table: &str) {
    for family in ["v", "v_idx", "vmeta"] {
        let keys = keys_with_prefix(db, &format!("{family}:{table}:"));
        assert!(keys.is_empty(), "{} {family} keys of {table} left", keys.len());
    }
}

fn versioned_accounts(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE tick (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE acct (id INT PRIMARY KEY, bal INT)").unwrap();
    db.execute("INSERT INTO acct VALUES (1, 1000), (2, 2000), (3, 3000)")
        .unwrap();
    for id in 1..=3 {
        db.execute("BEGIN").unwrap();
        db.execute(&format!("UPDATE acct SET bal = bal + 1 WHERE id = {id}"))
            .unwrap();
        db.execute("COMMIT").unwrap();
    }
}

/// The review's N1. `RENAME acct TO acct_old; CREATE TABLE acct;`: the new table's row
/// ids overlap the old ones. A row inserted after an anchor whose id had a version at the
/// anchor used to get it: the branch read `(1, 1001)` where the table was empty, and a
/// merge could put that on main. An anchor before the rename stays refused whatever
/// becomes of the renamed table.
#[test]
fn a_renamed_tables_history_stays_out_of_a_table_of_its_old_name() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    versioned_accounts(&db);
    tick(&db, 1);
    let before_rename = last_scn(&db);
    db.execute("ALTER TABLE acct RENAME TO acct_old").unwrap();
    no_history_of(&db, "acct");
    db.execute("CREATE TABLE acct (id INT PRIMARY KEY, bal INT)").unwrap();
    tick(&db, 2);
    let anchor = last_scn(&db);
    for (id, bal) in [(10, 1), (20, 2), (30, 3), (40, 4)] {
        db.execute(&format!("INSERT INTO acct VALUES ({id}, {bal})")).unwrap();
    }
    let as_of = format!("SELECT id, bal FROM acct AS OF SCN {anchor}");
    assert_eq!(pairs(&rows(&db, Family::Text, &as_of)), Vec::new(), "main's AS OF");

    db.execute(&format!("CREATE BRANCH sw FROM main AS OF SCN {anchor}"))
        .unwrap();
    assert_eq!(
        pairs(&rows_on(&db, "sw", Family::Text, "SELECT id, bal FROM acct")),
        Vec::new()
    );
    assert_eq!(
        pairs(&rows_on(&db, "sw", Family::Text, "SELECT id, bal FROM acct_old")),
        vec![(1, 1001), (2, 2001), (3, 3001)]
    );
    on(
        &db,
        "sw",
        Family::Text,
        "UPDATE acct SET bal = bal + 0 WHERE id = 1",
        &[],
    );
    let main_rows = vec![(10, 1), (20, 2), (30, 3), (40, 4)];
    db.execute("MERGE BRANCH sw INTO main").unwrap();
    assert_eq!(pairs(&rows(&db, Family::Text, "SELECT id, bal FROM acct")), main_rows);

    let err = anchor_refusal(&db, before_rename);
    assert!(err.contains("was renamed"), "{err}");
    db.execute("DROP TABLE acct_old").unwrap();
    let err = anchor_refusal(&db, before_rename);
    assert!(err.contains("\"acct\"") && err.contains("was renamed"), "{err}");
}

/// A drop's history goes with it too. A drop only took its delete events, and left the
/// table's versions for a later table of the name to read as its own.
#[test]
fn a_dropped_tables_history_stays_out_of_a_table_of_its_name() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    versioned_accounts(&db);
    db.execute("DROP TABLE acct").unwrap();
    no_history_of(&db, "acct");
    db.execute("CREATE TABLE acct (id INT PRIMARY KEY, bal INT)").unwrap();
    tick(&db, 1);
    let anchor = last_scn(&db);
    db.execute("INSERT INTO acct VALUES (10, 1)").unwrap();
    db.execute("INSERT INTO acct VALUES (20, 2)").unwrap();
    db.execute(&format!("CREATE BRANCH sw FROM main AS OF SCN {anchor}"))
        .unwrap();
    assert_eq!(
        pairs(&rows_on(&db, "sw", Family::Text, "SELECT id, bal FROM acct")),
        Vec::new()
    );
}

fn history_keys(db: &EmbeddedDatabase, table: &str) -> usize {
    ["v", "v_idx", "vmeta"]
        .iter()
        .map(|family| keys_with_prefix(db, &format!("{family}:{table}:")).len())
        .sum()
}

fn big_with_history(db: &EmbeddedDatabase) {
    load_big(db);
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE big SET v = v + 1").unwrap();
    db.execute("COMMIT").unwrap();
    assert!(
        history_keys(db, "big") > 10_000,
        "the test needs more than one write of history"
    );
}

/// The review's P1: a purge that stops part way (a crash, an error) used to leave the table
/// with part of its history, and `AS OF` answered an older version as a row's newest. The
/// name is no table by then, and what is left goes before a table has the name again: at the
/// next open, or first thing in a `CREATE` or a `RENAME` onto it. Stopped by a fail point
/// after its first write.
#[test]
fn a_history_purge_stopped_part_way_is_finished_before_the_name_is_a_table_again() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = on_disk(dir.path());
        big_with_history(&db);
        db.storage.debug_fail_merge_at(MergeFailSite::HistoryPurge, 0);
        db.execute("DROP TABLE big").expect("the drop is done");
        db.storage.debug_clear_merge_failpoint();
        assert!(history_keys(&db, "big") > 0, "stopped part way");
        assert_eq!(keys_with_prefix(&db, "meta:history_purge:big").len(), 1);
        assert!(
            db.query("SELECT id FROM big", &[]).is_err(),
            "and no table has the name"
        );
    }
    let db = on_disk(dir.path());
    no_history_of(&db, "big");
    assert!(
        keys_with_prefix(&db, "meta:history_purge:").is_empty(),
        "the open finished it"
    );

    for onto in [
        "CREATE TABLE big (id INT8 PRIMARY KEY, v INT8)",
        "ALTER TABLE other RENAME TO big",
    ] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        big_with_history(&db);
        db.execute("CREATE TABLE other (id INT8 PRIMARY KEY, v INT8)").unwrap();
        db.storage.debug_fail_merge_at(MergeFailSite::HistoryPurge, 0);
        db.execute("ALTER TABLE big RENAME TO big2")
            .expect("the rename is done");
        db.storage.debug_clear_merge_failpoint();
        assert!(history_keys(&db, "big") > 0, "{onto}: stopped part way");
        assert_eq!(
            rows(&db, Family::Text, "SELECT id FROM big2").len() as i64,
            OVER_A_CHUNK
        );
        db.execute(onto).unwrap();
        no_history_of(&db, "big");
        assert!(keys_with_prefix(&db, "meta:history_purge:").is_empty(), "{onto}");
        assert!(rows(&db, Family::Text, "SELECT id FROM big").is_empty(), "{onto}");
    }
}

/// The review's R1: a rename moved its CHECK and FOREIGN KEY records, its IDENTITY and
/// partition links in writes after its batch, and the purge ran in between, so a crash
/// there left the renamed table enforcing none of them. They are in the rename's batch:
/// when the purge's first write is made, they are under the new name already.
#[test]
fn a_rename_has_moved_its_side_records_before_its_purge() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    big_with_history(&db);
    db.execute("ALTER TABLE big ADD CONSTRAINT big_v_floor CHECK (v > -1000000)")
        .unwrap();
    let raw = db.storage.db();
    let seen: Arc<std::sync::Mutex<Option<(bool, bool)>>> = Arc::new(std::sync::Mutex::new(None));
    {
        let seen = Arc::clone(&seen);
        db.storage.debug_on_merge_site(
            MergeFailSite::HistoryPurge,
            Some(Arc::new(move || {
                let mut seen = seen.lock().unwrap();
                if seen.is_none() {
                    let present = |key: &[u8]| raw.get(key).unwrap().is_some();
                    *seen = Some((present(b"table_constraints:big2"), present(b"table_constraints:big")));
                }
            })),
        );
    }
    db.execute("ALTER TABLE big RENAME TO big2").unwrap();
    db.storage.debug_on_merge_site(MergeFailSite::HistoryPurge, None);
    assert_eq!(
        *seen.lock().unwrap(),
        Some((true, false)),
        "(under the new name, under the old) when the purge began"
    );
    assert!(
        db.execute("INSERT INTO big2 VALUES (99999, -5000000)").is_err(),
        "the CHECK holds"
    );
}

/// The review's R2: `AS OF` on a renamed table asked, for each row, whether it has a
/// version, and that seek stepped through every key the purge had just deleted under the
/// old name, which sorts right after: rows times purged keys, until compaction. The probes
/// stop at their row's prefix now. Counted by RocksDB on this thread.
#[test]
fn as_of_on_a_renamed_table_does_not_walk_the_purged_history() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    big_with_history(&db);
    db.execute("ALTER TABLE big RENAME TO big2").unwrap();
    no_history_of(&db, "big");
    db.execute("CREATE TABLE tick (id INT PRIMARY KEY)").unwrap();
    tick(&db, 1);
    let scn = last_scn(&db);
    rocksdb::perf::set_perf_stats(rocksdb::perf::PerfStatsLevel::EnableCount);
    let mut perf = rocksdb::perf::PerfContext::default();
    perf.reset();
    let found = rows(&db, Family::Text, &format!("SELECT id FROM big2 AS OF SCN {scn}"));
    let stepped = perf.metric(rocksdb::perf::PerfMetric::InternalDeleteSkippedCount);
    rocksdb::perf::set_perf_stats(rocksdb::perf::PerfStatsLevel::Disable);
    assert_eq!(found.len() as i64, OVER_A_CHUNK);
    assert!(
        stepped < 100_000,
        "{stepped} deleted keys stepped over for {} rows",
        found.len()
    );
}

/// More history than one write group's worth of keys goes all the same.
#[test]
fn a_long_history_is_purged_whole() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    load_big(&db);
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE big SET v = v + 1").unwrap();
    db.execute("COMMIT").unwrap();
    assert!(keys_with_prefix(&db, "v:big:").len() + keys_with_prefix(&db, "v_idx:big:").len() > 10_000);
    db.execute("ALTER TABLE big RENAME TO big2").unwrap();
    no_history_of(&db, "big");
    assert_eq!(
        rows(&db, Family::Text, "SELECT id FROM big2").len() as i64,
        OVER_A_CHUNK
    );
}

// ---------------------------------------------------------------------------
// A materialized view and a fork
// ---------------------------------------------------------------------------

const VIEW_ROWS: i64 = 3_000;

fn load_view_source(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE src (id INT8 PRIMARY KEY, v INT8)").unwrap();
    let values: Vec<String> = (1..=VIEW_ROWS).map(|row| format!("({row}, 1)")).collect();
    db.execute(&format!("INSERT INTO src VALUES {}", values.join(",")))
        .unwrap();
}

/// The review's N2. A fork landing inside a `REFRESH` kept "never been refreshed" or part
/// of the rows, for good: `REFRESH` on a branch is refused. It is wholly before or after.
#[test]
fn a_refresh_is_wholly_before_or_after_a_fork() {
    for trial in 0..6 {
        let concurrent = trial % 2 == 1;
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        load_view_source(&db);
        db.execute("CREATE MATERIALIZED VIEW m AS SELECT id, v FROM src")
            .unwrap();
        db.execute("UPDATE src SET v = 2").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let refreshing = {
            let (db, barrier) = (Arc::clone(&db), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                let sql = if concurrent {
                    "REFRESH MATERIALIZED VIEW CONCURRENTLY m"
                } else {
                    "REFRESH MATERIALIZED VIEW m"
                };
                db.execute(sql).unwrap();
            })
        };
        barrier.wait();
        // In its rows when the fork starts.
        while !refreshing.is_finished() {
            let in_rows = if concurrent {
                !keys_with_prefix(&db, "data:__mv_m__temp").is_empty()
            } else {
                keys_with_prefix(&db, "data:__mv_m:").len() as i64 != VIEW_ROWS
            };
            if in_rows {
                break;
            }
            std::hint::spin_loop();
        }
        fork(&db, "r");
        refreshing.join().unwrap();
        let found = pairs(&rows_on(&db, "r", Family::Text, "SELECT id, v FROM m"));
        assert_eq!(found.len() as i64, VIEW_ROWS, "trial {trial}: part of the view");
        let first = found.first().map(|(_, v)| *v);
        assert!(
            found.iter().all(|(_, v)| Some(*v) == first),
            "trial {trial}: both sides"
        );
    }
}

/// `CREATE MATERIALIZED VIEW` registers the view and fills its table in several writes.
#[test]
fn creating_a_materialized_view_is_wholly_before_or_after_a_fork() {
    for trial in 0..3 {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        load_view_source(&db);
        let creating = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                db.execute("CREATE MATERIALIZED VIEW m2 AS SELECT id, v FROM src")
                    .unwrap();
            })
        };
        while keys_with_prefix(&db, "data:__mv_m2:").is_empty() && !creating.is_finished() {
            std::hint::spin_loop();
        }
        fork(&db, "r");
        creating.join().unwrap();
        let found = rows_on(&db, "r", Family::Text, "SELECT id, v FROM m2");
        assert_eq!(found.len() as i64, VIEW_ROWS, "trial {trial}: part of the view");
    }
}

/// `CREATE SCHEMA` with elements runs each as a statement of its own: a fork sees all of
/// them or none.
#[test]
fn a_schema_with_elements_is_wholly_before_or_after_a_fork() {
    let tables: Vec<String> = (0..10).map(|n| format!("a{n}")).collect();
    let elements: Vec<String> = tables
        .iter()
        .map(|table| format!("CREATE TABLE {table} (id INT)"))
        .collect();
    for trial in 0..8 {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let creating = {
            let (db, barrier) = (Arc::clone(&db), Arc::clone(&barrier));
            let sql = format!("CREATE SCHEMA s {}", elements.join(" "));
            std::thread::spawn(move || {
                barrier.wait();
                db.execute(&sql).unwrap();
            })
        };
        barrier.wait();
        fork(&db, "r");
        creating.join().unwrap();
        let visible = tables
            .iter()
            .filter(|table| {
                db.with_request_branch_scope("r", || db.query(&format!("SELECT id FROM s.{table}"), &[]))
                    .is_ok()
            })
            .count();
        assert!(
            visible == 0 || visible == tables.len(),
            "trial {trial}: {visible} tables"
        );
    }
}

/// The review's N3. A `REFRESH` replaces the view's rows and keeps no history of them, so a
/// branch anchored before it read the view as empty. Refused, naming the view; past the
/// refresh it reads what the view held.
#[test]
fn an_anchor_before_a_refresh_is_refused_naming_the_view() {
    for concurrent in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        shaped(&db);
        db.execute("CREATE TABLE src (id INT8 PRIMARY KEY, v INT8)").unwrap();
        db.execute("INSERT INTO src VALUES (1, 1), (2, 2)").unwrap();
        db.execute("CREATE MATERIALIZED VIEW m AS SELECT id, v FROM src")
            .unwrap();
        tick(&db, 1);
        let anchor = last_scn(&db);
        db.execute(if concurrent {
            "REFRESH MATERIALIZED VIEW CONCURRENTLY m"
        } else {
            "REFRESH MATERIALIZED VIEW m"
        })
        .unwrap();
        let err = anchor_refusal(&db, anchor);
        assert!(
            err.contains(BRANCH_ANCHOR_UNREADABLE_MARKER) && err.contains("materialized view \"m\""),
            "[concurrent {concurrent}] {err}"
        );
        tick(&db, 2);
        let later = last_scn(&db);
        db.execute(&format!("CREATE BRANCH past FROM main AS OF SCN {later}"))
            .unwrap();
        assert_eq!(
            pairs(&rows_on(&db, "past", Family::Text, "SELECT id, v FROM m")),
            vec![(1, 1), (2, 2)],
            "[concurrent {concurrent}]"
        );
    }
}

// ---------------------------------------------------------------------------
// What is not forked
// ---------------------------------------------------------------------------

/// Views, materialized views and functions are shared by all branches, so a branch
/// session cannot change them: each statement is refused, 0A000 by its marker, on both
/// families, and main has what it had.
#[test]
fn a_branch_session_cannot_change_what_all_branches_share() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        db.execute("CREATE VIEW shared_view AS SELECT id FROM t").unwrap();
        db.execute("CREATE MATERIALIZED VIEW shared_mv AS SELECT id FROM t")
            .unwrap();
        db.execute("REFRESH MATERIALIZED VIEW shared_mv").unwrap();
        db.execute("CREATE FUNCTION dbl(x INT) RETURNS INT AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
            .unwrap();
        db.execute("CREATE PROCEDURE shared_proc() LANGUAGE sql AS $$DELETE FROM t$$")
            .unwrap();
        fork(&db, "r");
        for statement in [
            "CREATE VIEW v2 AS SELECT id FROM t",
            "CREATE OR REPLACE VIEW shared_view AS SELECT v FROM t",
            "DROP VIEW shared_view",
            "CREATE MATERIALIZED VIEW mv2 AS SELECT id FROM t",
            "REFRESH MATERIALIZED VIEW shared_mv",
            "DROP MATERIALIZED VIEW shared_mv",
            "CREATE FUNCTION f2(x INT) RETURNS INT AS $$ SELECT $1 $$ LANGUAGE sql",
            "CREATE OR REPLACE FUNCTION dbl(x INT) RETURNS INT AS $$ SELECT $1 * 3 $$ LANGUAGE sql",
            "DROP FUNCTION dbl",
            "CREATE PROCEDURE p2() LANGUAGE sql AS $$DELETE FROM t$$",
            "DROP PROCEDURE shared_proc",
        ] {
            let err = db
                .with_request_branch_scope("r", || exec(&db, family, statement, &[]))
                .expect_err(statement)
                .to_string();
            // The params family has no REFRESH or DROP MATERIALIZED VIEW at all, on main
            // either: they do not parse there, which changes nothing just the same.
            let unparsed_there =
                matches!(family, Family::Params) && statement.contains("MATERIALIZED") && err.contains("parse error");
            assert!(
                unparsed_there
                    || err.contains(BRANCH_WRITE_UNSUPPORTED_MARKER)
                    || err.contains(BRANCH_STATEMENT_UNSUPPORTED_MARKER),
                "[{family:?}] {statement}: {err}"
            );
        }
        assert_eq!(
            ids(&rows(&db, Family::Text, "SELECT id FROM shared_view")),
            vec![1, 2, 3]
        );
        assert_eq!(ids(&rows(&db, Family::Text, "SELECT id FROM shared_mv")), vec![1, 2, 3]);
        assert_eq!(int(rows(&db, Family::Text, "SELECT dbl(21)")[0].values.first()), 42);
        // Main's change shows on the branch: that is what shared means.
        db.execute("CREATE OR REPLACE FUNCTION dbl(x INT) RETURNS INT AS $$ SELECT $1 * 3 $$ LANGUAGE sql")
            .unwrap();
        assert_eq!(
            int(rows_on(&db, "r", Family::Text, "SELECT dbl(21)")[0].values.first()),
            63
        );
    }
}

// ---------------------------------------------------------------------------
// A table name a row's key cannot hold
// ---------------------------------------------------------------------------

/// `data:we:ird:1` is also a row of table `we`. Refused in every statement that names a
/// new table, by its marker (42602), on main and on a branch.
#[test]
fn a_table_name_cannot_hold_a_colon() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        fork(&db, "r");
        for statement in [
            r#"CREATE TABLE "we:ird" (id INT PRIMARY KEY)"#,
            r#"CREATE TABLE "we:ird" AS SELECT id FROM t"#,
            r#"ALTER TABLE t RENAME TO "we:ird""#,
        ] {
            let err = exec(&db, family, statement, &[]).expect_err(statement).to_string();
            assert!(
                err.contains(TABLE_NAME_HOLDS_COLON_MARKER),
                "[{family:?}] {statement}: {err}"
            );
        }
        for statement in [
            r#"CREATE TABLE "we:ird" (id INT PRIMARY KEY)"#,
            r#"CREATE TABLE "we:ird" AS SELECT id FROM t"#,
        ] {
            let err = db
                .with_request_branch_scope("r", || exec(&db, family, statement, &[]))
                .expect_err(statement)
                .to_string();
            assert!(
                err.contains(TABLE_NAME_HOLDS_COLON_MARKER),
                "[{family:?}] on r: {statement}: {err}"
            );
        }
        assert_eq!(
            pairs(&rows(&db, family, "SELECT id, v FROM t")),
            vec![(1, 10), (2, 20), (3, 30)]
        );
    }
}

/// A refused materialized view leaves no registration, and a schema no table could be
/// created in is refused like one.
#[test]
fn a_name_that_cannot_hold_a_table_registers_nothing() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    let err = db
        .execute(r#"CREATE MATERIALIZED VIEW "a:mv" AS SELECT id FROM t"#)
        .expect_err("refused")
        .to_string();
    assert!(err.contains(TABLE_NAME_HOLDS_COLON_MARKER), "{err}");
    assert!(keys_with_prefix(&db, "meta:mv:").is_empty(), "a registration is left");
    let err = db.execute(r#"CREATE SCHEMA "s:1""#).expect_err("refused").to_string();
    assert!(err.contains(TABLE_NAME_HOLDS_COLON_MARKER), "{err}");
    assert!(keys_with_prefix(&db, "meta:schema:s").is_empty());
}

/// A store that holds such a table keeps opening: only new names are refused.
#[test]
fn a_store_holding_such_a_table_keeps_opening() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = on_disk(dir.path());
        seed(&db);
        let schema = heliosdb_nano::Schema::new(vec![heliosdb_nano::Column::new("id", heliosdb_nano::DataType::Int4)]);
        db.storage
            .put(&b"meta:table:we:ird".to_vec(), &bincode::serialize(&schema).unwrap())
            .unwrap();
    }
    let db = on_disk(dir.path());
    assert_eq!(
        pairs(&rows(&db, Family::Text, "SELECT id, v FROM t")),
        vec![(1, 10), (2, 20), (3, 30)]
    );
    db.execute("INSERT INTO t VALUES (4, 40)").unwrap();
}

// ---------------------------------------------------------------------------
// The refusals of DROP
// ---------------------------------------------------------------------------

/// `MERGE … WITH (delete_branch_after = true)` of a branch with a live child: the drop
/// that would fail is refused before the merge lands. It used to land, and then answer
/// XX000.
#[test]
fn a_merge_that_could_not_drop_its_source_does_not_land() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    fork(&db, "p");
    on(&db, "p", Family::Text, "UPDATE t SET v = 11 WHERE id = 1", &[]);
    db.with_request_branch_scope("p", || db.execute("CREATE BRANCH k FROM p AS OF NOW"))
        .unwrap();
    let err = db
        .execute("MERGE BRANCH p INTO main WITH (delete_branch_after = true)")
        .expect_err("refused")
        .to_string();
    assert!(err.contains(BRANCH_HAS_CHILD_MARKER) && err.contains("\"k\""), "{err}");
    assert_eq!(
        pairs(&rows(&db, Family::Text, "SELECT id, v FROM t WHERE id = 1")),
        vec![(1, 10)],
        "nothing landed"
    );
    assert_eq!(db.storage.get_branch("p").unwrap().state, BranchState::Active);
    db.execute("DROP BRANCH k").unwrap();
    db.execute("MERGE BRANCH p INTO main WITH (delete_branch_after = true)")
        .unwrap();
    assert_eq!(
        pairs(&rows(&db, Family::Text, "SELECT id, v FROM t WHERE id = 1")),
        vec![(1, 11)]
    );
}

/// A merged child's record blocks its parent's drop: the message names it and says what
/// to do, and doing that works.
#[test]
fn a_drop_blocked_by_a_merged_child_says_so() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    fork(&db, "p");
    db.with_request_branch_scope("p", || db.execute("CREATE BRANCH c FROM p AS OF NOW"))
        .unwrap();
    on(&db, "c", Family::Text, "UPDATE t SET v = 99 WHERE id = 3", &[]);
    db.execute("MERGE BRANCH c INTO p").unwrap();
    let err = db.execute("DROP BRANCH p").expect_err("blocked").to_string();
    assert!(
        err.contains(BRANCH_HAS_CHILD_MARKER) && err.contains("DROP BRANCH c first"),
        "{err}"
    );
    db.execute("DROP BRANCH c").unwrap();
    db.execute("DROP BRANCH p").unwrap();
}

/// The review's N5: a child created after the check and before the merge closes the source
/// used to land the merge and then answer 2BP01. Now it is either: the merge, with its
/// source gone; or the refusal, with nothing changed.
#[test]
fn a_child_racing_a_merge_that_drops_its_source_never_lands_half() {
    for round in 0..20 {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        seed(&db);
        fork(&db, "p");
        on(&db, "p", Family::Text, "UPDATE t SET v = 11 WHERE id = 1", &[]);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let creating = {
            let (db, barrier) = (Arc::clone(&db), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                db.execute("CREATE BRANCH k FROM p AS OF NOW").is_ok()
            })
        };
        barrier.wait();
        let merged = db.execute("MERGE BRANCH p INTO main WITH (delete_branch_after = true)");
        let created = creating.join().unwrap();
        let row = pairs(&rows(&db, Family::Text, "SELECT id, v FROM t WHERE id = 1"));
        let state = db.storage.get_branch("p").map(|meta| meta.state);
        if merged.is_ok() {
            assert_eq!(row, vec![(1, 11)], "round {round}");
            assert!(!created, "round {round}: the source of a child was dropped");
            assert!(
                !matches!(state, Ok(BranchState::Active | BranchState::Merged { .. })),
                "round {round}"
            );
        } else {
            assert_eq!(row, vec![(1, 10)], "round {round}: {merged:?}");
            assert!(matches!(state, Ok(BranchState::Active)), "round {round}: {merged:?}");
        }
    }
}

/// `USE BRANCH c DROP TABLE t` answered success and ignored what follows the name. A
/// syntax error, with nothing bound and nothing run.
#[test]
fn use_branch_takes_one_name() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db);
        fork(&db, "r");
        for sql in [
            "USE BRANCH r DROP TABLE t",
            "USE DATABASE BRANCH r x",
            "USE BRANCH r; DROP TABLE t",
        ] {
            let err = exec(&db, family, sql, &[]).expect_err(sql);
            assert!(matches!(err, Error::SqlParse(_)), "[{family:?}] {sql}: {err}");
            assert_eq!(db.current_branch(), "main", "[{family:?}] {sql}");
        }
        assert_eq!(rows(&db, family, "SELECT id, v FROM t").len(), 3);
        exec(&db, family, "USE BRANCH r;", &[]).expect("a statement's semicolon is no word");
        assert_eq!(db.current_branch(), "r");
        // The review's P3: comments are no words either.
        for sql in [
            "USE BRANCH main -- back to main",
            "USE BRANCH r /* a hint */",
            "USE BRANCH main ;  ",
            "USE BRANCH r;\n",
            "USE BRANCH main /* one */ -- two",
        ] {
            exec(&db, family, sql, &[]).unwrap_or_else(|e| panic!("[{family:?}] {sql:?}: {e}"));
        }
        assert_eq!(db.current_branch(), "main");
        let err = exec(&db, family, "USE BRANCH r /* a hint */ DROP TABLE t", &[]).expect_err("a word after it");
        assert!(matches!(err, Error::SqlParse(_)), "[{family:?}] {err}");
    }
}

// ---------------------------------------------------------------------------
// An open with many merged branches
// ---------------------------------------------------------------------------

/// The review's F5: an open listed the branches once per merged branch, so it was
/// quadratic in them. How many listings it takes does not depend on how many there are.
#[test]
fn an_open_lists_the_branches_no_more_for_more_merged_branches() {
    let listings_at_open = |merged: usize| -> u64 {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = on_disk(dir.path());
            seed(&db);
            for n in 0..merged {
                fork(&db, &format!("m{n}"));
                db.execute(&format!("MERGE BRANCH m{n} INTO main")).unwrap();
            }
        }
        let db = on_disk(dir.path());
        db.storage.branch_manager().unwrap().debug_catalog_listings()
    };
    assert_eq!(listings_at_open(20), listings_at_open(2));
}

// ---------------------------------------------------------------------------
// Creation is linearized
// ---------------------------------------------------------------------------

/// Writers run through `CREATE BRANCH`. A row committed before it began is on the branch,
/// and one whose statement began after it returned is not. In between, either: but a
/// reader sees one fork point, and a later read of the branch sees the same rows. Looped:
/// on a tree with no drain this finds a write past the fork.
#[test]
fn a_write_after_create_branch_returned_is_invisible() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE racing (id INT8 PRIMARY KEY)").unwrap();
    let started = Arc::new(AtomicI64::new(0));
    let done = Arc::new(AtomicI64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..4)
        .map(|_| {
            let (db, started, done, stop) = (
                Arc::clone(&db),
                Arc::clone(&started),
                Arc::clone(&done),
                Arc::clone(&stop),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let id = started.fetch_add(1, Ordering::SeqCst) + 1;
                    db.execute(&format!("INSERT INTO racing VALUES ({id})")).unwrap();
                    done.fetch_max(id, Ordering::SeqCst);
                }
            })
        })
        .collect();
    for round in 0..25 {
        let name = format!("race_{round}");
        // Rows with an id at or below it may still be running; the ones that finished
        // before CREATE began are found below by what `done` says of each.
        let finished: Vec<i64> = {
            let rows_now = db.query("SELECT id FROM racing", &[]).unwrap();
            rows_now.iter().map(|row| int(row.values.first())).collect()
        };
        db.execute(&format!("CREATE BRANCH {name} FROM main AS OF NOW"))
            .unwrap();
        let begun_before_return = started.load(Ordering::SeqCst);
        // Let later writes through, so a leak has something to show.
        let target = begun_before_return + 8;
        while done.load(Ordering::SeqCst) < target {
            assert!(writers.iter().all(|writer| !writer.is_finished()), "a writer stopped");
            std::hint::spin_loop();
        }
        let seen: Vec<i64> = rows_on(&db, &name, Family::Text, "SELECT id FROM racing")
            .iter()
            .map(|row| int(row.values.first()))
            .collect();
        let seen_set: std::collections::HashSet<i64> = seen.iter().copied().collect();
        for id in &finished {
            assert!(
                seen_set.contains(id),
                "round {round}: row {id} committed before the fork is missing"
            );
        }
        if let Some(leaked) = seen.iter().find(|id| **id > begun_before_return) {
            panic!("round {round}: row {leaked} began after CREATE BRANCH returned ({begun_before_return})");
        }
        // The same rows again: a fork point does not move.
        let again = rows_on(&db, &name, Family::Text, "SELECT id FROM racing").len();
        assert_eq!(again, seen.len(), "round {round}");
        db.execute(&format!("DROP BRANCH {name}")).unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    for writer in writers {
        writer.join().unwrap();
    }
}

/// A parent's writer and the child's own writer of one row: whoever comes first, the
/// child's write is what the child reads. Without the stripes, a parent's copy decided
/// before the child's write lands after it, and the child loses its change.
#[test]
fn a_parents_copy_never_overwrites_the_childs_own_write() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE fought (id INT PRIMARY KEY, v INT)").unwrap();
    for id in 1..=400 {
        db.execute(&format!("INSERT INTO fought VALUES ({id}, 0)")).unwrap();
    }
    fork(&db, "r");
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let parent = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            go_rx.recv().unwrap();
            for id in 1..=400 {
                db.execute(&format!("UPDATE fought SET v = 1 WHERE id = {id}")).unwrap();
            }
        })
    };
    go_tx.send(()).unwrap();
    for id in 1..=400 {
        on(
            &db,
            "r",
            Family::Text,
            &format!("UPDATE fought SET v = {id} WHERE id = {id}"),
            &[],
        );
    }
    parent.join().unwrap();
    let found = pairs(&rows_on(&db, "r", Family::Text, "SELECT id, v FROM fought"));
    assert_eq!(found.len(), 400);
    for (id, v) in found {
        assert_eq!(id, v, "row {id} lost the branch's write to a copy");
    }
    // And they are the branch's changes, not copies.
    let r = branch_id(&db, "r");
    assert!(keys_with_prefix(&db, &format!("bfrz:{r}:fought:")).len() <= 400);
    db.execute("MERGE BRANCH r INTO main").unwrap();
    for (id, v) in pairs(&rows(&db, Family::Text, "SELECT id, v FROM fought")) {
        assert_eq!(id, v, "row {id}");
    }
}

/// While a parent writes, the branch never sees a parent's new row, by a scan, by a key
/// or by a uniqueness check: each reads main first and then asks the levels.
#[test]
fn a_reader_of_a_branch_racing_its_parent_sees_the_fork_point() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE raced (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO raced VALUES (1, 0), (2, 0)").unwrap();
    fork(&db, "r");
    // An overlay in the cache, so a stale copy of it would be served.
    let _ = rows_on(&db, "r", Family::Text, "SELECT id FROM raced");
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut v = 1;
            while !stop.load(Ordering::SeqCst) {
                db.execute(&format!("UPDATE raced SET v = {v} WHERE id = 1")).unwrap();
                db.execute(&format!("UPDATE raced SET v = {v} WHERE id = 2")).unwrap();
                db.execute(&format!("INSERT INTO raced VALUES ({}, 0)", v + 10))
                    .unwrap();
                db.execute(&format!("DELETE FROM raced WHERE id = {}", v + 10)).unwrap();
                v += 1;
            }
        })
    };
    for _ in 0..200 {
        let scan = pairs(&rows_on(&db, "r", Family::Text, "SELECT id, v FROM raced"));
        assert_eq!(scan, vec![(1, 0), (2, 0)]);
        let point = pairs(&rows_on(&db, "r", Family::Text, "SELECT id, v FROM raced WHERE id = 2"));
        assert_eq!(point, vec![(2, 0)]);
    }
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
}

// ---------------------------------------------------------------------------
// A big batch
// ---------------------------------------------------------------------------

/// `DROP TABLE` of a table larger than one chunk, with a live child and a sibling that
/// holds some rows of its own: the rows are there, whole and only once.
#[test]
fn dropping_a_large_table_under_a_live_child() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE big (id INT PRIMARY KEY, v INT)").unwrap();
    const ROWS: i32 = 12_000;
    for start in (0..ROWS).step_by(1_000) {
        let values: Vec<String> = (start..start + 1_000).map(|id| format!("({id}, {id})")).collect();
        db.execute(&format!("INSERT INTO big VALUES {}", values.join(",")))
            .unwrap();
    }
    fork(&db, "kept");
    fork(&db, "other");
    on(&db, "other", Family::Text, "UPDATE big SET v = -1 WHERE id = 5", &[]);
    db.execute("DROP TABLE big").unwrap();
    assert_eq!(
        rows_on(&db, "kept", Family::Text, "SELECT id FROM big").len(),
        ROWS as usize
    );
    assert_eq!(
        rows_on(&db, "other", Family::Text, "SELECT id FROM big").len(),
        ROWS as usize
    );
    assert_eq!(
        pairs(&rows_on(
            &db,
            "other",
            Family::Text,
            "SELECT id, v FROM big WHERE id = 5"
        )),
        vec![(5, -1)]
    );
    assert_eq!(
        pairs(&rows_on(
            &db,
            "kept",
            Family::Text,
            "SELECT id, v FROM big WHERE id = 5"
        )),
        vec![(5, 5)]
    );
    let kept = branch_id(&db, "kept");
    assert_eq!(keys_with_prefix(&db, &format!("bfrz:{kept}:big:")).len(), ROWS as usize);
    // Nothing a big transaction staged is lost by a copy written beside it.
    let state = keys_with_prefix(&db, "data:big:").len();
    assert_eq!(state, 0);
}

/// A transaction of more rows than one chunk: copies written before its batch, and none
/// of them becomes a change when it rolls back or fails.
#[test]
fn a_large_transaction_under_a_live_child() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE big (id INT PRIMARY KEY, v INT)").unwrap();
    const ROWS: i32 = 11_000;
    for start in (0..ROWS).step_by(1_000) {
        let values: Vec<String> = (start..start + 1_000).map(|id| format!("({id}, 0)")).collect();
        db.execute(&format!("INSERT INTO big VALUES {}", values.join(",")))
            .unwrap();
    }
    fork(&db, "r");
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE big SET v = 1").unwrap();
    db.execute("COMMIT").unwrap();
    assert!(rows_on(&db, "r", Family::Text, "SELECT id, v FROM big")
        .iter()
        .all(|row| int(row.values.get(1)) == 0));
    assert!(rows(&db, Family::Text, "SELECT id, v FROM big")
        .iter()
        .all(|row| int(row.values.get(1)) == 1));
    db.execute("MERGE BRANCH r INTO main").unwrap();
    assert!(rows(&db, Family::Text, "SELECT id, v FROM big")
        .iter()
        .all(|row| int(row.values.get(1)) == 1));
}

// ---------------------------------------------------------------------------
// Encryption
// ---------------------------------------------------------------------------

/// Encryption at rest on: a frozen copy is decoded for the key it came from and sealed
/// for the key it goes to, reads back, survives a reopen, and is not in the clear.
#[cfg(feature = "encryption")]
#[test]
fn frozen_copies_are_sealed_and_decode_on_the_branch() {
    const MARKER: &str = "QZX-B4-FROZEN-CANARY-7711";
    let dir = tempfile::tempdir().unwrap();
    let key_var = "NANO_B4_FORK_POINT_KEY";
    std::env::set_var(
        key_var,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    );
    let config = |path: &Path| {
        let mut config = Config::default();
        config.storage.path = Some(path.to_path_buf());
        config.storage.memory_only = false;
        config.encryption.enabled = true;
        config.encryption.key_source = heliosdb_nano::KeySource::Environment(key_var.to_string());
        config
    };
    let read = |db: &EmbeddedDatabase, branch: &str| -> Vec<(i64, Option<String>)> {
        let mut found: Vec<_> = rows_on(db, branch, Family::Text, "SELECT id, s FROM enc")
            .iter()
            .map(|row| (int(row.values.first()), text(row.values.get(1))))
            .collect();
        found.sort();
        found
    };
    let expected: Vec<(i64, Option<String>)> = vec![(1, Some(format!("{MARKER}-1"))), (2, Some(format!("{MARKER}-2")))];
    {
        let db = EmbeddedDatabase::with_config(config(dir.path())).unwrap();
        db.execute("CREATE TABLE enc (id INT PRIMARY KEY, s TEXT)").unwrap();
        db.execute(&format!("INSERT INTO enc VALUES (1, '{MARKER}-1'), (2, '{MARKER}-2')"))
            .unwrap();
        fork(&db, "r");
        db.execute("UPDATE enc SET s = 'moved' WHERE id = 1").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("DELETE FROM enc WHERE id = 2").unwrap();
        db.execute("COMMIT").unwrap();
        db.execute("INSERT INTO enc VALUES (3, 'later')").unwrap();
        assert_eq!(read(&db, "r"), expected);
        let r = branch_id(&db, "r");
        let raw = db.storage.db();
        for key in keys_with_prefix(&db, &format!("bdata:{r}:enc:")) {
            let stored = raw.get(&key).unwrap().unwrap();
            assert!(
                !String::from_utf8_lossy(&stored).contains(MARKER),
                "a frozen row in the clear"
            );
        }
    }
    let db = EmbeddedDatabase::with_config(config(dir.path())).unwrap();
    assert_eq!(read(&db, "r"), expected, "after a reopen");
    on(&db, "r", Family::Text, "UPDATE enc SET s = 'own' WHERE id = 2", &[]);
    assert_eq!(read(&db, "r")[1].1.as_deref(), Some("own"));
}

// ---------------------------------------------------------------------------
// The hook across a reopen
// ---------------------------------------------------------------------------

fn on_disk(path: &Path) -> EmbeddedDatabase {
    EmbeddedDatabase::new(path).unwrap()
}

/// The registry is rebuilt at open: the first write after one freezes like any other.
#[test]
fn the_hook_is_rebuilt_at_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = on_disk(dir.path());
        seed(&db);
        fork(&db, "r");
    }
    let db = on_disk(dir.path());
    db.execute("UPDATE t SET v = 11 WHERE id = 1").unwrap();
    db.execute("INSERT INTO t VALUES (4, 40)").unwrap();
    db.execute("ALTER TABLE t ADD COLUMN x INT DEFAULT 1").unwrap();
    let on_branch = rows_on(&db, "r", Family::Text, "SELECT * FROM t");
    assert!(on_branch.iter().all(|row| row.values.len() == 2));
    assert_eq!(pairs(&on_branch), vec![(1, 10), (2, 20), (3, 30)]);
}

/// A branch created by an earlier release has no fork record. It reads what its parent
/// held when this release first opened the store, and from then on it is a fork. MERGE
/// stays refused for any table it changed rows of. It has no record of what they were
/// written for.
#[test]
fn a_branch_with_no_fork_record() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = on_disk(dir.path());
        seed(&db);
        fork(&db, "old");
        fork(&db, "unchanged");
        for name in ["old", "unchanged"] {
            let id = branch_id(&db, name);
            db.storage.db().delete(format!("bmeta:{id}:fork")).unwrap();
        }
        // What an earlier release read: main live, until now.
        db.execute("UPDATE t SET v = 11 WHERE id = 1").unwrap();
        for name in ["old", "unchanged"] {
            let id = branch_id(&db, name);
            for family in ["bdata", "bfrz"] {
                for key in keys_with_prefix(&db, &format!("{family}:{id}:t:")) {
                    db.storage.db().delete(key).unwrap();
                }
            }
        }
    }
    let db = on_disk(dir.path());
    assert_eq!(
        pairs(&rows_on(&db, "old", Family::Text, "SELECT id, v FROM t WHERE id = 1")),
        vec![(1, 11)],
        "up to this open it read main live"
    );
    db.execute("UPDATE t SET v = 12 WHERE id = 1").unwrap();
    assert_eq!(
        pairs(&rows_on(&db, "old", Family::Text, "SELECT id, v FROM t WHERE id = 1")),
        vec![(1, 11)],
        "from here on it is a fork"
    );
    on(&db, "old", Family::Text, "UPDATE t SET v = 200 WHERE id = 2", &[]);
    let err = db.execute("MERGE BRANCH old INTO main").unwrap_err().to_string();
    assert!(
        err.contains(BRANCH_MERGE_SCHEMA_REFUSED_MARKER) && err.contains("fork point is not recorded"),
        "{err}"
    );
    db.execute("MERGE BRANCH unchanged INTO main")
        .expect("it changed no rows");
}

// ---------------------------------------------------------------------------
// Reclaim
// ---------------------------------------------------------------------------

/// After `DROP BRANCH` no key with the branch's prefixes remains, and another branch's are
/// all there, though ids `1` and `12` start alike. The name is free, the id is not.
#[test]
fn dropping_a_branch_reclaims_its_keys_and_only_its_keys() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    // Enough branches that one id is a decimal prefix of another's: 2 and 20.
    let names: Vec<String> = (0..20).map(|n| format!("n{n}")).collect();
    for name in &names {
        fork(&db, name);
        on(&db, name, Family::Text, "UPDATE t SET v = 99 WHERE id = 1", &[]);
        on(&db, name, Family::Text, "CREATE TABLE own (id INT PRIMARY KEY)", &[]);
        on(&db, name, Family::Text, "INSERT INTO own VALUES (1)", &[]);
    }
    db.execute("UPDATE t SET v = 21 WHERE id = 2").unwrap();
    let first = branch_id(&db, "n0");
    let longer: Vec<&String> = names
        .iter()
        .filter(|name| {
            let id = branch_id(&db, name);
            id != first && id.to_string().starts_with(&first.to_string())
        })
        .collect();
    assert!(!longer.is_empty(), "the test needs ids {first} and {first}x");
    let before: Vec<(String, usize)> = names
        .iter()
        .filter(|name| name.as_str() != "n0")
        .map(|name| (name.clone(), level_keys(&db, branch_id(&db, name))))
        .collect();

    assert!(level_keys(&db, first) > 0);
    db.execute("DROP BRANCH n0").unwrap();
    assert_eq!(level_keys(&db, first), 0);
    for (name, keys) in before {
        assert_eq!(level_keys(&db, branch_id(&db, &name)), keys, "{name} keeps its keys");
    }
    for name in longer {
        assert_eq!(
            pairs(&rows_on(&db, name, Family::Text, "SELECT id, v FROM t WHERE id = 1")),
            vec![(1, 99)]
        );
    }

    // The name is free; the new branch starts from now, under a new id.
    fork(&db, "n0");
    let again = branch_id(&db, "n0");
    assert_ne!(again, first);
    assert_eq!(
        pairs(&rows_on(&db, "n0", Family::Text, "SELECT id, v FROM t WHERE id = 2")),
        vec![(2, 21)]
    );
}

/// A finished merge reclaims its source, once a live descendant no longer needs it. An
/// `ABORT` puts it back, with nothing gone.
#[test]
fn a_merge_reclaims_its_source_and_an_abort_does_not() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    fork(&db, "r");
    on(&db, "r", Family::Text, "UPDATE t SET v = 100 WHERE id = 1", &[]);
    let r = branch_id(&db, "r");
    let held = level_keys(&db, r);

    db.storage.debug_fail_merge_at(MergeFailSite::Row, 0);
    db.execute("MERGE BRANCH r INTO main").expect_err("stopped");
    assert_eq!(
        level_keys(&db, r),
        held,
        "unfinished: its keys are what it finishes from"
    );
    db.execute("UPDATE t SET v = 21 WHERE id = 2").unwrap();
    db.execute("MERGE BRANCH r ABORT").unwrap();
    assert_eq!(
        pairs(&rows_on(&db, "r", Family::Text, "SELECT id, v FROM t")),
        vec![(1, 100), (2, 20), (3, 30)],
        "active again, with what it saw: main's write froze for it all along"
    );

    db.execute("MERGE BRANCH r INTO main").unwrap();
    assert_eq!(level_keys(&db, r), 0, "finished");
    assert_eq!(
        pairs(&rows(&db, Family::Text, "SELECT id, v FROM t")),
        vec![(1, 100), (2, 21), (3, 30)]
    );
    assert!(matches!(
        db.storage.get_branch("r").unwrap().state,
        BranchState::Merged { .. }
    ));
}

/// A reclaim stopped half way (`Reclaim` fails after its first batch), then the next open:
/// nothing of the branch is left. A source with an unfinished merge is left alone, and a
/// branch's keys that no live id owns (a creation that crashed) go too.
#[test]
fn a_reclaim_is_finished_by_the_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let (dropped, pending, orphan);
    {
        let db = on_disk(dir.path());
        seed(&db);
        db.execute("CREATE TABLE many (id INT PRIMARY KEY)").unwrap();
        let values: Vec<String> = (0..25_000).map(|id| format!("({id})")).collect();
        db.execute(&format!("INSERT INTO many VALUES {}", values.join(",")))
            .unwrap();
        fork(&db, "gone");
        fork(&db, "stopped");
        on(&db, "stopped", Family::Text, "UPDATE t SET v = 100 WHERE id = 1", &[]);
        db.execute("DROP TABLE many").unwrap();
        dropped = branch_id(&db, "gone");
        assert!(level_keys(&db, dropped) > 20_000);

        db.storage.debug_fail_merge_at(MergeFailSite::Reclaim, 0);
        db.execute("DROP BRANCH gone").unwrap();
        db.storage.debug_clear_merge_failpoint();
        let left = level_keys(&db, dropped);
        assert!(left > 0, "the drop is done, its keys are not all gone");
        assert!(db
            .storage
            .get_branch("gone")
            .is_ok_and(|meta| matches!(meta.state, BranchState::Dropped { .. })));

        db.storage.debug_fail_merge_at(MergeFailSite::Row, 0);
        db.execute("MERGE BRANCH stopped INTO main").expect_err("stopped");
        pending = branch_id(&db, "stopped");

        // A creation that crashed after reserving its id and before its metadata.
        orphan = pending + 50;
        db.storage.db().put(format!("bdata:{orphan}:t:1"), b"x").unwrap();
        db.storage.db().put(format!("bfrz:{orphan}:t:1"), b"").unwrap();
        db.storage.db().put(format!("bdel:{orphan}:t:2"), b"").unwrap();
    }
    // The open finishes what it can of the merge first: the fail point is gone, so it does.
    let db = on_disk(dir.path());
    assert_eq!(level_keys(&db, dropped), 0, "the reclaim was finished");
    assert_eq!(level_keys(&db, orphan), 0, "keys of no branch");
    assert_eq!(level_keys(&db, pending), 0, "the merge was finished, and then its keys");
    assert_eq!(
        pairs(&rows(&db, Family::Text, "SELECT id, v FROM t")),
        vec![(1, 100), (2, 20), (3, 30)]
    );
}

/// The open leaves a source whose unfinished merge it could not finish.
#[test]
fn the_open_keeps_the_keys_of_an_unfinished_merge() {
    let dir = tempfile::tempdir().unwrap();
    let pending;
    {
        let db = on_disk(dir.path());
        db.execute("CREATE TABLE k (id INT PRIMARY KEY, code INT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO k VALUES (1, 1)").unwrap();
        fork(&db, "stuck");
        on(&db, "stuck", Family::Text, "INSERT INTO k VALUES (2, 2)", &[]);
        db.storage.debug_fail_merge_at(MergeFailSite::Row, 0);
        db.execute("MERGE BRANCH stuck INTO main").expect_err("stopped");
        // Main takes the key while it is pending, so the finish at the open refuses.
        db.execute("INSERT INTO k VALUES (3, 2)").unwrap();
        pending = branch_id(&db, "stuck");
        assert!(level_keys(&db, pending) > 0);
    }
    let db = on_disk(dir.path());
    assert!(level_keys(&db, pending) > 0, "still what it would finish from");
    assert!(matches!(
        db.storage.get_branch("stuck").unwrap().state,
        BranchState::Merged { .. }
    ));
}

// ---------------------------------------------------------------------------
// The cost
// ---------------------------------------------------------------------------

/// With no live branch a write takes the gate's one load and never reaches the hook;
/// the first branch brings it in, and the last one takes it out again.
#[test]
fn no_branch_no_hook() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let runs = || db.storage.write_gate().debug_freeze_runs();
    let writes = |db: &EmbeddedDatabase| {
        db.execute("INSERT INTO c VALUES (1, 1)").unwrap();
        db.execute("UPDATE c SET v = 2 WHERE id = 1").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("UPDATE c SET v = 3 WHERE id = 1").unwrap();
        db.execute("COMMIT").unwrap();
        db.execute("DELETE FROM c WHERE id = 1").unwrap();
    };
    db.execute("CREATE TABLE c (id INT PRIMARY KEY, v INT)").unwrap();
    writes(&db);
    assert_eq!(runs(), 0, "a store that never had a branch");

    fork(&db, "r");
    let before = runs();
    writes(&db);
    assert!(runs() > before, "with a live child it is the funnel");

    db.execute("DROP BRANCH r").unwrap();
    let after = runs();
    writes(&db);
    assert_eq!(runs(), after, "and out again with the last one");

    fork(&db, "m");
    db.execute("MERGE BRANCH m INTO main").unwrap();
    let merged = runs();
    writes(&db);
    assert_eq!(runs(), merged, "a merged branch is written by nobody");
}

// ---------------------------------------------------------------------------
// The fork fence beside table locks (merged-tree review, M1 and M4)
// ---------------------------------------------------------------------------

/// A condition another thread will make true: wait on its state, never on time.
fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::yield_now();
    }
}

/// A `CREATE BRANCH` of main waits in the fork fence for a schema statement that waits
/// behind an open transaction. A migration whose transaction holds its first DDL's table
/// goes on with its second statement: it used to be refused 55006 at once, with the
/// first one landed and a rerun failing on the column it had added. A schema statement
/// holding nothing waits for the fork as for a lock, and `lock_timeout` ends it with
/// 55P03 and nothing done: it used to wait 30 s whatever the timeout, and then run.
#[test]
fn a_migration_goes_on_while_a_fork_waits_and_other_ddl_waits_like_a_lock() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    for table in ["m1", "big", "other"] {
        db.execute(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, v INT)"))
            .unwrap();
        db.execute(&format!("INSERT INTO {table} VALUES (1, 1)")).unwrap();
    }
    let holder = txn_session(&db, "holder");
    db.begin_transaction_for_session(holder).unwrap();
    db.execute_for_session(holder, "UPDATE big SET v = 2 WHERE id = 1")
        .unwrap();
    let migration = txn_session(&db, "migration");
    db.begin_transaction_for_session(migration).unwrap();
    db.execute_for_session(migration, "ALTER TABLE m1 ADD COLUMN c1 INT")
        .unwrap();

    std::thread::scope(|scope| {
        let blocked = scope.spawn(|| db.execute("ALTER TABLE big ADD COLUMN z INT"));
        wait_until("the ALTER of big to wait", || {
            db.storage.debug_table_lock_waiters("big").0 == 1
        });
        let forking = scope.spawn(|| db.execute("CREATE BRANCH f1 FROM main AS OF NOW"));
        wait_until("the fork to wait in the fence", || db.storage.debug_fork_fence_held());

        db.execute_for_session(migration, "ALTER TABLE m1 ADD COLUMN c2 INT")
            .expect("the migration's second statement goes on");

        db.storage
            .debug_set_table_lock_timeout(std::time::Duration::from_millis(100));
        let err = db
            .execute("ALTER TABLE other ADD COLUMN q INT")
            .expect_err("it waits for the fork as for a lock");
        assert!(
            err.to_string().contains(heliosdb_nano::TABLE_LOCK_TIMEOUT_MARKER),
            "{err}"
        );
        db.storage
            .debug_set_table_lock_timeout(std::time::Duration::from_secs(30));
        assert!(!forking.is_finished() && !blocked.is_finished());

        db.commit_transaction_for_session(migration).unwrap();
        db.rollback_transaction_for_session(holder).unwrap();
        blocked
            .join()
            .unwrap()
            .expect("big's ALTER runs once its holder is gone");
        forking.join().unwrap().expect("and then the fork");
    });
    let columns = |table: &str| db.storage.catalog().get_table_schema(table).unwrap().columns.len();
    assert_eq!(columns("m1"), 4, "both of the migration's columns");
    assert_eq!(columns("other"), 2, "the statement that gave up did nothing");
    assert_eq!(columns("big"), 3);
    assert_eq!(rows_on(&db, "f1", Family::Text, "SELECT * FROM m1")[0].values.len(), 4);
}

/// The merged-tree review's M12. A table's change of shape is timed by the clock, and
/// no snapshot record carries that time, while an open seeds the clock from snapshot
/// records: a store reopened (or promoted) with nothing written after a `RENAME` came
/// back with its clock below the rename's record, and an anchor taken after it, on a
/// table recreated under the old name, was refused as if from before the rename. The
/// renames of `z` move the clock with no snapshot record, so the gap is wide.
#[test]
fn an_anchor_after_a_rename_is_accepted_after_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = on_disk(dir.path());
        db.execute("CREATE TABLE big (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO big VALUES (1, 1), (2, 2)").unwrap();
        db.execute("COMMIT").unwrap();
        db.execute("CREATE TABLE z0 (id INT PRIMARY KEY)").unwrap();
        for step in 0..40 {
            db.execute(&format!("ALTER TABLE z{step} RENAME TO z{}", step + 1))
                .unwrap();
        }
        db.execute("ALTER TABLE big RENAME TO big2").unwrap();
    }
    let db = on_disk(dir.path());
    db.execute("CREATE TABLE big (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO big VALUES (1, 10)").unwrap();
    db.execute("COMMIT").unwrap();
    let anchor = last_scn(&db);
    db.execute(&format!("CREATE BRANCH after FROM main AS OF SCN {anchor}"))
        .expect("an anchor after the rename");
    assert_eq!(
        pairs(&rows_on(&db, "after", Family::Text, "SELECT id, v FROM big")),
        vec![(1, 10)]
    );
}
