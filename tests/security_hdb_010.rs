//! HDB-010 — constant folding must preserve SQL comparison semantics.
//!
//! WHAT WAS BROKEN. `ConstantFoldingRule::fold_expr` folded `Literal = Literal` and
//! `Literal <> Literal` with `Value`'s DERIVED `PartialEq` — Rust equality, not SQL.
//! That silently changed answers the moment both sides were constants:
//!
//!   * `NULL = NULL`  folded to TRUE   (SQL: NULL)
//!   * `1 = NULL`     folded to FALSE  (SQL: NULL)
//!   * `1 <> NULL`    folded to TRUE   (SQL: NULL)
//!   * `1 = 1.0`      folded to FALSE  (SQL: TRUE — `Int4(1)` vs `Numeric("1.0")`)
//!
//! A `WHERE 1 = NULL` therefore behaved like `WHERE FALSE` only by accident, while
//! `WHERE NULL = NULL` behaved like `WHERE TRUE` and returned every row — the same
//! predicate over a COLUMN returned none. Constant folding is an optimisation; it
//! must never change a result.
//!
//! THE FIX delegates the fold to `Evaluator::compare_values`, the very function the
//! runtime uses, and wraps a NULL result in a Boolean CAST so the column is still
//! described as `bool` rather than inferring as TEXT from a bare NULL literal.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn scalar(db: &EmbeddedDatabase, sql: &str) -> Value {
    let rows = db.query(sql, &[]).expect(sql);
    assert_eq!(rows.len(), 1, "{sql}");
    assert_eq!(rows[0].values.len(), 1, "{sql}");
    rows[0].values[0].clone()
}

#[test]
fn folded_equality_matches_runtime_null_and_numeric_semantics() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Three-valued logic: any comparison with NULL is NULL, for BOTH operators and
    // in both operand positions.
    for sql in [
        "SELECT NULL = NULL",
        "SELECT NULL <> NULL",
        "SELECT 1 = NULL",
        "SELECT NULL = 1",
        "SELECT 1 <> NULL",
        "SELECT NULL <> 1",
    ] {
        assert_eq!(scalar(&db, sql), Value::Null, "{sql}");
    }

    // Cross-type numeric equality: `compare_values` coerces Int4 against Numeric.
    assert_eq!(scalar(&db, "SELECT 1 = 1.0"), Value::Boolean(true));
    assert_eq!(scalar(&db, "SELECT 1 <> 1.0"), Value::Boolean(false));

    db.execute("CREATE TABLE hdb010 (n INT)").unwrap();
    db.execute("INSERT INTO hdb010 VALUES (1)").unwrap();

    // The same comparison over a COLUMN — the path the fold must agree with.
    assert_eq!(scalar(&db, "SELECT n = 1.0 FROM hdb010"), Value::Boolean(true));
    assert_eq!(scalar(&db, "SELECT n <> 1.0 FROM hdb010"), Value::Boolean(false));

    // A NULL predicate is not TRUE, so it keeps no rows.
    for sql in [
        "SELECT n FROM hdb010 WHERE NULL = NULL",
        "SELECT n FROM hdb010 WHERE 1 = NULL",
        "SELECT n FROM hdb010 WHERE 1 <> NULL",
        "SELECT n FROM hdb010 WHERE NULL <> 1",
    ] {
        assert!(db.query(sql, &[]).expect(sql).is_empty(), "{sql}");
    }

    // ...and a folded-TRUE predicate keeps the row.
    let kept = "SELECT n FROM hdb010 WHERE 1 = 1.0";
    let rows = db.query(kept, &[]).expect(kept);
    assert_eq!(rows.len(), 1, "1 = 1.0 is TRUE, the row must survive");
}

/// Non-NULL, same-type comparisons must keep folding to a plain boolean — the fix
/// must not turn every folded comparison into a NULL or leave it unfolded.
#[test]
fn folded_non_null_comparisons_still_answer_a_boolean() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    assert_eq!(scalar(&db, "SELECT true = true"), Value::Boolean(true));
    assert_eq!(scalar(&db, "SELECT true = false"), Value::Boolean(false));
    assert_eq!(scalar(&db, "SELECT 'a' = 'a'"), Value::Boolean(true));
    assert_eq!(scalar(&db, "SELECT 'a' <> 'b'"), Value::Boolean(true));
    assert_eq!(scalar(&db, "SELECT 1 = 1"), Value::Boolean(true));
    assert_eq!(scalar(&db, "SELECT 1 <> 1"), Value::Boolean(false));
}

/// The folded NULL comparison is wrapped in `CAST(NULL AS BOOLEAN)` so a PostgreSQL
/// client is told the column is `bool`, not `text` (a bare NULL literal infers as
/// TEXT — src/sql/type_inference.rs). The embedded API exposes only column NAMES
/// (`EmbeddedDatabase::query_with_columns`); the declared TYPES come from
/// `query_with_schema`, which is `pub(crate)` and so unreachable from an integration
/// test. The inferred type is therefore pinned by the unit test
/// `test_constant_folding_null_comparison_is_boolean_typed_null` in
/// src/optimizer/rules.rs; what is pinned HERE is that the cast changes neither the
/// shape nor the value of the public result.
#[test]
fn folded_null_comparison_keeps_one_column_holding_null() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    let sql = "SELECT NULL = NULL";
    let (rows, columns) = db.query_with_columns(sql).expect(sql);
    assert_eq!(columns.len(), 1, "columns: {columns:?}");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values, vec![Value::Null]);
}
