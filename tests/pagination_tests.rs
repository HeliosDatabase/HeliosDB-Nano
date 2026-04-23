//! Pagination regression suite — covers the four Markon acceptance
//! criteria plus keyset and top-K optimisation paths.
//!
//! See `FEATURE_REQUEST_pagination.md` for context.

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

/// Seed `count` rows of a two-column table used by most tests below.
fn seed(db: &EmbeddedDatabase, count: usize) -> Result<()> {
    db.execute("CREATE TABLE t (id SERIAL PRIMARY KEY, created_at INT, name TEXT)")?;
    for i in 0..count {
        db.execute(&format!(
            "INSERT INTO t (created_at, name) VALUES ({}, 'r{}')",
            1_000 + i,
            i
        ))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Acceptance criterion 1: basic LIMIT/OFFSET on an empty table returns 0
// rows without blowing up. The failure mode this guards against is the
// psycopg `_row_as_tuple_getter NotImplementedError` which was rooted in
// Describe sending NoData when LIMIT contained a $N placeholder.
// ---------------------------------------------------------------------
#[test]
fn empty_table_limit_offset_returns_no_rows() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT)")?;
    let rows = db.query("SELECT * FROM t LIMIT 10 OFFSET 0", &[])?;
    assert!(rows.is_empty());
    // And a second page on the empty table
    let rows = db.query("SELECT * FROM t LIMIT 10 OFFSET 100", &[])?;
    assert!(rows.is_empty());
    Ok(())
}

// ---------------------------------------------------------------------
// Acceptance criterion 2: LIMIT/OFFSET round-trips with parameter binding.
// The planner used to reject Expr::Value(Placeholder(_)) from expr_to_usize
// which made psycopg's Parse-time schema derivation fail and crashed
// SQLAlchemy row decoders. This test exercises the planner directly.
// ---------------------------------------------------------------------
#[test]
fn planner_accepts_limit_offset_placeholders() -> Result<()> {
    // The planner must succeed on a query with $N in LIMIT/OFFSET even
    // without parameter values — parameters are substituted at execute
    // time by substitute_parameters().
    use heliosdb_nano::sql::planner::Planner;
    use heliosdb_nano::sql::Parser;

    let parser = Parser::new();
    let ast = parser.parse_one("SELECT * FROM t LIMIT $1 OFFSET $2")?;
    let planner = Planner::new();
    let plan = planner.statement_to_plan(ast);
    assert!(plan.is_ok(), "planner should accept placeholders in LIMIT/OFFSET: {:?}", plan.err());
    Ok(())
}

// ---------------------------------------------------------------------
// Acceptance criterion 3: ORDER BY + LIMIT + OFFSET returns deterministic
// rows. Exercises the TopK fast path (Limit over Sort).
// ---------------------------------------------------------------------
#[test]
fn order_by_limit_offset_is_deterministic() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, 100)?;

    // Newest-first page 1 (offset 0): expect ids 100, 99, 98, 97, 96
    let rows = db.query("SELECT id FROM t ORDER BY id DESC LIMIT 5 OFFSET 0", &[])?;
    let ids: Vec<i64> = rows.iter().map(|t| match t.values[0] { Value::Int8(n) => n, Value::Int4(n) => n as i64, _ => -1 }).collect();
    assert_eq!(ids, vec![100, 99, 98, 97, 96]);

    // Page 3 (offset 10, limit 5): expect ids 90..86 descending
    let rows = db.query("SELECT id FROM t ORDER BY id DESC LIMIT 5 OFFSET 10", &[])?;
    let ids: Vec<i64> = rows.iter().map(|t| match t.values[0] { Value::Int8(n) => n, Value::Int4(n) => n as i64, _ => -1 }).collect();
    assert_eq!(ids, vec![90, 89, 88, 87, 86]);

    // Last-partial page: offset 95, limit 10 expects 5 rows (ids 5..1)
    let rows = db.query("SELECT id FROM t ORDER BY id DESC LIMIT 10 OFFSET 95", &[])?;
    assert_eq!(rows.len(), 5);
    Ok(())
}

// ---------------------------------------------------------------------
// Acceptance criterion 4: LEFT OUTER JOIN + LIMIT + OFFSET composes.
// ---------------------------------------------------------------------
#[test]
fn left_join_limit_offset_composes() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE companies (id SERIAL PRIMARY KEY, name TEXT)")?;
    db.execute("CREATE TABLE leads (id SERIAL PRIMARY KEY, company_id INT, label TEXT)")?;
    db.execute("INSERT INTO companies (name) VALUES ('acme'), ('globex'), ('initech')")?;
    for i in 0..20 {
        db.execute(&format!(
            "INSERT INTO leads (company_id, label) VALUES ({}, 'l{}')",
            (i % 3) + 1,
            i
        ))?;
    }

    let rows = db.query(
        "SELECT leads.id, companies.name \
         FROM leads LEFT OUTER JOIN companies ON leads.company_id = companies.id \
         ORDER BY leads.id \
         LIMIT 5 OFFSET 0",
        &[],
    )?;
    assert_eq!(rows.len(), 5);

    let rows_page2 = db.query(
        "SELECT leads.id, companies.name \
         FROM leads LEFT OUTER JOIN companies ON leads.company_id = companies.id \
         ORDER BY leads.id \
         LIMIT 5 OFFSET 5",
        &[],
    )?;
    assert_eq!(rows_page2.len(), 5);
    Ok(())
}

// ---------------------------------------------------------------------
// Acceptance criterion 5: keyset pagination via row-constructor comparison.
// WHERE (created_at, id) < (1020, 25) ORDER BY created_at DESC, id DESC LIMIT 5
// ---------------------------------------------------------------------
#[test]
fn keyset_tuple_comparison_works() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, 30)?;

    let page1 = db.query(
        "SELECT id, created_at FROM t \
         WHERE (created_at, id) < (1020, 25) \
         ORDER BY created_at DESC, id DESC \
         LIMIT 5",
        &[],
    )?;
    assert_eq!(page1.len(), 5, "first page should be full");
    // Extract id column for the first row
    let first_id = match page1[0].values[0] {
        Value::Int8(n) => n,
        Value::Int4(n) => n as i64,
        _ => panic!("unexpected type"),
    };
    // seeded data: id=i+1, created_at=1000+i. Row with (created_at, id) just
    // below (1020, 25) in (created_at, id) tuple order is (1020, 21) — i=20.
    assert_eq!(first_id, 21);

    // Single-column keyset: WHERE id < 10 ORDER BY id DESC LIMIT 3
    let single = db.query(
        "SELECT id FROM t WHERE id < 10 ORDER BY id DESC LIMIT 3",
        &[],
    )?;
    let ids: Vec<i64> = single.iter().map(|t| match t.values[0] { Value::Int8(n) => n, Value::Int4(n) => n as i64, _ => -1 }).collect();
    assert_eq!(ids, vec![9, 8, 7]);
    Ok(())
}

// ---------------------------------------------------------------------
// Acceptance criterion 6: row-constructor equality.
// (a, b) = (x, y) is shorthand for a = x AND b = y.
// ---------------------------------------------------------------------
#[test]
fn row_constructor_equality_and_inequality() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, 5)?;
    // Should match exactly one row: id=3, created_at=1002
    let rows = db.query("SELECT id FROM t WHERE (id, created_at) = (3, 1002)", &[])?;
    assert_eq!(rows.len(), 1);

    // Should match zero rows: mismatched tuple
    let rows = db.query("SELECT id FROM t WHERE (id, created_at) = (3, 9999)", &[])?;
    assert_eq!(rows.len(), 0);

    // NotEq behaves as the negation of eq: 4 rows out of 5 differ from (3, 1002)
    let rows = db.query("SELECT id FROM t WHERE (id, created_at) <> (3, 1002)", &[])?;
    assert_eq!(rows.len(), 4);
    Ok(())
}

// ---------------------------------------------------------------------
// Storage-level offset pushdown (scan_table_with_offset_limit): large
// offsets should be handled without visibly degrading behavior. This
// doesn't benchmark; it just confirms correctness for an OFFSET beyond
// the row count and right at the boundary.
// ---------------------------------------------------------------------
#[test]
fn large_offset_is_bounded_and_correct() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, 100)?;

    // Offset beyond row count → empty.
    let rows = db.query("SELECT id FROM t LIMIT 5 OFFSET 200", &[])?;
    assert!(rows.is_empty());

    // Offset at exactly the last row → exactly one row.
    let rows = db.query("SELECT id FROM t ORDER BY id LIMIT 5 OFFSET 99", &[])?;
    assert_eq!(rows.len(), 1);

    // Offset straddling the end → correct partial result.
    let rows = db.query("SELECT id FROM t ORDER BY id LIMIT 10 OFFSET 95", &[])?;
    assert_eq!(rows.len(), 5);
    Ok(())
}
