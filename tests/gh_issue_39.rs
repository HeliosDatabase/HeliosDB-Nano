//! Timestamp defaults must store text values, not quoted SQL literals.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

#[test]
fn invalid_temporal_default_conversion_fails_without_inserting_null() {
    for parameterized in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().expect("database");
        db.execute("CREATE TABLE invalid_default (id INT PRIMARY KEY, ts TIMESTAMP DEFAULT CURRENT_TIME)")
            .expect("create");
        let result = if parameterized {
            db.execute_params("INSERT INTO invalid_default (id) VALUES ($1)", &[Value::Int4(1)])
        } else {
            db.execute("INSERT INTO invalid_default (id) VALUES (1)")
        };
        assert!(
            result.is_err(),
            "invalid default silently accepted: bound={parameterized}"
        );
        assert!(db.query("SELECT * FROM invalid_default", &[]).expect("read").is_empty());
    }
}

#[test]
fn temporal_casts_have_exact_unquoted_text_and_preserve_timestamp_values() {
    let db = EmbeddedDatabase::new_in_memory().expect("database");
    let timestamp = chrono::DateTime::parse_from_rfc3339("2026-09-23T12:34:56.123456789Z")
        .expect("timestamp")
        .with_timezone(&chrono::Utc);
    for (value, expected) in [
        (Value::Timestamp(timestamp), "2026-09-23 12:34:56.123456+00"),
        (Value::Date(timestamp.date_naive()), "2026-09-23"),
        (Value::Time(timestamp.time()), "12:34:56.123456"),
    ] {
        for target in ["TEXT", "VARCHAR(80)"] {
            let rows = db
                .query_params(&format!("SELECT CAST($1 AS {target})"), &[value.clone()])
                .expect("cast");
            assert_eq!(rows[0].values[0], Value::String(expected.into()));
        }
    }
    let rows = db
        .query_params("SELECT CAST($1 AS TIMESTAMP)", &[Value::Timestamp(timestamp)])
        .expect("typed cast");
    assert_eq!(rows[0].values[0], Value::Timestamp(timestamp));
}

#[test]
fn timestamp_defaults_remain_typed_and_text_defaults_survive_reopen() {
    let dir = tempfile::tempdir().expect("directory");
    let stored;
    {
        let db = EmbeddedDatabase::new(dir.path()).expect("database");
        db.execute("CREATE TABLE temporal_persist (id INT PRIMARY KEY, txt TEXT DEFAULT CURRENT_TIMESTAMP, ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP)")
            .expect("create");
        db.execute("INSERT INTO temporal_persist (id) VALUES (1)")
            .expect("insert");
        stored = db.query("SELECT txt, ts FROM temporal_persist", &[]).expect("read");
        assert!(matches!(&stored[0].values[0], Value::String(s) if !s.contains('\'')));
        assert!(matches!(&stored[0].values[1], Value::Timestamp(_)));
    }
    let db = EmbeddedDatabase::new(dir.path()).expect("reopen");
    assert_eq!(
        db.query("SELECT txt, ts FROM temporal_persist", &[])
            .expect("read after reopen")[0]
            .values,
        stored[0].values
    );
    for expression in ["CURRENT_TIMESTAMP", "now()", "datetime('now')", "CURRENT_DATE"] {
        db.execute("DROP TABLE IF EXISTS typed_default").expect("drop");
        db.execute(&format!(
            "CREATE TABLE typed_default (id INT PRIMARY KEY, ts TIMESTAMP DEFAULT {expression})"
        ))
        .expect("create typed");
        db.execute_params("INSERT INTO typed_default (id) VALUES ($1)", &[Value::Int4(1)])
            .expect("typed insert");
        let rows = db.query("SELECT ts FROM typed_default", &[]).expect("typed read");
        assert!(matches!(&rows[0].values[0], Value::Timestamp(_)), "{expression}");
    }
}

#[test]
fn temporal_defaults_into_text_are_unquoted_on_both_executor_families() {
    for parameterized in [false, true] {
        for column_type in ["TEXT", "VARCHAR(80)"] {
            for expression in [
                "CURRENT_TIMESTAMP",
                "now()",
                "datetime('now')",
                "CURRENT_DATE",
                "CURRENT_TIME",
            ] {
                let db = EmbeddedDatabase::new_in_memory().expect("database");
                db.execute(&format!(
                    "CREATE TABLE defaults_probe (id INT PRIMARY KEY, value {column_type} DEFAULT {expression})"
                ))
                .expect("create table");
                if parameterized {
                    db.execute_params("INSERT INTO defaults_probe (id) VALUES ($1)", &[Value::Int4(1)])
                        .expect("bound insert");
                } else {
                    db.execute("INSERT INTO defaults_probe (id) VALUES (1)")
                        .expect("text insert");
                }
                let rows = db.query("SELECT value FROM defaults_probe", &[]).expect("read default");
                let Value::String(value) = &rows[0].values[0] else {
                    panic!("expected string: {:?}", rows[0].values[0]);
                };
                assert!(
                    !value.contains('\''),
                    "{expression} into {column_type}, bound={parameterized}: {value:?}"
                );
                if matches!(expression, "CURRENT_TIMESTAMP" | "now()" | "datetime('now')") {
                    assert_eq!(value.as_bytes().get(10), Some(&b' '), "{value:?}");
                    assert!(value.ends_with("+00"), "{value:?}");
                }
            }
        }
    }
}
