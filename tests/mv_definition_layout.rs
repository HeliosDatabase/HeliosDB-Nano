//! Refresh rebinds physical scans while preserving the declared MV output.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::{Config, EmbeddedDatabase, Value};
fn exec(db: &EmbeddedDatabase, params: bool, sql: &str) {
    if params {
        db.execute_params(sql, &[]).unwrap();
    } else {
        db.execute(sql).unwrap();
    }
}
fn values(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &[])
        .unwrap()
        .into_iter()
        .map(|tuple| tuple.values)
        .collect()
}
fn database(history: bool) -> (tempfile::TempDir, EmbeddedDatabase) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.storage.path = Some(dir.path().to_path_buf());
    config.storage.time_travel_enabled = history;
    config.storage.version_gc_interval_secs = Some(0);
    let db = EmbeddedDatabase::with_config(config).unwrap();
    (dir, db)
}
#[test]
fn refresh_after_source_add_keeps_star_definition_and_reads_new_values() {
    for params in [false, true] {
        for history in [false, true] {
            let (_dir, db) = database(history);
            exec(&db, params, "CREATE TABLE src (id INT PRIMARY KEY, label TEXT)");
            exec(&db, params, "INSERT INTO src VALUES (1,'old'),(2,'two')");
            exec(&db, params, "CREATE MATERIALIZED VIEW m AS SELECT * FROM src");
            exec(&db, params, "CREATE INDEX m_label ON __mv_m (label)");
            exec(&db, params, "ALTER TABLE src ADD COLUMN extra INT DEFAULT 7");
            exec(&db, params, "UPDATE src SET label='new' WHERE id=1");
            for refresh in [
                "REFRESH MATERIALIZED VIEW m",
                "REFRESH MATERIALIZED VIEW CONCURRENTLY m",
            ] {
                exec(&db, params, refresh);
                assert_eq!(
                    values(&db, "SELECT * FROM m ORDER BY id"),
                    vec![
                        vec![Value::Int4(1), Value::String("new".into())],
                        vec![Value::Int4(2), Value::String("two".into())]
                    ]
                );
                assert_eq!(
                    db.storage.catalog().get_table_schema("__mv_m").unwrap().columns.len(),
                    2
                );
                assert!(db.storage.art_indexes().index_exists("m_label"));
                assert_eq!(
                    values(&db, "SELECT id FROM m WHERE label='new'"),
                    vec![vec![Value::Int4(1)]]
                );
            }
        }
    }
}
#[test]
fn refresh_uses_current_source_storage_modes_and_preserves_declared_projection() {
    for params in [false, true] {
        for history in [false, true] {
            let (_dir, db) = database(history);
            exec(&db, params, "CREATE TABLE src (id INT PRIMARY KEY, label TEXT)");
            exec(&db, params, "INSERT INTO src VALUES (1,'one'),(2,'two')");
            exec(
                &db,
                params,
                "CREATE MATERIALIZED VIEW m AS SELECT id,label FROM src WHERE id>=1",
            );
            for mode in ["DICTIONARY", "COLUMNAR", "DEFAULT"] {
                exec(
                    &db,
                    params,
                    &format!("ALTER TABLE src ALTER COLUMN label SET STORAGE {mode}"),
                );
                exec(&db, params, "UPDATE src SET label='changed' WHERE id=1");
                exec(&db, params, "REFRESH MATERIALIZED VIEW CONCURRENTLY m");
                assert_eq!(
                    values(&db, "SELECT * FROM m ORDER BY id"),
                    vec![
                        vec![Value::Int4(1), Value::String("changed".into())],
                        vec![Value::Int4(2), Value::String("two".into())]
                    ]
                );
            }
        }
    }
}
#[test]
fn nested_definition_scans_rebind_after_adds_without_expanding_their_outputs() {
    for params in [false, true] {
        let (_dir, db) = database(true);
        exec(&db, params, "CREATE TABLE src (id INT PRIMARY KEY, v INT)");
        exec(&db, params, "INSERT INTO src VALUES (1,10),(2,20)");
        exec(
            &db,
            params,
            "CREATE MATERIALIZED VIEW agg AS SELECT SUM(v) AS total FROM src",
        );
        exec(
            &db,
            params,
            "CREATE MATERIALIZED VIEW nested AS SELECT id,(SELECT SUM(v) FROM src) AS total FROM src WHERE id=1",
        );
        exec(
            &db,
            params,
            "CREATE MATERIALIZED VIEW cte_mv AS WITH q AS (SELECT * FROM src) SELECT id,v FROM q WHERE id=1",
        );
        exec(
            &db,
            params,
            "CREATE MATERIALIZED VIEW cte_star AS WITH q AS (SELECT * FROM src) SELECT * FROM q WHERE id=1",
        );
        exec(
            &db,
            params,
            "CREATE MATERIALIZED VIEW cte_shadow AS WITH src AS (SELECT * FROM src) SELECT * FROM src WHERE id=1",
        );
        exec(&db, params, "CREATE MATERIALIZED VIEW cte_later AS WITH q AS (SELECT * FROM src), src AS (SELECT id,v FROM q) SELECT * FROM src WHERE id=1");
        exec(&db, params, "ALTER TABLE src ADD COLUMN extra INT DEFAULT 7");
        exec(&db, params, "UPDATE src SET v=30 WHERE id=1");
        for name in ["agg", "nested", "cte_mv", "cte_star", "cte_shadow", "cte_later"] {
            exec(&db, params, &format!("REFRESH MATERIALIZED VIEW {name}"));
        }
        let int = |v: &Value| match v {
            Value::Int4(v) => i64::from(*v),
            Value::Int8(v) => *v,
            other => panic!("{other:?}"),
        };
        assert_eq!(int(&values(&db, "SELECT total FROM agg")[0][0]), 50);
        let nested = values(&db, "SELECT id,total FROM nested");
        assert_eq!((int(&nested[0][0]), int(&nested[0][1])), (1, 50));
        assert_eq!(
            values(&db, "SELECT * FROM cte_mv"),
            vec![vec![Value::Int4(1), Value::Int4(30)]]
        );
        assert_eq!(
            values(&db, "SELECT * FROM cte_star"),
            vec![vec![Value::Int4(1), Value::Int4(30)]]
        );
        for name in ["cte_shadow", "cte_later"] {
            assert_eq!(
                values(&db, &format!("SELECT * FROM {name}")),
                vec![vec![Value::Int4(1), Value::Int4(30)]]
            );
        }
    }
}
#[test]
fn removed_definition_columns_refuse_before_replacing_materialized_rows() {
    for params in [false, true] {
        let (_dir, db) = database(true);
        exec(&db, params, "CREATE TABLE src (id INT PRIMARY KEY, v INT)");
        exec(&db, params, "INSERT INTO src VALUES (1,10)");
        exec(&db, params, "CREATE MATERIALIZED VIEW m AS SELECT * FROM src");
        let before = values(&db, "SELECT * FROM m");
        exec(&db, params, "ALTER TABLE src DROP COLUMN v");
        let sequence = db.storage.db().latest_sequence_number();
        let error = if params {
            db.execute_params("REFRESH MATERIALIZED VIEW m", &[])
        } else {
            db.execute("REFRESH MATERIALIZED VIEW m")
        }
        .unwrap_err();
        assert!(error.to_string().contains("definition depends on columns"), "{error}");
        assert_eq!(db.storage.db().latest_sequence_number(), sequence);
        assert_eq!(values(&db, "SELECT * FROM m"), before);
    }
}
