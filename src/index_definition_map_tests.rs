//! The map of which `meta:index:` record names which table, which a
//! `DROP TABLE` reads instead of every table's records
//! (`WriteGate::index_definitions_of_table`, v06.4).
//!
//! Whenever the map is offered it must be exactly what a walk of every record
//! finds: a map that missed a record would leave an orphan definition that
//! resurrects on the next table of that name.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use crate::EmbeddedDatabase;

/// `(index, table)` as a walk of every record finds them.
fn walked(db: &EmbeddedDatabase) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = db
        .storage
        .catalog()
        .list_index_definitions()
        .unwrap()
        .into_iter()
        .map(|(name, definition)| (name, definition.table_name))
        .collect();
    pairs.sort();
    pairs
}

fn tables(db: &EmbeddedDatabase) -> Vec<String> {
    let mut tables: Vec<String> = walked(db).into_iter().map(|(_, t)| t).collect();
    tables.extend(["y".to_string(), "seed".to_string(), "nope".to_string()]);
    tables.sort();
    tables.dedup();
    tables
}

/// Run `sql`; then for every table the map offers, it must agree with a walk.
fn run(db: &EmbeddedDatabase, sql: &str) -> bool {
    db.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    let all = walked(db);
    let mut offered = false;
    for table in tables(db) {
        if let Some(mut names) = db.storage.index_definitions_of_table(&table) {
            offered = true;
            names.sort();
            let expected: Vec<String> = all
                .iter()
                .filter(|(_, t)| t == &table)
                .map(|(n, _)| n.clone())
                .collect();
            assert_eq!(names, expected, "table {table} after `{sql}`");
        }
    }
    offered
}

/// A DROP TABLE walks when there is no map, and leaves one behind.
fn seed(db: &EmbeddedDatabase) {
    run(db, "CREATE TABLE seed (id INT PRIMARY KEY, c INT)");
    run(db, "CREATE INDEX seed_c ON seed (c)");
    assert!(run(db, "DROP TABLE seed"), "a DROP TABLE leaves the map behind");
}

#[test]
fn the_map_survives_creates_and_drops_and_follows_every_index_statement() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    for n in 0..10 {
        run(&db, &format!("CREATE TABLE keep_{n} (id INT PRIMARY KEY, c INT)"));
        run(&db, &format!("CREATE INDEX keep_{n}_c ON keep_{n} (c)"));
    }
    seed(&db);
    for round in 0..5 {
        assert!(
            run(&db, "CREATE TABLE y (id INT PRIMARY KEY, c INT, d INT)"),
            "round {round}"
        );
        assert!(run(&db, "CREATE INDEX y_c ON y (c)"), "round {round}");
        assert!(run(&db, "CREATE INDEX y_d ON y (d)"), "round {round}");
        assert!(run(&db, "DROP TABLE y"), "round {round}: the map is kept across DROP");
        assert!(
            walked(&db).iter().all(|(_, t)| t != "y"),
            "round {round}: y's definitions are gone"
        );
    }
    for sql in [
        "CREATE TABLE y (id INT PRIMARY KEY, c INT)",
        "CREATE INDEX y_c ON y (c)",
        "DROP INDEX y_c",
        "CREATE INDEX y_c2 ON y (c)",
        "ALTER TABLE y RENAME TO y2",
        "ALTER TABLE keep_1 RENAME COLUMN c TO c1",
        "DROP TABLE keep_2",
        "DROP TABLE y2",
    ] {
        run(&db, sql);
        seed(&db);
    }
    assert_eq!(walked(&db).len(), 9);
}

#[test]
fn a_dropped_table_takes_exactly_its_own_definitions_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        run(&db, "CREATE TABLE gone (id INT PRIMARY KEY, c INT)");
        run(&db, "CREATE TABLE gone2 (id INT PRIMARY KEY, c INT)");
        run(&db, "CREATE INDEX gone_c ON gone (c)");
        run(&db, "CREATE INDEX gone2_c ON gone2 (c)");
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    // Nothing is remembered across a restart: the first DROP walks.
    assert_eq!(db.storage.index_definitions_of_table("gone"), None);
    seed(&db);
    run(&db, "DROP TABLE gone");
    assert_eq!(walked(&db), vec![("gone2_c".to_string(), "gone2".to_string())]);
    run(&db, "CREATE TABLE gone (id INT PRIMARY KEY, c INT)");
    assert_eq!(db.storage.index_definitions_of_table("gone"), Some(Vec::new()));
}
