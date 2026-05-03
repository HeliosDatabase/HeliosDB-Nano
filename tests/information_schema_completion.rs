//! Bug 4 — `information_schema.routines` and `referential_constraints` views
//! plus the catch-all-errors-loudly behaviour.
//!
//! Previously the PG-wire catalog dispatcher (`src/protocol/postgres/catalog.rs:76-90`)
//! returned an empty schema with empty rows for any unknown `information_schema.*`
//! reference. ORMs that strict-check (e.g., TypeORM's `hasTable`) saw a misleading
//! empty result rather than an actionable error.
//!
//! v3.24.0 adds two new views (`routines`, `referential_constraints`) and a
//! whitelist of SQL-standard view names that legitimately return empty for
//! Nano's surface; anything outside the whitelist now returns an error.

use std::sync::Arc;
use heliosdb_nano::{EmbeddedDatabase, Value, protocol::postgres::catalog::PgCatalog};

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn catalog_with_db() -> (PgCatalog, Arc<EmbeddedDatabase>) {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let cat = PgCatalog::with_database(Arc::clone(&db));
    (cat, db)
}

#[test]
fn routines_view_has_well_formed_schema_and_zero_rows() {
    let (cat, _db) = catalog_with_db();
    let result = cat.handle_query(
        "SELECT routine_name, routine_type, data_type FROM information_schema.routines WHERE routine_schema = 'public'"
    ).expect("query").expect("intercepted");
    let (schema, rows) = result;
    // Projected to the three requested columns.
    assert_eq!(schema.columns.len(), 3, "expected 3 projected cols");
    assert_eq!(schema.columns[0].name, "routine_name");
    assert_eq!(schema.columns[1].name, "routine_type");
    assert_eq!(schema.columns[2].name, "data_type");
    // Nano doesn't persist a queryable function catalog; empty is correct.
    assert_eq!(rows.len(), 0, "expected zero rows for empty routine catalog");
}

#[test]
fn routines_view_select_star_exposes_full_sql_standard_columns() {
    let (cat, _db) = catalog_with_db();
    let (schema, _rows) = cat
        .handle_query("SELECT * FROM information_schema.routines")
        .expect("query")
        .expect("intercepted");
    // SQL standard core column names ORMs probe for.
    let names: Vec<_> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    for required in &[
        "specific_name",
        "routine_catalog",
        "routine_schema",
        "routine_name",
        "routine_type",
        "data_type",
        "routine_body",
        "routine_definition",
    ] {
        assert!(
            names.contains(required),
            "routines schema is missing column `{required}`; got {names:?}"
        );
    }
}

#[test]
fn referential_constraints_view_returns_zero_rows_for_no_fks() {
    let (cat, db) = catalog_with_db();
    db.execute("CREATE TABLE t (a INT PRIMARY KEY)").expect("create");
    let (schema, rows) = cat
        .handle_query("SELECT * FROM information_schema.referential_constraints")
        .expect("query")
        .expect("intercepted");
    assert!(schema.columns.iter().any(|c| c.name == "constraint_name"));
    assert!(schema.columns.iter().any(|c| c.name == "update_rule"));
    assert!(schema.columns.iter().any(|c| c.name == "delete_rule"));
    assert_eq!(rows.len(), 0);
}

#[test]
fn referential_constraints_view_exposes_real_fk_metadata() {
    let (cat, db) = catalog_with_db();
    db.execute("CREATE TABLE parents (id INT PRIMARY KEY)").expect("parents");
    db.execute(
        "CREATE TABLE kids (id INT PRIMARY KEY, p INT REFERENCES parents(id) ON DELETE CASCADE ON UPDATE NO ACTION)"
    ).expect("kids");

    let (schema, rows) = cat
        .handle_query("SELECT * FROM information_schema.referential_constraints WHERE constraint_schema = 'public'")
        .expect("query")
        .expect("intercepted");

    assert_eq!(rows.len(), 1, "expected exactly one FK row, got {}", rows.len());

    // Find indices.
    let idx = |name: &str| schema.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("column {name} missing"));
    let i_name = idx("constraint_name");
    let i_uname = idx("unique_constraint_name");
    let i_upd = idx("update_rule");
    let i_del = idx("delete_rule");
    let i_match = idx("match_option");

    let row = &rows[0];
    let name = s(&row.values[i_name]);
    assert!(name.contains("kids") && name.contains("parents"), "constraint name should reference kids+parents, got {name}");
    let uname = s(&row.values[i_uname]);
    assert!(uname.contains("parents"), "unique_constraint_name should reference parents, got {uname}");
    assert_eq!(s(&row.values[i_upd]), "NO ACTION");
    assert_eq!(s(&row.values[i_del]), "CASCADE");
    assert_eq!(s(&row.values[i_match]), "NONE");
}

#[test]
fn check_constraints_view_returns_zero_rows() {
    let (cat, db) = catalog_with_db();
    db.execute("CREATE TABLE t (a INT)").expect("create");
    let (schema, rows) = cat
        .handle_query("SELECT * FROM information_schema.check_constraints")
        .expect("query")
        .expect("intercepted");
    // SQL-standard columns:
    let names: Vec<_> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    for required in &["constraint_catalog", "constraint_schema", "constraint_name", "check_clause"] {
        assert!(names.contains(required), "missing {required}");
    }
    // We don't yet expose check constraints through this view; empty is OK.
    assert_eq!(rows.len(), 0);
}

#[test]
fn views_view_is_recognised_and_empty() {
    let (cat, _db) = catalog_with_db();
    let (schema, rows) = cat
        .handle_query("SELECT * FROM information_schema.views")
        .expect("query")
        .expect("intercepted");
    assert!(schema.columns.iter().any(|c| c.name == "view_definition"));
    assert_eq!(rows.len(), 0);
}

#[test]
fn whitelist_views_return_empty_without_error() {
    // These are SQL-standard view names that Nano legitimately doesn't populate
    // but ORM probes still hit. They must be recognised (return empty), not error.
    let (cat, _db) = catalog_with_db();
    for view in &[
        "triggers",
        "parameters",
        "sequences",
        "domains",
        "character_sets",
        "collations",
        "table_privileges",
        "column_privileges",
        "role_table_grants",
    ] {
        let q = format!("SELECT * FROM information_schema.{view}");
        let result = cat.handle_query(&q);
        assert!(result.is_ok(), "{view}: should not error, got {result:?}");
        assert!(result.unwrap().is_some(), "{view}: should be recognised and intercepted");
    }
}

#[test]
fn truly_unknown_information_schema_view_errors_loudly() {
    // An unknown view name (typo / made-up) should now error rather than
    // silently return an empty result — the v3.24.0 behaviour change.
    let (cat, _db) = catalog_with_db();
    let result = cat.handle_query(
        "SELECT * FROM information_schema.completely_made_up_view_name_xyz_42"
    );
    assert!(
        result.is_err(),
        "expected error for unknown information_schema view; got Ok: {result:?}"
    );
    let msg = result.unwrap_err().to_string().to_lowercase();
    assert!(
        msg.contains("information_schema") && (msg.contains("unknown") || msg.contains("not supported") || msg.contains("does not exist") || msg.contains("not a recognised") || msg.contains("not a recognized")),
        "error should mention information_schema and unknown/not-supported/does-not-exist/not-a-recognised; got {msg}"
    );
}

#[test]
fn existing_views_still_work() {
    // Regression check — make sure adding the new views didn't break the four
    // pre-existing handlers.
    let (cat, db) = catalog_with_db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT NOT NULL)").expect("create");

    for q in &[
        "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'",
        "SELECT column_name FROM information_schema.columns WHERE table_name = 't'",
        "SELECT schema_name FROM information_schema.schemata",
        "SELECT constraint_name FROM information_schema.key_column_usage",
        "SELECT constraint_name FROM information_schema.table_constraints",
    ] {
        let result = cat.handle_query(q);
        assert!(result.is_ok(), "regression on `{q}`: {result:?}");
        assert!(result.unwrap().is_some(), "regression on `{q}`: not intercepted");
    }
}
