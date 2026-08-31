//! GH#18 regression coverage: `pg_typeof(any) -> regtype`.
//!
//! `SELECT pg_typeof(id)` used to fail with `Unknown scalar function:
//! pg_typeof`. It now returns the PostgreSQL type NAME (`uuid`, `jsonb`,
//! `text`, `integer`, `timestamp without time zone`, …).
//!
//! Where the type comes from (see `Evaluator::pg_typeof_name` for the full
//! doc comment): the scalar-dispatch point has both the argument's
//! `LogicalExpr` and its runtime `Value`. Column references and explicit casts
//! are answered from the DECLARED type — so `pg_typeof(uuid_col)` is `uuid`
//! even on a NULL row, which is PostgreSQL's rule. Every other expression
//! shape falls back to the runtime value, which cannot separate declared types
//! that share a representation (varchar/char -> text, json -> jsonb,
//! timestamptz -> timestamp without time zone). That residual gap needs the
//! planner to annotate expression result types and is filed as a follow-up.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};

fn type_name(rows: &[Tuple]) -> String {
    match rows.first().and_then(|r| r.values.first()) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("pg_typeof should return a type name string, got {other:?}"),
    }
}

fn seed() -> Result<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        "CREATE TABLE typed (
            id UUID PRIMARY KEY,
            n INT,
            big BIGINT,
            label TEXT,
            name VARCHAR(32),
            doc JSONB,
            ts TIMESTAMP,
            flag BOOLEAN,
            amount DOUBLE PRECISION,
            maybe TEXT
        )",
    )?;
    db.execute(
        "INSERT INTO typed VALUES (
            '550e8400-e29b-41d4-a716-446655440000',
            42,
            9000000000,
            'hello',
            'nano',
            '{\"a\":1}',
            '2026-01-02 03:04:05',
            true,
            1.5,
            NULL
        )",
    )?;
    Ok(db)
}

#[test]
fn gh18_pg_typeof_reports_declared_column_types() -> Result<()> {
    let db = seed()?;

    for (expr, expected) in [
        ("id", "uuid"),
        ("n", "integer"),
        ("big", "bigint"),
        ("label", "text"),
        ("doc", "jsonb"),
        ("ts", "timestamp without time zone"),
        ("flag", "boolean"),
        ("amount", "double precision"),
    ] {
        let sql = format!("SELECT pg_typeof({expr}) FROM typed");
        let rows = db.query(&sql, &[])?;
        assert_eq!(type_name(&rows), expected, "pg_typeof({expr})");
    }

    // VARCHAR(n) keeps its PostgreSQL spelling, not `text`.
    let rows = db.query("SELECT pg_typeof(name) FROM typed", &[])?;
    assert_eq!(type_name(&rows), "character varying");

    Ok(())
}

#[test]
fn gh18_pg_typeof_of_a_null_column_is_the_declared_type() -> Result<()> {
    let db = seed()?;

    // PostgreSQL reports the DECLARED type, not "null": the column is TEXT and
    // the row's value is NULL.
    let rows = db.query("SELECT pg_typeof(maybe) FROM typed", &[])?;
    assert_eq!(type_name(&rows), "text");

    Ok(())
}

#[test]
fn gh18_pg_typeof_of_a_bare_null_literal_is_text() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // `SELECT pg_typeof(NULL)` is `text` in PostgreSQL: the literal is
    // `unknown` and unknown resolves to text.
    let rows = db.query("SELECT pg_typeof(NULL)", &[])?;
    assert_eq!(type_name(&rows), "text");

    Ok(())
}

#[test]
fn gh18_pg_typeof_of_a_cast_uses_the_cast_target() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    for (expr, expected) in [
        ("NULL::uuid", "uuid"),
        ("'{}'::jsonb", "jsonb"),
        ("1::bigint", "bigint"),
        ("'x'::text", "text"),
    ] {
        let sql = format!("SELECT pg_typeof({expr})");
        let rows = db.query(&sql, &[])?;
        assert_eq!(type_name(&rows), expected, "pg_typeof({expr})");
    }

    Ok(())
}

#[test]
fn gh18_pg_typeof_of_literals_falls_back_to_the_value() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    let rows = db.query("SELECT pg_typeof(1)", &[])?;
    assert_eq!(type_name(&rows), "integer");

    let rows = db.query("SELECT pg_typeof(true)", &[])?;
    assert_eq!(type_name(&rows), "boolean");

    let rows = db.query("SELECT pg_typeof('abc')", &[])?;
    assert_eq!(type_name(&rows), "text");

    Ok(())
}

#[test]
fn gh18_pg_typeof_accepts_the_pg_catalog_qualification() -> Result<()> {
    let db = seed()?;

    // psql / ORMs frequently emit the schema-qualified spelling.
    let rows = db.query("SELECT pg_catalog.pg_typeof(id) FROM typed", &[])?;
    assert_eq!(type_name(&rows), "uuid");

    Ok(())
}

#[test]
fn gh18_pg_typeof_works_on_the_params_executor_family() -> Result<()> {
    let db = seed()?;

    // The extended-protocol family (execute_plan_with_params_inner).
    let rows = db.query_params("SELECT pg_typeof(doc) FROM typed WHERE n = $1", &[Value::Int4(42)])?;
    assert_eq!(type_name(&rows), "jsonb");

    Ok(())
}

#[test]
fn gh18_pg_typeof_rejects_wrong_arity() -> Result<()> {
    let db = seed()?;

    // Must not silently succeed with a made-up answer.
    assert!(
        db.query("SELECT pg_typeof(n, label) FROM typed", &[]).is_err(),
        "pg_typeof() takes exactly one argument"
    );

    Ok(())
}
