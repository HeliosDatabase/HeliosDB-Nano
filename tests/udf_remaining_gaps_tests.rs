//! What user-defined functions still CANNOT do, pinned with hard assertions.
//!
//! Scalar invocation landed (see `tests/udf_invocation_tests.rs`). These are the
//! pieces deliberately left out of that change, each documented as a gap rather
//! than hidden behind a partial implementation. Every test here asserts
//! unconditionally: none of them is an `is_ok()` guard, and none of them prints
//! instead of asserting. If one starts failing because the feature arrived,
//! DELETE the test and write real coverage for the new behaviour.
//!
//! Companion pins for the paths that DO work live in
//! `tests/udf_invocation_tests.rs`.
//!
//! Function names here are unique per test: UDF resolution is process-global
//! (`src/sql/udf_bridge.rs`), so a name shared with a concurrently-open database
//! would make these assertions depend on test scheduling.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::EmbeddedDatabase;

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

fn err_of(db: &EmbeddedDatabase, sql: &str) -> String {
    match db.query(sql, &[]) {
        Ok(rows) => panic!("`{sql}` must fail, but returned {} row(s)", rows.len()),
        Err(e) => e.to_string(),
    }
}

// ===========================================================================
// GAP: set-returning functions. `Planner::is_table_function` is still the fixed
// `generate_series | unnest` whitelist, so `SELECT * FROM f()` plans as a plain
// table reference. Lifting it needs a return-signature slot on `StoredFunction`,
// which is a bincode-positional WAL/`meta:` payload — hence deferred.
// ===========================================================================

#[test]
fn select_star_from_a_function_still_resolves_as_a_missing_table() {
    let db = db();
    db.execute("CREATE FUNCTION g_srf(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 $$ LANGUAGE sql")
        .unwrap();
    assert!(db.function_registry.function_exists("g_srf"), "precondition");

    let err = err_of(&db, "SELECT * FROM g_srf(1)");
    assert!(err.contains("does not exist"), "expected 'does not exist', got: {err}");
    assert!(err.contains("g_srf"), "the error should name the function: {err}");
    assert!(
        !err.contains("Unknown scalar function"),
        "this route fails as a MISSING TABLE, not as an unknown scalar function: {err}"
    );
}

#[test]
fn returns_table_is_accepted_but_its_column_list_is_discarded() {
    // `RETURNS TABLE(...)` parses (the planner accepts it as a `Custom("TABLE")`
    // type) and the function registers, but neither `LogicalPlan::CreateFunction`
    // nor `StoredFunction` has anywhere to keep the column list, and
    // `execute_function` returns a single scalar `Value`.
    let db = db();
    db.execute("CREATE FUNCTION g_tab() RETURNS TABLE(a INT) AS $$ SELECT 1 $$ LANGUAGE sql")
        .expect("RETURNS TABLE must still be accepted");
    assert!(db.function_registry.function_exists("g_tab"));

    let err = err_of(&db, "SELECT * FROM g_tab()");
    assert!(err.contains("does not exist"), "got: {err}");
}

// ===========================================================================
// GAP: overload resolution. The registry keys on the LOWERCASE NAME ONLY, with
// no signature, so two same-name functions collide regardless of parameters.
// ===========================================================================

#[test]
fn two_functions_with_the_same_name_collide_even_with_different_signatures() {
    let db = db();
    db.execute("CREATE FUNCTION g_over(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 $$ LANGUAGE sql")
        .unwrap();

    let err = db
        .execute("CREATE FUNCTION g_over(a TEXT, b TEXT) RETURNS TEXT AS $$ SELECT $1 $$ LANGUAGE sql")
        .expect_err("no overloading: the second definition must collide")
        .to_string();
    assert!(err.contains("already exists"), "got: {err}");
}

// ===========================================================================
// GAP: functions and procedures are separate namespaces; CALL only sees
// procedures, and there is no statement-level PERFORM.
// ===========================================================================

#[test]
fn call_still_cannot_reach_a_function() {
    let db = db();
    db.execute("CREATE FUNCTION g_call(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 $$ LANGUAGE sql")
        .unwrap();

    let err = db
        .execute("CALL g_call(1)")
        .expect_err("CALL must not resolve a function")
        .to_string();
    assert!(err.contains("Procedure"), "expected a procedure diagnostic, got: {err}");
    assert!(err.contains("does not exist"), "got: {err}");
}

#[test]
fn perform_is_still_not_a_statement() {
    let db = db();
    db.execute("CREATE FUNCTION g_perform(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 $$ LANGUAGE sql")
        .unwrap();
    assert!(
        db.execute("PERFORM g_perform(1)").is_err(),
        "PERFORM must not parse as a statement"
    );
}

// ===========================================================================
// GAP: only `public.` qualification resolves. The registry is schema-less, so
// any other qualifier is not silently redirected — it stays unknown.
// ===========================================================================

#[test]
fn a_non_public_schema_qualifier_does_not_resolve() {
    let db = db();
    db.execute("CREATE FUNCTION g_qual(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
        .unwrap();

    let err = err_of(&db, "SELECT reporting.g_qual(1)");
    assert!(err.contains("Unknown scalar function"), "got: {err}");
}

// ===========================================================================
// GAP: the `RETURNS <type> RETURN <expr>` spelling registers, but its body is
// stored as the literal text `RETURN <expr>`, which is not a SQL statement.
// ===========================================================================

#[test]
fn the_returns_return_expr_form_registers_but_cannot_execute() {
    let db = db();
    db.execute("CREATE FUNCTION g_retexpr(x INTEGER) RETURNS INTEGER RETURN x * 2")
        .expect("the `RETURNS <type> RETURN <expr>` form must still be accepted");
    assert!(db.function_registry.function_exists("g_retexpr"), "it registers");

    // It RESOLVES (so this is not an "unknown function"); it fails when the
    // stored body is handed to the executor.
    let err = err_of(&db, "SELECT g_retexpr(21)");
    assert!(
        !err.contains("Unknown scalar function"),
        "the function resolves — the failure is its body, got: {err}"
    );
}

// ===========================================================================
// GAP: introspection. `pg_proc` is still a registered empty stub on the
// embedded path — populating it needs a `FunctionRegistry` handle inside
// `SystemViewRegistry::execute(&StorageEngine)` (filed as HC3).
// ===========================================================================

#[test]
fn pg_proc_is_still_empty_with_a_function_and_a_procedure_registered() {
    let db = db();
    db.execute("CREATE TABLE g_audit (id INTEGER)").unwrap();
    db.execute("CREATE FUNCTION g_visible(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 $$ LANGUAGE sql")
        .unwrap();
    db.execute("CREATE PROCEDURE g_proc(p INTEGER) LANGUAGE sql AS $$INSERT INTO g_audit VALUES ($p)$$")
        .unwrap();
    assert!(db.function_registry.function_exists("g_visible"), "precondition");
    assert!(db.function_registry.procedure_exists("g_proc"), "precondition");

    let rows = db
        .query("SELECT * FROM pg_proc", &[])
        .expect("pg_proc must resolve as a system view");
    assert_eq!(
        rows.len(),
        0,
        "pg_proc is still an empty stub even with routines registered"
    );
}
