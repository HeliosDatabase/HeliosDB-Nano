#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! `AS OF TIMESTAMP <expression>`: now(), now() - interval, CURRENT_TIMESTAMP,
//! a bound parameter (v05.8).

use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use std::time::Duration;

/// A table whose row read v = 1 until about two seconds ago and v = 2 since.
/// Returns the database and an instant at which v was 1.
fn db() -> (EmbeddedDatabase, chrono::DateTime<chrono::Utc>) {
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = true;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE tt (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO tt VALUES (1, 1)").unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let then = chrono::Utc::now();
    std::thread::sleep(Duration::from_millis(1100));
    db.execute("UPDATE tt SET v = 2 WHERE id = 1").unwrap();
    (db, then)
}

fn v(rows: &[heliosdb_nano::Tuple]) -> i64 {
    assert_eq!(rows.len(), 1, "{rows:?}");
    match rows[0].values[0] {
        Value::Int4(n) => i64::from(n),
        Value::Int8(n) => n,
        ref other => panic!("{other:?}"),
    }
}

#[test]
fn as_of_timestamp_now() {
    let (db, _) = db();
    assert_eq!(v(&db.query("SELECT v FROM tt AS OF TIMESTAMP now()", &[]).unwrap()), 2);
    assert_eq!(
        v(&db
            .query_params(
                "SELECT v FROM tt AS OF TIMESTAMP now() WHERE id = $1",
                &[Value::Int4(1)]
            )
            .unwrap()),
        2
    );
    assert_eq!(
        v(&db.query("SELECT COUNT(*) FROM tt AS OF TIMESTAMP now()", &[]).unwrap()),
        1
    );
}

#[test]
fn now_minus_interval() {
    let (db, _) = db();
    let sql = "SELECT v FROM tt AS OF TIMESTAMP now() - interval '1 second' WHERE id = 1";
    assert_eq!(v(&db.query(sql, &[]).unwrap()), 1);
    // Asked again: not answered from a result cache.
    assert_eq!(v(&db.query(sql, &[]).unwrap()), 1);
    let sql = "SELECT v FROM tt AS OF TIMESTAMP now() - interval '1 second' WHERE id = $1";
    assert_eq!(v(&db.query_params(sql, &[Value::Int4(1)]).unwrap()), 1);
}

#[test]
fn current_timestamp() {
    let (db, _) = db();
    assert_eq!(
        v(&db
            .query("SELECT v FROM tt AS OF TIMESTAMP CURRENT_TIMESTAMP", &[])
            .unwrap()),
        2
    );
    assert_eq!(
        v(&db
            .query(
                "SELECT v FROM tt AS OF TIMESTAMP CURRENT_TIMESTAMP - interval '1 second'",
                &[]
            )
            .unwrap()),
        1
    );
}

#[test]
fn bound_parameter() {
    let (db, then) = db();
    let sql = "SELECT v FROM tt AS OF TIMESTAMP $1 WHERE id = $2";
    assert_eq!(
        v(&db.query_params(sql, &[Value::Timestamp(then), Value::Int4(1)]).unwrap()),
        1
    );
    let text = then.format("%Y-%m-%d %H:%M:%S").to_string();
    assert_eq!(
        v(&db.query_params(sql, &[Value::String(text), Value::Int4(1)]).unwrap()),
        1
    );
    assert_eq!(
        v(&db
            .query_params(sql, &[Value::Timestamp(chrono::Utc::now()), Value::Int4(1)])
            .unwrap()),
        2
    );
}

#[test]
fn anchor_before_any_history_keeps_its_refusal() {
    let (db, _) = db();
    let err = db
        .query("SELECT v FROM tt AS OF TIMESTAMP now() - interval '3650 days'", &[])
        .unwrap_err();
    let lit = db
        .query("SELECT v FROM tt AS OF TIMESTAMP '2000-01-01 00:00:00'", &[])
        .unwrap_err();
    // The same refusal as the literal form.
    let head = |e: &heliosdb_nano::Error| e.to_string().chars().take(20).collect::<String>();
    assert_eq!(head(&err), head(&lit), "{err} / {lit}");
}

#[test]
fn literal_form_unchanged() {
    let (db, then) = db();
    let text = then.format("%Y-%m-%d %H:%M:%S").to_string();
    assert_eq!(
        v(&db
            .query(&format!("SELECT v FROM tt AS OF TIMESTAMP '{text}' WHERE id = 1"), &[])
            .unwrap()),
        1
    );
    assert_eq!(v(&db.query("SELECT v FROM tt AS OF NOW", &[]).unwrap()), 2);
}
