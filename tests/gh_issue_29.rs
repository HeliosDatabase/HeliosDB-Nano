//! GH#29 — confirmation harness for the five items reported fixed on main,
//! plus the C-collation documentation claim.
//!
//! The issue (filed against 3.58.1) asks the maintainers to CONFIRM five fixes
//! so they can be listed in the 4.31.0 release notes. This file is that
//! confirmation, expressed as permanent regression tests: every item is
//! exercised through BOTH executor families, and — where the issue claims the
//! defect is protocol-specific — through the exact entry point the wire handler
//! calls.
//!
//! The two executor families, and the entry points that reach them:
//!
//! | family | embedded call                          | inner                             | who sends this |
//! |--------|----------------------------------------|-----------------------------------|----------------|
//! | text   | `execute` / `query_with_columns`       | `execute_in_transaction_inner`    | psql SIMPLE query, MySQL wire, REPL |
//! | text   | `query_with_columns_for_session`       | ditto, with the session txn       | `src/protocol/postgres/handler.rs:1514` — THE simple-query SELECT path |
//! | text   | `execute_returning_for_session`        | ditto                             | `handler.rs:1552` — simple-query DML … RETURNING |
//! | params | `query_params` / `execute_params`      | `execute_plan_with_params_inner`  | PG EXTENDED protocol, REST/BaaS |
//! | params | `execute_params_returning_for_session` | ditto, with the session txn       | `handler_extended.rs:414` — what Prisma/node-pg send |
//!
//! Expected outcome on the tree at 4.31.1 (commit d7a6b3e):
//!
//! * item 1 (RETURNING escapes ROLLBACK) ......... PASS  (fixed; both halves)
//! * item 2 (table aliases, simple protocol) ..... PASS  (fixed) — EXCEPT the
//!   three NEW adjacent alias defects found while verifying it, ALL of which
//!   are expected to FAIL on this tree and are named so a failing run can
//!   never be misread as "the issue's item 2 is still open":
//!     - `alias_unquoted_mixed_case_qualifier_resolves`
//!       (`FROM t AS T1` + `T1.id` → `Column 't1.id' not found in schema`)
//!     - `alias_qualified_wildcard_selects_only_that_aliass_columns`
//!       (`SELECT a.*` over a self-join returns all four columns)
//!     - `adjacent_defect_wildcard_with_an_unknown_qualifier_is_silently_widened`
//!       (`SELECT nosuch.*` expands the whole row instead of erroring)
//!     - `alias_on_a_derived_table_qualifies_its_columns` (`FROM (…) AS s` +
//!       `s.x` → `Column 's.x' not found in schema`; lowest confidence of the
//!       four, and carries its own always-passing control)
//! * item 3 (CHAR(n) in DDL) ..................... PASS  (fixed)
//! * item 4 (information_schema.columns + WHERE) . PASS  (fixed)
//! * item 5 (`#>>` and friends) .................. PASS  (fixed), with
//!   `bare_question_mark_json_exists_is_still_unreachable` PINNING the one
//!   JSON operator that genuinely does not work.
//! * positive controls ........................... PASS on every tree.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::session::SessionId;
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

fn session(db: &EmbeddedDatabase, name: &str) -> SessionId {
    db.create_wire_session(name).expect("wire session")
}

/// Any integer-ish `Value` as `i64`.
fn as_i64(v: &Value) -> i64 {
    match *v {
        Value::Int2(n) => i64::from(n),
        Value::Int4(n) => i64::from(n),
        Value::Int8(n) => n,
        ref other => panic!("expected an integer, got {other:?}"),
    }
}

fn as_text(v: &Value) -> String {
    match *v {
        Value::String(ref s) => s.clone(),
        Value::Json(ref s) => s.clone(),
        ref other => panic!("expected text, got {other:?}"),
    }
}

fn scalar_i64(rows: &[Tuple]) -> i64 {
    as_i64(
        rows.first()
            .and_then(|r| r.values.first())
            .unwrap_or_else(|| panic!("expected one row with one column, got {rows:?}")),
    )
}

/// `SELECT id FROM <table>` on the text family, sorted.
fn ids_text_family(db: &EmbeddedDatabase, table: &str) -> Vec<i64> {
    let rows = db.query(&format!("SELECT id FROM {table}"), &[]).expect("select id");
    let mut ids: Vec<i64> = rows.iter().map(|r| as_i64(&r.values[0])).collect();
    ids.sort_unstable();
    ids
}

/// `SELECT id FROM <table>` as the given SESSION sees it (row-returning on
/// purpose: `COUNT(*)` can be answered from the eagerly-maintained PK ART
/// index and is therefore not a witness for row visibility).
fn ids_for_session(db: &EmbeddedDatabase, sid: SessionId, table: &str) -> Vec<i64> {
    let (rows, _cols) = db
        .query_with_columns_for_session(sid, &format!("SELECT id FROM {table}"))
        .expect("select id");
    let mut ids: Vec<i64> = rows.iter().map(|r| as_i64(&r.values[0])).collect();
    ids.sort_unstable();
    ids
}

// ===========================================================================
// POSITIVE CONTROLS — these must pass on EVERY tree, fixed or unfixed.
// A file whose every test passes vacuously is worse than no file at all;
// these prove the harness itself runs and the assertions have teeth.
// ===========================================================================

#[test]
fn control_harness_runs_on_both_executor_families() {
    let db = mem_db();
    db.execute("CREATE TABLE ctrl (id INT PRIMARY KEY, v TEXT)")
        .expect("create");
    db.execute("INSERT INTO ctrl VALUES (1, 'a')").expect("text insert");
    db.execute_params(
        "INSERT INTO ctrl VALUES ($1, $2)",
        &[Value::Int4(2), Value::String("b".into())],
    )
    .expect("params insert");

    assert_eq!(ids_text_family(&db, "ctrl"), vec![1, 2], "text family sees both rows");

    let rows = db
        .query_params("SELECT id FROM ctrl ORDER BY id", &[])
        .expect("params select");
    let ids: Vec<i64> = rows.iter().map(|r| as_i64(&r.values[0])).collect();
    assert_eq!(ids, vec![1, 2], "params family sees both rows");
}

#[test]
fn control_a_nonexistent_column_still_errors() {
    // Teeth for item 2: the resolver really does reject an unknown column, so a
    // passing alias test is not "everything resolves to something".
    let db = mem_db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").expect("create");
    assert!(
        db.query(r#"SELECT "t1"."nope" FROM t AS "t1""#, &[]).is_err(),
        "an unknown column must error, otherwise the alias tests prove nothing"
    );
    assert!(
        db.query_params(r#"SELECT "t1"."nope" FROM t AS "t1""#, &[]).is_err(),
        "…on the params family too"
    );
}

// ===========================================================================
// ITEM 1 — `RETURNING` must not escape `ROLLBACK`
//
// The issue's table says this is fixed; comment 1 narrows it: only the
// unparameterised / simple-protocol form was fixed at the time of writing, and
// the parameterised extended-protocol form was split out as #30.
//
// On this tree BOTH halves are fixed:
//   * text family   — `execute_returning_for_session`      (lib.rs:18717)
//   * params family — `execute_params_returning_for_session` (lib.rs:18800),
//     which plans the statement at lib.rs:18846 and passes `Some(&txn)` at
//     lib.rs:18853 instead of delegating to the session-less entry point.
//     `handler_extended.rs:294-299` classifies `INSERT … RETURNING` as NOT
//     row-returning, so Execute reaches exactly that function (line 414) and
//     not `query_params_for_session` — verified, so these session-level
//     assertions really do stand in for the wire path.
//
// tests/prisma_p0_extended_returning_txn.rs already pins the parameterised
// half in depth. What is asserted here is the issue's LITERAL reproducer
// (`INSERT INTO t VALUES (1,'a') RETURNING id` — no column list) on both
// families. NOT covered here (deliberately, and NOT claimed): the embedded
// global-transaction slot (`db.begin_transaction()` + `db.execute_returning`)
// and the CTE-wrapped form `WITH x AS (INSERT … RETURNING id) SELECT * FROM x`,
// which handler_extended routes to `query_params_for_session` instead.
// ===========================================================================

#[test]
fn item1_text_family_returning_is_undone_by_rollback() {
    let db = mem_db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .expect("create");
    let sid = session(&db, "psql");

    db.begin_transaction_for_session(sid).expect("begin");
    let (affected, rows) = db
        .execute_returning_for_session(sid, "INSERT INTO t VALUES (1, 'a') RETURNING id")
        .expect("insert returning");
    assert_eq!(affected, 1, "one row inserted");
    assert_eq!(as_i64(&rows[0].values[0]), 1, "RETURNING id yields 1");

    db.rollback_transaction_for_session(sid).expect("rollback");
    assert_eq!(
        ids_for_session(&db, sid, "t"),
        Vec::<i64>::new(),
        "*** the issue's item 1: INSERT … RETURNING survived ROLLBACK on the simple-query path ***"
    );
    db.destroy_session(sid).expect("destroy");
}

#[test]
fn item1_params_family_returning_is_undone_by_rollback() {
    let db = mem_db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .expect("create");
    let sid = session(&db, "prisma");

    db.begin_transaction_for_session(sid).expect("begin");
    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO t VALUES ($1, $2) RETURNING id",
        &[Value::Int4(1), Value::String("a".into())],
    )
    .expect("insert returning");

    db.rollback_transaction_for_session(sid).expect("rollback");
    assert_eq!(
        ids_for_session(&db, sid, "t"),
        Vec::<i64>::new(),
        "*** GH#30: a parameterized INSERT … RETURNING survived ROLLBACK on the extended protocol ***"
    );
    db.destroy_session(sid).expect("destroy");
}

#[test]
fn item1_update_and_delete_returning_are_undone_by_rollback_on_both_families() {
    for params_family in [false, true] {
        let db = mem_db();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .expect("create");
        db.execute("INSERT INTO t VALUES (1, 'a')").expect("seed");
        let sid = session(&db, "s");

        db.begin_transaction_for_session(sid).expect("begin");
        if params_family {
            db.execute_params_returning_for_session(
                sid,
                "UPDATE t SET v = $1 WHERE id = $2 RETURNING id",
                &[Value::String("b".into()), Value::Int4(1)],
            )
            .expect("update returning");
        } else {
            db.execute_returning_for_session(sid, "UPDATE t SET v = 'b' WHERE id = 1 RETURNING id")
                .expect("update returning");
        }
        db.rollback_transaction_for_session(sid).expect("rollback");

        let (rows, _c) = db
            .query_with_columns_for_session(sid, "SELECT v FROM t WHERE id = 1")
            .expect("read back");
        assert_eq!(
            as_text(&rows[0].values[0]),
            "a",
            "UPDATE … RETURNING must be undone by ROLLBACK (params_family={params_family})"
        );

        db.begin_transaction_for_session(sid).expect("begin 2");
        if params_family {
            db.execute_params_returning_for_session(sid, "DELETE FROM t WHERE id = $1 RETURNING id", &[Value::Int4(1)])
                .expect("delete returning");
        } else {
            db.execute_returning_for_session(sid, "DELETE FROM t WHERE id = 1 RETURNING id")
                .expect("delete returning");
        }
        db.rollback_transaction_for_session(sid).expect("rollback 2");
        assert_eq!(
            ids_for_session(&db, sid, "t"),
            vec![1],
            "DELETE … RETURNING must be undone by ROLLBACK (params_family={params_family})"
        );
        db.destroy_session(sid).expect("destroy");
    }
}

#[test]
fn item1_returning_still_commits_when_the_transaction_commits() {
    // The other half of the invariant: the fix must not turn the write into a
    // no-op. Passes on the unfixed tree too (it autocommitted) — deliberately.
    let db = mem_db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .expect("create");
    let sid = session(&db, "s");
    db.begin_transaction_for_session(sid).expect("begin");
    db.execute_params_returning_for_session(
        sid,
        "INSERT INTO t VALUES ($1, $2) RETURNING id",
        &[Value::Int4(7), Value::String("g".into())],
    )
    .expect("insert returning");
    db.commit_transaction_for_session(sid).expect("commit");
    assert_eq!(
        ids_for_session(&db, sid, "t"),
        vec![7],
        "a committed RETURNING must persist"
    );
    db.destroy_session(sid).expect("destroy");
}

// ===========================================================================
// ITEM 2 — `SELECT "t1"."id" FROM "public"."t" AS "t1"`
//
// Reported as failing on the SIMPLE protocol only ("Column 't1.id' not found")
// and working on the extended protocol.
//
// Mechanism on this tree: `Planner::table_factor_to_plan` keeps the alias on
// `LogicalPlan::Scan` (planner.rs:3024-3032); `scan::handle_scan` stamps every
// column's `source_table` with the alias and `source_table_name` with the real
// table (scan.rs:2244-2253); `Schema::get_qualified_column_index` (types.rs:659)
// matches EITHER. Nothing on that route is protocol-specific — the simple and
// extended paths share the planner, the optimizer and the executor — so this is
// asserted on all four entry points.
// ===========================================================================

fn alias_fixture() -> EmbeddedDatabase {
    let db = mem_db();
    db.execute(r#"CREATE TABLE "t" ("id" INT PRIMARY KEY, "v" TEXT)"#)
        .expect("create");
    db.execute("INSERT INTO t VALUES (1, 'a')").expect("seed 1");
    db.execute("INSERT INTO t VALUES (2, 'b')").expect("seed 2");
    db
}

#[test]
fn item2_quoted_alias_resolves_on_the_text_family() {
    let db = alias_fixture();
    // The issue's literal statement, no ORDER BY (sorted in Rust) so this test
    // fails for exactly one reason: the alias qualifier not resolving.
    let sql = r#"SELECT "t1"."id" FROM "public"."t" AS "t1""#;
    let (rows, cols) = db.query_with_columns(sql).expect(
        "*** the issue's item 2: a quoted table alias failed to resolve on the text \
         (simple-query) family ***",
    );
    let mut ids: Vec<i64> = rows.iter().map(|r| as_i64(&r.values[0])).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2], "alias-qualified projection must return both rows");
    assert_eq!(cols, vec!["id".to_string()], "the column is named `id`, not `t1.id`");
}

/// A qualified ORDER BY key ABOVE the projection is resolved by a different
/// rule than the projection itself, so it gets its own test rather than
/// riding along on the one above.
#[test]
fn item2_alias_qualified_order_by_resolves_on_both_families() {
    let db = alias_fixture();
    let sql = r#"SELECT "t1"."id" FROM "public"."t" AS "t1" ORDER BY "t1"."id" DESC"#;
    let text = db.query(sql, &[]).expect("alias-qualified ORDER BY, text family");
    let ids: Vec<i64> = text.iter().map(|r| as_i64(&r.values[0])).collect();
    assert_eq!(ids, vec![2, 1], "ORDER BY \"t1\".\"id\" DESC");
    let params = db
        .query_params(sql, &[])
        .expect("alias-qualified ORDER BY, params family");
    let ids: Vec<i64> = params.iter().map(|r| as_i64(&r.values[0])).collect();
    assert_eq!(ids, vec![2, 1], "…on the params family");
}

#[test]
fn item2_quoted_alias_resolves_on_the_simple_query_session_entry_point() {
    // `query_with_columns_for_session` is LITERALLY what
    // src/protocol/postgres/handler.rs:1514 calls for a simple-query SELECT.
    let db = alias_fixture();
    let sid = session(&db, "psql");
    let sql = r#"SELECT "t1"."id", "t1"."v" FROM "public"."t" AS "t1" WHERE "t1"."id" = 2"#;
    let (rows, _cols) = db
        .query_with_columns_for_session(sid, sql)
        .expect("*** item 2 on the exact simple-query entry point ***");
    assert_eq!(rows.len(), 1, "one row matches");
    assert_eq!(as_i64(&rows[0].values[0]), 2);
    assert_eq!(as_text(&rows[0].values[1]), "b");

    // …and inside an explicit transaction, which takes the OTHER branch of
    // `query_with_columns_for_session` (the in-transaction planner).
    db.begin_transaction_for_session(sid).expect("begin");
    let (rows, _cols) = db
        .query_with_columns_for_session(sid, sql)
        .expect("alias must resolve inside a session transaction too");
    assert_eq!(rows.len(), 1, "one row matches inside the transaction");
    db.rollback_transaction_for_session(sid).expect("rollback");
    db.destroy_session(sid).expect("destroy");
}

#[test]
fn item2_quoted_alias_resolves_on_the_params_family() {
    let db = alias_fixture();
    let rows = db
        .query_params(
            r#"SELECT "t1"."id" FROM "public"."t" AS "t1" WHERE "t1"."id" = $1"#,
            &[Value::Int4(1)],
        )
        .expect("alias must resolve on the extended-protocol family");
    assert_eq!(rows.len(), 1);
    assert_eq!(as_i64(&rows[0].values[0]), 1);
}

#[test]
fn item2_alias_spellings_matrix() {
    // Every spelling of the same query, both families. Unquoted lowercase
    // alias, quoted alias, alias without AS, schema-qualified and bare table,
    // and the 3-part `"public"."t"."id"` form Prisma emits.
    let db = alias_fixture();
    let queries = [
        r#"SELECT "t1"."id" FROM "public"."t" AS "t1""#,
        r#"SELECT t1.id FROM public.t AS t1"#,
        r#"SELECT t1.id FROM t t1"#,
        r#"SELECT "t1"."id" FROM "t" AS "t1""#,
        r#"SELECT "public"."t"."id" FROM "public"."t""#,
        // Nano is LENIENT here: it also resolves the real table name while an alias is
        // in scope (PostgreSQL rejects this with 42P01). Pinned as current behaviour.
        r#"SELECT "t"."id" FROM "public"."t" AS "t1""#,
    ];
    for sql in queries {
        let text = db
            .query(sql, &[])
            .unwrap_or_else(|e| panic!("text family failed for `{sql}`: {e}"));
        assert_eq!(text.len(), 2, "text family row count for `{sql}`");
        let params = db
            .query_params(sql, &[])
            .unwrap_or_else(|e| panic!("params family failed for `{sql}`: {e}"));
        assert_eq!(params.len(), 2, "params family row count for `{sql}`");
    }
}

#[test]
fn item2_alias_qualified_predicate_and_join_resolve() {
    // A self-join is the shape that made aliases load-bearing in the first
    // place; both families.
    let db = alias_fixture();
    let sql = r#"SELECT "a"."id", "b"."id" FROM "public"."t" AS "a"
                 JOIN "public"."t" AS "b" ON "a"."id" = "b"."id"
                 WHERE "a"."id" = 1"#;
    assert_eq!(db.query(sql, &[]).expect("text self-join").len(), 1);
    assert_eq!(db.query_params(sql, &[]).expect("params self-join").len(), 1);
}

/// ***EXPECTED TO FAIL on 4.31.1 — a NEW defect found while confirming item 2,
/// not part of the issue's list.***
///
/// PostgreSQL folds an UNQUOTED identifier to lower case, so `FROM t AS T1`
/// declares the alias `t1` and `T1.id` refers to it. Nano stores the alias RAW
/// (`planner.rs:3024`, `alias.name.value.clone()` — no `normalize_ident`) while
/// the column qualifier IS normalised (`planner.rs:4211`,
/// `Self::normalize_ident(...)` → `"t1"`). `Schema::get_qualified_column_index`
/// (types.rs:662) compares the two with `==`, so the reference misses and the
/// statement fails with `Column 't1.id' not found in schema`.
///
/// Unfixed value: `Err(QueryExecution("Column 't1.id' not found in schema"))`
/// on BOTH families. Fix: normalise the alias at the three sites in
/// `table_factor_to_plan` that read `alias.name.value`.
#[test]
fn alias_unquoted_mixed_case_qualifier_resolves() {
    let db = alias_fixture();
    let sql = "SELECT T1.id FROM t AS T1";
    let rows = db
        .query(sql, &[])
        .expect("an UNQUOTED alias must fold to lower case, as in PostgreSQL (text family)");
    assert_eq!(rows.len(), 2);
    let rows = db.query_params(sql, &[]).expect("…and on the params family");
    assert_eq!(rows.len(), 2);
}

/// ***EXPECTED TO FAIL on 4.31.1 — a SECOND adjacent alias defect, found by the
/// adversarial review of this file, not part of the issue's list.***
///
/// `SELECT "a".* FROM t AS "a" JOIN t AS "b" ON …` expands to ALL FOUR columns
/// (both sides of the self-join) instead of the two belonging to `a`.
///
/// Mechanism: `SelectItem::QualifiedWildcard` (src/sql/planner.rs:3263-3288)
/// resolves the qualifier against the PLAN-time input schema and matches ONLY
/// `column.source_table_name` (planner.rs:3276) — never `source_table`, which is
/// where the ALIAS lives. Plan-time Scan schemas come from
/// `Catalog::get_table_schema`, which stamps `source_table_name = <real table>`
/// (src/storage/catalog.rs:1017-1021) and leaves `source_table` unset, so for a
/// self-join every column reports `source_table_name == "t"` and the qualifier
/// `a` matches nothing. The `if !matched` fallback (planner.rs:3288-3296) then
/// expands EVERY column of the join — silently returning the wrong result set
/// rather than erroring.
///
/// PostgreSQL returns exactly `a`'s two columns. Fix together with the alias
/// normalisation: match `source_table` first (the alias), fall back to
/// `source_table_name`, and make a qualifier that matches NOTHING an error
/// (42P01) instead of a silent full expansion — the fail-closed choice, since
/// today a typo'd qualifier returns a wider row than the client asked for.
#[test]
fn alias_qualified_wildcard_selects_only_that_aliass_columns() {
    let db = alias_fixture();
    let sql = r#"SELECT "a".* FROM "t" AS "a" JOIN "t" AS "b" ON "a"."id" = "b"."id""#;
    let (rows, cols) = db
        .query_with_columns(sql)
        .expect("an alias-qualified wildcard must plan");
    assert_eq!(
        cols.len(),
        2,
        "`a.*` over a self-join must project ONLY a's columns, got {cols:?}"
    );
    assert_eq!(rows[0].values.len(), 2, "…and the rows must be that wide too");
    // Params family: same statement, same expectation.
    let params = db.query_params(sql, &[]).expect("params family plans it too");
    assert_eq!(
        params[0].values.len(),
        2,
        "`a.*` must project 2 columns on the params family as well"
    );
}

/// ***ALSO EXPECTED TO FAIL on 4.31.1*** — the fail-open half of the defect
/// above, and the reason the fix must reject rather than widen: a qualifier
/// that names NOTHING in scope is not an error. `SELECT "nosuch".* FROM t AS a`
/// falls into the `if !matched` branch (src/sql/planner.rs:3288-3296) and
/// expands EVERY column, so a typo'd or stale qualifier returns a full row
/// instead of `42P01`. Deliberately NOT named `control_*`: it does not pass on
/// the current tree.
#[test]
fn adjacent_defect_wildcard_with_an_unknown_qualifier_is_silently_widened() {
    let db = alias_fixture();
    let out = db.query_with_columns(r#"SELECT "nosuch".* FROM "t" AS "a""#);
    match out {
        Err(_) => {} // correct: 42P01-shaped rejection
        Ok((_rows, cols)) => panic!(
            "`nosuch.*` must not silently expand to the whole row; got columns {cols:?}              (src/sql/planner.rs:3288 `if !matched` fallback)"
        ),
    }
}

/// ***EXPECTED TO FAIL on 4.31.1 — a THIRD adjacent alias defect.*** Lower
/// confidence than the two above (derived from the code, not from a run), so it
/// is named and commented separately: a failure here localises to the DERIVED
/// (sub-select) alias, nothing else.
///
/// `TableFactor::Derived` DISCARDS the alias outright —
/// `let _ = alias; // Alias is used for column qualification but schema already
/// has names` (src/sql/planner.rs:3046) — and `ProjectOperator` builds its
/// output columns with `source_table: None` / `source_table_name: None`
/// (src/sql/executor/project.rs:60-65). So nothing in the sub-select's output
/// schema carries `s`, and `Schema::get_qualified_column_index(Some("s"), "x")`
/// (src/types.rs:662) misses. Neither `bind_expr_columns`
/// (src/sql/evaluator.rs:127-132) nor `direct_project_column_indices`
/// (src/sql/executor/project.rs:212) has an unqualified fallback, so the
/// statement fails with `Column 's.x' not found in schema`.
///
/// PostgreSQL requires the alias on a sub-select in FROM and resolves `s.x`
/// against it. Prisma, Drizzle and Knex all emit this shape.
///
/// Fix: carry the derived alias into the sub-plan's output schema (stamp
/// `source_table = alias` on the Project's columns, the same thing
/// `handle_scan` does for a base table) rather than dropping it.
#[test]
fn alias_on_a_derived_table_qualifies_its_columns() {
    let db = alias_fixture();
    // No ORDER BY: sorted in Rust so this test can fail for exactly one reason
    // — the derived alias not qualifying the column.
    let sql = r#"SELECT "s"."x" FROM (SELECT "id" AS "x" FROM "t") AS "s""#;
    let rows = db
        .query(sql, &[])
        .expect("a sub-select's alias must qualify its output columns (text family)");
    let mut ids: Vec<i64> = rows.iter().map(|r| as_i64(&r.values[0])).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2], "derived-table alias, text family");

    let rows = db.query_params(sql, &[]).expect("…and on the params family");
    assert_eq!(rows.len(), 2, "derived-table alias, params family");

    // Control that passes on ANY tree: the same sub-select WITHOUT the
    // qualifier resolves, proving the sub-select itself is fine and the failure
    // above is specifically the alias qualifier.
    let plain = r#"SELECT "x" FROM (SELECT "id" AS "x" FROM "t") AS "s""#;
    assert_eq!(
        db.query(plain, &[]).expect("unqualified derived projection").len(),
        2,
        "the sub-select itself works; only the alias qualifier is at issue"
    );
}

// ===========================================================================
// ITEM 3 — `CHAR(n)` accepted in DDL
//
// `Planner::sql_data_type_to_data_type` handles `SqlDataType::Char` /
// `Character` (planner.rs:5993-5992..6002), defaulting an omitted length to 1
// per the SQL standard, and `Evaluator::cast_value` blank-pads to the declared
// width (evaluator.rs:5626). Before that arm existed, CREATE TABLE failed with
// "Data type not yet supported: Char(Some(IntegerLength { length: 32 }))"
// (the generic arm is still at planner.rs:6152).
//
// NOTE: no test under tests/ covered `CHAR(n)` in DDL before this file.
// ===========================================================================

#[test]
fn item3_char_n_is_accepted_in_ddl_on_both_families() {
    for (i, params_family) in [false, true].into_iter().enumerate() {
        let db = mem_db();
        let table = format!("c{i}");
        let ddl = format!("CREATE TABLE {table} (id INT PRIMARY KEY, v CHAR(32))");
        // NB: bound to a `let` on purpose. A block-like `if … {} else {}` in
        // STATEMENT position cannot carry a trailing `.method()` — rustc parses
        // the block as a complete statement and then rejects the `.`.
        let ddl_result = if params_family {
            db.execute_params(&ddl, &[])
        } else {
            db.execute(&ddl)
        };
        ddl_result.unwrap_or_else(|e| {
            panic!("*** the issue's item 3: CHAR(32) rejected in DDL (params_family={params_family}): {e} ***")
        });

        if params_family {
            db.execute_params(
                &format!("INSERT INTO {table} VALUES ($1, $2)"),
                &[Value::Int4(1), Value::String("abc".into())],
            )
            .expect("insert into CHAR(32)");
        } else {
            db.execute(&format!("INSERT INTO {table} VALUES (1, 'abc')"))
                .expect("insert into CHAR(32)");
        }

        let rows = db
            .query(&format!("SELECT v FROM {table} WHERE id = 1"), &[])
            .expect("read back");
        assert_eq!(
            as_text(&rows[0].values[0]).trim_end(),
            "abc",
            "the CHAR(32) value must round-trip (blank padding tolerated)"
        );
    }
}

#[test]
fn item3_char_spellings_matrix() {
    let db = mem_db();
    // Every spelling sqlparser produces for the fixed-length character type.
    for (i, ty) in ["CHAR(32)", "CHARACTER(20)", "CHAR", "CHARACTER"]
        .into_iter()
        .enumerate()
    {
        db.execute(&format!("CREATE TABLE ch{i} (id INT PRIMARY KEY, v {ty})"))
            .unwrap_or_else(|e| panic!("`{ty}` must be accepted in DDL: {e}"));
    }
    // The bounded/unbounded VARCHAR spellings must keep working (control).
    db.execute("CREATE TABLE vc (id INT PRIMARY KEY, a VARCHAR(10), b CHARACTER VARYING(10), c TEXT)")
        .expect("VARCHAR spellings must keep working");
}

#[test]
fn item3_char_column_is_introspectable() {
    // Interface coverage: the type must also be visible through introspection,
    // not merely accepted by the parser.
    let db = mem_db();
    db.execute("CREATE TABLE ch (id INT PRIMARY KEY, v CHAR(32))")
        .expect("create");
    let rows = db
        .query(
            "SELECT data_type, udt_name FROM information_schema.columns \
             WHERE table_name = 'ch' AND column_name = 'v'",
            &[],
        )
        .expect("information_schema must describe the CHAR column");
    assert_eq!(rows.len(), 1, "exactly one row describes ch.v");
    let udt = as_text(&rows[0].values[1]).to_lowercase();
    assert!(
        udt.contains("char"),
        "udt_name for CHAR(32) should mention char, got {udt:?}"
    );
}

// ===========================================================================
// ITEM 4 — `information_schema.columns` with a WHERE filter
//
// Reported: the filtered query returns nothing while the unfiltered one lists
// the columns. On this tree the wire's substring router explicitly DEFERS every
// `information_schema.columns` query to the planner
// (`src/protocol/postgres/catalog.rs:136` → `return Ok(None)`), the view is
// served by the registry (`src/sql/phase3/system_views.rs:4477`), and the
// COUNT(*) fast path declines for registry-backed views
// (`src/sql/executor/mod.rs:3880` via `is_registry_backed_system_view`) so it
// cannot answer 0 from a storage row-counter that knows nothing about the view.
//
// The issue's literal repro uses a MIXED-CASE, quoted table name (`Account`),
// which is what Prisma creates — that is asserted here, not a lower-case stand-in.
// ===========================================================================

fn account_fixture() -> EmbeddedDatabase {
    let db = mem_db();
    db.execute(r#"CREATE TABLE "Account" ("id" INT PRIMARY KEY, "email" TEXT NOT NULL, "createdAt" TIMESTAMP)"#)
        .expect("create Account");
    db.execute(r#"CREATE TABLE "Session" ("id" INT PRIMARY KEY)"#)
        .expect("create Session");
    db
}

#[test]
fn item4_information_schema_columns_count_with_where_filter() {
    let db = account_fixture();
    let sql = "SELECT count(*) FROM information_schema.columns WHERE table_name = 'Account'";

    let text = db.query(sql, &[]).expect("text family");
    assert_eq!(
        scalar_i64(&text),
        3,
        "*** the issue's item 4: a filtered information_schema.columns count returned the wrong \
         number of rows on the text family ***"
    );

    let params = db.query_params(sql, &[]).expect("params family");
    assert_eq!(scalar_i64(&params), 3, "…and on the params family");
}

#[test]
fn item4_information_schema_columns_rows_with_where_filter() {
    let db = account_fixture();
    let sql = "SELECT column_name FROM information_schema.columns \
               WHERE table_name = 'Account' ORDER BY ordinal_position";

    for (family, rows) in [
        ("text", db.query(sql, &[]).expect("text family")),
        ("params", db.query_params(sql, &[]).expect("params family")),
    ] {
        let names: Vec<String> = rows.iter().map(|r| as_text(&r.values[0])).collect();
        assert_eq!(
            names,
            vec!["id".to_string(), "email".to_string(), "createdAt".to_string()],
            "filtered information_schema.columns must list Account's columns ({family} family)"
        );
    }
}

#[test]
fn item4_filtered_result_is_a_subset_of_the_unfiltered_one() {
    // The issue's exact contrast: unfiltered lists the columns, filtered
    // returns nothing. Assert the relationship, not just each half.
    let db = account_fixture();
    let all = db
        .query("SELECT table_name, column_name FROM information_schema.columns", &[])
        .expect("unfiltered");
    let account_rows = all.iter().filter(|r| as_text(&r.values[0]) == "Account").count();
    assert_eq!(account_rows, 3, "the UNFILTERED view must list Account's 3 columns");

    let filtered = db
        .query(
            "SELECT table_name, column_name FROM information_schema.columns WHERE table_name = 'Account'",
            &[],
        )
        .expect("filtered");
    assert_eq!(
        filtered.len(),
        account_rows,
        "*** filtering must not drop rows the unfiltered query returns (predicate pushdown over a \
         synthesised view) ***"
    );
}

#[test]
fn item4_filter_predicate_shapes_matrix() {
    let db = account_fixture();
    let cases: [(&str, i64); 6] = [
        ("WHERE table_name = 'Account'", 3),
        ("WHERE table_name='Account'", 3), // no spaces around `=`
        ("WHERE table_schema = 'public' AND table_name = 'Account'", 3),
        ("WHERE table_name IN ('Account','Session')", 4),
        ("WHERE table_name = 'account'", 0),     // case-sensitive, as in PostgreSQL
        ("WHERE table_name = 'NoSuchTable'", 0), // negative control
    ];
    for (clause, want) in cases {
        let sql = format!("SELECT count(*) FROM information_schema.columns {clause}");
        assert_eq!(
            scalar_i64(&db.query(&sql, &[]).expect("text family")),
            want,
            "text family: `{clause}`"
        );
        assert_eq!(
            scalar_i64(&db.query_params(&sql, &[]).expect("params family")),
            want,
            "params family: `{clause}`"
        );
    }
}

#[test]
fn item4_information_schema_tables_filter_too() {
    // The sibling view an ORM hits in the same introspection round.
    let db = account_fixture();
    let sql = "SELECT count(*) FROM information_schema.tables WHERE table_name = 'Account'";
    assert_eq!(scalar_i64(&db.query(sql, &[]).expect("text")), 1, "text family");
    assert_eq!(
        scalar_i64(&db.query_params(sql, &[]).expect("params")),
        1,
        "params family"
    );
}

// ===========================================================================
// ITEM 5 — the JSON operators
//
// `#>` / `#>>` are mapped at `src/sql/planner.rs:5008-5009` and evaluated at
// `src/sql/evaluator.rs:5232` (`json_path_get_op`). The full set lives at
// planner.rs:4984-4990 and evaluator.rs:3449-3457.
//
// Support, verified in this file:
//   ->   JsonGet          OK      ->>  JsonGetText     OK
//   #>   JsonPathGet      OK      #>>  JsonPathGetText OK
//   @>   JsonContains     OK      <@   JsonContainedBy OK
//   ?|   JsonExistsAny    OK      ?&   JsonExistsAll   OK
//   ?    JsonExists       NOT REACHABLE FROM SQL — see the pinned test below.
// ===========================================================================

fn json_fixture() -> EmbeddedDatabase {
    let db = mem_db();
    db.execute("CREATE TABLE j (id INT PRIMARY KEY, payload JSONB)")
        .expect("create");
    db.execute(r#"INSERT INTO j VALUES (1, '{"a":1,"b":{"c":"deep"},"arr":["x","y"]}')"#)
        .expect("seed");
    db
}

#[test]
fn item5_hash_long_arrow_extracts_a_path_as_text() {
    let db = json_fixture();
    let sql = "SELECT payload #>> '{b,c}' FROM j WHERE id = 1";
    let text = db.query(sql, &[]).expect(
        "*** the issue's item 5: `#>>` failed with \"Binary operator not yet supported: \
         HashLongArrow\" (text family) ***",
    );
    assert_eq!(as_text(&text[0].values[0]), "deep", "`#>>` yields BARE text");

    let params = db.query_params(sql, &[]).expect("`#>>` on the params family");
    assert_eq!(as_text(&params[0].values[0]), "deep", "`#>>` on the params family");
}

#[test]
fn item5_hash_arrow_extracts_a_path_as_json() {
    let db = json_fixture();
    let sql = "SELECT payload #> '{b,c}' FROM j WHERE id = 1";
    // `#>` keeps JSON typing, so a string value stays QUOTED — the entire
    // reason `#>>` exists as a separate operator.
    let text = as_text(&db.query(sql, &[]).expect("`#>` text family")[0].values[0]);
    assert_eq!(text, "\"deep\"", "`#>` yields JSON, not bare text");
    let params = as_text(&db.query_params(sql, &[]).expect("`#>` params family")[0].values[0]);
    assert_eq!(params, "\"deep\"", "`#>` on the params family");
}

#[test]
fn item5_json_operator_support_matrix() {
    let db = json_fixture();
    // (SQL fragment, expected row count when used as a WHERE predicate)
    let predicates: [(&str, usize); 8] = [
        ("payload -> 'a' IS NOT NULL", 1),
        ("payload ->> 'a' = '1'", 1),
        ("payload #> '{b,c}' IS NOT NULL", 1),
        ("payload #>> '{b,c}' = 'deep'", 1),
        (r#"payload @> '{"a":1}'"#, 1),
        (r#"payload <@ '{"a":1,"b":{"c":"deep"},"arr":["x","y"],"z":9}'"#, 1),
        ("payload ?| ARRAY['a','nope']", 1),
        ("payload ?& ARRAY['a','b']", 1),
    ];
    for (pred, want) in predicates {
        let sql = format!("SELECT id FROM j WHERE {pred}");
        let text = db
            .query(&sql, &[])
            .unwrap_or_else(|e| panic!("text family rejected `{pred}`: {e}"));
        assert_eq!(text.len(), want, "text family row count for `{pred}`");
        let params = db
            .query_params(&sql, &[])
            .unwrap_or_else(|e| panic!("params family rejected `{pred}`: {e}"));
        assert_eq!(params.len(), want, "params family row count for `{pred}`");
    }
    // Negative control: a predicate that must NOT match, so the matrix above is
    // not "every JSON predicate is true".
    assert_eq!(
        db.query("SELECT id FROM j WHERE payload ?& ARRAY['a','missing']", &[])
            .expect("?& negative")
            .len(),
        0,
        "`?&` must require ALL keys"
    );
}

/// PINS A KNOWN LIMITATION, deliberately: the bare `?` key-existence operator
/// is NOT reachable from SQL.
///
/// `sqlite_compat::rewrite_question_placeholders` (src/sql/sqlite_compat.rs:182)
/// runs on EVERY statement and rewrites every bare `?` into `$N` for SQLite
/// placeholder compatibility. `?|` and `?&` are exempted there (a `$N` can never
/// be followed by `|`/`&`), but a lone `?` is genuinely ambiguous and the
/// placeholder wins. The operator itself is implemented
/// (`BinaryOperator::JsonExists`, evaluator.rs:3453) and is unit-tested directly
/// against the evaluator (evaluator.rs ~8524).
///
/// If this test ever starts FAILING, `?` became reachable — update
/// `docs/llms.txt` ("SQL dialect notes") and this comment together.
#[test]
fn bare_question_mark_json_exists_is_still_unreachable() {
    let db = json_fixture();
    assert!(
        db.query("SELECT id FROM j WHERE payload ? 'a'", &[]).is_err(),
        "a bare `?` is rewritten to a positional placeholder; document the `?| ARRAY['a']` \
         workaround rather than claiming `?` works"
    );
    // The documented workaround must work.
    assert_eq!(
        db.query("SELECT id FROM j WHERE payload ?| ARRAY['a']", &[])
            .expect("the single-key workaround")
            .len(),
        1,
        "`?| ARRAY['key']` is the supported spelling of `? 'key'`"
    );
}

// ===========================================================================
// DOC CLAIM — TEXT ordering is C (byte-order) collation, by design.
//
// `compare_values` (src/sql/executor/mod.rs:5239) and the evaluator's
// comparison (evaluator.rs:3702) both use Rust `String::cmp`, i.e. byte order.
// This test PINS the documented behaviour so the note we are adding to
// docs/llms.txt cannot silently become false.
// ===========================================================================

#[test]
fn doc_text_ordering_is_byte_order_c_collation() {
    let db = mem_db();
    db.execute("CREATE TABLE w (id INT PRIMARY KEY, s TEXT)")
        .expect("create");
    for (i, s) in ["Zebra", "apple", "Apple", "Ápple", "banana"].iter().enumerate() {
        db.execute_params(
            "INSERT INTO w VALUES ($1, $2)",
            &[Value::Int4(i as i32), Value::String((*s).into())],
        )
        .expect("seed");
    }
    let rows = db.query("SELECT s FROM w ORDER BY s", &[]).expect("order by text");
    let got: Vec<String> = rows.iter().map(|r| as_text(&r.values[0])).collect();
    // C collation: all upper-case ASCII sorts before all lower-case ASCII, and
    // non-ASCII sorts after every ASCII byte. A locale (en_US) collation would
    // give ["apple", "Apple", "Ápple", "banana", "Zebra"].
    assert_eq!(
        got,
        vec![
            "Apple".to_string(),
            "Zebra".to_string(),
            "apple".to_string(),
            "banana".to_string(),
            "Ápple".to_string(),
        ],
        "TEXT ordering is C / byte-order collation by design — this is the behaviour the SQL \
         reference must state"
    );

    // Same on the params family, so ORMs that bind get the same order.
    let rows = db
        .query_params("SELECT s FROM w ORDER BY s", &[])
        .expect("order by text, params family");
    let got_params: Vec<String> = rows.iter().map(|r| as_text(&r.values[0])).collect();
    assert_eq!(got_params, got, "both executor families must order identically");
}
