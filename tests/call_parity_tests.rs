//! `CALL <procedure>` parity between the two DML executor families.
//!
//! HeliosDB Nano has two parallel DML executors, and until this suite existed only one of them
//! could run a procedure:
//!
//!   * "text family"   — `db.execute()`        → `execute_in_transaction_inner`
//!                        (psql simple-query, the whole MySQL wire, the REPL, embedded)
//!   * "params family" — `db.execute_params()` → `execute_params_inner`
//!                        → `execute_plan_with_params_inner`
//!                        (PG extended protocol — psycopg server-side bind, JDBC, sqlx,
//!                         Drizzle, node-postgres — plus every REST/BaaS write, and
//!                         trigger bodies via `execute_plan_internal`)
//!
//! **The defect this suite pins the fix for.** `execute_plan_with_params_inner` had no
//! `LogicalPlan::Call` arm. `CALL` fell through to its catch-all, which builds a
//! `sql::Executor` — and an `Executor` holds no `FunctionRegistry` handle, so its `Call` arm
//! could only return `StatusMessageOperator::new("Procedure 'p' called with N arguments")`.
//! Measured before the fix:
//!
//! | statement | text family | params family |
//! |---|---|---|
//! | `CALL p0()` (no args) | Ok, row inserted | **Ok(1), NO row inserted** |
//! | `CALL p1($1)` | n/a | **Ok(1), NO row inserted** |
//! | `CALL nonexistent_proc()` | `Err: Procedure … does not exist` | **Ok(1)** |
//!
//! It was not merely a no-op: it returned `rows_affected = 1`, actively claiming work was done,
//! and it never checked that the procedure existed. That is the population the v4.10.1 / v4.10.2
//! / v4.11.0 docs were telling to "use a `CREATE PROCEDURE` invoked with `CALL`" in place of a
//! trigger.
//!
//! **The fix.** One shared implementation, `EmbeddedDatabase::execute_call_plan` (`src/lib.rs`),
//! called from BOTH families' `Call` arms. The `Executor` stub
//! (`src/sql/executor/mod.rs`) now returns an error instead of a success status, so any path that
//! still reaches it fails loudly rather than silently claiming to have run a body.
//!
//! **`rows_affected` contract: 0, in both families.** `CALL` modifies no rows of its own;
//! PostgreSQL's command tag is a bare `CALL` with no count. The text family always returned 0;
//! the params family's 1 was the stub's own status *message* counted as a row. Pinned below.
//!
//! HOW TO MAINTAIN THIS FILE. Every test asserts unconditionally — never introduce an `is_ok()`
//! guard, and never assert on a row count read back through `SELECT COUNT(*)` (that returns one
//! row whether the count is 0 or 10,000). Two tests at the bottom pin a KNOWN GAP rather than
//! desired behaviour; they say so, in the style of `tests/function_unimplemented_tests.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

/// `call_audit` plus the three procedures every test below invokes.
fn setup(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE call_audit (id INTEGER, note TEXT)").unwrap();
    db.execute("CREATE PROCEDURE p_zero() LANGUAGE sql AS $$INSERT INTO call_audit VALUES (0, 'zero')$$")
        .expect("CREATE PROCEDURE p_zero");
    db.execute(
        "CREATE PROCEDURE p_one(p_id INTEGER) LANGUAGE sql \
         AS $$INSERT INTO call_audit VALUES ($p_id, 'one')$$",
    )
    .expect("CREATE PROCEDURE p_one");
    db.execute(
        "CREATE PROCEDURE p_pg(p_id INTEGER) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO call_audit VALUES ($p_id, 'pg'); END$$",
    )
    .expect("CREATE PROCEDURE p_pg");
}

/// Every `(id, note)` physically in `call_audit`. Deliberately not `SELECT COUNT(*)`.
fn audit(db: &EmbeddedDatabase) -> Vec<(i32, String)> {
    db.query("SELECT id, note FROM call_audit", &[])
        .expect("audit read")
        .iter()
        .map(|row| match (row.values.first(), row.values.get(1)) {
            (Some(Value::Int4(id)), Some(Value::String(note))) => (*id, note.clone()),
            other => panic!("unexpected call_audit row shape: {other:?}"),
        })
        .collect()
}

/// The single row `call_audit` must contain.
fn only_row(db: &EmbeddedDatabase) -> (i32, String) {
    let rows = audit(db);
    assert_eq!(rows.len(), 1, "expected exactly one audit row, got {rows:?}");
    rows.into_iter().next().expect("one row")
}

// ===========================================================================
// A zero-parameter procedure's BODY actually runs — in both families.
//
// The params half is the headline regression guard: it returned Ok(1) and wrote nothing.
// ===========================================================================

#[test]
fn zero_arg_procedure_body_runs_in_the_text_family() {
    let db = db();
    setup(&db);

    let affected = db.execute("CALL p_zero()").expect("CALL must execute the body");

    assert_eq!(affected, 0, "CALL modifies no rows of its own");
    assert_eq!(only_row(&db), (0, "zero".to_string()), "the body must have inserted");
}

#[test]
fn zero_arg_procedure_body_runs_in_the_params_family() {
    let db = db();
    setup(&db);

    let affected = db
        .execute_params("CALL p_zero()", &[])
        .expect("CALL must execute the body on the params family too");

    assert_eq!(affected, 0, "CALL modifies no rows of its own");
    assert_eq!(
        only_row(&db),
        (0, "zero".to_string()),
        "the params family used to return Ok(1) and write NOTHING"
    );
}

// ===========================================================================
// A parameterised procedure's body runs AND binds its argument — in both families.
// ===========================================================================

#[test]
fn literal_argument_binds_in_the_text_family() {
    let db = db();
    setup(&db);

    let affected = db.execute("CALL p_one(7)").expect("CALL must execute the body");

    assert_eq!(affected, 0);
    assert_eq!(
        only_row(&db),
        (7, "one".to_string()),
        "the argument must reach the body"
    );
}

#[test]
fn literal_argument_binds_in_the_params_family() {
    let db = db();
    setup(&db);

    let affected = db
        .execute_params("CALL p_one(7)", &[])
        .expect("CALL must execute the body on the params family too");

    assert_eq!(affected, 0);
    assert_eq!(
        only_row(&db),
        (7, "one".to_string()),
        "the argument must reach the body"
    );
}

#[test]
fn bound_parameter_argument_binds_in_the_params_family() {
    // `CALL p($1)` with a server-side bind — the shape every extended-protocol driver
    // actually emits, and the one that silently did nothing.
    let db = db();
    setup(&db);

    let affected = db
        .execute_params("CALL p_one($1)", &[Value::Int4(41)])
        .expect("a bound CALL argument must execute the body");

    assert_eq!(affected, 0);
    assert_eq!(
        only_row(&db),
        (41, "one".to_string()),
        "the BOUND value must reach the body, not a placeholder"
    );
}

#[test]
fn a_bound_placeholder_without_a_value_fails_in_the_text_family() {
    // The one legitimate asymmetry: the text family carries no bind values, so `$1` in a
    // CALL argument list has nothing to resolve against. It must ERROR, not write a row.
    let db = db();
    setup(&db);

    let res = db.execute("CALL p_one($1)");
    assert!(
        res.is_err(),
        "an unbound placeholder must not resolve — it must never silently succeed, got: {res:?}"
    );
    assert!(audit(&db).is_empty(), "nothing may have been written");
}

#[test]
fn plpgsql_procedure_body_runs_in_both_families() {
    let db_text = db();
    setup(&db_text);
    db_text.execute("CALL p_pg(3)").expect("plpgsql CALL, text family");
    assert_eq!(only_row(&db_text), (3, "pg".to_string()));

    let db_params = db();
    setup(&db_params);
    db_params
        .execute_params("CALL p_pg($1)", &[Value::Int4(3)])
        .expect("plpgsql CALL, params family");
    assert_eq!(only_row(&db_params), (3, "pg".to_string()));
}

// ===========================================================================
// A missing procedure ERRORS in both families, and the error names it.
//
// This is the sharpest half of the defect: the params family reported success for a
// procedure that did not exist.
// ===========================================================================

#[test]
fn calling_a_missing_procedure_errors_in_the_text_family() {
    let db = db();
    setup(&db);

    let err = db
        .execute("CALL nonexistent_proc()")
        .expect_err("a missing procedure must fail")
        .to_string();
    assert!(
        err.contains("nonexistent_proc"),
        "the error must name the procedure, got: {err}"
    );
    assert!(err.contains("does not exist"), "expected 'does not exist', got: {err}");
}

#[test]
fn calling_a_missing_procedure_errors_in_the_params_family() {
    let db = db();
    setup(&db);

    let err = db
        .execute_params("CALL nonexistent_proc()", &[])
        .expect_err("a missing procedure must fail on the params family too — it used to return Ok(1)")
        .to_string();
    assert!(
        err.contains("nonexistent_proc"),
        "the error must name the procedure, got: {err}"
    );
    assert!(err.contains("does not exist"), "expected 'does not exist', got: {err}");
}

#[test]
fn calling_a_missing_procedure_with_bound_arguments_errors_too() {
    let db = db();
    setup(&db);

    let err = db
        .execute_params("CALL nonexistent_proc($1)", &[Value::Int4(1)])
        .expect_err("a missing procedure must fail even with bound arguments")
        .to_string();
    assert!(
        err.contains("nonexistent_proc"),
        "the error must name the procedure, got: {err}"
    );
}

// ===========================================================================
// The `rows_affected` contract, pinned on its own: 0 in BOTH families.
// ===========================================================================

#[test]
fn call_reports_zero_rows_affected_in_both_families() {
    let db = db();
    setup(&db);

    let text = db.execute("CALL p_zero()").expect("text family CALL");
    let params = db.execute_params("CALL p_zero()", &[]).expect("params family CALL");

    assert_eq!(text, 0, "text family: CALL affects no rows of its own");
    assert_eq!(
        params, 0,
        "params family: CALL affects no rows of its own. This used to be 1 — the stub's own \
         status MESSAGE counted as an affected row, while the body never ran."
    );
    assert_eq!(text, params, "the two families must agree");

    // Both calls did run, so both bodies wrote: two rows, not one.
    assert_eq!(audit(&db).len(), 2, "both CALLs must have executed their body");
}

// ===========================================================================
// The `Executor` stub is no longer a silent success.
// ===========================================================================

#[test]
fn the_query_path_refuses_call_instead_of_faking_a_status_row() {
    // `db.query("CALL …")` plans the statement and hands it straight to `sql::Executor`,
    // which has no `FunctionRegistry` and therefore cannot run a body. It used to answer
    // with a one-row status message ("Procedure 'p_zero' called with 0 arguments"), which
    // reads exactly like success. It must fail instead.
    let db = db();
    setup(&db);

    let err = db
        .query("CALL p_zero()", &[])
        .expect_err("the query path cannot run a procedure body and must say so")
        .to_string();
    assert!(err.contains("p_zero"), "the error must name the procedure, got: {err}");
    assert!(
        err.contains("NOT executed"),
        "the error must make clear the body did NOT run, got: {err}"
    );
    assert!(audit(&db).is_empty(), "nothing may have been written");
}

// ===========================================================================
// KNOWN GAP — pinned deliberately. This is NOT the desired end state.
//
// A procedure body is run by re-entering `execute()`/`query()` on a `clone_for_trigger()`
// handle (the mechanism the text family has always used, unchanged by this fix). Both of
// those re-take the global `current_transaction` mutex when a global transaction is open,
// and `parking_lot::Mutex` is not reentrant — so a `CALL` issued while `execute()` already
// holds that guard would HANG the thread. It did hang, before this fix, on the text family.
//
// `GLOBAL_TXN_LOCK_HELD` (src/lib.rs) marks that window and `execute_call_plan` refuses
// loudly instead. A loud error beats a hang; it is still an error where PostgreSQL would
// have run the procedure.
//
// SCOPE: only the embedded API and the REPL can reach this. A `BEGIN` over the PG or MySQL
// wire creates a per-SESSION transaction (`handle_transaction_control_for_session`), which
// never populates the global slot, so `CALL` inside a wire transaction is unaffected.
//
// The real fix is to run body statements against the caller's transaction rather than
// through a fresh `execute()` — filed as ROADMAP_V5 §2.11, deliberately out of scope here
// (this change is about parity between the families). When that lands, DELETE these two
// tests and assert the body runs and joins the caller's transaction in BOTH families.
// ===========================================================================

#[test]
fn known_gap_call_inside_a_global_text_transaction_is_refused() {
    let db = db();
    setup(&db);

    db.execute("BEGIN").expect("BEGIN");
    let err = db
        .execute("CALL p_zero()")
        .expect_err("KNOWN GAP: refused rather than run — see the section comment")
        .to_string();
    db.execute("ROLLBACK").expect("ROLLBACK");

    assert!(err.contains("p_zero"), "the error must name the procedure, got: {err}");
    assert!(
        err.contains("NOT executed"),
        "the error must make clear the body did NOT run, got: {err}"
    );
    assert!(audit(&db).is_empty(), "the body must not have run");
}

#[test]
fn known_gap_the_params_family_does_run_inside_a_global_text_transaction() {
    // The counterpart asymmetry, pinned so it is not mistaken for a bug report. The params
    // family does NOT hold the global mutex across the statement, so re-entry succeeds and
    // the body joins the open global transaction. Same statement, different family,
    // different answer — which is exactly why §2.11 exists.
    let db = db();
    setup(&db);

    db.execute("BEGIN").expect("BEGIN");
    let affected = db
        .execute_params("CALL p_zero()", &[])
        .expect("KNOWN GAP: this one runs — see the section comment");
    db.execute("COMMIT").expect("COMMIT");

    assert_eq!(affected, 0);
    assert_eq!(
        only_row(&db),
        (0, "zero".to_string()),
        "the body ran and its write committed with the enclosing transaction"
    );
}
