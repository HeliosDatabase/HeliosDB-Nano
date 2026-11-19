//! v10.7 — branches that expire: `expires_in`, `label`, and reaping.
//!
//! Before: `CREATE BRANCH b WITH (expires_in = '2h', label = 'agent-42')`
//! answered "Unknown branch option: expires_in" and nothing ever reaped a
//! forgotten branch.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::{EmbeddedDatabase, Error, Value};
use std::time::Duration;

fn names(db: &EmbeddedDatabase) -> Vec<String> {
    db.query("SHOW BRANCHES", &[])
        .unwrap()
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn base() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    db
}

#[test]
fn malformed_expires_in_is_refused() {
    let db = base();
    for value in ["0s", "-1h", "abc", "2x", "h", ""] {
        let sql = format!("CREATE BRANCH e FROM main WITH (expires_in = '{value}')");
        match db.execute(&sql) {
            Err(Error::SqlParse(_)) => {}
            other => panic!("{sql}: {other:?}"),
        }
    }
    assert!(!names(&db).contains(&"e".to_string()));
}

#[test]
fn no_option_means_never() {
    let db = base();
    db.execute("CREATE BRANCH forever FROM main").unwrap();
    let rows = db
        .query(
            "SELECT expires_at, label FROM pg_database_branches() WHERE branch_name = 'forever'",
            &[],
        )
        .unwrap();
    assert_eq!(rows[0].values, vec![Value::Null, Value::Null]);
}

#[test]
fn label_and_expiry_are_listed() {
    let db = base();
    db.execute("CREATE BRANCH tagged FROM main WITH (expires_in = '2h', label = 'agent-42')")
        .unwrap();
    let rows = db
        .query_params(
            "SELECT expires_at, label FROM pg_database_branches() WHERE branch_name = $1",
            &[Value::String("tagged".into())],
        )
        .unwrap();
    match &rows[0].values[0] {
        Value::Timestamp(t) => {
            let left = *t - chrono::Utc::now();
            assert!(left.num_minutes() > 110 && left.num_minutes() <= 120, "{t}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rows[0].values[1], Value::String("agent-42".into()));
}

#[test]
fn expired_branch_refuses_new_statements() {
    let db = base();
    db.execute("CREATE BRANCH short FROM main WITH (expires_in = '1s')")
        .unwrap();
    db.execute("USE BRANCH short").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    let err = db.query("SELECT * FROM t", &[]).expect_err("expired branch");
    assert!(err.to_string().contains("expired"), "{err}");
    // Binding it again is refused too.
    db.execute("USE BRANCH main").unwrap();
    let err = db
        .execute("USE BRANCH short")
        .expect_err("cannot bind an expired branch");
    assert!(err.to_string().contains("expired"), "{err}");
}

#[test]
fn reaped_after_next_reap() {
    let db = base();
    db.execute("CREATE BRANCH gone_soon FROM main WITH (expires_in = '1s')")
        .unwrap();
    assert!(names(&db).contains(&"gone_soon".to_string()));
    std::thread::sleep(Duration::from_millis(1200));
    // The next CREATE BRANCH reaps.
    db.execute("CREATE BRANCH trigger_b FROM main").unwrap();
    let listed = names(&db);
    assert!(!listed.contains(&"gone_soon".to_string()), "{listed:?}");
    assert!(listed.contains(&"trigger_b".to_string()));
}

#[test]
fn expired_branch_with_live_child_is_kept_and_reported() {
    let db = base();
    db.execute("CREATE BRANCH parent_b FROM main WITH (expires_in = '1s')")
        .unwrap();
    db.execute("CREATE BRANCH child_b FROM parent_b").unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    db.execute("CREATE BRANCH trigger2 FROM main").unwrap();
    let listed = names(&db);
    assert!(
        listed.contains(&"parent_b".to_string()),
        "a parent with a child is kept: {listed:?}"
    );
    assert!(listed.contains(&"child_b".to_string()));
}

#[test]
fn embedded_needs_no_background_thread() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("CREATE BRANCH while_closed FROM main WITH (expires_in = '1s')")
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(1200));
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert!(!names(&db).contains(&"while_closed".to_string()), "reaped at open");
}

#[test]
fn clock_before_creation_stamp_reaps_nothing() {
    let mut meta_options = heliosdb_nano::storage::BranchOptions::default();
    meta_options
        .metadata
        .insert("created_at_unix_ms".into(), "2000000000000".into());
    meta_options.metadata.insert("expires_at_unix_ms".into(), "1000".into());
    let meta = heliosdb_nano::storage::BranchMetadata {
        name: "b".into(),
        branch_id: 2,
        parent_id: Some(1),
        created_at: 0,
        created_from_snapshot: 0,
        state: heliosdb_nano::storage::BranchState::Active,
        merge_base: None,
        options: meta_options,
        stats: Default::default(),
    };
    // Now (2026) is earlier than the creation stamp (2033): never expired.
    assert!(!meta.is_expired_at(chrono::Utc::now().timestamp_millis()));
    assert!(meta.is_expired_at(2_000_000_000_001));
}
