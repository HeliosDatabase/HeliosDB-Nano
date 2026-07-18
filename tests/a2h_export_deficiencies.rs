//! Regression coverage for the Any2HeliosDB (a2h) Oracle->HeliosDB export
//! deficiencies that were Nano-side fixes:
//!
//! #1 Trailing comma before `)` in a CREATE TABLE column/constraint list.
//! #2 `INTERVAL 'N' YEAR` / `INTERVAL 'N' MONTH` (and the `'N years'` string
//!    form) — approximated to days (Nano stores intervals as microseconds).
//! #4 An Oracle-style self-referencing CTE WITHOUT the `RECURSIVE` keyword.
//!
//! (#3, view dependency ordering, is an a2h-export concern — Postgres and Nano
//! both require a view's referents to exist at CREATE time — so it has no
//! Nano-side regression test.)

use heliosdb_nano::{EmbeddedDatabase, Value};

// ---------------------------------------------------------------------------
// #1 — trailing comma in CREATE TABLE
// ---------------------------------------------------------------------------

#[test]
fn export_trailing_comma_in_create_table() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // a2h Oracle export shape: dangling comma after the last constraint.
    db.execute("CREATE TABLE co_user (id NUMERIC(19,0) NOT NULL, name TEXT, PRIMARY KEY (id),)")
        .unwrap();
    db.execute("INSERT INTO co_user (id, name) VALUES (1, 'a')").unwrap();
    assert_eq!(db.query("SELECT * FROM co_user", &[]).unwrap().len(), 1);

    // Trailing comma after a plain column, with newlines.
    db.execute("CREATE TABLE t2 (\n  a INT,\n  b INT,\n)").unwrap();

    // A legitimate multi-row INSERT ... VALUES (..),(..) must be UNAFFECTED
    // (the rewrite is scoped to CREATE TABLE only).
    db.execute("CREATE TABLE t3 (a INT, b INT)").unwrap();
    let n = db.execute("INSERT INTO t3 (a, b) VALUES (1, 2), (3, 4)").unwrap();
    assert_eq!(n, 2, "multi-row VALUES must still insert both rows");

    // A NUMERIC scale spec `(19, 0)` (comma + digit + paren) must NOT be touched.
    assert_eq!(db.query("SELECT * FROM t3", &[]).unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// #2 — INTERVAL 'N' YEAR / MONTH
// ---------------------------------------------------------------------------

#[test]
fn export_interval_year_month_lowers() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Previously errored: "Unsupported interval field YEAR".
    let y = db
        .query("SELECT DATE '2020-01-01' + INTERVAL '2' YEAR AS d", &[])
        .unwrap();
    assert_eq!(y.len(), 1);
    assert!(
        !matches!(y[0].values[0], Value::Null),
        "INTERVAL '2' YEAR must compute a date"
    );

    let m = db
        .query("SELECT DATE '2020-01-01' + INTERVAL '3' MONTH AS d", &[])
        .unwrap();
    assert!(
        !matches!(m[0].values[0], Value::Null),
        "INTERVAL '3' MONTH must compute a date"
    );

    // String-form parity.
    let ys = db
        .query("SELECT DATE '2020-01-01' + INTERVAL '2 years' AS d", &[])
        .unwrap();
    assert!(
        !matches!(ys[0].values[0], Value::Null),
        "INTERVAL '2 years' must compute a date"
    );

    // DAY stays exact: 2020-01-01 + 1 day = 2020-01-02.
    let d = db
        .query("SELECT DATE '2020-01-01' + INTERVAL '1' DAY AS d", &[])
        .unwrap();
    assert!(!matches!(d[0].values[0], Value::Null));
}

// ---------------------------------------------------------------------------
// #4 — Oracle-style self-referencing CTE without RECURSIVE
// ---------------------------------------------------------------------------

const HIER_CTE: &str = "WITH ou_hierarchy (id, parent_id, depth) AS ( \
    SELECT id, parent_id, 0 AS depth FROM ou WHERE parent_id IS NULL \
    UNION ALL \
    SELECT o.id, o.parent_id, h.depth + 1 FROM ou o JOIN ou_hierarchy h ON o.parent_id = h.id \
  ) SELECT id, depth FROM ou_hierarchy ORDER BY id";

#[test]
fn export_non_recursive_self_referencing_cte() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE ou (id INT, parent_id INT)").unwrap();
    db.execute("INSERT INTO ou VALUES (1, NULL), (2, 1), (3, 2)").unwrap();

    // No RECURSIVE keyword (Oracle infers it) — must still resolve the
    // self-reference and return the full hierarchy.
    let r = db.query(HIER_CTE, &[]).unwrap();
    assert_eq!(r.len(), 3, "self-referencing CTE must return all 3 hierarchy rows");

    // Same wrapped in a CREATE VIEW + SELECT (the a2h export shape).
    db.execute(&format!("CREATE VIEW co_v_ou_hierarchy AS {HIER_CTE}"))
        .unwrap();
    assert_eq!(
        db.query("SELECT * FROM co_v_ou_hierarchy", &[]).unwrap().len(),
        3,
        "view over a non-RECURSIVE self-referencing CTE must return 3 rows"
    );
}
