//! Batch B — join semantics: `USING` / `NATURAL` lowering, the outer-join ON
//! residual, and the bare-wildcard expansion over a join whose two sides share
//! a column name.
//!
//! Three sprinter items, all of them "silent wrong rows on legal SQL":
//!
//! | item | shape | defect |
//! |------|-------|--------|
//! | `6b75aa7a9d42` | `a JOIN b USING (id)`, `a NATURAL JOIN b` | no join condition was emitted at all — a CARTESIAN product |
//! | `d746afc01c0b` | `a LEFT JOIN b ON a.id = b.id AND b.x > 5` | the non-equality term was a POST-join filter, which ate the NULL-extended rows |
//! | `a50328143c63` | bare `SELECT *` over a join whose sides share a name | every duplicate name resolved to the FIRST slot, so both `v` columns read the same side |
//!
//! The first two were fixed with GH#29 (`c6` blockers m5 / m6) and are pinned
//! in depth by `tests/gh_issue_29_resolution.rs`; what this file adds is the
//! sprinter items' OWN reproductions, so each item has a regression test that
//! names it and survives a refactor of the GH#29 file. The third is fixed here
//! (`Planner::expand_bare_wildcard`) and its tests DO fail on the tree before
//! that fix — `wildcard_over_join_with_duplicate_column_names_keeps_both_sides`
//! returns the left side's value in both `v` slots.
//!
//! Every embedded case runs on BOTH executor families and asserts they agree:
//!
//! * TEXT — `db.query(sql, &[])`, which runs the full optimizer pipeline;
//! * PARAMS — `db.query_params(sql, &[])` with ZERO actual parameters, which
//!   plans through `parameterized_plan_cached` and runs NO optimizer pass at
//!   all. That divergence is precisely how `d746afc01c0b` hid: the text family
//!   got the right answer from `JoinPredicatePushdownRule` while the params
//!   family lost every row.
//!
//! One wire test drives the real PostgreSQL protocol for the RowDescription
//! contract (both output columns are still named `v` — the qualifier is for
//! resolution, never for display) and for the `USING (nosuch)` SQLSTATE.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Harness — both executor families, one assertion
// ---------------------------------------------------------------------------

/// `(is_params_family, label)`.
const FAMILIES: [(bool, &str); 2] = [(false, "text"), (true, "params")];

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

fn run(db: &EmbeddedDatabase, sql: &str, params_family: bool) -> Result<Vec<Tuple>, String> {
    if params_family {
        db.query_params(sql, &[]).map_err(|e| e.to_string())
    } else {
        db.query(sql, &[]).map_err(|e| e.to_string())
    }
}

/// One cell, rendered so integers, text and NULL can live in the same row.
fn cell(value: &Value) -> String {
    match *value {
        Value::Null => "NULL".to_string(),
        Value::Int2(n) => n.to_string(),
        Value::Int4(n) => n.to_string(),
        Value::Int8(n) => n.to_string(),
        Value::String(ref s) => s.clone(),
        ref other => format!("{other:?}"),
    }
}

/// Every row of `sql`, rendered and SORTED, from BOTH families — which must
/// agree with each other before the value is returned.
fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<String>> {
    let mut agreed: Option<Vec<Vec<String>>> = None;
    for (params_family, family) in FAMILIES {
        let out = run(db, sql, params_family).unwrap_or_else(|e| panic!("[{family}] `{sql}` must plan and run: {e}"));
        let mut got: Vec<Vec<String>> = out.iter().map(|r| r.values.iter().map(cell).collect()).collect();
        got.sort();
        if let Some(ref prev) = agreed {
            assert_eq!(prev, &got, "the two executor families must agree on `{sql}`");
        }
        agreed = Some(got);
    }
    agreed.unwrap_or_default()
}

/// `sql` must be REFUSED on both families, with `needle` in the message.
fn refused(db: &EmbeddedDatabase, sql: &str, needle: &str) {
    for (params_family, family) in FAMILIES {
        match run(db, sql, params_family) {
            Ok(out) => panic!(
                "[{family}] `{sql}` must be refused (expected `{needle}`), got Ok with {} rows",
                out.len()
            ),
            Err(message) => assert!(
                message.contains(needle),
                "[{family}] `{sql}` was refused, but not with `{needle}`: {message}"
            ),
        }
    }
}

/// `na(id, a) = {(1, 10), (2, 20)}`, `nb(id, b) = {(1, 100), (3, 300)}` — the
/// sprinter items' own fixture. Exactly ONE key (`id = 1`) is shared, so a
/// cartesian product (4 rows) cannot be mistaken for the right answer (1 row),
/// and `nb.b` is never above 1000, so a residual `nb.b > 1000` excludes the
/// only matching pair.
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

/// `wa(id, v)` and `wb(id, v)`: the SAME two column names on both sides, with
/// DIFFERENT `v` values per side, so a wildcard that reads one side twice is
/// visible in the values and not only in the column count.
fn duplicate_name_pair() -> EmbeddedDatabase {
    let db = mem_db();
    db.execute("CREATE TABLE wa (id INT PRIMARY KEY, v TEXT)")
        .expect("create wa");
    db.execute("INSERT INTO wa VALUES (1, 'left-one'), (2, 'left-two')")
        .expect("seed wa");
    db.execute("CREATE TABLE wb (id INT PRIMARY KEY, v TEXT)")
        .expect("create wb");
    db.execute("INSERT INTO wb VALUES (1, 'right-one'), (2, 'right-two')")
        .expect("seed wb");
    db
}

// ===========================================================================
// CONTROLS — a file whose every test could pass vacuously is worse than none.
// ===========================================================================

#[test]
fn control_both_families_run_and_a_cross_join_really_does_multiply() {
    let db = natural_pair();
    // The harness reaches both families…
    assert_eq!(rows(&db, "SELECT id FROM na"), vec![vec!["1"], vec!["2"]]);
    // …and a join with NO condition really does produce 2 x 2 rows, which is
    // the wrong answer the USING / NATURAL tests below are asserting against.
    assert_eq!(rows(&db, "SELECT na.id, nb.id FROM na CROSS JOIN nb").len(), 4);
}

// ===========================================================================
// sprinter 6b75aa7a9d42 — `USING` / `NATURAL` must be an EQUI join
// ===========================================================================

#[test]
fn join_using_produces_the_matched_row_not_a_cross_product() {
    // `JoinConstraint::Using` reached a `_ => None` catch-all, so the join ran
    // with no condition at all: `na JOIN nb USING (id)` answered all 2 x 2
    // pairs. Both halves of the fix live in the PLANNER's initial lowering
    // (one `=` per named column) and in the executor's key binder (an
    // all-unqualified `=` keys lhs -> left input, rhs -> right input), so all
    // three planning pipelines get it — which is why both families are
    // asserted here.
    let db = natural_pair();
    // NOTE, and deliberately pinned: this engine does NOT merge the shared
    // output column, so `*` projects `(id, a, id, b)` where PostgreSQL
    // projects `(id, a, b)`. That is a separate, tracked contract move
    // (sprinter 781f55ba534d); what matters here is ONE row, not four.
    assert_eq!(
        rows(&db, "SELECT * FROM na JOIN nb USING (id)"),
        vec![vec!["1", "10", "1", "100"]],
        "USING must key on the named column — one matched row, not the 2x2 cross product"
    );
    assert_eq!(
        rows(&db, "SELECT na.id, na.a, nb.b FROM na JOIN nb USING (id)"),
        vec![vec!["1", "10", "100"]]
    );
    // The outer forms take different executor paths (LEFT keeps the hash join,
    // RIGHT / FULL fall to the nested loop), so each is pinned.
    assert_eq!(
        rows(&db, "SELECT na.id, na.a, nb.b FROM na LEFT JOIN nb USING (id)"),
        vec![vec!["1", "10", "100"], vec!["2", "20", "NULL"]]
    );
    assert_eq!(
        rows(&db, "SELECT na.id, na.a, nb.b FROM na RIGHT JOIN nb USING (id)"),
        vec![vec!["1", "10", "100"], vec!["NULL", "NULL", "300"]]
    );
    assert_eq!(
        rows(&db, "SELECT na.id, na.a, nb.b FROM na FULL JOIN nb USING (id)"),
        vec![
            vec!["1", "10", "100"],
            vec!["2", "20", "NULL"],
            vec!["NULL", "NULL", "300"],
        ]
    );
}

#[test]
fn natural_join_produces_the_matched_row_not_a_cross_product() {
    // Same root cause, same fixture: the lowering emitted
    // `Column{None,"id"} = Column{None,"id"}` and the key binder threw it out
    // as an "alias collision", leaving an empty key set — every pair matched.
    let db = natural_pair();
    assert_eq!(
        rows(&db, "SELECT * FROM na NATURAL JOIN nb"),
        vec![vec!["1", "10", "1", "100"]],
        "NATURAL must key on the common column — one matched row, not the 2x2 cross product"
    );
    assert_eq!(
        rows(&db, "SELECT na.id, na.a, nb.b FROM na NATURAL JOIN nb"),
        vec![vec!["1", "10", "100"]]
    );
    assert_eq!(
        rows(&db, "SELECT na.id, na.a, nb.b FROM na NATURAL LEFT JOIN nb"),
        vec![vec!["1", "10", "100"], vec!["2", "20", "NULL"]]
    );
    assert_eq!(
        rows(&db, "SELECT na.id, na.a, nb.b FROM na NATURAL FULL JOIN nb"),
        vec![
            vec!["1", "10", "100"],
            vec!["2", "20", "NULL"],
            vec!["NULL", "NULL", "300"],
        ]
    );
}

#[test]
fn using_missing_column_is_rejected() {
    // A `USING` column neither side carries — and one only ONE side carries —
    // is `42703 undefined_column`, as in PostgreSQL. Never a silent cross
    // join, which is what the `_ => None` catch-all produced.
    let db = natural_pair();
    refused(&db, "SELECT * FROM na JOIN nb USING (nosuch)", "does not exist");
    refused(&db, "SELECT * FROM na JOIN nb USING (a)", "does not exist");
    refused(&db, "SELECT * FROM na JOIN nb USING (b)", "does not exist");
    // …and the SQLSTATE that carries to a driver is pinned on the wire, in
    // `using_missing_column_is_42703_on_the_wire` below.
}

// ===========================================================================
// sprinter d746afc01c0b — an outer join's ON residual is checked INSIDE it
// ===========================================================================

#[test]
fn left_join_residual_keeps_null_extended_rows() {
    // `split_join_condition` put `nb.b > 1000` in the residual bucket and the
    // residual was attached as a POST-join `FilterOperator`, which sees the
    // NULL-extended row (`NULL > 1000` -> false) and drops it: the params
    // family returned ZERO rows where PostgreSQL returns both `na` rows
    // NULL-extended. A post-join filter is exactly equivalent for INNER only.
    let db = natural_pair();
    assert_eq!(
        rows(
            &db,
            "SELECT na.id, nb.id FROM na LEFT JOIN nb ON na.id = nb.id AND nb.b > 1000"
        ),
        vec![vec!["1", "NULL"], vec!["2", "NULL"]],
        "the whole ON is evaluated per candidate pair; unmatched left rows are NULL-extended"
    );
    // RIGHT and FULL preserve the BUILD side, which the hash join tracks per
    // key bucket rather than per tuple, so they take the nested loop — the
    // same defect with a different mechanism behind the fix.
    assert_eq!(
        rows(
            &db,
            "SELECT na.id, nb.id FROM nb RIGHT JOIN na ON na.id = nb.id AND nb.b > 1000"
        ),
        vec![vec!["1", "NULL"], vec!["2", "NULL"]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT na.id, nb.id FROM na FULL JOIN nb ON na.id = nb.id AND nb.b > 1000"
        ),
        vec![
            vec!["1", "NULL"],
            vec!["2", "NULL"],
            vec!["NULL", "1"],
            vec!["NULL", "3"]
        ]
    );
}

#[test]
fn inner_join_residual_still_filters() {
    // The control for the test above: under INNER the residual MUST still
    // exclude the only equal-key pair (`nb.b` is 100, not > 1000). A fix that
    // simply stopped applying the residual would pass `left_join_…` and fail
    // here.
    let db = natural_pair();
    assert!(
        rows(
            &db,
            "SELECT na.id, nb.id FROM na INNER JOIN nb ON na.id = nb.id AND nb.b > 1000"
        )
        .is_empty(),
        "the residual must still exclude the matching pair under INNER"
    );
    // …and it must ADMIT the pair when the residual is true, so the test is
    // not passing because the residual rejects everything.
    assert_eq!(
        rows(
            &db,
            "SELECT na.id, nb.id FROM na INNER JOIN nb ON na.id = nb.id AND nb.b > 10"
        ),
        vec![vec!["1", "1"]]
    );
    // The same true/false pair under LEFT: the matched row survives, and the
    // unmatched left row is NULL-extended either way.
    assert_eq!(
        rows(
            &db,
            "SELECT na.id, nb.id FROM na LEFT JOIN nb ON na.id = nb.id AND nb.b > 10"
        ),
        vec![vec!["1", "1"], vec!["2", "NULL"]]
    );
}

// ===========================================================================
// sprinter a50328143c63 — a bare `*` over a join keeps both sides' columns
// ===========================================================================

#[test]
fn wildcard_over_join_with_duplicate_column_names_keeps_both_sides() {
    // The bare wildcard expanded to one UNQUALIFIED `Column{None, name}` per
    // input column, and `ProjectOperator` resolves an unqualified name by
    // FIRST match — so both `v` slots re-read `wa.v`. The fix expands the bare
    // wildcard over the RANGE TABLE, stamping each duplicated name with the
    // qualifier its own entry answers to at runtime (what `wa.*` has always
    // done). BEFORE the fix this test reads `left-one` twice.
    let db = duplicate_name_pair();
    assert_eq!(
        rows(&db, "SELECT * FROM wa JOIN wb ON wa.id = wb.id"),
        vec![
            vec!["1", "left-one", "1", "right-one"],
            vec!["2", "left-two", "2", "right-two"],
        ],
        "each `v` output slot must read its OWN side"
    );
    // The written spellings were already right; they are the oracle the
    // wildcard is being held to.
    assert_eq!(
        rows(&db, "SELECT wa.id, wa.v, wb.id, wb.v FROM wa JOIN wb ON wa.id = wb.id"),
        rows(&db, "SELECT * FROM wa JOIN wb ON wa.id = wb.id"),
        "`*` must agree with the written column list"
    );
    assert_eq!(
        rows(&db, "SELECT wa.*, wb.* FROM wa JOIN wb ON wa.id = wb.id"),
        rows(&db, "SELECT * FROM wa JOIN wb ON wa.id = wb.id"),
        "`*` must agree with `wa.*, wb.*`"
    );
}

#[test]
fn wildcard_over_a_derived_table_that_shadows_a_name_keeps_both_sides() {
    // The sprinter item's literal reproduction: the right side is a SUB-SELECT
    // whose own select list renames a column to `v`. Its runtime qualifier is
    // the alias stamp (`SourceAliasOperator`), not a table name, so this
    // exercises the other half of the range-entry qualifier contract.
    let db = duplicate_name_pair();
    assert_eq!(
        rows(
            &db,
            "SELECT * FROM wa JOIN (SELECT id, upper(v) AS v FROM wa) s ON wa.id = s.id"
        ),
        vec![
            vec!["1", "left-one", "1", "LEFT-ONE"],
            vec!["2", "left-two", "2", "LEFT-TWO"],
        ],
        "the derived side's `v` must be ITS `v`, not the base table's"
    );
    assert_eq!(
        rows(
            &db,
            "SELECT wa.id, wa.v, s.id, s.v FROM wa JOIN (SELECT id, upper(v) AS v FROM wa) s ON wa.id = s.id"
        ),
        rows(
            &db,
            "SELECT * FROM wa JOIN (SELECT id, upper(v) AS v FROM wa) s ON wa.id = s.id"
        )
    );
}

#[test]
fn wildcard_over_a_join_without_duplicate_names_is_unchanged() {
    // The control for the shape the fix must NOT touch: when no name is shared
    // the by-name expansion is already correct, and the plan is left exactly
    // as it was. `na`/`nb` share `id` (covered above); `na`/`nc` share nothing.
    let db = natural_pair();
    db.execute("CREATE TABLE nc (nid INT PRIMARY KEY, c INT)")
        .expect("create nc");
    db.execute("INSERT INTO nc VALUES (1, 111), (2, 222)").expect("seed nc");
    assert_eq!(
        rows(&db, "SELECT * FROM na JOIN nc ON na.id = nc.nid"),
        vec![vec!["1", "10", "1", "111"], vec!["2", "20", "2", "222"]]
    );
    // A single-table `*` is the overwhelmingly common shape and must be
    // byte-identical to what it always was.
    assert_eq!(rows(&db, "SELECT * FROM na"), vec![vec!["1", "10"], vec!["2", "20"]]);
    // …including with no FROM at all, which records no range entry.
    assert_eq!(rows(&db, "SELECT 1 AS one"), vec![vec!["1"]]);
}

// ===========================================================================
// The wire: RowDescription names, and the USING SQLSTATE
// ===========================================================================

async fn pg_server() -> (String, tokio::task::JoinHandle<()>) {
    use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr: SocketAddr = listener.local_addr().expect("test addr");
    drop(listener);

    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let config = PgServerConfig::with_address(addr);
    let server = PgServer::new(config, db).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    (
        format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port()),
        handle,
    )
}

async fn pg_client(conn_string: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_postgres::connect(conn_string, tokio_postgres::NoTls),
    )
    .await
    .expect("connect timeout")
    .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[tokio::test]
async fn wildcard_row_description_still_names_both_columns_v() {
    // PostgreSQL's RowDescription for `SELECT * FROM wa JOIN wb …` names BOTH
    // output columns `v`: the qualifier the planner stamps is for RESOLUTION,
    // never for display. A fix that leaked the qualifier into the alias would
    // rename them `wa.v` / `wb.v` and break every client that reads by name.
    let (conn_string, _handle) = pg_server().await;
    let client = pg_client(&conn_string).await;
    for stmt in [
        "CREATE TABLE wa (id INT PRIMARY KEY, v TEXT)",
        "INSERT INTO wa VALUES (1, 'left-one')",
        "CREATE TABLE wb (id INT PRIMARY KEY, v TEXT)",
        "INSERT INTO wb VALUES (1, 'right-one')",
    ] {
        client
            .batch_execute(stmt)
            .await
            .unwrap_or_else(|e| panic!("`{stmt}` must run over the wire: {e}"));
    }

    let rows = client
        .query("SELECT * FROM wa JOIN wb ON wa.id = wb.id", &[])
        .await
        .expect("wildcard join over the wire");
    assert_eq!(rows.len(), 1, "one matched row");
    let names: Vec<&str> = rows[0].columns().iter().map(tokio_postgres::Column::name).collect();
    assert_eq!(
        names,
        vec!["id", "v", "id", "v"],
        "RowDescription keeps the BARE names, as PostgreSQL does"
    );
    // …and the values are still one per side, read positionally.
    assert_eq!(rows[0].get::<usize, i32>(0), 1);
    assert_eq!(rows[0].get::<usize, String>(1), "left-one");
    assert_eq!(rows[0].get::<usize, i32>(2), 1);
    assert_eq!(rows[0].get::<usize, String>(3), "right-one");
}

#[tokio::test]
async fn using_missing_column_is_42703_on_the_wire() {
    // The SQLSTATE a driver actually sees for a `USING` column neither side
    // carries: `42703 undefined_column`, never a successful cross join.
    let (conn_string, _handle) = pg_server().await;
    let client = pg_client(&conn_string).await;
    for stmt in [
        "CREATE TABLE na (id INT PRIMARY KEY, a INT)",
        "INSERT INTO na VALUES (1, 10), (2, 20)",
        "CREATE TABLE nb (id INT PRIMARY KEY, b INT)",
        "INSERT INTO nb VALUES (1, 100), (3, 300)",
    ] {
        client
            .batch_execute(stmt)
            .await
            .unwrap_or_else(|e| panic!("`{stmt}` must run over the wire: {e}"));
    }

    // The control first: the legal spelling answers exactly one row.
    let ok = client
        .query("SELECT na.id, na.a, nb.b FROM na JOIN nb USING (id)", &[])
        .await
        .expect("USING (id) must run");
    assert_eq!(ok.len(), 1, "USING (id) is an equi join, not a cross product");

    let err = match client.query("SELECT * FROM na JOIN nb USING (nosuch)", &[]).await {
        Ok(unexpected) => panic!(
            "`USING (nosuch)` must be refused, got Ok with {} rows",
            unexpected.len()
        ),
        Err(err) => err,
    };
    let db_error = err
        .as_db_error()
        .unwrap_or_else(|| panic!("expected a DbError, got: {err:?}"));
    assert_eq!(
        db_error.code(),
        &tokio_postgres::error::SqlState::UNDEFINED_COLUMN,
        "a USING column no side carries is 42703"
    );
}
