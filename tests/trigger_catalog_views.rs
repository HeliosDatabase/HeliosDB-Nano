#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! `information_schema.triggers` and `pg_trigger` list the triggers that
//! exist, with what they run.

use heliosdb_nano::{EmbeddedDatabase, Value};

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

fn db_with_triggers() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, qty INT, note TEXT)")
        .unwrap();
    db.execute("CREATE FUNCTION audit() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END $$ LANGUAGE plpgsql")
        .unwrap();
    db.execute(
        "CREATE TRIGGER b_audit AFTER INSERT OR UPDATE OF qty ON items FOR EACH ROW WHEN (NEW.qty > 0) EXECUTE \
         FUNCTION audit('a', 'it''s')",
    )
    .unwrap();
    db.execute("CREATE TRIGGER a_stmt BEFORE DELETE ON items FOR EACH STATEMENT EXECUTE FUNCTION audit()")
        .unwrap();
    db.execute("CREATE TRIGGER c_inline BEFORE INSERT ON items FOR EACH ROW SET NEW.note = 'x'")
        .unwrap();
    db
}

#[test]
fn both_views_are_empty_without_triggers() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    assert!(rows(&db, "SELECT * FROM information_schema.triggers").is_empty());
    assert!(rows(&db, "SELECT * FROM pg_trigger").is_empty());
}

#[test]
fn information_schema_triggers_has_one_row_per_trigger_and_event() {
    let db = db_with_triggers();
    assert_eq!(
        rows(
            &db,
            "SELECT trigger_name, event_manipulation, event_object_schema, event_object_table, action_orientation, \
             action_timing, action_statement FROM information_schema.triggers ORDER BY trigger_name, \
             event_manipulation"
        ),
        vec![
            "a_stmt|DELETE|public|items|STATEMENT|BEFORE|EXECUTE FUNCTION audit()",
            "b_audit|INSERT|public|items|ROW|AFTER|EXECUTE FUNCTION audit('a', 'it''s')",
            "b_audit|UPDATE|public|items|ROW|AFTER|EXECUTE FUNCTION audit('a', 'it''s')",
            "c_inline|INSERT|public|items|ROW|BEFORE|SET NEW.note = 'x'",
        ]
    );
    let condition = rows(
        &db,
        "SELECT action_condition FROM information_schema.triggers WHERE trigger_name = 'b_audit' AND \
         event_manipulation = 'INSERT'",
    );
    assert!(
        condition[0].contains("qty"),
        "the WHEN condition is shown: {condition:?}"
    );
    assert_eq!(
        rows(
            &db,
            "SELECT action_condition FROM information_schema.triggers WHERE trigger_name = 'a_stmt'"
        ),
        vec!["NULL"]
    );
}

#[test]
fn pg_trigger_has_one_row_per_trigger_with_type_bits_and_enabled_state() {
    let db = db_with_triggers();
    db.execute("ALTER TABLE items DISABLE TRIGGER c_inline").unwrap();
    assert_eq!(
        rows(
            &db,
            "SELECT tgname, tgrelname, tgfname, tgtype, tgenabled, tgnargs FROM pg_trigger ORDER BY tgname"
        ),
        vec![
            // statement (0) + before (2) + delete (8)
            "a_stmt|items|audit|10|O|0",
            // row (1) + after (0) + insert (4) + update (16)
            "b_audit|items|audit|21|O|2",
            // row (1) + before (2) + insert (4); an inline body has no function
            "c_inline|items|NULL|7|D|0",
        ]
    );
}

#[test]
fn the_views_follow_drop_rename_and_drop_table() {
    let db = db_with_triggers();
    db.execute("DROP TRIGGER a_stmt ON items").unwrap();
    db.execute("ALTER TRIGGER c_inline ON items RENAME TO c_renamed")
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT tgname FROM pg_trigger ORDER BY tgname"),
        vec!["b_audit", "c_renamed"]
    );
    db.execute("DROP TABLE items").unwrap();
    assert!(rows(&db, "SELECT tgname FROM pg_trigger").is_empty());
    assert!(rows(&db, "SELECT trigger_name FROM information_schema.triggers").is_empty());
}
