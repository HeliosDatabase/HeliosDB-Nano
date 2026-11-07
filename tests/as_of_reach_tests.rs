//! `AS OF` reaches every snapshot inside retention, however many commits
//! followed it (4.42.0 pre-release review, item P2).
//!
//! The snapshot GC kept the newest ~1000 snapshots in memory, and deleted the
//! `snapshot:` records of the rest with them. On a busy store an `AS OF
//! TIMESTAMP` anchor a few seconds old, and a `CREATE BRANCH … AS OF` at it
//! (the documented "rewind to 09:00" recipe), failed `XX000 No snapshot at or
//! before …` after a few thousand explicit transactions. The records now stay
//! until they are past retention, and a lookup the in-memory map cannot answer
//! walks them. Both anchors here predate the in-memory window; each is checked
//! on both executor families, through a branch, and after a restart.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Value};
use std::time::Duration;

/// Explicit transactions after the first anchor: each registers a snapshot,
/// and the in-memory map keeps between 1000 and 2000 of them.
const COMMITS: i64 = 4000;
/// The second anchor's place: still well before the in-memory window.
const MIDWAY: i64 = 500;

/// A wall-clock second strictly between two runs of commits: `AS OF TIMESTAMP`
/// resolves to the second, so a whole second separates the anchor from the
/// commits on either side of it.
fn anchor_between_commits() -> String {
    std::thread::sleep(Duration::from_millis(1100));
    let anchor = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    std::thread::sleep(Duration::from_millis(1100));
    anchor
}

fn single_int(rows: &[heliosdb_nano::Tuple], what: &str) -> i64 {
    assert_eq!(rows.len(), 1, "{what}");
    match rows[0].values[0] {
        Value::Int4(v) => i64::from(v),
        Value::Int8(v) => v,
        ref other => panic!("{what}: integer expected, got {other:?}"),
    }
}

/// `SELECT sum(v)`, on the text family and on the params family, which must agree.
fn sum(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let text = single_int(&db.query(sql, &[]).unwrap_or_else(|e| panic!("query {sql}: {e}")), sql);
    let params = single_int(
        &db.query_params(sql, &[])
            .unwrap_or_else(|e| panic!("query_params {sql}: {e}")),
        sql,
    );
    assert_eq!(text, params, "{sql}: the executor families disagree");
    text
}

fn sum_on_branch(db: &EmbeddedDatabase, branch: &str) -> i64 {
    let rows = db
        .with_request_branch_scope(branch, || db.query("SELECT sum(v) FROM t", &[]))
        .unwrap_or_else(|e| panic!("on {branch}: {e}"));
    single_int(&rows, branch)
}

/// Main at both anchors, a branch forked at each, the branches forked before a
/// restart, and an anchor before any snapshot (refused as it always was).
fn check(db: &EmbeddedDatabase, anchors: &[(String, i64)], fork_prefix: &str, earlier_forks: &[(String, i64)]) {
    assert_eq!(sum(db, "SELECT sum(v) FROM t"), COMMITS, "{fork_prefix}: now");
    for (i, (anchor, expected)) in anchors.iter().enumerate() {
        let sql = format!("SELECT sum(v) FROM t AS OF TIMESTAMP '{anchor}'");
        assert_eq!(sum(db, &sql), *expected, "{fork_prefix}: {sql}");

        let branch = format!("{fork_prefix}{i}");
        db.execute(&format!("CREATE BRANCH {branch} FROM main AS OF TIMESTAMP '{anchor}'"))
            .unwrap_or_else(|e| panic!("{fork_prefix}: CREATE BRANCH {branch} AS OF '{anchor}': {e}"));
        assert_eq!(sum_on_branch(db, &branch), *expected, "{fork_prefix}: branch {branch}");
    }
    for (branch, expected) in earlier_forks {
        assert_eq!(sum_on_branch(db, branch), *expected, "{fork_prefix}: branch {branch}");
    }

    let before_everything = "SELECT sum(v) FROM t AS OF TIMESTAMP '2000-01-01 00:00:00'";
    for refused in [
        db.query(before_everything, &[]),
        db.query_params(before_everything, &[]),
    ] {
        let err = refused.map(|_| ()).expect_err(before_everything).to_string();
        assert!(err.contains("No snapshot at or before"), "{fork_prefix}: {err}");
    }
}

#[test]
fn as_of_and_an_anchored_branch_reach_past_thousands_of_commits() {
    let dir = tempfile::tempdir().unwrap();
    let anchors;
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("BEGIN").unwrap();
        let seed: Vec<String> = (1..=10).map(|id| format!("({id}, 0)")).collect();
        db.execute(&format!("INSERT INTO t VALUES {}", seed.join(", ")))
            .unwrap();
        db.execute("COMMIT").unwrap();

        let first = anchor_between_commits();
        let mut midway = String::new();
        for i in 0..COMMITS {
            if i == MIDWAY {
                midway = anchor_between_commits();
            }
            db.execute("BEGIN").unwrap();
            db.execute(&format!("UPDATE t SET v = v + 1 WHERE id = {}", i % 10 + 1))
                .unwrap();
            db.execute("COMMIT").unwrap();
        }
        assert!(
            db.storage.snapshot_manager().snapshot_count() < 2100,
            "the in-memory map stays bounded"
        );
        anchors = vec![(first, 0), (midway, MIDWAY)];
        check(&db, &anchors, "before_restart_", &[]);
    }

    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert!(
        db.storage.snapshot_manager().snapshot_count() < 2100,
        "recovery keeps the in-memory map bounded"
    );
    let forked_before: Vec<(String, i64)> = anchors
        .iter()
        .enumerate()
        .map(|(i, (_, expected))| (format!("before_restart_{i}"), *expected))
        .collect();
    check(&db, &anchors, "after_restart_", &forked_before);
}
