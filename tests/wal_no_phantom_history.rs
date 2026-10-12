//! Append-before-commit hole (4.41.1 replication): the logical WAL feeds standbys and
//! crash replay, so it must describe only data that actually committed.
//!
//! Before the fix, an autocommit statement logged each row while it ran and the entry
//! was durable and broadcast immediately; if the statement then failed (a later row
//! violated a constraint) its transaction rolled back but the entries stayed, so a
//! standby received rows the primary never had.

use heliosdb_nano::storage::LogicalCatchup;
use heliosdb_nano::{Config, EmbeddedDatabase};

fn row_entries(db: &EmbeddedDatabase) -> Vec<(u64, String)> {
    match db.storage.logical_wal_catchup(0).expect("catch-up read") {
        LogicalCatchup::Entries { entries, .. } => entries
            .into_iter()
            .filter_map(|entry| {
                let text = format!("{:?}", entry.operation);
                // Row changes only (not DDL or counters), for table `t`.
                let row_change = ["Insert", "Update", "Delete"].iter().any(|kind| text.starts_with(kind));
                (row_change && text.contains("table: \"t\"")).then_some((entry.lsn, text))
            })
            .collect(),
        other => panic!("expected entries, got {other:?}"),
    }
}

/// Strict logical WAL: every fast-path statement logs while it runs, which is the
/// mode (and the shape of an HA primary) where the hole existed.
fn database() -> ((), EmbeddedDatabase) {
    let mut config = Config::in_memory();
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = true;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    assert!(db.storage.is_wal_enabled(), "this test needs the logical WAL");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    ((), db)
}

#[test]
fn failed_multi_row_insert_leaves_no_history() {
    let (_dir, db) = database();
    db.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    let committed = row_entries(&db);
    assert!(!committed.is_empty(), "a committed insert must be in the history");

    // Rows 2 and 3 are fine; the third value collides with row 1, so the statement fails.
    assert!(db.execute("INSERT INTO t VALUES (2, 2), (3, 3), (1, 9)").is_err());
    assert_eq!(
        db.query("SELECT id FROM t", &[]).unwrap().len(),
        1,
        "the statement must have rolled back"
    );
    assert_eq!(
        row_entries(&db),
        committed,
        "a rolled-back statement must not leave replicated history behind"
    );

    // And the history still follows real commits.
    db.execute("INSERT INTO t VALUES (4, 4)").unwrap();
    assert!(row_entries(&db).len() > committed.len());
}

#[test]
fn failed_multi_row_update_leaves_no_history() {
    let (_dir, db) = database();
    db.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    db.execute("INSERT INTO t VALUES (2, 2)").unwrap();
    let committed = row_entries(&db);

    // Setting every id to 5 succeeds for the first row and collides on the second.
    assert!(db.execute("UPDATE t SET id = 5").is_err());
    assert_eq!(
        row_entries(&db),
        committed,
        "a rolled-back UPDATE must not leave replicated history behind"
    );
}

#[test]
fn committed_statements_are_in_the_history_in_lsn_order() {
    let (_dir, db) = database();
    for id in 1..=5 {
        db.execute(&format!("INSERT INTO t VALUES ({id}, {id})")).unwrap();
    }
    db.execute("UPDATE t SET v = 99 WHERE id = 3").unwrap();
    db.execute("DELETE FROM t WHERE id = 5").unwrap();
    let entries = row_entries(&db);
    assert!(entries.len() >= 7, "5 inserts + update + delete, got {}", entries.len());
    assert!(entries.windows(2).all(|pair| pair[0].0 < pair[1].0), "LSN order");
    match db.storage.logical_wal_catchup(0).unwrap() {
        LogicalCatchup::Entries { latest, entries } => {
            let last = entries.last().map(|e| e.lsn).expect("entries");
            assert!(
                latest >= last,
                "the watermark ({latest}) must cover every entry ({last})"
            );
        }
        other => panic!("{other:?}"),
    }
}

fn ddl_entries(db: &EmbeddedDatabase) -> Vec<String> {
    match db.storage.logical_wal_catchup(0).expect("catch-up read") {
        LogicalCatchup::Entries { entries, .. } => entries
            .into_iter()
            .map(|e| format!("{:?}", e.operation))
            .filter(|t| t.starts_with("CreateTable") || t.starts_with("DropTable"))
            .collect(),
        other => panic!("expected entries, got {other:?}"),
    }
}

/// DDL issued over a session (the wire path) reaches the history through the commit
/// batch, in order, exactly once.
#[test]
fn session_ddl_is_in_the_history() {
    use heliosdb_nano::session::IsolationLevel;
    let (_d, db) = database();
    let before = ddl_entries(&db).len();
    let s = db.create_session("u", IsolationLevel::ReadCommitted).unwrap();
    db.execute_in_session(s, "CREATE TABLE sess_a (id INT PRIMARY KEY)")
        .unwrap();
    db.execute_in_session(s, "DROP TABLE sess_a").unwrap();
    let after = ddl_entries(&db);
    assert_eq!(after.len(), before + 2, "{after:?}");
    assert!(after[before].contains("sess_a") && after[before].starts_with("CreateTable"));
    assert!(after[before + 1].contains("sess_a") && after[before + 1].starts_with("DropTable"));
}
