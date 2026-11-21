#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! The life of a trigger definition: CREATE OR REPLACE, DROP, ENABLE and
//! DISABLE, RENAME, the function it names (replaced, dropped, missing), the
//! forms that are refused at CREATE, and what survives a restart.

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

fn tables(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, note TEXT)")
        .unwrap();
    db.execute("CREATE TABLE log (seq SERIAL PRIMARY KEY, line TEXT)")
        .unwrap();
}

fn db_with_tables() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    tables(&db);
    db
}

fn function(db: &EmbeddedDatabase, name: &str, body: &str) {
    db.execute(&format!(
        "CREATE OR REPLACE FUNCTION {name}() RETURNS TRIGGER AS $$ {body} $$ LANGUAGE plpgsql"
    ))
    .unwrap_or_else(|e| panic!("CREATE FUNCTION {name}: {e}"));
}

fn stamp(db: &EmbeddedDatabase, name: &str, value: &str) {
    function(db, name, &format!("BEGIN NEW.note := '{value}'; RETURN NEW; END"));
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("nano_trigger_ddl_{tag}_{id}"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

const STAMP_TRIGGER: &str = "CREATE TRIGGER items_stamp BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION stamp()";

#[test]
fn replacing_the_function_changes_what_the_trigger_does() {
    for family in FAMILIES {
        let db = db_with_tables();
        stamp(&db, "stamp", "v1");
        ok(&db, family, STAMP_TRIGGER);
        ok(&db, family, "INSERT INTO items VALUES (1, 'x')");
        stamp(&db, "stamp", "v2");
        ok(&db, family, "INSERT INTO items VALUES (2, 'x')");
        assert_eq!(
            rows(&db, "SELECT id, note FROM items ORDER BY id"),
            vec!["1|v1", "2|v2"],
            "{family:?}"
        );
    }
}

#[test]
fn a_function_a_trigger_runs_is_not_dropped_without_cascade() {
    for family in FAMILIES {
        let db = db_with_tables();
        stamp(&db, "stamp", "v1");
        ok(&db, family, STAMP_TRIGGER);
        let err = run(&db, family, "DROP FUNCTION stamp").expect_err("in use").to_string();
        assert!(
            err.contains("cannot be dropped because some objects depend on it") && err.contains("items_stamp"),
            "{family:?}: {err}"
        );
        ok(&db, family, "INSERT INTO items VALUES (1, 'x')");
        assert_eq!(
            rows(&db, "SELECT note FROM items"),
            vec!["v1"],
            "{family:?}: still fires"
        );

        ok(&db, family, "DROP FUNCTION stamp CASCADE");
        ok(&db, family, "INSERT INTO items VALUES (2, 'x')");
        assert_eq!(
            rows(&db, "SELECT note FROM items WHERE id = 2"),
            vec!["x"],
            "{family:?}: the trigger went with its function"
        );
        // The name is free again.
        stamp(&db, "stamp", "v3");
        ok(&db, family, STAMP_TRIGGER);
    }
}

#[test]
fn create_trigger_refuses_a_function_that_does_not_exist() {
    for family in FAMILIES {
        let db = db_with_tables();
        let err = run(&db, family, STAMP_TRIGGER)
            .expect_err("no such function")
            .to_string();
        assert!(err.contains("function stamp() does not exist"), "{family:?}: {err}");
        ok(&db, family, "INSERT INTO items VALUES (1, 'x')");
        // Nothing was registered: the same statement works once the function exists.
        stamp(&db, "stamp", "v1");
        ok(&db, family, STAMP_TRIGGER);
    }
}

#[test]
fn create_or_replace_trigger_replaces_and_a_plain_create_refuses_a_taken_name() {
    for family in FAMILIES {
        let db = db_with_tables();
        stamp(&db, "stamp", "v1");
        stamp(&db, "other", "other");
        ok(&db, family, STAMP_TRIGGER);
        let err = run(&db, family, STAMP_TRIGGER).expect_err("taken").to_string();
        assert!(err.contains("already exists"), "{family:?}: {err}");
        ok(
            &db,
            family,
            "CREATE OR REPLACE TRIGGER items_stamp BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION other()",
        );
        ok(&db, family, "INSERT INTO items VALUES (1, 'x')");
        assert_eq!(rows(&db, "SELECT note FROM items"), vec!["other"], "{family:?}");
    }
}

#[test]
fn drop_trigger_stops_it_and_if_exists_tolerates_a_missing_one() {
    for family in FAMILIES {
        let db = db_with_tables();
        stamp(&db, "stamp", "v1");
        ok(&db, family, STAMP_TRIGGER);
        ok(&db, family, "DROP TRIGGER items_stamp ON items");
        ok(&db, family, "INSERT INTO items VALUES (1, 'x')");
        assert_eq!(rows(&db, "SELECT note FROM items"), vec!["x"], "{family:?}");
        assert!(
            run(&db, family, "DROP TRIGGER items_stamp ON items").is_err(),
            "{family:?}"
        );
        ok(&db, family, "DROP TRIGGER IF EXISTS items_stamp ON items");
        // Without ON <table>: found by name.
        ok(&db, family, STAMP_TRIGGER);
        ok(&db, family, "DROP TRIGGER items_stamp");
        ok(&db, family, "DROP TRIGGER IF EXISTS items_stamp");
        assert!(run(&db, family, "DROP TRIGGER items_stamp").is_err(), "{family:?}");
        // The function is free to go now.
        ok(&db, family, "DROP FUNCTION stamp");
    }
}

#[test]
fn disable_and_enable_trigger_by_name_and_all() {
    for family in FAMILIES {
        let db = db_with_tables();
        stamp(&db, "stamp", "v1");
        ok(&db, family, STAMP_TRIGGER);
        ok(&db, family, "ALTER TABLE items DISABLE TRIGGER items_stamp");
        ok(&db, family, "INSERT INTO items VALUES (1, 'x')");
        ok(&db, family, "ALTER TABLE items ENABLE TRIGGER items_stamp");
        ok(&db, family, "INSERT INTO items VALUES (2, 'x')");
        ok(&db, family, "ALTER TABLE items DISABLE TRIGGER ALL");
        ok(&db, family, "INSERT INTO items VALUES (3, 'x')");
        ok(&db, family, "ALTER TABLE items ENABLE TRIGGER USER");
        ok(&db, family, "INSERT INTO items VALUES (4, 'x')");
        assert_eq!(
            rows(&db, "SELECT id, note FROM items ORDER BY id"),
            vec!["1|x", "2|v1", "3|x", "4|v1"],
            "{family:?}"
        );
        let err = run(&db, family, "ALTER TABLE items DISABLE TRIGGER nope")
            .expect_err("no such trigger")
            .to_string();
        assert!(
            err.contains("\"nope\"") && err.contains("does not exist"),
            "{family:?}: {err}"
        );
    }
}

#[test]
fn alter_trigger_rename_keeps_it_firing_under_the_new_name() {
    for family in FAMILIES {
        let db = db_with_tables();
        function(&db, "who", "BEGIN NEW.note := TG_NAME; RETURN NEW; END");
        ok(
            &db,
            family,
            "CREATE TRIGGER old_name BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION who()",
        );
        ok(&db, family, "ALTER TRIGGER old_name ON items RENAME TO new_name");
        ok(&db, family, "INSERT INTO items VALUES (1, 'x')");
        assert_eq!(rows(&db, "SELECT note FROM items"), vec!["new_name"], "{family:?}");
        assert!(
            run(&db, family, "DROP TRIGGER old_name ON items").is_err(),
            "{family:?}"
        );
        ok(&db, family, "DROP TRIGGER new_name ON items");
    }
}

#[test]
fn forms_that_cannot_fire_are_refused_at_create() {
    for family in FAMILIES {
        let db = db_with_tables();
        db.execute("CREATE VIEW items_v AS SELECT id, note FROM items").unwrap();
        stamp(&db, "stamp", "v1");
        db.execute("CREATE FUNCTION sql_fn() RETURNS INT AS $$ SELECT 1 $$ LANGUAGE sql")
            .unwrap();
        for (sql, what) in [
            (
                "CREATE TRIGGER t INSTEAD OF INSERT ON items_v FOR EACH ROW EXECUTE FUNCTION stamp()",
                "INSTEAD OF",
            ),
            (
                "CREATE CONSTRAINT TRIGGER t AFTER INSERT ON items DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE \
                 FUNCTION stamp()",
                "constraint and deferrable",
            ),
            (
                "CREATE TRIGGER t AFTER INSERT ON items REFERENCING NEW TABLE AS fresh FOR EACH STATEMENT EXECUTE \
                 FUNCTION stamp()",
                "REFERENCING",
            ),
            (
                "CREATE TRIGGER t BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION sql_fn()",
                "LANGUAGE sql",
            ),
        ] {
            let err = run(&db, family, sql).expect_err(what).to_string();
            assert!(
                err.contains(what) && err.contains("trigger form is not supported"),
                "{family:?}: `{sql}`: {err}"
            );
        }
        // Nothing was registered by the refused statements.
        ok(&db, family, "INSERT INTO items VALUES (1, 'x')");
        assert_eq!(rows(&db, "SELECT note FROM items"), vec!["x"], "{family:?}");
        let err = run(
            &db,
            family,
            "CREATE TRIGGER t AFTER UPDATE OF nope ON items FOR EACH ROW EXECUTE FUNCTION stamp()",
        )
        .expect_err("no such column")
        .to_string();
        assert!(err.contains("\"nope\""), "{family:?}: {err}");
    }
}

#[test]
fn triggers_survive_a_restart_with_their_function_arguments_and_enabled_state() {
    let dir = scratch_dir("restart");
    {
        let db = EmbeddedDatabase::new(&dir).expect("open");
        tables(&db);
        function(
            &db,
            "tag",
            "BEGIN NEW.note := TG_ARGV[0] || ':' || NEW.note; RETURN NEW; END",
        );
        function(
            &db,
            "audit",
            "BEGIN INSERT INTO log (line) VALUES ('ins ' || NEW.id); RETURN NEW; END",
        );
        db.execute("CREATE TRIGGER a_tag BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION tag('first')")
            .unwrap();
        db.execute(
            "CREATE TRIGGER b_audit AFTER INSERT ON items FOR EACH ROW WHEN (NEW.id > 1) EXECUTE FUNCTION audit()",
        )
        .unwrap();
        db.execute("CREATE TRIGGER c_off AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION audit()")
            .unwrap();
        db.execute("ALTER TABLE items DISABLE TRIGGER c_off").unwrap();
        db.execute("ALTER TRIGGER a_tag ON items RENAME TO a_tag2").unwrap();
        db.execute("INSERT INTO items VALUES (1, 'x')").unwrap();
    }
    {
        let db = EmbeddedDatabase::new(&dir).expect("reopen");
        db.execute("INSERT INTO items VALUES (2, 'y')").unwrap();
        assert_eq!(
            rows(&db, "SELECT id, note FROM items ORDER BY id"),
            vec!["1|first:x", "2|first:y"]
        );
        assert_eq!(
            rows(&db, "SELECT line FROM log ORDER BY seq"),
            vec!["ins 2"],
            "the WHEN condition and the disabled state came back"
        );
        db.execute("DROP TRIGGER a_tag2 ON items").unwrap();
    }
    {
        let db = EmbeddedDatabase::new(&dir).expect("reopen again");
        db.execute("INSERT INTO items VALUES (3, 'z')").unwrap();
        assert_eq!(
            rows(&db, "SELECT note FROM items WHERE id = 3"),
            vec!["z"],
            "the drop is durable"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn renaming_or_dropping_the_table_takes_its_triggers_along() {
    let db = db_with_tables();
    stamp(&db, "stamp", "v1");
    db.execute(STAMP_TRIGGER).unwrap();
    db.execute("ALTER TABLE items RENAME TO things").unwrap();
    db.execute("INSERT INTO things VALUES (1, 'x')").unwrap();
    assert_eq!(rows(&db, "SELECT note FROM things"), vec!["v1"]);
    db.execute("DROP TABLE things").unwrap();
    // The function is no longer used by any trigger.
    db.execute("DROP FUNCTION stamp").unwrap();
}
