//! Item #98 — the PostgreSQL JSON path-extraction operators `#>` and `#>>`.
//!
//! ## Why this file exists
//!
//! Until v4.21.0 the planner mapped sqlparser's `HashArrow` token to
//! `BinaryOperator::VectorInnerProduct`, so `jsonb_col #> '{a,b}'` silently
//! computed a *vector inner product*: a number where a JSON value belongs, with
//! no error. (pgvector's inner product is spelled `<#>` and never went through
//! that mapping, so nothing vector-related depended on it.) v4.21.0 replaced the
//! mishap with a loud "not supported yet". This suite pins the real
//! implementation that replaces the error.
//!
//! ## What is pinned
//!
//! * The `#>` / `#>>` split: `#>` yields JSON (a string value stays quoted),
//!   `#>>` yields bare text. That difference is the entire reason `#>>` exists,
//!   so it is asserted rather than assumed.
//! * Array subscripts arriving as TEXT. PostgreSQL's `#>` takes `text[]`, so
//!   `'{items,0}'` presents the index as the STRING `"0"`. The shared traversal
//!   re-reads a text element as a subscript when the current node is an array;
//!   without that, every array step returned NULL.
//! * A missing path is SQL NULL, not an error — PostgreSQL does not raise.
//! * All three right-operand spellings agree: the bare literal `'{a,b}'` (an
//!   uncast `text[]`), an explicit `ARRAY['a','b']`, and a bound parameter.
//! * The equivalence pin: `data #> '{a,b}'` equals
//!   `jsonb_extract_path(data,'a','b')` for a table of mixed documents. That is
//!   what proves the operator DELEGATES to the one traversal in
//!   `src/sql/evaluator.rs` instead of forking a second copy of it.
//! * The v4.21.0 mishap, in both directions, in one test: `<#>` still returns an
//!   inner-product distance, and `#>` no longer returns a number.
//!
//! Both executor families run every shape:
//!   * `db.query(sql, &[])`            -> execute_in_transaction_inner
//!   * `db.query_params(sql, &params)` -> execute_plan_with_params_inner
//!     (the path the PostgreSQL EXTENDED protocol, and therefore every real
//!     driver, uses)

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};

/// id 1: nested objects. id 2: an array. id 3: SQL NULL document.
fn seed_docs() -> Result<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, data JSONB)")?;
    db.execute(r#"INSERT INTO docs VALUES (1, '{"user":{"address":{"city":"NYC"},"age":41}}')"#)?;
    db.execute(r#"INSERT INTO docs VALUES (2, '{"items":["a","b","c"],"n":7}')"#)?;
    db.execute("INSERT INTO docs VALUES (3, NULL)")?;
    Ok(db)
}

fn first(rows: &[Tuple]) -> &Value {
    rows.first()
        .and_then(|r| r.values.first())
        .unwrap_or_else(|| panic!("expected at least one row with one column, got {rows:?}"))
}

fn json_text(rows: &[Tuple]) -> String {
    match first(rows) {
        Value::Json(j) => j.clone(),
        other => panic!("expected a JSON value (the `#>` result type), got {other:?}"),
    }
}

fn text(rows: &[Tuple]) -> String {
    match first(rows) {
        Value::String(s) => s.clone(),
        other => panic!("expected a text value (the `#>>` result type), got {other:?}"),
    }
}

fn is_null(rows: &[Tuple]) -> bool {
    matches!(first(rows), Value::Null)
}

// ---------------------------------------------------------------------------
// Nested object paths — the headline case
// ---------------------------------------------------------------------------

#[test]
fn hash_arrow_extracts_a_nested_object_path() -> Result<()> {
    let db = seed_docs()?;

    // `#>` keeps the JSON shape: a JSON string stays quoted.
    let rows = db.query("SELECT data #> '{user,address,city}' FROM docs WHERE id = 1", &[])?;
    assert_eq!(json_text(&rows), "\"NYC\"", "`#>` must return JSON, quotes included");

    // `#>>` unwraps it to bare text. This difference is the whole point of `#>>`.
    let rows = db.query("SELECT data #>> '{user,address,city}' FROM docs WHERE id = 1", &[])?;
    assert_eq!(text(&rows), "NYC", "`#>>` must return the unquoted text");

    // An intermediate node comes back as a whole JSON object.
    let rows = db.query("SELECT data #> '{user,address}' FROM docs WHERE id = 1", &[])?;
    assert_eq!(json_text(&rows), r#"{"city":"NYC"}"#);

    // A non-string leaf renders as its JSON text under both operators.
    let rows = db.query("SELECT data #> '{user,age}' FROM docs WHERE id = 1", &[])?;
    assert_eq!(json_text(&rows), "41");
    let rows = db.query("SELECT data #>> '{user,age}' FROM docs WHERE id = 1", &[])?;
    assert_eq!(text(&rows), "41");

    Ok(())
}

#[test]
fn hash_arrow_extracts_a_nested_object_path_on_params_family() -> Result<()> {
    let db = seed_docs()?;

    let rows = db.query_params(
        "SELECT data #> '{user,address,city}' FROM docs WHERE id = $1",
        &[Value::Int4(1)],
    )?;
    assert_eq!(json_text(&rows), "\"NYC\"");

    let rows = db.query_params(
        "SELECT data #>> '{user,address,city}' FROM docs WHERE id = $1",
        &[Value::Int4(1)],
    )?;
    assert_eq!(text(&rows), "NYC");

    Ok(())
}

// ---------------------------------------------------------------------------
// Array subscripts arriving as TEXT — the gap the shared traversal closes
// ---------------------------------------------------------------------------

#[test]
fn hash_arrow_indexes_an_array_through_a_text_subscript() -> Result<()> {
    let db = seed_docs()?;

    // `'{items,0}'` is a text[]; the "0" is a STRING. Before the shared
    // traversal this returned NULL because only a real integer indexed arrays.
    let rows = db.query("SELECT data #> '{items,0}' FROM docs WHERE id = 2", &[])?;
    assert_eq!(json_text(&rows), "\"a\"", "a text subscript must index a JSON array");
    let rows = db.query("SELECT data #>> '{items,0}' FROM docs WHERE id = 2", &[])?;
    assert_eq!(text(&rows), "a");

    let rows = db.query("SELECT data #>> '{items,2}' FROM docs WHERE id = 2", &[])?;
    assert_eq!(text(&rows), "c");

    // Negative subscripts count from the end, as in PostgreSQL.
    let rows = db.query("SELECT data #>> '{items,-1}' FROM docs WHERE id = 2", &[])?;
    assert_eq!(text(&rows), "c", "`-1` must be the LAST element");

    // Out of range, and further from the end than the array is long, are both
    // "absent" rather than an error.
    let rows = db.query("SELECT data #> '{items,9}' FROM docs WHERE id = 2", &[])?;
    assert!(is_null(&rows), "an out-of-range subscript is NULL");
    let rows = db.query("SELECT data #> '{items,-9}' FROM docs WHERE id = 2", &[])?;
    assert!(is_null(&rows), "a too-negative subscript is NULL, not a wrap-around");

    // Same, on the extended-protocol family.
    let rows = db.query_params("SELECT data #>> '{items,1}' FROM docs WHERE id = $1", &[Value::Int4(2)])?;
    assert_eq!(text(&rows), "b");

    Ok(())
}

// ---------------------------------------------------------------------------
// NULL rules: a missing path, a NULL document, a NULL path
// ---------------------------------------------------------------------------

#[test]
fn missing_path_is_null_not_an_error() -> Result<()> {
    let db = seed_docs()?;

    let rows = db.query("SELECT data #> '{nope,nope}' FROM docs WHERE id = 1", &[])?;
    assert!(is_null(&rows), "PostgreSQL returns NULL for an absent path");

    let rows = db.query("SELECT data #>> '{nope}' FROM docs WHERE id = 1", &[])?;
    assert!(is_null(&rows));

    // Descending into a scalar is also just "absent".
    let rows = db.query("SELECT data #> '{user,age,deeper}' FROM docs WHERE id = 1", &[])?;
    assert!(is_null(&rows));

    // A non-numeric key against an array is absent, not an error.
    let rows = db.query("SELECT data #> '{items,x}' FROM docs WHERE id = 2", &[])?;
    assert!(is_null(&rows));

    // Params family agrees.
    let rows = db.query_params("SELECT data #> '{nope}' FROM docs WHERE id = $1", &[Value::Int4(1)])?;
    assert!(is_null(&rows));

    Ok(())
}

#[test]
fn null_operands_yield_null() -> Result<()> {
    let db = seed_docs()?;

    // NULL document (id 3).
    let rows = db.query("SELECT data #> '{user}' FROM docs WHERE id = 3", &[])?;
    assert!(is_null(&rows), "a NULL left operand must give NULL");
    let rows = db.query("SELECT data #>> '{user}' FROM docs WHERE id = 3", &[])?;
    assert!(is_null(&rows));

    // NULL path.
    let rows = db.query("SELECT data #> NULL FROM docs WHERE id = 1", &[])?;
    assert!(is_null(&rows), "a NULL right operand must give NULL");

    // NULL document, on the params family.
    let rows = db.query_params("SELECT data #>> '{user}' FROM docs WHERE id = $1", &[Value::Int4(3)])?;
    assert!(is_null(&rows));

    Ok(())
}

// ---------------------------------------------------------------------------
// The three right-operand spellings must agree
// ---------------------------------------------------------------------------

#[test]
fn literal_array_and_bound_parameter_paths_all_agree() -> Result<()> {
    let db = seed_docs()?;

    // 1. Bare, uncast literal — PostgreSQL types it as text[].
    let literal = db.query("SELECT data #>> '{user,address,city}' FROM docs WHERE id = 1", &[])?;
    assert_eq!(text(&literal), "NYC");

    // 2. Explicit ARRAY constructor — arrives as a real SQL array value.
    let array = db.query(
        "SELECT data #>> ARRAY['user','address','city'] FROM docs WHERE id = 1",
        &[],
    )?;
    assert_eq!(text(&array), "NYC", "ARRAY[...] must resolve the same path");

    // 3. Bound parameter in the PostgreSQL array text form — what a text-protocol
    //    driver (psycopg) actually sends for a bound list.
    let bound = db.query_params(
        "SELECT data #>> $1 FROM docs WHERE id = 1",
        &[Value::String("{user,address,city}".to_string())],
    )?;
    assert_eq!(
        text(&bound),
        "NYC",
        "a bound text[] parameter must resolve the same path"
    );

    // An empty path returns the whole document, as in PostgreSQL.
    let whole = db.query("SELECT data #> '{}' FROM docs WHERE id = 2", &[])?;
    assert!(
        json_text(&whole).contains("\"items\""),
        "an empty path must return the whole document, got {}",
        json_text(&whole)
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Left-operand typing: the lenient unknown-literal rule, and a real error
// ---------------------------------------------------------------------------

#[test]
fn uncast_text_column_holding_json_works_on_the_left() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE leads (id INT PRIMARY KEY, profile TEXT, score INT)")?;
    db.execute(r#"INSERT INTO leads VALUES (1, '{"src":{"campaign":"web"}}', 7)"#)?;

    // Same lenient rule `->`, `@>` and `?|` use: a TEXT column holding JSON is
    // read as JSON rather than rejected.
    let rows = db.query("SELECT profile #>> '{src,campaign}' FROM leads WHERE id = 1", &[])?;
    assert_eq!(text(&rows), "web");

    // An uncast literal on the left too.
    let rows = db.query(r#"SELECT '{"a":{"b":"c"}}' #>> '{a,b}'"#, &[])?;
    assert_eq!(text(&rows), "c");

    Ok(())
}

#[test]
fn non_json_left_operand_is_a_loud_error() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE leads (id INT PRIMARY KEY, profile TEXT, score INT)")?;
    db.execute(r#"INSERT INTO leads VALUES (1, '{"src":{"campaign":"web"}}', 7)"#)?;

    // An INT is not JSON and cannot be coerced into one. Nothing may silently
    // succeed here — in particular it must not fall back to a number the way
    // the pre-v4.21.0 vector-inner-product mapping did.
    let err = db
        .query("SELECT score #> '{a}' FROM leads WHERE id = 1", &[])
        .expect_err("an INT left operand must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("#>"),
        "the error must name the operator the user wrote; got: {msg}"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Equivalence pin — proves ONE traversal, not two
// ---------------------------------------------------------------------------

#[test]
fn operator_and_function_return_the_same_value_for_every_document() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE mixed (id INT PRIMARY KEY, data JSONB)")?;
    db.execute(r#"INSERT INTO mixed VALUES (1, '{"a":{"b":"leaf"}}')"#)?;
    db.execute(r#"INSERT INTO mixed VALUES (2, '{"a":{"b":[1,2]}}')"#)?;
    db.execute(r#"INSERT INTO mixed VALUES (3, '{"a":{"c":"other"}}')"#)?;
    db.execute(r#"INSERT INTO mixed VALUES (4, '{"a":42}')"#)?;
    db.execute("INSERT INTO mixed VALUES (5, NULL)")?;

    // If the operator ever grows its own traversal, one of these five rows will
    // disagree with the function that shares it.
    let rows = db.query(
        "SELECT data #> '{a,b}', jsonb_extract_path(data, 'a', 'b') FROM mixed ORDER BY id",
        &[],
    )?;
    assert_eq!(rows.len(), 5);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            row.values[0],
            row.values[1],
            "row {} disagreed: `#>` gave {:?} but jsonb_extract_path gave {:?}",
            i + 1,
            row.values[0],
            row.values[1]
        );
    }

    // Same for the text-returning pair.
    let rows = db.query(
        "SELECT data #>> '{a,b}', jsonb_extract_path_text(data, 'a', 'b') FROM mixed ORDER BY id",
        &[],
    )?;
    assert_eq!(rows.len(), 5);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            row.values[0],
            row.values[1],
            "row {} disagreed: `#>>` gave {:?} but jsonb_extract_path_text gave {:?}",
            i + 1,
            row.values[0],
            row.values[1]
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// REGRESSION GUARD for the v4.21.0 mishap — both directions, one test, so a
// future re-mapping of HashArrow cannot pass by fixing only half of it.
// ---------------------------------------------------------------------------

#[test]
fn hash_arrow_is_json_and_angle_hash_angle_is_still_the_vector_inner_product() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE vecs (id INT PRIMARY KEY, vec VECTOR(3), data JSONB)")?;
    db.execute(r#"INSERT INTO vecs VALUES (1, '[1.0, 2.0, 3.0]', '{"a":{"b":5}}')"#)?;

    // Direction 1: `<#>` was never the operator at fault and must still be the
    // pgvector inner-product distance — a NUMBER.
    let rows = db.query("SELECT vec <#> '[1.0, 0.0, 0.0]' FROM vecs WHERE id = 1", &[])?;
    match first(&rows) {
        Value::Float4(_) | Value::Float8(_) => {}
        other => panic!("`<#>` must still yield a numeric distance, got {other:?}"),
    }

    // Direction 2: `#>` must NOT be a number any more. It is a JSON path.
    let rows = db.query("SELECT data #> '{a,b}' FROM vecs WHERE id = 1", &[])?;
    let got = first(&rows);
    assert!(
        !matches!(got, Value::Float4(_) | Value::Float8(_)),
        "`#>` computed a vector inner product again ({got:?}) — the v4.21.0 bug is back"
    );
    match got {
        Value::Json(j) => assert_eq!(j, "5"),
        other => panic!("`#>` must yield JSON, got {other:?}"),
    }

    Ok(())
}
