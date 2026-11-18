//! The list of foreign-key owners a `DROP TABLE` reads instead of walking every
//! constraint record (`WriteGate::foreign_key_owners`).
//!
//! Two properties. The list, whenever it is offered, is exactly what a walk of
//! every record finds: a list that misses an owner would let a DROP leave a
//! foreign key to a table that is gone. And it survives the statements that
//! used to discard it: before, any write of a constraint record did, so in a
//! schema of tables with keys every `DROP TABLE` walked every record again
//! (3.6 ms at 1,600 tables where 4.41.1 took 0.1 ms).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::EmbeddedDatabase;

/// The owners as a walk of every stored record finds them.
fn walked(db: &EmbeddedDatabase) -> Vec<String> {
    let mut owners: Vec<String> = db
        .storage
        .meta_blobs_with_prefix("table_constraints:")
        .unwrap()
        .into_iter()
        .filter(|(_, bytes)| {
            !bincode::deserialize::<crate::sql::TableConstraints>(bytes)
                .unwrap()
                .foreign_keys
                .is_empty()
        })
        .map(|(owner, _)| owner)
        .collect();
    owners.sort();
    owners
}

/// Run `sql`; whatever the list is afterwards, it must not disagree with a walk.
fn run(db: &EmbeddedDatabase, sql: &str) -> Option<Vec<String>> {
    db.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    let listed = db.storage.foreign_key_owners();
    if let Some(listed) = &listed {
        assert_eq!(listed, &walked(db), "after `{sql}`");
    }
    listed
}

/// A `DROP TABLE` walks when there is no list, and leaves one behind.
fn seed(db: &EmbeddedDatabase) {
    run(db, "CREATE TABLE seed_list (id INT PRIMARY KEY)");
    let listed = run(db, "DROP TABLE seed_list");
    assert!(listed.is_some(), "a DROP TABLE leaves the list behind");
}

#[test]
fn the_list_survives_creates_and_drops_of_tables_with_keys() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    run(&db, "CREATE TABLE parent (id INT PRIMARY KEY)");
    run(
        &db,
        "CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id))",
    );
    for n in 0..20 {
        run(
            &db,
            &format!("CREATE TABLE keyed_{n} (id INT PRIMARY KEY, v INT UNIQUE)"),
        );
    }
    seed(&db);
    for round in 0..5 {
        let created = run(&db, "CREATE TABLE y (id INT PRIMARY KEY, note TEXT)");
        assert_eq!(created, Some(vec!["child".to_string()]), "round {round}: after CREATE");
        let dropped = run(&db, "DROP TABLE y");
        assert_eq!(dropped, Some(vec!["child".to_string()]), "round {round}: after DROP");
    }
}

#[test]
fn every_schema_statement_leaves_a_list_that_agrees_with_a_walk_or_none() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    run(&db, "CREATE TABLE parent (id INT PRIMARY KEY, code INT UNIQUE)");
    seed(&db);
    for sql in [
        "CREATE TABLE c1 (id INT PRIMARY KEY, pid INT REFERENCES parent(id))",
        "CREATE TABLE c2 (id INT PRIMARY KEY, pid INT)",
        "ALTER TABLE c2 ADD CONSTRAINT c2_fk FOREIGN KEY (pid) REFERENCES parent(id)",
        "CREATE TABLE c3 (id INT PRIMARY KEY, pid INT, CONSTRAINT c3_fk FOREIGN KEY (pid) REFERENCES parent(code))",
        "ALTER TABLE c2 DROP CONSTRAINT c2_fk",
        "ALTER TABLE c1 RENAME TO c1_renamed",
        "ALTER TABLE c3 ADD COLUMN extra INT",
        "CREATE TABLE tree (id INT PRIMARY KEY, up INT REFERENCES tree(id))",
        "DROP TABLE c3",
        "CREATE INDEX c2_pid ON c2 (pid)",
        "TRUNCATE c2",
        "DROP TABLE tree",
        "DROP TABLE c2",
    ] {
        run(&db, sql);
        // The next DROP must find a list again, and a right one.
        seed(&db);
    }
    assert_eq!(db.storage.foreign_key_owners(), Some(vec!["c1_renamed".to_string()]));
}

#[test]
fn a_child_made_after_the_list_still_protects_its_parent() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    run(&db, "CREATE TABLE parent (id INT PRIMARY KEY)");
    run(&db, "INSERT INTO parent VALUES (1)");
    seed(&db);
    assert_eq!(db.storage.foreign_key_owners(), Some(Vec::new()));
    // The list was made when nothing named a foreign key.
    run(
        &db,
        "CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id))",
    );
    run(&db, "INSERT INTO child VALUES (10, 1)");
    let refused = db
        .execute("DROP TABLE parent")
        .expect_err("a child still names the parent");
    assert!(
        refused.to_string().to_lowercase().contains("child")
            || refused.to_string().to_lowercase().contains("depend")
            || refused.to_string().to_lowercase().contains("referenc"),
        "{refused}"
    );
    assert_eq!(db.query("SELECT id FROM parent", &[]).unwrap().len(), 1);

    // CASCADE takes the foreign key off the surviving child, and the list with it.
    run(&db, "DROP TABLE parent CASCADE");
    assert_eq!(walked(&db), Vec::<String>::new());
    run(&db, "INSERT INTO child VALUES (11, 999)");
    seed(&db);
    assert_eq!(db.storage.foreign_key_owners(), Some(Vec::new()));
}

#[test]
fn the_list_is_the_same_after_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        run(&db, "CREATE TABLE parent (id INT PRIMARY KEY)");
        run(
            &db,
            "CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id))",
        );
        seed(&db);
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    // Nothing is remembered across a restart: the first DROP walks.
    assert_eq!(db.storage.foreign_key_owners(), None);
    seed(&db);
    assert_eq!(db.storage.foreign_key_owners(), Some(vec!["child".to_string()]));
    assert!(db.execute("DROP TABLE parent").is_err());
}
