//! GH#17 regression coverage: JSON operators must accept an UNCAST string
//! literal as a JSON operand.
//!
//! PostgreSQL types a bare literal as `unknown` and resolves it against the
//! operator's signature, so `payload @> '{"user_id":"alice"}'` types the
//! literal as `jsonb`. Nano used to reject it with
//! `JSON contains operator requires JSON operands, got String("...")` and
//! demanded an explicit `::jsonb`.
//!
//! `@>` was the reported operator, but the same defect shape existed on `<@`,
//! `?`, `?|` and `?&` (only `->`/`->>` had been fixed before, by
//! tests/json_text_operator.rs). All five are fixed in the evaluator; the
//! three key-existence operators are additionally unreachable from SQL because
//! of a SEPARATE, pre-existing parser defect (pinned by the
//! `gh17_key_exists_operators_are_eaten_by_the_placeholder_rewrite` test
//! below), so their operator semantics are asserted as unit tests in
//! src/sql/evaluator.rs instead.
//!
//! Both executor families are exercised:
//!   * `db.query(sql, &[])`            -> execute_in_transaction_inner
//!   * `db.query_params(sql, &params)` -> execute_plan_with_params_inner
//!     (the path the PostgreSQL EXTENDED protocol uses)

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};

fn seed_json() -> Result<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE events (id INT PRIMARY KEY, payload JSONB)")?;
    db.execute(r#"INSERT INTO events VALUES (1, '{"user_id":"alice","kind":"login"}')"#)?;
    db.execute(r#"INSERT INTO events VALUES (2, '{"user_id":"bob","kind":"logout"}')"#)?;
    Ok(db)
}

fn ids(rows: &[Tuple]) -> Vec<i64> {
    rows.iter()
        .map(|r| match r.values.first() {
            Some(Value::Int2(v)) => i64::from(*v),
            Some(Value::Int4(v)) => i64::from(*v),
            Some(Value::Int8(v)) => *v,
            other => panic!("expected an integer id, got {other:?}"),
        })
        .collect()
}

fn bool_at(rows: &[Tuple], row: usize) -> bool {
    match rows.get(row).and_then(|r| r.values.first()) {
        Some(Value::Boolean(b)) => *b,
        other => panic!("expected a boolean, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The reporter's exact statement
// ---------------------------------------------------------------------------

#[test]
fn gh17_contains_accepts_uncast_string_literal() -> Result<()> {
    let db = seed_json()?;

    // Reported as failing: no cast on the right-hand literal.
    let rows = db.query(r#"SELECT id FROM events WHERE payload @> '{"user_id":"alice"}'"#, &[])?;
    assert_eq!(ids(&rows), vec![1], "uncast literal must resolve to jsonb");

    // Reported as working: must keep working (no regression).
    let rows_cast = db.query(
        r#"SELECT id FROM events WHERE payload @> '{"user_id":"alice"}'::jsonb"#,
        &[],
    )?;
    assert_eq!(ids(&rows_cast), vec![1]);

    Ok(())
}

#[test]
fn gh17_contains_uncast_literal_on_params_family() -> Result<()> {
    let db = seed_json()?;

    // Same shape through the extended-protocol executor, with the JSON
    // document arriving as a bound TEXT parameter (what psycopg sends for a
    // plain `str`) rather than as a literal.
    let rows = db.query_params(
        "SELECT id FROM events WHERE payload @> $1",
        &[Value::String(r#"{"user_id":"bob"}"#.to_string())],
    )?;
    assert_eq!(ids(&rows), vec![2]);

    Ok(())
}

// ---------------------------------------------------------------------------
// The rest of the class that IS reachable from SQL: <@, ->, ->>
// ---------------------------------------------------------------------------

#[test]
fn gh17_contained_by_accepts_uncast_string_literal() -> Result<()> {
    let db = seed_json()?;

    // `<@` shares json_contains_op with `@>` (operands swapped).
    let rows = db.query(
        r#"SELECT '{"user_id":"alice"}' <@ payload FROM events WHERE id = 1"#,
        &[],
    )?;
    assert!(bool_at(&rows, 0), "uncast literal <@ jsonb must be true");

    let rows = db.query(
        r#"SELECT '{"user_id":"nobody"}' <@ payload FROM events WHERE id = 1"#,
        &[],
    )?;
    assert!(!bool_at(&rows, 0));

    Ok(())
}

#[test]
fn gh17_contains_on_uncast_json_text_column() -> Result<()> {
    // The other half of the same class: a TEXT column holding JSON. `->`/`->>`
    // already accepted this (tests/json_text_operator.rs); `@>`/`<@` did not.
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE leads (id INT PRIMARY KEY, profile TEXT)")?;
    db.execute(r#"INSERT INTO leads VALUES (1, '{"score":7,"src":"web"}')"#)?;

    let rows = db.query(r#"SELECT profile @> '{"score":7}' FROM leads WHERE id = 1"#, &[])?;
    assert!(bool_at(&rows, 0), "@> on a TEXT column holding JSON");

    let rows = db.query(r#"SELECT profile @> '{"score":9}' FROM leads WHERE id = 1"#, &[])?;
    assert!(!bool_at(&rows, 0));

    // -> / ->> keep working (no regression from the shared operand helper).
    let rows = db.query(r#"SELECT profile->>'src' FROM leads WHERE id = 1"#, &[])?;
    match rows.first().and_then(|r| r.values.first()) {
        Some(Value::String(s)) => assert_eq!(s, "web"),
        other => panic!("expected 'web', got {other:?}"),
    }

    Ok(())
}

#[test]
fn gh17_arrow_operators_accept_uncast_left_literal() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    let rows = db.query(r#"SELECT '{"a":{"b":42}}'->'a'->>'b'"#, &[])?;
    match rows.first().and_then(|r| r.values.first()) {
        Some(Value::String(s)) => assert_eq!(s, "42"),
        other => panic!("expected \"42\", got {other:?}"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The `?`-family key-existence operators and the SQLite-compat placeholder
// rewrite.
//
// `sqlite_compat::translate` runs on EVERY statement, not only SQLite-dialect
// ones, and rewrote every bare `?` outside a string literal into `$N`. That
// turned `payload ?| ARRAY[...]` into `payload $1| ARRAY[...]`, so `?|` and
// `?&` died at the parse stage and could never reach the evaluator that
// implements them.
//
// `?|` and `?&` are now exempt from the rewrite: a `$N` placeholder can never
// be followed by `|` or `&`, so the two spellings are unambiguous.
//
// Bare `?` is STILL rewritten, deliberately. It is genuinely ambiguous with a
// positional placeholder — PostgreSQL has the same collision — and SQLite
// placeholder compatibility is the whole purpose of that rewrite.
//
// Single-key workaround: `col ?| ARRAY['key']`, freed by the same exemption.
// (PostgreSQL's own escape hatch `jsonb_exists()` is NOT implemented here.)
// ---------------------------------------------------------------------------

#[test]
fn gh17_pipe_and_amp_key_existence_operators_reach_the_evaluator() -> Result<()> {
    let db = seed_json()?;

    // `?|` — ANY of the keys present.
    let rows = db.query(
        "SELECT id FROM events WHERE payload ?| ARRAY['user_id','nope'] ORDER BY id",
        &[],
    )?;
    assert_eq!(
        ids(&rows),
        vec![1, 2],
        "`?|` must match rows having ANY listed key; it is no longer eaten by the \
         placeholder rewrite"
    );

    // `?&` — ALL of the keys present.
    let rows = db.query(
        "SELECT id FROM events WHERE payload ?& ARRAY['user_id','kind'] ORDER BY id",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![1, 2], "`?&` must match rows having ALL listed keys");

    // `?&` with a key no row has must match nothing — proves the operator is
    // really evaluated rather than being parsed and ignored.
    let rows = db.query(
        "SELECT id FROM events WHERE payload ?& ARRAY['user_id','absent'] ORDER BY id",
        &[],
    )?;
    assert!(
        rows.is_empty(),
        "`?&` must require EVERY listed key; got {:?}",
        ids(&rows)
    );

    // Params family (the PostgreSQL extended-query path every real driver uses).
    let rows = db.query_params(
        "SELECT id FROM events WHERE payload ?& ARRAY['user_id','kind'] AND id = $1",
        &[Value::Int4(2)],
    )?;
    assert_eq!(ids(&rows), vec![2], "`?&` must work on the params family too");

    Ok(())
}

#[test]
fn gh17_bare_question_stays_a_placeholder_with_a_working_array_workaround() -> Result<()> {
    let db = seed_json()?;

    // Deliberate, documented residue: bare `?` is ambiguous with a positional
    // placeholder, so the rewrite still claims it. This must fail LOUDLY rather
    // than silently evaluating to something else.
    assert!(
        db.query("SELECT id FROM events WHERE payload ? 'user_id'", &[])
            .is_err(),
        "bare `?` is still rewritten to a positional placeholder by design; if this \
         ever starts succeeding, update the docs and this test together"
    );

    // The documented workaround must actually work, or the residue has no
    // escape hatch. `jsonb_exists()` is NOT implemented in this build, so the
    // single-key form is a one-element `?|`, which the same exemption frees.
    let rows = db.query(
        "SELECT id FROM events WHERE payload ?| ARRAY['user_id'] ORDER BY id",
        &[],
    )?;
    assert_eq!(
        ids(&rows),
        vec![1, 2],
        "`?| ARRAY['key']` is the documented single-key workaround for bare `?`"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// `@>` is ALSO the array containment operator — it must not be broken
// ---------------------------------------------------------------------------

#[test]
fn gh17_array_containment_still_distinguished_from_json() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // A SQL array is Value::Array, never Value::Json, so the array arm and the
    // JSON arm cannot alias.
    let rows = db.query("SELECT ARRAY[1,2,3] @> ARRAY[2]", &[])?;
    assert!(bool_at(&rows, 0), "int[] @> int[] containment");

    let rows = db.query("SELECT ARRAY[1,2,3] @> ARRAY[4]", &[])?;
    assert!(!bool_at(&rows, 0));

    // <@ is the same operator with the operands swapped.
    let rows = db.query("SELECT ARRAY[2] <@ ARRAY[1,2,3]", &[])?;
    assert!(bool_at(&rows, 0), "int[] <@ int[] containment");

    let rows = db.query("SELECT ARRAY['a','b'] @> ARRAY['b']", &[])?;
    assert!(bool_at(&rows, 0), "text[] @> text[] containment");

    // JSON arrays keep going through the JSON arm.
    let rows = db.query(r#"SELECT '[1,2,3]'::jsonb @> '[2]'"#, &[])?;
    assert!(bool_at(&rows, 0), "jsonb array containment");

    Ok(())
}

// ---------------------------------------------------------------------------
// Errors must name the problem in user terms, never leak `String("...")`
// ---------------------------------------------------------------------------

#[test]
fn gh17_error_text_does_not_leak_rust_debug_form() -> Result<()> {
    let db = seed_json()?;

    // A right operand that is genuinely not JSON.
    let err = db
        .query(r#"SELECT payload @> 'not json at all' FROM events WHERE id = 1"#, &[])
        .expect_err("a non-JSON operand must still be rejected");
    let msg = err.to_string();
    assert!(
        !msg.contains("String(\""),
        "error must not leak the Rust Debug form: {msg}"
    );
    assert!(msg.contains("@>"), "error should name the operator that failed: {msg}");
    assert!(
        msg.contains("JSON"),
        "error should name JSON as the expected shape: {msg}"
    );

    // An operand whose type can never be JSON.
    let err = db
        .query("SELECT payload @> 42 FROM events WHERE id = 1", &[])
        .expect_err("an integer operand must still be rejected");
    let msg = err.to_string();
    assert!(
        !msg.contains("Int4(") && !msg.contains("String(\""),
        "error must not leak the Rust Debug form: {msg}"
    );
    assert!(msg.contains("cast"), "error should point at the remedy: {msg}");

    Ok(())
}

#[test]
fn gh17_null_semantics_unchanged() -> Result<()> {
    let db = seed_json()?;
    db.execute("INSERT INTO events VALUES (3, NULL)")?;

    // NULL left operand contains nothing.
    let rows = db.query(r#"SELECT payload @> '{"a":1}' FROM events WHERE id = 3"#, &[])?;
    assert!(!bool_at(&rows, 0));

    Ok(())
}
