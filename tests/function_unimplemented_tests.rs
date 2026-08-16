//! User-defined FUNCTIONS are NOT callable. PROCEDURES work, under one specific rule.
//! This suite pins both, exactly as they ship today.
//!
//! HeliosDB Nano accepts and registers every `CREATE FUNCTION` form, and then nothing in the
//! database can call the result. Three independent, unconditional breaks:
//!
//!   1. `src/sql/evaluator.rs` has ZERO references to any function registry. Its
//!      scalar-function match ends in `_ => Err("Unknown scalar function: {}")`
//!      (`src/sql/evaluator.rs:1154`), so no expression on any path — select list, `WHERE`,
//!      projection, or the bound-parameter family — can resolve a user function.
//!   2. `FunctionRegistry::execute_function` (`src/sql/functions.rs:190`) has exactly ONE call
//!      site in `src/`, and it is inside `mod tests` (`src/sql/functions.rs:603`; `#[cfg(test)]`
//!      begins at line 462). The executor works in its own unit test and is never reached in
//!      production.
//!   3. `Planner::is_table_function` (`src/sql/planner.rs:2078`) is
//!      `matches!(name, "generate_series" | "unnest")` — a fixed whitelist — so
//!      `SELECT * FROM my_udf()` cannot resolve a user function either.
//!
//! PROCEDURES ARE THE OPPOSITE CASE, and the contrast is the point: `execute_procedure` HAS a
//! real call site (`EmbeddedDatabase::execute_call_plan`, `src/lib.rs`), so `CALL` runs and its
//! arguments bind. One rule governs the binding, and this suite pins it:
//!
//! NOTE: this suite exercises the TEXT executor family only (`db.execute()`). That was once the
//! only family in which `CALL` did anything at all — see `tests/call_parity_tests.rs` and
//! ROADMAP §2.11 for the params family, where `CALL` was a silent no-op until both families were
//! routed through one shared implementation.
//!
//!   * **The `$` sigil is mandatory.** Only `$<paramname>` and `$N` are placeholders, so a
//!     bare parameter name survives into the planner as a column reference — in BOTH
//!     languages. This is deliberate: PostgreSQL resolves bare PL/pgSQL variable names, and
//!     Nano does not, so that a variable can never silently shadow a column of the same name.
//!
//! Both languages bind, through the SAME scanner (`src/sql/interpolate.rs`, called from
//! `execute_sql_procedure` and from `ExecutionContext::interpolate`). `LANGUAGE plpgsql` used
//! not to substitute anything at all — it declared parameters into the procedural scope and
//! then passed body statements to the executor verbatim — which is what the two `plpgsql`
//! tests below used to pin; they now assert the substitution instead. See
//! `tests/procedure_interpolation_tests.rs` for the full interpolation matrix (literal,
//! comment and dollar-quote regions, prefix capture, and re-scanning of substituted values).
//!
//! WHY THIS SUITE EXISTS. v4.10.1 documented triggers as unimplemented and told users to reach
//! for "a `CREATE PROCEDURE` invoked with `CALL` (procedures do execute)" instead. Both halves of
//! that advice needed checking. Functions cannot be called at all; the procedure advice was
//! sound but under-specified, and an agent following it without the sigil rule above lands on a
//! form that errors. The pre-existing `tests/plpgsql_hardening_tests.rs` covers the same ground in the
//! `if result.is_ok()` / `match { Ok => assert…, Err => eprintln! }` style that provides no
//! regression protection — `test_function_in_select_scalar` and
//! `test_plpgsql_return_from_function` both always take the `Err` arm and only `eprintln!` it. It
//! is left in place (its header does state the limitation, and several of its procedure tests
//! assert unconditionally) but it is not what pins this behaviour.
//!
//! EMBEDDED vs WIRE. `information_schema.routines`, `information_schema.parameters` and `pg_proc`
//! are wire-only catalog surfaces (`src/protocol/postgres/catalog.rs`). On the PostgreSQL wire all
//! three resolve and return zero rows with a function registered. This is an embedded-API suite,
//! so it asserts what the EMBEDDED path does: `information_schema.routines` / `.parameters` do not
//! resolve at all (they are absent from both embedded system-view registries), and `pg_proc`
//! resolves as a registered empty stub returning zero rows
//! (`src/sql/phase3/system_views.rs:2346` registers it, `:2811` returns `Ok(vec![])`).
//!
//! HOW TO MAINTAIN THIS FILE. Every test asserts unconditionally. Never introduce an `is_ok()`
//! guard, and never assert `rows.len() > 0` against `SELECT COUNT(*)` — a count query returns
//! exactly one row whether the count is 0 or 10,000, so that assertion is always true and proves
//! nothing. Use `rows_in()` below instead.
//!
//! THE PROCEDURE TESTS ARE NOT "UNIMPLEMENTED" PINS. Every one of them asserts a feature that
//! works and must keep working; treat a failure there as a regression. The only limitation
//! among them is the bare-name pair, and that is by design (the sigil rule above), not a filed
//! gap. ROADMAP §2.9 fix shape (b) — plpgsql argument binding — has landed.
//!
//! IF A CALL TEST FAILS BECAUSE THE FUNCTION RESOLVED: user-defined functions have been
//! implemented. Do NOT "fix" the test. Delete this suite, write real function tests (scalar calls
//! in every clause, arity and type checking, `LANGUAGE sql` vs `LANGUAGE plpgsql` bodies,
//! `RETURNS TABLE`, volatility vs the result cache, recursion, `DROP FUNCTION` while in use), and
//! update the docs that currently document this as unimplemented: `README.md`, `AGENTS.md`,
//! `docs/llms.txt`, `docs/compatibility/plpgsql.md`,
//! `docs/compatibility/information_schema.md`, `docs/plans/ROADMAP_V5.md` (§2.9), and the
//! `.claude/skills/heliosdb-nano-schema/SKILL.md` (Recipe 6) / `heliosdb-nano-migrate` skills.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

/// Rows physically present in `table`. Deliberately NOT `SELECT COUNT(*)`: a count query
/// always returns exactly one row, so `rows.len()` on it can never detect an empty table.
fn rows_in(db: &EmbeddedDatabase, table: &str) -> usize {
    let sql = format!("SELECT * FROM {table}");
    db.query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .len()
}

/// Run `sql` through the embedded executor and return the error it must produce.
fn err_of(db: &EmbeddedDatabase, sql: &str) -> String {
    match db.query(sql, &[]) {
        Ok(rows) => panic!("`{sql}` must fail, but returned {} row(s)", rows.len()),
        Err(e) => e.to_string(),
    }
}

/// Subject table `fn_posts(id, author_id)` plus the scalar function `fn_dbl(x)` that every
/// invocation test tries — and fails — to call.
fn setup(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE fn_posts (id INT, author_id INT)").unwrap();
    db.execute("INSERT INTO fn_posts (id, author_id) VALUES (1, 7), (2, 7)")
        .unwrap();
    db.execute("CREATE FUNCTION fn_dbl(x INTEGER) RETURNS INTEGER AS $$ SELECT x * 2 $$ LANGUAGE sql")
        .expect("CREATE FUNCTION ... LANGUAGE sql must be accepted");
}

// ===========================================================================
// Registration: all three CREATE FUNCTION forms are accepted and stored.
// ===========================================================================

#[test]
fn create_function_language_plpgsql_registers() {
    let db = db();
    db.execute("CREATE TABLE fn_posts (id INT, author_id INT)").unwrap();

    db.execute(
        "CREATE FUNCTION fn_post_count(uid INTEGER) RETURNS INTEGER AS $$
         DECLARE cnt INTEGER;
         BEGIN
             SELECT COUNT(*) INTO cnt FROM fn_posts WHERE author_id = uid;
             RETURN cnt;
         END;
         $$ LANGUAGE plpgsql",
    )
    .expect("CREATE FUNCTION ... LANGUAGE plpgsql must be accepted");

    assert!(
        db.function_registry.function_exists("fn_post_count"),
        "the function must be stored in the registry"
    );
}

#[test]
fn create_function_language_sql_registers() {
    let db = db();
    db.execute("CREATE FUNCTION fn_dbl(x INTEGER) RETURNS INTEGER AS $$ SELECT x * 2 $$ LANGUAGE sql")
        .expect("CREATE FUNCTION ... LANGUAGE sql must be accepted");

    assert!(db.function_registry.function_exists("fn_dbl"), "must be stored");
}

#[test]
fn create_function_returns_return_expr_registers() {
    let db = db();
    db.execute("CREATE FUNCTION fn_dbl2(x INTEGER) RETURNS INTEGER RETURN x * 2")
        .expect("the `RETURNS <type> RETURN <expr>` form must be accepted");

    assert!(db.function_registry.function_exists("fn_dbl2"), "must be stored");
}

#[test]
fn duplicate_create_function_without_or_replace_is_rejected() {
    // Independent proof that the first CREATE really stored something: a second one collides.
    let db = db();
    setup(&db);

    let sql = "CREATE FUNCTION fn_dbl(x INTEGER) RETURNS INTEGER AS $$ SELECT x * 2 $$ LANGUAGE sql";
    let err = db
        .execute(sql)
        .expect_err("re-creating the same function must fail")
        .to_string();
    assert!(err.contains("already exists"), "expected 'already exists', got: {err}");
}

// ===========================================================================
// Invocation: every route fails. This is the headline pin.
//
// If one of these starts succeeding, user-defined functions have been implemented —
// rewrite this suite rather than relaxing the test (see the file header).
// ===========================================================================

#[test]
fn scalar_call_in_select_list_is_unknown() {
    let db = db();
    db.execute("CREATE TABLE fn_posts (id INT, author_id INT)").unwrap();
    db.execute(
        "CREATE FUNCTION fn_post_count(uid INTEGER) RETURNS INTEGER AS $$
         DECLARE cnt INTEGER;
         BEGIN
             SELECT COUNT(*) INTO cnt FROM fn_posts WHERE author_id = uid;
             RETURN cnt;
         END;
         $$ LANGUAGE plpgsql",
    )
    .unwrap();

    let err = err_of(&db, "SELECT fn_post_count(7)");
    assert!(
        err.contains("Unknown scalar function"),
        "a registered function must still be unknown to the evaluator, got: {err}"
    );
    assert!(
        err.contains("fn_post_count"),
        "the error should name the function: {err}"
    );
}

#[test]
fn scalar_call_bare_select_is_unknown() {
    let db = db();
    setup(&db);

    let err = err_of(&db, "SELECT fn_dbl(21)");
    assert!(
        err.contains("Unknown scalar function"),
        "expected 'Unknown scalar function', got: {err}"
    );
}

#[test]
fn schema_qualified_call_is_unknown() {
    let db = db();
    setup(&db);

    // Qualification does not help — the qualified text is what fails to resolve.
    let err = err_of(&db, "SELECT public.fn_dbl(21)");
    assert!(
        err.contains("Unknown scalar function"),
        "expected 'Unknown scalar function', got: {err}"
    );
}

#[test]
fn call_in_projection_alongside_columns_is_unknown() {
    let db = db();
    setup(&db);

    let err = err_of(&db, "SELECT id, fn_dbl(id) FROM fn_posts");
    assert!(
        err.contains("Unknown scalar function"),
        "expected 'Unknown scalar function', got: {err}"
    );
}

#[test]
fn call_in_where_clause_is_unknown() {
    let db = db();
    setup(&db);

    let err = err_of(&db, "SELECT id FROM fn_posts WHERE fn_dbl(id) = 2");
    assert!(
        err.contains("Unknown scalar function"),
        "expected 'Unknown scalar function', got: {err}"
    );
}

#[test]
fn table_function_call_resolves_as_a_missing_table() {
    let db = db();
    setup(&db);

    // Distinct failure class: `is_table_function` is a fixed `generate_series | unnest`
    // whitelist, so `FROM fn_dbl(21)` is planned as a plain table reference and the planner
    // looks for a *table* named fn_dbl.
    let err = err_of(&db, "SELECT * FROM fn_dbl(21)");
    assert!(err.contains("does not exist"), "expected 'does not exist', got: {err}");
    assert!(err.contains("fn_dbl"), "the error should name fn_dbl: {err}");
    assert!(
        !err.contains("Unknown scalar function"),
        "this route fails as a missing table, not as an unknown scalar function: {err}"
    );
}

#[test]
fn call_statement_does_not_reach_a_function() {
    let db = db();
    setup(&db);

    // Functions and procedures are separate registries; `CALL` only consults procedures.
    let err = db
        .execute("CALL fn_dbl(21)")
        .expect_err("CALL must not resolve a function")
        .to_string();
    assert!(err.contains("Procedure"), "expected a procedure diagnostic, got: {err}");
    assert!(err.contains("does not exist"), "expected 'does not exist', got: {err}");
}

#[test]
fn perform_is_not_a_statement() {
    let db = db();
    setup(&db);

    // PL/pgSQL's `PERFORM` has no statement-level grammar here, so it cannot be used to
    // invoke a function for its side effects either.
    let res = db.execute("PERFORM fn_dbl(21)");
    assert!(res.is_err(), "PERFORM must not parse as a statement");
}

#[test]
fn bound_parameter_family_is_also_unknown() {
    // The params executor family is a separate code path from `execute`/`query`; it fails
    // identically, because the break is in the shared evaluator.
    let db = db();
    setup(&db);

    let err = db
        .execute_params("SELECT fn_dbl($1)", &[Value::Int4(21)])
        .expect_err("the bound-parameter path must fail too")
        .to_string();
    assert!(
        err.contains("Unknown scalar function"),
        "expected 'Unknown scalar function' from the params family, got: {err}"
    );
}

// ===========================================================================
// Procedures: `CALL` executes AND binds arguments, in both languages, via a `$`-sigil
// reference. These are the working forms; a failure here is a REGRESSION, not a
// limitation that started working. See the file header.
// ===========================================================================

/// The single table every procedure body below writes into.
fn proc_setup(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE fn_audit (id INTEGER, note TEXT)").unwrap();
}

/// `(id, note)` of the single row in `fn_audit`.
fn only_audit_row(db: &EmbeddedDatabase) -> (i32, String) {
    let rows = db.query("SELECT id, note FROM fn_audit", &[]).expect("audit read");
    assert_eq!(rows.len(), 1, "expected exactly one audit row, got {}", rows.len());
    let row = rows.first().expect("one row");
    match (row.values.first(), row.values.get(1)) {
        (Some(Value::Int4(id)), Some(Value::String(note))) => (*id, note.clone()),
        other => panic!("unexpected audit row shape: {other:?}"),
    }
}

#[test]
fn language_sql_procedure_binds_parameters_by_name() {
    let db = db();
    proc_setup(&db);

    // The documented working form: LANGUAGE sql + `$<paramname>`.
    db.execute(
        "CREATE PROCEDURE fn_p_named(p_id INTEGER, p_op TEXT) LANGUAGE sql \
         AS $$INSERT INTO fn_audit VALUES ($p_id, $p_op)$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL fn_p_named(42, 'hello')")
        .expect("CALL must execute the body");

    assert_eq!(
        only_audit_row(&db),
        (42, "hello".to_string()),
        "both arguments must be substituted into the body"
    );
}

#[test]
fn language_sql_procedure_binds_parameters_positionally() {
    let db = db();
    proc_setup(&db);

    // The same mechanism, spelled positionally.
    db.execute(
        "CREATE PROCEDURE fn_p_pos(p_id INTEGER, p_op TEXT) LANGUAGE sql \
         AS $$INSERT INTO fn_audit VALUES ($1, $2)$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL fn_p_pos(7, 'seven')")
        .expect("CALL must execute the body");

    assert_eq!(
        only_audit_row(&db),
        (7, "seven".to_string()),
        "$1/$2 must be substituted from the call's arguments"
    );
}

#[test]
fn procedure_with_no_parameters_executes() {
    let db = db();
    proc_setup(&db);

    db.execute("CREATE PROCEDURE fn_p_const() LANGUAGE sql AS $$INSERT INTO fn_audit VALUES (0, 'const')$$")
        .expect("CREATE PROCEDURE must be accepted");
    db.execute("CALL fn_p_const()").expect("CALL must execute the body");

    assert_eq!(only_audit_row(&db), (0, "const".to_string()));
}

#[test]
fn procedure_argument_is_silently_discarded_when_the_body_ignores_it() {
    let db = db();
    proc_setup(&db);

    db.execute(
        "CREATE PROCEDURE fn_p_ignore(p_id INTEGER) LANGUAGE sql \
         AS $$INSERT INTO fn_audit VALUES (0, 'ignored')$$",
    )
    .expect("CREATE PROCEDURE with a parameter must be accepted");

    db.execute("CALL fn_p_ignore(7)")
        .expect("CALL must succeed when the body ignores the parameter");

    // Substitution is textual: a body that never spells `$p_id` simply drops the argument.
    // No error, no warning — worth knowing, but not a defect.
    assert_eq!(only_audit_row(&db), (0, "ignored".to_string()), "7 must go nowhere");
}

// ---------------------------------------------------------------------------
// `LANGUAGE plpgsql` binds too, through the same scanner...
// ---------------------------------------------------------------------------

#[test]
fn language_plpgsql_procedure_binds_named_parameters() {
    let db = db();
    proc_setup(&db);

    db.execute(
        "CREATE PROCEDURE fn_p_pg_named(p_id INTEGER) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO fn_audit VALUES ($p_id, 'x'); END$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    // Was: `Invalid parameter placeholder: $p_id` — nothing substituted `$p_id`, so it
    // reached the planner's placeholder parser, which requires a numeric index. The
    // procedural runtime now interpolates it from the variable scope before the
    // statement is executed (`ExecutionContext::interpolate`).
    db.execute("CALL fn_p_pg_named(7)")
        .expect("a plpgsql body must substitute $<paramname>");

    assert_eq!(only_audit_row(&db), (7, "x".to_string()));
}

#[test]
fn language_plpgsql_procedure_binds_positional_parameters() {
    let db = db();
    proc_setup(&db);

    db.execute(
        "CREATE PROCEDURE fn_p_pg_pos(p_id INTEGER) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO fn_audit VALUES ($1, 'x'); END$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    // Was: `Parameter $1 not provided` — the executor closure runs body statements with
    // no bind parameters (`db_clone.execute(sql)` in `execute_call_plan`), so `$1` arrived
    // unbound. The call's arguments now reach the runtime as
    // `ExecutionContext::positional_params` and are interpolated textually.
    db.execute("CALL fn_p_pg_pos(7)")
        .expect("a plpgsql body must substitute $1 from the call's arguments");

    assert_eq!(only_audit_row(&db), (7, "x".to_string()));
}

// ---------------------------------------------------------------------------
// ...but the `$` sigil is still mandatory, pinned by its failures.
// ---------------------------------------------------------------------------

#[test]
fn bare_parameter_name_fails_in_language_sql() {
    let db = db();
    proc_setup(&db);

    // Substitution only matches `$`-prefixed tokens, so a bare `n` survives into the planner
    // as a column reference against the INSERT's target schema.
    db.execute("CREATE PROCEDURE fn_p_bare_sql(n INTEGER) LANGUAGE sql AS $$INSERT INTO fn_audit VALUES (n, 'x')$$")
        .expect("CREATE PROCEDURE must be accepted");

    let err = db
        .execute("CALL fn_p_bare_sql(7)")
        .expect_err("a bare parameter name must not resolve")
        .to_string();
    assert!(
        err.contains("not found in schema"),
        "expected a column-resolution error, got: {err}"
    );
    assert_eq!(rows_in(&db, "fn_audit"), 0, "nothing may have been written");
}

#[test]
fn bare_parameter_name_fails_in_language_plpgsql_too() {
    let db = db();
    proc_setup(&db);

    db.execute(
        "CREATE PROCEDURE fn_p_bare_pg(n INTEGER) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO fn_audit VALUES (n, 'x'); END$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    let err = db
        .execute("CALL fn_p_bare_pg(7)")
        .expect_err("a bare parameter name must not resolve")
        .to_string();
    assert!(
        err.contains("not found in schema"),
        "expected a column-resolution error, got: {err}"
    );
    assert_eq!(rows_in(&db, "fn_audit"), 0, "nothing may have been written");
}

// ===========================================================================
// Introspection: a registered routine is invisible.
//
// See the EMBEDDED vs WIRE note in the file header — over the PostgreSQL wire all three
// surfaces resolve and return zero rows; these assertions pin the embedded path.
// ===========================================================================

#[test]
fn pg_proc_is_empty_with_a_function_registered() {
    let db = db();
    setup(&db);
    assert!(db.function_registry.function_exists("fn_dbl"), "precondition");

    // `pg_proc` is a registered empty stub: it resolves, and it returns nothing, always.
    let rows = db
        .query("SELECT * FROM pg_proc", &[])
        .expect("pg_proc must resolve as a system view");
    assert_eq!(rows.len(), 0, "pg_proc must be empty even with a function registered");
}

#[test]
fn information_schema_routines_does_not_resolve_on_the_embedded_path() {
    let db = db();
    setup(&db);

    // Neither embedded system-view registry defines `information_schema.routines`. On the wire
    // it exists and returns zero rows (`query_information_schema_routines` returns
    // `(schema, vec![])` by construction, `src/protocol/postgres/catalog.rs:2398`).
    let res = db.query("SELECT * FROM information_schema.routines", &[]);
    assert!(
        res.is_err(),
        "information_schema.routines is not an embedded surface, got: {res:?}"
    );
}

#[test]
fn information_schema_parameters_does_not_resolve_on_the_embedded_path() {
    let db = db();
    setup(&db);

    let res = db.query("SELECT * FROM information_schema.parameters", &[]);
    assert!(
        res.is_err(),
        "information_schema.parameters is not an embedded surface, got: {res:?}"
    );
}

#[test]
fn a_registered_procedure_is_equally_invisible() {
    let db = db();
    proc_setup(&db);
    db.execute("CREATE PROCEDURE fn_p_const() LANGUAGE sql AS $$INSERT INTO fn_audit VALUES (0, 'const')$$")
        .unwrap();
    assert!(db.function_registry.procedure_exists("fn_p_const"), "precondition");

    let rows = db.query("SELECT * FROM pg_proc", &[]).expect("pg_proc must resolve");
    assert_eq!(rows.len(), 0, "pg_proc must be empty with a procedure registered too");
}
