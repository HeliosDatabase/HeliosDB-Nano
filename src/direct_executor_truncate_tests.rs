//! Exercise the direct executor with the actual outer transaction. The public
//! EXPLAIN ANALYZE TRUNCATE refusal remains covered and unchanged.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use crate::sql::{Executor, LogicalPlan};
use std::path::Path;

fn open(path: &Path, strict: bool) -> EmbeddedDatabase {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.logical_wal_per_statement = strict;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    EmbeddedDatabase::with_config(config).unwrap()
}
fn seed(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, tag TEXT, body TEXT, v INT)")
        .unwrap();
    db.execute("INSERT INTO src VALUES (1, 'old', 'body1', 8), (2, 'old', 'body2', 16)")
        .unwrap();
}
fn rows(db: &EmbeddedDatabase, table: &str) -> Vec<Vec<Value>> {
    let mut rows: Vec<_> = db
        .query(&format!("SELECT * FROM {table}"), &[])
        .unwrap()
        .into_iter()
        .map(|tuple| tuple.values)
        .collect();
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}
fn run(db: &EmbeddedDatabase, params: bool, sql: &str) {
    if params {
        db.execute_params(sql, &[]).unwrap();
    } else {
        db.execute(sql).unwrap();
    }
}
fn direct_truncate(db: &EmbeddedDatabase) {
    let current = db.current_transaction.lock();
    let txn = current.as_ref().expect("BEGIN installed the outer transaction");
    Executor::with_storage(db.storage.as_ref())
        .with_transaction(txn)
        .execute(&LogicalPlan::Truncate {
            table_name: "src".into(),
        })
        .unwrap();
}

#[test]
fn direct_executor_truncate_savepoint_rollback_and_later_reinsert_keep_the_right_keys() {
    for params in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path(), false);
        seed(&db);
        let original = rows(&db, "src");
        run(&db, params, "BEGIN");
        run(&db, params, "SAVEPOINT before_truncate");
        direct_truncate(&db);
        run(
            &db,
            params,
            "INSERT INTO src VALUES (1, 'replacement', 'replacement', 99)",
        );
        run(&db, params, "ROLLBACK TO SAVEPOINT before_truncate");
        assert_eq!(rows(&db, "src"), original);
        run(&db, params, "COMMIT");
        assert_eq!(rows(&db, "src"), original);
        assert!(db.execute("INSERT INTO src VALUES (1, 'duplicate', '', 0)").is_err());
        run(&db, params, "BEGIN");
        direct_truncate(&db);
        run(
            &db,
            params,
            "INSERT INTO src VALUES (1, 'replacement', 'replacement', 99)",
        );
        run(&db, params, "COMMIT");
        assert_eq!(
            rows(&db, "src"),
            vec![vec![
                Value::Int4(1),
                Value::String("replacement".into()),
                Value::String("replacement".into()),
                Value::Int4(99)
            ]]
        );
        assert_eq!(db.query("SELECT id FROM src WHERE id = 1", &[]).unwrap().len(), 1);
        assert!(db.execute("INSERT INTO src VALUES (1, 'duplicate', '', 0)").is_err());
    }
}

#[test]
fn truncate_rollback_reconciles_current_schema_missing_values_and_all_side_storage() {
    for params in [false, true] {
        for savepoint in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = open(dir.path(), false);
            seed(&db);
            db.execute("ALTER TABLE src ALTER COLUMN tag SET STORAGE DICTIONARY")
                .unwrap();
            db.execute("ALTER TABLE src ALTER COLUMN body SET STORAGE CONTENT_ADDRESSED")
                .unwrap();
            db.execute("ALTER TABLE src ALTER COLUMN v SET STORAGE COLUMNAR")
                .unwrap();
            let original = rows(&db, "src");
            run(&db, params, "BEGIN");
            if savepoint {
                run(&db, params, "SAVEPOINT before_truncate");
            }
            direct_truncate(&db);
            run(&db, params, "ALTER TABLE src ADD COLUMN retained INT DEFAULT 7");
            if savepoint {
                run(&db, params, "ROLLBACK TO SAVEPOINT before_truncate");
                run(&db, params, "COMMIT");
            } else {
                run(&db, params, "ROLLBACK");
            }
            let expected: Vec<_> = original
                .into_iter()
                .map(|mut row| {
                    row.push(Value::Int4(7));
                    row
                })
                .collect();
            assert_eq!(rows(&db, "src"), expected);
            assert_eq!(
                db.storage.catalog().get_table_schema("src").unwrap().columns.len(),
                5,
                "existing immediate DDL persists across DML rollback"
            );
            assert_eq!(db.query("SELECT id FROM src WHERE id = 1", &[]).unwrap().len(), 1);
            assert!(db
                .execute("INSERT INTO src VALUES (1, 'duplicate', '', 0, 7)")
                .unwrap_err()
                .to_string()
                .to_ascii_lowercase()
                .contains("duplicate"));
        }
    }
}

#[test]
fn materialized_view_plan_tracks_backing_layout_and_decodes_current_storage() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM src").unwrap();
    let expected = rows(&db, "src");
    for mode in ["DICTIONARY", "CONTENT_ADDRESSED", "COLUMNAR", "DEFAULT"] {
        let (pinned, _) = db.wire_parameterized_plan("SELECT * FROM mv").unwrap();
        let tables = pinned.layout.tables();
        assert!(tables.iter().any(|name| name == "mv"));
        assert!(tables.iter().any(|name| name == "__mv_mv"));
        assert!(pinned.layout.is_current());
        db.execute(&format!("ALTER TABLE __mv_mv ALTER COLUMN tag SET STORAGE {mode}"))
            .unwrap();
        assert!(
            !pinned.layout.is_current(),
            "backing storage ALTER invalidates the MV plan"
        );
        assert_eq!(rows(&db, "mv"), expected, "{mode}");
        let actual: Vec<_> = db
            .query_params("SELECT * FROM mv ORDER BY id", &[])
            .unwrap()
            .into_iter()
            .map(|row| row.values)
            .collect();
        assert_eq!(actual, expected, "params {mode}");
        for sql in [
            "SELECT mv.tag FROM mv WHERE id = 1",
            "SELECT tag FROM mv WHERE tag = 'old'",
        ] {
            let actual = db.query(sql, &[]).unwrap();
            assert!(!actual.is_empty(), "{mode}: {sql}");
            assert!(actual.iter().all(|row| row.values == vec![Value::String("old".into())]));
        }
    }
}
