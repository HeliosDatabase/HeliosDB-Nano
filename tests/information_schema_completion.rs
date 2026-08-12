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

use heliosdb_nano::{protocol::postgres::catalog::PgCatalog, EmbeddedDatabase, Value};
use std::sync::Arc;

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
    // Query via the SQL path (planner-backed SystemViewRegistry). The legacy
    // PgCatalog::handle_query interception for this view was removed in the v3.31
    // phase-2.8 migration; the view is now served by the planner.
    let (_cat, db) = catalog_with_db();
    db.execute("CREATE TABLE t (a INT PRIMARY KEY)").expect("create");
    let (rows, cols) = db
        .query_with_columns("SELECT * FROM information_schema.referential_constraints")
        .expect("query");
    assert!(cols.iter().any(|c| c == "constraint_name"));
    assert!(cols.iter().any(|c| c == "update_rule"));
    assert!(cols.iter().any(|c| c == "delete_rule"));
    assert_eq!(rows.len(), 0);
}

#[test]
fn referential_constraints_view_exposes_real_fk_metadata() {
    let (_cat, db) = catalog_with_db();
    db.execute("CREATE TABLE parents (id INT PRIMARY KEY)")
        .expect("parents");
    db.execute(
        "CREATE TABLE kids (id INT PRIMARY KEY, p INT REFERENCES parents(id) ON DELETE CASCADE ON UPDATE NO ACTION)",
    )
    .expect("kids");

    // Served by the planner-backed SystemViewRegistry (legacy interception removed).
    let (rows, cols) = db
        .query_with_columns(
            "SELECT * FROM information_schema.referential_constraints WHERE constraint_schema = 'public'",
        )
        .expect("query");

    assert_eq!(rows.len(), 1, "expected exactly one FK row, got {}", rows.len());

    // Find indices.
    let idx = |name: &str| {
        cols.iter()
            .position(|c| c == name)
            .unwrap_or_else(|| panic!("column {name} missing"))
    };
    let i_name = idx("constraint_name");
    let i_uname = idx("unique_constraint_name");
    let i_upd = idx("update_rule");
    let i_del = idx("delete_rule");
    let i_match = idx("match_option");

    let row = &rows[0];
    let name = s(&row.values[i_name]);
    assert!(
        name.contains("kids") && name.contains("parents"),
        "constraint name should reference kids+parents, got {name}"
    );
    let uname = s(&row.values[i_uname]);
    assert!(
        uname.contains("parents"),
        "unique_constraint_name should reference parents, got {uname}"
    );
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
    for required in &[
        "constraint_catalog",
        "constraint_schema",
        "constraint_name",
        "check_clause",
    ] {
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
        assert!(
            result.unwrap().is_some(),
            "{view}: should be recognised and intercepted"
        );
    }

    // `sequences` left the placeholder whitelist in v3.60: the wire catalog
    // DEFERS it (Ok(None)) so the planner-backed SystemViewRegistry serves
    // LIVE rows (catalog.rs `information_schema.sequences` arm — sequence
    // discovery is a migration-tooling requirement). Re-intercepting it with
    // an empty stub would silently break migration tooling, so pin the
    // deferral direction too.
    let seq = cat.handle_query("SELECT * FROM information_schema.sequences");
    assert!(seq.is_ok(), "sequences: should not error, got {seq:?}");
    assert!(
        seq.unwrap().is_none(),
        "sequences: must DEFER to the SystemViewRegistry (live rows), not an empty stub"
    );
}

#[test]
fn truly_unknown_information_schema_view_errors_loudly() {
    // An unknown view name (typo / made-up) should now error rather than
    // silently return an empty result — the v3.24.0 behaviour change.
    let (cat, _db) = catalog_with_db();
    let result = cat.handle_query("SELECT * FROM information_schema.completely_made_up_view_name_xyz_42");
    assert!(
        result.is_err(),
        "expected error for unknown information_schema view; got Ok: {result:?}"
    );
    let msg = result.unwrap_err().to_string().to_lowercase();
    assert!(
        msg.contains("information_schema")
            && (msg.contains("unknown")
                || msg.contains("not supported")
                || msg.contains("does not exist")
                || msg.contains("not a recognised")
                || msg.contains("not a recognized")),
        "error should mention information_schema and unknown/not-supported/does-not-exist/not-a-recognised; got {msg}"
    );
}

#[test]
fn existing_views_still_work() {
    // Regression check — the pre-existing information_schema views still answer
    // for a PG-wire client. After the v3.31 phase-2.8 migration these views are
    // served one of two ways: the PgCatalog still intercepts some (e.g. schemata),
    // while others (tables/columns/key_column_usage/table_constraints) return
    // Ok(None) from handle_query and fall through to the planner-backed
    // SystemViewRegistry. So each view must be answerable by EITHER path.
    let (cat, db) = catalog_with_db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT NOT NULL)")
        .expect("create");

    for q in &[
        "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'",
        "SELECT column_name FROM information_schema.columns WHERE table_name = 't'",
        "SELECT schema_name FROM information_schema.schemata",
        "SELECT constraint_name FROM information_schema.key_column_usage",
        "SELECT constraint_name FROM information_schema.table_constraints",
    ] {
        let intercepted = matches!(cat.handle_query(q), Ok(Some(_)));
        let via_planner = db.query_with_columns(q).is_ok();
        assert!(
            intercepted || via_planner,
            "regression on `{q}`: not served by PgCatalog interception nor the planner"
        );
    }
}

// ===========================================================================
// Status-column pins (added 2026-08-12, task #61).
//
// docs/compatibility/information_schema.md marked most of these views
// "Complete" for a long time while they returned zero rows — the tests in this
// file were right and the doc contradicted them, and nobody reconciled the two.
// These pins exist so the doc cannot drift back: every view the doc calls
// "always empty" is asserted to have ZERO ROWS **with the objects it describes
// actually present**, which is stronger than the older whitelist test above
// (that one only asserts the view is intercepted, not that it stays empty).
//
// If one of these starts returning rows, that view was implemented: update
// docs/compatibility/information_schema.md's Status column and the unknown-view
// error text in src/protocol/postgres/catalog.rs, then rewrite the test.
// ===========================================================================

/// A database with a view, a view-on-a-view, a CHECK constraint and an FK —
/// i.e. data that SHOULD populate every view asserted empty below.
fn catalog_with_populated_schema() -> (PgCatalog, Arc<EmbeddedDatabase>) {
    let (cat, db) = catalog_with_db();
    db.execute("CREATE TABLE is_parent (id INT PRIMARY KEY, code TEXT UNIQUE)")
        .unwrap();
    db.execute(
        "CREATE TABLE is_child (id INT PRIMARY KEY, pid INT REFERENCES is_parent(id), \
         qty INT CHECK (qty > 0))",
    )
    .unwrap();
    db.execute("CREATE VIEW is_v AS SELECT id, qty FROM is_child").unwrap();
    db.execute("CREATE VIEW is_v2 AS SELECT id FROM is_v").unwrap();
    (cat, db)
}

fn row_count(cat: &PgCatalog, view: &str) -> usize {
    let q = format!("SELECT * FROM information_schema.{view}");
    cat.handle_query(&q)
        .unwrap_or_else(|e| panic!("{view}: should be recognised, got Err({e})"))
        .unwrap_or_else(|| panic!("{view}: should be intercepted by the catalog, got None"))
        .1
        .len()
}

#[test]
fn always_empty_views_stay_empty_even_with_the_objects_they_describe_present() {
    let (cat, _db) = catalog_with_populated_schema();
    for view in &[
        "views",
        "view_table_usage",
        "view_column_usage",
        "check_constraints",
        "constraint_column_usage",
        "routines",
        "parameters",
        "character_sets",
        "collations",
    ] {
        assert_eq!(
            row_count(&cat, view),
            0,
            "information_schema.{view} is documented as always empty, but returned rows"
        );
    }
}

#[test]
fn populated_views_do_return_rows() {
    // The other half of the contract: these six are documented "Populated", so a
    // regression that emptied them must fail here rather than quietly matching
    // the always-empty expectation above.
    let (cat, _db) = catalog_with_populated_schema();
    for view in &[
        "tables",
        "columns",
        "schemata",
        "key_column_usage",
        "table_constraints",
        "referential_constraints",
    ] {
        assert!(
            row_count(&cat, view) > 0,
            "information_schema.{view} is documented as populated, but returned no rows"
        );
    }
}

#[test]
fn tables_view_lists_base_tables_but_not_views() {
    // PostgreSQL lists views here with table_type = 'VIEW'. Nano does not, so a
    // client enumerating relations through this view will miss every view.
    let (cat, _db) = catalog_with_populated_schema();
    let (schema, rows) = cat
        .handle_query("SELECT * FROM information_schema.tables")
        .unwrap()
        .expect("tables must be intercepted");
    let name_idx = schema
        .columns
        .iter()
        .position(|c| c.name == "table_name")
        .expect("table_name column");
    let names: Vec<String> = rows.iter().map(|r| s(&r.values[name_idx])).collect();
    assert!(names.iter().any(|n| n == "is_parent"), "base tables listed: {names:?}");
    assert!(
        !names.iter().any(|n| n == "is_v"),
        "views are NOT listed in information_schema.tables; got {names:?}"
    );
}

#[test]
fn privilege_views_stay_empty_after_a_grant() {
    let (cat, db) = catalog_with_populated_schema();
    // GRANT is accepted (CREATE ROLE is not supported at all), and changes nothing here.
    let _ = db.execute("GRANT SELECT ON is_parent TO app_user");
    for view in &[
        "table_privileges",
        "column_privileges",
        "role_table_grants",
        "role_column_grants",
    ] {
        assert_eq!(
            row_count(&cat, view),
            0,
            "information_schema.{view} does not reflect grants; it returned rows"
        );
    }
}

#[test]
fn catalog_name_is_not_implemented_at_all() {
    // Documented as "Not implemented": it raises the unknown-view error rather
    // than returning an empty result, exactly like a typo does.
    let (cat, _db) = catalog_with_db();
    let real = cat.handle_query("SELECT * FROM information_schema.catalog_name");
    let typo = cat.handle_query("SELECT * FROM information_schema.does_not_exist");
    assert!(real.is_err(), "catalog_name should error, got {real:?}");
    assert!(typo.is_err(), "an unknown view should error, got {typo:?}");
}

#[test]
fn unknown_view_error_lists_the_always_empty_views_honestly() {
    // The error text is the most-read description of this surface. It used to
    // claim routines/check_constraints/views were implemented; they are not.
    let (cat, _db) = catalog_with_db();
    let err = cat
        .handle_query("SELECT * FROM information_schema.does_not_exist")
        .expect_err("unknown view must error")
        .to_string();
    assert!(err.contains("ALWAYS EMPTY"), "error should flag the empty views: {err}");
    for empty in &["views", "check_constraints", "routines"] {
        let tail = err.split("ALWAYS EMPTY").nth(1).unwrap_or("");
        assert!(
            tail.contains(empty),
            "`{empty}` must be listed as always-empty, not as populated: {err}"
        );
    }
}
