//! `setval()` moves the generator of a SERIAL column (GitHub #51).
//!
//! A table migrated with its ids needs its sequence moved past them:
//! `SELECT setval(pg_get_serial_sequence('jobs', 'id'), (SELECT MAX(id) FROM jobs))`.
//! The name `pg_get_serial_sequence` returns had nothing behind it: `setval`
//! created a sequence of that literal name, answered with its argument, and
//! the column went on generating from where it was.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn int(value: &Value) -> i64 {
    match value {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

fn scalar(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db.query(sql, &[]).unwrap_or_else(|error| panic!("{sql}: {error}"));
    int(&rows[0].values[0])
}

fn insert(db: &EmbeddedDatabase, data: &str) -> i64 {
    let (_, rows) = db
        .execute_returning(&format!("INSERT INTO jobs (data) VALUES ('{data}') RETURNING id"))
        .unwrap_or_else(|error| panic!("insert {data}: {error}"));
    int(&rows[0].values[0])
}

fn jobs(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE jobs (id BIGSERIAL PRIMARY KEY, data TEXT)")
        .unwrap();
}

#[test]
fn setval_moves_the_generator_under_every_spelling_of_the_name() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    jobs(&db);
    assert_eq!(insert(&db, "a"), 1);
    assert_eq!(scalar(&db, "SELECT setval('public.jobs_id_seq', 9)"), 9);
    assert_eq!(insert(&db, "b"), 10);
    assert_eq!(scalar(&db, "SELECT setval('jobs_id_seq', 30)"), 30);
    assert_eq!(insert(&db, "c"), 31);
    assert_eq!(
        scalar(&db, "SELECT setval(pg_get_serial_sequence('jobs', 'id'), 50)"),
        50
    );
    assert_eq!(insert(&db, "d"), 51);
    // `is_called = false`: the value itself is the next one.
    assert_eq!(scalar(&db, "SELECT setval('jobs_id_seq', 70, false)"), 70);
    assert_eq!(insert(&db, "e"), 70);
    // nextval on the same name takes the key the next row would have had.
    assert_eq!(scalar(&db, "SELECT nextval('jobs_id_seq')"), 71);
    assert_eq!(insert(&db, "f"), 72);
}

#[test]
fn the_idiom_after_a_migration_with_explicit_ids() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE events (seq BIGSERIAL PRIMARY KEY, note TEXT)")
        .unwrap();
    jobs(&db);
    db.execute("INSERT INTO jobs (id, data) VALUES (5, 'a'), (900, 'b')")
        .unwrap();
    assert_eq!(
        scalar(
            &db,
            "SELECT setval(pg_get_serial_sequence('jobs', 'id'), (SELECT MAX(id) FROM jobs))"
        ),
        900
    );
    assert_eq!(insert(&db, "c"), 901);
    // Another table's generator was not touched.
    db.execute("INSERT INTO events (note) VALUES ('x')").unwrap();
    assert_eq!(scalar(&db, "SELECT seq FROM events"), 1);
}

#[test]
fn the_generator_never_moves_below_a_key_it_may_have_given() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    jobs(&db);
    assert_eq!(scalar(&db, "SELECT setval('jobs_id_seq', 40)"), 40);
    assert_eq!(insert(&db, "a"), 41);
    // Asked to go back: no key can repeat.
    assert_eq!(scalar(&db, "SELECT setval('jobs_id_seq', 3)"), 3);
    assert_eq!(insert(&db, "b"), 42);
    assert!(db.query("SELECT setval('jobs_id_seq', 0)", &[]).is_err());
    assert!(db.query("SELECT setval('jobs_id_seq', -5)", &[]).is_err());
}

#[test]
fn the_position_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        jobs(&db);
        assert_eq!(scalar(&db, "SELECT setval('public.jobs_id_seq', 500)"), 500);
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert_eq!(insert(&db, "after restart"), 501);
}

#[test]
fn a_sequence_of_its_own_is_what_it_was_and_answers_to_its_schema_too() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SEQUENCE ticket_no_seq").unwrap();
    db.execute("CREATE TABLE tickets (id BIGINT PRIMARY KEY DEFAULT nextval('ticket_no_seq'), v TEXT)")
        .unwrap();
    assert_eq!(scalar(&db, "SELECT setval('ticket_no_seq', 100)"), 100);
    db.execute("INSERT INTO tickets (v) VALUES ('a')").unwrap();
    assert_eq!(scalar(&db, "SELECT MAX(id) FROM tickets"), 101);
    // `public.ticket_no_seq` is the same sequence, not a new one.
    assert_eq!(scalar(&db, "SELECT setval('public.ticket_no_seq', 200)"), 200);
    db.execute("INSERT INTO tickets (v) VALUES ('b')").unwrap();
    assert_eq!(scalar(&db, "SELECT MAX(id) FROM tickets"), 201);
    assert_eq!(scalar(&db, "SELECT nextval('public.ticket_no_seq')"), 202);
    assert_eq!(scalar(&db, "SELECT nextval('ticket_no_seq')"), 203);
}

#[test]
fn a_table_made_after_the_name_was_first_used_takes_the_name_over() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    // No such table yet: the name is a sequence created on the spot, as before.
    assert_eq!(scalar(&db, "SELECT nextval('later_id_seq')"), 1);
    db.execute("CREATE TABLE later (id SERIAL PRIMARY KEY, v TEXT)")
        .unwrap();
    assert_eq!(scalar(&db, "SELECT setval('later_id_seq', 20)"), 20);
    db.execute("INSERT INTO later (v) VALUES ('a')").unwrap();
    assert_eq!(scalar(&db, "SELECT id FROM later"), 21);
}
