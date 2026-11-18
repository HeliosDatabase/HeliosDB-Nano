//! `INSERT … SELECT` into a table with a foreign key to itself.
//!
//! A tree is loaded with `INSERT INTO node SELECT …`, and restored the same way
//! from a copy. Its rows name each other. Since `INSERT … SELECT` stages its
//! rows and commits them as one group, every row was checked before any row of
//! the statement could be found, and a row whose parent came earlier in the same
//! statement was refused with a foreign-key violation (4.41.1 wrote row by row
//! and accepted it). The key of the table to itself is now checked once the
//! rows are staged, so the parent may stand on either side of the child, as in
//! PostgreSQL, where the check runs at the end of the statement.
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

fn ids(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    db.query(sql, &[])
        .unwrap()
        .iter()
        .map(|row| int(&row.values[0]))
        .collect()
}

fn tree() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE node (id INT PRIMARY KEY, parent INT REFERENCES node(id))")
        .unwrap();
    db.execute("INSERT INTO node VALUES (1, NULL)").unwrap();
    db
}

#[test]
fn a_tree_loads_when_each_parent_comes_before_its_children() {
    let db = tree();
    // Node g hangs under node g / 2: 2 and 3 under the root, 4 and 5 under 2, …
    assert_eq!(
        db.execute("INSERT INTO node SELECT g, g / 2 FROM generate_series(2, 63) g")
            .unwrap(),
        62
    );
    assert_eq!(ids(&db, "SELECT id FROM node ORDER BY id").len(), 63);
    assert_eq!(ids(&db, "SELECT parent FROM node WHERE id = 63"), [31]);
}

#[test]
fn a_child_may_come_before_its_parent_in_the_same_statement() {
    let db = tree();
    db.execute("CREATE TABLE staging (id INT PRIMARY KEY, parent INT)")
        .unwrap();
    // Children first: 7 and 6 name 3, which comes last.
    db.execute("INSERT INTO staging VALUES (7, 3), (6, 3), (3, 1)").unwrap();
    assert_eq!(
        db.execute("INSERT INTO node SELECT id, parent FROM staging ORDER BY id DESC")
            .unwrap(),
        3
    );
    assert_eq!(ids(&db, "SELECT id FROM node ORDER BY id"), [1, 3, 6, 7]);
}

#[test]
fn a_row_may_name_itself() {
    let db = tree();
    assert_eq!(db.execute("INSERT INTO node SELECT 9, 9").unwrap(), 1);
    assert_eq!(ids(&db, "SELECT parent FROM node WHERE id = 9"), [9]);
}

#[test]
fn a_parent_that_is_nowhere_refuses_the_statement_and_leaves_nothing() {
    let db = tree();
    let refused = db
        .execute("INSERT INTO node SELECT g, CASE WHEN g = 5 THEN 99 ELSE 1 END FROM generate_series(2, 8) g")
        .expect_err("no node 99");
    assert!(refused.to_string().contains("Foreign key"), "{refused}");
    assert_eq!(ids(&db, "SELECT id FROM node ORDER BY id"), [1]);
    // The table still takes rows afterwards.
    assert_eq!(
        db.execute("INSERT INTO node SELECT g, 1 FROM generate_series(2, 4) g")
            .unwrap(),
        3
    );
    assert_eq!(ids(&db, "SELECT id FROM node ORDER BY id"), [1, 2, 3, 4]);
}

#[test]
fn the_same_with_bound_parameters_and_inside_a_transaction() {
    let db = tree();
    assert_eq!(
        db.execute_params(
            "INSERT INTO node SELECT g, g / 2 FROM generate_series(2, $1) g",
            &[Value::Int4(15)],
        )
        .unwrap(),
        14
    );
    let refused = db
        .execute_params(
            "INSERT INTO node SELECT g, $1 FROM generate_series(100, 102) g",
            &[Value::Int4(77)],
        )
        .expect_err("no node 77");
    assert!(refused.to_string().contains("Foreign key"), "{refused}");
    assert_eq!(ids(&db, "SELECT id FROM node ORDER BY id").len(), 15);

    db.execute("BEGIN").unwrap();
    assert_eq!(
        db.execute("INSERT INTO node SELECT g, g - 1 FROM generate_series(16, 20) g")
            .unwrap(),
        5
    );
    db.execute("ROLLBACK").unwrap();
    assert_eq!(ids(&db, "SELECT id FROM node ORDER BY id").len(), 15);
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO node SELECT g, g - 1 FROM generate_series(16, 20) g")
        .unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(ids(&db, "SELECT id FROM node ORDER BY id").len(), 20);
}

#[test]
fn a_tree_is_restored_from_its_copy() {
    let db = tree();
    db.execute("INSERT INTO node SELECT g, g / 2 FROM generate_series(2, 31) g")
        .unwrap();
    db.execute("CREATE TABLE node_copy AS SELECT * FROM node").unwrap();
    db.execute("DELETE FROM node WHERE id > 15").unwrap();
    db.execute("DELETE FROM node WHERE id > 7").unwrap();
    db.execute("DELETE FROM node WHERE id > 3").unwrap();
    db.execute("DELETE FROM node WHERE id > 1").unwrap();
    db.execute("DELETE FROM node").unwrap();
    assert_eq!(db.execute("INSERT INTO node SELECT * FROM node_copy").unwrap(), 31);
    assert_eq!(ids(&db, "SELECT id FROM node ORDER BY id").len(), 31);
    assert_eq!(ids(&db, "SELECT parent FROM node WHERE id = 31"), [15]);
}

#[test]
fn a_key_to_another_table_is_still_checked_before_anything_is_staged() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE owner (id INT PRIMARY KEY)").unwrap();
    db.execute(
        "CREATE TABLE node (id INT PRIMARY KEY, parent INT REFERENCES node(id), owner_id INT REFERENCES owner(id))",
    )
    .unwrap();
    db.execute("INSERT INTO owner VALUES (1)").unwrap();
    assert_eq!(
        db.execute(
            "INSERT INTO node SELECT g, CASE WHEN g = 1 THEN NULL ELSE g - 1 END, 1 FROM generate_series(1, 5) g"
        )
        .unwrap(),
        5
    );
    let refused = db
        .execute("INSERT INTO node SELECT g, g - 1, 2 FROM generate_series(6, 8) g")
        .expect_err("no owner 2");
    assert!(refused.to_string().contains("owner"), "{refused}");
    assert_eq!(ids(&db, "SELECT id FROM node ORDER BY id"), [1, 2, 3, 4, 5]);
}
