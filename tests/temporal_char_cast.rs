//! CHAR stores temporal text, not the SQL-literal representation.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

#[test]
fn bound_temporal_char_casts_preserve_bytes_and_padding() {
    let db = EmbeddedDatabase::new_in_memory().expect("database");
    let timestamp = chrono::DateTime::parse_from_rfc3339("2026-09-23T12:34:56.123456789Z")
        .expect("timestamp")
        .with_timezone(&chrono::Utc);
    for (value, expected) in [
        (Value::Timestamp(timestamp), "2026-09-23 12:34:56.123456+00"),
        (Value::Date(timestamp.date_naive()), "2026-09-23"),
        (Value::Time(timestamp.time()), "12:34:56.123456"),
    ] {
        for width in [1, expected.chars().count(), 40] {
            let rows = db
                .query_params(&format!("SELECT CAST($1 AS CHAR({width}))"), &[value.clone()])
                .expect("cast");
            // Preserve Nano's existing non-truncating over-length behavior.
            assert_eq!(rows[0].values[0], Value::String(format!("{expected:width$}")));
        }
    }
}

#[test]
fn temporal_char_defaults_are_unquoted_on_both_executor_families() {
    for parameterized in [false, true] {
        for expression in ["CURRENT_TIMESTAMP", "CURRENT_DATE", "CURRENT_TIME"] {
            let db = EmbeddedDatabase::new_in_memory().expect("database");
            db.execute(&format!(
                "CREATE TABLE temporal_char (id INT PRIMARY KEY, value CHAR(40) DEFAULT {expression})"
            ))
            .expect("create");
            if parameterized {
                db.execute_params("INSERT INTO temporal_char (id) VALUES ($1)", &[Value::Int4(1)])
                    .expect("bound insert");
            } else {
                db.execute("INSERT INTO temporal_char (id) VALUES (1)")
                    .expect("text insert");
            }
            let rows = db.query("SELECT value FROM temporal_char", &[]).expect("read");
            let Value::String(value) = &rows[0].values[0] else {
                panic!("expected string");
            };
            assert!(!value.contains('\''), "{expression}, bound={parameterized}: {value:?}");
            assert_eq!(value.chars().count(), 40);
            let unpadded = value.trim_end_matches(' ');
            let expected_len = match expression {
                "CURRENT_DATE" => 10,
                "CURRENT_TIME" => 15,
                _ => 29,
            };
            assert_eq!(unpadded.len(), expected_len, "{value:?}");
        }
    }
}

#[test]
fn char_non_temporal_controls_keep_existing_behavior() {
    let db = EmbeddedDatabase::new_in_memory().expect("database");
    for (value, expected) in [
        (Value::Null, Value::Null),
        (Value::String("é".into()), Value::String("é  ".into())),
        (Value::String("long".into()), Value::String("long".into())),
    ] {
        let rows = db.query_params("SELECT CAST($1 AS CHAR(3))", &[value]).expect("cast");
        assert_eq!(rows[0].values[0], expected);
    }
    let uuid = uuid::Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").expect("uuid");
    let rows = db
        .query_params("SELECT CAST($1 AS CHAR(40))", &[Value::Uuid(uuid)])
        .expect("UUID cast");
    assert_eq!(rows[0].values[0], Value::String(format!("{uuid}    ")));
}

#[test]
fn explicit_sql_temporal_char_casts_and_persisted_defaults() {
    let dir = tempfile::tempdir().expect("directory");
    let stored;
    {
        let db = EmbeddedDatabase::new(dir.path()).expect("database");
        for (source, expected) in [
            ("CAST('2026-09-23' AS DATE)", "2026-09-23"),
            ("CAST('12:34:56.123456' AS TIME)", "12:34:56.123456"),
            (
                "CAST('2026-09-23 12:34:56.123456' AS TIMESTAMP)",
                "2026-09-23 12:34:56.123456+00",
            ),
        ] {
            let rows = db
                .query(&format!("SELECT CAST({source} AS CHAR(40))"), &[])
                .expect("SQL cast");
            assert_eq!(rows[0].values[0], Value::String(format!("{expected:40}")));
        }
        db.execute("CREATE TABLE persisted_char (id INT PRIMARY KEY, value CHAR(40) DEFAULT CURRENT_DATE)")
            .expect("create");
        db.execute("INSERT INTO persisted_char (id) VALUES (1)")
            .expect("text insert");
        db.execute_params("INSERT INTO persisted_char (id) VALUES ($1)", &[Value::Int4(2)])
            .expect("bound insert");
        stored = db
            .query("SELECT id, value FROM persisted_char ORDER BY id", &[])
            .expect("read");
        assert_eq!(stored.len(), 2, "both executor families must insert a row");
        for (index, row) in stored.iter().enumerate() {
            assert_eq!(row.values[0], Value::Int4(i32::try_from(index + 1).expect("id")));
            let Value::String(value) = &row.values[1] else {
                panic!("expected string")
            };
            assert_eq!(value.len(), 40);
            assert_eq!(value.trim_end_matches(' ').len(), 10);
            assert!(!value.contains('\''));
        }
    }
    let db = EmbeddedDatabase::new(dir.path()).expect("reopen");
    let reopened = db
        .query("SELECT id, value FROM persisted_char ORDER BY id", &[])
        .expect("read reopened");
    assert_eq!(reopened.len(), stored.len());
    for (before, after) in stored.iter().zip(&reopened) {
        assert_eq!(before.values, after.values);
    }
}

#[test]
fn explicit_temporal_char_inserts_preserve_exact_bytes_after_reopen() {
    let dir = tempfile::tempdir().expect("directory");
    let timestamp = chrono::DateTime::parse_from_rfc3339("2026-09-23T12:34:56.123456789Z")
        .expect("timestamp")
        .with_timezone(&chrono::Utc);
    let cases = [
        (
            Value::Timestamp(timestamp),
            "CAST('2026-09-23 12:34:56.123456789' AS TIMESTAMP)",
            "2026-09-23 12:34:56.123456+00",
        ),
        (
            Value::Date(timestamp.date_naive()),
            "CAST('2026-09-23' AS DATE)",
            "2026-09-23",
        ),
        (
            Value::Time(timestamp.time()),
            "CAST('12:34:56.123456789' AS TIME)",
            "12:34:56.123456",
        ),
    ];
    let mut expected_rows = Vec::new();
    {
        let db = EmbeddedDatabase::new(dir.path()).expect("database");
        db.execute("CREATE TABLE explicit_char (id INT PRIMARY KEY, value CHAR(40))")
            .expect("create");
        for (index, (value, sql, expected)) in cases.iter().enumerate() {
            let text_id = i32::try_from(index * 2 + 1).expect("id");
            let bound_id = text_id + 1;
            db.execute(&format!(
                "INSERT INTO explicit_char VALUES ({text_id}, CAST({sql} AS CHAR(40)))"
            ))
            .expect("text cast insert");
            db.execute_params(
                "INSERT INTO explicit_char VALUES ($1, CAST($2 AS CHAR(40)))",
                &[Value::Int4(bound_id), value.clone()],
            )
            .expect("bound cast insert");
            for id in [text_id, bound_id] {
                expected_rows.push(vec![Value::Int4(id), Value::String(format!("{expected:40}"))]);
            }
        }
        let rows = db
            .query("SELECT id, value FROM explicit_char ORDER BY id", &[])
            .expect("read");
        assert_eq!(rows.len(), 6);
        for (row, expected) in rows.iter().zip(&expected_rows) {
            assert_eq!(&row.values, expected);
        }
    }
    let db = EmbeddedDatabase::new(dir.path()).expect("reopen");
    let rows = db
        .query("SELECT id, value FROM explicit_char ORDER BY id", &[])
        .expect("read reopened");
    assert_eq!(rows.len(), 6);
    for (row, expected) in rows.iter().zip(&expected_rows) {
        assert_eq!(&row.values, expected);
    }
}

#[test]
fn temporal_sql_literal_display_retains_quotes_and_nanoseconds() {
    let timestamp = chrono::DateTime::parse_from_rfc3339("2026-09-23T12:34:56.123456789Z")
        .expect("timestamp")
        .with_timezone(&chrono::Utc);
    assert_eq!(
        Value::Timestamp(timestamp).to_string(),
        "'2026-09-23T12:34:56.123456789+00:00'"
    );
    assert_eq!(Value::Date(timestamp.date_naive()).to_string(), "'2026-09-23'");
    assert_eq!(Value::Time(timestamp.time()).to_string(), "'12:34:56.123456789'");
}
