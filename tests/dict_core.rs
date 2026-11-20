#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
#![allow(clippy::assert_is_empty)]
//! The foundation of the system dictionary, on the embedded route.
//!
//! Every read goes through both statement families (`query_with_columns` and
//! `query_params_with_columns`) and both must agree. The PostgreSQL wire is
//! covered by the in-module tests next to the protocol handler.

use heliosdb_nano::{Column, DataType, EmbeddedDatabase, Schema, Value};

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "NULL".to_string(),
        other => other.to_string(),
    }
}

/// Rows and column names of `sql`, required equal on both statement families.
fn both(db: &EmbeddedDatabase, sql: &str) -> (Vec<Vec<String>>, Vec<String>) {
    let (text_rows, text_cols) = db
        .query_with_columns(sql)
        .unwrap_or_else(|e| panic!("text family failed on `{sql}`: {e}"));
    let (param_rows, param_cols) = db
        .query_params_with_columns(sql, &[])
        .unwrap_or_else(|e| panic!("params family failed on `{sql}`: {e}"));
    let render = |rows: &[heliosdb_nano::Tuple]| -> Vec<Vec<String>> {
        rows.iter().map(|r| r.values.iter().map(s).collect()).collect()
    };
    let text = render(&text_rows);
    assert_eq!(text_cols, param_cols, "columns differ between families for `{sql}`");
    assert_eq!(text, render(&param_rows), "rows differ between families for `{sql}`");
    (text, text_cols)
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<String>> {
    both(db, sql).0
}

fn one(db: &EmbeddedDatabase, sql: &str) -> String {
    let r = rows(db, sql);
    assert_eq!(r.len(), 1, "`{sql}` must return one row, got {r:?}");
    r[0][0].clone()
}

/// The statement must be refused on both statement families; returns both messages.
fn refused(db: &EmbeddedDatabase, sql: &str) -> Vec<String> {
    let text = db
        .execute(sql)
        .expect_err(&format!("`{sql}` must be refused (text family)"));
    let params = db
        .execute_params(sql, &[])
        .expect_err(&format!("`{sql}` must be refused (params family)"));
    vec![text.to_string(), params.to_string()]
}

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

// ---------------------------------------------------------------------------
// Item 1: producers reach the routine registry
// ---------------------------------------------------------------------------

#[test]
fn pg_proc_lists_user_functions_and_procedures() {
    let db = db();
    db.execute("CREATE TABLE dc_audit (id INTEGER)").unwrap();
    db.execute("CREATE FUNCTION dc_add(a INTEGER, b TEXT) RETURNS INTEGER AS $$ SELECT $1 $$ LANGUAGE sql")
        .unwrap();
    db.execute("CREATE PROCEDURE dc_ins(p INTEGER) LANGUAGE sql AS $$INSERT INTO dc_audit VALUES ($p)$$")
        .unwrap();

    let got = rows(
        &db,
        "SELECT proname, prokind, prorettype, proargtypes, pronargs, prolang FROM pg_proc ORDER BY proname",
    );
    assert_eq!(
        got,
        vec![
            vec!["dc_add", "f", "23", "23 25", "2", "14"],
            vec!["dc_ins", "p", "2278", "23", "1", "14"],
        ],
        "pg_proc must list the function and the procedure with their types"
    );

    // The body is there, the schema joins, and the schema-qualified spelling agrees.
    assert!(one(&db, "SELECT prosrc FROM pg_proc WHERE proname = 'dc_add'").contains("SELECT $1"));
    assert_eq!(
        one(
            &db,
            "SELECT n.nspname FROM pg_catalog.pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE p.proname = 'dc_ins'"
        ),
        "public"
    );
    let oid_before = one(&db, "SELECT oid FROM pg_proc WHERE proname = 'dc_ins'");

    db.execute("DROP FUNCTION dc_add").unwrap();
    assert_eq!(rows(&db, "SELECT proname FROM pg_proc"), vec![vec!["dc_ins"]]);
    assert_eq!(
        one(&db, "SELECT oid FROM pg_proc WHERE proname = 'dc_ins'"),
        oid_before,
        "a routine's oid does not move when another routine is dropped"
    );
}

#[test]
fn pg_proc_is_scoped_to_its_own_database() {
    let a = db();
    let b = db();
    a.execute("CREATE FUNCTION dc_only_a(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 $$ LANGUAGE sql")
        .unwrap();
    assert_eq!(rows(&a, "SELECT proname FROM pg_proc"), vec![vec!["dc_only_a"]]);
    assert!(
        rows(&b, "SELECT proname FROM pg_proc").is_empty(),
        "another open database must not see this one's routines"
    );
}

// ---------------------------------------------------------------------------
// Item 2: one internal-object predicate
// ---------------------------------------------------------------------------

#[test]
fn a_user_table_named_mv_x_is_a_table() {
    let db = db();
    db.execute("CREATE TABLE mv_x (id INTEGER PRIMARY KEY, note TEXT)")
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT type, name FROM sqlite_master WHERE name = 'mv_x'"),
        vec![vec!["table", "mv_x"]],
        "a user table whose name starts with mv_ is a table"
    );
    assert_eq!(
        one(
            &db,
            "SELECT table_type FROM information_schema.tables WHERE table_name = 'mv_x'"
        ),
        "BASE TABLE"
    );
    assert_eq!(one(&db, "SELECT relkind FROM pg_class WHERE relname = 'mv_x'"), "r");
    assert_eq!(one(&db, "SELECT count(*) FROM pg_tables WHERE tablename = 'mv_x'"), "1");
}

#[test]
fn a_materialized_view_data_table_is_internal() {
    let db = db();
    db.execute("CREATE TABLE dc_base (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    db.execute("INSERT INTO dc_base VALUES (1, 10), (2, 20)").unwrap();
    db.execute("CREATE MATERIALIZED VIEW dc_mv AS SELECT id, n FROM dc_base")
        .unwrap();

    for sql in [
        "SELECT count(*) FROM information_schema.tables WHERE table_name LIKE '\\_\\_mv\\_%'",
        "SELECT count(*) FROM pg_tables WHERE tablename LIKE '\\_\\_mv\\_%'",
        "SELECT count(*) FROM sqlite_master WHERE name LIKE '\\_\\_mv\\_%'",
        "SELECT count(*) FROM pg_class WHERE relname LIKE '\\_\\_mv\\_%'",
    ] {
        assert_eq!(
            one(&db, sql),
            "0",
            "the data table of a materialized view is internal: {sql}"
        );
    }
    assert_eq!(
        rows(&db, "SELECT type, name FROM sqlite_master WHERE name = 'dc_mv'"),
        vec![vec!["view", "dc_mv"]]
    );
    // pg_class shows the materialized view itself, and its columns join.
    assert_eq!(one(&db, "SELECT relkind FROM pg_class WHERE relname = 'dc_mv'"), "m");
    assert_eq!(
        rows(
            &db,
            "SELECT a.attname FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             WHERE c.relname = 'dc_mv' AND a.attnum > 0 ORDER BY a.attnum"
        ),
        vec![vec!["id"], vec!["n"]]
    );
    assert_eq!(
        one(&db, "SELECT count(*) FROM pg_matviews WHERE matviewname = 'dc_mv'"),
        "1"
    );
    // The view still reads.
    assert_eq!(one(&db, "SELECT count(*) FROM dc_mv"), "2");
}

// ---------------------------------------------------------------------------
// Item 3: reserved names
// ---------------------------------------------------------------------------

#[test]
fn reserved_names_are_refused() {
    let db = db();
    db.execute("CREATE TABLE dc_ok (id INTEGER PRIMARY KEY)").unwrap();
    for sql in [
        "CREATE TABLE hdb_mine (id INTEGER)",
        "CREATE TABLE IF NOT EXISTS hdb_mine (id INTEGER)",
        "CREATE TABLE pg_class (id INTEGER)",
        "CREATE TABLE public.pg_settings (id INTEGER)",
        "CREATE TABLE app.hdb_mine (id INTEGER)",
        "CREATE TABLE hdb.mine (id INTEGER)",
        "CREATE TABLE information_schema.tables (id INTEGER)",
        "CREATE TABLE sqlite_master (id INTEGER)",
        "CREATE TABLE hdb_ctas AS SELECT 1 AS x",
        "CREATE VIEW hdb_v AS SELECT id FROM dc_ok",
        "CREATE VIEW pg_tables AS SELECT id FROM dc_ok",
        "CREATE MATERIALIZED VIEW hdb_mv AS SELECT id FROM dc_ok",
        "ALTER TABLE dc_ok RENAME TO hdb_renamed",
        "ALTER TABLE dc_ok RENAME TO pg_class",
    ] {
        for message in refused(&db, sql) {
            assert!(
                message.contains("is reserved for the system dictionary"),
                "`{sql}` must be refused as a reserved name, got: {message}"
            );
        }
    }
    // Nothing was created, and the table that was not renamed is still there.
    assert_eq!(
        one(
            &db,
            "SELECT count(*) FROM information_schema.tables WHERE table_name LIKE 'hdb\\_%'"
        ),
        "0"
    );
    assert_eq!(one(&db, "SELECT count(*) FROM dc_ok"), "0");
    // pg_class is still the catalog.
    assert_eq!(one(&db, "SELECT relkind FROM pg_class WHERE relname = 'dc_ok'"), "r");

    // Names that only look similar stay free.
    db.execute("CREATE TABLE hdbx (id INTEGER)").unwrap();
    db.execute("CREATE TABLE my_hdb_t (id INTEGER)").unwrap();
    db.execute("CREATE TABLE pg_class_backup (id INTEGER)").unwrap();
    db.execute("CREATE VIEW dc_v AS SELECT id FROM dc_ok").unwrap();
    db.execute("ALTER TABLE hdbx RENAME TO hdbx2").unwrap();
}

/// A database made by an earlier release may hold a user table with the name
/// of a system view. It keeps reading its own table, also after a reopen.
#[test]
fn an_existing_user_table_with_the_name_of_a_system_view_wins() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("note", DataType::Text),
    ]);
    {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        // The catalog call is how such a table is made today: the statement is refused.
        db.storage.catalog().create_table("pg_inherits", schema).unwrap();
        db.execute("INSERT INTO pg_inherits VALUES (7, 'mine')").unwrap();
        assert_eq!(
            rows(&db, "SELECT id, note FROM pg_inherits"),
            vec![vec!["7", "mine"]],
            "the user table wins over the system view of the same name"
        );
        assert_eq!(one(&db, "SELECT count(*) FROM pg_inherits"), "1");
        assert_eq!(one(&db, "SELECT p.note FROM pg_inherits p WHERE p.id = 7"), "mine");
    }
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert_eq!(rows(&db, "SELECT id, note FROM pg_inherits"), vec![vec!["7", "mine"]]);
    // Once the table is gone the name is the system view again.
    db.execute("DROP TABLE pg_inherits").unwrap();
    assert!(rows(&db, "SELECT * FROM pg_inherits").is_empty());
    for message in refused(&db, "CREATE TABLE pg_inherits (id INTEGER)") {
        assert!(message.contains("is reserved for the system dictionary"), "{message}");
    }
}

/// Engine-owned `_hdb_code_*` / `_hdb_graph_*` tables are protected while the
/// feature that owns them is compiled into the binary.
#[test]
fn engine_tables_are_protected_while_their_feature_is_compiled_in() {
    let db = db();
    db.execute("CREATE TABLE IF NOT EXISTS _hdb_code_dc_probe (id INTEGER)")
        .unwrap();
    db.execute("CREATE TABLE IF NOT EXISTS _hdb_graph_dc_probe (id INTEGER)")
        .unwrap();
    for (feature_on, table) in [
        (cfg!(feature = "code-graph"), "_hdb_code_dc_probe"),
        (cfg!(feature = "graph-rag"), "_hdb_graph_dc_probe"),
    ] {
        let statements = [
            format!("ALTER TABLE {table} ADD COLUMN extra INTEGER"),
            format!("ALTER TABLE {table} RENAME TO dc_stolen"),
            format!("DROP TABLE {table}"),
        ];
        if feature_on {
            for sql in &statements {
                for message in refused(&db, sql) {
                    assert!(
                        message.contains("is owned by the engine"),
                        "`{sql}` must be refused, got: {message}"
                    );
                }
            }
            assert_eq!(one(&db, &format!("SELECT count(*) FROM {table}")), "0");
        } else {
            // Nothing owns the table in this binary: it is an ordinary table.
            db.execute(&statements[0]).unwrap();
            db.execute(&statements[2]).unwrap();
        }
    }
}
