#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! Every way a row is written fires the table's triggers: bound parameters,
//! `INSERT … SELECT`, `INSERT … ON CONFLICT` (the insert triggers, and the
//! update triggers for the rows DO UPDATE changes), `RETURNING`, foreign-key
//! actions on a child table, a procedure's writes, and a transaction opened
//! on the handle.

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

fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<String> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .iter()
        .map(|row| row.values.iter().map(text).collect::<Vec<_>>().join("|"))
        .collect()
}

/// `items(id, qty, note)` with a BEFORE trigger that stamps `note`, an AFTER
/// trigger for every event that writes to `log`, and `log(seq, line)`.
fn db_with_audited_items() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, qty INT, note TEXT)")
        .unwrap();
    db.execute("CREATE TABLE log (seq SERIAL PRIMARY KEY, line TEXT)")
        .unwrap();
    db.execute(
        "CREATE FUNCTION stamp() RETURNS TRIGGER AS $$ BEGIN NEW.note := lower(TG_OP); RETURN NEW; END $$ LANGUAGE \
         plpgsql",
    )
    .unwrap();
    db.execute(
        "CREATE FUNCTION audit() RETURNS TRIGGER AS $$ BEGIN \
           IF TG_OP = 'DELETE' THEN INSERT INTO log (line) VALUES ('D ' || OLD.id); RETURN OLD; END IF; \
           IF TG_OP = 'UPDATE' THEN INSERT INTO log (line) VALUES ('U ' || NEW.id || ' ' || OLD.qty || '->' || NEW.qty); \
             RETURN NEW; END IF; \
           INSERT INTO log (line) VALUES ('I ' || NEW.id || ' ' || NEW.qty); RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .unwrap();
    db.execute("CREATE TRIGGER a_stamp BEFORE INSERT OR UPDATE ON items FOR EACH ROW EXECUTE FUNCTION stamp()")
        .unwrap();
    db.execute(
        "CREATE TRIGGER b_audit AFTER INSERT OR UPDATE OR DELETE ON items FOR EACH ROW EXECUTE FUNCTION audit()",
    )
    .unwrap();
    db
}

fn log_lines(db: &EmbeddedDatabase) -> Vec<String> {
    rows(db, "SELECT line FROM log ORDER BY seq")
}

#[test]
fn bound_parameters_fire_triggers_for_insert_update_and_delete() {
    let db = db_with_audited_items();
    db.execute_params(
        "INSERT INTO items (id, qty, note) VALUES ($1, $2, $3)",
        &[Value::Int4(1), Value::Int4(10), Value::String("x".into())],
    )
    .unwrap();
    db.execute_params(
        "UPDATE items SET qty = $1 WHERE id = $2",
        &[Value::Int4(11), Value::Int4(1)],
    )
    .unwrap();
    assert_eq!(rows(&db, "SELECT qty, note FROM items"), vec!["11|update"]);
    db.execute_params("DELETE FROM items WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    assert_eq!(log_lines(&db), vec!["I 1 10", "U 1 10->11", "D 1"]);
}

#[test]
fn returning_shows_the_row_as_the_before_trigger_left_it() {
    let db = db_with_audited_items();
    let returned = db
        .query("INSERT INTO items VALUES (1, 10, 'x') RETURNING id, note", &[])
        .unwrap();
    assert_eq!(returned.len(), 1);
    assert_eq!(text(&returned[0].values[1]), "insert");
    let returned = db
        .query("UPDATE items SET qty = 12 WHERE id = 1 RETURNING qty, note", &[])
        .unwrap();
    assert_eq!(text(&returned[0].values[1]), "update");
    assert_eq!(log_lines(&db), vec!["I 1 10", "U 1 10->12"]);
}

#[test]
fn insert_select_fires_once_per_row() {
    for family in FAMILIES {
        let db = db_with_audited_items();
        db.execute("CREATE TABLE source (id INT PRIMARY KEY, qty INT)").unwrap();
        db.execute("INSERT INTO source VALUES (1, 10), (2, 20), (3, 30)")
            .unwrap();
        assert_eq!(
            ok(
                &db,
                family,
                "INSERT INTO items (id, qty) SELECT id, qty FROM source WHERE qty > 10"
            ),
            2
        );
        assert_eq!(
            rows(&db, "SELECT id, note FROM items ORDER BY id"),
            vec!["2|insert", "3|insert"],
            "{family:?}"
        );
        assert_eq!(log_lines(&db), vec!["I 2 20", "I 3 30"], "{family:?}");
    }
}

#[test]
fn on_conflict_fires_insert_triggers_and_update_triggers_for_the_do_update_rows() {
    for family in FAMILIES {
        let db = db_with_audited_items();
        ok(&db, family, "INSERT INTO items VALUES (1, 10, 'x')");
        ok(
            &db,
            family,
            "INSERT INTO items VALUES (1, 50, 'y'), (2, 20, 'z') ON CONFLICT (id) DO UPDATE SET qty = EXCLUDED.qty",
        );
        assert_eq!(
            rows(&db, "SELECT id, qty, note FROM items ORDER BY id"),
            vec!["1|50|update", "2|20|insert"],
            "{family:?}"
        );
        assert_eq!(log_lines(&db), vec!["I 1 10", "U 1 10->50", "I 2 20"], "{family:?}");

        ok(
            &db,
            family,
            "INSERT INTO items VALUES (2, 99, 'w') ON CONFLICT (id) DO NOTHING",
        );
        assert_eq!(
            log_lines(&db).len(),
            3,
            "{family:?}: DO NOTHING on a conflict writes no row"
        );
    }
}

#[test]
fn a_foreign_key_cascade_fires_the_child_tables_triggers() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id) ON DELETE CASCADE)")
            .unwrap();
        db.execute("CREATE TABLE log (seq SERIAL PRIMARY KEY, line TEXT)")
            .unwrap();
        db.execute(
            "CREATE FUNCTION child_gone() RETURNS TRIGGER AS $$ BEGIN INSERT INTO log (line) VALUES ('child ' || \
             OLD.id || ' of ' || OLD.parent_id); RETURN OLD; END $$ LANGUAGE plpgsql",
        )
        .unwrap();
        db.execute("CREATE TRIGGER child_gone AFTER DELETE ON child FOR EACH ROW EXECUTE FUNCTION child_gone()")
            .unwrap();
        db.execute("INSERT INTO parent VALUES (1), (2)").unwrap();
        db.execute("INSERT INTO child VALUES (10, 1), (11, 1), (20, 2)")
            .unwrap();
        ok(&db, family, "DELETE FROM parent WHERE id = 1");
        assert_eq!(rows(&db, "SELECT id FROM child"), vec!["20"], "{family:?}");
        let mut lines = log_lines(&db);
        lines.sort();
        assert_eq!(lines, vec!["child 10 of 1", "child 11 of 1"], "{family:?}");
    }
}

#[test]
fn a_foreign_key_set_null_fires_the_child_tables_update_triggers() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id) ON DELETE SET NULL)")
            .unwrap();
        db.execute("CREATE TABLE log (seq SERIAL PRIMARY KEY, line TEXT)")
            .unwrap();
        db.execute(
            "CREATE FUNCTION child_orphaned() RETURNS TRIGGER AS $$ BEGIN INSERT INTO log (line) VALUES ('child ' || \
             NEW.id || ' lost ' || OLD.parent_id); RETURN NEW; END $$ LANGUAGE plpgsql",
        )
        .unwrap();
        db.execute(
            "CREATE TRIGGER child_orphaned AFTER UPDATE ON child FOR EACH ROW EXECUTE FUNCTION child_orphaned()",
        )
        .unwrap();
        db.execute("INSERT INTO parent VALUES (1)").unwrap();
        db.execute("INSERT INTO child VALUES (10, 1)").unwrap();
        ok(&db, family, "DELETE FROM parent WHERE id = 1");
        assert_eq!(
            rows(&db, "SELECT id, parent_id FROM child"),
            vec!["10|NULL"],
            "{family:?}"
        );
        assert_eq!(log_lines(&db), vec!["child 10 lost 1"], "{family:?}");
    }
}

#[test]
fn a_procedures_writes_fire_triggers() {
    for family in FAMILIES {
        let db = db_with_audited_items();
        db.execute(
            "CREATE PROCEDURE restock(p_id INT, p_qty INT) LANGUAGE plpgsql AS $$ BEGIN INSERT INTO items VALUES \
             ($p_id, $p_qty, 'p'); UPDATE items SET qty = qty + 1 WHERE id = $p_id; END $$",
        )
        .unwrap();
        ok(&db, family, "CALL restock(7, 70)");
        assert_eq!(
            rows(&db, "SELECT qty, note FROM items"),
            vec!["71|update"],
            "{family:?}"
        );
        assert_eq!(log_lines(&db), vec!["I 7 70", "U 7 70->71"], "{family:?}");
    }
}

#[test]
fn a_transaction_on_the_handle_fires_triggers_and_rolls_their_writes_back() {
    let db = db_with_audited_items();
    let txn = db.begin_transaction().unwrap();
    txn.execute("INSERT INTO items VALUES (1, 10, 'x')", &[]).unwrap();
    txn.rollback().unwrap();
    assert!(rows(&db, "SELECT id FROM items").is_empty());
    assert!(log_lines(&db).is_empty());

    let txn = db.begin_transaction().unwrap();
    txn.execute("INSERT INTO items VALUES (2, 20, 'x')", &[]).unwrap();
    txn.commit().unwrap();
    assert_eq!(rows(&db, "SELECT id, note FROM items"), vec!["2|insert"]);
    assert_eq!(log_lines(&db), vec!["I 2 20"]);
}

#[test]
fn an_embedded_begin_fires_triggers_for_both_families_inside_it() {
    let db = db_with_audited_items();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO items VALUES (1, 10, 'x')").unwrap();
    db.execute_params("INSERT INTO items VALUES ($1, 20, 'x')", &[Value::Int4(2)])
        .unwrap();
    db.execute_params("UPDATE items SET qty = 21 WHERE id = $1", &[Value::Int4(2)])
        .unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(log_lines(&db), vec!["I 1 10", "I 2 20", "U 2 20->21"]);
}
