//! GH#29 — plan-time name resolution (the root behind the six alias /
//! qualifier failures in `tests/gh_issue_29.rs`, and sprinter b96bc6b51ae5).
//!
//! Through v4.31.1 the planner emitted every column reference blind and left
//! resolution to the evaluator, PER ROW. So `SELECT nosuch FROM t` on an EMPTY
//! table returned `Ok` with zero rows (nothing was ever evaluated), an unknown
//! wildcard qualifier expanded to the whole row, a derived table's alias could
//! not qualify its columns, an unquoted mixed-case alias could not be
//! referenced, and a `RETURNING bogus."n"` resolved by bare name. PostgreSQL
//! refuses all of these at plan time. Every reference in the SELECT list,
//! WHERE, JOIN … ON, GROUP BY, HAVING, ORDER BY and RETURNING now resolves
//! against the FROM scope (`src/sql/scope.rs`, wired through
//! `src/sql/planner.rs`) — by qualifier and name, case-folded unless quoted —
//! and a miss is an error whether or not the query would return rows:
//!
//! * unknown column ................ `Column "x" does not exist`      (42703)
//! * unknown qualifier / `bogus.*` . `missing FROM-clause entry …`    (42P01)
//! * alias declared twice .......... `… specified more than once`     (42712)
//! * duplicate-named sub-select column shadowed `column reference "x" is ambiguous` (42702)
//!
//! Candidate 2 (this file's second pass): a sub-select's or a view's output is
//! stamped with its alias at RUNTIME (`LogicalPlan::Project::source_alias`,
//! `SourceAliasOperator`), so `s.id` next to a base table that also has an
//! `id` — the SQLAlchemy `anon_1` / Prisma `_count` join shape — RESOLVES
//! instead of being refused; a column-alias list `AS s(a, b)` / `(VALUES …)
//! AS v(id, name)` is honoured; `UPDATE … FROM` / `DELETE … USING` entries
//! are in scope; `UPDATE`/`DELETE` refuse an unknown column on an EMPTY table
//! like `SELECT` does; an output alias is case-folded for `ORDER BY` /
//! `GROUP BY` resolution (never in the RowDescription).
//!
//! Every assertion runs on BOTH executor families (`query` → text family,
//! `query_params` → params family) — the planner is shared, so a fix cannot
//! land in one family only, and this file proves it did not. Column-absence
//! probes go through `information_schema.columns`, never through the very
//! statement under test.
//!
//! # Contract changes pinned here (statements that worked by accident)
//!
//! | statement | v4.31.1 | now |
//! |---|---|---|
//! | `SELECT nosuch.* FROM t a` | whole row | 42P01 |
//! | `SELECT a.* FROM t a JOIN t b …` | all columns of both sides | `a`'s columns |
//! | `SELECT ACCOUNT.* FROM "Account"` | resolved case-insensitively | 42P01 |
//! | `SELECT nosuch FROM t` on an empty table | `Ok`, 0 rows | 42703 |
//! | `CREATE VIEW v AS SELECT nosuch FROM t` | accepted, failed at first read | refused at CREATE |
//! | `UPDATE/DELETE … RETURNING bogus.col` / `bogus.*` | the target's column / whole row | 42P01 |
//! | `FROM (…) AS s(x)` | alias list silently ignored | column named `x` (list longer than the output: 42P10) |
//! | `FROM generate_series(1,3) AS G` | column named `G` | column named `g` |
//! | `FROM t JOIN (SELECT id FROM t) s ON s.id = t.id` | runtime error | resolves (`s.id` is the sub-select's column) |
//! | `UPDATE t SET v = nosuch` / `… WHERE nosuch = 1` / `DELETE … WHERE nosuch = 1` on an EMPTY table | `Ok`, 0 rows | 42703 |
//! | `CREATE MATERIALIZED VIEW … SELECT s.id FROM t JOIN (SELECT id FROM t) s …` (the bare name is carried by BOTH entries) | runtime error | 0A000 naming the workaround (the stored plan cannot carry the alias); every UNshadowed `s.col` / `v.col` in an MV body is rewritten to the bare name and works, REFRESH included |
//! | `SELECT t.id FROM t AS a JOIN (SELECT id FROM u) t ON …` (derived alias = an aliased base table's real name) | runtime error | resolves to the DERIVED table (alias match beats real-name match at runtime) |
//! | `UPDATE t SET v = DEFAULT` | runtime miss | the column's declared default (NULL when none) |
//! | `SELECT v AS Grp, count(*) FROM t GROUP BY grp` | runtime miss | groups by `v` (the key folds to the item's expression); `HAVING <output alias>` is 42703 at plan time |
//! | `FROM (SELECT a.id, b.id … ) s` (sub-select output repeats a name) | 42702 for EVERY `s.col` | 42702 only for `s.id`; `s.other` resolves; `AS s(w, x, y, z)` renames positionally |
//! | `SELECT * FROM (SELECT …, row_number() OVER (…) AS rn FROM t) s WHERE s.rn = 1` | window call pushed below the projection | evaluated above it (the pagination idiom returns the right row) |
//! | `FROM u AS a JOIN (SELECT id FROM t) u ON u.id + 10 = a.id` (hash join keyed on an EXPRESSION) | 0 rows (each side keyed per tuple by "natural operand, other on Err") | the rows (operand sides decided once at construction, alias tier first) |
//! | `UPDATE t SET v = nosuch WHERE id = 1` / `… WHERE id = 99` (the PK point-update fast path) | `Ok`, 0 rows when the key is absent | 42703 before any row is touched |
//! | `INSERT … ON CONFLICT (id) DO UPDATE SET v = t.nosuch` / `excluded.nosuch` | accepted, failed per CONFLICTING row only | 42703 at plan time (the target is in scope, by alias too) |
//! | `FROM t AS "A" JOIN t AS "a" ON "a".id = "A".id + 1` (case-distinct quoted aliases, expression key) | c4: (2, 1) — the key resolver case-folded aliases and guessed | (1, 2): the exact-case pass runs first; a term whose operands fit both sides is declined |
//! | `a RIGHT/FULL JOIN b ON a.id = b.id AND b.x = b.y` (a declined term under an outer join that preserves the build side) | c4: b's `x <> y` row dropped | NULL-extended (nested-loop join) |
//! | `… JOIN … ON a.id = b.id AND a.x = (SELECT max(x) FROM c)` | c4: the subquery term silently "no match" | materialized, evaluated; an evaluation error is the statement's error |
//! | `… JOIN … ON a.id = b.id AND 5 = b.price` (NUMERIC / DOUBLE) | c4: matched nothing (raw-value hash) | the literal term is left to the coercing evaluator |
//! | `UPDATE t SET v = v + 1 WHERE id = 99` (`v` TEXT, missing row) | `Ok`, 0 rows | 42883 `operator does not exist: text + integer` at plan time |
//! | `SELECT id FROM (SELECT a.id, b.id …) s` (a name one derived entry's WRITTEN-OUT select list carries twice) | the bare `id`: the first slot; the qualified `s.id`: the runtime miss the row above fixes | 42702, bare and qualified alike, AND for `s.*` — but only when the select list is AUTHOR-WRITTEN, which includes a list MIXING a wildcard with a written item (`SELECT *, b.id`). A list that is ENTIRELY wildcards, and a STORED relation's own schema (a base table, a MATERIALIZED VIEW), resolve to the first slot as v4.31.1 did (c12 M1/M2/M3) |
//! | `FROM a NATURAL JOIN b` / `NATURAL LEFT/RIGHT/FULL` | v4.31.1: the equi-join (c5 made it a CARTESIAN PRODUCT inside this unreleased cycle — `id = id`, both operands unqualified, bound to the same slot — and c10 removed that) | the equi-join, unchanged vs the release: both operands stay unqualified and an all-unqualified `=` term keys in the natural order |
//! | `FROM a JOIN b USING (col)` (every join type) | a CARTESIAN PRODUCT — `USING` was never lowered | the equi-join; a column either side lacks is 42703 |
//! | `… JOIN … ON a.id = b.id AND a.x = 20` (an equality no key binder can bind) | the term was SILENTLY DROPPED (syntax bucketing, then the index nested loop took only the first equality of the AND chain) | bucketed by BINDABILITY; the index nested loop declines a compound ON |
//! | `a LEFT/RIGHT/FULL JOIN b ON a.id = b.id AND <residual>` | the residual was a post-join filter, which ate the NULL-extended rows (the LEFT case returned ZERO rows) | evaluated INSIDE the join (LEFT: per candidate pair; RIGHT/FULL: nested loop). INNER keeps the post-join filter |
//! | `… ON a.id = b.id AND a.x = (SELECT … WHERE c.k = a.k)` (CORRELATED) | NULL was substituted; the term was never true and rows went missing | 0A000 `correlated subquery in JOIN ... ON is not supported` — and ONLY for a reference the subquery's own scopes cannot resolve: an UNCORRELATED subquery that fails in an ON clause keeps its own message and SQLSTATE |
//! | `a LEFT JOIN b ON a.id = b.id AND b.k = $1` (a PARAMETER in the residual) | c6: `Parameter $1 not provided` — the residual moved into the join operator's own evaluator, which was built with an EMPTY bind vector | evaluated: every evaluator a join operator builds carries the statement's bind values |
//! | `a NATURAL JOIN b NATURAL JOIN c` / chained `USING` (three or more tables) | c6: a cartesian product on the TEXT family — the c6 lowering qualified ONE operand, which `JoinPredicatePushdown` read as one-sided and pushed WHOLE into the other input, leaving the join with no ON | the equi join on BOTH families. BOTH operands are bare, so the pre-existing `refs.is_empty()` guard keeps the term on the join; the MIXED spelling is kept there by `has_unqualified_column_ref`, because a pushdown rule may never drop a term it cannot classify |
//! | `a JOIN b ON id = b.id` (a conjunct mixing a bare reference with a qualified one) | pushed into `b`, where the bare name resolved to b's own column: a tautology, and the join ran with NO condition | kept on the join |
//! | `CREATE MATERIALIZED VIEW … FROM t NATURAL JOIN v` (one side a view / derived table) | c6: 0A000 at CREATE — the c6 lowering qualified the view side with its stamped alias and the de-stamp pass refused a stamped reference whose bare name the other side also carries, which a shared join column always is | accepted. With bare operands the generated term carries no stamp, so `sql::mv_destamp` has nothing to rewrite in it, and REFRESH after a reopen re-executes the stored plan |
//!
//! Deliberately UNCHANGED and pinned below: the real table name still resolves
//! while an alias is in scope (`SELECT t.id FROM t AS t1`; PostgreSQL: 42P01),
//! and an unqualified name two tables share still takes the first match.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

/// `t(id INT PRIMARY KEY, v TEXT)`, EMPTY. The empty table is the point: it is
/// exactly the shape on which per-row resolution never ran.
fn empty_t() -> EmbeddedDatabase {
    let db = mem_db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .expect("create t");
    db
}

/// `t` with rows (1,'a') and (2,'b').
fn seeded_t() -> EmbeddedDatabase {
    let db = empty_t();
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')").expect("seed t");
    db
}

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
        ref other => panic!("expected text, got {other:?}"),
    }
}

/// The two executor families, by name, so a failure message says which one.
const FAMILIES: [(bool, &str); 2] = [(false, "text"), (true, "params")];

fn run(db: &EmbeddedDatabase, sql: &str, params_family: bool) -> Result<Vec<Tuple>, String> {
    let out = if params_family {
        db.query_params(sql, &[])
    } else {
        db.query(sql, &[])
    };
    out.map_err(|e| e.to_string())
}

/// `sql` must be REFUSED on both families with a message containing `needle`.
fn assert_refused(db: &EmbeddedDatabase, sql: &str, needle: &str) {
    for (params_family, family) in FAMILIES {
        match run(db, sql, params_family) {
            Ok(rows) => panic!(
                "[{family}] `{sql}` must be refused (expected `{needle}`), got Ok with {} rows",
                rows.len()
            ),
            Err(msg) => assert!(
                msg.contains(needle),
                "[{family}] `{sql}` was refused, but not with `{needle}`: {msg}"
            ),
        }
    }
}

/// `sql` must succeed on both families with exactly `rows` rows.
fn assert_rows(db: &EmbeddedDatabase, sql: &str, rows: usize) -> Vec<Tuple> {
    let mut last = Vec::new();
    for (params_family, family) in FAMILIES {
        let out = run(db, sql, params_family).unwrap_or_else(|e| panic!("[{family}] `{sql}` must plan and run: {e}"));
        assert_eq!(out.len(), rows, "[{family}] row count for `{sql}`");
        last = out;
    }
    last
}

/// Sorted first-column integers of `sql`, both families (must agree).
fn ids(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    let mut agreed: Option<Vec<i64>> = None;
    for (params_family, family) in FAMILIES {
        let out = run(db, sql, params_family).unwrap_or_else(|e| panic!("[{family}] `{sql}` must plan and run: {e}"));
        let mut got: Vec<i64> = out.iter().map(|r| as_i64(&r.values[0])).collect();
        got.sort_unstable();
        if let Some(prev) = &agreed {
            assert_eq!(prev, &got, "both families must agree on `{sql}`");
        }
        agreed = Some(got);
    }
    agreed.unwrap_or_default()
}

/// Column-absence probe through the catalog, never through the statement
/// under test.
fn column_exists(db: &EmbeddedDatabase, table: &str, column: &str) -> bool {
    let rows = db
        .query_params(
            "SELECT count(*) FROM information_schema.columns WHERE table_name = $1 AND column_name = $2",
            &[Value::String(table.into()), Value::String(column.into())],
        )
        .expect("information_schema probe");
    as_i64(&rows[0].values[0]) > 0
}

/// Output column names of `sql` (text family; the params family carries no
/// names) plus the params-family row width, which must agree.
fn column_names(db: &EmbeddedDatabase, sql: &str) -> Vec<String> {
    let (rows, cols) = db
        .query_with_columns(sql)
        .unwrap_or_else(|e| panic!("[text] `{sql}` must plan and run: {e}"));
    if let Some(first) = rows.first() {
        assert_eq!(first.values.len(), cols.len(), "[text] row width vs names for `{sql}`");
    }
    let params = db
        .query_params(sql, &[])
        .unwrap_or_else(|e| panic!("[params] `{sql}` must plan and run: {e}"));
    if let Some(first) = params.first() {
        assert_eq!(first.values.len(), cols.len(), "[params] row width for `{sql}`");
    }
    cols
}

const UNDEFINED_COLUMN: &str = "does not exist";
const MISSING_FROM_CLAUSE: &str = "missing FROM-clause entry";
const AMBIGUOUS: &str = "is ambiguous";
const DUPLICATE_ALIAS: &str = "specified more than once";

// ===========================================================================
// THE ROOT — an unknown column is an error regardless of row count
// ===========================================================================

#[test]
fn root_unknown_column_on_an_empty_table_is_refused_on_both_families() {
    let db = empty_t();
    assert!(!column_exists(&db, "t", "nosuch"), "probe: `nosuch` must not exist");
    assert!(column_exists(&db, "t", "id"), "probe: `id` must exist");

    // v4.31.1: Ok, 0 rows — the evaluator never ran, so nothing was refused.
    assert_refused(&db, "SELECT nosuch FROM t", "Column \"nosuch\" does not exist");
    assert_refused(&db, "SELECT nosuch FROM t", UNDEFINED_COLUMN);

    // Positive control on the SAME empty table: a real column plans and
    // returns zero rows, so the refusal above is about the name, not the table.
    assert_rows(&db, "SELECT id FROM t", 0);
    assert_rows(&db, "SELECT id, v FROM t WHERE id = 1", 0);
}

#[test]
fn root_unknown_column_on_a_one_row_table_is_the_same_refusal() {
    let db = empty_t();
    db.execute("INSERT INTO t VALUES (1, 'a')").expect("seed");
    // Same class, same wording, one row or none: cardinality-independent.
    assert_refused(&db, "SELECT nosuch FROM t", "Column \"nosuch\" does not exist");
    assert_rows(&db, "SELECT id FROM t", 1);
}

#[test]
fn root_qualified_unknown_column_is_refused_with_the_qualifier_spelled() {
    let db = empty_t();
    assert_refused(&db, "SELECT t.nosuch FROM t", "Column \"t\".\"nosuch\" does not exist");
    assert_refused(
        &db,
        r#"SELECT "t1"."nope" FROM t AS "t1""#,
        "Column \"t1\".\"nope\" does not exist",
    );
    assert_refused(
        &db,
        "SELECT T1.nope FROM t AS T1",
        "Column \"t1\".\"nope\" does not exist",
    );
    // Control: the qualifier itself is fine.
    assert_rows(&db, r#"SELECT "t1"."id" FROM t AS "t1""#, 0);
}

#[test]
fn root_unknown_column_is_refused_in_every_clause() {
    let db = seeded_t();
    // WHERE
    assert_refused(
        &db,
        "SELECT id FROM t WHERE nosuch = 1",
        "Column \"nosuch\" does not exist",
    );
    // JOIN … ON, bare and qualified
    assert_refused(
        &db,
        "SELECT a.id FROM t a JOIN t b ON a.id = b.nosuch",
        "Column \"b\".\"nosuch\" does not exist",
    );
    assert_refused(
        &db,
        "SELECT a.id FROM t a JOIN t b ON nosuch = 1",
        "Column \"nosuch\" does not exist",
    );
    // GROUP BY
    assert_refused(
        &db,
        "SELECT count(*) FROM t GROUP BY nosuch",
        "Column \"nosuch\" does not exist",
    );
    // HAVING
    assert_refused(
        &db,
        "SELECT v, count(*) FROM t GROUP BY v HAVING nosuch > 1",
        "Column \"nosuch\" does not exist",
    );
    // ORDER BY
    assert_refused(
        &db,
        "SELECT id FROM t ORDER BY nosuch",
        "Column \"nosuch\" does not exist",
    );
    // Inside an expression, a function argument and a CASE branch
    assert_refused(&db, "SELECT id + nosuch FROM t", "Column \"nosuch\" does not exist");
    assert_refused(&db, "SELECT upper(nosuch) FROM t", "Column \"nosuch\" does not exist");
    assert_refused(
        &db,
        "SELECT CASE WHEN id = 1 THEN nosuch ELSE v END FROM t",
        "Column \"nosuch\" does not exist",
    );
    // In a sub-select — validation is NOT switched off inside subqueries.
    assert_refused(
        &db,
        "SELECT id FROM t WHERE EXISTS (SELECT nosuch FROM t)",
        "Column \"nosuch\" does not exist",
    );
    assert_refused(
        &db,
        "SELECT * FROM (SELECT nosuch FROM t) s",
        "Column \"nosuch\" does not exist",
    );

    // Controls, same clauses, real names.
    assert_rows(&db, "SELECT id FROM t WHERE v = 'a'", 1);
    assert_rows(&db, "SELECT a.id FROM t a JOIN t b ON a.id = b.id", 2);
    assert_rows(&db, "SELECT count(*) FROM t GROUP BY v", 2);
    assert_rows(&db, "SELECT v, count(*) FROM t GROUP BY v HAVING count(*) > 0", 2);
    assert_rows(&db, "SELECT id FROM t ORDER BY v", 2);
}

#[test]
fn root_a_bare_identifier_without_from_is_refused_but_functions_and_literals_are_not() {
    let db = empty_t();
    // PostgreSQL: `SELECT x` → 42703. There is no FROM scope at all.
    assert_refused(&db, "SELECT nosuch", "Column \"nosuch\" does not exist");
    // These lower to functions / literals / parameters, never to a column.
    assert_rows(&db, "SELECT 1 + 1", 1);
    assert_rows(&db, "SELECT current_user", 1);
    assert_rows(&db, "SELECT now()", 1);
    assert_rows(&db, "SELECT TRUE, NULL", 1);
    let rows = db
        .query_params("SELECT $1", &[Value::Int4(7)])
        .expect("a parameter is not a column");
    assert_eq!(as_i64(&rows[0].values[0]), 7);
}

#[test]
fn root_a_prepared_statement_with_an_unknown_column_is_refused_before_execution() {
    let db = empty_t();
    let err = db
        .query_params("SELECT nosuch FROM t WHERE id = $1", &[Value::Int4(1)])
        .expect_err("planning a parameterised statement resolves its columns");
    assert!(
        err.to_string().contains("Column \"nosuch\" does not exist"),
        "got: {err}"
    );
}

#[test]
fn root_create_view_with_an_unknown_column_is_refused_at_definition_time() {
    // v4.31.1 accepted the view and failed on the first read that had rows.
    // Text family only: the params family has no DDL arm (see the extended
    // CREATE TABLE gap noted in src/lib.rs, not GH#29's family).
    let db = empty_t();
    let err = db
        .execute("CREATE VIEW bad AS SELECT nosuch FROM t")
        .expect_err("a view body is planned at CREATE");
    assert!(
        err.to_string().contains("Column \"nosuch\" does not exist"),
        "got: {err}"
    );
    db.execute("CREATE VIEW good AS SELECT id, v FROM t")
        .expect("control: a valid view body is accepted");
    assert_rows(&db, "SELECT id FROM good", 0);
}

// ===========================================================================
// QUALIFIED WILDCARD — exactly that entry's columns, or 42P01
// ===========================================================================

#[test]
fn wildcard_with_an_unknown_qualifier_is_42p01_never_the_whole_row() {
    let db = seeded_t();
    assert_refused(&db, "SELECT bogus.* FROM t a", MISSING_FROM_CLAUSE);
    assert_refused(&db, r#"SELECT "nosuch".* FROM "t" AS "a""#, MISSING_FROM_CLAUSE);
    // An alias that exists — but on a DIFFERENT statement — is still unknown.
    assert_refused(&db, "SELECT a.* FROM t b", MISSING_FROM_CLAUSE);
    // Case is exact: the unquoted `ACCOUNT` folds to `account`, which is not
    // the quoted table `"Account"` (v4.31.1 matched this case-insensitively).
    db.execute(r#"CREATE TABLE "Account" ("id" INT PRIMARY KEY)"#)
        .expect("create");
    assert_refused(&db, r#"SELECT ACCOUNT.* FROM "Account""#, MISSING_FROM_CLAUSE);
    assert_rows(&db, r#"SELECT "Account".* FROM "Account""#, 0);
}

#[test]
fn qualified_wildcard_projects_only_that_entrys_columns() {
    let db = seeded_t();
    // Self-join: `a.*` is two columns, not four.
    assert_eq!(
        column_names(&db, "SELECT a.* FROM t a JOIN t b ON a.id = b.id"),
        vec!["id".to_string(), "v".to_string()]
    );
    // Mixed with a single column from the other side.
    assert_eq!(
        column_names(&db, "SELECT a.*, b.id FROM t a JOIN t b ON a.id = b.id"),
        vec!["id".to_string(), "v".to_string(), "id".to_string()]
    );
    // The §6.3 guard: `T.*` with an unquoted mixed-case alias must keep
    // working now that an unknown qualifier is refused — it resolves because
    // the alias is case-folded where it is declared.
    assert_eq!(
        column_names(&db, "SELECT T.* FROM t AS T"),
        vec!["id".to_string(), "v".to_string()]
    );
    // Schema-qualified spelling.
    assert_eq!(
        column_names(&db, "SELECT public.t.* FROM t"),
        vec!["id".to_string(), "v".to_string()]
    );
    // A table function and a sub-select expand to their own single column.
    assert_eq!(
        column_names(&db, "SELECT g.* FROM generate_series(1, 3) g"),
        vec!["g".to_string()]
    );
    assert_rows(&db, "SELECT g.* FROM generate_series(1, 3) g", 3);
    assert_eq!(
        column_names(&db, "SELECT s.* FROM (SELECT id AS x FROM t) s"),
        vec!["x".to_string()]
    );
    // Under GROUP BY (v4.31.1 emitted ONE NULL against two aliases).
    let rows = assert_rows(&db, "SELECT a.* FROM t a GROUP BY a.id, a.v", 2);
    assert_eq!(rows[0].values.len(), 2, "`a.*` under GROUP BY is two columns wide");
    // On an EMPTY table the shape still plans (zero rows, right width).
    let empty = empty_t();
    assert_eq!(
        column_names(&empty, "SELECT a.* FROM t a"),
        vec!["id".to_string(), "v".to_string()]
    );
}

// ===========================================================================
// ALIASES — case folding is exact, duplicates are refused
// ===========================================================================

#[test]
fn alias_case_folding_follows_postgresql_exactly() {
    let db = seeded_t();
    // Unquoted `T1` declares `t1`.
    for sql in [
        "SELECT T1.id FROM t AS T1",
        "SELECT t1.id FROM t AS T1",
        r#"SELECT "t1".id FROM t AS T1"#,
        "SELECT T1.id FROM t T1 WHERE T1.id > 0 ORDER BY T1.id",
    ] {
        assert_rows(&db, sql, 2);
    }
    assert_refused(&db, r#"SELECT "T1".id FROM t AS T1"#, MISSING_FROM_CLAUSE);
    // Quoted `"T1"` is preserved as written.
    assert_rows(&db, r#"SELECT "T1".id FROM t AS "T1""#, 2);
    assert_refused(&db, r#"SELECT T1.id FROM t AS "T1""#, MISSING_FROM_CLAUSE);
    assert_refused(&db, r#"SELECT t1.id FROM t AS "T1""#, MISSING_FROM_CLAUSE);
    // Two quoted aliases differing only by case are two relations.
    assert_rows(
        &db,
        r#"SELECT "A".id, "a".v FROM t AS "A" JOIN t AS "a" ON "A".id = "a".id"#,
        2,
    );
}

#[test]
fn table_function_alias_is_case_folded_too() {
    // Contract change: `FROM generate_series(1, 3) AS G` names its column `g`
    // (PostgreSQL's answer); v4.31.1 named it `G`, which nothing could reference.
    let db = mem_db();
    assert_eq!(
        column_names(&db, "SELECT g FROM generate_series(1, 2) AS G"),
        vec!["g".to_string()]
    );
    assert_rows(&db, "SELECT G.g FROM generate_series(1, 2) AS G WHERE g > 1", 1);
}

#[test]
fn duplicate_alias_at_one_level_is_refused() {
    let db = seeded_t();
    assert_refused(&db, "SELECT a.id FROM t a JOIN t a ON a.id = a.id", DUPLICATE_ALIAS);
    // Two unaliased references to the same table, likewise (PostgreSQL:
    // `table name "t" specified more than once`).
    assert_refused(&db, "SELECT t.id FROM t JOIN t ON t.id = t.id", DUPLICATE_ALIAS);
}

#[test]
fn real_table_name_stays_lenient_while_aliased_but_not_when_shared() {
    let db = seeded_t();
    // Deliberately kept (PostgreSQL would raise 42P01): the extra spelling can
    // only name the SAME relation. Pinned by gh_issue_29::item2_alias_spellings_matrix too.
    assert_rows(&db, "SELECT t.id FROM t AS t1", 2);
    assert_rows(&db, r#"SELECT "t"."v" FROM "public"."t" AS "t1" WHERE t1.id = 1"#, 1);
    // …but when two aliased entries share the real name it is not usable
    // (which relation would it be?) — 42P01, exactly as in PostgreSQL.
    assert_refused(&db, "SELECT t.id FROM t a JOIN t b ON a.id = b.id", MISSING_FROM_CLAUSE);
    // The alias always wins over a real name: `b` here is an alias of `t`,
    // and there is also a real table `b` NOT in FROM.
    db.execute("CREATE TABLE b (id INT PRIMARY KEY, w TEXT)")
        .expect("create b");
    assert_refused(&db, "SELECT b.w FROM t AS b", "Column \"b\".\"w\" does not exist");
}

#[test]
fn unqualified_name_shared_by_two_tables_stays_first_match() {
    // Residual, pinned as current behaviour: PostgreSQL raises 42702 here.
    let db = seeded_t();
    assert_rows(&db, "SELECT id FROM t a JOIN t b ON a.id = b.id", 2);
}

// ===========================================================================
// DERIVED TABLES, CTEs, VIEWS — the alias qualifies the output
// ===========================================================================

#[test]
fn derived_table_alias_qualifies_its_columns_in_every_clause() {
    let db = seeded_t();
    assert_eq!(ids(&db, "SELECT s.x FROM (SELECT id AS x FROM t) s"), vec![1, 2]);
    assert_eq!(
        ids(&db, "SELECT s.x FROM (SELECT id AS x FROM t) s WHERE s.x = 2"),
        vec![2]
    );
    let rows = assert_rows(&db, "SELECT s.x FROM (SELECT id AS x FROM t) s ORDER BY s.x DESC", 2);
    assert_eq!(as_i64(&rows[0].values[0]), 2, "ORDER BY s.x DESC");
    assert_eq!(
        ids(&db, "SELECT t.id FROM t JOIN (SELECT id AS x FROM t) s ON s.x = t.id"),
        vec![1, 2]
    );
    // Over a UNION body and over an ORDER BY … LIMIT body.
    assert_eq!(
        ids(
            &db,
            "SELECT s.x FROM (SELECT id AS x FROM t UNION SELECT id + 10 AS x FROM t) s"
        ),
        vec![1, 2, 11, 12]
    );
    assert_eq!(
        ids(
            &db,
            "SELECT s.x FROM (SELECT id AS x FROM t ORDER BY id DESC LIMIT 1) s"
        ),
        vec![2]
    );
    // Unknown column behind a valid derived alias.
    assert_refused(
        &db,
        "SELECT s.nope FROM (SELECT id AS x FROM t) s",
        "Column \"s\".\"nope\" does not exist",
    );
    // Unknown qualifier next to a derived table.
    assert_refused(&db, "SELECT z.x FROM (SELECT id AS x FROM t) s", MISSING_FROM_CLAUSE);
    // Control that passes on any tree: the unqualified spelling.
    assert_eq!(ids(&db, "SELECT x FROM (SELECT id AS x FROM t) s"), vec![1, 2]);
}

#[test]
fn derived_table_column_shadowed_by_another_entry_resolves_through_its_alias() {
    // Candidate 1 refused this with 42702: a sub-select's output carried no
    // source qualifier at runtime, so `s.id` could only be rewritten to the
    // bare `id`, which `t` also has. Candidate 2 stamps the sub-select's
    // output with its alias at runtime (the same `source_table` tag a base
    // table gets from `handle_scan`), so the reference resolves — and to the
    // RIGHT column: the values below come from the sub-select, not from `t`.
    // This is the shape SQLAlchemy (`anon_1`), Prisma (`_count` self-joins)
    // and hand-written reports emit and cannot rewrite. v4.31.1: a runtime
    // error.
    let db = seeded_t();
    assert_eq!(
        ids(&db, "SELECT s.id FROM t JOIN (SELECT id FROM t) s ON s.id = t.id"),
        vec![1, 2]
    );
    // The projected VALUE is the sub-select's, filtered through the sub-select's
    // own WHERE: only the row with v = 'b' survives the join.
    assert_eq!(
        ids(
            &db,
            "SELECT s.id FROM t JOIN (SELECT id FROM t WHERE v = 'b') s ON s.id = t.id"
        ),
        vec![2]
    );
    let rows = assert_rows(
        &db,
        "SELECT s.v FROM t JOIN (SELECT id, v FROM t) s ON s.id = t.id WHERE t.id = 2",
        1,
    );
    assert_eq!(as_text(&rows[0].values[0]), "b");
    // `s.*` over the join describes exactly the sub-select's columns.
    let cols = column_names(&db, "SELECT s.* FROM (SELECT id, v FROM t) s JOIN t ON t.id = s.id");
    assert_eq!(cols, vec!["id", "v"]);
    assert_rows(&db, "SELECT s.* FROM t JOIN (SELECT id FROM t) s ON t.id = 1", 2);
    // A predicate on the alias above the join is pushed into the sub-select
    // (the optimizer strips the alias qualifier when crossing the stamp) and
    // still selects the right rows.
    assert_eq!(
        ids(
            &db,
            "SELECT s.id FROM t JOIN (SELECT id FROM t) s ON s.id = t.id WHERE s.id = 2"
        ),
        vec![2]
    );
    // SQLAlchemy's shape: every column aliased inside, referenced through
    // `anon_1`, joined back to the base table it came from.
    let rows = assert_rows(
        &db,
        "SELECT anon_1.id, anon_1.v FROM (SELECT t.id AS id, t.v AS v FROM t) AS anon_1 \
         JOIN t ON t.id = anon_1.id WHERE anon_1.v = 'a'",
        1,
    );
    assert_eq!(as_i64(&rows[0].values[0]), 1);
    assert_eq!(as_text(&rows[0].values[1]), "a");
    // Prisma's `_count` shape: an aggregating sub-select joined to its base.
    let rows = assert_rows(
        &db,
        "SELECT t.id, agg.cnt FROM t LEFT JOIN (SELECT id, count(*) AS cnt FROM t GROUP BY id) agg \
         ON agg.id = t.id ORDER BY t.id",
        2,
    );
    assert_eq!(as_i64(&rows[0].values[1]), 1);
    // A sub-select whose root is not a projection (ORDER BY … LIMIT, UNION)
    // is wrapped in one that carries the alias.
    assert_eq!(
        ids(
            &db,
            "SELECT s.id FROM t JOIN (SELECT id FROM t ORDER BY id DESC LIMIT 1) s ON s.id = t.id"
        ),
        vec![2]
    );
    assert_eq!(
        ids(
            &db,
            "SELECT s.id FROM t JOIN (SELECT id FROM t UNION SELECT id + 10 FROM t) s ON s.id = t.id"
        ),
        vec![1, 2]
    );
    // The user-side spelling keeps working too.
    assert_eq!(
        ids(
            &db,
            "SELECT s.sid FROM t JOIN (SELECT id AS sid FROM t) s ON s.sid = t.id"
        ),
        vec![1, 2]
    );
}

#[test]
fn derived_table_with_duplicate_output_names_refuses_only_the_duplicated_name() {
    // Candidate 2 left the WHOLE sub-select unstamped whenever any inner name
    // repeated, so every `s.col` took the bare-name path and was 42702 when
    // another entry carried it. Candidate 3 (m3): the root projection is
    // stamped regardless; the scope refuses 42702 only a qualified reference
    // to the DUPLICATED name, and `s.*` (which would emit it twice) — every
    // other column resolves through the alias, to the right value.
    let db = seeded_t();
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, tid INT)")
        .expect("create u");
    db.execute("INSERT INTO u VALUES (11, 1), (12, 2)").expect("seed u");
    // `id` twice, `v` and `tid` once each.
    let dup = "(SELECT t.id, t.v, u.id, u.tid FROM t JOIN u ON u.tid = t.id)";
    let rows = assert_rows(
        &db,
        &format!("SELECT s.v, s.tid FROM {dup} s JOIN t ON t.id = s.tid WHERE t.v = 'b'"),
        1,
    );
    assert_eq!(as_text(&rows[0].values[0]), "b");
    assert_eq!(as_i64(&rows[0].values[1]), 2);
    assert_eq!(
        ids(&db, &format!("SELECT s.tid FROM {dup} s ORDER BY s.tid")),
        vec![1, 2]
    );
    assert_refused(&db, &format!("SELECT s.id FROM {dup} s"), AMBIGUOUS);
    // OVER-REFUSAL, stated (m4c): PostgreSQL ACCEPTS `SELECT s.*` here — it
    // simply returns both `id` columns. We cannot: `s.*` expands to `s.id`
    // TWICE and the runtime lookup by alias and name can only answer with the
    // first slot, so the second column would silently repeat the first. This
    // is fail-closed until the shared output column is merged (sprinter
    // 781f55ba534d); it is NOT proof that `s.id` must be refused — that one is
    // refused because the WRITTEN list really names two columns.
    assert_refused(&db, &format!("SELECT s.* FROM {dup} s"), AMBIGUOUS);
    // The UNQUALIFIED spelling of the duplicated name is just as ambiguous
    // (PostgreSQL 42702); a unique name still resolves bare.
    assert_refused(&db, &format!("SELECT id FROM {dup} s"), AMBIGUOUS);
    assert_refused(&db, &format!("SELECT tid FROM {dup} s WHERE id = 1"), AMBIGUOUS);
    assert_eq!(ids(&db, &format!("SELECT tid FROM {dup} s ORDER BY tid")), vec![1, 2]);
    // A non-projection root (ORDER BY … LIMIT) is wrapped positionally
    // (`BoundColumn`), so the duplicate cannot make two slots read one input.
    let rows = assert_rows(
        &db,
        &format!("SELECT s.v, s.tid FROM (SELECT t.id, t.v, u.id, u.tid FROM t JOIN u ON u.tid = t.id ORDER BY t.id DESC LIMIT 1) s"),
        1,
    );
    assert_eq!(as_text(&rows[0].values[0]), "b");
    assert_eq!(as_i64(&rows[0].values[1]), 2);
}

#[test]
fn column_alias_list_over_duplicate_output_names_renames_positionally() {
    // Candidate 2 refused this with 0A000 although it is PostgreSQL's own
    // tool for exactly this output (m4). In place, the aliases vector is
    // positional; a wrapped root reads its input positionally.
    let db = seeded_t();
    // a = (1,'a') joins b = (2,'b'): four distinct values, two repeated names.
    let body = "SELECT a.id, a.v, b.id, b.v FROM t a JOIN t b ON b.id = a.id + 1";
    let rows = assert_rows(
        &db,
        &format!("SELECT s.w, s.x, s.y, s.z FROM ({body}) AS s(w, x, y, z)"),
        1,
    );
    assert_eq!(as_i64(&rows[0].values[0]), 1);
    assert_eq!(as_text(&rows[0].values[1]), "a");
    assert_eq!(as_i64(&rows[0].values[2]), 2, "the SECOND `id` is b's");
    assert_eq!(as_text(&rows[0].values[3]), "b");
    assert_eq!(
        column_names(&db, &format!("SELECT * FROM ({body}) AS s(w, x, y, z)")),
        vec!["w", "x", "y", "z"]
    );
    // Wrapped root (ORDER BY … LIMIT) with the same list.
    let rows = assert_rows(
        &db,
        &format!("SELECT s.w, s.y FROM ({body} ORDER BY a.id LIMIT 1) AS s(w, x, y, z) WHERE s.y = 2"),
        1,
    );
    assert_eq!(as_i64(&rows[0].values[0]), 1);
    assert_eq!(as_i64(&rows[0].values[1]), 2);
    // A shorter list renames the first columns; the rest keep their inner
    // names — `s(w, x)` leaves `id` and `v` unique, `s(w)` leaves `v` twice.
    let rows = assert_rows(&db, &format!("SELECT s.w, s.x, s.id, s.v FROM ({body}) AS s(w, x)"), 1);
    assert_eq!(as_i64(&rows[0].values[0]), 1);
    assert_eq!(as_text(&rows[0].values[1]), "a");
    assert_eq!(as_i64(&rows[0].values[2]), 2);
    assert_eq!(as_text(&rows[0].values[3]), "b");
    assert_refused(&db, &format!("SELECT s.v FROM ({body}) AS s(w)"), AMBIGUOUS);
}

#[test]
fn derived_table_column_alias_list_is_honoured() {
    // v4.31.1 dropped `(x)` and returned a column named `id`; candidate 1
    // refused the list (0A000). The list now renames the output positionally,
    // a shorter list keeps the remaining inner names, and a longer one is
    // 42P10 with PostgreSQL's wording.
    let db = seeded_t();
    assert_eq!(column_names(&db, "SELECT * FROM (SELECT id FROM t) s(x)"), vec!["x"]);
    assert_eq!(ids(&db, "SELECT s.x FROM (SELECT id FROM t) s(x)"), vec![1, 2]);
    assert_eq!(ids(&db, "SELECT x FROM (SELECT id FROM t) s(x) WHERE s.x = 2"), vec![2]);
    assert_eq!(
        column_names(&db, "SELECT * FROM (SELECT id, v FROM t) s(x)"),
        vec!["x", "v"],
        "a shorter list keeps the remaining inner names"
    );
    // The list is folded like every other identifier: `s(X)` declares `x`.
    assert_eq!(column_names(&db, "SELECT s.x FROM (SELECT id FROM t) s(X)"), vec!["x"]);
    // `(VALUES …) AS v(id, name)` — the shape ORMs and hand-written upserts use.
    let rows = assert_rows(
        &db,
        "SELECT v.id, v.name FROM (VALUES (1, 'one'), (2, 'two')) AS v(id, name) ORDER BY v.id",
        2,
    );
    assert_eq!(as_i64(&rows[0].values[0]), 1);
    assert_eq!(as_text(&rows[0].values[1]), "one");
    assert_eq!(
        column_names(&db, "SELECT * FROM (VALUES (1, 'one')) AS v(id, name)"),
        vec!["id", "name"]
    );
    // The inner names are gone: only the list's names resolve.
    assert_refused(
        &db,
        "SELECT s.id FROM (SELECT id FROM t) s(x)",
        "Column \"s\".\"id\" does not exist",
    );
    assert_refused(&db, "SELECT * FROM (SELECT id FROM t) s(x, y)", "columns available but");
}

#[test]
fn cte_alias_qualifies_its_columns() {
    let db = seeded_t();
    assert_eq!(
        ids(&db, "WITH c AS (SELECT id, v FROM t) SELECT c.id FROM c"),
        vec![1, 2]
    );
    assert_eq!(
        ids(
            &db,
            "WITH c AS (SELECT id, v FROM t) SELECT x.id FROM c x JOIN c y ON x.id = y.id WHERE y.v = 'b'"
        ),
        vec![2]
    );
    assert_eq!(
        ids(
            &db,
            "WITH c AS (SELECT id, v FROM t) SELECT X.id FROM c AS X ORDER BY X.id"
        ),
        vec![1, 2]
    );
    assert_refused(
        &db,
        "WITH c AS (SELECT id, v FROM t) SELECT bogus.id FROM c",
        MISSING_FROM_CLAUSE,
    );
    assert_refused(
        &db,
        "WITH c AS (SELECT id, v FROM t) SELECT c.nope FROM c",
        "Column \"c\".\"nope\" does not exist",
    );
    // The CTE body itself is validated.
    assert_refused(
        &db,
        "WITH c AS (SELECT nosuch FROM t) SELECT * FROM c",
        "Column \"nosuch\" does not exist",
    );
}

#[test]
fn view_alias_qualifies_its_columns() {
    let db = seeded_t();
    db.execute("CREATE VIEW myview AS SELECT id, v FROM t")
        .expect("create view");
    // Works for the first time: a view expands to a projection whose output
    // carried no qualifier the runtime lookup could match.
    assert_eq!(ids(&db, "SELECT v1.id FROM myview v1"), vec![1, 2]);
    assert_eq!(ids(&db, "SELECT myview.id FROM myview WHERE myview.v = 'a'"), vec![1]);
    assert_eq!(ids(&db, "SELECT v1.id FROM myview v1 ORDER BY v1.id DESC"), vec![1, 2]);
    assert_refused(&db, "SELECT nosuch.id FROM myview v1", MISSING_FROM_CLAUSE);
    assert_refused(
        &db,
        "SELECT v1.nope FROM myview v1",
        "Column \"v1\".\"nope\" does not exist",
    );
}

// ===========================================================================
// RETURNING — the qualifier must name the target (GH#23's stated residual)
// ===========================================================================

#[test]
fn returning_with_an_unknown_qualifier_is_refused_on_both_families_with_nothing_written() {
    for (params_entry, family) in FAMILIES {
        let db = seeded_t();
        let returning = |sql: &str| -> Result<(u64, Vec<Tuple>), String> {
            if params_entry {
                db.execute_params_returning(sql, &[]).map_err(|e| e.to_string())
            } else {
                db.execute_returning(sql).map_err(|e| e.to_string())
            }
        };
        for sql in [
            r#"UPDATE t SET v = 'z' WHERE id = 1 RETURNING bogus.id"#,
            r#"UPDATE t SET v = 'z' WHERE id = 1 RETURNING bogus."v""#,
            r#"UPDATE t SET v = 'z' WHERE id = 1 RETURNING bogus.*"#,
            r#"UPDATE t SET v = 'z' WHERE id = 1 RETURNING bogus.id AS c"#,
            r#"UPDATE t SET v = 'z' WHERE id = 1 RETURNING bogus.id + 1"#,
            r#"DELETE FROM t WHERE id = 1 RETURNING bogus."v""#,
            r#"DELETE FROM t AS x WHERE id = 1 RETURNING y.id"#,
            r#"INSERT INTO t VALUES (3, 'c') RETURNING bogus.id"#,
            // The unquoted `Typed` folds to `typed`; the table is `t`, not `typed`.
            r#"UPDATE t SET v = 'z' WHERE id = 1 RETURNING Typed."v""#,
        ] {
            match returning(sql) {
                Ok((n, rows)) => panic!("[{family}] `{sql}` must be refused, got {n} affected / {rows:?}"),
                Err(msg) => assert!(msg.contains(MISSING_FROM_CLAUSE), "[{family}] `{sql}`: {msg}"),
            }
        }
        // Refused at plan time: nothing was written or deleted.
        let rows = db.query("SELECT v FROM t WHERE id = 1", &[]).expect("read back");
        assert_eq!(
            as_text(&rows[0].values[0]),
            "a",
            "[{family}] the UPDATE must not have run"
        );
        assert_eq!(
            ids(&db, "SELECT id FROM t"),
            vec![1, 2],
            "[{family}] nothing deleted or inserted"
        );
    }
}

#[test]
fn returning_qualified_by_the_target_keeps_resolving() {
    for (params_entry, family) in FAMILIES {
        let db = seeded_t();
        let returning = |sql: &str| -> (u64, Vec<Tuple>) {
            let out = if params_entry {
                db.execute_params_returning(sql, &[])
            } else {
                db.execute_returning(sql)
            };
            out.unwrap_or_else(|e| panic!("[{family}] `{sql}` must run: {e}"))
        };
        let (_, rows) = returning("UPDATE t SET v = 'q' WHERE id = 1 RETURNING t.id, t.v");
        assert_eq!(as_i64(&rows[0].values[0]), 1);
        assert_eq!(as_text(&rows[0].values[1]), "q");
        let (_, rows) = returning("UPDATE t SET v = 'r' WHERE id = 1 RETURNING public.t.v");
        assert_eq!(as_text(&rows[0].values[0]), "r");
        let (_, rows) = returning("UPDATE t AS u SET v = 's' WHERE u.id = 1 RETURNING u.v AS c");
        assert_eq!(as_text(&rows[0].values[0]), "s");
        let (_, rows) = returning("UPDATE t SET v = 'w' WHERE id = 1 RETURNING t.*");
        assert_eq!(rows[0].values.len(), 2, "[{family}] t.* is the whole row");
        let (_, rows) = returning("DELETE FROM t AS x WHERE x.id = 2 RETURNING x.id AS c, x.v");
        assert_eq!(as_i64(&rows[0].values[0]), 2);
        assert_eq!(as_text(&rows[0].values[1]), "b");
        let (_, rows) = returning("INSERT INTO t VALUES (5, 'e') RETURNING t.id");
        assert_eq!(as_i64(&rows[0].values[0]), 5);
        assert_eq!(ids(&db, "SELECT id FROM t"), vec![1, 5], "[{family}]");
    }
}

// ===========================================================================
// DML WHERE — the target is the one entry in scope; subqueries walk outward
// ===========================================================================

#[test]
fn dml_target_alias_and_correlated_subqueries_resolve() {
    for (params_family, family) in FAMILIES {
        let db = mem_db();
        db.execute("CREATE TABLE p (id INT PRIMARY KEY, login TEXT)")
            .expect("create p");
        db.execute("CREATE TABLE s (id INT PRIMARY KEY, pid INT, n INT)")
            .expect("create s");
        db.execute("INSERT INTO p VALUES (1, 'alice'), (2, 'bob')")
            .expect("seed p");
        db.execute("INSERT INTO s VALUES (10, 1, 0), (11, 2, 0), (12, 2, 0)")
            .expect("seed s");
        let exec = |sql: &str| -> u64 {
            let out = if params_family {
                db.execute_params(sql, &[])
            } else {
                db.execute(sql)
            };
            out.unwrap_or_else(|e| panic!("[{family}] `{sql}` must run: {e}"))
        };
        // A correlated EXISTS / IN inside a DML predicate is GH#32's shape
        // (the DML evaluator has no executor context for subqueries — a
        // pre-existing gap pinned in tests/gh_issue_32.rs), NOT this file's:
        // here only the alias / qualifier half is asserted.
        // Contract change: `DELETE FROM t AS x WHERE x.id = …` now resolves
        // (v4.31.1: a runtime miss on the alias).
        assert_eq!(exec("DELETE FROM s AS x WHERE x.id = 11"), 1, "[{family}]");
        assert_eq!(exec("UPDATE s AS y SET n = 7 WHERE y.id = 12"), 1, "[{family}]");
        assert_eq!(exec("UPDATE s AS y SET n = y.n + 1 WHERE y.pid = 1"), 1, "[{family}]");
        assert_eq!(ids(&db, "SELECT n FROM s ORDER BY id"), vec![1, 7], "[{family}]");
        // An unknown qualifier in a DML predicate is 42P01, not a runtime miss.
        let err = if params_family {
            db.execute_params("DELETE FROM s WHERE bogus.id = 1", &[]).err()
        } else {
            db.execute("DELETE FROM s WHERE bogus.id = 1").err()
        };
        let err = err.unwrap_or_else(|| panic!("[{family}] an unknown qualifier in DELETE must be refused"));
        assert!(err.to_string().contains(MISSING_FROM_CLAUSE), "[{family}] {err}");
    }
}

#[test]
fn excluded_and_trigger_pseudo_relations_bypass_the_scope() {
    let db = seeded_t();
    // EXCLUDED keeps its own path (`resolve_excluded_refs`).
    db.execute("INSERT INTO t VALUES (1, 'upsert') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v")
        .expect("ON CONFLICT … EXCLUDED");
    db.execute_params(
        "INSERT INTO t VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v",
        &[Value::Int4(2), Value::String("upsert2".into())],
    )
    .expect("ON CONFLICT … EXCLUDED on the params family");
    let rows = db.query("SELECT v FROM t WHERE id = 1", &[]).expect("read");
    assert_eq!(as_text(&rows[0].values[0]), "upsert");
    // NEW / OLD in a trigger WHEN clause and body keep their evaluator path.
    db.execute("CREATE TABLE trg_dst (id INT, tag TEXT)").expect("create");
    db.execute(
        "CREATE FUNCTION rewrite_fn() RETURNS TRIGGER AS $$ BEGIN NEW.tag = 'set-by-trigger'; RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .expect("CREATE FUNCTION");
    db.execute(
        "CREATE TRIGGER trg BEFORE INSERT ON trg_dst FOR EACH ROW WHEN (NEW.id > 10) EXECUTE FUNCTION rewrite_fn()",
    )
    .expect("CREATE TRIGGER");
    db.execute("INSERT INTO trg_dst (id, tag) VALUES (1, 'original'), (99, 'original')")
        .expect("insert through the trigger");
    let rows = db.query("SELECT tag FROM trg_dst ORDER BY id", &[]).expect("read back");
    assert_eq!(as_text(&rows[0].values[0]), "original");
    assert_eq!(as_text(&rows[1].values[0]), "set-by-trigger");
}

// ===========================================================================
// POSITIVE CONTROLS — every legal spelling still plans on both families
// ===========================================================================

#[test]
fn positive_controls_every_legal_shape_still_resolves() {
    let db = seeded_t();
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, tid INT)")
        .expect("create u");
    db.execute("INSERT INTO u VALUES (7, 1)").expect("seed u");
    db.execute("CREATE VIEW myview AS SELECT id, v FROM t")
        .expect("create view");
    let cases: &[(&str, usize)] = &[
        ("SELECT count(*) FROM t GROUP BY v", 2),
        ("SELECT v, count(*) AS n FROM t GROUP BY v ORDER BY n", 2),
        (
            "SELECT v, count(*) AS n FROM t GROUP BY v HAVING count(*) > 0 ORDER BY v",
            2,
        ),
        ("SELECT id AS k FROM t ORDER BY k", 2),
        ("SELECT id FROM t ORDER BY 1", 2),
        ("SELECT id FROM t ORDER BY v", 2),
        ("SELECT DISTINCT v FROM t ORDER BY v", 2),
        ("SELECT id FROM t UNION SELECT id FROM u ORDER BY id", 3),
        (
            "SELECT id FROM t o WHERE EXISTS (SELECT 1 FROM t i WHERE i.id = o.id)",
            2,
        ),
        ("SELECT t.id, u.id FROM t, u WHERE t.id = u.tid", 1),
        ("SELECT t.id FROM t LEFT JOIN u ON u.tid = t.id", 2),
        (
            "SELECT t.id FROM t JOIN u ON u.tid = t.id JOIN t t2 ON t2.id = u.tid",
            1,
        ),
        ("SELECT id, row_number() OVER (ORDER BY id) FROM t", 2),
        ("SELECT v1.id FROM myview v1", 2),
        ("WITH c AS (SELECT id FROM t) SELECT x.id FROM c x", 2),
        ("SELECT g FROM generate_series(1, 3) AS g", 3),
        (
            "SELECT c.name, g FROM (SELECT v AS name FROM t) c, generate_series(1, 2) AS g",
            4,
        ),
        (
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relname = 't'",
            1,
        ),
        (
            "SELECT c.column_name FROM information_schema.columns c WHERE c.table_name = 't'",
            2,
        ),
        (
            "SELECT columns.column_name FROM information_schema.columns WHERE table_name = 't'",
            2,
        ),
        ("SELECT t.id FROM public.t WHERE public.t.id = 1", 1),
        (r#"SELECT "t"."id" FROM "public"."t" AS "t1""#, 2),
        ("SELECT t.id FROM t AS t1", 2),
        ("SELECT id FROM t WHERE id IN (SELECT tid FROM u)", 1),
        ("SELECT id FROM t WHERE id = ANY(ARRAY[1, 2])", 2),
        ("SELECT upper(v) FROM t WHERE lower(v) = 'a'", 1),
        ("SELECT CASE WHEN id = 1 THEN v ELSE 'other' END FROM t", 2),
        ("SELECT id FROM t WHERE v LIKE 'a%'", 1),
        ("SELECT id FROM t WHERE id BETWEEN 1 AND 2", 2),
        ("SELECT id FROM t WHERE v IS NOT NULL", 2),
    ];
    for (sql, rows) in cases {
        assert_rows(&db, sql, *rows);
    }
}

// ===========================================================================
// Candidate 2 — FIX 1: UPDATE / DELETE refuse an unknown column on an EMPTY
// table (the root, closed for DML too)
// ===========================================================================

#[test]
fn dml_unknown_column_is_refused_on_an_empty_table_on_both_families() {
    // Candidate 1 validated SELECT's clauses only; `update_to_plan` /
    // `delete_to_plan` lowered SET values and WHERE with `expr_to_logical`
    // alone, so on an EMPTY table (nothing evaluated per row) these succeeded
    // with 0 rows. PostgreSQL: 42703, regardless of row count.
    let db = empty_t();
    assert!(!column_exists(&db, "t", "nosuch"), "probe through the catalog");
    for (params_family, family) in FAMILIES {
        let exec = |sql: &str| -> Result<u64, String> {
            if params_family {
                db.execute_params(sql, &[]).map_err(|e| e.to_string())
            } else {
                db.execute(sql).map_err(|e| e.to_string())
            }
        };
        for sql in [
            "UPDATE t SET v = nosuch",
            "UPDATE t SET v = 'x' WHERE nosuch = 1",
            "UPDATE t SET v = t.nosuch WHERE id = 1",
            "DELETE FROM t WHERE nosuch = 1",
            "DELETE FROM t AS x WHERE x.nosuch = 1",
        ] {
            match exec(sql) {
                Ok(n) => panic!("[{family}] `{sql}` must be refused on an empty table, got Ok({n})"),
                Err(msg) => assert!(
                    msg.contains(UNDEFINED_COLUMN) && msg.contains("nosuch"),
                    "[{family}] `{sql}` was refused, but not as an unknown column: {msg}"
                ),
            }
        }
        // Positive controls on the same empty table: every legal shape plans.
        for sql in [
            "UPDATE t SET v = 'x' WHERE id = 1",
            "UPDATE t SET v = v || 'x'",
            "UPDATE t AS x SET v = x.v WHERE x.id = 1",
            "UPDATE t SET v = (SELECT max(v) FROM t AS i WHERE i.id = t.id)",
            "DELETE FROM t WHERE id = 1",
            "DELETE FROM t WHERE EXISTS (SELECT 1 FROM t AS i WHERE i.id = t.id)",
        ] {
            assert_eq!(
                exec(sql).unwrap_or_else(|e| panic!("[{family}] `{sql}` must plan: {e}")),
                0,
                "[{family}] {sql}"
            );
        }
    }
}

// ===========================================================================
// Candidate 2 — FIX 4a: `UPDATE … FROM` / `DELETE … USING` entries are in scope
// ===========================================================================

#[test]
fn update_from_and_delete_using_entries_are_in_scope() {
    // Candidate 1 refused `s` with 42P01 (`missing FROM-clause entry`): the
    // DML scope held the target only. The FROM / USING entries are now planned
    // for their range entries, so the statement plans as in PostgreSQL. The
    // DML executor evaluates against the target's row as it always did (no
    // join support in the DML executor — unchanged from v4.31.1), which an
    // EMPTY target never exercises.
    let db = empty_t();
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, v TEXT)")
        .expect("create s");
    for (params_family, family) in FAMILIES {
        let exec = |sql: &str| -> Result<u64, String> {
            if params_family {
                db.execute_params(sql, &[]).map_err(|e| e.to_string())
            } else {
                db.execute(sql).map_err(|e| e.to_string())
            }
        };
        for sql in [
            "UPDATE t SET v = s.v FROM s WHERE s.id = t.id",
            "UPDATE t SET v = x.v FROM s AS x WHERE x.id = t.id",
            "DELETE FROM t USING s WHERE s.id = t.id",
            "DELETE FROM t USING s AS x WHERE x.id = t.id AND x.v = 'gone'",
        ] {
            assert_eq!(
                exec(sql).unwrap_or_else(|e| panic!("[{family}] `{sql}` must plan: {e}")),
                0,
                "[{family}] {sql}"
            );
        }
        // …and the extra entry is validated like any other range entry.
        for (sql, needle) in [
            ("UPDATE t SET v = s.nosuch FROM s WHERE s.id = t.id", UNDEFINED_COLUMN),
            ("DELETE FROM t USING s WHERE s.nosuch = t.id", UNDEFINED_COLUMN),
            ("UPDATE t SET v = s.v FROM s WHERE bogus.id = t.id", MISSING_FROM_CLAUSE),
            (
                "UPDATE t SET v = 'x' FROM nosuch_table WHERE nosuch_table.id = t.id",
                "does not exist",
            ),
        ] {
            match exec(sql) {
                Ok(n) => panic!("[{family}] `{sql}` must be refused, got Ok({n})"),
                Err(msg) => assert!(
                    msg.contains(needle),
                    "[{family}] `{sql}`: expected `{needle}`, got {msg}"
                ),
            }
        }
    }
}

// ===========================================================================
// Candidate 2 — FIX 4b: an output alias is case-folded for resolution
// ===========================================================================

#[test]
fn output_alias_is_folded_for_order_by_resolution_but_not_in_the_row_description() {
    // PostgreSQL folds `AS Total` to `total` at parse time, so `ORDER BY total`
    // resolves. Nano keeps the alias as written for the RowDescription
    // (documented, lenient) and folds only the comparison (candidate 1
    // refused these with 42703).
    let db = seeded_t();
    assert_eq!(
        column_names(&db, "SELECT count(*) AS Total FROM t ORDER BY total"),
        vec!["Total"]
    );
    assert_eq!(column_names(&db, "SELECT id AS Foo FROM t ORDER BY foo"), vec!["Foo"]);
    assert_eq!(ids(&db, "SELECT id AS Foo FROM t ORDER BY foo DESC"), vec![1, 2]);
    let rows = assert_rows(&db, "SELECT id AS Foo FROM t ORDER BY foo DESC", 2);
    assert_eq!(as_i64(&rows[0].values[0]), 2, "…and it is actually sorted by the alias");
    assert_eq!(
        column_names(&db, r#"SELECT id AS "Foo" FROM t ORDER BY "Foo""#),
        vec!["Foo"]
    );
    assert_eq!(
        column_names(&db, "SELECT v AS Grp, count(*) AS N FROM t GROUP BY v ORDER BY n"),
        vec!["Grp", "N"]
    );
}

// ===========================================================================
// Candidate 2 — a VIEW's alias resolves next to a base table with the same
// column names (same mechanism as a sub-select)
// ===========================================================================

#[test]
fn view_column_shadowed_by_a_base_table_resolves_through_its_alias() {
    let db = seeded_t();
    db.execute("CREATE VIEW myview AS SELECT id, v FROM t")
        .expect("create view");
    assert_eq!(
        ids(&db, "SELECT myview.id FROM t JOIN myview ON myview.id = t.id"),
        vec![1, 2]
    );
    let rows = assert_rows(
        &db,
        "SELECT v1.v FROM t JOIN myview v1 ON v1.id = t.id WHERE t.id = 1",
        1,
    );
    assert_eq!(as_text(&rows[0].values[0]), "a");
    assert_eq!(
        column_names(&db, "SELECT v1.* FROM myview v1 JOIN t ON t.id = v1.id"),
        vec!["id", "v"]
    );
}

// ===========================================================================
// Candidate 3 (M2+M3) — materialized views: every unshadowed alias-qualified
// reference is rewritten to the bare name before the plan is stored (REFRESH
// after a close + reopen re-executes the stored bytes); only the genuinely
// shadowed shape is refused, with the workaround named
// ===========================================================================

/// A file-backed store in a scratch directory, so the MV plan really is
/// re-read from disk after `drop` + reopen.
fn scratch_store() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8 path").to_string();
    (dir, path)
}

fn open_store(path: &str) -> EmbeddedDatabase {
    EmbeddedDatabase::new(path).expect("open store")
}

/// Text-family scalar of `sql` as i64; the params family must agree.
fn scalar_i64(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = assert_rows(db, sql, 1);
    as_i64(&rows[0].values[0])
}

#[test]
fn materialized_view_over_an_aggregate_of_an_aliased_sub_select_survives_reopen_and_refresh() {
    // (i) `sum(s.x)` — the reference lives in an aggregate argument, which the
    // candidate-2 guard never walked: the MV was accepted, materialized once
    // and failed at the first REFRESH because the stamp is not persisted.
    let (_dir, path) = scratch_store();
    {
        let db = open_store(&path);
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, x INT)")
            .expect("create t");
        db.execute("INSERT INTO t VALUES (1, 10), (2, 20)").expect("seed t");
        db.execute("CREATE MATERIALIZED VIEW m AS SELECT sum(s.x) AS total FROM (SELECT x FROM t) s")
            .expect("unshadowed alias in an aggregate argument: accepted");
        assert_eq!(scalar_i64(&db, "SELECT total FROM m"), 30);
        db.execute("INSERT INTO t VALUES (3, 30)").expect("insert");
        db.execute("REFRESH MATERIALIZED VIEW m")
            .expect("refresh before reopen");
        assert_eq!(scalar_i64(&db, "SELECT total FROM m"), 60);
        db.close().expect("close");
    }
    let db = open_store(&path);
    db.execute("INSERT INTO t VALUES (4, 40)").expect("insert after reopen");
    db.execute("REFRESH MATERIALIZED VIEW m")
        .expect("REFRESH after reopen re-executes the STORED plan, which must not need the stamp");
    assert_eq!(scalar_i64(&db, "SELECT total FROM m"), 100);
}

#[test]
fn materialized_view_over_an_aliased_view_survives_reopen_and_refresh() {
    // (ii) `SELECT v.id FROM myview v` — the common unshadowed shape
    // candidate 2 refused with 0A000; (iii) an MV over a view whose OWN body
    // qualifies a nested sub-select.
    let (_dir, path) = scratch_store();
    {
        let db = open_store(&path);
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .expect("create t");
        db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')").expect("seed t");
        db.execute("CREATE VIEW myview AS SELECT id, v FROM t").expect("view");
        db.execute("CREATE MATERIALIZED VIEW m_view AS SELECT v.id, v.v FROM myview v WHERE v.id > 0")
            .expect("(ii) an MV over an aliased view is accepted");
        assert_eq!(ids(&db, "SELECT id FROM m_view"), vec![1, 2]);
        db.execute("CREATE VIEW v2 AS SELECT s.id AS sid, s.v AS sv FROM (SELECT id, v FROM t) s")
            .expect("view with an internally qualified sub-select");
        db.execute("CREATE MATERIALIZED VIEW m_v2 AS SELECT * FROM v2")
            .expect("(iii) an MV over a view whose body qualifies a nested sub-select is accepted");
        assert_eq!(ids(&db, "SELECT sid FROM m_v2"), vec![1, 2]);
        db.execute("CREATE MATERIALIZED VIEW m_total AS SELECT s.total FROM (SELECT count(*) AS total FROM t) s")
            .expect("`s.total` over an aggregating sub-select is accepted");
        assert_eq!(scalar_i64(&db, "SELECT total FROM m_total"), 2);
        db.close().expect("close");
    }
    let db = open_store(&path);
    db.execute("INSERT INTO t VALUES (3, 'c')")
        .expect("insert after reopen");
    for mv in ["m_view", "m_v2", "m_total"] {
        db.execute(&format!("REFRESH MATERIALIZED VIEW {mv}"))
            .unwrap_or_else(|e| panic!("REFRESH {mv} after reopen must re-execute the stored plan: {e}"));
    }
    assert_eq!(ids(&db, "SELECT id FROM m_view"), vec![1, 2, 3]);
    assert_eq!(ids(&db, "SELECT sid FROM m_v2"), vec![1, 2, 3]);
    assert_eq!(scalar_i64(&db, "SELECT total FROM m_total"), 3);
}

#[test]
fn materialized_view_over_a_shadowed_alias_is_refused_with_the_workaround_which_works() {
    // (iv) the genuinely shadowed shape: `s.id` where `t` ALSO carries `id`.
    // The stored plan cannot carry the stamp and the bare name is ambiguous,
    // so it is refused at CREATE with the workaround named — and the
    // workaround spelling creates, survives a reopen and refreshes.
    let (_dir, path) = scratch_store();
    {
        let db = open_store(&path);
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .expect("create t");
        db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')").expect("seed t");
        let refused = db
            .execute("CREATE MATERIALIZED VIEW m AS SELECT s.id FROM t JOIN (SELECT id FROM t) s ON s.id = t.id")
            .err()
            .map(|e| e.to_string())
            .expect("a shadowed alias-qualified reference must be refused");
        assert!(
            refused.contains("materialized view cannot reference a sub-select or view by its alias"),
            "{refused}"
        );
        assert!(refused.contains("alias the column inside the sub-select"), "{refused}");
        // The same shape in an aggregate argument is refused too (not
        // materialized-then-broken as in candidate 2).
        let refused = db
            .execute("CREATE MATERIALIZED VIEW m AS SELECT count(s.id) FROM t JOIN (SELECT id FROM t) s ON s.id = t.id")
            .err()
            .map(|e| e.to_string())
            .expect("shadowed reference inside an aggregate argument");
        assert!(refused.contains("alias the column inside the sub-select"), "{refused}");
        assert!(
            db.query("SELECT 1 FROM m", &[]).is_err(),
            "nothing was materialized by a refused CREATE"
        );
        db.execute(
            "CREATE MATERIALIZED VIEW m2 AS SELECT s.sid FROM t JOIN (SELECT id AS sid FROM t) s ON s.sid = t.id",
        )
        .expect("the workaround spelling (unique bare name) is accepted, alias-qualified");
        assert_eq!(ids(&db, "SELECT sid FROM m2"), vec![1, 2]);
        db.close().expect("close");
    }
    let db = open_store(&path);
    db.execute("INSERT INTO t VALUES (3, 'c')")
        .expect("insert after reopen");
    db.execute("REFRESH MATERIALIZED VIEW m2")
        .expect("refresh m2 after reopen");
    assert_eq!(ids(&db, "SELECT sid FROM m2"), vec![1, 2, 3]);
}

// ===========================================================================
// Candidate 3 (M1) — a derived alias equal to the REAL name of an earlier
// ALIASED base table resolves to the derived table at runtime
// ===========================================================================

#[test]
fn derived_alias_equal_to_an_aliased_base_tables_real_name_resolves_to_the_derived_table() {
    // `FROM u AS a JOIN (SELECT id FROM t) u` is legal PostgreSQL: the alias
    // `a` hides u's real name, so `u.id` names the derived table. Candidate 2
    // matched EITHER the alias or the real name in one positional pass and
    // answered `u.id` with a's column (silent wrong rows). Values are
    // distinct on purpose: u's ids are 11/12, t's are 1/2.
    let db = seeded_t();
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, tid INT)")
        .expect("create u");
    db.execute("INSERT INTO u VALUES (11, 1), (12, 2)").expect("seed u");
    // Direct equi-join key path (hash join key resolution).
    assert_eq!(
        ids(&db, "SELECT u.id FROM u AS a JOIN (SELECT id FROM t) u ON u.id = a.tid"),
        vec![1, 2],
        "u.id is the DERIVED table's column"
    );
    let rows = assert_rows(
        &db,
        "SELECT a.id, u.id FROM u AS a JOIN (SELECT id FROM t) u ON u.id = a.tid ORDER BY a.id",
        2,
    );
    assert_eq!((as_i64(&rows[0].values[0]), as_i64(&rows[0].values[1])), (11, 1));
    assert_eq!((as_i64(&rows[1].values[0]), as_i64(&rows[1].values[1])), (12, 2));
    // Evaluator path (the ON clause is not a plain column pair) and a WHERE
    // on the derived alias: still the derived table, still the right rows.
    assert_eq!(
        ids(
            &db,
            "SELECT u.id FROM u AS a JOIN (SELECT id FROM t) u ON u.id + 10 = a.id"
        ),
        vec![1, 2]
    );
    assert_eq!(
        ids(
            &db,
            "SELECT u.id FROM u AS a JOIN (SELECT id FROM t) u ON u.id = a.tid WHERE u.id = 2"
        ),
        vec![2]
    );
    assert_eq!(
        ids(
            &db,
            "SELECT a.id FROM u AS a JOIN (SELECT id FROM t) u ON u.id = a.tid WHERE u.id = 2"
        ),
        vec![12]
    );
    // The lenient real-name spelling of an aliased base table keeps working
    // when nothing else claims the name.
    assert_eq!(ids(&db, "SELECT u.id FROM u AS a WHERE u.tid = 1"), vec![11]);
}

// ===========================================================================
// Candidate 3 (m1) — `UPDATE … SET col = DEFAULT`
// ===========================================================================

#[test]
fn update_set_default_applies_the_declared_default_or_null_on_both_families() {
    // sqlparser lowers the bare `DEFAULT` to `Identifier("DEFAULT")`; candidate
    // 2 refused it as an unknown column (42703). PostgreSQL applies the
    // column's default — NULL when there is none.
    for (params_family, family) in FAMILIES {
        let db = mem_db();
        db.execute("CREATE TABLE d (id INT PRIMARY KEY, v INT DEFAULT 7, w INT)")
            .expect("create d");
        db.execute("INSERT INTO d VALUES (1, 1, 1)").expect("seed d");
        let exec = |sql: &str| -> Result<u64, String> {
            if params_family {
                db.execute_params(sql, &[]).map_err(|e| e.to_string())
            } else {
                db.execute(sql).map_err(|e| e.to_string())
            }
        };
        assert_eq!(
            exec("UPDATE d SET v = DEFAULT WHERE id = 1").unwrap_or_else(|e| panic!("[{family}] {e}")),
            1,
            "[{family}]"
        );
        let rows = assert_rows(&db, "SELECT v, w FROM d WHERE id = 1", 1);
        assert_eq!(as_i64(&rows[0].values[0]), 7, "[{family}] the declared default");
        assert_eq!(as_i64(&rows[0].values[1]), 1, "[{family}] untouched");
        assert_eq!(
            exec("UPDATE d SET w = DEFAULT, v = 3 WHERE id = 1").unwrap_or_else(|e| panic!("[{family}] {e}")),
            1
        );
        let rows = assert_rows(&db, "SELECT v, w FROM d WHERE id = 1", 1);
        assert_eq!(as_i64(&rows[0].values[0]), 3, "[{family}]");
        assert_eq!(rows[0].values[1], Value::Null, "[{family}] no declared default: NULL");
        // A quoted "default" is an identifier, and there is no such column.
        match exec(r#"UPDATE d SET v = "default" WHERE id = 1"#) {
            Ok(n) => panic!("[{family}] a quoted \"default\" must be an unknown column, got Ok({n})"),
            Err(msg) => assert!(msg.contains(UNDEFINED_COLUMN), "[{family}] {msg}"),
        }
        assert!(!column_exists(&db, "d", "default"));
    }
}

// ===========================================================================
// Candidate 3 (m2) — GROUP BY folds an output alias to its expression;
// HAVING never accepts an output alias
// ===========================================================================

#[test]
fn group_by_output_alias_groups_by_the_items_expression_and_having_alias_is_refused() {
    let db = seeded_t();
    db.execute("INSERT INTO t VALUES (3, 'a')").expect("third row");
    // Case-folded (candidate 2 passed the plan-time check and missed at runtime).
    let rows = assert_rows(
        &db,
        "SELECT v AS Grp, count(*) AS n FROM t GROUP BY grp ORDER BY grp",
        2,
    );
    assert_eq!(as_text(&rows[0].values[0]), "a");
    assert_eq!(as_i64(&rows[0].values[1]), 2);
    assert_eq!(as_text(&rows[1].values[0]), "b");
    assert_eq!(as_i64(&rows[1].values[1]), 1);
    assert_eq!(
        column_names(&db, "SELECT v AS Grp, count(*) AS n FROM t GROUP BY grp"),
        vec!["Grp", "n"],
        "the RowDescription keeps the alias as written"
    );
    // Exact-case alias of an EXPRESSION (`a + b AS s … GROUP BY s`): the key
    // folds to the expression, which the select list then meets as a group key.
    let rows = assert_rows(
        &db,
        "SELECT id + 100 AS k, count(*) AS n FROM t GROUP BY k ORDER BY k",
        3,
    );
    assert_eq!(as_i64(&rows[0].values[0]), 101);
    assert_eq!(as_i64(&rows[0].values[1]), 1);
    assert_eq!(as_i64(&rows[2].values[0]), 103);
    // A FROM column of the same name wins over the alias (PostgreSQL): this
    // groups by t.v (two groups), not by the aggregate the alias names.
    assert_eq!(ids(&db, "SELECT count(*) AS v FROM t GROUP BY v"), vec![1, 2]);
    // HAVING: an output alias is 42703 at plan time — never a runtime miss —
    // while the aggregate itself keeps working.
    assert_refused(
        &db,
        "SELECT v, count(*) AS n FROM t GROUP BY v HAVING n > 1",
        UNDEFINED_COLUMN,
    );
    assert_refused(
        &db,
        "SELECT v AS Grp, count(*) FROM t GROUP BY grp HAVING grp = 'a'",
        UNDEFINED_COLUMN,
    );
    assert_eq!(
        ids(&db, "SELECT count(*) FROM t GROUP BY v HAVING count(*) > 1"),
        vec![2]
    );
    // A QUOTED alias is exact-only (PostgreSQL): `"Grp"` folds, `grp` does not.
    let rows = assert_rows(
        &db,
        "SELECT v AS \"Grp\", count(*) AS n FROM t GROUP BY \"Grp\" ORDER BY \"Grp\"",
        2,
    );
    assert_eq!(as_text(&rows[0].values[0]), "a");
    assert_eq!(as_i64(&rows[0].values[1]), 2);
    assert_refused(
        &db,
        "SELECT v AS \"Grp\", count(*) FROM t GROUP BY grp",
        UNDEFINED_COLUMN,
    );
    // Still refused: a key that names nothing.
    assert_refused(&db, "SELECT v, count(*) FROM t GROUP BY nosuch", UNDEFINED_COLUMN);
}

// ===========================================================================
// Candidate 3 (m5) — a filter on a window-function alias stays ABOVE the
// derived table's projection
// ===========================================================================

#[test]
fn row_number_pagination_idiom_filters_on_the_window_result() {
    // The alias strip made the qualified spelling eligible for pushdown, and
    // the pushdown substituted the window expression into a Filter BELOW the
    // projection, where it has no meaning per input row.
    let db = seeded_t();
    db.execute("INSERT INTO t VALUES (3, 'c')").expect("third row");
    let idiom = "(SELECT id, row_number() OVER (ORDER BY id DESC) AS rn FROM t) s";
    assert_eq!(ids(&db, &format!("SELECT s.id FROM {idiom} WHERE s.rn = 1")), vec![3]);
    assert_eq!(ids(&db, &format!("SELECT id FROM {idiom} WHERE rn = 1")), vec![3]);
    assert_eq!(ids(&db, &format!("SELECT * FROM {idiom} WHERE s.rn = 1")), vec![3]);
    assert_eq!(
        ids(&db, &format!("SELECT s.id FROM {idiom} WHERE s.rn BETWEEN 2 AND 3")),
        vec![1, 2]
    );
    let rows = assert_rows(&db, &format!("SELECT s.id, s.rn FROM {idiom} WHERE s.rn = 2"), 1);
    assert_eq!(as_i64(&rows[0].values[0]), 2);
    assert_eq!(as_i64(&rows[0].values[1]), 2);
    // A predicate on a PLAIN column of a window sub-select stays above the
    // projection too: the window is numbered over the whole input (PostgreSQL
    // evaluates the outer WHERE after the sub-select's window), so id 2 keeps
    // rn = 2 instead of being renumbered 1 over a pre-filtered input.
    let rows = assert_rows(&db, &format!("SELECT s.id, s.rn FROM {idiom} WHERE s.id = 2"), 1);
    assert_eq!(as_i64(&rows[0].values[0]), 2);
    assert_eq!(as_i64(&rows[0].values[1]), 2);
}

// ===========================================================================
// Candidate 4 (F1) — hash-join EXPRESSION keys decide their side ONCE
// ===========================================================================

#[test]
fn hash_join_expression_keys_resolve_their_side_at_construction_not_per_tuple() {
    // Candidate 3 fixed the plain-column key path only. `ON u.id + 10 = a.id`
    // went through `HashJoinOperator::extract_join_columns`, which assigned an
    // operand to a side PER TUPLE by "evaluate the natural operand; on Err
    // evaluate the other": the left input `u AS a` evaluated `u.id + 10`
    // through the real-name fallback (a's own id + 10 = 21/22) while the
    // right input evaluated `u.id + 10` too (11/12) — the keys never met and
    // the join returned no rows. Sides are now decided once at construction
    // (alias tier first on either side; the real name only when no side
    // carries the alias). Values distinct on purpose: u 11/12, t 1/2.
    let db = seeded_t();
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, tid INT)")
        .expect("create u");
    db.execute("INSERT INTO u VALUES (11, 1), (12, 2)").expect("seed u");
    let pairs = |sql: &str| -> Vec<(i64, i64)> {
        let mut agreed: Option<Vec<(i64, i64)>> = None;
        for (params_family, family) in FAMILIES {
            let out =
                run(&db, sql, params_family).unwrap_or_else(|e| panic!("[{family}] `{sql}` must plan and run: {e}"));
            let mut got: Vec<(i64, i64)> = out
                .iter()
                .map(|r| (as_i64(&r.values[0]), as_i64(&r.values[1])))
                .collect();
            got.sort_unstable();
            if let Some(prev) = &agreed {
                assert_eq!(prev, &got, "both families must agree on `{sql}`");
            }
            agreed = Some(got);
        }
        agreed.unwrap_or_default()
    };
    let expected = vec![(11, 1), (12, 2)];
    // The shapes the c3 proof run observed with 0 rows.
    assert_eq!(
        pairs("SELECT a.id, u.id FROM u AS a JOIN (SELECT id FROM t) u ON u.id + 10 = a.id"),
        expected,
        "(a-value, derived-value)"
    );
    assert_eq!(
        ids(
            &db,
            "SELECT u.id FROM u AS a JOIN (SELECT id FROM t) u ON u.id + 10 = a.id WHERE u.id = 2"
        ),
        vec![2]
    );
    // Operands reversed.
    assert_eq!(
        pairs("SELECT a.id, u.id FROM u AS a JOIN (SELECT id FROM t) u ON a.id = u.id + 10"),
        expected
    );
    // Composite: one plain column pair and one expression pair.
    assert_eq!(
        pairs("SELECT a.id, u.id FROM u AS a JOIN (SELECT id FROM t) u ON u.id = a.tid AND u.id + 10 = a.id"),
        expected
    );
    // Control: the alias tier matches on the LEFT input.
    assert_eq!(
        pairs("SELECT a.id, u.id FROM (SELECT id FROM t) u JOIN u AS a ON u.id + 10 = a.id"),
        expected
    );
    // Self-join sanity: each operand's alias hits exactly one side.
    assert_eq!(
        pairs("SELECT x.id, y.id FROM t x JOIN t y ON x.id + 1 = y.id"),
        vec![(1, 2)]
    );
    // A term with a literal on one side keys the other side on the literal
    // (a filter), it is never guessed onto the wrong input.
    assert_eq!(
        pairs("SELECT a.id, u.id FROM u AS a JOIN (SELECT id FROM t) u ON u.id = a.tid AND u.id = 2"),
        vec![(12, 2)]
    );
    // Non-equality residual next to an expression key still filters.
    assert_eq!(
        pairs("SELECT a.id, u.id FROM u AS a JOIN (SELECT id FROM t) u ON u.id + 10 = a.id AND a.id > 11"),
        vec![(12, 2)]
    );
}

// ===========================================================================
// Candidate 4 (F2) — every UPDATE family resolves SET / WHERE before running
// ===========================================================================

#[test]
fn update_fast_path_and_on_conflict_refuse_an_unknown_column_before_touching_a_row() {
    // The text family's PK point-update fast path (`try_fast_update`) skips
    // the parser, so it never ran `update_to_plan`'s resolution; through
    // candidate 3 it looked the row up FIRST and answered `Ok(0)` for a
    // missing key — any key on an empty table, or `WHERE id = 99` — without
    // reading the SET value. And `ON CONFLICT DO UPDATE` was lowered with no
    // scope at all, so its SET / WHERE failed only per CONFLICTING row.
    // PostgreSQL: 42703 at plan time, regardless of row count.
    let db = empty_t();
    assert!(!column_exists(&db, "t", "nosuch"), "probe through the catalog");
    let exec = |sql: &str, params_family: bool| -> Result<u64, String> {
        if params_family {
            db.execute_params(sql, &[]).map_err(|e| e.to_string())
        } else {
            db.execute(sql).map_err(|e| e.to_string())
        }
    };
    let refused = |sql: &str| {
        for (params_family, family) in FAMILIES {
            match exec(sql, params_family) {
                Ok(n) => panic!("[{family}] `{sql}` must be refused before touching a row, got Ok({n})"),
                Err(msg) => assert!(
                    msg.contains(UNDEFINED_COLUMN) && msg.contains("nosuch"),
                    "[{family}] `{sql}` was refused, but not as an unknown column: {msg}"
                ),
            }
        }
    };
    const REFUSED: [&str; 13] = [
        "UPDATE t SET v = nosuch",
        "UPDATE t SET v = 'x' WHERE nosuch = 1",
        "UPDATE t SET v = t.nosuch WHERE id = 1",
        "DELETE FROM t WHERE nosuch = 1",
        "DELETE FROM t AS x WHERE x.nosuch = 1",
        "UPDATE t SET v = nosuch WHERE id = 1",
        "UPDATE t SET v = 'x' WHERE id = 1 AND nosuch = 1",
        "UPDATE t SET v = nosuch WHERE id = 99",
        "INSERT INTO t VALUES (1, 'z') ON CONFLICT (id) DO UPDATE SET v = t.nosuch",
        "INSERT INTO t VALUES (1, 'z') ON CONFLICT (id) DO UPDATE SET v = nosuch",
        "INSERT INTO t VALUES (1, 'z') ON CONFLICT (id) DO UPDATE SET v = excluded.nosuch",
        "INSERT INTO t VALUES (1, 'z') ON CONFLICT (id) DO UPDATE SET v = excluded.v WHERE t.nosuch = 1",
        "INSERT INTO t AS x VALUES (1, 'z') ON CONFLICT (id) DO UPDATE SET v = x.nosuch",
    ];
    // Empty table: refused, and a refused INSERT … ON CONFLICT wrote nothing.
    for sql in REFUSED {
        refused(sql);
    }
    assert_rows(&db, "SELECT id FROM t", 0);
    // Non-empty table: the same refusals, and the rows are untouched.
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')").expect("seed t");
    for sql in REFUSED {
        refused(sql);
    }
    let rows = assert_rows(&db, "SELECT id, v FROM t ORDER BY id", 2);
    assert_eq!(
        (as_i64(&rows[0].values[0]), as_text(&rows[0].values[1])),
        (1, "a".to_string())
    );
    assert_eq!(
        (as_i64(&rows[1].values[0]), as_text(&rows[1].values[1])),
        (2, "b".to_string())
    );
    // Positive controls, text family (the fast path) then params family:
    // a literal, a self-arithmetic on the SET column, and the upsert idiom
    // with EXCLUDED and the target's alias all keep working.
    db.execute("CREATE TABLE n (id INT PRIMARY KEY, k INT)")
        .expect("create n");
    db.execute("INSERT INTO n VALUES (1, 5)").expect("seed n");
    assert_eq!(
        exec("UPDATE t SET v = 'x' WHERE id = 1", false).expect("literal fast path"),
        1
    );
    assert_eq!(
        exec("UPDATE n SET k = k + 2 WHERE id = 1", false).expect("self-arithmetic fast path"),
        1
    );
    assert_eq!(
        exec("UPDATE t SET v = 'y' WHERE id = 2", true).expect("params family"),
        1
    );
    assert_eq!(
        exec("UPDATE n SET k = k * 2 WHERE id = 1", true).expect("params family"),
        1
    );
    assert_eq!(
        exec("UPDATE t SET v = 'q' WHERE id = 99", false).expect("missing key, literal"),
        0
    );
    assert_eq!(
        exec("UPDATE t SET v = 'q' WHERE id = 99", true).expect("missing key, params"),
        0
    );
    assert_eq!(
        exec(
            "INSERT INTO t VALUES (1, 'z') ON CONFLICT (id) DO UPDATE SET v = excluded.v",
            false
        )
        .expect("upsert"),
        1
    );
    assert_eq!(
        exec(
            "INSERT INTO t AS x VALUES (2, 'w') ON CONFLICT (id) DO UPDATE SET v = excluded.v WHERE x.id = 2",
            true
        )
        .expect("aliased upsert"),
        1
    );
    let rows = assert_rows(&db, "SELECT id, v FROM t ORDER BY id", 2);
    assert_eq!(as_text(&rows[0].values[1]), "z");
    assert_eq!(as_text(&rows[1].values[1]), "w");
    assert_eq!(ids(&db, "SELECT k FROM n"), vec![14]);
}

// ===========================================================================
// Candidate 5 — the c4 review's MAJOR and minors
// ===========================================================================

fn opt_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Null => None,
        other => Some(as_i64(other)),
    }
}

/// Sorted (first, second) integer pairs of `sql`, both families (must agree).
fn int_pairs(db: &EmbeddedDatabase, sql: &str) -> Vec<(i64, i64)> {
    let mut agreed: Option<Vec<(i64, i64)>> = None;
    for (params_family, family) in FAMILIES {
        let out = run(db, sql, params_family).unwrap_or_else(|e| panic!("[{family}] `{sql}` must plan and run: {e}"));
        let mut got: Vec<(i64, i64)> = out
            .iter()
            .map(|r| (as_i64(&r.values[0]), as_i64(&r.values[1])))
            .collect();
        got.sort_unstable();
        if let Some(prev) = &agreed {
            assert_eq!(prev, &got, "both families must agree on `{sql}`");
        }
        agreed = Some(got);
    }
    agreed.unwrap_or_default()
}

/// Sorted rows of `sql` as nullable integers (`NULL` → `None`), both
/// families (must agree).
fn nullable_int_rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<Option<i64>>> {
    let mut agreed: Option<Vec<Vec<Option<i64>>>> = None;
    for (params_family, family) in FAMILIES {
        let out = run(db, sql, params_family).unwrap_or_else(|e| panic!("[{family}] `{sql}` must plan and run: {e}"));
        let mut got: Vec<Vec<Option<i64>>> = out.iter().map(|r| r.values.iter().map(opt_i64).collect()).collect();
        got.sort();
        if let Some(prev) = &agreed {
            assert_eq!(prev, &got, "both families must agree on `{sql}`");
        }
        agreed = Some(got);
    }
    agreed.unwrap_or_default()
}

#[test]
fn hash_join_case_distinct_quoted_aliases_are_keyed_exactly_not_case_folded() {
    // `FROM t AS "A" JOIN t AS "a"` is two legal, case-distinct range
    // entries (a quoted identifier keeps its case). Candidate 4's key
    // resolver compared aliases ASCII-case-insensitively while the evaluator
    // is exact-case, so `"a".id` and `"A".id + 1` each fitted BOTH sides and
    // the natural-order guess keyed A.id = a.id + 1 — the pair (2, 1) where
    // PostgreSQL answers (1, 2). The exact-case pass now runs first on both
    // sides; a term whose operands still fit both sides is declined and
    // re-evaluated by the exact-case combined evaluator.
    let db = seeded_t();
    assert_eq!(
        int_pairs(
            &db,
            r#"SELECT "A".id, "a".id FROM t AS "A" JOIN t AS "a" ON "a".id = "A".id + 1"#
        ),
        vec![(1, 2)],
        r#"("A".id, "a".id)"#
    );
    assert_eq!(
        int_pairs(
            &db,
            r#"SELECT "A".id, "a".id FROM t AS "A" JOIN t AS "a" ON "A".id + 1 = "a".id"#
        ),
        vec![(1, 2)],
        "operands reversed"
    );
    // Plain-column keys under the same aliases (the direct index path).
    assert_eq!(
        int_pairs(
            &db,
            r#"SELECT "A".id, "a".id FROM t AS "A" JOIN t AS "a" ON "a".id = "A".id"#
        ),
        vec![(1, 1), (2, 2)]
    );
    // The unquoted twin keeps hashing (pinned structurally by
    // `sql::executor::join::gh29_c5_key_binding_tests`) and answers the same.
    assert_eq!(
        int_pairs(&db, "SELECT a.id, b.id FROM t AS a JOIN t AS b ON b.id = a.id + 1"),
        vec![(1, 2)]
    );
}

#[test]
fn right_and_full_joins_null_extend_build_rows_that_fail_a_declined_term() {
    // `jb.x = jb.y` has both operands on one side, so it leaves the hash
    // key. The hash join tracks unmatched build rows per key BUCKET: once
    // `ja` 1 matched b's (1, 1, 1), the bucket was "matched" and b's
    // (1, 1, 2) — which fails x = y — was dropped instead of NULL-extended.
    // A declined term under RIGHT / FULL now takes the nested-loop join
    // (per-tuple matched bitmap).
    let db = mem_db();
    db.execute("CREATE TABLE ja (id INT PRIMARY KEY)").expect("create ja");
    db.execute("INSERT INTO ja VALUES (1), (3)").expect("seed ja");
    db.execute("CREATE TABLE jb (id INT, x INT, y INT)").expect("create jb");
    db.execute("INSERT INTO jb VALUES (1, 1, 1), (1, 1, 2), (2, 5, 5)")
        .expect("seed jb");
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT ja.id, jb.id, jb.x, jb.y FROM ja RIGHT JOIN jb ON ja.id = jb.id AND jb.x = jb.y"
        ),
        vec![
            vec![None, Some(1), Some(1), Some(2)],
            vec![None, Some(2), Some(5), Some(5)],
            vec![Some(1), Some(1), Some(1), Some(1)],
        ],
        "RIGHT JOIN: (ja.id, jb.id, jb.x, jb.y)"
    );
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT ja.id, jb.id, jb.x, jb.y FROM ja FULL JOIN jb ON ja.id = jb.id AND jb.x = jb.y"
        ),
        vec![
            vec![None, Some(1), Some(1), Some(2)],
            vec![None, Some(2), Some(5), Some(5)],
            vec![Some(1), Some(1), Some(1), Some(1)],
            vec![Some(3), None, None, None],
        ],
        "FULL JOIN: (ja.id, jb.id, jb.x, jb.y)"
    );
    // Same shape, INNER and LEFT: the declined term filters, nothing is
    // NULL-extended that should not be.
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT ja.id, jb.y FROM ja JOIN jb ON ja.id = jb.id AND jb.x = jb.y"
        ),
        vec![vec![Some(1), Some(1)]]
    );
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT ja.id, jb.y FROM ja LEFT JOIN jb ON ja.id = jb.id AND jb.x = jb.y"
        ),
        vec![vec![Some(1), Some(1)], vec![Some(3), None]]
    );
}

#[test]
fn a_declined_subquery_term_is_materialized_and_evaluated_never_swallowed() {
    // A scalar subquery in an `=` term is declined from the key (the binder
    // does not walk it). Candidate 4 handed the hash join the un-materialized
    // condition and mapped the combined evaluator's error to "no match"
    // (`unwrap_or(false)`): 0 rows, silently. The condition is now
    // materialized before the join is built and an evaluation error is the
    // statement's error.
    let db = mem_db();
    db.execute("CREATE TABLE sa (id INT PRIMARY KEY, x INT)")
        .expect("create sa");
    db.execute("INSERT INTO sa VALUES (1, 10), (2, 20)").expect("seed sa");
    db.execute("CREATE TABLE sb (id INT PRIMARY KEY, x INT)")
        .expect("create sb");
    db.execute("INSERT INTO sb VALUES (1, 20), (2, 10)").expect("seed sb");
    db.execute("CREATE TABLE sc (x INT)").expect("create sc");
    db.execute("INSERT INTO sc VALUES (20)").expect("seed sc");
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT sa.id, sb.id FROM sa JOIN sb ON sa.id = sb.id AND sa.x = (SELECT max(x) FROM sc)"
        ),
        vec![vec![Some(2), Some(2)]]
    );
    // LEFT keeps a left-side term in the ON clause (it may not be pushed
    // below the join), so this one is declined and re-evaluated per pair.
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT sa.id, sb.id FROM sa LEFT JOIN sb ON sa.id = sb.id AND sa.x = (SELECT max(x) FROM sc)"
        ),
        vec![vec![Some(1), None], vec![Some(2), Some(2)]]
    );
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT sa.id, sb.id FROM sa LEFT JOIN sb ON sa.id = sb.id AND sb.x = (SELECT max(x) FROM sc)"
        ),
        vec![vec![Some(1), Some(1)], vec![Some(2), None]]
    );
}

#[test]
fn a_literal_keyed_term_is_left_to_the_coercing_evaluator() {
    // Candidate 4 keyed `5 = pa.pn` on the raw literal: the hash key compares
    // values with no int / numeric / float coercion, so `Int(5)` never met
    // `Numeric(5.00)` and the term matched nothing (main matched everything —
    // wrong the other way). A term with a literal operand now leaves the key;
    // the column pair still hashes and the evaluator, which coerces, decides.
    let db = mem_db();
    db.execute("CREATE TABLE pa (id INT PRIMARY KEY, pn NUMERIC(10, 2), pd DOUBLE PRECISION)")
        .expect("create pa");
    db.execute("INSERT INTO pa VALUES (1, 5.00, 5.0), (2, 7.50, 7.5)")
        .expect("seed pa");
    db.execute("CREATE TABLE pb (id INT PRIMARY KEY)").expect("create pb");
    db.execute("INSERT INTO pb VALUES (1), (2)").expect("seed pb");
    for column in ["pn", "pd"] {
        for term in [format!("5 = pa.{column}"), format!("pa.{column} = 5")] {
            assert_eq!(
                nullable_int_rows(
                    &db,
                    &format!("SELECT pa.id, pb.id FROM pa JOIN pb ON pa.id = pb.id AND {term}")
                ),
                vec![vec![Some(1), Some(1)]],
                "INNER, `{term}`"
            );
            // LEFT keeps the left-side term in the ON clause.
            assert_eq!(
                nullable_int_rows(
                    &db,
                    &format!("SELECT pa.id, pb.id FROM pa LEFT JOIN pb ON pa.id = pb.id AND {term}")
                ),
                vec![vec![Some(1), Some(1)], vec![Some(2), None]],
                "LEFT, `{term}`"
            );
            // …and RIGHT keeps a right-side term (the declined term under an
            // outer join that preserves the build side: nested-loop join).
            assert_eq!(
                nullable_int_rows(
                    &db,
                    &format!("SELECT pa.id, pb.id FROM pb RIGHT JOIN pa ON pb.id = pa.id AND {term}")
                ),
                vec![vec![Some(1), Some(1)], vec![Some(2), None]],
                "RIGHT, `{term}`"
            );
        }
    }
}

#[test]
fn a_missing_target_table_is_reported_as_such_whatever_the_set_spelling() {
    // A target the catalog cannot describe is recorded OPAQUE (an entry with
    // no columns accepts every name), so the SET / WHERE references are not
    // refused at plan time and the executor reports the missing table —
    // 42P01, never 42703 — for the bare and the qualified spelling, on
    // INSERT … ON CONFLICT DO UPDATE and on UPDATE.
    let db = empty_t();
    for sql in [
        "INSERT INTO nosuch VALUES (1, 'z') ON CONFLICT (id) DO UPDATE SET v = v + 1",
        "INSERT INTO nosuch VALUES (1, 'z') ON CONFLICT (id) DO UPDATE SET v = nosuch.v + 1",
        "UPDATE nosuch SET v = v + 1 WHERE id = 1",
        "UPDATE nosuch SET v = nosuch.v + 1 WHERE id = 1",
    ] {
        for (params_family, family) in FAMILIES {
            let out = if params_family {
                db.execute_params(sql, &[])
            } else {
                db.execute(sql)
            };
            match out {
                Ok(n) => panic!("[{family}] `{sql}` must fail on the missing table, got Ok({n})"),
                Err(e) => {
                    let msg = e.to_string();
                    assert!(
                        msg.contains("nosuch") && (msg.contains("does not exist") || msg.contains("not found")),
                        "[{family}] `{sql}` must name the missing table: {msg}"
                    );
                    assert!(
                        !msg.contains("Column"),
                        "[{family}] `{sql}` must not be refused as an unknown column: {msg}"
                    );
                }
            }
        }
    }
}

#[test]
fn update_arithmetic_on_a_text_column_is_refused_at_plan_time_even_for_a_missing_row() {
    // `UPDATE t SET v = v + 1` on a TEXT column is `operator does not exist:
    // text + integer` (42883) in PostgreSQL, whether or not a row matches.
    // Through candidate 4 the text family's PK fast path accepted the shape
    // for any column type, found no row and answered `Ok(0)`; with a row it
    // fell to the planner, whose executor errored only per MATCHED row. The
    // fast-path shape check now requires a numeric SET column, and the
    // planner refuses the shape before any row is looked up — both families.
    let db = seeded_t();
    db.execute("CREATE TABLE n (id INT PRIMARY KEY, k INT)")
        .expect("create n");
    db.execute("INSERT INTO n VALUES (1, 5)").expect("seed n");
    let exec = |sql: &str, params_family: bool| -> Result<u64, String> {
        if params_family {
            db.execute_params(sql, &[]).map_err(|e| e.to_string())
        } else {
            db.execute(sql).map_err(|e| e.to_string())
        }
    };
    for sql in [
        "UPDATE t SET v = v + 1 WHERE id = 99",
        "UPDATE t SET v = v + 1 WHERE id = 1",
        "UPDATE t SET v = 1 + v WHERE id = 99",
        "UPDATE t SET v = v * 2 WHERE id = 99",
        "UPDATE t SET v = t.v - 1 WHERE id = 99",
        "UPDATE t SET v = v + 1.5 WHERE id = 99",
    ] {
        for (params_family, family) in FAMILIES {
            match exec(sql, params_family) {
                Ok(n) => panic!("[{family}] `{sql}` must be refused, got Ok({n})"),
                Err(msg) => assert!(
                    msg.contains("operator does not exist:") && msg.contains("text"),
                    "[{family}] `{sql}` was refused, but not as an undefined operator: {msg}"
                ),
            }
        }
    }
    let rows = assert_rows(&db, "SELECT v FROM t ORDER BY id", 2);
    assert_eq!(as_text(&rows[0].values[0]), "a", "rows untouched");
    assert_eq!(as_text(&rows[1].values[0]), "b", "rows untouched");
    // Control: the numeric fast path keeps answering 0 for a missing key and
    // evaluating for a present one, on both families.
    assert_eq!(exec("UPDATE n SET k = k + 1 WHERE id = 99", false).expect("text"), 0);
    assert_eq!(exec("UPDATE n SET k = k + 1 WHERE id = 99", true).expect("params"), 0);
    assert_eq!(exec("UPDATE n SET k = k + 1 WHERE id = 1", false).expect("text"), 1);
    assert_eq!(exec("UPDATE n SET k = k + 1 WHERE id = 1", true).expect("params"), 1);
    assert_eq!(ids(&db, "SELECT k FROM n"), vec![7]);
}

#[test]
fn a_bare_name_a_derived_entry_carries_twice_is_ambiguous_not_the_first_slot() {
    // `s` carries `id` twice; the bare spelling is as ambiguous as `s.id`
    // (PostgreSQL 42702). Candidate 4's `refuse_unresolved` acted on
    // `UndefinedColumn` only, so the bare name read the first slot.
    let db = seeded_t();
    assert_refused(
        &db,
        "SELECT id FROM (SELECT a.id, b.id FROM t a JOIN t b ON a.id = b.id) s",
        AMBIGUOUS,
    );
    // Renamed positionally, it resolves.
    assert_eq!(
        ids(
            &db,
            "SELECT x FROM (SELECT a.id, b.id FROM t a JOIN t b ON a.id = b.id) s(x, y)"
        ),
        vec![1, 2]
    );
}

// ---------------------------------------------------------------------------
// GH#29 (candidate 6)
// ---------------------------------------------------------------------------

/// `na(id, a)` = {(1,10), (2,20)} and `nb(id, b)` = {(1,100), (3,300)} — two
/// inputs whose `id`s overlap in exactly ONE row, so a cartesian product
/// (4 rows) cannot be mistaken for the join (1 row).
fn natural_pair() -> EmbeddedDatabase {
    let db = mem_db();
    db.execute("CREATE TABLE na (id INT PRIMARY KEY, a INT)")
        .expect("create na");
    db.execute("INSERT INTO na VALUES (1, 10), (2, 20)").expect("seed na");
    db.execute("CREATE TABLE nb (id INT PRIMARY KEY, b INT)")
        .expect("create nb");
    db.execute("INSERT INTO nb VALUES (1, 100), (3, 300)").expect("seed nb");
    db
}

#[test]
fn natural_join_of_every_type_is_an_equi_join_not_a_cartesian_product() {
    // BLOCKER (inside this cycle; v4.31.1 answered these correctly). The
    // planner lowers `na NATURAL JOIN nb` to
    // `Column{None,"id"} = Column{None,"id"}` — it always has — and candidate
    // 5's key binder declined that as an "alias collision": the key came out
    // empty, the join fell to the nested loop, and the nested loop's
    // combined-schema binder resolved BOTH bare operands to the FIRST `id`
    // slot — `na.id = na.id`, true for every pair. The fix is the binder, and
    // only the binder: an all-unqualified double fit keeps the natural order
    // (lhs -> left input, rhs -> right input) instead of being declined.
    let db = natural_pair();
    assert_eq!(
        nullable_int_rows(&db, "SELECT na.id, na.a, nb.b FROM na NATURAL JOIN nb"),
        vec![vec![Some(1), Some(10), Some(100)]],
        "NATURAL JOIN must key on the shared column"
    );
    assert_eq!(
        nullable_int_rows(&db, "SELECT na.id, na.a, nb.b FROM na NATURAL LEFT JOIN nb"),
        vec![vec![Some(1), Some(10), Some(100)], vec![Some(2), Some(20), None]]
    );
    assert_eq!(
        nullable_int_rows(&db, "SELECT na.id, na.a, nb.b FROM na NATURAL RIGHT JOIN nb"),
        vec![vec![None, None, Some(300)], vec![Some(1), Some(10), Some(100)]]
    );
    assert_eq!(
        nullable_int_rows(&db, "SELECT na.id, na.a, nb.b FROM na NATURAL FULL JOIN nb"),
        vec![
            vec![None, None, Some(300)],
            vec![Some(1), Some(10), Some(100)],
            vec![Some(2), Some(20), None],
        ]
    );
    // `SELECT *` is the shape a client actually writes. The join column is
    // still emitted once per side (PostgreSQL merges it — pre-existing,
    // orthogonal to the cartesian bug), so assert the ROW COUNT, which is
    // what the cartesian product got wrong.
    assert_rows(&db, "SELECT * FROM na NATURAL JOIN nb", 1);
    assert_rows(&db, "SELECT * FROM na NATURAL LEFT JOIN nb", 2);
    assert_rows(&db, "SELECT * FROM na NATURAL FULL JOIN nb", 3);
}

#[test]
fn join_using_is_lowered_and_is_an_equi_join_not_a_cartesian_product() {
    // m6. `JoinConstraint::Using` fell to the planner's `_ => None`, so the
    // join ran with NO condition at all: a cross join on both families.
    let db = natural_pair();
    assert_eq!(
        nullable_int_rows(&db, "SELECT na.id, na.a, nb.b FROM na JOIN nb USING (id)"),
        vec![vec![Some(1), Some(10), Some(100)]]
    );
    assert_eq!(
        nullable_int_rows(&db, "SELECT na.id, na.a, nb.b FROM na LEFT JOIN nb USING (id)"),
        vec![vec![Some(1), Some(10), Some(100)], vec![Some(2), Some(20), None]]
    );
    assert_eq!(
        nullable_int_rows(&db, "SELECT na.id, na.a, nb.b FROM na RIGHT JOIN nb USING (id)"),
        vec![vec![None, None, Some(300)], vec![Some(1), Some(10), Some(100)]]
    );
    assert_eq!(
        nullable_int_rows(&db, "SELECT na.id, na.a, nb.b FROM na FULL JOIN nb USING (id)"),
        vec![
            vec![None, None, Some(300)],
            vec![Some(1), Some(10), Some(100)],
            vec![Some(2), Some(20), None],
        ]
    );
    assert_rows(&db, "SELECT * FROM na JOIN nb USING (id)", 1);
    // A USING column neither side carries is 42703, as in PostgreSQL — never
    // a silent cross join.
    assert_refused(&db, "SELECT * FROM na JOIN nb USING (nosuch)", "does not exist");
    // …and one only ONE side carries is refused too.
    assert_refused(&db, "SELECT * FROM na JOIN nb USING (a)", "does not exist");
}

#[test]
fn a_residual_on_term_under_an_outer_join_never_eats_the_null_extended_rows() {
    // m5. `equi + residual` built a hash join on the equi part and put the
    // residual in a post-join `FilterOperator`, which is legal for INNER and
    // WRONG for LEFT / RIGHT / FULL: the NULL-extended rows fail the filter
    // and vanish. `na LEFT JOIN nb ON na.id = nb.id AND nb.b > 1000` returned
    // ZERO rows on the parameterized family.
    let db = natural_pair();
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT na.id, nb.id FROM na LEFT JOIN nb ON na.id = nb.id AND nb.b > 1000"
        ),
        vec![vec![Some(1), None], vec![Some(2), None]]
    );
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT na.id, nb.id FROM na LEFT JOIN nb ON na.id = nb.id AND nb.b > 10"
        ),
        vec![vec![Some(1), Some(1)], vec![Some(2), None]]
    );
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT na.id, nb.id FROM nb RIGHT JOIN na ON na.id = nb.id AND nb.b > 1000"
        ),
        vec![vec![Some(1), None], vec![Some(2), None]]
    );
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT na.id, nb.id FROM na FULL JOIN nb ON na.id = nb.id AND nb.b > 1000"
        ),
        vec![
            vec![None, Some(1)],
            vec![None, Some(3)],
            vec![Some(1), None],
            vec![Some(2), None],
        ]
    );
    // INNER keeps the post-join filter, and it is still exactly equivalent.
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT na.id, nb.id FROM na JOIN nb ON na.id = nb.id AND nb.b > 1000"
        ),
        Vec::<Vec<Option<i64>>>::new()
    );
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT na.id, nb.id FROM na JOIN nb ON na.id = nb.id AND nb.b > 10"
        ),
        vec![vec![Some(1), Some(1)]]
    );
}

#[test]
fn a_declined_equality_term_is_never_swallowed_by_the_index_nested_loop() {
    // m4. The index nested loop took the FIRST equality of an `AND` chain and
    // emitted every indexed match, dropping every other term of the ON
    // clause. On the text family the optimizer pushes most such terms into a
    // scan — but it may NOT push a term kept on an outer join, and the
    // parameterized family runs no optimizer passes at all, so both showed it.
    let db = mem_db();
    db.execute("CREATE TABLE sa (id INT PRIMARY KEY, x INT)")
        .expect("create sa");
    db.execute("INSERT INTO sa VALUES (1, 10), (2, 20)").expect("seed sa");
    db.execute("CREATE TABLE sb (id INT PRIMARY KEY, x INT)")
        .expect("create sb");
    db.execute("INSERT INTO sb VALUES (1, 20), (2, 10)").expect("seed sb");
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT sa.id, sb.id FROM sa JOIN sb ON sa.id = sb.id AND sa.x = 20"
        ),
        vec![vec![Some(2), Some(2)]]
    );
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT sa.id, sb.id FROM sa LEFT JOIN sb ON sa.id = sb.id AND sa.x = 20"
        ),
        vec![vec![Some(1), None], vec![Some(2), Some(2)]]
    );
    // Two equality terms: the second one was dropped outright, so ids that
    // matched but whose `x` did not came back anyway.
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT sa.id, sb.id FROM sa JOIN sb ON sa.id = sb.id AND sa.x = sb.x"
        ),
        Vec::<Vec<Option<i64>>>::new()
    );
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT sa.id, sb.id FROM sa LEFT JOIN sb ON sa.id = sb.id AND sa.x = sb.x"
        ),
        vec![vec![Some(1), None], vec![Some(2), None]]
    );
}

#[test]
fn a_correlated_subquery_in_an_on_term_is_refused_never_read_as_null() {
    // m1. The ON condition is materialized once, before either input is
    // built, so a CORRELATED scalar subquery cannot be executed there.
    // Standing NULL in for it — which is right for drizzle's introspection
    // queries — makes the term silently never true, i.e. missing join rows
    // with no diagnostic. It is refused instead.
    let db = mem_db();
    db.execute("CREATE TABLE ca (id INT PRIMARY KEY, k INT, x INT)")
        .expect("create ca");
    db.execute("INSERT INTO ca VALUES (1, 1, 10), (2, 2, 20)")
        .expect("seed ca");
    db.execute("CREATE TABLE cb (id INT PRIMARY KEY)").expect("create cb");
    db.execute("INSERT INTO cb VALUES (1), (2)").expect("seed cb");
    db.execute("CREATE TABLE cc (k INT, x INT)").expect("create cc");
    db.execute("INSERT INTO cc VALUES (1, 10), (2, 99)").expect("seed cc");
    // LEFT, so the left-side term stays in the ON clause on BOTH families
    // (the optimizer may not push it below a LEFT join, and the parameterized
    // family runs no optimizer passes at all).
    assert_refused(
        &db,
        "SELECT ca.id, cb.id FROM ca LEFT JOIN cb ON ca.id = cb.id \
         AND ca.x = (SELECT max(x) FROM cc WHERE cc.k = ca.k)",
        "correlated subquery in JOIN ... ON is not supported",
    );
    // The UNCORRELATED spelling keeps returning the right rows.
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT ca.id, cb.id FROM ca LEFT JOIN cb ON ca.id = cb.id AND ca.x = (SELECT max(x) FROM cc)"
        ),
        vec![vec![Some(1), None], vec![Some(2), None]]
    );
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT ca.id, cb.id FROM ca LEFT JOIN cb ON ca.id = cb.id AND ca.x = (SELECT min(x) FROM cc)"
        ),
        vec![vec![Some(1), Some(1)], vec![Some(2), None]]
    );
}

// ===========================================================================
// Candidate 7 — the four majors and the minors the three c6 reviews found
// ===========================================================================

/// Rows of a PARAMETERIZED statement (the family that carries bind values;
/// `query` ignores its params argument), as nullable integers, sorted.
fn param_rows(db: &EmbeddedDatabase, sql: &str, params: &[Value]) -> Vec<Vec<Option<i64>>> {
    let out = db
        .query_params(sql, params)
        .unwrap_or_else(|e| panic!("[params] `{sql}` must plan and run: {e}"));
    let mut got: Vec<Vec<Option<i64>>> = out.iter().map(|r| r.values.iter().map(opt_i64).collect()).collect();
    got.sort();
    got
}

fn param_pair() -> EmbeddedDatabase {
    let db = mem_db();
    db.execute("CREATE TABLE pa (id INT PRIMARY KEY, a INT)")
        .expect("create pa");
    db.execute("INSERT INTO pa VALUES (1, 10), (2, 20)").expect("seed pa");
    db.execute("CREATE TABLE pb (id INT PRIMARY KEY, k INT)")
        .expect("create pb");
    db.execute("INSERT INTO pb VALUES (1, 7), (2, 9)").expect("seed pb");
    db
}

/// M1 (BLOCKER). `a LEFT JOIN b ON a.id = b.id AND b.k = $1` — legal,
/// parameterized SQL, and the family the whole fix exists for. Candidate 6
/// moved the residual out of the `FilterOperator` (built WITH the statement's
/// parameters) and into the join operator's own evaluator, which was built
/// with an EMPTY parameter vector: every such statement hard-failed with
/// `Parameter $1 not provided`.
#[test]
fn a_parameter_in_a_join_residual_evaluates_on_the_params_family() {
    let db = param_pair();
    let one = [Value::Int4(7)];

    // INNER: the residual is a post-join filter (it always had the parameters).
    assert_eq!(
        param_rows(
            &db,
            "SELECT pa.id, pb.id FROM pa JOIN pb ON pa.id = pb.id AND pb.k = $1",
            &one
        ),
        vec![vec![Some(1), Some(1)]]
    );
    // LEFT: the residual is checked INSIDE the hash join, by its evaluator.
    assert_eq!(
        param_rows(
            &db,
            "SELECT pa.id, pb.id FROM pa LEFT JOIN pb ON pa.id = pb.id AND pb.k = $1",
            &one
        ),
        vec![vec![Some(1), Some(1)], vec![Some(2), None]]
    );
    // RIGHT / FULL: the residual is evaluated by the NESTED LOOP's evaluator.
    assert_eq!(
        param_rows(
            &db,
            "SELECT pa.id, pb.id FROM pa RIGHT JOIN pb ON pa.id = pb.id AND pb.k = $1",
            &one
        ),
        vec![vec![None, Some(2)], vec![Some(1), Some(1)]]
    );
    assert_eq!(
        param_rows(
            &db,
            "SELECT pa.id, pb.id FROM pa FULL JOIN pb ON pa.id = pb.id AND pb.k = $1",
            &one
        ),
        vec![vec![None, Some(2)], vec![Some(1), Some(1)], vec![Some(2), None]]
    );
    // A NATURAL / USING join with an EXTRA `$1` term in a WHERE clause the
    // optimizer may push, and in the ON clause of a further join.
    assert_eq!(
        param_rows(&db, "SELECT pa.id, pa.a FROM pa NATURAL JOIN pb WHERE pb.k = $1", &one),
        vec![vec![Some(1), Some(10)]]
    );
    assert_eq!(
        param_rows(
            &db,
            "SELECT pa.id, pa.a FROM pa JOIN pb USING (id) WHERE pb.k = $1",
            &one
        ),
        vec![vec![Some(1), Some(10)]]
    );
    // Two parameters, one of them keyed against a column.
    assert_eq!(
        param_rows(
            &db,
            "SELECT pa.id, pb.id FROM pa LEFT JOIN pb ON pa.id = pb.id AND pb.k = $1 AND pa.a = $2",
            &[Value::Int4(7), Value::Int4(10)]
        ),
        vec![vec![Some(1), Some(1)], vec![Some(2), None]]
    );

    // Text-family control: the same statements with the literal spelled out
    // return the same rows (the text family cannot bind `$1` — `query`
    // ignores its params argument — so the literal IS the control).
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT pa.id, pb.id FROM pa LEFT JOIN pb ON pa.id = pb.id AND pb.k = 7"
        ),
        vec![vec![Some(1), Some(1)], vec![Some(2), None]]
    );
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT pa.id, pb.id FROM pa FULL JOIN pb ON pa.id = pb.id AND pb.k = 7"
        ),
        vec![vec![None, Some(2)], vec![Some(1), Some(1)], vec![Some(2), None]]
    );
}

fn chain_db() -> EmbeddedDatabase {
    let db = mem_db();
    // Values chosen so a cartesian product cannot pass for the join: each
    // table has two rows and only id = 1 is shared by all of them.
    db.execute("CREATE TABLE c1 (id INT PRIMARY KEY, a INT)")
        .expect("create c1");
    db.execute("INSERT INTO c1 VALUES (1, 10), (2, 20)").expect("seed c1");
    db.execute("CREATE TABLE c2 (id INT PRIMARY KEY, b INT)")
        .expect("create c2");
    db.execute("INSERT INTO c2 VALUES (1, 100), (3, 300)").expect("seed c2");
    db.execute("CREATE TABLE c3 (id INT PRIMARY KEY, c INT)")
        .expect("create c3");
    db.execute("INSERT INTO c3 VALUES (1, 1000), (2, 2000)")
        .expect("seed c3");
    db.execute("CREATE TABLE c4 (id INT PRIMARY KEY, d INT)")
        .expect("create c4");
    db.execute("INSERT INTO c4 VALUES (1, 10000), (2, 20000)")
        .expect("seed c4");
    db
}

/// M2 (BLOCKER). A CHAINED `NATURAL JOIN` / `JOIN … USING` is an equi join on
/// BOTH families.
///
/// Candidate 6 qualified a side only when exactly one column of that name was
/// on it — so from the SECOND join onwards (the left input is itself a join
/// and carries the shared name twice) it emitted an ASYMMETRIC
/// `Column{None,id} = Column{tbl,id}`. `JoinPredicatePushdownRule` reads
/// qualified refs only, called that one-sided, and pushed the WHOLE term into
/// the other input: the join was left with no ON condition at all, so on the
/// TEXT family a 3-table NATURAL join was still a cartesian product while the
/// params family (no optimizer) got the right rows — the two families
/// disagreeing in the opposite direction from the bug we started with.
/// `nullable_int_rows` asserts both families agree on every statement here.
#[test]
fn chained_natural_and_using_joins_are_equi_joins_on_both_families() {
    let db = chain_db();

    // --- three tables, INNER: only id = 1 is in all three ---
    for sql in [
        "SELECT c1.id, c1.a, c2.b, c3.c FROM c1 NATURAL JOIN c2 NATURAL JOIN c3",
        "SELECT c1.id, c1.a, c2.b, c3.c FROM c1 JOIN c2 USING (id) JOIN c3 USING (id)",
    ] {
        assert_eq!(
            nullable_int_rows(&db, sql),
            vec![vec![Some(1), Some(10), Some(100), Some(1000)]],
            "`{sql}` must be an equi join, not a cartesian product"
        );
    }

    // --- three tables, LEFT: the NULL-extended row survives and still keys ---
    for sql in [
        "SELECT c1.id, c1.a, c2.b, c3.c FROM c1 NATURAL LEFT JOIN c2 NATURAL LEFT JOIN c3",
        "SELECT c1.id, c1.a, c2.b, c3.c FROM c1 LEFT JOIN c2 USING (id) LEFT JOIN c3 USING (id)",
    ] {
        assert_eq!(
            nullable_int_rows(&db, sql),
            vec![
                vec![Some(1), Some(10), Some(100), Some(1000)],
                vec![Some(2), Some(20), None, Some(2000)],
            ],
            "`{sql}`"
        );
    }

    // --- four tables ---
    for sql in [
        "SELECT c1.id, c1.a, c2.b, c3.c, c4.d FROM c1 NATURAL JOIN c2 NATURAL JOIN c3 NATURAL JOIN c4",
        "SELECT c1.id, c1.a, c2.b, c3.c, c4.d FROM c1 JOIN c2 USING (id) JOIN c3 USING (id) JOIN c4 USING (id)",
    ] {
        assert_eq!(
            nullable_int_rows(&db, sql),
            vec![vec![Some(1), Some(10), Some(100), Some(1000), Some(10000)]],
            "`{sql}`"
        );
    }
    for sql in [
        "SELECT c1.id, c2.b, c3.c, c4.d FROM c1 NATURAL LEFT JOIN c2 NATURAL LEFT JOIN c3 NATURAL LEFT JOIN c4",
        "SELECT c1.id, c2.b, c3.c, c4.d FROM c1 LEFT JOIN c2 USING (id) LEFT JOIN c3 USING (id) LEFT JOIN c4 USING (id)",
    ] {
        assert_eq!(
            nullable_int_rows(&db, sql),
            vec![
                vec![Some(1), Some(100), Some(1000), Some(10000)],
                vec![Some(2), None, Some(2000), Some(20000)],
            ],
            "`{sql}`"
        );
    }

    // `SELECT *` is what a client writes. The join column is still emitted
    // once per side (PostgreSQL merges it — pre-existing and orthogonal), so
    // pin the ROW COUNT, which is what the cartesian product got wrong: 8 for
    // three tables, 16 for four.
    assert_rows(&db, "SELECT * FROM c1 NATURAL JOIN c2 NATURAL JOIN c3", 1);
    assert_rows(&db, "SELECT * FROM c1 JOIN c2 USING (id) JOIN c3 USING (id)", 1);
    assert_rows(
        &db,
        "SELECT * FROM c1 NATURAL JOIN c2 NATURAL JOIN c3 NATURAL JOIN c4",
        1,
    );
    assert_rows(&db, "SELECT * FROM c1 NATURAL LEFT JOIN c2 NATURAL LEFT JOIN c3", 2);

    // Mixed spellings, and a plain ON after a NATURAL join.
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT c1.id, c2.b, c3.c FROM c1 NATURAL JOIN c2 JOIN c3 ON c3.id = c1.id"
        ),
        vec![vec![Some(1), Some(100), Some(1000)]]
    );
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT c1.id, c2.b, c3.c FROM c1 JOIN c2 USING (id) NATURAL JOIN c3"
        ),
        vec![vec![Some(1), Some(100), Some(1000)]]
    );
}

/// M2b. A pushdown rule must MOVE a predicate or LEAVE it alone — never drop
/// it. `ON id = b.id` (a bare reference next to a qualified one) is the shape
/// that exposed it, independently of NATURAL JOIN: the conjunct looked
/// right-only, was pushed into `b`, and the join ran with no condition.
#[test]
fn a_half_qualified_on_term_is_never_pushed_out_of_the_join() {
    let db = mem_db();
    // `k` is carried by he1 only and `m` by he2 only, so each bare name is
    // legal and one-sided — which is exactly what the rule may NOT conclude
    // from the qualifiers alone, because it cannot see a bare name at all.
    db.execute("CREATE TABLE he1 (id INT PRIMARY KEY, k INT)")
        .expect("create he1");
    db.execute("INSERT INTO he1 VALUES (1, 5), (2, 6)").expect("seed he1");
    db.execute("CREATE TABLE he2 (id INT PRIMARY KEY, m INT)")
        .expect("create he2");
    db.execute("INSERT INTO he2 VALUES (10, 5), (11, 7)").expect("seed he2");

    assert_eq!(
        nullable_int_rows(&db, "SELECT he1.id, he2.id FROM he1 JOIN he2 ON k = he2.m"),
        vec![vec![Some(1), Some(10)]],
        "a bare left-side name against a qualified right-side one"
    );
    assert_eq!(
        nullable_int_rows(&db, "SELECT he1.id, he2.id FROM he1 JOIN he2 ON he1.k = m"),
        vec![vec![Some(1), Some(10)]],
        "…and the mirror image"
    );
    // Under a LEFT join, where the rule may not push a right-only term at
    // all, the same term must still key the join rather than vanish.
    assert_eq!(
        nullable_int_rows(&db, "SELECT he1.id, he2.id FROM he1 LEFT JOIN he2 ON k = he2.m"),
        vec![vec![Some(1), Some(10)], vec![Some(2), None]]
    );
}

/// M5. The 0A000 refusal is keyed on CORRELATION, not on "a subquery inside a
/// join's ON failed". Candidate 6 keyed it on the flag, so a missing table, a
/// division by zero and a type error inside an UNCORRELATED subquery were all
/// reported as `correlated subquery in JOIN ... ON is not supported`, with the
/// real diagnostic discarded into a debug trace.
#[test]
fn only_a_correlated_subquery_in_on_is_refused_as_unsupported() {
    let db = mem_db();
    db.execute("CREATE TABLE ma (id INT PRIMARY KEY, k INT, x INT)")
        .expect("create ma");
    db.execute("INSERT INTO ma VALUES (1, 1, 10), (2, 2, 20)")
        .expect("seed ma");
    db.execute("CREATE TABLE mb (id INT PRIMARY KEY)").expect("create mb");
    db.execute("INSERT INTO mb VALUES (1), (2)").expect("seed mb");
    db.execute("CREATE TABLE mc (k INT, x INT)").expect("create mc");
    db.execute("INSERT INTO mc VALUES (1, 10), (2, 99)").expect("seed mc");

    // (a) genuinely correlated -> 0A000 (candidate 6's pin, kept).
    assert_refused(
        &db,
        "SELECT ma.id, mb.id FROM ma LEFT JOIN mb ON ma.id = mb.id \
         AND ma.x = (SELECT max(x) FROM mc WHERE mc.k = ma.k)",
        "correlated subquery in JOIN ... ON is not supported",
    );

    // (b) UNCORRELATED over a table that does not exist -> the table's own
    // diagnostic, never the feature refusal.
    for (params_family, family) in FAMILIES {
        let sql = "SELECT ma.id, mb.id FROM ma LEFT JOIN mb ON ma.id = mb.id \
                   AND ma.x = (SELECT max(x) FROM nosuchtable)";
        match run(&db, sql, params_family) {
            Ok(rows) => panic!("[{family}] must be refused, got {} rows", rows.len()),
            Err(msg) => {
                assert!(
                    !msg.contains("correlated subquery in JOIN"),
                    "[{family}] a missing table is not a correlation: {msg}"
                );
                assert!(
                    msg.to_lowercase().contains("nosuchtable"),
                    "[{family}] the refusal must name the table: {msg}"
                );
            }
        }
    }

    // (c) UNCORRELATED, arithmetic error -> the arithmetic error, with its
    // own message. This is the shape that distinguishes the two designs: the
    // subquery PLANS, then fails while being executed for materialization.
    for (params_family, family) in FAMILIES {
        let sql = "SELECT ma.id, mb.id FROM ma LEFT JOIN mb ON ma.id = mb.id \
                   AND ma.x = (SELECT max(x) / 0 FROM mc)";
        match run(&db, sql, params_family) {
            Ok(rows) => panic!("[{family}] a division by zero must be refused, got {} rows", rows.len()),
            Err(msg) => {
                assert!(
                    !msg.contains("correlated subquery in JOIN"),
                    "[{family}] a division by zero is not a correlation: {msg}"
                );
                assert!(
                    msg.to_lowercase().contains("division by zero"),
                    "[{family}] the subquery's own diagnostic must survive: {msg}"
                );
            }
        }
    }

    // (d) the drizzle NULL-fallback shape — a correlated scalar subquery
    // OUTSIDE a join's ON — is untouched: this candidate narrows the refusal
    // to the join path AND to a genuine correlation, so whatever this shape
    // did before candidate 6 it still does, and in particular it is never
    // reported as the JOIN ... ON feature refusal.
    match db.query_params(
        "SELECT ma.id, (SELECT max(x) FROM mc WHERE mc.k = ma.k) FROM ma ORDER BY ma.id",
        &[],
    ) {
        Ok(rows) => assert_eq!(rows.len(), 2, "the NULL stand-in still lets the statement answer"),
        Err(e) => assert!(
            !e.to_string().contains("correlated subquery in JOIN"),
            "the JOIN ... ON refusal must never escape onto another path: {e}"
        ),
    }

    // …and the UNCORRELATED spelling inside an ON still returns the rows.
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT ma.id, mb.id FROM ma LEFT JOIN mb ON ma.id = mb.id AND ma.x = (SELECT min(x) FROM mc)"
        ),
        vec![vec![Some(1), Some(1)], vec![Some(2), None]]
    );
}

/// n6. `CREATE MATERIALIZED VIEW` over a `NATURAL JOIN` / `JOIN … USING`
/// where one side is a VIEW. Candidate 6's lowering qualified that side with
/// its stamped alias, and the de-stamp pass refused a stamped reference whose
/// bare name the other side also carries — which a shared join column always
/// is: `CREATE` was 0A000. Refusing SQL that worked is a regression, and
/// candidate 10 removed the cause rather than the symptom — the generated
/// term is bare on both sides, so it carries no stamp for `sql::mv_destamp`
/// to refuse or rewrite (`mv_destamp.rs` is byte-identical to its pre-c6
/// state). This pins that the statement is ACCEPTED and that the stored plan
/// REFRESHes after a reopen, which is the c3 mv_destamp contract.
#[test]
fn a_materialized_view_over_a_natural_join_with_a_view_survives_reopen_and_refresh() {
    let (_dir, path) = scratch_store();
    {
        let db = open_store(&path);
        db.execute("CREATE TABLE nt (id INT PRIMARY KEY, a INT)")
            .expect("create nt");
        db.execute("INSERT INTO nt VALUES (1, 10), (2, 20)").expect("seed nt");
        db.execute("CREATE TABLE nu (id INT PRIMARY KEY, b INT)")
            .expect("create nu");
        db.execute("INSERT INTO nu VALUES (1, 100), (3, 300)").expect("seed nu");
        db.execute("CREATE VIEW nv AS SELECT id, b FROM nu").expect("view");

        db.execute(
            "CREATE MATERIALIZED VIEW m_nat AS SELECT nt.id AS id, nt.a AS a, nv.b AS b FROM nt NATURAL JOIN nv",
        )
        .expect("an MV over a NATURAL join with a view must be accepted");
        db.execute("CREATE MATERIALIZED VIEW m_using AS SELECT nt.id AS id, nv.b AS b FROM nt JOIN nv USING (id)")
            .expect("…and over JOIN … USING");
        assert_eq!(ids(&db, "SELECT id FROM m_nat"), vec![1]);
        assert_eq!(ids(&db, "SELECT id FROM m_using"), vec![1]);
        db.close().expect("close");
    }
    let db = open_store(&path);
    db.execute("INSERT INTO nu VALUES (2, 200)")
        .expect("insert after reopen");
    for mv in ["m_nat", "m_using"] {
        db.execute(&format!("REFRESH MATERIALIZED VIEW {mv}"))
            .unwrap_or_else(|e| panic!("REFRESH {mv} after reopen must re-execute the stored plan: {e}"));
    }
    assert_eq!(
        ids(&db, "SELECT id FROM m_nat"),
        vec![1, 2],
        "the stored plan still keys on the shared column"
    );
    assert_eq!(ids(&db, "SELECT id FROM m_using"), vec![1, 2]);
}

// ---- GH#29 (candidate 8) ----
//
// (m2) A `$n` really inside the ON clause of a join whose left input is a
// NATURAL / USING lowering is evaluated. (m4) A self-contained subquery over
// a SCHEMA-QUALIFIED relation is not misread as correlated. (m5) A FROM chain
// naming one relation twice is not turned into a tautology.
//
// Candidate 10 removed this block's M1 pin (`a_chained_natural_or_using_join_
// keys_on_the_column_postgresql_merged`) with the qualification design it
// tested: the generated operands are bare again, so a chain keys on the
// LEFTMOST contributor — correct for an INNER/LEFT-topped left input, and for
// a RIGHT/FULL-topped one the pre-existing gap that belongs to the un-merged
// output arity, not to this issue.

/// m2. The pin candidate 7's brief asked for and candidate 7 did not deliver:
/// a `$n` inside the ON clause of a join whose LEFT INPUT is a `NATURAL` /
/// `USING` lowering. A parameter in a WHERE clause goes to a `FilterOperator`,
/// which has carried the bind values all along and was never the broken path;
/// only a term the JOIN operator evaluates exercises the evaluator M1 fixed.
#[test]
fn a_parameter_in_the_on_clause_above_a_natural_join_is_evaluated() {
    let db = chain_db();
    let hit = [Value::Int4(1000)];
    let miss = [Value::Int4(2000)];

    // `c1 NATURAL JOIN c2` is one row (id = 1). The third join's ON carries
    // the parameter, so the parameter decides rows INSIDE that join.
    for chain in ["c1 NATURAL JOIN c2", "c1 JOIN c2 USING (id)"] {
        // INNER: the residual is a post-join filter, and must still see `$1`.
        let sql = format!("SELECT c1.id, c3.c FROM {chain} JOIN c3 ON c3.id = c1.id AND c3.c = $1");
        assert_eq!(param_rows(&db, &sql, &hit), vec![vec![Some(1), Some(1000)]], "`{sql}`");
        assert_eq!(param_rows(&db, &sql, &miss), Vec::<Vec<Option<i64>>>::new(), "`{sql}`");

        // LEFT: the residual is checked by the HASH JOIN, per candidate pair.
        let sql = format!("SELECT c1.id, c3.c FROM {chain} LEFT JOIN c3 ON c3.id = c1.id AND c3.c = $1");
        assert_eq!(param_rows(&db, &sql, &hit), vec![vec![Some(1), Some(1000)]], "`{sql}`");
        assert_eq!(param_rows(&db, &sql, &miss), vec![vec![Some(1), None]], "`{sql}`");

        // RIGHT / FULL: the whole condition is the NESTED LOOP's.
        let sql = format!("SELECT c1.id, c3.c FROM {chain} RIGHT JOIN c3 ON c3.id = c1.id AND c3.c = $1");
        assert_eq!(
            param_rows(&db, &sql, &hit),
            vec![vec![None, Some(2000)], vec![Some(1), Some(1000)]],
            "`{sql}`"
        );
        assert_eq!(
            param_rows(&db, &sql, &miss),
            vec![vec![None, Some(1000)], vec![None, Some(2000)]],
            "`{sql}`"
        );

        let sql = format!("SELECT c1.id, c3.c FROM {chain} FULL JOIN c3 ON c3.id = c1.id AND c3.c = $1");
        assert_eq!(
            param_rows(&db, &sql, &hit),
            vec![vec![None, Some(2000)], vec![Some(1), Some(1000)]],
            "`{sql}`"
        );
        assert_eq!(
            param_rows(&db, &sql, &miss),
            vec![vec![None, Some(1000)], vec![None, Some(2000)], vec![Some(1), None]],
            "`{sql}`"
        );

        // Two parameters, both inside the join's ON.
        let sql = format!("SELECT c1.id, c3.c FROM {chain} LEFT JOIN c3 ON c3.id = c1.id AND c3.c = $1 AND c1.a = $2");
        assert_eq!(
            param_rows(&db, &sql, &[Value::Int4(1000), Value::Int4(10)]),
            vec![vec![Some(1), Some(1000)]],
            "`{sql}`"
        );
        assert_eq!(
            param_rows(&db, &sql, &[Value::Int4(1000), Value::Int4(99)]),
            vec![vec![Some(1), None]],
            "`{sql}`"
        );
    }

    // Text-family control: the same shape with the literal spelled out (the
    // text family ignores its params argument, so the literal IS the control).
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT c1.id, c3.c FROM c1 NATURAL JOIN c2 LEFT JOIN c3 ON c3.id = c1.id AND c3.c = 2000"
        ),
        vec![vec![Some(1), None]]
    );
}

/// m4. `collect_plan_relations` inserted a Scan's `table_name` VERBATIM, but a
/// Scan's table_name is the RESOLVED key — `schema.table` outside `public` —
/// while a column qualifier in the same subquery is the BARE relation name. A
/// self-contained subquery over such a scan was therefore read as CORRELATED,
/// and the 0A000 replaced the real diagnostic: the very defect M5 removed.
#[test]
fn an_uncorrelated_subquery_over_a_schema_qualified_relation_is_not_called_correlated() {
    let db = mem_db();
    db.execute("CREATE SCHEMA an").expect("create schema");
    db.execute("CREATE TABLE qa (id INT PRIMARY KEY, x INT)")
        .expect("create qa");
    db.execute("INSERT INTO qa VALUES (1, 10), (2, 20)").expect("seed qa");
    db.execute("CREATE TABLE qb (id INT PRIMARY KEY)").expect("create qb");
    db.execute("INSERT INTO qb VALUES (1), (2)").expect("seed qb");
    db.execute("CREATE TABLE an.mc (k INT, x INT)").expect("create an.mc");
    db.execute("INSERT INTO an.mc VALUES (1, 10), (2, 99)")
        .expect("seed an.mc");

    // UNCORRELATED over `an.mc`, qualified by the BARE relation name: the
    // rows, not a refusal.
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT qa.id, qb.id FROM qa LEFT JOIN qb ON qa.id = qb.id \
             AND qa.x = (SELECT min(x) FROM an.mc WHERE mc.k = 1)"
        ),
        vec![vec![Some(1), Some(1)], vec![Some(2), None]]
    );

    // …and one that FAILS keeps its own diagnostic, not the 0A000.
    for (params_family, family) in FAMILIES {
        let sql = "SELECT qa.id, qb.id FROM qa LEFT JOIN qb ON qa.id = qb.id \
                   AND qa.x = (SELECT max(x) / 0 FROM an.mc WHERE mc.k = 1)";
        match run(&db, sql, params_family) {
            Ok(rows) => panic!("[{family}] a division by zero must be refused, got {} rows", rows.len()),
            Err(msg) => {
                assert!(
                    !msg.contains("correlated subquery in JOIN"),
                    "[{family}] a self-contained subquery over a schema-qualified table is not a correlation: {msg}"
                );
                assert!(
                    msg.to_lowercase().contains("division by zero"),
                    "[{family}] the subquery's own diagnostic must survive: {msg}"
                );
            }
        }
    }

    // A genuinely correlated subquery over the SAME relation is still 0A000.
    assert_refused(
        &db,
        "SELECT qa.id, qb.id FROM qa LEFT JOIN qb ON qa.id = qb.id \
         AND qa.x = (SELECT max(x) FROM an.mc WHERE mc.k = qa.id)",
        "correlated subquery in JOIN ... ON is not supported",
    );
}

/// m5. A FROM chain that names ONE relation twice — which PostgreSQL refuses
/// outright with `table name "c1" specified more than once` — must not be
/// turned into a tautology. We stay lenient and run it, but the generated term
/// may not collapse to one slot.
///
/// Candidate 10 dropped this pin's other half (a derived table carrying the
/// shared name twice was 42702): with BARE operands there is no qualifier to
/// pick, so there is nothing to be ambiguous about at lowering time, and an
/// unqualified reference resolves to the first matching column — the
/// engine-wide rule, not this issue's.
#[test]
fn a_from_chain_naming_one_relation_twice_is_not_a_tautology() {
    let db = chain_db();

    // Both operands are bare, so the key binder assigns them in the natural
    // order (lhs -> left input, rhs -> right input) instead of declining the
    // term and letting the combined evaluator resolve both to ONE slot — the
    // tautology, i.e. the cartesian product this whole issue is about.
    // `c1 NATURAL JOIN c2` is one row (id = 1) and c1 has two, so a tautology
    // is 2 rows and the join is 1.
    assert_rows(&db, "SELECT * FROM c1 NATURAL JOIN c2 NATURAL JOIN c1", 1);
    assert_rows(&db, "SELECT * FROM c1 JOIN c2 USING (id) JOIN c1 USING (id)", 1);

    // …and a derived table that really does carry the shared name twice now
    // RUNS rather than being refused: the generated term is bare, so there is
    // no qualifier to be ambiguous about at lowering time. `c1 ⋈ c3` on `id`
    // is {1, 2} and `c2` is {1, 3}, so `d ⋈ c2` is {1} — one row, `c2.b` =
    // 100. (The statement names no duplicated column itself; `d.id` written
    // out over this WRITTEN-OUT select list is still 42702 — see
    // `an_explicitly_written_duplicate_select_list_is_still_ambiguous`.)
    for sql in [
        "SELECT c2.b FROM (SELECT c1.id, c3.id FROM c1 JOIN c3 ON c1.id = c3.id) d NATURAL JOIN c2",
        "SELECT c2.b FROM (SELECT c1.id, c3.id FROM c1 JOIN c3 ON c1.id = c3.id) d JOIN c2 USING (id)",
    ] {
        assert_eq!(nullable_int_rows(&db, sql), vec![vec![Some(100)]], "`{sql}`");
    }
}

// ---- GH#29 (candidate 9) ----
//
// (m7) The compound-ON / NATURAL / USING / `$n` corpus also runs through the
// OPTIMIZER-RUNNING params pipeline (`query_params_with_columns`), which is a
// THIRD read pipeline: `query_params` and the PostgreSQL extended protocol run
// NO optimizer passes at all.
//
// Candidate 10 removed this block's M1, M2 and m3 pins with the qualification
// design they tested; the shapes M1 covered are re-pinned, without the 42702
// and without the merged-contributor values, in the candidate-10 block below.

/// `y1 ⋈ y2` on `id` is `{2, 4}`; `y3` carries `{1, 2, 3}`. So a join of the
/// merged side against `y3` is ONE row for INNER, two for LEFT, three for
/// RIGHT and four for FULL — four different answers, none of which a cartesian
/// product (6 rows for INNER) can imitate.
fn merged_side_db() -> EmbeddedDatabase {
    let db = mem_db();
    db.execute("CREATE TABLE y1 (id INT PRIMARY KEY, a INT)")
        .expect("create y1");
    db.execute("INSERT INTO y1 VALUES (1, 11), (2, 22), (4, 44)")
        .expect("seed y1");
    db.execute("CREATE TABLE y2 (id INT PRIMARY KEY, b INT)")
        .expect("create y2");
    db.execute("INSERT INTO y2 VALUES (2, 222), (3, 333), (4, 444)")
        .expect("seed y2");
    db.execute("CREATE TABLE y3 (id INT PRIMARY KEY, c INT)")
        .expect("create y3");
    db.execute("INSERT INTO y3 VALUES (1, 1111), (2, 2222), (3, 3333)")
        .expect("seed y3");
    db.execute("CREATE VIEW yn AS SELECT * FROM y1 NATURAL JOIN y2")
        .expect("create view yn");
    db.execute("CREATE VIEW yu AS SELECT * FROM y1 JOIN y2 USING (id)")
        .expect("create view yu");
    db
}

/// Rows of `sql` through `query_params_with_columns` — the THIRD read
/// pipeline, and the only PARAMETERIZED one that runs the optimizer
/// (ConstantFolding / Selection / JoinPredicate / ProjectionPruning /
/// StorageFilter). `query_params` and `query_params_for_session` — the
/// embedded params API AND the PostgreSQL extended protocol — run none of
/// them, so a pin on either of those says nothing about this one (m7).
fn optimized_param_rows(db: &EmbeddedDatabase, sql: &str, params: &[Value]) -> Vec<Vec<Option<i64>>> {
    let (rows, _columns) = db
        .query_params_with_columns(sql, params)
        .unwrap_or_else(|e| panic!("[params+optimizer] `{sql}` must plan and run: {e}"));
    let mut got: Vec<Vec<Option<i64>>> = rows.iter().map(|r| r.values.iter().map(opt_i64).collect()).collect();
    got.sort();
    got
}

/// m7. Candidate 8 believed the PostgreSQL extended protocol was the
/// optimizer-running params path; it is the optimizer-FREE one
/// (`query_params_for_session` → `parameterized_plan_cached`). So the whole
/// `$n`-inside-an-ON corpus was pinned twice on the same pipeline. This drives
/// it — and the chained-merge corpus — through `query_params_with_columns`,
/// where JoinPredicatePushdown, SelectionPushdown, ProjectionPruning and
/// StorageFilterPushdown all run over the generated join condition.
#[test]
fn the_parameterized_on_corpus_also_runs_through_the_optimizer_running_pipeline() {
    let db = chain_db();
    let hit = [Value::Int4(1000)];
    let miss = [Value::Int4(2000)];

    for chain in ["c1 NATURAL JOIN c2", "c1 JOIN c2 USING (id)"] {
        // INNER: the right-only conjunct is MOVED into a Filter above c3 by
        // JoinPredicatePushdownRule and removed from the ON — exactly once.
        let sql = format!("SELECT c1.id, c3.c FROM {chain} JOIN c3 ON c3.id = c1.id AND c3.c = $1");
        assert_eq!(
            optimized_param_rows(&db, &sql, &hit),
            vec![vec![Some(1), Some(1000)]],
            "`{sql}`"
        );
        assert_eq!(
            optimized_param_rows(&db, &sql, &miss),
            Vec::<Vec<Option<i64>>>::new(),
            "`{sql}`"
        );

        // LEFT: the conjunct is kept on the join and checked per candidate
        // pair, with the bind values the optimizer must not have dropped.
        let sql = format!("SELECT c1.id, c3.c FROM {chain} LEFT JOIN c3 ON c3.id = c1.id AND c3.c = $1");
        assert_eq!(
            optimized_param_rows(&db, &sql, &hit),
            vec![vec![Some(1), Some(1000)]],
            "`{sql}`"
        );
        assert_eq!(
            optimized_param_rows(&db, &sql, &miss),
            vec![vec![Some(1), None]],
            "`{sql}`"
        );

        // RIGHT / FULL: the whole condition is the nested loop's.
        let sql = format!("SELECT c1.id, c3.c FROM {chain} FULL JOIN c3 ON c3.id = c1.id AND c3.c = $1");
        assert_eq!(
            optimized_param_rows(&db, &sql, &hit),
            vec![vec![None, Some(2000)], vec![Some(1), Some(1000)]],
            "`{sql}`"
        );
        assert_eq!(
            optimized_param_rows(&db, &sql, &miss),
            vec![vec![None, Some(1000)], vec![None, Some(2000)], vec![Some(1), None]],
            "`{sql}`"
        );
    }

    // …and a NATURAL/USING join whose SIDE is a derived table or a view over
    // another one, through the optimizer-running parameterized pipeline.
    let sides = merged_side_db();
    assert_eq!(
        optimized_param_rows(
            &sides,
            "SELECT j.a, j.b, y3.id, y3.c FROM (SELECT * FROM y1 NATURAL JOIN y2) j NATURAL JOIN y3 WHERE y3.c > $1",
            &[Value::Int4(0)]
        ),
        vec![vec![Some(22), Some(222), Some(2), Some(2222)]]
    );
    assert_eq!(
        optimized_param_rows(
            &sides,
            "SELECT j.a, j.b, y3.id, y3.c FROM yn j LEFT JOIN y3 USING (id)",
            &[]
        ),
        vec![
            vec![Some(22), Some(222), Some(2), Some(2222)],
            vec![Some(44), Some(444), None, None],
        ]
    );
}

// ---- GH#29 (candidate 10) ----
//
// The qualification half of candidate 6 is REMOVED: the planner emits BOTH
// operands of a `NATURAL` / `USING` term UNQUALIFIED again, as origin/main
// emitted for `NATURAL`, and every function and every 42702 built on the
// qualified shape is gone. What makes the term an equi join is candidate 6's
// OTHER half — the key binder assigns an all-unqualified `=` term in the
// natural order (lhs -> left input, rhs -> right input) — which is kept.
//
// (P3) A `NATURAL` / `USING` join whose SIDE is a CTE, view or derived table
// over another one RETURNS ROWS, including the `SELECT *` spelling. It is the
// one shape the removed design refused (42702) because this engine does not
// merge the shared output column, so such a side carries the name twice where
// PostgreSQL carries it once. Candidate 11 (M1) took the WRITTEN `j.id`, and
// candidate 12 (M1) took `j.*` as well: v4.31.1 answers both, so refusing
// either was a regression against the shipped release. Nothing over such a
// side is refused any more — the candidate-12 block holds the pins.
//
// KNOWN DEVIATION, deliberate and stated (sprinter 781f55ba534d): a chained
// `NATURAL`/`USING` join whose LEFT input tops out in a RIGHT or FULL join
// keys on the LEFTMOST contributor where PostgreSQL keys on the right one /
// `COALESCE(left, right)`. That is what origin/main does today and what this
// engine's un-merged output arity forces, so it is NOT pinned here either way.

/// P3. Every side spelling — derived table, view, CTE, each with a `NATURAL`
/// and a `USING` body — joined every way, on BOTH executor families.
///
/// `y1 ⋈ y2` is `{2, 4}` and `y3` is `{1, 2, 3}`, so the four join types give
/// four different answers (1, 2, 3 and 4 rows) and a cartesian product gives
/// six: no two of them can be mistaken for one another. The side's own top
/// join is INNER, so its leftmost contributor IS the value PostgreSQL merges —
/// the deviation above is not in play here.
#[test]
fn a_natural_or_using_join_over_a_cte_view_or_derived_table_returns_rows() {
    let db = merged_side_db();

    let sides: [(&str, &str, &str); 6] = [
        ("derived/NATURAL", "", "(SELECT * FROM y1 NATURAL JOIN y2) j"),
        ("derived/USING", "", "(SELECT * FROM y1 JOIN y2 USING (id)) j"),
        ("view/NATURAL", "", "yn j"),
        ("view/USING", "", "yu j"),
        ("cte/NATURAL", "WITH j AS (SELECT * FROM y1 NATURAL JOIN y2) ", "j"),
        ("cte/USING", "WITH j AS (SELECT * FROM y1 JOIN y2 USING (id)) ", "j"),
    ];

    // (j.a, j.b, y3.id, y3.c), sorted.
    let inner: Vec<Vec<Option<i64>>> = vec![vec![Some(22), Some(222), Some(2), Some(2222)]];
    let left: Vec<Vec<Option<i64>>> = vec![
        vec![Some(22), Some(222), Some(2), Some(2222)],
        vec![Some(44), Some(444), None, None],
    ];
    let right: Vec<Vec<Option<i64>>> = vec![
        vec![None, None, Some(1), Some(1111)],
        vec![None, None, Some(3), Some(3333)],
        vec![Some(22), Some(222), Some(2), Some(2222)],
    ];
    let full: Vec<Vec<Option<i64>>> = vec![
        vec![None, None, Some(1), Some(1111)],
        vec![None, None, Some(3), Some(3333)],
        vec![Some(22), Some(222), Some(2), Some(2222)],
        vec![Some(44), Some(444), None, None],
    ];

    for (label, prefix, side) in sides {
        for (kind, expected) in [("", &inner), ("LEFT ", &left), ("RIGHT ", &right), ("FULL ", &full)] {
            for join in [format!("NATURAL {kind}JOIN y3"), format!("{kind}JOIN y3 USING (id)")] {
                let sql = format!("{prefix}SELECT j.a, j.b, y3.id, y3.c FROM {side} {join}");
                assert_eq!(&nullable_int_rows(&db, &sql), expected, "[{label}] `{sql}`");
            }
        }
        // …and the `SELECT *` spelling, which is what a client writes. ROW
        // COUNT only, so the un-merged output arity is never frozen as a
        // contract. This is the statement the removed design refused.
        for join in ["NATURAL JOIN y3", "JOIN y3 USING (id)"] {
            assert_rows(&db, &format!("{prefix}SELECT * FROM {side} {join}"), 1);
        }
    }
}

/// What a reference to a wildcard-expanded side that carries the shared name
/// twice does: ALL of it resolves — the written `j.id`, the bare `id`, and
/// (candidate 12, M1) `j.*` as well.
///
/// Candidate 10 refused the written `id` / `j.id`; candidate 11 kept refusing
/// `j.*`. Both were REGRESSIONS against v4.31.1, which answers all three —
/// `SELECT j.*` there returns the un-merged pair `[2, 22, 2, 222]`, the same
/// arity a bare `SELECT *` gives today. Refusing the qualified spelling while
/// allowing the bare one was inconsistent as well as a regression.
///
/// STILL REFUSED, and pinned here as the control: `s.*` over a select list
/// the AUTHOR wrote the name into twice.
#[test]
fn a_qualified_wildcard_over_a_wildcard_expanded_output_name_resolves() {
    let db = merged_side_db();
    let dup = "(SELECT * FROM y1 NATURAL JOIN y2) j";

    // `j.*` expands to the entry's own columns, duplicates included — exactly
    // as the bare `*` does. Value-asserted AND arity-asserted: four columns,
    // `(id, a, id, b)`, with the doubled name read from the first slot (the
    // sub-select itself already wrote that slot into both, and a NATURAL join
    // equates them). PostgreSQL returns THREE columns here, because it merges
    // the shared output column and we do not — sprinter 781f55ba534d, stated
    // in the CHANGELOG as a known deviation, and NOT closed by this pin.
    assert_eq!(
        nullable_int_rows(&db, &format!("SELECT j.* FROM {dup}")),
        vec![
            vec![Some(2), Some(22), Some(2), Some(222)],
            vec![Some(4), Some(44), Some(4), Some(444)],
        ]
    );
    assert_eq!(
        column_names(&db, &format!("SELECT j.* FROM {dup}")),
        vec!["id".to_string(), "a".to_string(), "id".to_string(), "b".to_string()],
        "`j.*` emits the entry's columns, duplicates included, like the bare `*`"
    );
    // …and the bare `*` over the same side has the SAME arity. That equality
    // is the reason the qualified spelling may not be refused.
    assert_eq!(
        column_names(&db, &format!("SELECT * FROM {dup}")).len(),
        column_names(&db, &format!("SELECT j.* FROM {dup}")).len()
    );

    // The written reference candidate 10 refused: v4.31.1 returns these rows.
    assert_eq!(ids(&db, &format!("SELECT j.id FROM {dup}")), vec![2, 4]);
    assert_eq!(ids(&db, &format!("SELECT id FROM {dup}")), vec![2, 4]);
    assert_eq!(ids(&db, &format!("SELECT j.a FROM {dup} WHERE id = 2")), vec![22]);
    // A name the side carries ONCE still resolves, bare and qualified.
    assert_eq!(ids(&db, &format!("SELECT a FROM {dup} ORDER BY a")), vec![22, 44]);
    assert_eq!(ids(&db, &format!("SELECT j.b FROM {dup} ORDER BY j.b")), vec![222, 444]);

    // CONTROL, the other way round: an AUTHOR-WRITTEN duplicate keeps its
    // 42702 for `s.*` — it would emit `s.id` twice and read ONE slot for both,
    // and there the two columns really are two different ones.
    let written = "(SELECT y1.id, y2.id, y1.a, y2.b FROM y1 JOIN y2 ON y1.id = y2.id) s";
    assert_refused(&db, &format!("SELECT s.* FROM {written}"), AMBIGUOUS);
}

// ---- GH#29 (candidate 11) ----
//
// (M1, REGRESSION against the SHIPPED release) A QUALIFIED reference to a
// `NATURAL`/`USING` side that carries the shared name twice —
// `WITH j AS (SELECT * FROM y1 NATURAL JOIN y2) SELECT j.id … FROM j` — was
// refused `42702`. v4.31.1 RETURNS the row, and so does PostgreSQL, whose `j`
// carries `id` ONCE because it merges the shared output column. This engine
// does not merge it (sprinter 781f55ba534d), so candidate 1/2's "a qualified
// reference to a name a sub-select's output carries twice is 42702" — correct
// for a select list that really names two different columns — also fired on a
// duplicate WE manufactured. It now turns on the select LIST: an explicitly
// written list keeps its `42702`, a wildcard-expanded one resolves to the
// FIRST of the two slots, which is what the sub-select itself returns.
//
// (M2, contract change, kept) A reference QUALIFIED by a relation the FROM
// chain names twice is `42712`, where v4.31.1 answered with the first one.
// PostgreSQL refuses it with the same message.
//
// (m4) A chained join whose left input tops out in RIGHT/FULL is pinned as
// NOT a cartesian product — at the LEFTMOST-contributor answer, which is the
// stated deviation and NOT PostgreSQL's; and an MV over a CHAINED `NATURAL`
// join survives store -> close -> reopen -> REFRESH.

/// M1. `j.id` / `v.id` / `d.id` over a CTE, a VIEW and a derived table whose
/// body is a `NATURAL` join and a `USING` join — with and without a further
/// join — RETURNS THE ROW, value-asserted on BOTH executor families.
///
/// `y1 ⋈ y2` is `{2, 4}` with `(a, b)` = `(22, 222)` and `(44, 444)`, so the
/// first slot of the doubled `id` carries 2 and 4; `y3` is `{1, 2, 3}`, so the
/// further join keeps only `id = 2`. No value here can be reached by reading
/// the wrong slot: every row of `y1 ⋈ y2` has the two `id` slots equal, and a
/// cartesian product would be 2 or 6 rows, never 1.
#[test]
fn a_qualified_reference_to_a_wildcard_expanded_join_side_returns_the_row() {
    let db = merged_side_db();

    // (label, WITH prefix, FROM side, qualifier to write)
    let sides: [(&str, &str, &str, &str); 8] = [
        ("derived/NATURAL", "", "(SELECT * FROM y1 NATURAL JOIN y2) j", "j"),
        ("derived/USING", "", "(SELECT * FROM y1 JOIN y2 USING (id)) j", "j"),
        ("view/NATURAL aliased", "", "yn v", "v"),
        ("view/USING aliased", "", "yu v", "v"),
        // A view referenced by its own NAME, with no alias: the qualifier is
        // the view name, which is the spelling a client writes.
        ("view/NATURAL bare", "", "yn", "yn"),
        ("view/USING bare", "", "yu", "yu"),
        ("cte/NATURAL", "WITH j AS (SELECT * FROM y1 NATURAL JOIN y2) ", "j", "j"),
        (
            "cte/USING",
            "WITH j AS (SELECT * FROM y1 JOIN y2 USING (id)) ",
            "j",
            "j",
        ),
    ];

    for (label, prefix, side, q) in sides {
        // (a) No second join at all — the plainest spelling, and the one the
        // brief calls out: `SELECT j.id FROM j`.
        let sql = format!("{prefix}SELECT {q}.id, {q}.a, {q}.b FROM {side}");
        assert_eq!(
            nullable_int_rows(&db, &sql),
            vec![vec![Some(2), Some(22), Some(222)], vec![Some(4), Some(44), Some(444)],],
            "[{label}] `{sql}`"
        );

        // (b) …and the BARE spelling of the same doubled name, which is the
        // same rule (`ScopeStack::resolve`'s unqualified arm).
        let sql = format!("{prefix}SELECT id, a, b FROM {side}");
        assert_eq!(
            nullable_int_rows(&db, &sql),
            vec![vec![Some(2), Some(22), Some(222)], vec![Some(4), Some(44), Some(444)],],
            "[{label}] `{sql}`"
        );

        // (c) With a further join — INNER and LEFT, `NATURAL` and `USING` —
        // which is the evidence statement's shape.
        for (kind, expected) in [
            ("", vec![vec![Some(2), Some(22), Some(222), Some(2222)]]),
            (
                "LEFT ",
                vec![
                    vec![Some(2), Some(22), Some(222), Some(2222)],
                    vec![Some(4), Some(44), Some(444), None],
                ],
            ),
        ] {
            for join in [format!("NATURAL {kind}JOIN y3"), format!("{kind}JOIN y3 USING (id)")] {
                let sql = format!("{prefix}SELECT {q}.id, {q}.a, {q}.b, y3.c FROM {side} {join}");
                assert_eq!(nullable_int_rows(&db, &sql), expected, "[{label}] `{sql}`");
            }
        }
    }
}

/// M1's controls, the other way round: a select list that WRITES OUT the same
/// name twice really does carry two different columns, and PostgreSQL refuses
/// a reference to it. Both name spellings, derived table and CTE.
#[test]
fn an_explicitly_written_duplicate_select_list_is_still_ambiguous() {
    let db = merged_side_db();
    let explicit = "SELECT y1.id, y2.id, y1.a, y2.b FROM y1 JOIN y2 ON y1.id = y2.id";
    let derived = format!("({explicit}) s");
    let cte_prefix = format!("WITH s AS ({explicit}) ");

    for (label, prefix, side) in [("derived", "", derived.as_str()), ("cte", cte_prefix.as_str(), "s")] {
        for sql in [
            format!("{prefix}SELECT s.id FROM {side}"),
            format!("{prefix}SELECT id FROM {side}"),
            format!("{prefix}SELECT s.a FROM {side} WHERE id = 2"),
        ] {
            assert_refused(&db, &sql, AMBIGUOUS);
        }
        // A name the list carries ONCE still resolves, bare and qualified.
        assert_eq!(
            nullable_int_rows(&db, &format!("{prefix}SELECT s.a, b FROM {side}")),
            vec![vec![Some(22), Some(222)], vec![Some(44), Some(444)]],
            "[{label}] a unique name must still resolve"
        );
    }
}

/// M2 (contract change, deliberately kept). A reference QUALIFIED by a
/// relation the FROM chain names TWICE is `42712 table name "c1" specified
/// more than once`; v4.31.1 resolved it against the first `c1`. PostgreSQL
/// raises exactly that, so this is a move towards parity.
#[test]
fn a_reference_qualified_by_a_relation_named_twice_is_refused() {
    let db = chain_db();

    for sql in [
        "SELECT c1.id FROM c1 NATURAL JOIN c2 NATURAL JOIN c1",
        "SELECT c1.a FROM c1 JOIN c2 USING (id) JOIN c1 USING (id)",
        "SELECT c2.b FROM c1 NATURAL JOIN c2 NATURAL JOIN c1 WHERE c1.id = 1",
    ] {
        assert_refused(&db, sql, DUPLICATE_ALIAS);
    }

    // LENIENT, and stated: PostgreSQL refuses the FROM clause itself, whether
    // or not anything names the repeated relation. We refuse only the
    // reference, so a statement that names neither `c1` still plans — and it
    // must still be the join, never the tautology candidate 5 produced.
    assert_rows(&db, "SELECT * FROM c1 NATURAL JOIN c2 NATURAL JOIN c1", 1);
    assert_eq!(
        nullable_int_rows(&db, "SELECT c2.b FROM c1 NATURAL JOIN c2 NATURAL JOIN c1"),
        vec![vec![Some(100)]]
    );
}

/// m4(a). A chained `NATURAL` / `USING` join whose LEFT input tops out in a
/// `RIGHT` or `FULL` join. This pin exists to prove the chain is NOT a
/// cartesian product; it is NOT a correctness pin.
///
/// KNOWN DEVIATION, stated and accepted (sprinter 781f55ba534d): we key on the
/// LEFTMOST column of the shared name on the left input, i.e. `r1.id`, which
/// the outer join NULL-extends. PostgreSQL merges the `RIGHT` join's shared
/// column to the RIGHT contributor (`FULL`: `COALESCE(left, right)`), so where
/// we return ONE row PostgreSQL returns TWO — it also matches the row whose
/// `id` only `r2` carries. Do not read the expected values below as
/// PostgreSQL's answer; read them as "the engine keys on ONE column, and the
/// join is an equi join".
#[test]
fn a_chain_over_a_right_or_full_topped_input_is_not_a_cartesian_product() {
    let db = mem_db();
    db.execute("CREATE TABLE r1 (id INT PRIMARY KEY, p INT)")
        .expect("create r1");
    db.execute("INSERT INTO r1 VALUES (2, 20), (5, 50)").expect("seed r1");
    db.execute("CREATE TABLE r2 (id INT PRIMARY KEY, q INT)")
        .expect("create r2");
    db.execute("INSERT INTO r2 VALUES (2, 200), (3, 300)").expect("seed r2");
    db.execute("CREATE TABLE r3 (id INT PRIMARY KEY, s INT)")
        .expect("create r3");
    db.execute("INSERT INTO r3 VALUES (2, 2000), (3, 3000)")
        .expect("seed r3");

    // `r1 RIGHT JOIN r2` is 2 rows, `r1 FULL JOIN r2` is 3, and `r3` is 2, so
    // a cartesian product would be 4 or 6 rows. Both chains are ONE row.
    for (label, left) in [
        ("RIGHT-topped", "r1 NATURAL RIGHT JOIN r2"),
        ("RIGHT-topped/USING", "r1 RIGHT JOIN r2 USING (id)"),
        ("FULL-topped", "r1 NATURAL FULL JOIN r2"),
        ("FULL-topped/USING", "r1 FULL JOIN r2 USING (id)"),
    ] {
        for join in ["NATURAL JOIN r3", "JOIN r3 USING (id)"] {
            let sql = format!("SELECT r1.p, r2.q, r3.s FROM {left} {join}");
            assert_eq!(
                nullable_int_rows(&db, &sql),
                vec![vec![Some(20), Some(200), Some(2000)]],
                "[{label}] `{sql}` — the leftmost-contributor answer, not PostgreSQL's"
            );
            // …and the `SELECT *` spelling by ROW COUNT only, so the un-merged
            // output arity is never frozen as a contract.
            assert_rows(&db, &format!("SELECT * FROM {left} {join}"), 1);
        }
    }
}

/// m4(b). A MATERIALIZED VIEW over a CHAINED `NATURAL` / `USING` join
/// survives store -> close -> reopen -> `REFRESH`. This is the candidate-3
/// `sql::mv_destamp` contract: the stored plan is positional bincode and
/// carries no alias stamp, so a generated join term that carried one could not
/// be re-executed after a reopen. With both operands bare it carries none —
/// this pin is what says so, and it is the shape candidate 9 broke.
#[test]
fn a_materialized_view_over_a_chained_natural_join_survives_reopen_and_refresh() {
    let (_dir, path) = scratch_store();
    {
        let db = open_store(&path);
        db.execute("CREATE TABLE w1 (id INT PRIMARY KEY, a INT)")
            .expect("create w1");
        db.execute("INSERT INTO w1 VALUES (1, 10), (2, 20)").expect("seed w1");
        db.execute("CREATE TABLE w2 (id INT PRIMARY KEY, b INT)")
            .expect("create w2");
        db.execute("INSERT INTO w2 VALUES (1, 100), (2, 200)").expect("seed w2");
        db.execute("CREATE TABLE w3 (id INT PRIMARY KEY, c INT)")
            .expect("create w3");
        db.execute("INSERT INTO w3 VALUES (1, 1000)").expect("seed w3");
        db.execute("CREATE VIEW w3v AS SELECT id, c FROM w3")
            .expect("create w3v");

        db.execute(
            "CREATE MATERIALIZED VIEW m_chain AS \
             SELECT w1.id AS id, w1.a AS a, w3.c AS c FROM w1 NATURAL JOIN w2 NATURAL JOIN w3",
        )
        .expect("an MV over a CHAINED NATURAL join must be accepted");
        db.execute(
            "CREATE MATERIALIZED VIEW m_chain_u AS \
             SELECT w1.id AS id, w3v.c AS c FROM w1 JOIN w2 USING (id) JOIN w3v USING (id)",
        )
        .expect("…and over a chained USING whose last side is a VIEW");
        assert_eq!(ids(&db, "SELECT id FROM m_chain"), vec![1]);
        assert_eq!(ids(&db, "SELECT id FROM m_chain_u"), vec![1]);
        db.close().expect("close");
    }
    let db = open_store(&path);
    db.execute("INSERT INTO w3 VALUES (2, 2000)")
        .expect("insert after reopen");
    for mv in ["m_chain", "m_chain_u"] {
        db.execute(&format!("REFRESH MATERIALIZED VIEW {mv}"))
            .unwrap_or_else(|e| panic!("REFRESH {mv} after reopen must re-execute the stored plan: {e}"));
    }
    assert_eq!(
        ids(&db, "SELECT id FROM m_chain"),
        vec![1, 2],
        "the stored chain still keys on the shared column after a reopen"
    );
    assert_eq!(ids(&db, "SELECT id FROM m_chain_u"), vec![1, 2]);
}

// ---- GH#29 (candidate 12) ----
//
// (M1, REGRESSION against the SHIPPED release) `j.*` over a CTE, view or
// derived table whose body is a wildcard over a `NATURAL` / `USING` join was
// still `42702` after candidate 11 — the ONE call site left on the raw schema
// question (`Planner::expand_qualified_wildcard`). v4.31.1 ANSWERS it, with
// the un-merged pair `[2, 22, 2, 222]`, which is the same arity the bare
// `SELECT *` gives today and which candidate 11 already allowed. `q.*` now
// expands to that entry's columns, duplicates included, exactly as `*` does.
//
// (M2, the same regression, one spelling candidate 11 could not reach) A
// MATERIALIZED VIEW is planned as a base-table Scan of its STORED relation, so
// it has no select list in the statement being planned at all — and every
// reference to a name its stored schema repeats was `42702`. The duplicate
// lives in the catalog: the author could not have written it twice at the
// reference site, so it can never be the "two different columns" case. It is
// the default now: only an AUTHOR-WRITTEN select list arms the refusal.
//
// (M3, the flag was too broad the OTHER way) Candidate 11 disarmed the
// refusal whenever the select list CONTAINED a wildcard, so a list MIXING a
// wildcard with written items — `SELECT *, y2.id` — silently answered a
// genuine PostgreSQL ambiguity that v4.31.1 ERRORED on. Only a list that is
// ENTIRELY wildcards disarms it.
//
// (m4e) A body topped by a `NATURAL RIGHT` / `FULL` join: the first slot of
// the doubled name is the LEFT contributor, which the outer join NULL-extends,
// so we answer NULL where PostgreSQL — which MERGES the shared column rather
// than calling it ambiguous — answers with the right contributor's value.

/// M1. `q.*` over every side spelling — derived table, view (aliased and
/// bare), CTE, each with a `NATURAL` and a `USING` body — returns the row,
/// VALUE-asserted and ARITY-asserted, on both executor families.
///
/// `y1 ⋈ y2` is `{2, 4}`, so the entry is `(id, a, id, b)` = `(2, 22, 2, 222)`
/// and `(4, 44, 4, 444)`: four columns, the doubled name read from the first
/// slot (the sub-select's own `*` already wrote that slot into both, and a
/// NATURAL join equates them). PostgreSQL returns THREE columns, because it
/// merges the shared output column and we do not — sprinter 781f55ba534d,
/// stated as a known deviation and NOT closed here.
#[test]
fn a_qualified_wildcard_over_a_wildcard_expanded_side_returns_its_columns() {
    let db = merged_side_db();

    // (label, WITH prefix, FROM side, qualifier to write)
    let sides: [(&str, &str, &str, &str); 8] = [
        ("derived/NATURAL", "", "(SELECT * FROM y1 NATURAL JOIN y2) j", "j"),
        ("derived/USING", "", "(SELECT * FROM y1 JOIN y2 USING (id)) j", "j"),
        ("view/NATURAL aliased", "", "yn v", "v"),
        ("view/USING aliased", "", "yu v", "v"),
        ("view/NATURAL bare", "", "yn", "yn"),
        ("view/USING bare", "", "yu", "yu"),
        ("cte/NATURAL", "WITH j AS (SELECT * FROM y1 NATURAL JOIN y2) ", "j", "j"),
        (
            "cte/USING",
            "WITH j AS (SELECT * FROM y1 JOIN y2 USING (id)) ",
            "j",
            "j",
        ),
    ];

    for (label, prefix, side, q) in sides {
        let sql = format!("{prefix}SELECT {q}.* FROM {side}");
        assert_eq!(
            nullable_int_rows(&db, &sql),
            vec![
                vec![Some(2), Some(22), Some(2), Some(222)],
                vec![Some(4), Some(44), Some(4), Some(444)],
            ],
            "[{label}] `{sql}`"
        );
        assert_eq!(
            column_names(&db, &sql),
            vec!["id".to_string(), "a".to_string(), "id".to_string(), "b".to_string()],
            "[{label}] `{sql}` emits the entry's columns, duplicates included"
        );
        // …the same arity the bare `*` gives. That equality is the reason the
        // qualified spelling may not be refused while the bare one resolves.
        let bare = format!("{prefix}SELECT * FROM {side}");
        assert_eq!(
            column_names(&db, &bare).len(),
            column_names(&db, &sql).len(),
            "[{label}] `{sql}` vs `{bare}`"
        );
    }

    // CONTROL: `s.*` over an AUTHOR-WRITTEN duplicate select list keeps its
    // 42702 — there the two columns really are two different ones.
    let written = "(SELECT y1.id, y2.id, y1.a, y2.b FROM y1 JOIN y2 ON y1.id = y2.id) s";
    assert_refused(&db, &format!("SELECT s.* FROM {written}"), AMBIGUOUS);
}

/// M2. A MATERIALIZED VIEW whose body is a wildcard over a `NATURAL` /
/// `USING` join: `SELECT id FROM m`, `SELECT m.id FROM m`, `SELECT * FROM m`
/// and `SELECT m.* FROM m` all return the row, as v4.31.1 answers them.
///
/// The duplicate lives in the MV's STORED SCHEMA. Nothing at the reference
/// site wrote it, so it can never be the author-named ambiguity — and the
/// entry falls out of the same predicate as every other stored relation,
/// because it has no select list in the statement being planned.
///
/// The store round trip is pinned too: CREATE, REFRESH, close, reopen,
/// REFRESH again. The stored plan is positional bincode, so a reopen
/// re-executes it from bytes (the candidate-3 `sql::mv_destamp` contract).
#[test]
fn a_materialized_view_over_a_wildcard_natural_join_resolves_its_doubled_name() {
    let (_dir, path) = scratch_store();
    {
        let db = open_store(&path);
        db.execute("CREATE TABLE v1 (id INT PRIMARY KEY, a INT)")
            .expect("create v1");
        db.execute("INSERT INTO v1 VALUES (1, 11), (2, 22), (4, 44)")
            .expect("seed v1");
        db.execute("CREATE TABLE v2 (id INT PRIMARY KEY, b INT)")
            .expect("create v2");
        db.execute("INSERT INTO v2 VALUES (2, 222), (3, 333), (4, 444)")
            .expect("seed v2");
        db.execute("CREATE MATERIALIZED VIEW mn AS SELECT * FROM v1 NATURAL JOIN v2")
            .expect("an MV over a wildcard NATURAL body must be accepted");
        db.execute("CREATE MATERIALIZED VIEW mu AS SELECT * FROM v1 JOIN v2 USING (id)")
            .expect("…and over a wildcard USING body");

        for mv in ["mn", "mu"] {
            // The four spellings the regression covered.
            assert_eq!(ids(&db, &format!("SELECT id FROM {mv}")), vec![2, 4], "`{mv}`");
            assert_eq!(ids(&db, &format!("SELECT {mv}.id FROM {mv}")), vec![2, 4], "`{mv}`");
            assert_eq!(
                nullable_int_rows(&db, &format!("SELECT * FROM {mv}")),
                vec![
                    vec![Some(2), Some(22), Some(2), Some(222)],
                    vec![Some(4), Some(44), Some(4), Some(444)],
                ],
                "`{mv}`"
            );
            assert_eq!(
                nullable_int_rows(&db, &format!("SELECT {mv}.* FROM {mv}")),
                nullable_int_rows(&db, &format!("SELECT * FROM {mv}")),
                "`{mv}.*` and `*` over a stored relation are the same columns"
            );
            // A name the stored schema carries ONCE is unaffected.
            assert_eq!(
                ids(&db, &format!("SELECT {mv}.b FROM {mv} ORDER BY {mv}.b")),
                vec![222, 444]
            );
            db.execute(&format!("REFRESH MATERIALIZED VIEW {mv}"))
                .unwrap_or_else(|e| panic!("REFRESH {mv} must re-execute the stored plan: {e}"));
            assert_eq!(ids(&db, &format!("SELECT {mv}.id FROM {mv}")), vec![2, 4], "`{mv}`");
        }
        db.close().expect("close");
    }

    // …and after a reopen, which re-executes the stored plan from bincode.
    let db = open_store(&path);
    db.execute("INSERT INTO v2 VALUES (1, 111)")
        .expect("insert after reopen");
    for mv in ["mn", "mu"] {
        db.execute(&format!("REFRESH MATERIALIZED VIEW {mv}"))
            .unwrap_or_else(|e| panic!("REFRESH {mv} after reopen must re-execute the stored plan: {e}"));
        assert_eq!(
            ids(&db, &format!("SELECT id FROM {mv}")),
            vec![1, 2, 4],
            "`{mv}` still keys on the shared column after a reopen"
        );
        assert_eq!(ids(&db, &format!("SELECT {mv}.id FROM {mv}")), vec![1, 2, 4], "`{mv}`");
    }
}

/// M3. A select list that MIXES a wildcard with written items is
/// AUTHOR-WRITTEN and keeps its `42702` — for `s.id`, for the bare `id` and
/// for `s.*`. Candidate 11 turned all three into silent answers, which is a
/// change from "error" to "wrong row", the one direction that is never
/// acceptable: v4.31.1 ERRORS here.
///
/// The controls: a list that is ENTIRELY wildcards still resolves, whether it
/// is one bare `*` or two qualified ones.
#[test]
fn a_select_list_mixing_a_wildcard_with_written_items_stays_ambiguous() {
    let db = merged_side_db();
    let on = "FROM y1 JOIN y2 ON y1.id = y2.id";

    for (label, body) in [
        ("wildcard first", format!("SELECT *, y2.id {on}")),
        ("wildcard last", format!("SELECT y1.id, y2.id, * {on}")),
        ("qualified wildcard + written", format!("SELECT y1.*, y2.b, y2.id {on}")),
    ] {
        let derived = format!("({body}) s");
        let cte_prefix = format!("WITH s AS ({body}) ");
        for (shape, prefix, side) in [("derived", "", derived.as_str()), ("cte", cte_prefix.as_str(), "s")] {
            for sql in [
                format!("{prefix}SELECT s.id FROM {side}"),
                format!("{prefix}SELECT id FROM {side}"),
                format!("{prefix}SELECT s.* FROM {side}"),
            ] {
                assert_refused(&db, &sql, AMBIGUOUS);
            }
            // A name the list carries ONCE still resolves.
            assert_eq!(
                nullable_int_rows(&db, &format!("{prefix}SELECT s.a, s.b FROM {side}")),
                vec![vec![Some(22), Some(222)], vec![Some(44), Some(444)]],
                "[{label}/{shape}] a unique name must still resolve"
            );
        }
    }

    // CONTROL 1: one bare `*` — the engine shaped the whole list.
    assert_eq!(
        nullable_int_rows(&db, "SELECT s.id, s.a, s.b FROM (SELECT * FROM y1 NATURAL JOIN y2) s"),
        vec![vec![Some(2), Some(22), Some(222)], vec![Some(4), Some(44), Some(444)],]
    );
    // CONTROL 2: TWO qualified wildcards and nothing written — still the
    // engine's own shape, so `s.id` still resolves to the first slot.
    assert_eq!(
        nullable_int_rows(&db, &format!("SELECT s.id, s.a, s.b FROM (SELECT y1.*, y2.* {on}) s"),),
        vec![vec![Some(2), Some(22), Some(222)], vec![Some(4), Some(44), Some(444)],]
    );
}

/// m4(e). The doubled name resolves to the FIRST slot, which is the LEFT
/// contributor — and a `NATURAL RIGHT` / `FULL` join NULL-extends it.
///
/// KNOWN DEVIATION, stated: PostgreSQL does not call this name ambiguous
/// either, but it MERGES the shared output column (`RIGHT`: the right
/// contributor; `FULL`: `COALESCE(left, right)`), so for the row only `y2`
/// carries it answers `3` where we answer NULL. Read the values below as
/// "the first slot, whatever the join did to it", not as PostgreSQL's answer.
/// Closing it is the merged output arity (sprinter 781f55ba534d), filed
/// separately.
#[test]
fn a_right_or_full_topped_wildcard_body_answers_null_where_postgresql_merges() {
    let db = merged_side_db();

    // y1 = {1, 2, 4}, y2 = {2, 3, 4}. RIGHT keeps y2's three rows; the id-3
    // row has no left contributor, so the first `id` slot is NULL.
    assert_eq!(
        nullable_int_rows(&db, "SELECT j.id, j.b FROM (SELECT * FROM y1 NATURAL RIGHT JOIN y2) j"),
        vec![
            vec![None, Some(333)],
            vec![Some(2), Some(222)],
            vec![Some(4), Some(444)],
        ],
        "PostgreSQL answers 3 for the middle row; we answer NULL"
    );
    // FULL keeps both unmatched sides: id 1 (left only) and id 3 (right only).
    assert_eq!(
        nullable_int_rows(&db, "SELECT j.id, j.b FROM (SELECT * FROM y1 NATURAL FULL JOIN y2) j"),
        vec![
            vec![None, Some(333)],
            vec![Some(1), None],
            vec![Some(2), Some(222)],
            vec![Some(4), Some(444)],
        ],
        "PostgreSQL answers COALESCE(left, right) = 3 for the first row; we answer NULL"
    );
    // The USING spelling is the same lowering, so it deviates identically.
    assert_eq!(
        nullable_int_rows(
            &db,
            "SELECT j.id, j.b FROM (SELECT * FROM y1 RIGHT JOIN y2 USING (id)) j",
        ),
        vec![
            vec![None, Some(333)],
            vec![Some(2), Some(222)],
            vec![Some(4), Some(444)],
        ]
    );
}
