//! v10.1 — branch statements refuse what they do not understand.
//!
//! Before: `MERGE BRANCH dev INTO main CHECK` and `... DRY RUN` performed the
//! real merge, `DROP BRANCH b4 PLEASE` dropped b4, `WITH (bogus)` /
//! `WITH (replication_factor)` were skipped silently, `region = 'a=b'` was
//! skipped (split on every `=`), and `delete_branch_after = yes` merged and
//! kept the branch. Each now fails with a syntax error (42601 on the wire)
//! before anything changes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::{EmbeddedDatabase, Error, Value};

fn seeded() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'main')").unwrap();
    db.execute("CREATE BRANCH dev AS OF NOW").unwrap();
    db.execute("USE BRANCH dev").unwrap();
    db.execute("INSERT INTO t VALUES (2, 'dev')").unwrap();
    db.execute("USE BRANCH main").unwrap();
    db
}

fn main_ids(db: &EmbeddedDatabase) -> usize {
    db.query("SELECT id FROM t", &[]).unwrap().len()
}

fn branch_names(db: &EmbeddedDatabase) -> Vec<String> {
    db.query("SHOW BRANCHES", &[])
        .unwrap()
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn assert_syntax_error(result: heliosdb_nano::Result<u64>, what: &str) {
    match result {
        Err(Error::SqlParse(_)) => {}
        other => panic!("{what}: expected a syntax error, got {other:?}"),
    }
}

#[test]
fn merge_with_trailing_word_is_refused_and_merges_nothing() {
    for sql in [
        "MERGE BRANCH dev INTO main CHECK",
        "MERGE BRANCH dev INTO main DRY RUN;",
        "MERGE BRANCH dev INTO main WITH (conflict_resolution = 'branch_wins') NOW",
    ] {
        let db = seeded();
        assert_syntax_error(db.execute(sql), sql);
        assert_eq!(main_ids(&db), 1, "{sql}: nothing may be merged");
        // The parameter family refuses it too.
        assert_syntax_error(db.execute_params(sql, &[]), sql);
        assert_eq!(main_ids(&db), 1, "{sql}");
    }
}

#[test]
fn drop_with_trailing_word_is_refused() {
    let db = seeded();
    db.execute("CREATE BRANCH b4 AS OF NOW").unwrap();
    assert_syntax_error(db.execute("DROP BRANCH b4 PLEASE"), "DROP BRANCH b4 PLEASE");
    assert_syntax_error(
        db.execute("DROP DATABASE BRANCH IF EXISTS b4 now;"),
        "DROP DATABASE BRANCH IF EXISTS b4 now",
    );
    assert!(branch_names(&db).contains(&"b4".to_string()), "b4 must survive");
}

#[test]
fn with_option_without_value_is_refused() {
    let db = seeded();
    for sql in [
        "CREATE BRANCH x1 FROM main WITH (replication_factor)",
        "CREATE BRANCH x2 FROM main WITH (bogus)",
        "CREATE BRANCH x3 FROM main WITH (region = )",
    ] {
        assert_syntax_error(db.execute(sql), sql);
    }
    assert!(!branch_names(&db).iter().any(|n| n.starts_with('x')));
    for sql in [
        "MERGE BRANCH dev INTO main WITH (delete_branch_after)",
        "MERGE BRANCH dev INTO main WITH ()",
    ] {
        assert_syntax_error(db.execute(sql), sql);
        assert_eq!(main_ids(&db), 1, "{sql}");
    }
}

#[test]
fn with_unknown_option_is_refused() {
    let db = seeded();
    assert!(db.execute("CREATE BRANCH y FROM main WITH (colour = 'red')").is_err());
    assert!(db.execute("MERGE BRANCH dev INTO main WITH (dry_run = true)").is_err());
    assert_eq!(main_ids(&db), 1);
}

#[test]
fn quoted_value_may_contain_equals_and_comma() {
    let db = seeded();
    db.execute("CREATE BRANCH q FROM main WITH (region = 'a=b,c')")
        .expect("a quoted value may hold = and ,");
    assert!(branch_names(&db).contains(&"q".to_string()));
}

#[test]
fn delete_branch_after_rejects_non_boolean() {
    let db = seeded();
    assert_syntax_error(
        db.execute("MERGE BRANCH dev INTO main WITH (delete_branch_after = yes)"),
        "delete_branch_after = yes",
    );
    assert_eq!(main_ids(&db), 1);
    assert!(branch_names(&db).contains(&"dev".to_string()));
}

#[test]
fn trailing_semicolon_and_comment_still_accepted() {
    let db = seeded();
    db.execute("CREATE BRANCH c1 AS OF NOW").unwrap();
    db.execute("DROP BRANCH c1; -- done").expect("drop with ; and comment");
    db.execute("MERGE BRANCH dev INTO main /* go */ ;")
        .expect("merge with comment");
    assert_eq!(main_ids(&db), 2);
}

#[test]
fn merge_abort_unchanged() {
    let db = seeded();
    // No unfinished merge: ABORT answers with its own message, not a parse error.
    let result = db.execute("MERGE BRANCH dev ABORT");
    assert!(!matches!(result, Err(Error::SqlParse(_))), "{result:?}");
}
