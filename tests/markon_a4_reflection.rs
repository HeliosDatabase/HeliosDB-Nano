use heliosdb_nano::{protocol::postgres::catalog::PgCatalog, EmbeddedDatabase, Value};
use std::sync::Arc;

fn as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn as_bool(value: &Value) -> bool {
    match value {
        Value::Boolean(value) => *value,
        other => panic!("expected bool, got {other:?}"),
    }
}

fn as_i32(value: &Value) -> i32 {
    match value {
        Value::Int2(value) => i32::from(*value),
        Value::Int4(value) => *value,
        Value::Int8(value) => *value as i32,
        other => panic!("expected int, got {other:?}"),
    }
}

#[test]
fn pg_index_lists_manual_secondary_indexes_with_pg_class_relation() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE a4_users (id INT PRIMARY KEY, email TEXT, name TEXT)")
        .unwrap();
    db.execute("CREATE INDEX idx_a4_users_email ON a4_users(email)")
        .unwrap();

    let rows = db
        .query(
            "SELECT c.relname, i.indisprimary, i.indisunique, i.indkey \
             FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             JOIN pg_class t ON t.oid = i.indrelid \
             WHERE t.relname = 'a4_users'",
            &[],
        )
        .unwrap();

    let mut saw_pk = false;
    let mut saw_secondary = false;
    for row in rows {
        let name = as_text(&row.values[0]);
        if name == "a4_users_pkey" {
            saw_pk = true;
            assert!(as_bool(&row.values[1]));
            assert!(as_bool(&row.values[2]));
            assert_eq!(as_text(&row.values[3]), "1");
        }
        if name == "idx_a4_users_email" {
            saw_secondary = true;
            assert!(!as_bool(&row.values[1]));
            assert!(!as_bool(&row.values[2]));
            assert_eq!(as_text(&row.values[3]), "2");
        }
    }

    assert!(saw_pk, "pg_index did not expose the primary-key index");
    assert!(saw_secondary, "pg_index did not expose the manual secondary index");
}

#[test]
fn pg_constraint_and_information_schema_expose_foreign_key_columns() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE a4_parent (id INT PRIMARY KEY)").unwrap();
    db.execute(
        "CREATE TABLE a4_child (\
            id INT PRIMARY KEY, \
            parent_id INT, \
            CONSTRAINT a4_child_parent_fk \
            FOREIGN KEY(parent_id) REFERENCES a4_parent(id) \
            ON DELETE CASCADE ON UPDATE NO ACTION\
        )",
    )
    .unwrap();

    let constraint_rows = db
        .query(
            "SELECT conname, contype, conkey, confkey, confdeltype, confupdtype \
             FROM pg_constraint \
             WHERE conname = 'a4_child_parent_fk'",
            &[],
        )
        .unwrap();
    assert_eq!(constraint_rows.len(), 1);
    let row = &constraint_rows[0];
    assert_eq!(as_text(&row.values[0]), "a4_child_parent_fk");
    assert_eq!(as_text(&row.values[1]), "f");
    assert_eq!(as_text(&row.values[2]), "{2}");
    assert_eq!(as_text(&row.values[3]), "{1}");
    assert_eq!(as_text(&row.values[4]), "c");
    assert_eq!(as_text(&row.values[5]), "a");

    let kcu_rows = db
        .query(
            "SELECT constraint_name, table_name, column_name, ordinal_position \
             FROM information_schema.key_column_usage \
             WHERE constraint_name = 'a4_child_parent_fk'",
            &[],
        )
        .unwrap();
    assert_eq!(kcu_rows.len(), 1);
    assert_eq!(as_text(&kcu_rows[0].values[0]), "a4_child_parent_fk");
    assert_eq!(as_text(&kcu_rows[0].values[1]), "a4_child");
    assert_eq!(as_text(&kcu_rows[0].values[2]), "parent_id");
    assert_eq!(as_i32(&kcu_rows[0].values[3]), 1);

    let table_constraint_rows = db
        .query(
            "SELECT constraint_type \
             FROM information_schema.table_constraints \
             WHERE constraint_name = 'a4_child_parent_fk'",
            &[],
        )
        .unwrap();
    assert_eq!(table_constraint_rows.len(), 1);
    assert_eq!(as_text(&table_constraint_rows[0].values[0]), "FOREIGN KEY");
}

#[test]
fn pg_wire_catalog_single_view_reflection_exposes_foreign_key_rows() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE a4_wire_parent (id INT PRIMARY KEY)").unwrap();
    db.execute(
        "CREATE TABLE a4_wire_child (\
            id INT PRIMARY KEY, \
            parent_id INT, \
            CONSTRAINT a4_wire_child_parent_fk \
            FOREIGN KEY(parent_id) REFERENCES a4_wire_parent(id)\
        )",
    )
    .unwrap();

    let catalog = PgCatalog::with_database(db);
    let (_schema, kcu_rows) = catalog
        .handle_query("SELECT * FROM information_schema.key_column_usage")
        .unwrap()
        .expect("catalog should intercept single-view key_column_usage");
    assert!(
        kcu_rows.iter().any(|row| {
            as_text(&row.values[2]) == "a4_wire_child_parent_fk"
                && as_text(&row.values[3]) == "a4_wire_child"
                && as_text(&row.values[4]) == "parent_id"
        }),
        "pg-wire key_column_usage did not expose the FK row: {kcu_rows:?}"
    );

    let (_schema, tc_rows) = catalog
        .handle_query("SELECT * FROM information_schema.table_constraints")
        .unwrap()
        .expect("catalog should intercept single-view table_constraints");
    assert!(
        tc_rows.iter().any(|row| {
            as_text(&row.values[2]) == "a4_wire_child_parent_fk" && as_text(&row.values[4]) == "FOREIGN KEY"
        }),
        "pg-wire table_constraints did not expose the FK row: {tc_rows:?}"
    );
}
