//! Bug 8 — minimal repro at the planner layer.
//!
//! Bypasses the PG-wire server-on-port (which has an unbounded-recursion
//! issue in tests/server_mode_integration_test.rs that's been `#[ignore]`'d
//! since at least v3.19.x) and exercises the same code path the extended-
//! query Describe handler uses: `Planner::statement_to_plan` on a statement
//! containing `$N` placeholders, then `LogicalPlan::schema()`.
//!
//! If the resulting schema's columns have empty / missing names, the bug
//! lives in the planner. If the columns look fine here, Bug 8 is downstream
//! of the planner and the next investigation step is the Describe / wire-
//! encoding path.

use heliosdb_nano::{EmbeddedDatabase, sql::{Parser, Planner}};

fn extract(db: &EmbeddedDatabase, sql: &str) -> Vec<(String, String)> {
    let parser = Parser::new();
    let stmt = parser.parse_one(sql).unwrap_or_else(|e| panic!("parse: {e}"));
    let catalog = db.storage.catalog();
    let plan = Planner::with_catalog(&catalog)
        .with_sql(sql.to_string())
        .statement_to_plan(stmt)
        .unwrap_or_else(|e| panic!("plan: {e}"));
    let schema = plan.schema();
    schema
        .columns
        .iter()
        .map(|c| (c.name.clone(), format!("{:?}", c.data_type)))
        .collect()
}

#[test]
fn planner_schema_simple_select_with_param() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE pings (week_bucket TEXT, hash TEXT, dashboard_version TEXT)").unwrap();

    // Literal form (works on simple-query path today)
    let lit = extract(&db, "SELECT COUNT(*) FROM pings WHERE week_bucket = '2026-18'");
    eprintln!("literal:    {:?}", lit);

    // Parameterised form (the Bug 8 input)
    let prm = extract(&db, "SELECT COUNT(*) FROM pings WHERE week_bucket = $1");
    eprintln!("paramised:  {:?}", prm);

    // The two schemas should be identical — the WHERE-side $1 doesn't
    // change the projection.
    assert_eq!(lit, prm, "Bug 8 may live in the planner: parameterised form's schema differs from the literal form's schema");

    // Sanity: column name must be non-empty.
    assert_eq!(prm.len(), 1);
    assert!(!prm[0].0.is_empty(), "Bug 8 in planner: COUNT(*) column has empty name in parameterised form");
}

#[test]
fn planner_schema_two_col_select_with_param() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (a INT, b TEXT)").unwrap();

    let lit = extract(&db, "SELECT a, b FROM t WHERE a = 1");
    let prm = extract(&db, "SELECT a, b FROM t WHERE a = $1");
    eprintln!("literal:   {:?}", lit);
    eprintln!("paramised: {:?}", prm);

    assert_eq!(lit.len(), 2);
    assert_eq!(prm.len(), 2);
    assert_eq!(lit[0].0, "a");
    assert_eq!(lit[1].0, "b");
    assert_eq!(prm[0].0, "a", "Bug 8 in planner: paramised plan's first col missing name");
    assert_eq!(prm[1].0, "b", "Bug 8 in planner: paramised plan's second col missing name");
}

#[test]
fn planner_schema_count_distinct_with_param() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE pings (week_bucket TEXT, hash TEXT)").unwrap();

    let lit = extract(&db, "SELECT COUNT(DISTINCT hash) FROM pings WHERE week_bucket = '2026-18'");
    let prm = extract(&db, "SELECT COUNT(DISTINCT hash) FROM pings WHERE week_bucket = $1");
    eprintln!("literal:   {:?}", lit);
    eprintln!("paramised: {:?}", prm);

    assert_eq!(lit, prm, "Bug 9 in planner: parameterised form differs");
    assert_eq!(prm.len(), 1);
    assert!(!prm[0].0.is_empty(), "Bug 9 in planner: COUNT(DISTINCT) column has empty name");
}

#[test]
fn planner_schema_aliased_aggregate_with_param() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE pings (week_bucket TEXT)").unwrap();

    let prm = extract(&db, "SELECT COUNT(*) AS xyzzy FROM pings WHERE week_bucket = $1");
    eprintln!("paramised aliased: {:?}", prm);
    assert_eq!(prm.len(), 1);
    assert_eq!(prm[0].0, "xyzzy", "alias must survive even with parametrised WHERE");
}
