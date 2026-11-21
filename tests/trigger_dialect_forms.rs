#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! Triggers written with an inline body: the MySQL forms (`FOR EACH ROW`
//! with one statement or `BEGIN … END`, `SET NEW.col`, `SIGNAL`) and the
//! SQLite forms (`BEGIN … END`, `WHEN`, `RAISE(ABORT, …)`, `RAISE(IGNORE)`).
//! They run on the same machinery as a trigger that names a function; a
//! construct that is not taken is refused at CREATE, by name.

use heliosdb_nano::{EmbeddedDatabase, Value};

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}

const FAMILIES: [Family; 2] = [Family::Text, Family::Params];

fn run(db: &EmbeddedDatabase, family: Family, sql: &str) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
}

fn ok(db: &EmbeddedDatabase, family: Family, sql: &str) -> u64 {
    run(db, family, sql).unwrap_or_else(|e| panic!("{family:?}: `{sql}` failed: {e}"))
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<String> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .iter()
        .map(|row| {
            row.values
                .iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::String(s) => s.clone(),
                    other => format!("{other}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn db_with_tables() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, qty INT, note TEXT)")
        .unwrap();
    db.execute("CREATE TABLE log (seq SERIAL PRIMARY KEY, line TEXT)")
        .unwrap();
    db
}

#[test]
fn mysql_single_statement_bodies_run() {
    for family in FAMILIES {
        let db = db_with_tables();
        ok(
            &db,
            family,
            "CREATE TRIGGER items_bi BEFORE INSERT ON items FOR EACH ROW SET NEW.note = 'new', NEW.qty = NEW.qty + 1",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_ad AFTER DELETE ON items FOR EACH ROW INSERT INTO log (line) VALUES (OLD.note)",
        );
        ok(&db, family, "INSERT INTO items VALUES (1, 1, 'x')");
        assert_eq!(rows(&db, "SELECT qty, note FROM items"), vec!["2|new"], "{family:?}");
        ok(&db, family, "DELETE FROM items WHERE id = 1");
        assert_eq!(rows(&db, "SELECT line FROM log"), vec!["new"], "{family:?}");
    }
}

#[test]
fn a_mysql_begin_end_body_with_if_set_and_signal_runs() {
    for family in FAMILIES {
        let db = db_with_tables();
        ok(
            &db,
            family,
            "CREATE TRIGGER `items_bu` BEFORE UPDATE ON `items` FOR EACH ROW BEGIN \
               IF NEW.qty < 0 THEN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'qty cannot go below zero'; \
               ELSEIF NEW.qty > 100 THEN SET NEW.qty = 100; \
               ELSE SET NEW.note = 'ok'; END IF; \
               INSERT INTO log (line) VALUES (OLD.qty); \
             END",
        );
        db.execute("INSERT INTO items VALUES (1, 10, 'x'), (2, 20, 'y')")
            .unwrap();
        ok(&db, family, "UPDATE items SET qty = 500 WHERE id = 1");
        ok(&db, family, "UPDATE items SET qty = 21 WHERE id = 2");
        let err = run(&db, family, "UPDATE items SET qty = -1 WHERE id = 2")
            .expect_err("SIGNAL")
            .to_string();
        assert!(
            err.contains("qty cannot go below zero") && err.contains("45000"),
            "{family:?}: {err}"
        );
        assert_eq!(
            rows(&db, "SELECT id, qty, note FROM items ORDER BY id"),
            vec!["1|100|x", "2|21|ok"],
            "{family:?}"
        );
        assert_eq!(
            rows(&db, "SELECT line FROM log ORDER BY seq"),
            vec!["10", "20"],
            "{family:?}"
        );
    }
}

#[test]
fn sqlite_bodies_with_when_and_raise_run() {
    for family in FAMILIES {
        let db = db_with_tables();
        // Row-level without FOR EACH ROW, WHEN without parentheses.
        ok(
            &db,
            family,
            "CREATE TRIGGER items_log AFTER UPDATE OF qty ON items WHEN NEW.qty <> OLD.qty BEGIN \
               INSERT INTO log (line) VALUES (OLD.qty || '->' || NEW.qty); \
               UPDATE items SET note = 'changed' WHERE id = NEW.id; \
             END;",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER IF NOT EXISTS items_guard BEFORE DELETE ON items BEGIN \
               SELECT RAISE(ABORT, 'locked rows stay') WHERE OLD.note = 'locked'; \
             END",
        );
        // IF NOT EXISTS: the second create is a no-op.
        ok(
            &db,
            family,
            "CREATE TRIGGER IF NOT EXISTS items_guard BEFORE DELETE ON items BEGIN SELECT RAISE(IGNORE); END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_skip BEFORE INSERT ON items BEGIN SELECT RAISE(IGNORE) WHERE NEW.qty IS NULL; END",
        );
        db.execute("INSERT INTO items VALUES (1, 10, 'x'), (2, 20, 'locked'), (3, NULL, 'z')")
            .unwrap();
        assert_eq!(
            rows(&db, "SELECT id FROM items ORDER BY id"),
            vec!["1", "2"],
            "{family:?}"
        );
        ok(&db, family, "UPDATE items SET qty = 11 WHERE id = 1");
        ok(&db, family, "UPDATE items SET qty = 20 WHERE id = 2");
        assert_eq!(rows(&db, "SELECT line FROM log"), vec!["10->11"], "{family:?}");
        assert_eq!(
            rows(&db, "SELECT note FROM items WHERE id = 1"),
            vec!["changed"],
            "{family:?}"
        );
        let err = run(&db, family, "DELETE FROM items WHERE id = 2")
            .expect_err("RAISE(ABORT)")
            .to_string();
        assert!(err.contains("locked rows stay"), "{family:?}: {err}");
        ok(&db, family, "DELETE FROM items WHERE id = 1");
        assert_eq!(rows(&db, "SELECT id FROM items"), vec!["2"], "{family:?}");
        // DROP TRIGGER without a table, as both dialects write it.
        ok(&db, family, "DROP TRIGGER items_guard");
        ok(&db, family, "DELETE FROM items WHERE id = 2");
        assert!(rows(&db, "SELECT id FROM items").is_empty(), "{family:?}");
    }
}

#[test]
fn body_constructs_that_are_not_taken_are_refused_at_create_by_name() {
    for family in FAMILIES {
        let db = db_with_tables();
        for (sql, what) in [
            (
                "CREATE TRIGGER g BEFORE INSERT ON items FOR EACH ROW BEGIN DECLARE n INT; SET NEW.qty = 1; END",
                "DECLARE",
            ),
            (
                "CREATE TRIGGER g BEFORE INSERT ON items FOR EACH ROW SET @last = NEW.id",
                "SET of anything but NEW.<column>",
            ),
            (
                "CREATE TRIGGER g BEFORE INSERT ON items BEGIN SELECT RAISE(ROLLBACK, 'no'); END",
                "RAISE(ROLLBACK",
            ),
            (
                "CREATE TRIGGER g AFTER INSERT ON items FOR EACH ROW FOLLOWS other INSERT INTO log (line) VALUES ('x')",
                "FOLLOWS",
            ),
            (
                "CREATE TEMP TRIGGER g AFTER INSERT ON items BEGIN DELETE FROM log; END",
                "TEMPORARY",
            ),
        ] {
            let err = run(&db, family, sql).expect_err(what).to_string();
            assert!(
                err.contains(what) && err.contains("trigger form is not supported"),
                "{family:?}: `{sql}`: {err}"
            );
        }
        ok(&db, family, "INSERT INTO items VALUES (1, 1, 'x')");
        assert_eq!(
            rows(&db, "SELECT qty FROM items"),
            vec!["1"],
            "{family:?}: nothing was created"
        );
    }
}

#[test]
fn an_inline_trigger_survives_a_restart() {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("nano_trigger_inline_{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        db.execute("CREATE TABLE items (id INT PRIMARY KEY, qty INT, note TEXT)")
            .unwrap();
        db.execute("CREATE TRIGGER items_bi BEFORE INSERT ON items FOR EACH ROW SET NEW.note = 'stamped'")
            .unwrap();
    }
    {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        db.execute("INSERT INTO items VALUES (1, 1, 'x')").unwrap();
        assert_eq!(rows(&db, "SELECT note FROM items"), vec!["stamped"]);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
