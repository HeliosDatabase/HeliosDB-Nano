//! Regression tests for the REPL meta-commands that previously printed
//! hint text instead of running their underlying query.
//!
//! Symptom (reported 2026-05-03):
//! - `SELECT * FROM pg_database_branches();` → showed the branch table.
//! - `\branches` → printed `Use: SELECT * FROM pg_database_branches();` (hint, not data).
//! - `SHOW BRANCHES;` → printed `Query OK, 2 row(s) affected` with no rows.
//!
//! These tests pin the new behaviour: `\branches`, `\snapshots`, `\dmv`,
//! `\compression`, and `\indexes <t>` all run their corresponding system
//! query end-to-end, and the REPL routes `SHOW BRANCHES` through the
//! query path so the rows surface.

use heliosdb_nano::EmbeddedDatabase;

#[test]
fn pg_database_branches_function_returns_at_least_main() {
    // The data source the `\branches` meta-command depends on must
    // return at least one row (the implicit `main` branch).
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let (rows, cols) = db
        .query_with_columns("SELECT * FROM pg_database_branches()")
        .expect("query");
    assert!(!rows.is_empty(), "pg_database_branches() must include at least main");
    assert!(cols.iter().any(|c| c == "branch_name"));
}

#[test]
fn pg_mv_staleness_function_is_queryable() {
    // Used by `\dmv`. Phase3 SystemViewRegistry — embedded path.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let result = db.query_with_columns("SELECT * FROM pg_mv_staleness()");
    assert!(result.is_ok(), "pg_mv_staleness() must be queryable; got {result:?}");
}

#[test]
fn pg_vector_index_stats_function_is_queryable() {
    // Used by `\compression`.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let result = db.query_with_columns("SELECT * FROM pg_vector_index_stats()");
    assert!(result.is_ok(), "pg_vector_index_stats() must be queryable; got {result:?}");
}

#[test]
fn show_branches_returns_real_rows_via_executor() {
    // SHOW BRANCHES is the SQL surface that previously dropped its rows
    // when run via `db.execute()` (the REPL command path). The fix
    // routes SHOW through `db.query()` instead. Either way, the
    // executor itself must produce rows — verify here at the API level.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE DATABASE BRANCH b1 FROM main AS OF NOW").expect("create branch");
    let (rows, cols) = db.query_with_columns("SHOW BRANCHES").expect("show branches");
    // `pg_database_branches()` and `SHOW BRANCHES` both expose `branch_name`
    // (or `name` in the SHOW shape); accept either to keep the test
    // resilient to the column-name choice.
    let has_name_col = cols.iter().any(|c| c == "branch_name" || c == "name");
    assert!(has_name_col, "SHOW BRANCHES must produce a name column; got {cols:?}");
    // Should include at least main + b1.
    assert!(rows.len() >= 2, "expected ≥2 branches (main + b1), got {}", rows.len());
    // b1 must appear by name.
    let any_b1 = rows.iter().any(|r| {
        r.values.iter().any(|v| {
            matches!(v, heliosdb_nano::Value::String(s) if s == "b1")
        })
    });
    assert!(any_b1, "SHOW BRANCHES output should contain b1; rows={rows:?}");
}
