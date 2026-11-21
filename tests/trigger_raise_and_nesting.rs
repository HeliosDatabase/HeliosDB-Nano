#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! A trigger function's errors and writes: RAISE EXCEPTION fails the
//! statement and leaves nothing of it, the function's own writes belong to
//! the firing transaction, a write in a function fires the triggers of the
//! table it writes to, and a chain that never ends is stopped.

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
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, qty INT)").unwrap();
    db.execute("CREATE TABLE log (seq SERIAL PRIMARY KEY, line TEXT)")
        .unwrap();
    db
}

fn function(db: &EmbeddedDatabase, name: &str, body: &str) {
    db.execute(&format!(
        "CREATE OR REPLACE FUNCTION {name}() RETURNS TRIGGER AS $$ {body} $$ LANGUAGE plpgsql"
    ))
    .unwrap_or_else(|e| panic!("CREATE FUNCTION {name}: {e}"));
}

#[test]
fn raise_exception_in_a_before_trigger_fails_the_statement_and_stores_nothing() {
    for family in FAMILIES {
        let db = db_with_tables();
        function(
            &db,
            "positive",
            "BEGIN IF NEW.qty < 0 THEN RAISE EXCEPTION 'qty % is negative for item %', NEW.qty, NEW.id; END IF; \
             INSERT INTO log (line) VALUES ('accepted ' || NEW.id); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_positive BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION positive()",
        );
        let err = run(&db, family, "INSERT INTO items VALUES (1, 5), (2, -3), (3, 7)")
            .expect_err("the second row is refused")
            .to_string();
        assert!(err.contains("qty -3 is negative for item 2"), "{family:?}: {err}");
        assert!(
            err.contains("items_positive"),
            "{family:?}: the trigger is named: {err}"
        );
        // Neither the first row nor the audit line the function wrote for it stays.
        assert!(rows(&db, "SELECT id FROM items").is_empty(), "{family:?}");
        assert!(rows(&db, "SELECT line FROM log").is_empty(), "{family:?}");
        // The next statement works.
        ok(&db, family, "INSERT INTO items VALUES (4, 1)");
        assert_eq!(rows(&db, "SELECT line FROM log"), vec!["accepted 4"], "{family:?}");
    }
}

#[test]
fn raise_exception_in_an_after_trigger_undoes_the_rows_already_written() {
    for family in FAMILIES {
        let db = db_with_tables();
        db.execute("INSERT INTO items VALUES (1, 10), (2, 20)").unwrap();
        function(
            &db,
            "no_zero",
            "BEGIN IF NEW.qty = 0 THEN RAISE EXCEPTION SQLSTATE '23514' 'item % would be empty', NEW.id; END IF; \
             RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_no_zero AFTER UPDATE ON items FOR EACH ROW EXECUTE FUNCTION no_zero()",
        );
        let err = run(&db, family, "UPDATE items SET qty = qty - 10")
            .expect_err("item 1 would be empty")
            .to_string();
        assert!(err.contains("item 1 would be empty"), "{family:?}: {err}");
        assert!(err.contains("23514"), "{family:?}: the SQLSTATE travels with it: {err}");
        assert_eq!(
            rows(&db, "SELECT id, qty FROM items ORDER BY id"),
            vec!["1|10", "2|20"],
            "{family:?}"
        );
        let err = run(&db, family, "DELETE FROM items WHERE id = 99").map(|_| ());
        assert!(err.is_ok(), "{family:?}");
    }
}

#[test]
fn a_trigger_functions_writes_roll_back_with_the_transaction_and_with_a_savepoint() {
    let db = db_with_tables();
    function(
        &db,
        "audit",
        "BEGIN INSERT INTO log (line) VALUES ('ins ' || NEW.id); RETURN NEW; END",
    );
    db.execute("CREATE TRIGGER items_audit AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION audit()")
        .unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO items VALUES (1, 1)").unwrap();
    assert_eq!(
        rows(&db, "SELECT line FROM log"),
        vec!["ins 1"],
        "visible inside the transaction"
    );
    db.execute("ROLLBACK").unwrap();
    assert!(rows(&db, "SELECT id FROM items").is_empty());
    assert!(
        rows(&db, "SELECT line FROM log").is_empty(),
        "the audit row rolled back too"
    );

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO items VALUES (2, 2)").unwrap();
    db.execute("SAVEPOINT s").unwrap();
    db.execute("INSERT INTO items VALUES (3, 3)").unwrap();
    db.execute("ROLLBACK TO SAVEPOINT s").unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(rows(&db, "SELECT id FROM items"), vec!["2"]);
    assert_eq!(rows(&db, "SELECT line FROM log"), vec!["ins 2"]);
}

#[test]
fn a_write_in_a_trigger_function_fires_the_triggers_of_the_table_it_writes_to() {
    for family in FAMILIES {
        let db = db_with_tables();
        db.execute("CREATE TABLE totals (id INT PRIMARY KEY, total INT)")
            .unwrap();
        db.execute("INSERT INTO totals VALUES (1, 0)").unwrap();
        function(
            &db,
            "add_to_total",
            "BEGIN UPDATE totals SET total = total + NEW.qty WHERE id = 1; RETURN NEW; END",
        );
        function(
            &db,
            "log_total",
            "BEGIN INSERT INTO log (line) VALUES ('total ' || OLD.total || '->' || NEW.total); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_total AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION add_to_total()",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER totals_log AFTER UPDATE ON totals FOR EACH ROW EXECUTE FUNCTION log_total()",
        );
        ok(&db, family, "INSERT INTO items VALUES (1, 5), (2, 7)");
        assert_eq!(rows(&db, "SELECT total FROM totals"), vec!["12"], "{family:?}");
        assert_eq!(
            rows(&db, "SELECT line FROM log ORDER BY seq"),
            vec!["total 0->5", "total 5->12"],
            "{family:?}"
        );
    }
}

#[test]
fn a_trigger_chain_that_never_ends_is_stopped_with_a_clear_error_and_nothing_is_written() {
    for family in FAMILIES {
        let db = db_with_tables();
        // Every insert into items inserts the next row into items.
        function(
            &db,
            "again",
            "BEGIN INSERT INTO items VALUES (NEW.id + 1, NEW.qty); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_again AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION again()",
        );
        let err = run(&db, family, "INSERT INTO items VALUES (1, 1)")
            .expect_err("the chain has no end")
            .to_string();
        assert!(err.contains("trigger nesting is too deep"), "{family:?}: {err}");
        assert!(err.contains("items_again"), "{family:?}: {err}");
        assert!(rows(&db, "SELECT id FROM items").is_empty(), "{family:?}");
    }
}

#[test]
fn a_bounded_chain_of_nested_triggers_completes() {
    let db = db_with_tables();
    // Counts down: each insert inserts the next smaller id until 0.
    function(
        &db,
        "down",
        "BEGIN IF NEW.id > 0 THEN INSERT INTO items VALUES (NEW.id - 1, NEW.qty); END IF; RETURN NEW; END",
    );
    db.execute("CREATE TRIGGER items_down AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION down()")
        .unwrap();
    db.execute("INSERT INTO items VALUES (3, 1)").unwrap();
    assert_eq!(rows(&db, "SELECT id FROM items ORDER BY id"), vec!["0", "1", "2", "3"]);
}

#[test]
fn an_error_in_the_functions_sql_fails_the_statement_and_names_the_trigger() {
    for family in FAMILIES {
        let db = db_with_tables();
        function(
            &db,
            "broken",
            "BEGIN INSERT INTO no_such_table VALUES (NEW.id); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_broken AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION broken()",
        );
        let err = run(&db, family, "INSERT INTO items VALUES (1, 1)")
            .expect_err("the function's INSERT fails")
            .to_string();
        assert!(err.contains("no_such_table"), "{family:?}: {err}");
        assert!(err.contains("items_broken"), "{family:?}: {err}");
        assert!(rows(&db, "SELECT id FROM items").is_empty(), "{family:?}");
    }
}

#[test]
fn a_field_the_table_does_not_have_and_a_record_the_event_does_not_have_are_errors() {
    for family in FAMILIES {
        let db = db_with_tables();
        function(&db, "bad_field", "BEGIN NEW.nope := 1; RETURN NEW; END");
        function(
            &db,
            "bad_record",
            "BEGIN INSERT INTO log (line) VALUES ('was ' || OLD.qty); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER a_field BEFORE UPDATE ON items FOR EACH ROW EXECUTE FUNCTION bad_field()",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER b_record AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION bad_record()",
        );
        let err = run(&db, family, "INSERT INTO items VALUES (1, 1)")
            .expect_err("OLD in an INSERT trigger")
            .to_string();
        assert!(err.contains("\"old\" is not assigned yet"), "{family:?}: {err}");
        db.execute("ALTER TABLE items DISABLE TRIGGER b_record").unwrap();
        db.execute("INSERT INTO items VALUES (1, 1)").unwrap();
        let err = run(&db, family, "UPDATE items SET qty = 2")
            .expect_err("no such field")
            .to_string();
        assert!(err.contains("has no field \"nope\""), "{family:?}: {err}");
        assert_eq!(rows(&db, "SELECT qty FROM items"), vec!["1"], "{family:?}");
    }
}
