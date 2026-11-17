//! DROP TABLE finds the foreign keys that depend on a table from the stored
//! constraint records. While no constraint record has been written since a walk
//! of every record listed the tables that name a foreign key, a DROP reads those
//! records by name instead of walking them all again (the write gate counts every
//! write of a constraint record). These tests change the foreign keys between two
//! DROPs in every way a statement can, and check that each DROP still sees them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use heliosdb_nano::EmbeddedDatabase;

fn refused_for_dependents(db: &EmbeddedDatabase, sql: &str, child: &str) {
    let error = db.execute(sql).expect_err(sql).to_string();
    assert!(
        error.contains("cannot be dropped because some objects depend on it") && error.contains(child),
        "{sql}: {error}"
    );
}

fn count(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db.query(sql, &[]).unwrap();
    match rows.first().and_then(|row| row.values.first()) {
        Some(heliosdb_nano::Value::Int8(n)) => *n,
        Some(heliosdb_nano::Value::Int4(n)) => i64::from(*n),
        other => panic!("not a count: {other:?}"),
    }
}

#[test]
fn a_foreign_key_created_after_a_drop_is_seen_by_the_next_drop() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    for name in ["spare_a", "spare_b", "spare_c"] {
        db.execute(&format!("CREATE TABLE {name} (id INT)")).unwrap();
    }
    // These DROPs list the tables that name a foreign key (none) and then use the list.
    db.execute("DROP TABLE spare_a").unwrap();
    db.execute("DROP TABLE spare_b").unwrap();

    db.execute("CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id))")
        .unwrap();
    refused_for_dependents(&db, "DROP TABLE parent", "child");
    // Listed again now, the child among them; still refused, then cascaded.
    db.execute("DROP TABLE spare_c").unwrap();
    refused_for_dependents(&db, "DROP TABLE parent", "child");
    db.execute("INSERT INTO parent VALUES (1)").unwrap();
    db.execute("INSERT INTO child VALUES (10, 1)").unwrap();
    db.execute("DROP TABLE parent CASCADE").unwrap();
    // The child stays, without its foreign key: a row naming a parent that is gone is accepted.
    db.execute("INSERT INTO child VALUES (11, 99)").unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM child"), 2);
}

#[test]
fn a_foreign_key_added_by_alter_table_is_seen_by_the_next_drop() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE child (id INT PRIMARY KEY, parent_id INT)")
        .unwrap();
    db.execute("CREATE TABLE spare (id INT)").unwrap();
    db.execute("DROP TABLE spare").unwrap();

    db.execute("ALTER TABLE child ADD CONSTRAINT child_parent FOREIGN KEY (parent_id) REFERENCES parent(id)")
        .unwrap();
    refused_for_dependents(&db, "DROP TABLE parent", "child");
}

#[test]
fn a_foreign_key_dropped_by_alter_table_no_longer_holds_the_parent() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    db.execute(
        "CREATE TABLE child (id INT PRIMARY KEY, parent_id INT, \
         CONSTRAINT child_parent FOREIGN KEY (parent_id) REFERENCES parent(id))",
    )
    .unwrap();
    db.execute("CREATE TABLE spare (id INT)").unwrap();
    db.execute("DROP TABLE spare").unwrap();
    refused_for_dependents(&db, "DROP TABLE parent", "child");

    db.execute("ALTER TABLE child DROP CONSTRAINT child_parent").unwrap();
    db.execute("DROP TABLE parent").unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM child"), 0);
}

#[test]
fn a_renamed_child_still_holds_its_parent() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id))")
        .unwrap();
    db.execute("CREATE TABLE spare (id INT)").unwrap();
    db.execute("DROP TABLE spare").unwrap();
    refused_for_dependents(&db, "DROP TABLE parent", "child");

    db.execute("ALTER TABLE child RENAME TO renamed_child").unwrap();
    refused_for_dependents(&db, "DROP TABLE parent", "renamed_child");
}

#[test]
fn dropping_a_child_releases_its_parent() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id))")
        .unwrap();
    refused_for_dependents(&db, "DROP TABLE parent", "child");
    db.execute("DROP TABLE child").unwrap();
    db.execute("DROP TABLE parent").unwrap();
}

#[test]
fn a_drop_inside_a_transaction_sees_a_child_created_before_it() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE spare (id INT)").unwrap();
    db.execute("DROP TABLE spare").unwrap();
    db.execute("CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id))")
        .unwrap();
    db.execute("BEGIN").unwrap();
    refused_for_dependents(&db, "DROP TABLE parent", "child");
    db.execute("ROLLBACK").unwrap();
}
