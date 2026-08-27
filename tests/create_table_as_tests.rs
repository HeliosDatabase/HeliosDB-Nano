//! Tests for `CREATE TABLE ... AS <query>` (CTAS) and `SELECT ... INTO <table>`
//!
//! Both forms used to be silently dropped: the planner never read
//! `CreateTable::query`, so `CREATE TABLE d AS SELECT …` created a ZERO-COLUMN
//! table and reported success, and `SELECT … INTO t` was ignored outright.
//! These cover:
//! - Row copying and exact value round-tripping (not just counts)
//! - `SELECT *`, WHERE filters, expressions/aliases, aggregates
//! - Column derivation from the query's STATIC schema — an EMPTY source must
//!   still yield a correctly-columned, insertable table
//! - Duplicate output column names rejected instead of silently shadowing
//! - `IF NOT EXISTS` skipping the whole statement, duplicate-name errors
//! - `WITH [NO] DATA`
//! - A failed population leaving NO half-built table behind
//! - `SELECT … INTO` through both `execute()` and `query()`
//! - PL/pgSQL `SELECT … INTO <var>` staying a VARIABLE assignment (it must
//!   never create a table)
//! - `search_path` scoping of both the target and the source

mod test_helpers;

use heliosdb_nano::{EmbeddedDatabase, Result, Value};
use test_helpers::create_test_db;

/// Helper: read an integer scalar without pinning its exact width — COUNT(*)
/// and friends legitimately widen.
fn as_i64(value: &Value) -> i64 {
    match value {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("expected an integer, got {other:?}"),
    }
}

/// Helper: 3-row source table used by most of the copy tests.
fn setup_source(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE ctas_src (id INT, n INT, label TEXT, active BOOLEAN)")?;
    db.execute("INSERT INTO ctas_src (id, n, label, active) VALUES (1, 10, 'alpha', true)")?;
    db.execute("INSERT INTO ctas_src (id, n, label, active) VALUES (2, 20, 'beta', false)")?;
    db.execute("INSERT INTO ctas_src (id, n, label, active) VALUES (3, 30, 'gamma', true)")?;
    Ok(())
}

// ============================================================================
// Test 1: rows AND their contents actually land in the new table
// ============================================================================

#[test]
fn ctas_copies_rows_and_content() -> Result<()> {
    let db = create_test_db()?;
    setup_source(&db)?;

    let rows = db.execute("CREATE TABLE ctas_dst AS SELECT id, n FROM ctas_src")?;
    assert_eq!(rows, 3, "CTAS must report the number of rows it populated");

    let results = db.query("SELECT id, n FROM ctas_dst ORDER BY id", &[])?;
    assert_eq!(results.len(), 3, "the target table must hold all 3 source rows");
    assert_eq!(results[0].values[0], Value::Int4(1));
    assert_eq!(results[0].values[1], Value::Int4(10));
    assert_eq!(results[1].values[0], Value::Int4(2));
    assert_eq!(results[1].values[1], Value::Int4(20));
    assert_eq!(results[2].values[0], Value::Int4(3));
    assert_eq!(results[2].values[1], Value::Int4(30));

    Ok(())
}

// ============================================================================
// Test 2: SELECT * copies every column
// ============================================================================

#[test]
fn ctas_select_star_copies_all_columns() -> Result<()> {
    let db = create_test_db()?;
    setup_source(&db)?;

    let rows = db.execute("CREATE TABLE ctas_star AS SELECT * FROM ctas_src")?;
    assert_eq!(rows, 3);

    let (_rows, columns) = db.query_with_columns("SELECT * FROM ctas_star")?;
    assert_eq!(
        columns,
        vec!["id", "n", "label", "active"],
        "SELECT * must reproduce the source's column names and order"
    );

    let results = db.query("SELECT id, n, label, active FROM ctas_star ORDER BY id", &[])?;
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].values[2], Value::String("alpha".to_string()));
    assert_eq!(results[0].values[3], Value::Boolean(true));
    assert_eq!(results[1].values[3], Value::Boolean(false));

    Ok(())
}

// ============================================================================
// Test 3: WHERE filter copies only the matching subset
// ============================================================================

#[test]
fn ctas_where_filter_copies_subset() -> Result<()> {
    let db = create_test_db()?;
    setup_source(&db)?;

    let rows = db.execute("CREATE TABLE ctas_big AS SELECT id, n FROM ctas_src WHERE n > 15")?;
    assert_eq!(rows, 2, "only n=20 and n=30 qualify");

    let results = db.query("SELECT id FROM ctas_big ORDER BY id", &[])?;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].values[0], Value::Int4(2));
    assert_eq!(results[1].values[0], Value::Int4(3));

    Ok(())
}

// ============================================================================
// Test 4: types survive the round trip, and the new table accepts typed writes
// ============================================================================

#[test]
fn ctas_types_survive_round_trip() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE ctas_types_src (a INT, b BIGINT, c TEXT, d DOUBLE PRECISION, e BOOLEAN, f TIMESTAMP)")?;
    db.execute(
        "INSERT INTO ctas_types_src (a, b, c, d, e, f) \
         VALUES (7, 9000000000, 'text', 2.5, true, '2024-01-02 03:04:05')",
    )?;

    let rows = db.execute("CREATE TABLE ctas_types_dst AS SELECT * FROM ctas_types_src")?;
    assert_eq!(rows, 1);

    let results = db.query("SELECT a, b, c, d, e, f FROM ctas_types_dst", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values[0], Value::Int4(7));
    assert_eq!(results[0].values[1], Value::Int8(9_000_000_000));
    assert_eq!(results[0].values[2], Value::String("text".to_string()));
    assert_eq!(results[0].values[3], Value::Float8(2.5));
    assert_eq!(results[0].values[4], Value::Boolean(true));
    assert!(
        matches!(results[0].values[5], Value::Timestamp(_)),
        "the TIMESTAMP column must stay a timestamp, got {:?}",
        results[0].values[5]
    );

    // Independent proof the derived column TYPES are real and not all-Text: a
    // typed INSERT into the derived table has to be accepted and stored.
    db.execute(
        "INSERT INTO ctas_types_dst (a, b, c, d, e, f) \
         VALUES (8, 9000000001, 'more', 3.5, false, '2025-06-07 08:09:10')",
    )?;
    let results = db.query("SELECT a, b, d, e FROM ctas_types_dst WHERE a = 8", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values[0], Value::Int4(8));
    assert_eq!(results[0].values[1], Value::Int8(9_000_000_001));
    assert_eq!(results[0].values[2], Value::Float8(3.5));
    assert_eq!(results[0].values[3], Value::Boolean(false));

    Ok(())
}

// ============================================================================
// Test 5: an EMPTY source still produces a correctly-columned table
//
// This is the test that kills any attempt to derive the schema from result
// tuples: with zero rows there is nothing to infer from, so a tuple-inference
// implementation creates a zero-column table — the original bug.
// ============================================================================

#[test]
fn ctas_empty_source_yields_correct_columns() -> Result<()> {
    let db = create_test_db()?;
    setup_source(&db)?;

    let rows = db.execute("CREATE TABLE ctas_empty AS SELECT id, label FROM ctas_src WHERE 1 = 0")?;
    assert_eq!(rows, 0, "no source row qualifies");

    let (results, columns) = db.query_with_columns("SELECT * FROM ctas_empty")?;
    assert!(results.is_empty(), "the table must exist and be empty");
    assert_eq!(
        columns,
        vec!["id", "label"],
        "columns come from the query's STATIC schema, not from its (empty) rows"
    );

    // And the derived columns are real, typed and writable.
    db.execute("INSERT INTO ctas_empty (id, label) VALUES (42, 'later')")?;
    let results = db.query("SELECT id, label FROM ctas_empty", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values[0], Value::Int4(42));
    assert_eq!(results[0].values[1], Value::String("later".to_string()));

    Ok(())
}

// ============================================================================
// Test 6: expressions and aliases keep their names AND their values
// ============================================================================

#[test]
fn ctas_expressions_and_aliases() -> Result<()> {
    let db = create_test_db()?;
    setup_source(&db)?;

    let rows = db.execute("CREATE TABLE ctas_expr AS SELECT id + n AS total, upper(label) AS uname FROM ctas_src")?;
    assert_eq!(rows, 3);

    let (_rows, columns) = db.query_with_columns("SELECT * FROM ctas_expr")?;
    assert_eq!(
        columns,
        vec!["total", "uname"],
        "the SELECT aliases become column names"
    );

    let results = db.query("SELECT total, uname FROM ctas_expr ORDER BY total", &[])?;
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].values[0], Value::Int4(11));
    assert_eq!(results[0].values[1], Value::String("ALPHA".to_string()));
    assert_eq!(results[2].values[0], Value::Int4(33));
    assert_eq!(results[2].values[1], Value::String("GAMMA".to_string()));

    Ok(())
}

// ============================================================================
// Test 7: duplicate output column names are rejected, not silently shadowed
// ============================================================================

#[test]
fn ctas_duplicate_column_names_error() -> Result<()> {
    let db = create_test_db()?;
    setup_source(&db)?;

    let err = db
        .execute("CREATE TABLE ctas_dup AS SELECT id, id FROM ctas_src")
        .expect_err("two columns named `id` must be rejected")
        .to_string();
    assert!(
        err.contains("specified more than once"),
        "expected a duplicate-column error, got: {err}"
    );

    assert!(
        db.query("SELECT * FROM ctas_dup", &[]).is_err(),
        "the rejected statement must leave no table behind"
    );

    Ok(())
}

// ============================================================================
// Test 8: IF NOT EXISTS on an existing table skips the WHOLE statement
// ============================================================================

#[test]
fn ctas_if_not_exists_existing_table_not_repopulated() -> Result<()> {
    let db = create_test_db()?;
    setup_source(&db)?;

    db.execute("CREATE TABLE ctas_ine (id INT, n INT)")?;
    db.execute("INSERT INTO ctas_ine (id, n) VALUES (99, 999)")?;

    let rows = db.execute("CREATE TABLE IF NOT EXISTS ctas_ine AS SELECT id, n FROM ctas_src")?;
    assert_eq!(rows, 0, "the statement is skipped entirely — nothing is populated");

    let results = db.query("SELECT id, n FROM ctas_ine", &[])?;
    assert_eq!(results.len(), 1, "the existing table must be left exactly as it was");
    assert_eq!(results[0].values[0], Value::Int4(99));
    assert_eq!(results[0].values[1], Value::Int4(999));

    Ok(())
}

// ============================================================================
// Test 9: a duplicate CTAS target errors and leaves the original intact
// ============================================================================

#[test]
fn ctas_duplicate_table_errors_original_intact() -> Result<()> {
    let db = create_test_db()?;
    setup_source(&db)?;

    db.execute("CREATE TABLE ctas_once AS SELECT id, n FROM ctas_src WHERE n > 15")?;

    let err = db
        .execute("CREATE TABLE ctas_once AS SELECT id, n FROM ctas_src")
        .expect_err("re-creating the same table must fail")
        .to_string();
    assert!(err.contains("already exists"), "expected 'already exists', got: {err}");

    let results = db.query("SELECT id FROM ctas_once ORDER BY id", &[])?;
    assert_eq!(results.len(), 2, "the first table's contents must be untouched");
    assert_eq!(results[0].values[0], Value::Int4(2));

    Ok(())
}

// ============================================================================
// Test 10: WITH NO DATA / WITH DATA
// ============================================================================

#[test]
fn ctas_with_no_data_creates_columns_only() -> Result<()> {
    let db = create_test_db()?;
    setup_source(&db)?;

    let rows = db.execute("CREATE TABLE ctas_nodata AS SELECT id, label FROM ctas_src WITH NO DATA")?;
    assert_eq!(rows, 0, "WITH NO DATA populates nothing");

    let (results, columns) = db.query_with_columns("SELECT * FROM ctas_nodata")?;
    assert!(results.is_empty(), "WITH NO DATA must not copy any row");
    assert_eq!(columns, vec!["id", "label"], "but the columns must still be created");

    Ok(())
}

#[test]
fn ctas_with_data_explicit() -> Result<()> {
    let db = create_test_db()?;
    setup_source(&db)?;

    let rows = db.execute("CREATE TABLE ctas_withdata AS SELECT id, label FROM ctas_src WITH DATA")?;
    assert_eq!(rows, 3, "the explicit WITH DATA spelling populates, like the default");

    let results = db.query("SELECT id FROM ctas_withdata", &[])?;
    assert_eq!(results.len(), 3);

    Ok(())
}

// ============================================================================
// Test 11: a failed population leaves NO half-built table behind
// ============================================================================

#[test]
fn ctas_failed_population_leaves_no_table() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE ctas_div_src (n INT)")?;
    db.execute("INSERT INTO ctas_div_src (n) VALUES (5)")?;
    db.execute("INSERT INTO ctas_div_src (n) VALUES (0)")?;

    let err = db
        .execute("CREATE TABLE ctas_div_dst AS SELECT 10 / n AS q FROM ctas_div_src")
        .expect_err("dividing by the n=0 row must fail the statement")
        .to_string();
    assert!(
        err.contains("Division by zero"),
        "the ORIGINAL population error must surface, got: {err}"
    );

    assert!(
        db.query("SELECT * FROM ctas_div_dst", &[]).is_err(),
        "the compensating drop must remove the half-built table"
    );

    // And the name is genuinely free again.
    db.execute("CREATE TABLE ctas_div_dst (q INT)")?;

    Ok(())
}

// ============================================================================
// Test 12: SELECT ... INTO, through both execute() and query()
// ============================================================================

#[test]
fn select_into_creates_table() -> Result<()> {
    let db = create_test_db()?;
    setup_source(&db)?;

    // execute() route.
    let rows = db.execute("SELECT id, label INTO ctas_into_exec FROM ctas_src WHERE n > 15")?;
    assert_eq!(rows, 2);
    let results = db.query("SELECT id, label FROM ctas_into_exec ORDER BY id", &[])?;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].values[0], Value::Int4(2));
    assert_eq!(results[0].values[1], Value::String("beta".to_string()));

    // query() route: wire routing is keyword-based, so a `SELECT … INTO` also
    // arrives on the read entry point. It must run there and return NO rows.
    let returned = db.query("SELECT id, label INTO ctas_into_query FROM ctas_src", &[])?;
    assert!(returned.is_empty(), "the query surface returns no rows for a CTAS");
    let results = db.query("SELECT id FROM ctas_into_query ORDER BY id", &[])?;
    assert_eq!(results.len(), 3, "but the table is created and populated");

    // query_with_columns() is the wire path's own surface — same contract.
    let (rows, columns) = db.query_with_columns("SELECT id INTO ctas_into_cols FROM ctas_src")?;
    assert!(rows.is_empty() && columns.is_empty());
    assert_eq!(db.query("SELECT id FROM ctas_into_cols", &[])?.len(), 3);

    Ok(())
}

// ============================================================================
// Test 13: PL/pgSQL `SELECT ... INTO <var>` is a VARIABLE assignment
//
// Regression pin: once the SQL layer treats a top-level `SELECT … INTO t` as
// CTAS, a routine body containing `SELECT COUNT(*) INTO cnt FROM …` must NOT
// start creating a junk table named `cnt`.
// ============================================================================

#[test]
fn select_into_in_plpgsql_body_is_variable_not_table() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE ctas_proc_src (id INT)")?;
    db.execute("INSERT INTO ctas_proc_src (id) VALUES (1)")?;
    db.execute("INSERT INTO ctas_proc_src (id) VALUES (2)")?;

    db.execute(
        "CREATE PROCEDURE ctas_proc() LANGUAGE plpgsql AS $$\n\
         DECLARE cnt INTEGER;\n\
         BEGIN\n\
             SELECT COUNT(*) INTO cnt FROM ctas_proc_src;\n\
         END;\n\
         $$",
    )?;

    db.execute("CALL ctas_proc()")?;

    assert!(
        db.query("SELECT * FROM cnt", &[]).is_err(),
        "the INTO target is a plpgsql VARIABLE — no table named `cnt` may exist"
    );

    Ok(())
}

// ============================================================================
// Test 14: an aggregate source names its columns from the SELECT list
//
// Pins the assumption that the planner always tops an Aggregate with a Project
// carrying the user's aliases — if it ever stops doing so, `agg_0` would be
// frozen into a persisted catalog schema.
// ============================================================================

#[test]
fn ctas_aggregate_column_names() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE ctas_emp (name TEXT, dept TEXT)")?;
    db.execute("INSERT INTO ctas_emp (name, dept) VALUES ('a', 'eng')")?;
    db.execute("INSERT INTO ctas_emp (name, dept) VALUES ('b', 'eng')")?;
    db.execute("INSERT INTO ctas_emp (name, dept) VALUES ('c', 'sales')")?;

    let rows = db.execute("CREATE TABLE ctas_by_dept AS SELECT dept, count(*) AS n FROM ctas_emp GROUP BY dept")?;
    assert_eq!(rows, 2, "one row per department");

    let (_rows, columns) = db.query_with_columns("SELECT * FROM ctas_by_dept")?;
    assert_eq!(
        columns,
        vec!["dept", "n"],
        "group key and aggregate alias — never the internal group_0 / agg_0 names"
    );

    let results = db.query("SELECT dept, n FROM ctas_by_dept ORDER BY dept", &[])?;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].values[0], Value::String("eng".to_string()));
    assert_eq!(as_i64(&results[0].values[1]), 2);
    assert_eq!(results[1].values[0], Value::String("sales".to_string()));
    assert_eq!(as_i64(&results[1].values[1]), 1);

    Ok(())
}

// ============================================================================
// Test 15: both the target and the source resolve through search_path
// ============================================================================

#[test]
fn ctas_respects_search_path() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE SCHEMA ctas_s1")?;
    db.execute("SET search_path TO ctas_s1")?;
    db.execute("CREATE TABLE sp_src (id INT)")?;
    db.execute("INSERT INTO sp_src (id) VALUES (5)")?;

    let rows = db.execute("CREATE TABLE sp_dst AS SELECT id FROM sp_src")?;
    assert_eq!(rows, 1, "the bare source must resolve to ctas_s1.sp_src");

    // The bare target landed in the current schema.
    let results = db.query("SELECT id FROM ctas_s1.sp_dst", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values[0], Value::Int4(5));

    db.execute("SET search_path TO public")?;
    assert!(
        db.query("SELECT id FROM sp_dst", &[]).is_err(),
        "under public the bare name must not resolve — the table lives in ctas_s1"
    );

    Ok(())
}
