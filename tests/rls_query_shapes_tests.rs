//! Row-level-security READ policies apply to every relation a statement reads,
//! whatever the query shape (plan brief v05.6).
//!
//! Before this fix the plan rewrite that turns a SELECT policy into a scan
//! predicate only descended through Scan / Filter / Project / Aggregate / Join /
//! Sort / Limit / FilteredScan and returned every other node untouched, and it
//! never looked inside the plans embedded in subquery expressions. So, with a
//! tenant context and a policy that shows 2 of `docs`' 4 rows, a set operation
//! (UNION / INTERSECT / EXCEPT), a CTE, an IN / EXISTS / scalar subquery, a
//! derived table over a UNION, an `INSERT … SELECT` with a UNION source, a CTAS
//! over a CTE, and `UPDATE plain SET v = (SELECT body FROM docs …)` all read the
//! hidden rows.
//!
//! Each shape is checked through the text family (`query` / `execute`), the
//! parameter family (`query_params` / `execute_params`, with a `$1` inside the
//! protected read), an open embedded transaction, and a no-context control that
//! must still see every row. Row CONTENTS are compared, not only counts.
//!
//! Every database is `new_in_memory()`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, deprecated)]

use heliosdb_nano::tenant::{IsolationMode, RLSCommand, TenantContext, TenantId};
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// `docs`: 4 rows, alice owns 1 and 3 (bodies a1, a3); bob owns 2, carol 4.
/// `plain`: no policy, ids 1..4 with values p1..p4. `sink`: empty, no policy.
fn seed(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT NOT NULL, body TEXT)")
        .unwrap();
    db.execute("INSERT INTO docs VALUES (1, 'alice', 'a1'), (2, 'bob', 'b2'), (3, 'alice', 'a3'), (4, 'carol', 'c4')")
        .unwrap();
    db.execute("CREATE TABLE plain (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO plain VALUES (1, 'p1'), (2, 'p2'), (3, 'p3'), (4, 'p4')")
        .unwrap();
    db.execute("CREATE TABLE sink (id INT, body TEXT)").unwrap();
}

fn set_ctx(db: &EmbeddedDatabase, tenant_id: TenantId) {
    db.tenant_manager.set_current_context(TenantContext {
        tenant_id,
        user_id: "alice".to_string(),
        roles: vec!["member".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });
}

fn create_policy(db: &EmbeddedDatabase, using: &str) {
    db.tenant_manager.create_rls_policy(
        "docs".to_string(),
        "p_owner".to_string(),
        "alice reads her own docs".to_string(),
        RLSCommand::Select,
        using.to_string(),
        None,
    );
}

/// A seeded database with the policy created and the tenant context active.
fn protected_db(using: &str) -> (EmbeddedDatabase, TenantId) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    let tenant = db
        .tenant_manager
        .register_tenant("acme".to_string(), IsolationMode::SharedSchema);
    create_policy(&db, using);
    set_ctx(&db, tenant.id);
    (db, tenant.id)
}

/// A seeded database with the same policy but NO tenant context.
fn control_db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    db.tenant_manager
        .register_tenant("acme".to_string(), IsolationMode::SharedSchema);
    create_policy(&db, "owner = 'alice'");
    db
}

fn render(rows: &[Tuple]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|t| {
            t.values
                .iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    out.sort();
    out
}

fn strs(xs: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = xs.iter().map(|s| (*s).to_string()).collect();
    v.sort();
    v
}

/// `{p}` in a template becomes the literal `0` (text family) or `$1` bound to
/// `0` (parameter family). It always sits inside the protected read, so the
/// parameter family exercises parameters inside the rewritten plans too.
fn text_sql(template: &str) -> String {
    template.replace("{p}", "0")
}

fn param_sql(template: &str) -> String {
    template.replace("{p}", "$1")
}

const PARAMS: [Value; 1] = [Value::Int4(0)];

/// Run a read shape through every surface and compare row contents.
fn check_read(shape: &str, template: &str, filtered: &[&str], unfiltered: &[&str]) {
    let (db, _) = protected_db("owner = 'alice'");
    let want = strs(filtered);

    let got = render(
        &db.query(&text_sql(template), &[])
            .unwrap_or_else(|e| panic!("{shape}: query: {e}")),
    );
    assert_eq!(got, want, "{shape}: query() under the policy");

    let got = render(
        &db.query_params(&param_sql(template), &PARAMS)
            .unwrap_or_else(|e| panic!("{shape}: query_params: {e}")),
    );
    assert_eq!(got, want, "{shape}: query_params() under the policy");

    let (rows, _) = db
        .query_with_columns(&text_sql(template))
        .unwrap_or_else(|e| panic!("{shape}: query_with_columns: {e}"));
    assert_eq!(render(&rows), want, "{shape}: query_with_columns() under the policy");

    let (rows, _) = db
        .query_params_with_columns(&param_sql(template), &PARAMS)
        .unwrap_or_else(|e| panic!("{shape}: query_params_with_columns: {e}"));
    assert_eq!(
        render(&rows),
        want,
        "{shape}: query_params_with_columns() under the policy"
    );

    let n = db
        .execute(&text_sql(template))
        .unwrap_or_else(|e| panic!("{shape}: execute: {e}"));
    assert_eq!(n as usize, want.len(), "{shape}: execute() row count under the policy");
    let n = db
        .execute_params(&param_sql(template), &PARAMS)
        .unwrap_or_else(|e| panic!("{shape}: execute_params: {e}"));
    assert_eq!(
        n as usize,
        want.len(),
        "{shape}: execute_params() row count under the policy"
    );

    {
        let tx = db.begin_transaction().unwrap();
        let rows = tx
            .query(&text_sql(template), &[])
            .unwrap_or_else(|e| panic!("{shape}: Transaction::query: {e}"));
        assert_eq!(render(&rows), want, "{shape}: Transaction::query under the policy");
        tx.rollback().unwrap();
    }

    db.execute("BEGIN").unwrap();
    let got = render(&db.query(&text_sql(template), &[]).unwrap());
    let got_p = render(&db.query_params(&param_sql(template), &PARAMS).unwrap());
    db.execute("COMMIT").unwrap();
    assert_eq!(got, want, "{shape}: query() inside BEGIN");
    assert_eq!(got_p, want, "{shape}: query_params() inside BEGIN");

    // No-context control: the same policy exists, nobody is bound to a tenant,
    // so every row is visible.
    let ctl = control_db();
    let all = strs(unfiltered);
    assert_eq!(
        render(&ctl.query(&text_sql(template), &[]).unwrap()),
        all,
        "{shape}: control query()"
    );
    assert_eq!(
        render(&ctl.query_params(&param_sql(template), &PARAMS).unwrap()),
        all,
        "{shape}: control query_params()"
    );
}

// ---------------------------------------------------------------------------
// Set operations
// ---------------------------------------------------------------------------

#[test]
fn union_all() {
    check_read(
        "union_all",
        "SELECT body FROM docs WHERE id > {p} UNION ALL SELECT body FROM docs",
        &["a1", "a1", "a3", "a3"],
        &["a1", "a1", "b2", "b2", "a3", "a3", "c4", "c4"],
    );
}

#[test]
fn union_protected_on_the_right() {
    check_read(
        "union",
        "SELECT v FROM plain WHERE id > {p} UNION SELECT body FROM docs",
        &["p1", "p2", "p3", "p4", "a1", "a3"],
        &["p1", "p2", "p3", "p4", "a1", "b2", "a3", "c4"],
    );
}

#[test]
fn intersect_protected_on_either_side() {
    check_read(
        "intersect_right",
        "SELECT id FROM plain WHERE id > {p} INTERSECT SELECT id FROM docs",
        &["1", "3"],
        &["1", "2", "3", "4"],
    );
    check_read(
        "intersect_left",
        "SELECT id FROM docs WHERE id > {p} INTERSECT SELECT id FROM plain",
        &["1", "3"],
        &["1", "2", "3", "4"],
    );
}

#[test]
fn except_protected_on_either_side() {
    check_read(
        "except_right",
        "SELECT id FROM plain WHERE id > {p} EXCEPT SELECT id FROM docs",
        &["2", "4"],
        &[],
    );
    check_read(
        "except_left",
        "SELECT id FROM docs WHERE id > {p} EXCEPT SELECT id FROM plain WHERE id = 1",
        &["3"],
        &["2", "3", "4"],
    );
}

#[test]
fn derived_table_over_union() {
    check_read(
        "derived_table_over_union",
        "SELECT count(*) FROM (SELECT id FROM docs WHERE id > {p} UNION ALL SELECT id FROM docs) AS u",
        &["4"],
        &["8"],
    );
}

// ---------------------------------------------------------------------------
// CTEs
// ---------------------------------------------------------------------------

#[test]
fn with_cte() {
    check_read(
        "with",
        "WITH d AS (SELECT id, body FROM docs WHERE id > {p}) SELECT body FROM d",
        &["a1", "a3"],
        &["a1", "b2", "a3", "c4"],
    );
}

#[test]
fn nested_with() {
    check_read(
        "nested_with",
        "WITH d AS (SELECT id, body FROM docs WHERE id > {p}), e AS (SELECT body FROM d) SELECT body FROM e",
        &["a1", "a3"],
        &["a1", "b2", "a3", "c4"],
    );
}

#[test]
fn with_body_reads_protected_table() {
    check_read(
        "with_body",
        "WITH x AS (SELECT id FROM plain) SELECT docs.body FROM x JOIN docs ON docs.id = x.id WHERE docs.id > {p}",
        &["a1", "a3"],
        &["a1", "b2", "a3", "c4"],
    );
}

// ---------------------------------------------------------------------------
// Subquery expressions
// ---------------------------------------------------------------------------

#[test]
fn in_subquery() {
    check_read(
        "in_subquery",
        "SELECT v FROM plain WHERE id IN (SELECT id FROM docs WHERE id > {p})",
        &["p1", "p3"],
        &["p1", "p2", "p3", "p4"],
    );
}

#[test]
fn not_in_subquery() {
    check_read(
        "not_in_subquery",
        "SELECT v FROM plain WHERE id NOT IN (SELECT id FROM docs WHERE id > {p})",
        &["p2", "p4"],
        &[],
    );
}

#[test]
fn exists() {
    check_read(
        "exists",
        "SELECT v FROM plain WHERE EXISTS (SELECT 1 FROM docs WHERE docs.id = plain.id AND docs.id > {p})",
        &["p1", "p3"],
        &["p1", "p2", "p3", "p4"],
    );
}

#[test]
fn not_exists() {
    check_read(
        "not_exists",
        "SELECT v FROM plain WHERE NOT EXISTS (SELECT 1 FROM docs WHERE docs.id = plain.id AND docs.id > {p})",
        &["p2", "p4"],
        &[],
    );
}

#[test]
fn scalar_in_select_list() {
    check_read(
        "scalar_in_select_list",
        "SELECT (SELECT count(*) FROM docs WHERE id > {p})",
        &["2"],
        &["4"],
    );
}

#[test]
fn correlated_scalar() {
    check_read(
        "correlated_scalar",
        "SELECT plain.id, (SELECT owner FROM docs WHERE docs.id = plain.id AND docs.id > {p}) FROM plain",
        &["1|alice", "2|NULL", "3|alice", "4|NULL"],
        &["1|alice", "2|bob", "3|alice", "4|carol"],
    );
}

#[test]
fn case_when_in_subquery() {
    check_read(
        "case_when_in_subquery",
        "SELECT CASE WHEN id IN (SELECT id FROM docs WHERE id > {p}) THEN 'y' ELSE 'n' END FROM plain",
        &["y", "n", "y", "n"],
        &["y", "y", "y", "y"],
    );
}

#[test]
fn having_subquery() {
    check_read(
        "having_subquery",
        "SELECT count(*) FROM plain GROUP BY v HAVING count(*) * 2 = (SELECT count(*) FROM docs WHERE id > {p})",
        &["1", "1", "1", "1"],
        &[],
    );
}

#[test]
fn join_on_subquery() {
    check_read(
        "join_on_subquery",
        "SELECT a.v FROM plain AS a JOIN plain AS b ON a.id = b.id AND a.id IN (SELECT id FROM docs WHERE id > {p})",
        &["p1", "p3"],
        &["p1", "p2", "p3", "p4"],
    );
}

#[test]
fn subquery_inside_union_branch() {
    check_read(
        "subquery_inside_union_branch",
        "SELECT v FROM plain WHERE id IN (SELECT id FROM docs WHERE id > {p}) UNION ALL SELECT v FROM plain WHERE id = 4",
        &["p1", "p3", "p4"],
        &["p1", "p2", "p3", "p4", "p4"],
    );
}

// ---------------------------------------------------------------------------
// Writes whose source reads the protected table
// ---------------------------------------------------------------------------

fn read_unfiltered(db: &EmbeddedDatabase, tenant: TenantId, sql: &str) -> Vec<String> {
    db.tenant_manager.clear_current_context();
    let rows = db.query(sql, &[]).unwrap();
    set_ctx(db, tenant);
    render(&rows)
}

#[test]
fn insert_select_with_union_source() {
    let template = "INSERT INTO sink SELECT id, body FROM docs WHERE id > {p} UNION ALL SELECT id + 10, body FROM docs";
    let want = strs(&["1|a1", "3|a3", "11|a1", "13|a3"]);

    let (db, tenant) = protected_db("owner = 'alice'");
    let n = db.execute(&text_sql(template)).unwrap();
    assert_eq!(n, 4, "execute(): rows copied");
    assert_eq!(
        read_unfiltered(&db, tenant, "SELECT id, body FROM sink"),
        want,
        "execute()"
    );

    let (db, tenant) = protected_db("owner = 'alice'");
    let n = db.execute_params(&param_sql(template), &PARAMS).unwrap();
    assert_eq!(n, 4, "execute_params(): rows copied");
    assert_eq!(
        read_unfiltered(&db, tenant, "SELECT id, body FROM sink"),
        want,
        "execute_params()"
    );

    let ctl = control_db();
    assert_eq!(ctl.execute(&text_sql(template)).unwrap(), 8, "control copies every row");
}

#[test]
fn insert_select_with_subquery_source() {
    let template = "INSERT INTO sink SELECT id, v FROM plain WHERE id IN (SELECT id FROM docs WHERE id > {p})";
    let want = strs(&["1|p1", "3|p3"]);

    let (db, tenant) = protected_db("owner = 'alice'");
    db.execute(&text_sql(template)).unwrap();
    assert_eq!(
        read_unfiltered(&db, tenant, "SELECT id, body FROM sink"),
        want,
        "execute()"
    );

    let (db, tenant) = protected_db("owner = 'alice'");
    db.execute_params(&param_sql(template), &PARAMS).unwrap();
    assert_eq!(
        read_unfiltered(&db, tenant, "SELECT id, body FROM sink"),
        want,
        "execute_params()"
    );
}

#[test]
fn ctas_with_cte_source() {
    let sql = "CREATE TABLE ct AS WITH d AS (SELECT id, body FROM docs WHERE id > 0) SELECT id, body FROM d";
    let (db, tenant) = protected_db("owner = 'alice'");
    db.execute(sql).unwrap();
    assert_eq!(
        read_unfiltered(&db, tenant, "SELECT id, body FROM ct"),
        strs(&["1|a1", "3|a3"]),
        "CTAS over a CTE copied hidden rows"
    );

    let sql = "CREATE TABLE ct2 AS SELECT id, body FROM docs WHERE id > 0 UNION ALL SELECT id, body FROM docs";
    db.execute(sql).unwrap();
    assert_eq!(
        read_unfiltered(&db, tenant, "SELECT id, body FROM ct2"),
        strs(&["1|a1", "3|a3", "1|a1", "3|a3"]),
        "CTAS over a UNION copied hidden rows"
    );

    let ctl = control_db();
    ctl.execute("CREATE TABLE ct AS WITH d AS (SELECT id, body FROM docs WHERE id > 0) SELECT id, body FROM d")
        .unwrap();
    assert_eq!(ctl.query("SELECT id FROM ct", &[]).unwrap().len(), 4, "control");
}

#[test]
fn update_set_scalar_subquery_does_not_copy_hidden_rows() {
    let template = "UPDATE plain SET v = (SELECT body FROM docs WHERE docs.id = plain.id AND docs.id > {p})";
    let want = strs(&["1|a1", "2|NULL", "3|a3", "4|NULL"]);

    let (db, tenant) = protected_db("owner = 'alice'");
    db.execute(&text_sql(template)).unwrap();
    assert_eq!(
        read_unfiltered(&db, tenant, "SELECT id, v FROM plain"),
        want,
        "execute()"
    );

    let (db, tenant) = protected_db("owner = 'alice'");
    db.execute_params(&param_sql(template), &PARAMS).unwrap();
    assert_eq!(
        read_unfiltered(&db, tenant, "SELECT id, v FROM plain"),
        want,
        "execute_params()"
    );

    let (db, tenant) = protected_db("owner = 'alice'");
    db.execute("BEGIN").unwrap();
    db.execute(&text_sql(template)).unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(
        read_unfiltered(&db, tenant, "SELECT id, v FROM plain"),
        want,
        "inside BEGIN"
    );

    let (db, tenant) = protected_db("owner = 'alice'");
    db.execute("UPDATE plain SET v = (SELECT count(*) FROM docs WHERE id > 0) WHERE id = 1")
        .unwrap();
    assert_eq!(
        read_unfiltered(&db, tenant, "SELECT v FROM plain WHERE id = 1"),
        strs(&["2"]),
        "uncorrelated scalar subquery in SET"
    );

    let ctl = control_db();
    ctl.execute(&text_sql(template)).unwrap();
    assert_eq!(
        render(&ctl.query("SELECT id, v FROM plain", &[]).unwrap()),
        strs(&["1|a1", "2|b2", "3|a3", "4|c4"]),
        "control"
    );
}

/// The "Actual Rows" figure EXPLAIN ANALYZE reports for the executed plan.
fn actual_rows(rows: &[Tuple]) -> Vec<String> {
    rows.iter()
        .flat_map(|t| t.values.iter())
        .filter_map(|v| match v {
            Value::String(s) if s.contains("Actual Rows") => Some(s.trim().to_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn explain_analyze_row_counts() {
    let sql = "EXPLAIN ANALYZE SELECT body FROM docs WHERE id > 0 UNION ALL SELECT body FROM docs";
    let (db, _) = protected_db("owner = 'alice'");
    let filtered = actual_rows(&db.query(sql, &[]).unwrap());
    assert_eq!(
        filtered,
        vec!["Actual Rows    : 4".to_string()],
        "EXPLAIN ANALYZE must run the filtered plan"
    );
    let ctl = control_db();
    let unfiltered = actual_rows(&ctl.query(sql, &[]).unwrap());
    assert_eq!(unfiltered, vec!["Actual Rows    : 8".to_string()], "control");
}

// ---------------------------------------------------------------------------
// Fail closed
// ---------------------------------------------------------------------------

#[test]
fn rls_parse_error_fails_closed_for_set_operations_and_subqueries() {
    let (db, _) = protected_db("owner = = 'alice'");
    for sql in [
        "SELECT body FROM plain JOIN docs ON docs.id = plain.id UNION ALL SELECT v FROM plain",
        "SELECT v FROM plain UNION ALL SELECT body FROM docs",
        "WITH d AS (SELECT body FROM docs) SELECT body FROM d",
        "SELECT v FROM plain WHERE id IN (SELECT id FROM docs)",
        "SELECT (SELECT count(*) FROM docs)",
    ] {
        assert!(
            db.query(sql, &[]).is_err(),
            "query() served rows under an unparseable policy: {sql}"
        );
        assert!(
            db.query_params(sql, &[]).is_err(),
            "query_params() served rows under an unparseable policy: {sql}"
        );
    }
    assert!(
        db.execute("UPDATE plain SET v = (SELECT body FROM docs WHERE docs.id = plain.id)")
            .is_err(),
        "UPDATE … SET (SELECT …) ran under an unparseable policy"
    );
}
