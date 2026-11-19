//! v10.2 — branch listings stop giving false answers.
//!
//! Before: `pg_branch_stats()` reported modified_keys / storage_bytes /
//! commit_count as 0 for a branch that changed rows (nothing maintains those
//! counters), every listing printed `created_at` — a logical counter — as a
//! 1970 date, and `pg_compare_branches` could only fail with a message
//! suggesting a call that also failed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn db_with_changed_branch() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE BRANCH dev AS OF NOW").unwrap();
    db.execute("USE BRANCH dev").unwrap();
    db.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    db.execute("USE BRANCH main").unwrap();
    db
}

#[test]
fn pg_branch_stats_reports_changed_rows_or_null() {
    let db = db_with_changed_branch();
    let rows = db.query("SELECT * FROM pg_branch_stats()", &[]).unwrap();
    let dev = rows
        .iter()
        .find(|r| r.values.first() == Some(&Value::String("dev".into())))
        .expect("dev listed");
    for (i, v) in dev.values.iter().enumerate().skip(1).take(3) {
        assert!(
            !matches!(v, Value::Int8(0) | Value::Int4(0)),
            "column {i}: an unmeasured counter must be NULL, not 0: {:?}",
            dev.values
        );
    }
    // Through the parameter family too.
    let rows = db.query_params("SELECT * FROM pg_branch_stats()", &[]).unwrap();
    assert!(!rows.is_empty());
}

#[test]
fn pg_compare_branches_is_gone_or_names_its_replacement() {
    let db = db_with_changed_branch();
    let err = db
        .query("SELECT * FROM pg_compare_branches()", &[])
        .expect_err("pg_compare_branches compares nothing");
    assert!(err.to_string().contains("not implemented"), "{err}");
}

#[test]
fn listings_show_wall_clock_creation_time() {
    let db = db_with_changed_branch();
    let before = chrono::Utc::now() - chrono::Duration::minutes(5);
    for sql in ["SHOW BRANCHES", "SELECT * FROM pg_database_branches()"] {
        let rows = db.query(sql, &[]).unwrap();
        let dev = rows
            .iter()
            .find(|r| r.values.first() == Some(&Value::String("dev".into())))
            .unwrap_or_else(|| panic!("{sql}: dev not listed"));
        let created = dev
            .values
            .iter()
            .find_map(|v| match v {
                Value::Timestamp(t) => Some(*t),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{sql}: no timestamp in {:?}", dev.values));
        assert!(created > before, "{sql}: created_at {created} is not wall-clock");
    }
}
