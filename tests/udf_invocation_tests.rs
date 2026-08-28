//! User-defined FUNCTIONS are callable, on both DML executor families, and they
//! survive a restart. This suite pins that end to end.
//!
//! It REPLACES `tests/function_unimplemented_tests.rs`, whose header mandated its
//! own deletion the moment a call test started passing. What changed:
//!
//!   1. `src/sql/udf_bridge.rs` — a process-scoped handle (the same pattern
//!      `sql::sequences` uses for `nextval`) that lets the session-less
//!      `Evaluator` reach a `FunctionRegistry` and a re-entrant SQL executor.
//!   2. `src/sql/evaluator.rs` — the TERMINAL arm of scalar dispatch (reached
//!      only after every built-in name has missed) consults that handle before
//!      producing `Unknown scalar function`. The already-complete interpreter,
//!      `FunctionRegistry::execute_function`, finally has a production caller.
//!   3. `src/lib.rs` — `CREATE/DROP FUNCTION` and `CREATE/DROP PROCEDURE` run
//!      through four shared helpers called by BOTH executor families, and write
//!      a durable `meta:function:` / `meta:procedure:` record that the next open
//!      loads back into the registry.
//!
//! THE `$` SIGIL IS STILL MANDATORY, in both languages, for both functions and
//! procedures. Only `$<paramname>` and `$N` are placeholders; a bare parameter
//! name survives into the planner as a column reference. That is deliberate (a
//! variable must never silently shadow a column) and it is pinned here and in
//! `tests/udf_remaining_gaps_tests.rs`.
//!
//! HOW TO MAINTAIN THIS FILE. Every test asserts unconditionally. Never
//! introduce an `is_ok()` guard or an `Err => eprintln!` arm — that style is why
//! `tests/plpgsql_hardening_tests.rs` provided no protection here for two
//! releases. Never assert `rows.len() > 0` against `SELECT COUNT(*)`: a count
//! query returns exactly one row whether the count is 0 or 10,000.
//!
//! FUNCTION NAMES MUST BE UNIQUE PER TEST whenever the assertion depends on the
//! function's DEFINITION, its ABSENCE, or the state of the database its body
//! runs against. UDF resolution is process-global (`sql::udf_bridge` resolves a
//! bare name against the most recently opened live database that defines it —
//! see that module's header for why), and `cargo test` runs these in parallel in
//! ONE process. `fn_dbl` is shared below only because every test defines it
//! identically as a pure `$1 * 2` that touches no table, so which live database
//! answers cannot change the result. Anything else — DROP, CREATE OR REPLACE, a
//! body that reads a table, a per-database config such as the transaction guard
//! or the call-depth limit — gets its own name.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{Config, EmbeddedDatabase, Value};

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

/// Integral value of a numeric `Value`, regardless of which integer width the
/// expression evaluator produced. Asserting on a specific variant here would pin
/// arithmetic-width behaviour that has nothing to do with UDFs.
fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        Value::Numeric(s) => s.parse::<i64>().unwrap_or_else(|e| panic!("numeric `{s}`: {e}")),
        other => panic!("expected an integer value, got {other:?}"),
    }
}

/// The single scalar a one-row/one-column query must produce.
fn scalar(db: &EmbeddedDatabase, sql: &str) -> Value {
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    assert_eq!(rows.len(), 1, "`{sql}` must return exactly one row, got {}", rows.len());
    rows[0]
        .values
        .first()
        .unwrap_or_else(|| panic!("`{sql}` returned a row with no columns"))
        .clone()
}

/// Run `sql` and return the error it must produce.
fn err_of(db: &EmbeddedDatabase, sql: &str) -> String {
    match db.query(sql, &[]) {
        Ok(rows) => panic!("`{sql}` must fail, but returned {} row(s)", rows.len()),
        Err(e) => e.to_string(),
    }
}

/// Rows physically present in `table`. Deliberately NOT `SELECT COUNT(*)`.
fn rows_in(db: &EmbeddedDatabase, table: &str) -> usize {
    let sql = format!("SELECT * FROM {table}");
    db.query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .len()
}

/// `fn_posts(id, author_id)` with two rows for author 7, plus the scalar
/// `fn_dbl(x) = x * 2` that most invocation tests below call.
fn setup(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE fn_posts (id INT, author_id INT)").unwrap();
    db.execute("INSERT INTO fn_posts (id, author_id) VALUES (1, 7), (2, 7)")
        .unwrap();
    db.execute("CREATE FUNCTION fn_dbl(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
        .expect("CREATE FUNCTION ... LANGUAGE sql must be accepted");
}

// ===========================================================================
// Scalar invocation — the headline capability, every clause.
// ===========================================================================

#[test]
fn sql_function_in_a_bare_select_returns_the_computed_value() {
    let db = db();
    setup(&db);
    assert_eq!(as_i64(&scalar(&db, "SELECT fn_dbl(21)")), 42);
}

#[test]
fn sql_function_in_a_projection_alongside_columns() {
    let db = db();
    setup(&db);

    let rows = db
        .query("SELECT id, fn_dbl(id) FROM fn_posts ORDER BY id", &[])
        .unwrap();
    assert_eq!(rows.len(), 2, "one row per post");
    assert_eq!(as_i64(&rows[0].values[0]), 1);
    assert_eq!(as_i64(&rows[0].values[1]), 2);
    assert_eq!(as_i64(&rows[1].values[0]), 2);
    assert_eq!(as_i64(&rows[1].values[1]), 4);
}

#[test]
fn sql_function_in_a_where_clause_filters() {
    let db = db();
    setup(&db);

    let rows = db.query("SELECT id FROM fn_posts WHERE fn_dbl(id) = 2", &[]).unwrap();
    assert_eq!(rows.len(), 1, "only id=1 doubles to 2");
    assert_eq!(as_i64(&rows[0].values[0]), 1);
}

#[test]
fn public_schema_qualification_resolves() {
    let db = db();
    setup(&db);
    // The registry is schema-less, so exactly one qualifier is accepted: the
    // schema every UDF is in. See `tests/udf_remaining_gaps_tests.rs` for the
    // pin that any OTHER qualifier still errors.
    assert_eq!(as_i64(&scalar(&db, "SELECT public.fn_dbl(21)")), 42);
}

#[test]
fn function_is_case_insensitive_by_name() {
    let db = db();
    setup(&db);
    // `FunctionRegistry` keys on the lowercase name.
    assert_eq!(as_i64(&scalar(&db, "SELECT FN_DBL(4)")), 8);
}

// ===========================================================================
// Both executor families — registration AND invocation.
// ===========================================================================

#[test]
fn params_family_create_function_actually_registers_it() {
    // REGRESSION PIN. `execute_params` had no `CreateFunction` arm: the plan fell
    // to the catch-all, which built a bare `sql::Executor` whose arm returned a
    // status message. The statement reported success with rows_affected = 1 and
    // registered NOTHING — and the PostgreSQL extended query protocol routes
    // every non-SELECT statement through exactly this path.
    let db = db();
    db.execute_params(
        "CREATE FUNCTION fn_p_dbl(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql",
        &[],
    )
    .expect("params-family CREATE FUNCTION must succeed");

    assert!(
        db.function_registry.function_exists("fn_p_dbl"),
        "the params family must REGISTER the function, not just report success"
    );
    assert_eq!(as_i64(&scalar(&db, "SELECT fn_p_dbl(3)")), 6);
}

#[test]
fn params_family_invocation_with_a_bound_argument() {
    let db = db();
    setup(&db);

    let rows = db.query_params("SELECT fn_dbl($1)", &[Value::Int4(21)]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(as_i64(&rows[0].values[0]), 42);
}

#[test]
fn params_family_drop_function_actually_drops_it() {
    let db = db();
    db.execute("CREATE FUNCTION fn_pdrop(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
        .unwrap();
    assert!(db.function_registry.function_exists("fn_pdrop"), "precondition");

    db.execute_params("DROP FUNCTION fn_pdrop", &[])
        .expect("params-family DROP FUNCTION must succeed");
    assert!(
        !db.function_registry.function_exists("fn_pdrop"),
        "the params family must actually drop it"
    );
    assert!(err_of(&db, "SELECT fn_pdrop(1)").contains("Unknown scalar function"));
}

#[test]
fn params_family_create_procedure_runs_on_both_families() {
    // The user-visible half of the params-family hole: procedures DO execute, so
    // a fake "created" on the extended protocol was a functional regression.
    // PostgreSQL's `CREATE PROCEDURE … LANGUAGE … AS $$…$$` is not sqlparser
    // grammar either, so this also pins the shared pre-parse.
    let db = db();
    db.execute("CREATE TABLE fn_audit (id INTEGER, note TEXT)").unwrap();

    db.execute_params(
        "CREATE PROCEDURE fn_p_log(p_id INTEGER) LANGUAGE sql AS $$INSERT INTO fn_audit VALUES ($p_id, 'x')$$",
        &[],
    )
    .expect("params-family CREATE PROCEDURE must succeed");
    assert!(
        db.function_registry.procedure_exists("fn_p_log"),
        "the params family must REGISTER the procedure"
    );

    db.execute("CALL fn_p_log(1)").expect("text-family CALL");
    db.execute_params("CALL fn_p_log(2)", &[]).expect("params-family CALL");
    assert_eq!(rows_in(&db, "fn_audit"), 2, "both CALLs must have run the body");
}

// ===========================================================================
// PL/pgSQL bodies.
// ===========================================================================

#[test]
fn plpgsql_function_returns_a_computed_expression() {
    let db = db();
    db.execute(
        "CREATE FUNCTION fn_pg_dbl(x INTEGER) RETURNS INTEGER AS $$ BEGIN RETURN $1 * 2; END $$ LANGUAGE plpgsql",
    )
    .expect("CREATE FUNCTION ... LANGUAGE plpgsql must be accepted");

    // The RETURN expression is evaluated as SQL after `$`-interpolation. It used
    // to be handed to the evaluator as a STRING LITERAL of its own source text
    // (`ProceduralParser::parse_expression` never built an expression tree), so
    // this would have returned the text "$1 * 2".
    assert_eq!(as_i64(&scalar(&db, "SELECT fn_pg_dbl(21)")), 42);
}

#[test]
fn plpgsql_function_with_declare_and_select_into() {
    let db = db();
    setup(&db);

    db.execute(
        "CREATE FUNCTION fn_post_count(uid INTEGER) RETURNS INTEGER AS $$
         DECLARE cnt INTEGER;
         BEGIN
             SELECT COUNT(*) INTO cnt FROM fn_posts WHERE author_id = $uid;
             RETURN $cnt;
         END;
         $$ LANGUAGE plpgsql",
    )
    .expect("CREATE FUNCTION ... LANGUAGE plpgsql must be accepted");

    assert_eq!(as_i64(&scalar(&db, "SELECT fn_post_count(7)")), 2);
    assert_eq!(as_i64(&scalar(&db, "SELECT fn_post_count(999)")), 0);
}

#[test]
fn plpgsql_function_body_can_write_and_the_rows_are_visible() {
    let db = db();
    db.execute("CREATE TABLE fn_audit (id INTEGER, note TEXT)").unwrap();
    db.execute(
        "CREATE FUNCTION fn_log(n INTEGER) RETURNS INTEGER AS $$ \
         BEGIN INSERT INTO fn_audit VALUES ($1, 'logged'); RETURN $1; END $$ LANGUAGE plpgsql",
    )
    .unwrap();

    assert_eq!(as_i64(&scalar(&db, "SELECT fn_log(5)")), 5);
    let rows = db.query("SELECT id, note FROM fn_audit", &[]).unwrap();
    assert_eq!(rows.len(), 1, "the body's INSERT must have run");
    assert_eq!(as_i64(&rows[0].values[0]), 5);
}

#[test]
fn plpgsql_control_flow_in_a_function_is_refused_loudly() {
    // NOT a silent wrong answer. `ProceduralParser::parse_expression` captures
    // expression TEXT instead of parsing it, so an `IF <cond> THEN` condition
    // evaluates to a non-boolean string and the ELSE branch always runs. The
    // function path refuses rather than manufacturing that wrong answer.
    let db = db();
    db.execute(
        "CREATE FUNCTION fn_branchy(x INTEGER) RETURNS INTEGER AS $$ \
         BEGIN IF $1 > 0 THEN RETURN 1; ELSE RETURN 2; END IF; END $$ LANGUAGE plpgsql",
    )
    .expect("CREATE FUNCTION must still be accepted — the refusal is at call time");

    let err = err_of(&db, "SELECT fn_branchy(1)");
    assert!(err.contains("was NOT executed"), "must be loud, got: {err}");
    assert!(err.contains("IF statement"), "must name the construct, got: {err}");
}

// ===========================================================================
// Negative matrix.
// ===========================================================================

#[test]
fn null_argument_propagates_to_the_body() {
    let db = db();
    setup(&db);
    // Not STRICT: the NULL is substituted and the body decides. `NULL * 2` is NULL.
    assert_eq!(scalar(&db, "SELECT fn_dbl(NULL)"), Value::Null);
}

#[test]
fn too_few_arguments_errors() {
    let db = db();
    setup(&db);
    let err = err_of(&db, "SELECT fn_dbl()");
    assert!(err.contains("requires at least 1 arguments"), "got: {err}");
}

#[test]
fn too_many_arguments_errors() {
    let db = db();
    setup(&db);
    let err = err_of(&db, "SELECT fn_dbl(1, 2)");
    assert!(err.contains("accepts at most 1 arguments"), "got: {err}");
}

#[test]
fn an_unregistered_name_is_still_an_unknown_scalar_function() {
    let db = db();
    setup(&db);
    let err = err_of(&db, "SELECT fn_never_defined(1)");
    assert!(err.contains("Unknown scalar function"), "got: {err}");
    assert!(err.contains("fn_never_defined"), "the error must name it: {err}");
}

#[test]
fn unsupported_language_errors() {
    use heliosdb_nano::sql::logical_plan::{FunctionParam, ParamMode};
    use heliosdb_nano::sql::StoredFunction;
    use heliosdb_nano::DataType;

    // Registered directly: `LANGUAGE plperl` is not something this build's
    // grammar is guaranteed to accept, and the assertion under test is the
    // INTERPRETER's language dispatch, not the parser's.
    let db = db();
    db.function_registry
        .register_function(StoredFunction {
            name: "fn_perl".to_string(),
            or_replace: false,
            params: vec![FunctionParam {
                name: "x".to_string(),
                data_type: DataType::Int4,
                mode: ParamMode::In,
                default: None,
            }],
            return_type: Some(DataType::Int4),
            body: "1".to_string(),
            language: "plperl".to_string(),
            volatility: None,
            created_at: 0,
        })
        .unwrap();

    let err = err_of(&db, "SELECT fn_perl(1)");
    assert!(err.contains("Unsupported function language"), "got: {err}");
    assert!(err.contains("plperl"), "the error must name the language: {err}");
}

#[test]
fn a_sql_body_returning_no_rows_yields_null() {
    let db = db();
    setup(&db);
    db.execute(
        "CREATE FUNCTION fn_missing_post() RETURNS INTEGER AS $$ SELECT id FROM fn_posts WHERE id = -1 $$ LANGUAGE sql",
    )
    .unwrap();

    assert_eq!(scalar(&db, "SELECT fn_missing_post()"), Value::Null);
}

#[test]
fn a_body_that_fails_surfaces_the_body_error() {
    let db = db();
    setup(&db);
    db.execute("CREATE FUNCTION fn_broken() RETURNS INTEGER AS $$ SELECT * FROM no_such_table $$ LANGUAGE sql")
        .unwrap();

    let err = err_of(&db, "SELECT fn_broken()");
    assert!(err.contains("no_such_table"), "the body's failure must surface: {err}");
}

#[test]
fn dropping_a_function_makes_it_uncallable_again() {
    let db = db();
    db.execute("CREATE FUNCTION fn_droppable(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
        .unwrap();
    assert_eq!(as_i64(&scalar(&db, "SELECT fn_droppable(1)")), 2);

    db.execute("DROP FUNCTION fn_droppable").unwrap();
    let err = err_of(&db, "SELECT fn_droppable(1)");
    assert!(err.contains("Unknown scalar function"), "got: {err}");
}

#[test]
fn drop_function_if_exists_on_a_missing_name_is_quiet() {
    let db = db();
    // A function that MUST survive: `IF EXISTS` on a missing name must be a
    // no-op, not a "drop something". The bare `.expect()` below is a real
    // assertion (it panics on Err), but on its own it only pinned "returns
    // Ok" — it could not have caught a no-op that also removed an unrelated
    // definition, which is the failure worth pinning here.
    db.execute("CREATE FUNCTION fn_quiet_survivor(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
        .unwrap();

    let affected = db
        .execute("DROP FUNCTION IF EXISTS fn_never_created")
        .expect("IF EXISTS must be a no-op, not an error");
    assert_eq!(affected, 0, "a no-op DROP must report zero affected rows");

    assert_eq!(
        as_i64(&scalar(&db, "SELECT fn_quiet_survivor(21)")),
        42,
        "an unrelated function must still be callable after a no-op DROP … IF EXISTS"
    );

    // Both executor families: the params family is what every real driver uses.
    db.execute_params("DROP FUNCTION IF EXISTS fn_never_created", &[])
        .expect("params family must treat IF EXISTS as a no-op too");
    assert_eq!(as_i64(&scalar(&db, "SELECT fn_quiet_survivor(21)")), 42);
}

/// RESULT-CACHE STALENESS. `SELECT f(…)` is cached by SQL TEXT, and
/// `query_is_non_deterministic` knows only the built-in volatile names, so a
/// UDF-bearing SELECT is treated as deterministic and cacheable. Redefining the
/// function must clear that cache — which is why
/// `EmbeddedDatabase::plan_invalidates_sql_caches` lists CreateFunction /
/// CreateProcedure / DropFunction / DropProcedure.
///
/// The IDENTICAL query text is used before and after on purpose: adding an
/// `AS alias` to the second read keys a different cache entry and hides the bug.
#[test]
fn create_or_replace_invalidates_the_result_cache() {
    let db = db();
    db.execute("CREATE FUNCTION fn_cache_repl(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
        .unwrap();
    let sql = "SELECT fn_cache_repl(10)";
    assert_eq!(as_i64(&scalar(&db, sql)), 20);
    // Read it twice so the result really is cached before the redefinition.
    assert_eq!(as_i64(&scalar(&db, sql)), 20);

    db.execute(
        "CREATE OR REPLACE FUNCTION fn_cache_repl(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 3 $$ LANGUAGE sql",
    )
    .expect("OR REPLACE must be accepted");

    assert_eq!(
        as_i64(&scalar(&db, sql)),
        30,
        "the SAME query text must not keep serving the pre-replace cached result"
    );
}

/// Same class, worse spelling: after `DROP FUNCTION` the identical query text
/// must ERROR rather than keep answering out of the result cache.
#[test]
fn drop_function_invalidates_the_result_cache() {
    let db = db();
    db.execute("CREATE FUNCTION fn_cache_drop(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
        .unwrap();
    let sql = "SELECT fn_cache_drop(10)";
    assert_eq!(as_i64(&scalar(&db, sql)), 20);
    assert_eq!(as_i64(&scalar(&db, sql)), 20);

    db.execute("DROP FUNCTION fn_cache_drop")
        .expect("dropping an existing function must succeed");

    let err = err_of(&db, sql);
    assert!(
        err.contains("fn_cache_drop"),
        "a dropped function must stop answering and name itself, got: {err}"
    );
}

#[test]
fn drop_function_without_if_exists_on_a_missing_name_errors() {
    let db = db();
    let err = db
        .execute("DROP FUNCTION fn_never_created")
        .expect_err("dropping a missing function must fail")
        .to_string();
    assert!(err.contains("does not exist"), "got: {err}");
}

#[test]
fn create_or_replace_changes_the_result() {
    let db = db();
    db.execute("CREATE FUNCTION fn_repl(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
        .unwrap();
    assert_eq!(as_i64(&scalar(&db, "SELECT fn_repl(10)")), 20);

    db.execute("CREATE OR REPLACE FUNCTION fn_repl(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 3 $$ LANGUAGE sql")
        .expect("OR REPLACE must be accepted");

    assert_eq!(
        as_i64(&scalar(&db, "SELECT fn_repl(10) AS tripled")),
        30,
        "the replaced body must take effect"
    );
}

#[test]
fn duplicate_create_without_or_replace_is_rejected() {
    let db = db();
    setup(&db);
    let err = db
        .execute("CREATE FUNCTION fn_dbl(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
        .expect_err("re-creating the same function must fail")
        .to_string();
    assert!(err.contains("already exists"), "got: {err}");
}

// ===========================================================================
// Recursion depth — the tunable, not a hardcoded constant.
// ===========================================================================

fn db_with_udf_depth(depth: u32) -> EmbeddedDatabase {
    let mut config = Config::in_memory();
    config.session.udf_max_call_depth = depth;
    EmbeddedDatabase::with_config(config).expect("configured in-memory db")
}

#[test]
fn a_self_recursive_function_hits_the_depth_limit_instead_of_the_stack() {
    let db = db_with_udf_depth(4);
    db.execute("CREATE FUNCTION fn_rec(x INTEGER) RETURNS INTEGER AS $$ SELECT fn_rec($1) $$ LANGUAGE sql")
        .unwrap();

    let err = err_of(&db, "SELECT fn_rec(1)");
    assert!(err.contains("depth limit (4)"), "must name the configured limit: {err}");
    assert!(err.contains("fn_rec"), "must name the function: {err}");
}

#[test]
fn the_depth_limit_is_the_configured_value_not_a_constant() {
    let db = db_with_udf_depth(2);
    db.execute("CREATE FUNCTION fn_rec2(x INTEGER) RETURNS INTEGER AS $$ SELECT fn_rec2($1) $$ LANGUAGE sql")
        .unwrap();

    let err = err_of(&db, "SELECT fn_rec2(1)");
    assert!(
        err.contains("depth limit (2)"),
        "the config value must be honoured: {err}"
    );
}

#[test]
fn nesting_below_the_limit_still_works() {
    let db = db_with_udf_depth(4);
    db.execute("CREATE FUNCTION fn_inner(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
        .unwrap();
    db.execute("CREATE FUNCTION fn_outer(x INTEGER) RETURNS INTEGER AS $$ SELECT fn_inner($1) + 1 $$ LANGUAGE sql")
        .unwrap();

    assert_eq!(as_i64(&scalar(&db, "SELECT fn_outer(10)")), 21);
}

#[test]
fn the_default_udf_call_depth_is_32() {
    // Interface-coverage gate: the limit is a `[session]` config parameter with a
    // documented default, not a magic number in the evaluator.
    assert_eq!(Config::default().session.udf_max_call_depth, 32);
    let mut bad = Config::default();
    bad.session.udf_max_call_depth = 0;
    assert!(
        bad.session.validate().is_err(),
        "0 would refuse every call and must be rejected by validation"
    );
}

// ===========================================================================
// Durability across a restart.
// ===========================================================================

#[test]
fn functions_and_procedures_survive_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("udf_restart");

    {
        let db = EmbeddedDatabase::new(&path).expect("open");
        db.execute("CREATE TABLE r_audit (id INTEGER, note TEXT)").unwrap();
        db.execute("CREATE FUNCTION r_dbl(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
            .unwrap();
        db.execute("CREATE PROCEDURE r_log(p_id INTEGER) LANGUAGE sql AS $$INSERT INTO r_audit VALUES ($p_id, 'x')$$")
            .unwrap();
        assert_eq!(as_i64(&scalar(&db, "SELECT r_dbl(4)")), 8);
    }

    {
        let db = EmbeddedDatabase::new(&path).expect("reopen");
        assert!(
            db.function_registry.function_exists("r_dbl"),
            "the function must be reloaded from meta:function:"
        );
        assert!(
            db.function_registry.procedure_exists("r_log"),
            "the procedure must be reloaded from meta:procedure:"
        );
        assert_eq!(as_i64(&scalar(&db, "SELECT r_dbl(4)")), 8, "and be callable");
        db.execute("CALL r_log(1)").expect("the reloaded procedure must run");
        assert_eq!(rows_in(&db, "r_audit"), 1);
    }
}

#[test]
fn a_dropped_function_stays_dropped_across_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("udf_drop_restart");

    {
        let db = EmbeddedDatabase::new(&path).expect("open");
        db.execute("CREATE FUNCTION d_dbl(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
            .unwrap();
        db.execute("DROP FUNCTION d_dbl").unwrap();
    }

    {
        let db = EmbeddedDatabase::new(&path).expect("reopen");
        assert!(
            !db.function_registry.function_exists("d_dbl"),
            "the durable record must have been removed by DROP, not resurrected"
        );
        assert!(err_of(&db, "SELECT d_dbl(1)").contains("Unknown scalar function"));
    }
}

#[test]
fn the_data_dir_reopens_after_drop_so_no_arc_cycle_leaked() {
    // The UDF bridge holds a `clone_for_trigger()` handle; if that clone carried
    // the bridge back, the `Arc<UdfBridge>` strong count could never reach zero,
    // `StorageEngine::Drop` would never run, and the data dir would stay locked.
    // Reopening the SAME directory three times proves Drop ran each time.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("udf_no_leak");

    for round in 0..3 {
        let db = EmbeddedDatabase::new(&path).unwrap_or_else(|e| panic!("round {round} open failed: {e}"));
        assert!(db.has_udf_bridge(), "the opened handle must own the bridge");
        drop(db);
    }
}

// ===========================================================================
// Transaction guard, and the execution paths that must NOT pretend.
// ===========================================================================

#[test]
fn a_udf_inside_a_global_transaction_is_refused_not_hung() {
    // `query()` holds the NON-reentrant `current_transaction` mutex across
    // execution while a global (embedded/REPL) transaction is open, and the
    // function body re-enters `query()`. Refusing loudly is the contract; the
    // test passing at all is the proof it does not deadlock.
    let db = db();
    // Its OWN name: the guard is a property of THIS database's transaction
    // state, and resolution is process-global.
    db.execute("CREATE FUNCTION fn_txn_dbl(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql")
        .unwrap();

    db.execute("BEGIN").expect("BEGIN");
    let err = err_of(&db, "SELECT fn_txn_dbl(1)");
    db.execute("ROLLBACK").expect("ROLLBACK");

    assert!(err.contains("was NOT executed"), "must be loud, got: {err}");
    assert!(
        err.contains("explicit transaction"),
        "must say why it was refused: {err}"
    );

    // ...and the handle is still usable afterwards.
    assert_eq!(as_i64(&scalar(&db, "SELECT fn_txn_dbl(3)")), 6);
}

#[test]
fn the_query_route_refuses_to_fake_create_function() {
    // `query()` hands its plan to the SELECT-only physical executor, which holds
    // no `FunctionRegistry`. That arm used to answer `StatusMessageOperator`
    // "Function 'x' created" — a success that registered nothing.
    let db = db();
    let err = err_of(
        &db,
        "CREATE FUNCTION q_dbl(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 * 2 $$ LANGUAGE sql",
    );
    assert!(err.contains("was NOT created"), "must be loud, got: {err}");
    assert!(
        !db.function_registry.function_exists("q_dbl"),
        "and nothing may have been registered"
    );
}

// ===========================================================================
// PROCEDURES — the forms that already worked and must keep working.
// Moved verbatim in intent from the deleted `function_unimplemented_tests.rs`;
// a failure here is a REGRESSION, not a limitation that started working.
// ===========================================================================

fn proc_setup(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE fn_audit (id INTEGER, note TEXT)").unwrap();
}

fn only_audit_row(db: &EmbeddedDatabase) -> (i64, String) {
    let rows = db.query("SELECT id, note FROM fn_audit", &[]).expect("audit read");
    assert_eq!(rows.len(), 1, "expected exactly one audit row, got {}", rows.len());
    match (rows[0].values.first(), rows[0].values.get(1)) {
        (Some(id), Some(Value::String(note))) => (as_i64(id), note.clone()),
        other => panic!("unexpected audit row shape: {other:?}"),
    }
}

#[test]
fn language_sql_procedure_binds_parameters_by_name() {
    let db = db();
    proc_setup(&db);
    db.execute(
        "CREATE PROCEDURE fn_p_named(p_id INTEGER, p_op TEXT) LANGUAGE sql \
         AS $$INSERT INTO fn_audit VALUES ($p_id, $p_op)$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL fn_p_named(42, 'hello')").expect("CALL runs the body");
    assert_eq!(only_audit_row(&db), (42, "hello".to_string()));
}

#[test]
fn language_sql_procedure_binds_parameters_positionally() {
    let db = db();
    proc_setup(&db);
    db.execute(
        "CREATE PROCEDURE fn_p_pos(p_id INTEGER, p_op TEXT) LANGUAGE sql \
         AS $$INSERT INTO fn_audit VALUES ($1, $2)$$",
    )
    .expect("CREATE PROCEDURE must be accepted");

    db.execute("CALL fn_p_pos(7, 'seven')").expect("CALL runs the body");
    assert_eq!(only_audit_row(&db), (7, "seven".to_string()));
}

#[test]
fn language_plpgsql_procedure_binds_named_and_positional_parameters() {
    let db = db();
    proc_setup(&db);
    db.execute(
        "CREATE PROCEDURE fn_p_pg(p_id INTEGER) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO fn_audit VALUES ($p_id, 'x'); END$$",
    )
    .expect("CREATE PROCEDURE must be accepted");
    db.execute("CALL fn_p_pg(7)").expect("named `$p_id` must substitute");
    assert_eq!(only_audit_row(&db), (7, "x".to_string()));

    db.execute("DELETE FROM fn_audit").unwrap();
    db.execute(
        "CREATE PROCEDURE fn_p_pg2(p_id INTEGER) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO fn_audit VALUES ($1, 'y'); END$$",
    )
    .unwrap();
    db.execute("CALL fn_p_pg2(9)").expect("positional `$1` must substitute");
    assert_eq!(only_audit_row(&db), (9, "y".to_string()));
}

#[test]
fn procedure_with_no_parameters_executes() {
    let db = db();
    proc_setup(&db);
    db.execute("CREATE PROCEDURE fn_p_const() LANGUAGE sql AS $$INSERT INTO fn_audit VALUES (0, 'const')$$")
        .unwrap();
    db.execute("CALL fn_p_const()").expect("CALL runs the body");
    assert_eq!(only_audit_row(&db), (0, "const".to_string()));
}

#[test]
fn a_bare_parameter_name_still_fails_the_sigil_rule_in_both_languages() {
    for (name, create) in [
        (
            "fn_p_bare_sql",
            "CREATE PROCEDURE fn_p_bare_sql(n INTEGER) LANGUAGE sql AS $$INSERT INTO fn_audit VALUES (n, 'x')$$",
        ),
        (
            "fn_p_bare_pg",
            "CREATE PROCEDURE fn_p_bare_pg(n INTEGER) LANGUAGE plpgsql \
             AS $$BEGIN INSERT INTO fn_audit VALUES (n, 'x'); END$$",
        ),
    ] {
        let db = db();
        proc_setup(&db);
        db.execute(create).expect("CREATE PROCEDURE must be accepted");

        let err = db
            .execute(&format!("CALL {name}(7)"))
            .expect_err("a bare parameter name must not resolve")
            .to_string();
        assert!(
            err.contains("not found in schema"),
            "expected a column-resolution error for {name}, got: {err}"
        );
        assert_eq!(rows_in(&db, "fn_audit"), 0, "nothing may have been written");
    }
}
