#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! Triggers run their function: row-level BEFORE and AFTER for INSERT, UPDATE
//! and DELETE with NEW and OLD, statement-level triggers, TRUNCATE, WHEN,
//! UPDATE OF, firing order, the return value, and the TG_ variables.
//!
//! Every case runs on both statement families: `execute` (the embedded API,
//! the simple protocol, the MySQL wire, the shell) and `execute_params` (the
//! extended protocol, REST).

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

fn text(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::String(s) => s.clone(),
        other => format!("{other}"),
    }
}

/// Every row of `sql`, each as its values joined with `|`.
fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<String> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .iter()
        .map(|row| row.values.iter().map(text).collect::<Vec<_>>().join("|"))
        .collect()
}

/// `items(id, qty, note)` and `log(seq, line)`.
fn db_with_tables() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, qty INT, note TEXT)")
        .unwrap();
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

fn log_lines(db: &EmbeddedDatabase) -> Vec<String> {
    rows(db, "SELECT line FROM log ORDER BY seq")
}

#[test]
fn an_after_insert_row_trigger_writes_one_audit_row_per_inserted_row() {
    for family in FAMILIES {
        let db = db_with_tables();
        function(
            &db,
            "audit",
            "BEGIN INSERT INTO log (line) VALUES (TG_OP || ' ' || NEW.id || ' ' || NEW.note); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_audit AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION audit()",
        );
        assert_eq!(ok(&db, family, "INSERT INTO items VALUES (1, 5, 'a'), (2, 6, 'b')"), 2);
        ok(&db, family, "INSERT INTO items VALUES (3, 7, 'c')");
        assert_eq!(
            log_lines(&db),
            vec!["INSERT 1 a", "INSERT 2 b", "INSERT 3 c"],
            "{family:?}"
        );
    }
}

#[test]
fn a_before_insert_row_trigger_changes_the_stored_row() {
    for family in FAMILIES {
        let db = db_with_tables();
        function(
            &db,
            "clamp",
            "BEGIN IF NEW.qty < 0 THEN NEW.qty := 0; NEW.note := 'clamped'; END IF; RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_clamp BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION clamp()",
        );
        ok(&db, family, "INSERT INTO items VALUES (1, -4, 'a'), (2, 9, 'b')");
        assert_eq!(
            rows(&db, "SELECT id, qty, note FROM items ORDER BY id"),
            vec!["1|0|clamped", "2|9|b"],
            "{family:?}"
        );
    }
}

#[test]
fn a_before_row_trigger_that_returns_null_skips_the_row() {
    for family in FAMILIES {
        let db = db_with_tables();
        function(
            &db,
            "skip_odd",
            "BEGIN IF NEW.id % 2 = 1 THEN RETURN NULL; END IF; RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_skip BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION skip_odd()",
        );
        assert_eq!(
            ok(
                &db,
                family,
                "INSERT INTO items VALUES (1, 1, 'a'), (2, 2, 'b'), (3, 3, 'c')"
            ),
            1,
            "{family:?}: only the kept row is counted"
        );
        assert_eq!(rows(&db, "SELECT id FROM items"), vec!["2"], "{family:?}");
    }
}

#[test]
fn update_triggers_see_old_and_new_and_a_before_trigger_changes_new() {
    for family in FAMILIES {
        let db = db_with_tables();
        db.execute("INSERT INTO items VALUES (1, 10, 'a'), (2, 20, 'b')")
            .unwrap();
        function(
            &db,
            "bump",
            "BEGIN NEW.note := OLD.note || '>' || NEW.qty; RETURN NEW; END",
        );
        function(
            &db,
            "audit_upd",
            "BEGIN INSERT INTO log (line) VALUES (TG_OP || ' ' || OLD.qty || '->' || NEW.qty || ' ' || NEW.note); \
             RETURN NULL; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER a_bump BEFORE UPDATE ON items FOR EACH ROW EXECUTE FUNCTION bump()",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER b_audit AFTER UPDATE ON items FOR EACH ROW EXECUTE FUNCTION audit_upd()",
        );
        assert_eq!(ok(&db, family, "UPDATE items SET qty = qty + 1 WHERE id = 1"), 1);
        assert_eq!(
            rows(&db, "SELECT id, qty, note FROM items ORDER BY id"),
            vec!["1|11|a>11", "2|20|b"],
            "{family:?}"
        );
        // The AFTER trigger returned NULL: its return value is ignored.
        assert_eq!(log_lines(&db), vec!["UPDATE 10->11 a>11"], "{family:?}");
    }
}

#[test]
fn a_before_update_trigger_that_returns_null_leaves_the_row_as_it_was() {
    for family in FAMILIES {
        let db = db_with_tables();
        db.execute("INSERT INTO items VALUES (1, 10, 'locked'), (2, 20, 'b')")
            .unwrap();
        function(
            &db,
            "keep_locked",
            "BEGIN IF OLD.note = 'locked' THEN RETURN NULL; END IF; RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_lock BEFORE UPDATE ON items FOR EACH ROW EXECUTE FUNCTION keep_locked()",
        );
        assert_eq!(ok(&db, family, "UPDATE items SET qty = 0"), 1, "{family:?}");
        assert_eq!(
            rows(&db, "SELECT id, qty FROM items ORDER BY id"),
            vec!["1|10", "2|0"],
            "{family:?}"
        );
    }
}

#[test]
fn delete_triggers_see_old_and_a_before_trigger_can_keep_the_row() {
    for family in FAMILIES {
        let db = db_with_tables();
        db.execute("INSERT INTO items VALUES (1, 10, 'keep'), (2, 20, 'b'), (3, 30, 'c')")
            .unwrap();
        function(
            &db,
            "guard",
            "BEGIN IF OLD.note = 'keep' THEN RETURN NULL; END IF; RETURN OLD; END",
        );
        function(
            &db,
            "audit_del",
            "BEGIN INSERT INTO log (line) VALUES (TG_OP || ' ' || OLD.id); RETURN OLD; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_guard BEFORE DELETE ON items FOR EACH ROW EXECUTE FUNCTION guard()",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_gone AFTER DELETE ON items FOR EACH ROW EXECUTE FUNCTION audit_del()",
        );
        assert_eq!(ok(&db, family, "DELETE FROM items WHERE id <= 2"), 1, "{family:?}");
        assert_eq!(
            rows(&db, "SELECT id FROM items ORDER BY id"),
            vec!["1", "3"],
            "{family:?}"
        );
        assert_eq!(log_lines(&db), vec!["DELETE 2"], "{family:?}");
    }
}

#[test]
fn one_trigger_for_several_events_tells_them_apart_with_tg_op() {
    for family in FAMILIES {
        let db = db_with_tables();
        function(
            &db,
            "any_change",
            "BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO log (line) VALUES ('del ' || OLD.id); RETURN OLD; END IF; \
             INSERT INTO log (line) VALUES (lower(TG_OP) || ' ' || NEW.id); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_any AFTER INSERT OR UPDATE OR DELETE ON items FOR EACH ROW EXECUTE FUNCTION \
             any_change()",
        );
        ok(&db, family, "INSERT INTO items VALUES (1, 1, 'a')");
        ok(&db, family, "UPDATE items SET qty = 2 WHERE id = 1");
        ok(&db, family, "DELETE FROM items WHERE id = 1");
        assert_eq!(log_lines(&db), vec!["insert 1", "update 1", "del 1"], "{family:?}");
    }
}

#[test]
fn statement_level_triggers_fire_once_before_and_once_after_even_for_no_rows() {
    for family in FAMILIES {
        let db = db_with_tables();
        function(
            &db,
            "mark",
            "BEGIN INSERT INTO log (line) VALUES (TG_WHEN || ' ' || TG_LEVEL || ' ' || TG_OP); RETURN NULL; END",
        );
        function(
            &db,
            "row_mark",
            "BEGIN INSERT INTO log (line) VALUES ('row ' || NEW.id); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER s_before BEFORE INSERT OR UPDATE OR DELETE ON items FOR EACH STATEMENT EXECUTE FUNCTION \
             mark()",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER s_after AFTER INSERT OR UPDATE OR DELETE ON items FOR EACH STATEMENT EXECUTE FUNCTION \
             mark()",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER r_after AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION row_mark()",
        );
        ok(&db, family, "INSERT INTO items VALUES (1, 1, 'a'), (2, 2, 'b')");
        ok(&db, family, "UPDATE items SET qty = 0 WHERE id = 99");
        ok(&db, family, "DELETE FROM items WHERE id = 99");
        assert_eq!(
            log_lines(&db),
            vec![
                "BEFORE STATEMENT INSERT",
                "row 1",
                "row 2",
                "AFTER STATEMENT INSERT",
                "BEFORE STATEMENT UPDATE",
                "AFTER STATEMENT UPDATE",
                "BEFORE STATEMENT DELETE",
                "AFTER STATEMENT DELETE",
            ],
            "{family:?}"
        );
    }
}

#[test]
fn truncate_fires_its_statement_level_triggers() {
    for family in FAMILIES {
        let db = db_with_tables();
        db.execute("INSERT INTO items VALUES (1, 1, 'a')").unwrap();
        function(
            &db,
            "mark",
            "BEGIN INSERT INTO log (line) VALUES (TG_WHEN || ' ' || TG_OP || ' ' || TG_TABLE_NAME); RETURN NULL; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER t_before BEFORE TRUNCATE ON items FOR EACH STATEMENT EXECUTE FUNCTION mark()",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER t_after AFTER TRUNCATE ON items FOR EACH STATEMENT EXECUTE FUNCTION mark()",
        );
        ok(&db, family, "TRUNCATE TABLE items");
        assert!(rows(&db, "SELECT id FROM items").is_empty(), "{family:?}");
        assert_eq!(
            log_lines(&db),
            vec!["BEFORE TRUNCATE items", "AFTER TRUNCATE items"],
            "{family:?}"
        );
    }
}

#[test]
fn the_when_condition_decides_per_row_whether_the_trigger_fires() {
    for family in FAMILIES {
        let db = db_with_tables();
        db.execute("INSERT INTO items VALUES (1, 10, 'a'), (2, 20, 'b')")
            .unwrap();
        function(
            &db,
            "note_it",
            "BEGIN INSERT INTO log (line) VALUES (TG_NAME || ' ' || NEW.id); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER big_insert AFTER INSERT ON items FOR EACH ROW WHEN (NEW.qty > 100) EXECUTE FUNCTION \
             note_it()",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER qty_changed AFTER UPDATE ON items FOR EACH ROW WHEN (OLD.qty IS DISTINCT FROM NEW.qty) \
             EXECUTE FUNCTION note_it()",
        );
        ok(&db, family, "INSERT INTO items VALUES (3, 5, 'c'), (4, 500, 'd')");
        ok(&db, family, "UPDATE items SET note = 'same qty' WHERE id = 1");
        ok(&db, family, "UPDATE items SET qty = 21 WHERE id = 2");
        assert_eq!(log_lines(&db), vec!["big_insert 4", "qty_changed 2"], "{family:?}");
    }
}

#[test]
fn update_of_fires_only_when_a_listed_column_is_assigned() {
    for family in FAMILIES {
        let db = db_with_tables();
        db.execute("INSERT INTO items VALUES (1, 10, 'a')").unwrap();
        function(
            &db,
            "note_it",
            "BEGIN INSERT INTO log (line) VALUES ('qty of ' || NEW.id); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER qty_only AFTER UPDATE OF qty ON items FOR EACH ROW EXECUTE FUNCTION note_it()",
        );
        ok(&db, family, "UPDATE items SET note = 'x' WHERE id = 1");
        assert!(log_lines(&db).is_empty(), "{family:?}: note is not a listed column");
        ok(&db, family, "UPDATE items SET note = 'y', qty = 11 WHERE id = 1");
        assert_eq!(log_lines(&db), vec!["qty of 1"], "{family:?}");
    }
}

#[test]
fn triggers_of_one_kind_fire_in_name_order_and_each_sees_the_previous_change() {
    for family in FAMILIES {
        let db = db_with_tables();
        function(&db, "add_b", "BEGIN NEW.note := NEW.note || 'b'; RETURN NEW; END");
        function(&db, "add_a", "BEGIN NEW.note := NEW.note || 'a'; RETURN NEW; END");
        function(&db, "add_c", "BEGIN NEW.note := NEW.note || 'c'; RETURN NEW; END");
        // Created out of order on purpose.
        for (trigger, function) in [("t2", "add_b"), ("t3", "add_c"), ("t1", "add_a")] {
            ok(
                &db,
                family,
                &format!("CREATE TRIGGER {trigger} BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION {function}()"),
            );
        }
        ok(&db, family, "INSERT INTO items VALUES (1, 1, '>')");
        assert_eq!(rows(&db, "SELECT note FROM items"), vec![">abc"], "{family:?}");
    }
}

#[test]
fn the_tg_variables_and_arguments_describe_the_firing() {
    for family in FAMILIES {
        let db = db_with_tables();
        function(
            &db,
            "describe",
            "BEGIN INSERT INTO log (line) VALUES (TG_NAME || ',' || TG_WHEN || ',' || TG_LEVEL || ',' || TG_OP || ',' \
             || TG_TABLE_SCHEMA || '.' || TG_TABLE_NAME || ',' || TG_NARGS || ',' || TG_ARGV[0] || ',' || TG_ARGV[1]); \
             RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_desc BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION describe('first', 42)",
        );
        ok(&db, family, "INSERT INTO items VALUES (1, 1, 'a')");
        assert_eq!(
            log_lines(&db),
            vec!["items_desc,BEFORE,ROW,INSERT,public.items,2,first,42"],
            "{family:?}"
        );
    }
}

#[test]
fn a_trigger_function_reads_what_the_statement_has_written_so_far() {
    for family in FAMILIES {
        let db = db_with_tables();
        // AFTER ROW triggers run when every row of the statement is in place.
        function(
            &db,
            "count_rows",
            "DECLARE n INT; BEGIN SELECT count(*) INTO n FROM items; INSERT INTO log (line) VALUES (NEW.id || ' of ' \
             || $n); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_count AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION count_rows()",
        );
        ok(&db, family, "INSERT INTO items VALUES (1, 1, 'a'), (2, 2, 'b')");
        assert_eq!(log_lines(&db), vec!["1 of 2", "2 of 2"], "{family:?}");
    }
}

#[test]
fn an_after_insert_trigger_sees_the_key_the_write_generated() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE orders (id SERIAL PRIMARY KEY, what TEXT)")
            .unwrap();
        db.execute("CREATE TABLE log (seq SERIAL PRIMARY KEY, line TEXT)")
            .unwrap();
        function(
            &db,
            "audit",
            "BEGIN INSERT INTO log (line) VALUES (NEW.id || ':' || NEW.what); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER orders_audit AFTER INSERT ON orders FOR EACH ROW EXECUTE FUNCTION audit()",
        );
        ok(&db, family, "INSERT INTO orders (what) VALUES ('tea')");
        ok(&db, family, "INSERT INTO orders (what) VALUES ('milk'), ('salt')");
        let stored = rows(&db, "SELECT id || ':' || what FROM orders ORDER BY id");
        assert_eq!(stored.len(), 3, "{family:?}");
        assert_eq!(log_lines(&db), stored, "{family:?}: NEW.id is the stored key");
    }
}

#[test]
fn a_table_without_a_trigger_is_not_touched_by_another_tables_trigger() {
    for family in FAMILIES {
        let db = db_with_tables();
        db.execute("CREATE TABLE other (id INT PRIMARY KEY, qty INT)").unwrap();
        function(
            &db,
            "audit",
            "BEGIN INSERT INTO log (line) VALUES ('items ' || NEW.id); RETURN NEW; END",
        );
        ok(
            &db,
            family,
            "CREATE TRIGGER items_audit AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION audit()",
        );
        ok(&db, family, "INSERT INTO other VALUES (1, 1)");
        ok(&db, family, "UPDATE other SET qty = 2");
        ok(&db, family, "DELETE FROM other");
        assert!(log_lines(&db).is_empty(), "{family:?}");
    }
}

#[test]
fn a_function_that_ends_without_return_fails_the_statement() {
    for family in FAMILIES {
        let db = db_with_tables();
        function(&db, "forgot", "BEGIN NEW.note := 'x'; END");
        ok(
            &db,
            family,
            "CREATE TRIGGER items_forgot BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION forgot()",
        );
        let err = run(&db, family, "INSERT INTO items VALUES (1, 1, 'a')")
            .expect_err("no RETURN")
            .to_string();
        assert!(err.contains("without RETURN"), "{family:?}: {err}");
        assert!(rows(&db, "SELECT id FROM items").is_empty(), "{family:?}");
    }
}
