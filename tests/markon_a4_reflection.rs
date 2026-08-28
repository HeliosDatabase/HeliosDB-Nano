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
fn pg_indexes_lists_manual_and_vector_indexes() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE a4_idx_users (id INT PRIMARY KEY, email TEXT)")
        .unwrap();
    db.execute("CREATE INDEX idx_a4_idx_users_email ON a4_idx_users(email)")
        .unwrap();
    db.execute("CREATE TABLE a4_idx_docs (id INT PRIMARY KEY, embedding VECTOR(3))")
        .unwrap();
    db.execute("INSERT INTO a4_idx_docs VALUES (1, '[1.0, 0.0, 0.0]')")
        .unwrap();
    db.execute(
        "CREATE INDEX idx_a4_idx_docs_embedding \
         ON a4_idx_docs USING hnsw (embedding vector_cosine_ops)",
    )
    .unwrap();

    // HC3: `pg_indexes` moved OFF the wire-only substring router and INTO the
    // planner-backed SystemViewRegistry, so `handle_query` now defers
    // (`Ok(None)`) and the same rows arrive through the engine — which is also
    // what finally makes this view reachable from the embedded / REPL / Python
    // routes, where it used to error with "does not exist".
    let catalog = PgCatalog::with_database(Arc::clone(&db));
    assert!(
        catalog
            .handle_query("SELECT * FROM pg_indexes")
            .expect("pg_indexes must not error at the wire")
            .is_none(),
        "pg_indexes must DEFER to the SystemViewRegistry"
    );
    let (rows, cols) = db
        .query_with_columns("SELECT * FROM pg_indexes")
        .expect("pg_indexes must be served by the planner");
    assert_eq!(
        cols,
        vec![
            "schemaname".to_string(),
            "tablename".to_string(),
            "indexname".to_string(),
            "tablespace".to_string(),
            "indexdef".to_string(),
        ]
    );

    let mut saw_manual = false;
    let mut saw_vector = false;
    for row in rows {
        let name = as_text(&row.values[2]);
        let def = as_text(&row.values[4]);
        if name == "idx_a4_idx_users_email" {
            saw_manual = true;
            assert!(def.contains("USING btree"), "manual indexdef was {def}");
        }
        if name == "idx_a4_idx_docs_embedding" {
            saw_vector = true;
            assert!(def.contains("USING hnsw"), "vector indexdef was {def}");
            assert!(def.contains("vector_cosine_ops"), "vector indexdef was {def}");
        }
    }

    assert!(saw_manual, "pg_indexes did not expose the manual secondary index");
    assert!(saw_vector, "pg_indexes did not expose the HNSW vector index");
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

    // HC3: single-view `information_schema` reflection moved OFF the PG-wire
    // substring interceptor and INTO the planner-backed SystemViewRegistry —
    // the same move `pg_indexes_lists_manual_and_vector_indexes` above pins.
    //
    // The subject of this test has always been "a reflecting client can see
    // the foreign key", never "the interceptor is what produced the rows", so
    // it now pins BOTH halves of the new contract:
    //   1. the wire DEFERS (`Ok(None)`) — no raw-text interception is left for
    //      a query mentioning these views to be hijacked by; and
    //   2. the FK rows still come back, through the engine route that PG wire,
    //      MySQL wire, embedded, REPL and Python now all share.
    // Restoring interception to satisfy (2) would fail (1) — which is the
    // point.
    let catalog = PgCatalog::with_database(Arc::clone(&db));
    for view in [
        "information_schema.key_column_usage",
        "information_schema.table_constraints",
    ] {
        let sql = format!("SELECT * FROM {view}");
        assert!(
            catalog
                .handle_query(&sql)
                .unwrap_or_else(|e| panic!("{view} must not error at the wire: {e}"))
                .is_none(),
            "{view} must DEFER to the SystemViewRegistry, not be intercepted on raw wire text"
        );
    }

    // Column NAMES are asserted too (the interceptor-era test only indexed
    // positionally): a shape drift that silently shifted `column_name` into
    // another slot would now be caught instead of misread.
    let (kcu_rows, kcu_cols) = db
        .query_with_columns("SELECT * FROM information_schema.key_column_usage")
        .expect("key_column_usage must be served by the planner");
    assert_eq!(
        kcu_cols,
        vec![
            "constraint_catalog".to_string(),
            "constraint_schema".to_string(),
            "constraint_name".to_string(),
            "table_name".to_string(),
            "column_name".to_string(),
            "ordinal_position".to_string(),
        ]
    );
    assert!(
        kcu_rows.iter().any(|row| {
            as_text(&row.values[2]) == "a4_wire_child_parent_fk"
                && as_text(&row.values[3]) == "a4_wire_child"
                && as_text(&row.values[4]) == "parent_id"
                && as_i32(&row.values[5]) == 1
        }),
        "key_column_usage did not expose the FK row: {kcu_rows:?}"
    );

    let (tc_rows, tc_cols) = db
        .query_with_columns("SELECT * FROM information_schema.table_constraints")
        .expect("table_constraints must be served by the planner");
    assert_eq!(
        tc_cols,
        vec![
            "constraint_catalog".to_string(),
            "constraint_schema".to_string(),
            "constraint_name".to_string(),
            "table_name".to_string(),
            "constraint_type".to_string(),
        ]
    );
    assert!(
        tc_rows.iter().any(|row| {
            as_text(&row.values[2]) == "a4_wire_child_parent_fk" && as_text(&row.values[4]) == "FOREIGN KEY"
        }),
        "table_constraints did not expose the FK row: {tc_rows:?}"
    );

    // The capability the substring router never had, and the reason deferring
    // is an upgrade rather than a downgrade: a real WHERE clause. The child
    // table also carries `a4_wire_child_pkey`, so an unfiltered (or
    // wrongly-filtered) result would not be exactly one row.
    let filtered = db
        .query(
            "SELECT constraint_name, table_name, column_name \
             FROM information_schema.key_column_usage \
             WHERE constraint_name = 'a4_wire_child_parent_fk'",
            &[],
        )
        .expect("key_column_usage must support WHERE through the planner");
    assert_eq!(
        filtered.len(),
        1,
        "WHERE constraint_name must select exactly the FK row: {filtered:?}"
    );
    assert_eq!(as_text(&filtered[0].values[0]), "a4_wire_child_parent_fk");
    assert_eq!(as_text(&filtered[0].values[1]), "a4_wire_child");
    assert_eq!(as_text(&filtered[0].values[2]), "parent_id");
}
