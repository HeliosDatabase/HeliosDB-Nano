//! A scalar function call in FROM is one row of one column (GitHub #46).
//!
//! `SELECT * FROM current_schema()` is how TypeORM asks for the schema at
//! start-up. The call was read as a table of that name, its arguments dropped:
//! "Table 'current_schema' does not exist".
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn one(db: &EmbeddedDatabase, sql: &str) -> (Vec<String>, Vec<Value>) {
    let (rows, columns) = db
        .query_with_columns(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    assert_eq!(rows.len(), 1, "{sql}: one row");
    let embedded = db.query(sql, &[]).unwrap_or_else(|error| panic!("{sql}: {error}"));
    assert_eq!(embedded.len(), 1, "{sql}: one row through query()");
    (columns, rows[0].values.clone())
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => panic!("not text: {other:?}"),
    }
}

#[test]
fn current_schema_in_from_is_one_row_named_after_the_function() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let (columns, values) = one(&db, "SELECT * FROM current_schema()");
    assert_eq!(columns, ["current_schema"]);
    assert_eq!(text(&values[0]), "public");
    // The same value the select-list form gives.
    let (_, scalar) = one(&db, "SELECT current_schema()");
    assert_eq!(values, scalar);
    let (columns, values) = one(&db, "SELECT current_schema FROM current_schema()");
    assert_eq!(columns, ["current_schema"]);
    assert_eq!(text(&values[0]), "public");
}

#[test]
fn aliases_name_the_relation_and_the_column() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let (columns, values) = one(&db, "SELECT s FROM current_schema() AS s");
    assert_eq!(columns, ["s"]);
    assert_eq!(text(&values[0]), "public");
    let (columns, _) = one(&db, "SELECT x.name FROM current_schema() AS x(name)");
    assert_eq!(columns, ["name"]);
    let (columns, values) = one(&db, "SELECT * FROM current_database() AS d, current_schema() AS s");
    assert_eq!(columns, ["d", "s"]);
    assert_eq!(text(&values[1]), "public");
}

#[test]
fn other_scalars_arguments_and_joins_work_the_same_way() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let (columns, values) = one(&db, "SELECT * FROM upper('abc')");
    assert_eq!(columns, ["upper"]);
    assert_eq!(text(&values[0]), "ABC");
    let (_, values) = one(&db, "SELECT version FROM version()");
    assert!(text(&values[0]).contains("HeliosDB"), "{values:?}");
    let (_, values) = one(&db, "SELECT * FROM pg_catalog.current_schema()");
    assert_eq!(text(&values[0]), "public");

    db.execute("CREATE TABLE t (id INT PRIMARY KEY, schema_name TEXT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'public'), (2, 'other')").unwrap();
    let rows = db
        .query(
            "SELECT t.id FROM t JOIN current_schema() AS s ON s.s = t.schema_name ORDER BY t.id",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int4(1));
}

#[test]
fn table_functions_tables_and_unknown_names_are_what_they_were() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    assert_eq!(db.query("SELECT * FROM generate_series(1, 3)", &[]).unwrap().len(), 3);
    // A missing table is still a missing table.
    let missing = db.query("SELECT * FROM nosuch", &[]).expect_err("no such table");
    assert!(missing.to_string().contains("does not exist"), "{missing}");
    // A missing function says so, and an aggregate is refused.
    let unknown = db
        .query("SELECT * FROM nosuch_fn(1)", &[])
        .expect_err("no such function");
    assert!(unknown.to_string().to_lowercase().contains("nosuch_fn"), "{unknown}");
    assert!(db.query("SELECT * FROM count(1)", &[]).is_err());
}
