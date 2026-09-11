//! GH #33 — `::vector` on a BOUND PARAMETER (and on an `ARRAY[...]` literal).
//!
//! Install as `tests/gh_issue_33.rs`.
//!
//! # What is actually broken, mechanism first
//!
//! `Planner::expr_to_logical`'s `Expr::Cast` arm (src/sql/planner.rs:4523-4562)
//! special-cases a BARE `::vector` (no dimension). It infers the dimension ONLY
//! when the cast operand is `Expr::Value(SingleQuotedString)`. Every other
//! operand — a `$N` placeholder, an `ARRAY[...]` literal, a column reference —
//! takes the `else` branch at src/sql/planner.rs:4547-4553 and errors:
//!
//!     bare ::vector cast can only infer dimension from a literal string;
//!     use ::vector(N) explicitly otherwise
//!
//! That is sub-defect (a). It is fatal for drivers because `$N::vector` is the
//! only form a driver can emit — pgvector clients never inline the embedding.
//!
//! A SECOND, independent break sits one arm earlier, in `Expr::Array`
//! (src/sql/planner.rs:4471-4520). The "is this a vector literal" test is
//!
//!     all(|e| matches!(e, Expr::Value(Number(_, _))))          (planner.rs:4474)
//!
//! and sqlparser 0.53 parses a NEGATIVE number as `Expr::UnaryOp { Minus, .. }`,
//! not as `Value::Number`. So any array containing a negative element — i.e.
//! EVERY real embedding — fails the `all_numeric` test, falls into the general
//! array arm, and dies at src/sql/planner.rs:4516 with
//!
//!     Unsupported array element type
//!
//! which is exactly the second, "different" message the issue reports from the
//! mem0 path. It is not a second code path in the wire layer: psycopg2 does
//! CLIENT-SIDE interpolation, so `cur.execute("… %s::vector", ([-0.1, …],))`
//! reaches the server as the TEXT query `… ARRAY[-0.1, …]::vector`. The two
//! messages are the two planner arms above, reached by the same statement
//! depending on whether the embedding happens to contain a negative component.
//!
//! Sub-defect (c) — the advertised result OID — is not observable from the
//! embedded API and lives in the companion wire test file
//! (`src/protocol/postgres/wire_tests.rs`): `datatype_to_oid`
//! (src/protocol/postgres/handler.rs:2854) maps `DataType::Vector(_)` to OID
//! **1000**, which is PostgreSQL's `_bool` (boolean ARRAY). psycopg2 has
//! `BOOLEANARRAY` registered for 1000, so it runs its ARRAY parser over the
//! pgvector text `[1,0,0]` and raises `array does not start with '{'`.
//!
//! # Executor families
//!
//! Both families share ONE planner, so the bare-cast rejection is common to
//! both, but the shapes differ and both are covered here:
//!   * text family   (`db.execute` / `db.query` → `execute_in_transaction_inner`):
//!     the psql and psycopg2-mogrified shapes, `ARRAY[...]::vector`.
//!   * params family (`db.query_params` / `db.execute_params` →
//!     `execute_plan_with_params_inner`): the PG EXTENDED protocol and REST
//!     shape, `$N::vector`.
//!
//! # Expected outcome on the CURRENT tree
//!
//! FAIL: bare_vector_cast_on_bound_parameter
//!       bare_vector_cast_on_array_literal_text_family
//!       negative_element_array_literal_casts_to_vector
//!       float_array_parameter_casts_to_vector
//!       mem0_search_shape_end_to_end
//!       mem0_insert_shape_with_bound_vector
//!       mem0_insert_returning_shape_with_bound_vector
//!       mem0_update_shape_with_bound_vector
//! PASS (positive controls / fail-closed guards, before AND after the fix):
//!       control_inline_vector_literal_cast_still_works
//!       control_column_named_vector_is_usable
//!       control_explicit_dim_cast_on_text_parameter_works
//!       control_empty_literal_bare_cast_still_errors
//!       guard_dimension_mismatch_on_parameter_still_rejected
//!       guard_column_dimension_enforced_on_write   (passes today for the WRONG
//!            reason — see its doc comment; the fix must keep it green for the
//!            RIGHT one)
//!       guard_empty_vector_parameter_still_rejected
//!       guard_non_numeric_text_parameter_still_rejected

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

/// The mem0 table, spelled the way mem0's pgvector store spells it — the
/// embedding column is literally named `vector`.
fn seed_mem0(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE mem0 (id INT PRIMARY KEY, vector VECTOR(3), payload TEXT)")
        .expect("CREATE TABLE mem0");
    db.execute("INSERT INTO mem0 VALUES (1, '[1.0,0.0,0.0]'::vector(3), 'north')")
        .expect("seed row 1");
    db.execute("INSERT INTO mem0 VALUES (2, '[0.0,1.0,0.0]'::vector(3), 'east')")
        .expect("seed row 2");
    db.execute("INSERT INTO mem0 VALUES (3, '[0.0,0.0,1.0]'::vector(3), 'up')")
        .expect("seed row 3");
}

fn expect_vector(v: &Value, context: &str) -> Vec<f32> {
    match v {
        Value::Vector(x) => x.clone(),
        other => panic!("{context}: expected Value::Vector, got {other:?}"),
    }
}

fn expect_int(v: &Value, context: &str) -> i64 {
    match v {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("{context}: expected an integer, got {other:?}"),
    }
}

fn expect_f64(v: &Value, context: &str) -> f64 {
    match v {
        Value::Float4(f) => f64::from(*f),
        Value::Float8(f) => *f,
        Value::Numeric(s) => s
            .parse::<f64>()
            .unwrap_or_else(|_| panic!("{context}: bad numeric {s:?}")),
        other => panic!("{context}: expected a float, got {other:?}"),
    }
}

/// The three-component unit vector a driver would bind, in the four shapes a
/// real client produces.
fn probe_pgvector_text() -> Value {
    Value::String("[1.0,0.0,0.0]".to_string())
}

fn probe_pg_array_text() -> Value {
    // What node-pg / psycopg send for a `float8[]` parameter in TEXT format,
    // and what `decode_text_parameter`'s catch-all (src/protocol/postgres/
    // prepared.rs:388) turns an unknown array OID into today.
    Value::String("{1.0,0.0,0.0}".to_string())
}

fn probe_float8_array() -> Value {
    Value::Array(vec![Value::Float8(1.0), Value::Float8(0.0), Value::Float8(0.0)])
}

fn probe_float4_array() -> Value {
    Value::Array(vec![Value::Float4(1.0), Value::Float4(0.0), Value::Float4(0.0)])
}

// ===========================================================================
// 0. Positive controls — these MUST pass on the unfixed tree too.
//    If any of them fails, the harness (not the engine) is what broke.
// ===========================================================================

/// POSITIVE CONTROL. The inline literal form the issue reports as working must
/// keep working, in both executor families. This is the assertion that proves
/// this file actually runs and that vectors work at all here.
#[test]
fn control_inline_vector_literal_cast_still_works() {
    let db = mem_db();
    seed_mem0(&db);

    // text family
    let rows = db
        .query("SELECT '[1.0,2.0,3.0]'::vector AS v", &[])
        .expect("bare ::vector on an inline literal must work (text family)");
    assert_eq!(rows.len(), 1, "one row expected");
    assert_eq!(
        expect_vector(&rows[0].values[0], "inline bare cast"),
        vec![1.0, 2.0, 3.0]
    );

    // params family, zero parameters
    let rows = db
        .query_params("SELECT '{1.0,2.0,3.0}'::vector AS v FROM mem0 WHERE id = 1", &[])
        .expect("bare ::vector on an inline `{…}` literal must work (params family)");
    assert_eq!(
        expect_vector(&rows[0].values[0], "inline bare cast, params family"),
        vec![1.0, 2.0, 3.0]
    );

    // And the distance operator over two inline literals (the issue's psql
    // line `SELECT '[1,2,3]'::vector <=> '[1,2,3]'::vector`).
    let rows = db
        .query("SELECT '[1,2,3]'::vector <=> '[1,2,3]'::vector AS d", &[])
        .expect("inline <=> inline must work");
    assert!(
        expect_f64(&rows[0].values[0], "cosine distance of a vector with itself").abs() < 1e-4,
        "cosine distance of a vector with itself must be ~0"
    );
}

/// POSITIVE CONTROL. A column literally named `vector` (mem0's spelling) is a
/// plain identifier and must be creatable, insertable and selectable. If this
/// ever fails, the mem0 tests below are failing for the WRONG reason.
#[test]
fn control_column_named_vector_is_usable() {
    let db = mem_db();
    seed_mem0(&db);
    let rows = db
        .query("SELECT id, vector, payload FROM mem0 ORDER BY id", &[])
        .expect("a column named `vector` must be selectable");
    assert_eq!(rows.len(), 3);
    assert_eq!(expect_int(&rows[0].values[0], "id"), 1);
    assert_eq!(expect_vector(&rows[0].values[1], "vector column"), vec![1.0, 0.0, 0.0]);
}

/// POSITIVE CONTROL / verdict evidence for sub-defect (b). The issue reports
/// `cur.execute("SELECT %s::vector(3)", ("[1,2,3]",))` as failing with
/// "array does not start with '{'". That message is psycopg2's RESULT parser,
/// not this engine: the EXPLICIT-dimension cast on a text parameter already
/// works. `Evaluator::cast_value`'s `DataType::Vector` arm
/// (src/sql/evaluator.rs:5645-5690) routes `Value::String` through
/// `crate::types::parse_vector_text`, which accepts `[…]`, `{…}` and bare
/// `a,b,c` (src/types.rs:303-316).
///
/// So this test PASSES on the unfixed tree, and it is what localises the real
/// (b) defect to the RESULT OID (see the wire tests), not to parameter binding.
#[test]
fn control_explicit_dim_cast_on_text_parameter_works() {
    let db = mem_db();
    seed_mem0(&db);

    for (label, param) in [
        ("pgvector `[…]` text", probe_pgvector_text()),
        ("PostgreSQL `{…}` array text", probe_pg_array_text()),
    ] {
        let rows = db
            .query_params("SELECT $1::vector(3) AS v FROM mem0 WHERE id = 1", &[param])
            .unwrap_or_else(|e| panic!("{label}: explicit ::vector(3) on a bound parameter must work, got {e}"));
        assert_eq!(rows.len(), 1, "{label}: one row");
        assert_eq!(
            expect_vector(&rows[0].values[0], label),
            vec![1.0, 0.0, 0.0],
            "{label}: value must round-trip"
        );
    }
}

/// POSITIVE CONTROL / regression guard for the existing
/// `tests/compat_ada_core.rs::vector_bare_cast_on_empty_literal_errors`:
/// `'[]'::vector` has no dimension to infer and must still be REJECTED after
/// the fix. The fix must not turn "unknown dimension" into "zero dimensions".
#[test]
fn control_empty_literal_bare_cast_still_errors() {
    let db = mem_db();
    let r = db.query("SELECT '[]'::vector", &[]);
    assert!(
        r.is_err(),
        "bare ::vector on an empty literal must still error (no dimension to infer); got {r:?}"
    );
}

// ===========================================================================
// 1. Sub-defect (a): bare `::vector` on a bound parameter — the form every
//    pgvector driver emits over the extended protocol.
// ===========================================================================

/// OPEN on the current tree.
///
/// `SELECT $1::vector` is rejected at PLAN time by src/sql/planner.rs:4547-4553
/// because the cast operand is `Expr::Value(Placeholder("$1"))`, not a quoted
/// string. pgvector resolves the dimension from the value (its `vector_in` runs
/// with typmod -1 for an unadorned cast), so all four driver-side shapes below
/// must be accepted and must produce the same 3-element vector.
#[test]
fn bare_vector_cast_on_bound_parameter() {
    let db = mem_db();
    seed_mem0(&db);

    for (label, param) in [
        (
            "pgvector `[…]` text (psycopg3/node-pg text param)",
            probe_pgvector_text(),
        ),
        (
            "PostgreSQL `{…}` array text (float8[] text param)",
            probe_pg_array_text(),
        ),
        ("float8[] bound as an array value", probe_float8_array()),
        ("float4[] bound as an array value", probe_float4_array()),
    ] {
        let rows = db
            .query_params("SELECT $1::vector AS v FROM mem0 WHERE id = 1", &[param])
            .unwrap_or_else(|e| {
                panic!(
                    "{label}: `$1::vector` must infer the dimension from the bound value \
                     (pgvector accepts an unadorned ::vector cast on a parameter), got: {e}"
                )
            });
        assert_eq!(rows.len(), 1, "{label}: one row");
        assert_eq!(
            expect_vector(&rows[0].values[0], label),
            vec![1.0, 0.0, 0.0],
            "{label}: the bound value must survive the cast unchanged"
        );
    }
}

/// OPEN on the current tree. The text-family twin: psycopg2 interpolates a
/// Python list client-side, so the SERVER sees `ARRAY[…]::vector`. The issue
/// reports this verbatim from psql:
///     SELECT ARRAY[1.0,2.0,3.0]::vector;   -- ERROR: bare ::vector cast …
/// while `ARRAY[1.0,2.0,3.0]::vector(3)` succeeds. The element count of an
/// array literal is known at plan time, so the bare form must infer 3.
#[test]
fn bare_vector_cast_on_array_literal_text_family() {
    let db = mem_db();

    let rows = db
        .query("SELECT ARRAY[1.0,2.0,3.0]::vector AS v", &[])
        .expect("`ARRAY[1.0,2.0,3.0]::vector` must infer dimension 3 from the literal's element count");
    assert_eq!(
        expect_vector(&rows[0].values[0], "ARRAY[...]::vector"),
        vec![1.0, 2.0, 3.0]
    );

    // Control inside the same test: the explicit-dimension form the issue
    // reports as already working must keep working.
    let rows = db
        .query("SELECT ARRAY[1.0,2.0,3.0]::vector(3) AS v", &[])
        .expect("ARRAY[...]::vector(3) works today and must keep working");
    assert_eq!(
        expect_vector(&rows[0].values[0], "ARRAY[...]::vector(3)"),
        vec![1.0, 2.0, 3.0]
    );

    // OPEN too, and for a THIRD reason: an all-INTEGER array literal fails the
    // `has_floats` probe at src/sql/planner.rs:4477, so it becomes
    // `Value::Array([Int4, …])` rather than `Value::Vector`, and
    // `cast_value`'s `DataType::Vector` arm has no `Value::Array` case
    // (src/sql/evaluator.rs:5687) → "Cannot cast Array([...]) to VECTOR(3)".
    // An integer-valued embedding is still a vector.
    for sql in [
        "SELECT ARRAY[1,2,3]::vector(3) AS v",
        "SELECT ARRAY[1,2,3]::vector AS v",
    ] {
        let rows = db
            .query(sql, &[])
            .unwrap_or_else(|e| panic!("`{sql}`: an integer array literal must cast to vector, got {e}"));
        assert_eq!(expect_vector(&rows[0].values[0], sql), vec![1.0, 2.0, 3.0]);
    }
}

/// OPEN on the current tree — and this is the arm that produces the issue's
/// SECOND error message, `Unsupported array element type`.
///
/// sqlparser 0.53 parses `-0.1` as `Expr::UnaryOp { Minus, Number }`, so the
/// `all_numeric` probe at src/sql/planner.rs:4474 is false for any array with a
/// negative element and the general-array arm rejects it at
/// src/sql/planner.rs:4516. Every real embedding has negative components, so
/// the psycopg2-mogrified mem0 statement ALWAYS lands here — including the
/// explicit-dimension form, which the issue believed worked.
#[test]
fn negative_element_array_literal_casts_to_vector() {
    let db = mem_db();

    for sql in [
        "SELECT ARRAY[-0.1,0.2,-0.3]::vector AS v",
        "SELECT ARRAY[-0.1,0.2,-0.3]::vector(3) AS v",
    ] {
        let rows = db.query(sql, &[]).unwrap_or_else(|e| {
            panic!(
                "`{sql}` must build a 3-element vector — a negative element is a \
                 `UnaryOp(Minus, Number)` in sqlparser and must be folded, got: {e}"
            )
        });
        let v = expect_vector(&rows[0].values[0], sql);
        assert_eq!(v.len(), 3, "`{sql}`: three components");
        assert!((v[0] - (-0.1)).abs() < 1e-6, "`{sql}`: v[0] must be -0.1, got {}", v[0]);
        assert!((v[1] - 0.2).abs() < 1e-6, "`{sql}`: v[1] must be 0.2, got {}", v[1]);
        assert!((v[2] - (-0.3)).abs() < 1e-6, "`{sql}`: v[2] must be -0.3, got {}", v[2]);
    }

    // Also OPEN today, for the SAME reason (NOT a control — `ARRAY[-1,2,-3]`
    // hits the identical `_ => Unsupported array element type` arm at
    // src/sql/planner.rs:4516). Asserted here so the sign-folding fix (F1) is
    // forced to keep an all-INTEGER array an ARRAY rather than reclassifying it
    // as a vector: `has_floats` (planner.rs:4477) must stay false for it.
    let rows = db
        .query("SELECT ARRAY[-1,2,-3] AS a", &[])
        .expect("an all-integer array literal must parse AND stay an ARRAY");
    match &rows[0].values[0] {
        Value::Array(items) => assert_eq!(items.len(), 3, "integer array keeps three elements"),
        other => panic!("ARRAY[-1,2,-3] must stay an ARRAY, got {other:?}"),
    }
}

/// OPEN on the current tree. A `float4[]`/`float8[]` ARRAY *parameter* — what
/// psycopg2 sends for a Python list and what node-pg sends for a JS array —
/// must be castable to `vector`, with an explicit dimension as well as a bare
/// one. `Evaluator::cast_value`'s `DataType::Vector` arm
/// (src/sql/evaluator.rs:5645-5690) has arms only for `Value::Vector` and
/// `Value::String`; a `Value::Array` falls to the catch-all at
/// src/sql/evaluator.rs:5687 → "Cannot cast Array([...]) to VECTOR(3)".
#[test]
fn float_array_parameter_casts_to_vector() {
    let db = mem_db();
    seed_mem0(&db);

    for (label, param) in [
        ("float8[]", probe_float8_array()),
        ("float4[]", probe_float4_array()),
        (
            "int4[] (an integer-valued embedding is still a vector)",
            Value::Array(vec![Value::Int4(1), Value::Int4(0), Value::Int4(0)]),
        ),
    ] {
        let rows = db
            .query_params("SELECT $1::vector(3) AS v FROM mem0 WHERE id = 1", &[param])
            .unwrap_or_else(|e| panic!("{label}: an array parameter must cast to vector(3), got {e}"));
        assert_eq!(
            expect_vector(&rows[0].values[0], label),
            vec![1.0, 0.0, 0.0],
            "{label}: numeric array elements must become vector components"
        );
    }
}

// ===========================================================================
// 2. The mem0 shapes, end to end.
// ===========================================================================

/// OPEN on the current tree. The issue's literal reproducer:
///
///     SELECT id, vector <=> %s::vector AS distance, payload
///     FROM mem0 ORDER BY distance LIMIT %s
///
/// Asserted end to end: it must run, it must return the rows in ASCENDING
/// distance order, and the nearest row must be the one that equals the probe.
/// Ordering is asserted explicitly because a mis-ordered KNN answer is silent.
#[test]
fn mem0_search_shape_end_to_end() {
    let db = mem_db();
    seed_mem0(&db);

    const SQL: &str = "SELECT id, vector <=> $1::vector AS distance, payload \
                       FROM mem0 ORDER BY distance LIMIT $2";

    for (label, probe) in [
        ("text probe", probe_pgvector_text()),
        ("float8[] probe", probe_float8_array()),
    ] {
        let rows = db
            .query_params(SQL, &[probe, Value::Int8(2)])
            .unwrap_or_else(|e| panic!("{label}: the mem0 search shape must execute, got: {e}"));

        assert_eq!(rows.len(), 2, "{label}: LIMIT $2 = 2 must bound the result to 2 rows");
        assert_eq!(
            expect_int(&rows[0].values[0], "nearest id"),
            1,
            "{label}: row 1 is the probe itself and must sort first"
        );
        let d0 = expect_f64(&rows[0].values[1], "nearest distance");
        let d1 = expect_f64(&rows[1].values[1], "second distance");
        assert!(d0.abs() < 1e-4, "{label}: self-distance must be ~0, got {d0}");
        assert!(
            d0 <= d1,
            "{label}: results must be ordered by ascending distance ({d0} <= {d1})"
        );
        assert_eq!(
            rows[0].values[2],
            Value::String("north".to_string()),
            "{label}: the payload column must come back intact"
        );
    }
}

/// OPEN on the current tree. mem0's insert path binds the embedding the same
/// way. Params family (`execute_params` → `execute_plan_with_params_inner`).
#[test]
fn mem0_insert_shape_with_bound_vector() {
    let db = mem_db();
    seed_mem0(&db);

    let n = db
        .execute_params(
            "INSERT INTO mem0 (id, vector, payload) VALUES ($1, $2::vector, $3)",
            &[Value::Int4(4), probe_pgvector_text(), Value::String("west".to_string())],
        )
        .expect("INSERT with `$2::vector` must work — it is mem0's insert path");
    assert_eq!(n, 1, "one row inserted");

    let rows = db
        .query("SELECT vector FROM mem0 WHERE id = 4", &[])
        .expect("read back");
    assert_eq!(
        expect_vector(&rows[0].values[0], "inserted vector"),
        vec![1.0, 0.0, 0.0]
    );
}

/// OPEN on the current tree. The THIRD params entry point:
/// `execute_params_returning` → `execute_plan_with_params_inner` with a
/// RETURNING projection. This is the one the PostgreSQL extended protocol uses
/// for `INSERT … RETURNING` (handler_extended.rs routes row-returning DML here)
/// and the one the REST/BaaS layer uses for `POST /rest/v1/<table>` with
/// `Prefer: return=representation`. A fix that only touches `query_params`
/// would leave this path broken, so it is asserted separately.
#[test]
fn mem0_insert_returning_shape_with_bound_vector() {
    let db = mem_db();
    seed_mem0(&db);

    let (n, rows) = db
        .execute_params_returning(
            "INSERT INTO mem0 (id, vector, payload) VALUES ($1, $2::vector, $3) RETURNING id, vector",
            &[
                Value::Int4(7),
                probe_float8_array(),
                Value::String("returning".to_string()),
            ],
        )
        .expect("INSERT … RETURNING with `$2::vector` must work (execute_params_returning family)");
    assert_eq!(n, 1, "one row inserted");
    assert_eq!(rows.len(), 1, "RETURNING must emit exactly one tuple");
    assert_eq!(expect_int(&rows[0].values[0], "returned id"), 7);
    assert_eq!(
        expect_vector(&rows[0].values[1], "returned vector"),
        vec![1.0, 0.0, 0.0],
        "RETURNING must carry the coerced vector, not the raw array parameter"
    );
}

/// OPEN on the current tree. mem0's update path.
#[test]
fn mem0_update_shape_with_bound_vector() {
    let db = mem_db();
    seed_mem0(&db);

    let n = db
        .execute_params(
            "UPDATE mem0 SET vector = $1::vector WHERE id = $2",
            &[
                Value::Array(vec![Value::Float8(0.5), Value::Float8(0.5), Value::Float8(0.0)]),
                Value::Int4(2),
            ],
        )
        .expect("UPDATE with `$1::vector` and an array parameter must work");
    assert_eq!(n, 1, "one row updated");

    let rows = db
        .query("SELECT vector FROM mem0 WHERE id = 2", &[])
        .expect("read back");
    let v = expect_vector(&rows[0].values[0], "updated vector");
    assert!(
        (v[0] - 0.5).abs() < 1e-6 && (v[1] - 0.5).abs() < 1e-6 && v[2].abs() < 1e-6,
        "updated vector must be [0.5,0.5,0.0], got {v:?}"
    );
}

// ===========================================================================
// 3. Fail-closed guards. These pass BEFORE and AFTER; they are what stops the
//    fix from being implemented as "accept anything".
// ===========================================================================

/// An explicit dimension must still be ENFORCED against a bound parameter.
/// Widening the cast to accept parameters must not turn `::vector(3)` into a
/// no-op — a 4-element probe against a 3-dimension cast is an error in
/// pgvector and must stay one here.
#[test]
fn guard_dimension_mismatch_on_parameter_still_rejected() {
    let db = mem_db();
    seed_mem0(&db);

    for (label, param) in [
        ("text form", Value::String("[1.0,2.0,3.0,4.0]".to_string())),
        (
            "array form",
            Value::Array(vec![
                Value::Float8(1.0),
                Value::Float8(2.0),
                Value::Float8(3.0),
                Value::Float8(4.0),
            ]),
        ),
    ] {
        let r = db.query_params("SELECT $1::vector(3) AS v FROM mem0 WHERE id = 1", &[param]);
        assert!(
            r.is_err(),
            "{label}: a 4-element parameter cast to ::vector(3) must be rejected; got {r:?}"
        );
    }
}

/// FAIL-CLOSED REQUIREMENT CREATED BY THIS FIX. Read this one carefully.
///
/// Today this test passes for the WRONG reason: the bare `$2::vector` cast is
/// rejected at plan time, so the row never reaches storage. Once the bare cast
/// starts inferring the dimension from the VALUE, the cast no longer constrains
/// anything, and the only remaining guard for a `VECTOR(3)` column is a
/// dimension check on the write path — which does not exist today
/// (`type_matches` at src/sql/executor/mod.rs:251 and src/sql/executor/scan.rs:263
/// only check `matches!(v, Value::Vector(_))`; the known gap is documented in
/// tests/compat_ada_core.rs:47-54).
///
/// So: widening the cast WITHOUT adding that check would turn a plan-time error
/// into silent corruption of a vector column, and this test would start failing.
/// It must keep passing, for the right reason.
#[test]
fn guard_column_dimension_enforced_on_write() {
    let db = mem_db();
    seed_mem0(&db);

    // params family
    let r = db.execute_params(
        "INSERT INTO mem0 (id, vector, payload) VALUES ($1, $2::vector, $3)",
        &[
            Value::Int4(9),
            Value::String("[1.0,2.0,3.0,4.0]".to_string()),
            Value::String("bad".to_string()),
        ],
    );
    assert!(
        r.is_err(),
        "a 4-element vector must not be storable in a VECTOR(3) column (params family); got {r:?}"
    );

    // text family
    let r = db.execute("INSERT INTO mem0 (id, vector, payload) VALUES (10, ARRAY[1.0,2.0,3.0,4.0]::vector, 'bad')");
    assert!(
        r.is_err(),
        "a 4-element vector must not be storable in a VECTOR(3) column (text family); got {r:?}"
    );

    assert_eq!(
        db.query("SELECT id FROM mem0", &[]).expect("scan").len(),
        3,
        "no wrong-dimension row may have landed in mem0"
    );
}

/// An EMPTY parameter has no dimension to infer and must be rejected, not
/// silently accepted as a zero-dimensional vector. This is the parameterised
/// twin of `control_empty_literal_bare_cast_still_errors`, and it is the
/// assertion that forbids implementing the fix as "dimension 0 = anything".
#[test]
fn guard_empty_vector_parameter_still_rejected() {
    let db = mem_db();
    seed_mem0(&db);

    for (label, param) in [
        ("empty `[]` text", Value::String("[]".to_string())),
        ("empty `{}` text", Value::String("{}".to_string())),
        ("empty array value", Value::Array(vec![])),
    ] {
        // NON-VACUOUS TODAY: `::vector(3)` plans fine on the current tree, so
        // this arm exercises `Evaluator::cast_value` (src/sql/evaluator.rs:5645)
        // rather than the blanket plan-time refusal, and it must stay an error
        // after F2 adds the `Value::Array` arm.
        let r = db.query_params("SELECT $1::vector(3) AS v FROM mem0 WHERE id = 1", &[param.clone()]);
        assert!(
            r.is_err(),
            "{label}: an empty value must never satisfy ::vector(3); got {r:?}"
        );
        // VACUOUS TODAY (the bare cast is refused at plan time whatever the
        // value) — this is the post-fix half of the guard: once F3 makes the
        // bare form infer from the value, "no elements" must be an ERROR and
        // never a zero-dimensional vector.
        let r = db.query_params("SELECT $1::vector AS v FROM mem0 WHERE id = 1", &[param]);
        assert!(
            r.is_err(),
            "{label}: an empty value has no dimension and must be rejected by a bare ::vector; got {r:?}"
        );
    }
}

/// Junk must not become a vector. A bare `::vector` on a non-numeric parameter
/// must ERROR, never coerce to an empty or garbage vector — the security-shaped
/// half of "infer from the value".
#[test]
fn guard_non_numeric_text_parameter_still_rejected() {
    let db = mem_db();
    seed_mem0(&db);

    for (label, param) in [
        ("prose", Value::String("not a vector".to_string())),
        ("json object", Value::String("{\"a\":1}".to_string())),
        ("half-open bracket", Value::String("[1,2,3".to_string())),
        (
            "array of strings",
            Value::Array(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
                Value::String("c".to_string()),
            ]),
        ),
    ] {
        // NON-VACUOUS TODAY: the explicit-dimension spelling plans, so this
        // arm really runs `cast_value` and proves junk is refused there — the
        // half of the guard that survives F3 unchanged.
        let r = db.query_params("SELECT $1::vector(3) AS v FROM mem0 WHERE id = 1", &[param.clone()]);
        assert!(
            r.is_err(),
            "{label}: a non-numeric parameter must not satisfy ::vector(3); got {r:?}"
        );
        // Post-fix half: the bare form must reject the same junk rather than
        // coercing it to an empty or garbage vector.
        let r = db.query_params("SELECT $1::vector AS v FROM mem0 WHERE id = 1", &[param]);
        assert!(
            r.is_err(),
            "{label}: a non-numeric parameter must not cast to vector; got {r:?}"
        );
    }
}
