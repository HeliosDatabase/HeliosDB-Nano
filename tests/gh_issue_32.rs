//! GH#32 — `DELETE`/`UPDATE … WHERE <col> IN (SELECT …)` is rejected with
//! "IN subquery evaluation requires executor context. Use executor for subquery
//! evaluation." (`src/sql/evaluator.rs:513`).
//!
//! # Mechanism
//!
//! Subqueries are NOT evaluated by `Evaluator`. They are materialised one level
//! up, by `Executor::materialize_subqueries` (`src/sql/executor/mod.rs:1926`,
//! uncorrelated) and `Executor::materialize_subqueries_with_outer`
//! (`src/sql/executor/mod.rs:2321`, correlated), which run the inner plan and
//! rewrite the node into an `InList`/`InSet`/`Literal`. Every SELECT-side
//! predicate site calls one of them — the Filter arm (`executor/mod.rs:3447`
//! correlated / `:3462` uncorrelated), the scan/pushdown predicate binder
//! (`executor/mod.rs:1192`, `:1317`, `:1643`) and the join site
//! (`executor/join.rs:2030`). (`materialize_subqueries` does NOT appear in
//! `executor/scan.rs` at all — an earlier triage cited `scan.rs:384/:536/:911`
//! for it; those citations are wrong. The verdict is unaffected.)
//! `Evaluator::evaluate` therefore
//! reaches `LogicalExpr::InSubquery` / `Exists` / `ScalarSubquery` only when a
//! caller forgot, and its arms are pure `Err(...)` (`evaluator.rs:509`, `:525`,
//! `:537`).
//!
//! The four DML predicate sites all forgot:
//!
//! | family | statement | site |
//! |--------|-----------|------|
//! | text (`db.execute` → `execute_in_transaction_inner`)          | UPDATE | `src/lib.rs:6489` |
//! | text                                                          | DELETE | `src/lib.rs:6882` |
//! | params (`db.execute_params` → `execute_plan_with_params_inner`)| UPDATE | `src/lib.rs:15724` |
//! | params                                                        | DELETE | `src/lib.rs:16036` |
//!
//! Each is a bare `evaluator.evaluate(predicate, &tuple)?` inside the per-row
//! loop, on the raw planner predicate. Nothing materialises it first.
//!
//! That is also why the reporter saw the statement "succeed while the subquery
//! matches nothing": the error is raised per ROW OF THE TARGET TABLE, so an
//! empty target table never reaches the evaluator at all. It has nothing to do
//! with how many rows the subquery returns.
//!
//! # Two more sites this file pins
//!
//! * `SELECT count(*) FROM t WHERE … IN (SELECT …)` — the COUNT(*)-over-
//!   Filter(Scan) fast path at `src/sql/executor/mod.rs:3967` also evaluates the
//!   RAW predicate. So the claim "SELECT works" is only true for row-returning
//!   SELECTs.
//! * NULL semantics of `NOT IN`. `materialize_subqueries` emits an `InList` for
//!   <= 16 subquery rows and an `InSet` (HashSet) for more
//!   (`executor/mod.rs:1949`). The `InList` evaluator arm implements the SQL
//!   three-valued rule (`evaluator.rs:408-495`: a NULL in the list turns a
//!   non-match into NULL, so `NOT IN` yields no rows). The `InSet` arm
//!   (`evaluator.rs:499-506`) does NOT — it never inspects the set for NULL, so
//!   `NOT IN` over a 17+-row subquery containing NULL wrongly returns TRUE. That
//!   half is broken on the READ path today, independent of the DML defect.
//!
//! # Blast radius the first triage missed: MySQL multi-table DELETE
//!
//! `src/protocol/mysql/translator.rs` REWRITES MySQL's multi-table
//! `DELETE a, b FROM …` into exactly the shape this bug rejects:
//! `DELETE FROM {table} WHERE {col} IN (SELECT {alias}.{col} FROM … WHERE …)`
//! (`translator.rs:653`, `:656` for the INNER JOIN form; `:690`, `:695` for the
//! comma-join form WordPress transient cleanup emits). Every one of those
//! translated statements therefore dies in the evaluator, so MySQL-wire
//! multi-table DELETE — an advertised surface — has never worked. The
//! translator's own tests assert only the produced STRING and never execute it,
//! which is why this stayed invisible.
//! `mysql_multi_table_delete_translation_shape` below pins it.
//!
//! # A note on `InSet` producers
//!
//! `LogicalExpr::InSet` is produced in exactly ONE place, `materialize_subqueries`
//! (`executor/mod.rs:1954`). `evaluator.rs:162` is `bind_expr_columns` — a
//! pass-through that rebinds columns, not a producer — so no `InSet` can reach
//! the evaluator except through the subquery materialiser, and the step-4 fix
//! has exactly one upstream to worry about.
//!
//! # Reading the results
//!
//! Functions whose name ends in `_control` MUST pass on the unfixed tree — they
//! prove the harness runs. Everything else is expected to FAIL on the unfixed
//! tree and pass after the fix.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::session::SessionId;
use heliosdb_nano::{EmbeddedDatabase, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// `false` = the TEXT family (`db.execute()` → `execute_in_transaction_inner`;
/// psql simple query, MySQL wire, embedded `execute`).
/// `true`  = the PARAMS family (`db.execute_params()` →
/// `execute_plan_with_params_inner`; the PostgreSQL EXTENDED protocol every real
/// driver speaks, plus the REST/BaaS layer).
const FAMILIES: [bool; 2] = [false, true];

fn family(params_family: bool) -> &'static str {
    if params_family {
        "params"
    } else {
        "text"
    }
}

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

fn ddl(db: &EmbeddedDatabase, sql: &str) {
    db.execute(sql).unwrap_or_else(|e| panic!("setup `{sql}` failed: {e}"));
}

/// Run one DML statement through the requested executor family.
fn run(db: &EmbeddedDatabase, sql: &str, params_family: bool) -> heliosdb_nano::Result<u64> {
    if params_family {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    }
}

/// Run and require success, reporting the family in the panic message.
fn run_ok(db: &EmbeddedDatabase, sql: &str, params_family: bool) -> u64 {
    run(db, sql, params_family).unwrap_or_else(|e| panic!("[{} family] `{sql}` failed: {e}", family(params_family)))
}

fn int_of(v: &Value) -> i64 {
    match *v {
        Value::Int2(n) => i64::from(n),
        Value::Int4(n) => i64::from(n),
        Value::Int8(n) => n,
        ref other => panic!("expected an integer, got {other:?}"),
    }
}

/// Every `id` physically present in `table`, sorted.
///
/// A row-returning scan on purpose, never `SELECT count(*)`: a count query
/// returns one row whether the answer is 0 or 10000, and — as this file's
/// `count_star_*` test shows — the COUNT(*) fast path has its own subquery
/// defect, so it is not a witness for anything here.
fn ids_in(db: &EmbeddedDatabase, table: &str) -> Vec<i64> {
    let sql = format!("SELECT id FROM {table}");
    let mut ids: Vec<i64> = db
        .query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .iter()
        .map(|t| int_of(&t.values[0]))
        .collect();
    ids.sort_unstable();
    ids
}

/// Every `n` in `s`, keyed by `id`, sorted by id. Used by the UPDATE tests.
fn n_by_id(db: &EmbeddedDatabase) -> Vec<(i64, i64)> {
    let mut rows: Vec<(i64, i64)> = db
        .query("SELECT id, n FROM s", &[])
        .expect("SELECT id, n FROM s")
        .iter()
        .map(|t| (int_of(&t.values[0]), int_of(&t.values[1])))
        .collect();
    rows.sort_unstable();
    rows
}

/// The reproducer's shape, with integer keys so the assertions read clearly:
///
/// ```text
/// p:  (1,'alice')  (2,'bob')  (3,'carol')
/// s:  (10, pid 1, n 1)  (11, pid 2, n 2)  (12, pid 3, n 3)
/// ```
fn fixture() -> EmbeddedDatabase {
    let db = mem_db();
    ddl(&db, "CREATE TABLE p (id INT PRIMARY KEY, login TEXT)");
    ddl(&db, "CREATE TABLE s (id INT PRIMARY KEY, pid INT, n INT)");
    ddl(&db, "INSERT INTO p VALUES (1, 'alice')");
    ddl(&db, "INSERT INTO p VALUES (2, 'bob')");
    ddl(&db, "INSERT INTO p VALUES (3, 'carol')");
    ddl(&db, "INSERT INTO s VALUES (10, 1, 1)");
    ddl(&db, "INSERT INTO s VALUES (11, 2, 2)");
    ddl(&db, "INSERT INTO s VALUES (12, 3, 3)");
    db
}

/// `pn` is the NULL-bearing subquery source: `SELECT v FROM pn` yields
/// `rows` non-NULL values (1000, 1001, …) plus, when `with_null`, one NULL.
///
/// `rows + with_null > 16` is what pushes `materialize_subqueries` off the
/// `InList` branch and onto the `InSet` (HashSet) branch — the one with no NULL
/// handling at all.
fn add_pn(db: &EmbeddedDatabase, rows: i64, with_null: bool) {
    ddl(db, "CREATE TABLE pn (k INT PRIMARY KEY, v INT)");
    for i in 0..rows {
        ddl(db, &format!("INSERT INTO pn VALUES ({}, {})", i, 1000 + i));
    }
    if with_null {
        ddl(db, &format!("INSERT INTO pn VALUES ({}, NULL)", rows));
    }
}

// ---------------------------------------------------------------------------
// POSITIVE CONTROLS (`*_control`, MUST pass before AND after the fix) and
// non-vacuity guards.
// ---------------------------------------------------------------------------

/// Plain (subquery-free) `DELETE`/`UPDATE` predicates work in both families, and
/// a row-returning `SELECT` with `IN (SELECT …)` already works.
///
/// If this one ever fails the harness is broken, not the feature: the SELECT
/// half is the proof that executor-backed subquery evaluation EXISTS and is
/// wired up — the DML arms simply do not call it.
#[test]
fn plain_predicates_and_select_subquery_control() {
    for params_family in FAMILIES {
        let db = fixture();

        // Subquery-free DML: unaffected by this issue, both families.
        assert_eq!(run_ok(&db, "DELETE FROM s WHERE n = 3", params_family), 1);
        assert_eq!(ids_in(&db, "s"), vec![10, 11], "[{}]", family(params_family));
        assert_eq!(run_ok(&db, "UPDATE s SET n = 42 WHERE pid = 1", params_family), 1);
        assert_eq!(n_by_id(&db), vec![(10, 42), (11, 2)], "[{}]", family(params_family));

        // The executor-backed subquery path, reached from a row-returning SELECT.
        let rows = db
            .query(
                "SELECT id FROM s WHERE pid IN (SELECT id FROM p WHERE login LIKE 'ali%')",
                &[],
            )
            .expect("SELECT … IN (SELECT …) must work");
        let ids: Vec<i64> = rows.iter().map(|t| int_of(&t.values[0])).collect();
        assert_eq!(ids, vec![10], "SELECT … IN (SELECT …) [{}]", family(params_family));
    }
}

/// Correlated `EXISTS` on the READ path, where `Executor`'s Filter arm already
/// routes through `materialize_subqueries_with_outer`
/// (`src/sql/executor/mod.rs:3447`). Expected to pass on the unfixed tree; kept
/// OUT of the control above so an unrelated correlated-SELECT gap cannot mask
/// the control's job. If this one fails on the unfixed tree, the correlated DML
/// tests below need a working read path first.
#[test]
fn select_exists_correlated_probe() {
    let db = fixture();
    let rows = db
        .query(
            "SELECT id FROM s WHERE EXISTS (SELECT 1 FROM p WHERE p.id = s.pid AND p.login = 'alice')",
            &[],
        )
        .expect("SELECT … EXISTS (…)");
    let ids: Vec<i64> = rows.iter().map(|t| int_of(&t.values[0])).collect();
    assert_eq!(ids, vec![10]);
}

/// The `InSet` (17+ subquery rows) branch of `materialize_subqueries` is LIVE
/// and otherwise correct — without a NULL in the set, `NOT IN` over it deletes
/// exactly the non-members.
///
/// This is the non-vacuity guard for `delete_not_in_subquery_with_null_large`
/// below: it proves that test's failure is about NULL, not about the branch
/// being unreachable. It is a control for the SET SIZE only — it still exercises
/// the DML defect, so it fails on the unfixed tree like its sibling.
#[test]
fn delete_not_in_large_subquery_without_null_deletes_non_members() {
    for params_family in FAMILIES {
        let db = fixture();
        add_pn(&db, 20, false); // 20 rows, no NULL → InSet branch
        ddl(&db, "INSERT INTO s VALUES (13, 1000, 9)"); // pid 1000 IS in pn

        // 10, 11, 12 have pid 1/2/3 — none of them in pn — so all three go.
        assert_eq!(
            run_ok(&db, "DELETE FROM s WHERE pid NOT IN (SELECT v FROM pn)", params_family),
            3
        );
        assert_eq!(ids_in(&db, "s"), vec![13], "[{}]", family(params_family));
    }
}

// ---------------------------------------------------------------------------
// DELETE — IN (SELECT …)
// ---------------------------------------------------------------------------

/// ***UNFIXED*** The issue verbatim, integer-keyed. Both families.
///
/// Unfixed tree: `Err("Query execution error: IN subquery evaluation requires
/// executor context. Use executor for subquery evaluation.")`, zero rows deleted.
#[test]
fn delete_where_in_subquery() {
    for params_family in FAMILIES {
        let db = fixture();
        assert_eq!(
            run_ok(
                &db,
                "DELETE FROM s WHERE pid IN (SELECT id FROM p WHERE login LIKE 'ali%')",
                params_family
            ),
            1,
            "[{}] one child of 'alice'",
            family(params_family)
        );
        assert_eq!(ids_in(&db, "s"), vec![11, 12], "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** The issue's own reproducer shape: UUID primary keys and a real
/// `REFERENCES` foreign key, so the referencing-FK enforcement path
/// (`enforce_referencing_fks_on_delete`) participates.
#[test]
fn delete_where_in_subquery_uuid_and_foreign_key() {
    const ALICE: &str = "11111111-1111-4111-8111-111111111111";
    const BOB: &str = "33333333-3333-4333-8333-333333333333";
    const CHILD_A: &str = "22222222-2222-4222-8222-222222222222";
    const CHILD_B: &str = "44444444-4444-4444-8444-444444444444";

    for params_family in FAMILIES {
        let db = mem_db();
        ddl(
            &db,
            r#"CREATE TABLE "P" ("id" UUID PRIMARY KEY, "login" VARCHAR(39) NOT NULL)"#,
        );
        ddl(
            &db,
            r#"CREATE TABLE "S" ("id" UUID PRIMARY KEY, "pid" UUID NOT NULL REFERENCES "P"("id"), "n" INTEGER)"#,
        );
        ddl(&db, &format!("INSERT INTO \"P\" VALUES ('{ALICE}', 'alice')"));
        ddl(&db, &format!("INSERT INTO \"P\" VALUES ('{BOB}', 'bob')"));
        ddl(&db, &format!("INSERT INTO \"S\" VALUES ('{CHILD_A}', '{ALICE}', 1)"));
        ddl(&db, &format!("INSERT INTO \"S\" VALUES ('{CHILD_B}', '{BOB}', 2)"));

        assert_eq!(
            run_ok(
                &db,
                r#"DELETE FROM "S" WHERE "pid" IN (SELECT "id" FROM "P" WHERE "login" LIKE 'ali%')"#,
                params_family
            ),
            1,
            "[{}]",
            family(params_family)
        );
        let remaining = db.query(r#"SELECT "id" FROM "S""#, &[]).expect("scan S");
        assert_eq!(remaining.len(), 1, "[{}]", family(params_family));
        let surviving = match &remaining[0].values[0] {
            Value::Uuid(u) => u.to_string(),
            Value::String(s) => s.clone(),
            other => panic!("[{}] unexpected id type {other:?}", family(params_family)),
        };
        assert_eq!(
            surviving,
            CHILD_B,
            "[{}] the surviving child must be bob's",
            family(params_family)
        );
    }
}

/// ***UNFIXED*** An `IN (SELECT …)` whose subquery matches NOTHING must delete
/// nothing and must NOT raise. (The reporter believed this case already worked;
/// it only appeared to, because their target table was still empty.)
#[test]
fn delete_where_in_subquery_empty_result_deletes_nothing() {
    for params_family in FAMILIES {
        let db = fixture();
        assert_eq!(
            run_ok(
                &db,
                "DELETE FROM s WHERE pid IN (SELECT id FROM p WHERE login = 'nobody')",
                params_family
            ),
            0,
            "[{}]",
            family(params_family)
        );
        assert_eq!(ids_in(&db, "s"), vec![10, 11, 12], "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** Bound parameters must reach the SUBQUERY's executor, not just
/// the outer predicate. Params family only — `$1` has no meaning in the text
/// family.
#[test]
fn delete_where_in_subquery_with_bound_parameter() {
    let db = fixture();
    let deleted = db
        .execute_params(
            "DELETE FROM s WHERE pid IN (SELECT id FROM p WHERE login = $1)",
            &[Value::String("alice".to_string())],
        )
        .expect("parameterised DELETE … IN (SELECT … WHERE login = $1)");
    assert_eq!(deleted, 1);
    assert_eq!(ids_in(&db, "s"), vec![11, 12]);
}

/// ***UNFIXED*** `DELETE … RETURNING` with an `IN (SELECT …)` predicate — the
/// shape Prisma and the REST layer emit.
#[test]
fn delete_where_in_subquery_returning() {
    // Text family.
    let db = fixture();
    let (count, rows) = db
        .execute_returning("DELETE FROM s WHERE pid IN (SELECT id FROM p WHERE login LIKE 'ali%') RETURNING id")
        .expect("text-family DELETE … IN (SELECT …) RETURNING");
    assert_eq!(count, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(int_of(&rows[0].values[0]), 10);

    // Params family.
    let db = fixture();
    let (count, rows) = db
        .execute_params_returning(
            "DELETE FROM s WHERE pid IN (SELECT id FROM p WHERE login LIKE 'ali%') RETURNING id",
            &[],
        )
        .expect("params-family DELETE … IN (SELECT …) RETURNING");
    assert_eq!(count, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(int_of(&rows[0].values[0]), 10);
}

// ---------------------------------------------------------------------------
// DELETE — NOT IN (SELECT …), including the NULL rule
// ---------------------------------------------------------------------------

/// ***UNFIXED*** `NOT IN` over a NULL-free subquery deletes the non-members.
#[test]
fn delete_where_not_in_subquery() {
    for params_family in FAMILIES {
        let db = fixture();
        assert_eq!(
            run_ok(
                &db,
                "DELETE FROM s WHERE pid NOT IN (SELECT id FROM p WHERE login LIKE 'ali%')",
                params_family
            ),
            2,
            "[{}]",
            family(params_family)
        );
        assert_eq!(ids_in(&db, "s"), vec![10], "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** THE CLASSIC MISTAKE, small (`InList`) branch.
///
/// `x NOT IN (…, NULL, …)` is never TRUE in SQL: a non-match yields UNKNOWN, so
/// the row is not selected. `DELETE … WHERE pid NOT IN (SELECT v FROM pn)` where
/// `pn` contains a NULL must delete **nothing**.
#[test]
fn delete_not_in_subquery_with_null_small_deletes_nothing() {
    for params_family in FAMILIES {
        let db = fixture();
        add_pn(&db, 3, true); // 4 subquery rows → InList branch
        assert_eq!(
            run_ok(&db, "DELETE FROM s WHERE pid NOT IN (SELECT v FROM pn)", params_family),
            0,
            "[{}] a NULL in the subquery result makes NOT IN select no rows",
            family(params_family)
        );
        assert_eq!(ids_in(&db, "s"), vec![10, 11, 12], "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** THE CLASSIC MISTAKE, large (`InSet`) branch — the one that
/// stays broken if the fix only routes DML through `materialize_subqueries`.
///
/// With 17+ subquery rows `materialize_subqueries` emits `LogicalExpr::InSet`
/// (`executor/mod.rs:1949`), and the evaluator's `InSet` arm
/// (`evaluator.rs:499-506`) never inspects the set for NULL. So the same
/// statement that correctly deletes nothing at 4 rows deletes EVERYTHING at 21.
#[test]
fn delete_not_in_subquery_with_null_large_deletes_nothing() {
    for params_family in FAMILIES {
        let db = fixture();
        add_pn(&db, 20, true); // 21 subquery rows → InSet branch
        assert_eq!(
            run_ok(&db, "DELETE FROM s WHERE pid NOT IN (SELECT v FROM pn)", params_family),
            0,
            "[{}] InSet must honour SQL's NOT IN NULL rule exactly as InList does",
            family(params_family)
        );
        assert_eq!(ids_in(&db, "s"), vec![10, 11, 12], "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** The same NULL rule on the READ path, where subqueries already
/// work. This one is independent of the DML defect: it fails today purely
/// because of the `InSet` arm, and it is the regression pin for that half of the
/// fix.
///
/// Unfixed tree: returns all three rows of `s`. Correct: none.
#[test]
fn select_not_in_large_subquery_with_null_returns_no_rows() {
    let db = fixture();
    add_pn(&db, 20, true); // 21 subquery rows → InSet branch

    let small = db
        .query(
            "SELECT id FROM s WHERE pid NOT IN (SELECT v FROM pn WHERE v IS NULL OR v < 1003)",
            &[],
        )
        .expect("small NOT IN");
    assert!(small.is_empty(), "InList branch already honours the NULL rule");

    let large = db
        .query("SELECT id FROM s WHERE pid NOT IN (SELECT v FROM pn)", &[])
        .expect("large NOT IN");
    assert!(
        large.is_empty(),
        "a NULL anywhere in the subquery result makes NOT IN select no rows; got {} row(s)",
        large.len()
    );
}

// ---------------------------------------------------------------------------
// DELETE — EXISTS / NOT EXISTS / scalar / correlated
// ---------------------------------------------------------------------------

/// ***UNFIXED*** Correlated `EXISTS` in a DELETE predicate.
///
/// Unfixed tree: "EXISTS subquery evaluation requires executor context"
/// (`evaluator.rs:541`).
#[test]
fn delete_where_exists_correlated() {
    for params_family in FAMILIES {
        let db = fixture();
        assert_eq!(
            run_ok(
                &db,
                "DELETE FROM s WHERE EXISTS (SELECT 1 FROM p WHERE p.id = s.pid AND p.login LIKE 'ali%')",
                params_family
            ),
            1,
            "[{}]",
            family(params_family)
        );
        assert_eq!(ids_in(&db, "s"), vec![11, 12], "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** Correlated `NOT EXISTS` in a DELETE predicate.
#[test]
fn delete_where_not_exists_correlated() {
    for params_family in FAMILIES {
        let db = fixture();
        assert_eq!(
            run_ok(
                &db,
                "DELETE FROM s WHERE NOT EXISTS (SELECT 1 FROM p WHERE p.id = s.pid AND p.login LIKE 'ali%')",
                params_family
            ),
            2,
            "[{}]",
            family(params_family)
        );
        assert_eq!(ids_in(&db, "s"), vec![10], "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** Uncorrelated scalar subquery in a DELETE predicate.
///
/// Unfixed tree: "Scalar subquery reached the evaluator without materialisation"
/// (`evaluator.rs:531`).
#[test]
fn delete_where_scalar_subquery() {
    for params_family in FAMILIES {
        let db = fixture();
        ddl(&db, "CREATE TABLE lim (k INT PRIMARY KEY, v INT)");
        ddl(&db, "INSERT INTO lim VALUES (1, 3)");
        assert_eq!(
            run_ok(&db, "DELETE FROM s WHERE n = (SELECT v FROM lim)", params_family),
            1,
            "[{}]",
            family(params_family)
        );
        assert_eq!(ids_in(&db, "s"), vec![10, 11], "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** A CORRELATED `IN (SELECT …)`: the inner plan references the
/// outer row (`s.pid`), so it can only be answered per outer row
/// (`materialize_subqueries_with_outer`), never once at plan-build time.
#[test]
fn delete_where_in_correlated_subquery() {
    for params_family in FAMILIES {
        let db = fixture();
        assert_eq!(
            run_ok(
                &db,
                "DELETE FROM s WHERE pid IN (SELECT p.id FROM p WHERE p.id = s.pid AND p.login LIKE 'ali%')",
                params_family
            ),
            1,
            "[{}]",
            family(params_family)
        );
        assert_eq!(ids_in(&db, "s"), vec![11, 12], "[{}]", family(params_family));
    }
}

// ---------------------------------------------------------------------------
// UPDATE — the same matrix
// ---------------------------------------------------------------------------

/// ***UNFIXED*** `UPDATE … WHERE col IN (SELECT …)`, both families.
#[test]
fn update_where_in_subquery() {
    for params_family in FAMILIES {
        let db = fixture();
        assert_eq!(
            run_ok(
                &db,
                "UPDATE s SET n = 99 WHERE pid IN (SELECT id FROM p WHERE login LIKE 'ali%')",
                params_family
            ),
            1,
            "[{}]",
            family(params_family)
        );
        assert_eq!(
            n_by_id(&db),
            vec![(10, 99), (11, 2), (12, 3)],
            "[{}]",
            family(params_family)
        );
    }
}

/// ***UNFIXED*** `UPDATE … WHERE col IN (SELECT …)` whose subquery is empty:
/// updates nothing, raises nothing.
#[test]
fn update_where_in_subquery_empty_result_updates_nothing() {
    for params_family in FAMILIES {
        let db = fixture();
        assert_eq!(
            run_ok(
                &db,
                "UPDATE s SET n = 99 WHERE pid IN (SELECT id FROM p WHERE login = 'nobody')",
                params_family
            ),
            0,
            "[{}]",
            family(params_family)
        );
        assert_eq!(
            n_by_id(&db),
            vec![(10, 1), (11, 2), (12, 3)],
            "[{}]",
            family(params_family)
        );
    }
}

/// ***UNFIXED*** `UPDATE … WHERE col NOT IN (SELECT …)`.
#[test]
fn update_where_not_in_subquery() {
    for params_family in FAMILIES {
        let db = fixture();
        assert_eq!(
            run_ok(
                &db,
                "UPDATE s SET n = 99 WHERE pid NOT IN (SELECT id FROM p WHERE login LIKE 'ali%')",
                params_family
            ),
            2,
            "[{}]",
            family(params_family)
        );
        assert_eq!(
            n_by_id(&db),
            vec![(10, 1), (11, 99), (12, 99)],
            "[{}]",
            family(params_family)
        );
    }
}

/// ***UNFIXED*** `NOT IN` NULL rule in an UPDATE, `InList` branch.
#[test]
fn update_not_in_subquery_with_null_small_updates_nothing() {
    for params_family in FAMILIES {
        let db = fixture();
        add_pn(&db, 3, true);
        assert_eq!(
            run_ok(
                &db,
                "UPDATE s SET n = 99 WHERE pid NOT IN (SELECT v FROM pn)",
                params_family
            ),
            0,
            "[{}]",
            family(params_family)
        );
        assert_eq!(
            n_by_id(&db),
            vec![(10, 1), (11, 2), (12, 3)],
            "[{}]",
            family(params_family)
        );
    }
}

/// ***UNFIXED*** `NOT IN` NULL rule in an UPDATE, `InSet` branch.
#[test]
fn update_not_in_subquery_with_null_large_updates_nothing() {
    for params_family in FAMILIES {
        let db = fixture();
        add_pn(&db, 20, true);
        assert_eq!(
            run_ok(
                &db,
                "UPDATE s SET n = 99 WHERE pid NOT IN (SELECT v FROM pn)",
                params_family
            ),
            0,
            "[{}]",
            family(params_family)
        );
        assert_eq!(
            n_by_id(&db),
            vec![(10, 1), (11, 2), (12, 3)],
            "[{}]",
            family(params_family)
        );
    }
}

/// ***UNFIXED*** Correlated `EXISTS` / `NOT EXISTS` in an UPDATE predicate.
#[test]
fn update_where_exists_and_not_exists_correlated() {
    for params_family in FAMILIES {
        let db = fixture();
        assert_eq!(
            run_ok(
                &db,
                "UPDATE s SET n = 99 WHERE EXISTS (SELECT 1 FROM p WHERE p.id = s.pid AND p.login LIKE 'ali%')",
                params_family
            ),
            1,
            "[{}]",
            family(params_family)
        );
        assert_eq!(
            n_by_id(&db),
            vec![(10, 99), (11, 2), (12, 3)],
            "[{}]",
            family(params_family)
        );

        assert_eq!(
            run_ok(
                &db,
                "UPDATE s SET n = 77 WHERE NOT EXISTS (SELECT 1 FROM p WHERE p.id = s.pid AND p.login LIKE 'ali%')",
                params_family
            ),
            2,
            "[{}]",
            family(params_family)
        );
        assert_eq!(
            n_by_id(&db),
            vec![(10, 99), (11, 77), (12, 77)],
            "[{}]",
            family(params_family)
        );
    }
}

/// ***UNFIXED*** Scalar subquery in an UPDATE **predicate**.
///
/// Note the asymmetry this pins: the same scalar subquery on the RIGHT of
/// `SET n = (SELECT …)` already works, because the UPDATE arms call
/// `materialize_scalar_subqueries_for_row` on the assignment expression
/// (`src/lib.rs:6513`, `src/lib.rs:15740`) — and only on the assignment
/// expression. The WHERE clause was never given the same treatment.
#[test]
fn update_where_scalar_subquery() {
    for params_family in FAMILIES {
        let db = fixture();
        ddl(&db, "CREATE TABLE lim (k INT PRIMARY KEY, v INT)");
        ddl(&db, "INSERT INTO lim VALUES (1, 3)");

        // The SET side already works — proves the asymmetry is real.
        assert_eq!(
            run_ok(&db, "UPDATE s SET n = (SELECT v FROM lim) WHERE id = 11", params_family),
            1,
            "[{}] scalar subquery in SET already works",
            family(params_family)
        );

        assert_eq!(
            run_ok(&db, "UPDATE s SET n = 99 WHERE n = (SELECT v FROM lim)", params_family),
            2,
            "[{}] scalar subquery in WHERE",
            family(params_family)
        );
        assert_eq!(
            n_by_id(&db),
            vec![(10, 1), (11, 99), (12, 99)],
            "[{}]",
            family(params_family)
        );
    }
}

/// ***UNFIXED*** Correlated `IN (SELECT …)` in an UPDATE predicate.
#[test]
fn update_where_in_correlated_subquery() {
    for params_family in FAMILIES {
        let db = fixture();
        assert_eq!(
            run_ok(
                &db,
                "UPDATE s SET n = 99 WHERE pid IN (SELECT p.id FROM p WHERE p.id = s.pid AND p.login LIKE 'ali%')",
                params_family
            ),
            1,
            "[{}]",
            family(params_family)
        );
        assert_eq!(
            n_by_id(&db),
            vec![(10, 99), (11, 2), (12, 3)],
            "[{}]",
            family(params_family)
        );
    }
}

// ---------------------------------------------------------------------------
// Transaction visibility (see 3fc8385 — the uncommitted-write census)
// ---------------------------------------------------------------------------

fn session(db: &EmbeddedDatabase, name: &str) -> SessionId {
    db.create_wire_session(name).expect("wire session")
}

/// ***UNFIXED*** A DML subquery must see the statement's OWN uncommitted writes.
///
/// `BEGIN; INSERT INTO p …; DELETE FROM s WHERE pid IN (SELECT id FROM p …)`
/// must delete the child of the row this transaction just inserted. If the fix
/// runs the subquery on a bare `Executor::with_storage(...)` with no transaction
/// attached, the inner SELECT reads committed storage only and this fails with
/// `1` instead of `2`.
#[test]
fn delete_in_subquery_sees_own_uncommitted_insert() {
    let db = fixture();
    ddl(&db, "INSERT INTO s VALUES (13, 4, 4)");

    let sid = session(&db, "writer");
    db.begin_transaction_for_session(sid).expect("BEGIN");
    db.execute_for_session(sid, "INSERT INTO p VALUES (4, 'alicia')")
        .expect("INSERT INTO p inside the transaction");
    let deleted = db
        .execute_for_session(
            sid,
            "DELETE FROM s WHERE pid IN (SELECT id FROM p WHERE login LIKE 'ali%')",
        )
        .expect("DELETE … IN (SELECT …) inside a transaction");
    assert_eq!(deleted, 2, "must see both 'alice' (committed) and 'alicia' (own write)");
    db.commit_transaction_for_session(sid).expect("COMMIT");

    assert_eq!(ids_in(&db, "s"), vec![11, 12]);
}

/// ***UNFIXED*** A DML subquery must NOT see ANOTHER session's uncommitted rows.
///
/// Session A holds an open transaction with an uncommitted `p` row whose login
/// matches. Session B's autocommit DELETE must delete only the child of the
/// COMMITTED match. Getting this wrong is the failure class 3fc8385 fixed for
/// COUNT(*)/PK lookups; it must not be reintroduced through the subquery
/// executor.
#[test]
fn delete_in_subquery_does_not_see_other_sessions_uncommitted_rows() {
    let db = fixture();
    ddl(&db, "INSERT INTO s VALUES (13, 4, 4)");

    let writer = session(&db, "writer");
    let deleter = session(&db, "deleter");

    db.begin_transaction_for_session(writer).expect("BEGIN");
    db.execute_for_session(writer, "INSERT INTO p VALUES (4, 'alicia')")
        .expect("uncommitted INSERT INTO p");

    let deleted = db
        .execute_for_session(
            deleter,
            "DELETE FROM s WHERE pid IN (SELECT id FROM p WHERE login LIKE 'ali%')",
        )
        .expect("DELETE … IN (SELECT …) from a second session");
    assert_eq!(
        deleted, 1,
        "only the child of the COMMITTED 'alice' may be deleted; the other session's \
         uncommitted 'alicia' must be invisible"
    );

    db.rollback_transaction_for_session(writer).expect("ROLLBACK");
    assert_eq!(ids_in(&db, "s"), vec![11, 12, 13]);
}

// ---------------------------------------------------------------------------
// The fifth site: COUNT(*) over Filter(Scan) (src/sql/executor/mod.rs:3967)
// ---------------------------------------------------------------------------

/// ***UNFIXED*** `SELECT count(*) … WHERE col IN (SELECT …)`.
///
/// This is a READ, not DML, and it is the counterexample to "SELECT works": the
/// COUNT(*)-over-Filter(Scan) fast path evaluates the RAW predicate at
/// `src/sql/executor/mod.rs:3967`, so it hits the same evaluator arm. The
/// row-returning form of the same query (asserted in the control above) works.
#[test]
fn count_star_with_in_subquery() {
    let db = fixture();
    let rows = db
        .query(
            "SELECT count(*) FROM s WHERE pid IN (SELECT id FROM p WHERE login LIKE 'ali%')",
            &[],
        )
        .expect("count(*) with an IN (SELECT …) predicate");
    assert_eq!(rows.len(), 1);
    assert_eq!(int_of(&rows[0].values[0]), 1);
}

/// ***UNFIXED*** Same, with EXISTS.
#[test]
fn count_star_with_exists_subquery() {
    let db = fixture();
    let rows = db
        .query(
            "SELECT count(*) FROM s WHERE EXISTS (SELECT 1 FROM p WHERE p.id = s.pid AND p.login LIKE 'ali%')",
            &[],
        )
        .expect("count(*) with an EXISTS predicate");
    assert_eq!(rows.len(), 1);
    assert_eq!(int_of(&rows[0].values[0]), 1);
}

// ---------------------------------------------------------------------------
// ADDED BY ADVERSARIAL REVIEW — the semantics the fix's "fail-closed" choices
// hinge on, plus the two blast-radius shapes the first pass missed.
// ---------------------------------------------------------------------------

/// ***UNFIXED*** `x NOT IN (<empty set>)` is TRUE for every row, so this DELETE
/// must empty the table.
///
/// This is the assertion that makes the fix plan's step-6 fail-closed rule
/// LOAD-BEARING rather than decorative. `materialize_subqueries_with_outer`'s
/// `run` closure swallows a failing subquery into `unwrap_or_default()` — an
/// EMPTY result (`src/sql/executor/mod.rs:2340`), and the uncorrelated scalar
/// arm swallows to NULL (`:1994-2001`). "Subquery failed" and "subquery
/// legitimately returned nothing" are therefore INDISTINGUISHABLE downstream,
/// and this test proves that the second of the two must delete everything. So
/// the first one MUST NOT be allowed to reach here: inside DML the swallow has
/// to be off and the error has to abort the statement. A fix that keeps the
/// swallow turns any subquery error into `DELETE ALL ROWS`.
#[test]
fn delete_not_in_empty_subquery_deletes_all() {
    for params_family in FAMILIES {
        let db = fixture();
        ddl(&db, "CREATE TABLE empt (k INT PRIMARY KEY, v INT)");
        assert_eq!(
            run_ok(
                &db,
                "DELETE FROM s WHERE pid NOT IN (SELECT v FROM empt)",
                params_family
            ),
            3,
            "[{}] NOT IN over an empty set is TRUE for every row",
            family(params_family)
        );
        assert!(ids_in(&db, "s").is_empty(), "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** The mirror image: `IN (<empty set>)` is FALSE for every row.
/// Together with the test above this pins both edges of the empty-subquery rule,
/// so an implementer cannot satisfy one by breaking the other.
#[test]
fn delete_in_empty_subquery_deletes_nothing() {
    for params_family in FAMILIES {
        let db = fixture();
        ddl(&db, "CREATE TABLE empt (k INT PRIMARY KEY, v INT)");
        assert_eq!(
            run_ok(&db, "DELETE FROM s WHERE pid IN (SELECT v FROM empt)", params_family),
            0,
            "[{}]",
            family(params_family)
        );
        assert_eq!(ids_in(&db, "s"), vec![10, 11, 12], "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** A scalar subquery over ZERO rows is NULL, and `n = NULL` is
/// UNKNOWN — so nothing is deleted. The `match result { Boolean(b) => b, _ =>
/// false }` collapse at the four DML sites is what delivers this; the fix plan's
/// step 3 says to KEEP it, and this is the test that holds it to that.
#[test]
fn delete_where_empty_scalar_subquery_deletes_nothing() {
    for params_family in FAMILIES {
        let db = fixture();
        ddl(&db, "CREATE TABLE lim (k INT PRIMARY KEY, v INT)");
        assert_eq!(
            run_ok(&db, "DELETE FROM s WHERE n = (SELECT v FROM lim)", params_family),
            0,
            "[{}] `n = NULL` is UNKNOWN, not TRUE",
            family(params_family)
        );
        assert_eq!(ids_in(&db, "s"), vec![10, 11, 12], "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** The subquery reads the TABLE BEING DELETED FROM.
///
/// PostgreSQL evaluates the sub-SELECT against the statement's snapshot, once —
/// it must not observe rows the same statement has already removed. This is the
/// regression pin for the fix plan's "materialise ONCE, before the row loop"
/// requirement: an implementer who re-runs the subquery per row to keep the code
/// simple gets a different (and order-dependent) answer here as soon as the
/// deletes start landing, on top of the O(rows x subquery) cost.
#[test]
fn delete_in_subquery_over_the_target_table_itself() {
    for params_family in FAMILIES {
        let db = fixture();
        assert_eq!(
            run_ok(
                &db,
                "DELETE FROM s WHERE id IN (SELECT id FROM s WHERE n > 1)",
                params_family
            ),
            2,
            "[{}]",
            family(params_family)
        );
        assert_eq!(ids_in(&db, "s"), vec![10], "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** The exact SQL `src/protocol/mysql/translator.rs` emits for a
/// MySQL multi-table `DELETE t, tt FROM terms t INNER JOIN term_tax tt ON …`
/// (`translator.rs:653`/`:656`). WordPress and any MySQL client that uses
/// multi-table DELETE reaches this shape and nothing else.
///
/// The first assertion is an IN-TEST CONTROL: it runs the translator's inner
/// SELECT on its own. It must pass on the unfixed tree. If it ever fails, the
/// aliased-INNER-JOIN subquery is what is broken, not the DML predicate path,
/// and this test is reporting the wrong thing.
#[test]
fn mysql_multi_table_delete_translation_shape() {
    for params_family in FAMILIES {
        let db = mem_db();
        ddl(&db, "CREATE TABLE terms (term_id INT PRIMARY KEY, name TEXT)");
        ddl(&db, "CREATE TABLE term_tax (term_id INT PRIMARY KEY, taxonomy TEXT)");
        ddl(&db, "INSERT INTO terms VALUES (1, 'keep')");
        ddl(&db, "INSERT INTO terms VALUES (2, 'drop')");
        ddl(&db, "INSERT INTO term_tax VALUES (1, 'category')");
        ddl(&db, "INSERT INTO term_tax VALUES (2, 'gone')");

        const INNER: &str = "SELECT t.term_id FROM terms AS t INNER JOIN term_tax AS tt \
                             ON t.term_id = tt.term_id WHERE tt.taxonomy = 'gone'";

        // CONTROL — the translator's inner SELECT, standalone.
        let rows = db.query(INNER, &[]).expect("the translated inner SELECT must work");
        let ids: Vec<i64> = rows.iter().map(|t| int_of(&t.values[0])).collect();
        assert_eq!(ids, vec![2], "[{}] in-test control", family(params_family));

        // The translated statement itself.
        let sql = format!("DELETE FROM terms WHERE term_id IN ({INNER})");
        assert_eq!(run_ok(&db, &sql, params_family), 1, "[{}]", family(params_family));
        assert_eq!(ids_in(&db, "terms"), vec![1], "[{}]", family(params_family));
    }
}

/// ***UNFIXED*** Transaction visibility, PARAMS family (the PostgreSQL EXTENDED
/// protocol and the REST layer).
///
/// The sibling test above uses `execute_for_session`, which is the TEXT family
/// only — and a fix in one family says nothing about the other, since the two
/// arms resolve their transaction differently (`txn` threaded through
/// `execute_in_transaction_inner` vs `active_txn` resolved at
/// `src/lib.rs:16011-16018`). The subquery executor must be handed the PARAMS
/// arm's `active_txn` too.
#[test]
fn delete_in_subquery_sees_own_uncommitted_insert_params_family() {
    let db = fixture();
    ddl(&db, "INSERT INTO s VALUES (13, 4, 4)");

    let sid = session(&db, "params-writer");
    db.begin_transaction_for_session(sid).expect("BEGIN");
    db.execute_params_for_session(sid, "INSERT INTO p VALUES (4, 'alicia')", &[])
        .expect("params INSERT INTO p inside the transaction");
    let deleted = db
        .execute_params_for_session(
            sid,
            "DELETE FROM s WHERE pid IN (SELECT id FROM p WHERE login LIKE 'ali%')",
            &[],
        )
        .expect("params-family DELETE … IN (SELECT …) inside a transaction");
    assert_eq!(
        deleted, 2,
        "the params arm's subquery must see the statement's own staged INSERT"
    );
    db.commit_transaction_for_session(sid).expect("COMMIT");

    assert_eq!(ids_in(&db, "s"), vec![11, 12]);
}

/// ***UNFIXED*** Transaction isolation, PARAMS family — the 3fc8385 invariant.
/// Another session's uncommitted `p` row must stay invisible to this DELETE's
/// subquery. This one must keep passing once a transaction IS attached, which is
/// the whole point of "attach the STATEMENT'S OWN transaction and nothing else".
#[test]
fn delete_in_subquery_params_family_does_not_see_other_sessions_uncommitted_rows() {
    let db = fixture();
    ddl(&db, "INSERT INTO s VALUES (13, 4, 4)");

    let writer = session(&db, "writer");
    let deleter = session(&db, "params-deleter");

    db.begin_transaction_for_session(writer).expect("BEGIN");
    db.execute_for_session(writer, "INSERT INTO p VALUES (4, 'alicia')")
        .expect("uncommitted INSERT INTO p");

    let deleted = db
        .execute_params_for_session(
            deleter,
            "DELETE FROM s WHERE pid IN (SELECT id FROM p WHERE login LIKE 'ali%')",
            &[],
        )
        .expect("params-family DELETE … IN (SELECT …) from a second session");
    assert_eq!(
        deleted, 1,
        "only the child of the COMMITTED 'alice' may be deleted from the params family too"
    );

    db.rollback_transaction_for_session(writer).expect("ROLLBACK");
    assert_eq!(ids_in(&db, "s"), vec![11, 12, 13]);
}

/// ***UNFIXED*** `UPDATE … RETURNING` with an `IN (SELECT …)` predicate, both
/// families. The DELETE form is covered above; UPDATE + RETURNING is a separate
/// code path (the RETURNING projection is built from `updates`, after the
/// predicate) and the two families build it independently.
#[test]
fn update_where_in_subquery_returning() {
    const SQL: &str = "UPDATE s SET n = 99 WHERE pid IN (SELECT id FROM p WHERE login LIKE 'ali%') RETURNING id, n";

    let db = fixture();
    let (count, rows) = db.execute_returning(SQL).expect("text-family UPDATE … RETURNING");
    assert_eq!(count, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(int_of(&rows[0].values[0]), 10);
    assert_eq!(int_of(&rows[0].values[1]), 99);

    let db = fixture();
    let (count, rows) = db
        .execute_params_returning(SQL, &[])
        .expect("params-family UPDATE … RETURNING");
    assert_eq!(count, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(int_of(&rows[0].values[0]), 10);
    assert_eq!(int_of(&rows[0].values[1]), 99);
}

/// ***UNFIXED*** The `InSet` NULL rule at the exact boundary.
///
/// `materialize_subqueries` switches branch at `results.len() > 16`
/// (`src/sql/executor/mod.rs:1949`). This test runs the SAME statement either
/// side of that line and demands the SAME answer. It is the test that fails if
/// the fix plan's step-4 evaluator repair is skipped, or if
/// `in_subquery_hash_threshold` is later retuned without carrying the NULL rule
/// across — identical SQL must not change meaning because a table grew by one
/// row.
#[test]
fn not_in_null_rule_is_identical_either_side_of_the_inset_boundary() {
    // 16 subquery rows (15 values + NULL) → InList; 17 (16 + NULL) → InSet.
    for rows in [15_i64, 16] {
        let db = fixture();
        add_pn(&db, rows, true);
        let got = db
            .query("SELECT id FROM s WHERE pid NOT IN (SELECT v FROM pn)", &[])
            .unwrap_or_else(|e| panic!("NOT IN over {} subquery rows: {e}", rows + 1));
        assert!(
            got.is_empty(),
            "a NULL in a {}-row subquery result must make NOT IN select no rows; got {}",
            rows + 1,
            got.len()
        );
    }
}
