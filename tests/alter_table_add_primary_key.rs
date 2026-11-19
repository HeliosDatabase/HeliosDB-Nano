#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! `ALTER TABLE … ADD [CONSTRAINT name] PRIMARY KEY (cols)` on a table with
//! rows (v05.10): the statement every pg_dump emits.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn seeded(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE pre (id INT, k INT, v TEXT)").unwrap();
    db.execute("INSERT INTO pre VALUES (1, 10, 'a'), (2, 10, 'b'), (3, 20, 'c')")
        .unwrap();
}

fn count(db: &EmbeddedDatabase, sql: &str) -> usize {
    db.query(sql, &[]).unwrap().len()
}

fn key_is_enforced_and_used(db: &EmbeddedDatabase) {
    // Key lookups find the rows, in both families.
    assert_eq!(count(db, "SELECT v FROM pre WHERE id = 2"), 1);
    assert_eq!(
        db.query_params("SELECT v FROM pre WHERE id = $1", &[Value::Int4(3)])
            .unwrap()
            .len(),
        1
    );
    // A duplicate key is refused.
    let err = db.execute("INSERT INTO pre VALUES (1, 99, 'dup')").unwrap_err();
    assert!(err.to_string().to_lowercase().contains("duplicate"), "{err}");
    // A NULL key is refused.
    assert!(db.execute("INSERT INTO pre VALUES (NULL, 99, 'n')").is_err());
    // The UPDATE / DELETE key paths find the rows.
    assert_eq!(db.execute("UPDATE pre SET v = 'x' WHERE id = 1").unwrap(), 1);
    assert_eq!(
        db.execute_params(
            "UPDATE pre SET v = $1 WHERE id = $2",
            &[Value::String("y".into()), Value::Int4(2)]
        )
        .unwrap(),
        1
    );
    assert_eq!(db.execute("DELETE FROM pre WHERE id = 3").unwrap(), 1);
    db.execute("INSERT INTO pre VALUES (3, 20, 'c')").unwrap();
    assert_eq!(count(db, "SELECT id FROM pre"), 3);
}

#[test]
fn unnamed_named_and_only() {
    for sql in [
        "ALTER TABLE pre ADD PRIMARY KEY (id)",
        "ALTER TABLE pre ADD CONSTRAINT pre_pk PRIMARY KEY (id)",
        "ALTER TABLE ONLY pre ADD CONSTRAINT pre_pk PRIMARY KEY (id)",
    ] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seeded(&db);
        db.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        key_is_enforced_and_used(&db);
    }
}

#[test]
fn params_family() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded(&db);
    db.execute_params("ALTER TABLE pre ADD PRIMARY KEY (id)", &[]).unwrap();
    key_is_enforced_and_used(&db);
}

#[test]
fn composite() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded(&db);
    db.execute("ALTER TABLE pre ADD PRIMARY KEY (k, id)").unwrap();
    assert!(db.execute("INSERT INTO pre VALUES (1, 10, 'dup')").is_err());
    db.execute("INSERT INTO pre VALUES (1, 20, 'ok')").unwrap();
    assert_eq!(count(&db, "SELECT v FROM pre WHERE k = 10 AND id = 2"), 1);
}

#[test]
fn null_violation_23502() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded(&db);
    db.execute("INSERT INTO pre VALUES (NULL, 30, 'n')").unwrap();
    let err = db.execute("ALTER TABLE pre ADD PRIMARY KEY (id)").unwrap_err();
    assert!(err.to_string().contains("not-null"), "{err}");
    // Nothing changed: no key.
    db.execute("INSERT INTO pre VALUES (1, 10, 'again')").unwrap();
}

#[test]
fn duplicate_violation_23505() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded(&db);
    let err = db.execute("ALTER TABLE pre ADD PRIMARY KEY (k)").unwrap_err();
    assert!(err.to_string().to_lowercase().contains("duplicate"), "{err}");
    db.execute("INSERT INTO pre VALUES (1, 10, 'again')").unwrap();
    // And a key can still be added afterwards.
    db.execute("DELETE FROM pre WHERE v = 'again'").unwrap();
    db.execute("ALTER TABLE pre ADD PRIMARY KEY (id)").unwrap();
    key_is_enforced_and_used(&db);
}

#[test]
fn second_primary_key_refused() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE has (id INT PRIMARY KEY, k INT)").unwrap();
    let err = db.execute("ALTER TABLE has ADD PRIMARY KEY (k)").unwrap_err();
    assert!(err.to_string().contains("multiple primary keys"), "{err}");
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded(&db);
    db.execute("ALTER TABLE pre ADD PRIMARY KEY (id)").unwrap();
    let err = db.execute("ALTER TABLE pre ADD PRIMARY KEY (k)").unwrap_err();
    assert!(err.to_string().contains("multiple primary keys"), "{err}");
}

#[test]
fn survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        seeded(&db);
        db.execute("ALTER TABLE pre ADD CONSTRAINT pre_pk PRIMARY KEY (id)")
            .unwrap();
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    key_is_enforced_and_used(&db);
}

#[test]
fn inside_a_transaction_over_committed_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded(&db);
    db.begin().unwrap();
    db.execute("ALTER TABLE pre ADD PRIMARY KEY (id)").unwrap();
    db.commit().unwrap();
    key_is_enforced_and_used(&db);
}
