//! Row-level security across QUERY SHAPES — the plan-shape complement to
//! `tests/rls_read_parity_tests.rs` (which covers CALL SITES).
//!
//! The defect this file exists for: a read policy was attached to a scan leaf by
//! TWO different mechanisms depending on which plan node the leaf was. The
//! `Scan` arm of `apply_rls_to_plan_recursive` wrapped a `Filter` ABOVE the scan;
//! the `FilteredScan` arm merged the policy INTO the scan's own predicate. One
//! rule, two implementations.
//!
//! That only matters because of a second mechanism: `ProjectionPruningRule`
//! pushes a projection INTO a bare `Scan` when a `Project{distinct:false}` sits
//! directly above it, and the text-family pipelines apply RLS *after* the
//! optimizer. So for `SELECT id FROM docs` the plan reaching RLS was
//! `Project([id], Scan{projection:[0]})`, the injected `Filter` referenced
//! `owner`, and the scan no longer emitted `owner`:
//!
//! ```text
//! SELECT *          ok    SELECT id            Err "Column 'owner' not found in schema"
//! SELECT id, owner  ok    SELECT body          Err (same)
//! SELECT COUNT(*)   ok    SELECT id LIMIT 1    Err (same)
//! ```
//!
//! Every escaping shape escapes for a *different structural* reason — WHERE
//! interposes a `Filter`/`FilteredScan` so pruning never fires, ORDER BY puts a
//! `Sort` between the `Project` and the `Scan`, DISTINCT fails the rule's
//! `distinct: false` match, aggregates interpose an `Aggregate`, and
//! `SELECT id, owner` prunes but keeps the policy column inside the projection.
//! That last one is why the read-parity suite never caught this: its workhorse
//! read is `SELECT id, owner FROM orders`.
//!
//! The failure was CLOSED (an error, never rows), so the risk this file guards
//! is the *wrong fix*: widening the projection to include the policy column
//! makes `SELECT id` return 3 rows instead of 2, and leaks `owner` into the
//! output schema. Therefore every assertion here pins ROW COUNTS **and
//! CONTENTS**, and the column-name assertions pin that the policy column is
//! absent. A test that only checked `is_ok()` would pass the wrong fix.
//!
//! Both executor families run every shape: the text family (`query`,
//! `query_with_columns`, `execute`) optimizes before applying RLS, the params
//! family (`query_params`, `execute_params`) never optimizes, and
//! `query_params_with_columns` applies RLS *before* optimizing. The fix must be
//! order-independent, so all three see identical rows.
//!
//! Every database here is `new_in_memory()`; nothing touches the repo's `data/`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::tenant::{IsolationMode, RLSCommand, TenantContext, TenantId};
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

const DOCS_DDL: &str = "CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT, body TEXT)";

// ---------------------------------------------------------------------------
// Fixtures (mirroring tests/rls_read_parity_tests.rs)
// ---------------------------------------------------------------------------

fn set_ctx(db: &EmbeddedDatabase, tenant_id: TenantId) {
    db.tenant_manager.set_current_context(TenantContext {
        tenant_id,
        user_id: "alice".to_string(),
        roles: vec!["member".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });
}

/// Register an RLS-enabled (`SharedSchema`) tenant and activate its context.
/// `should_apply_rls` needs all three: a context, an existing tenant, and
/// `rls_enabled` on that tenant.
fn activate_tenant(db: &EmbeddedDatabase) -> TenantId {
    let tenant = db
        .tenant_manager
        .register_tenant("rls-projection".to_string(), IsolationMode::SharedSchema);
    set_ctx(db, tenant.id);
    tenant.id
}

fn policy(db: &EmbeddedDatabase, table: &str, name: &str, cmd: RLSCommand, using: &str) {
    db.tenant_manager.create_rls_policy(
        table.to_string(),
        name.to_string(),
        format!("test policy {name}"),
        cmd,
        using.to_string(),
        None,
    );
}

/// `docs` with three rows, seeded BEFORE any policy exists so the fixture itself
/// is never filtered. ids 1 and 3 are alice's; id 2 is bob's.
fn seed_docs(db: &EmbeddedDatabase) {
    db.execute(DOCS_DDL).unwrap();
    db.execute("INSERT INTO docs VALUES (1, 'alice', 'a1')").unwrap();
    db.execute("INSERT INTO docs VALUES (2, 'bob', 'b1')").unwrap();
    db.execute("INSERT INTO docs VALUES (3, 'alice', 'a2')").unwrap();
}

/// Seeded table + activated tenant + a policy hiding bob's row.
/// `RLSCommand::All` (not `Select`) so the SELECT path is reached through the
/// command-matching branch a real `CREATE POLICY ... FOR ALL` produces.
fn seeded_with_policy(db: &EmbeddedDatabase) -> TenantId {
    seed_docs(db);
    let tenant = activate_tenant(db);
    policy(db, "docs", "read_own", RLSCommand::All, "owner = 'alice'");
    tenant
}

// ---------------------------------------------------------------------------
// Value extraction — panic rather than coerce, so a type surprise is visible
// ---------------------------------------------------------------------------

fn as_i64(value: &Value) -> i64 {
    match value {
        Value::Int2(i) => i64::from(*i),
        Value::Int4(i) => i64::from(*i),
        Value::Int8(i) => *i,
        Value::Numeric(n) => n.parse::<i64>().unwrap_or_else(|_| panic!("non-integer numeric {n:?}")),
        other => panic!("expected an integer value, got {other:?}"),
    }
}

fn as_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => panic!("expected a text value, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The two executor families, run over the same statement
// ---------------------------------------------------------------------------

/// Every row-returning read family that reaches the RLS plan rewrite.
///
/// * `query` / `query_with_columns` — text family: optimize, THEN apply RLS.
///   This is the family the projection bug was measured on.
/// * `query_params` — params family: never optimizes (so it never saw a pruned
///   scan), but after the fix its RLS plans change shape too.
/// * `query_params_with_columns` — applies RLS BEFORE optimizing; pins that the
///   rewrite is order-independent.
fn rows_by_family(db: &EmbeddedDatabase, sql: &str) -> Vec<(&'static str, Vec<Tuple>)> {
    vec![
        ("query", db.query(sql, &[]).unwrap()),
        ("query_with_columns", db.query_with_columns(sql).unwrap().0),
        ("query_params", db.query_params(sql, &[]).unwrap()),
        (
            "query_params_with_columns",
            db.query_params_with_columns(sql, &[]).unwrap().0,
        ),
    ]
}

/// The count-returning twins. `execute*` on a SELECT returns the row count, and
/// that count is itself a disclosure if it is not the filtered one.
fn counts_by_family(db: &EmbeddedDatabase, sql: &str) -> Vec<(&'static str, u64)> {
    vec![
        ("execute", db.execute(sql).unwrap()),
        ("execute_params", db.execute_params(sql, &[]).unwrap()),
    ]
}

/// Assert the exact set of `id` values (column 0) returned by every family, and
/// that both count-returning families agree on the cardinality.
fn assert_ids(db: &EmbeddedDatabase, sql: &str, expected: &[i64]) {
    for (family, rows) in rows_by_family(db, sql) {
        let mut ids: Vec<i64> = rows.iter().map(|row| as_i64(&row.values[0])).collect();
        ids.sort_unstable();
        assert_eq!(
            ids, expected,
            "[{family}] `{sql}` returned the wrong ROWS under the policy \
             (expected ids {expected:?})"
        );
    }
    for (family, count) in counts_by_family(db, sql) {
        assert_eq!(
            count as usize,
            expected.len(),
            "[{family}] `{sql}` returned the wrong row COUNT under the policy"
        );
    }
}

/// Assert the exact set of text values in column 0 across every family.
fn assert_texts(db: &EmbeddedDatabase, sql: &str, expected: &[&str]) {
    for (family, rows) in rows_by_family(db, sql) {
        let mut texts: Vec<String> = rows.iter().map(|row| as_text(&row.values[0])).collect();
        texts.sort();
        assert_eq!(
            texts, expected,
            "[{family}] `{sql}` returned the wrong ROWS under the policy"
        );
    }
}

/// Assert every family errors, and that no family returned rows.
fn assert_all_families_err(db: &EmbeddedDatabase, sql: &str) {
    let mut leaked: Vec<(&'static str, usize)> = Vec::new();
    if let Ok(rows) = db.query(sql, &[]) {
        leaked.push(("query", rows.len()));
    }
    if let Ok((rows, _)) = db.query_with_columns(sql) {
        leaked.push(("query_with_columns", rows.len()));
    }
    if let Ok(rows) = db.query_params(sql, &[]) {
        leaked.push(("query_params", rows.len()));
    }
    if let Ok((rows, _)) = db.query_params_with_columns(sql, &[]) {
        leaked.push(("query_params_with_columns", rows.len()));
    }
    if let Ok(count) = db.execute(sql) {
        leaked.push(("execute", count as usize));
    }
    if let Ok(count) = db.execute_params(sql, &[]) {
        leaked.push(("execute_params", count as usize));
    }
    assert!(
        leaked.is_empty(),
        "a policy that cannot be applied must FAIL CLOSED on every read family; \
         these returned rows instead: {leaked:?}"
    );
}

// ===========================================================================
// The measured table — every shape, both families
// ===========================================================================

/// The regression target. `Project([id], Scan)` is the exact shape
/// `ProjectionPruningRule` rewrites to `Project([id], Scan{projection:[0]})`,
/// which left the injected policy `Filter` referencing a dropped `owner`.
///
/// The assertion is on CONTENTS: the wrong fix (widening the projection to keep
/// the policy column) makes this return ids `[1, 2, 3]`.
#[test]
fn select_single_column_is_filtered_not_errored() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded_with_policy(&db);

    assert_ids(&db, "SELECT id FROM docs", &[1, 3]);
}

/// Same shape, projecting a column that sits AFTER the policy column, so the
/// pruned index set (`[2]`) brackets `owner` from the other side.
#[test]
fn select_body_only_is_filtered_not_errored() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded_with_policy(&db);

    assert_texts(&db, "SELECT body FROM docs", &["a1", "a2"]);
}

/// `LIMIT` wraps OUTERMOST (`Limit(Project(Scan))`), and the optimizer driver
/// recurses into the `Project`, so pruning still fires. Membership is the
/// security assertion: bob's row must never be the one returned.
#[test]
fn select_single_column_with_limit_is_filtered() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded_with_policy(&db);

    for (family, rows) in rows_by_family(&db, "SELECT id FROM docs LIMIT 1") {
        assert_eq!(rows.len(), 1, "[{family}] LIMIT 1 must return exactly one row");
        let id = as_i64(&rows[0].values[0]);
        assert!(
            id == 1 || id == 3,
            "[{family}] LIMIT 1 returned id {id}, which the policy hides"
        );
    }

    // OFFSET walks past the first visible row; it must land on the second
    // VISIBLE row, never on the hidden one.
    for (family, rows) in rows_by_family(&db, "SELECT id FROM docs LIMIT 1 OFFSET 1") {
        assert_eq!(rows.len(), 1, "[{family}] LIMIT 1 OFFSET 1 must return exactly one row");
        let id = as_i64(&rows[0].values[0]);
        assert!(
            id == 1 || id == 3,
            "[{family}] LIMIT 1 OFFSET 1 returned id {id}, which the policy hides"
        );
    }

    // Deterministic variant: with an ORDER BY the visible rows are [1, 3], so
    // OFFSET 1 is exactly 3. If the policy were skipped it would be 2.
    for (family, rows) in rows_by_family(&db, "SELECT id FROM docs ORDER BY id LIMIT 1 OFFSET 1") {
        assert_eq!(rows.len(), 1, "[{family}] ordered LIMIT/OFFSET must return one row");
        assert_eq!(
            as_i64(&rows[0].values[0]),
            3,
            "[{family}] the second VISIBLE row is id 3; id 2 is hidden by the policy"
        );
    }
}

/// The shapes that already worked, pinned so the fix does not move them. Each
/// escapes pruning for a different structural reason — see the module header.
#[test]
fn previously_working_shapes_stay_filtered() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded_with_policy(&db);

    // Wildcard: expands to every column, so pruning cannot shrink anything.
    assert_ids(&db, "SELECT * FROM docs", &[1, 3]);
    // Prunes to [0,1], but the policy column survives at index 1.
    assert_ids(&db, "SELECT id, owner FROM docs", &[1, 3]);
    // WHERE interposes a Filter/FilteredScan, so the Project's input is not a
    // bare Scan and pruning never fires.
    assert_ids(&db, "SELECT id FROM docs WHERE id > 0", &[1, 3]);
    // A non-pushable predicate keeps the executor-level Filter shape.
    assert_ids(&db, "SELECT id FROM docs WHERE id = 1 OR id = 3", &[1, 3]);
    // ORDER BY places the Sort BELOW the Project.
    assert_ids(&db, "SELECT id FROM docs ORDER BY id", &[1, 3]);
    // DISTINCT fails the pruning rule's `distinct: false` match.
    assert_ids(&db, "SELECT DISTINCT id FROM docs", &[1, 3]);
}

/// Ordered reads must also be ordered CORRECTLY, not merely filtered: the fix
/// moves the policy from a `Filter` above the scan into the scan itself, which
/// is a different point in the pipeline relative to `Sort`.
#[test]
fn order_by_returns_visible_rows_in_order() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded_with_policy(&db);

    for (family, rows) in rows_by_family(&db, "SELECT id FROM docs ORDER BY id") {
        let ids: Vec<i64> = rows.iter().map(|row| as_i64(&row.values[0])).collect();
        assert_eq!(
            ids,
            vec![1, 3],
            "[{family}] ORDER BY must yield the visible rows in order"
        );
    }
}

/// Aggregates run over the FILTERED row set. `SUM` is the sharper probe: a
/// count can be right by accident, but 1+3=4 can only come from the correct two
/// rows (an unfiltered sum is 6, and bob's row alone would move it).
#[test]
fn aggregates_run_over_filtered_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded_with_policy(&db);

    for (family, rows) in rows_by_family(&db, "SELECT COUNT(*) FROM docs") {
        assert_eq!(rows.len(), 1, "[{family}] COUNT(*) returns one row");
        assert_eq!(
            as_i64(&rows[0].values[0]),
            2,
            "[{family}] COUNT(*) must count only the rows the policy shows"
        );
    }
    for (family, rows) in rows_by_family(&db, "SELECT SUM(id) FROM docs") {
        assert_eq!(rows.len(), 1, "[{family}] SUM returns one row");
        assert_eq!(
            as_i64(&rows[0].values[0]),
            4,
            "[{family}] SUM(id) over the visible rows is 1+3=4 (unfiltered would be 6)"
        );
    }
}

/// Control: with no tenant context the rewrite is skipped entirely and all three
/// rows are visible. Without this, a fix that broke reads outright would still
/// pass every filtered assertion above.
#[test]
fn no_tenant_context_sees_every_row() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_docs(&db);
    let tenant = activate_tenant(&db);
    policy(&db, "docs", "read_own", RLSCommand::All, "owner = 'alice'");
    db.tenant_manager.clear_current_context();

    assert_ids(&db, "SELECT id FROM docs", &[1, 2, 3]);
    assert_ids(&db, "SELECT * FROM docs", &[1, 2, 3]);
    assert_texts(&db, "SELECT body FROM docs", &["a1", "a2", "b1"]);

    // And the policy still applies once the context returns.
    set_ctx(&db, tenant);
    assert_ids(&db, "SELECT id FROM docs", &[1, 3]);
}

// ===========================================================================
// The policy column must never reach the caller
// ===========================================================================

/// The output schema is what the wire's `RowDescription` and every embedded
/// caller sees. A fix that widened the projection to keep `owner` addressable
/// would surface it here — turning a CLOSED failure into an OPEN one.
#[test]
fn policy_column_never_appears_in_output_columns() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded_with_policy(&db);

    let (rows, columns) = db.query_with_columns("SELECT id FROM docs").unwrap();
    assert_eq!(columns, vec!["id".to_string()], "text family leaked the policy column");
    assert_eq!(rows.len(), 2, "text family returned the wrong row count");
    for row in &rows {
        assert_eq!(row.values.len(), 1, "a projected row carried an extra value: {row:?}");
    }

    let (rows, columns) = db.query_params_with_columns("SELECT id FROM docs", &[]).unwrap();
    assert_eq!(
        columns,
        vec!["id".to_string()],
        "params family leaked the policy column"
    );
    assert_eq!(rows.len(), 2, "params family returned the wrong row count");
    for row in &rows {
        assert_eq!(row.values.len(), 1, "a projected row carried an extra value: {row:?}");
    }

    // Same for a projection that excludes the policy column on the other side.
    let (_, columns) = db.query_with_columns("SELECT body FROM docs").unwrap();
    assert_eq!(columns, vec!["body".to_string()]);
}

/// The strongest form: a policy on a column the user has NO other way to read.
/// The predicate must still be evaluated (pre-projection, against the base
/// table) while the column stays invisible in the result.
#[test]
fn policy_on_a_column_the_projection_never_exposes() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE notes (id INT PRIMARY KEY, body TEXT, secret TEXT)")
        .unwrap();
    db.execute("INSERT INTO notes VALUES (1, 'n1', 'keep')").unwrap();
    db.execute("INSERT INTO notes VALUES (2, 'n2', 'drop')").unwrap();
    db.execute("INSERT INTO notes VALUES (3, 'n3', 'drop')").unwrap();
    activate_tenant(&db);
    policy(&db, "notes", "secret_only", RLSCommand::All, "secret = 'keep'");

    for (family, rows) in rows_by_family(&db, "SELECT id FROM notes") {
        let ids: Vec<i64> = rows.iter().map(|row| as_i64(&row.values[0])).collect();
        assert_eq!(ids, vec![1], "[{family}] the invisible-column policy did not filter");
        assert_eq!(
            rows[0].values.len(),
            1,
            "[{family}] the policy column leaked into the row"
        );
    }

    let (_, columns) = db.query_with_columns("SELECT id FROM notes").unwrap();
    assert!(
        !columns.iter().any(|c| c.eq_ignore_ascii_case("secret")),
        "the policy column must not appear in the output columns: {columns:?}"
    );
}

/// A policy referencing TWO columns, neither of them projected. Both must be
/// decoded for the predicate and neither may reach the caller.
#[test]
fn two_column_policy_with_neither_column_projected() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_docs(&db);
    activate_tenant(&db);
    policy(
        &db,
        "docs",
        "own_and_early",
        RLSCommand::Select,
        "owner = 'alice' AND id < 3",
    );

    // Only row 1 satisfies both conjuncts; `SELECT body` projects neither
    // `owner` nor `id`.
    assert_texts(&db, "SELECT body FROM docs", &["a1"]);

    let (rows, columns) = db.query_with_columns("SELECT body FROM docs").unwrap();
    assert_eq!(columns, vec!["body".to_string()]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values.len(), 1, "policy columns leaked into the row");
}

// ===========================================================================
// Predicate kinds the storage layer cannot push down
// ===========================================================================

/// A policy whose predicate is a FUNCTION call cannot be extracted into a
/// storage-level comparison, so the scan must fall back to the full-width
/// evaluator re-filter. That fallback is only correct if it runs against the
/// un-projected schema — the exact property the fix depends on.
#[test]
fn function_valued_policy_filters_under_a_pruned_projection() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute(DOCS_DDL).unwrap();

    let tenant = db
        .tenant_manager
        .register_tenant("rls-fn".to_string(), IsolationMode::SharedSchema);
    let tenant_id = tenant.id.to_string();

    db.execute(&format!("INSERT INTO docs VALUES (1, '{tenant_id}', 'a1')"))
        .unwrap();
    db.execute("INSERT INTO docs VALUES (2, 'other-tenant', 'b1')").unwrap();
    db.execute(&format!("INSERT INTO docs VALUES (3, '{tenant_id}', 'a2')"))
        .unwrap();

    set_ctx(&db, tenant.id);
    policy(&db, "docs", "by_tenant", RLSCommand::All, "owner = current_tenant()");

    assert_ids(&db, "SELECT id FROM docs", &[1, 3]);
    assert_texts(&db, "SELECT body FROM docs", &["a1", "a2"]);
    assert_ids(&db, "SELECT id FROM docs LIMIT 2", &[1, 3]);
}

// ===========================================================================
// Joins — the policied side is a join input
// ===========================================================================

/// One side policied. The projected variant additionally exercises join-input
/// pruning, which builds `FilteredScan{projection: Some, predicate}` where the
/// predicate column is OUTSIDE the projection — the same shape the fix now
/// produces at the top level.
#[test]
fn join_with_one_policied_side_filters_that_side() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_docs(&db);
    db.execute("CREATE TABLE refs (id INT PRIMARY KEY, doc_id INT)")
        .unwrap();
    db.execute("INSERT INTO refs VALUES (10, 1)").unwrap();
    db.execute("INSERT INTO refs VALUES (20, 2)").unwrap();
    db.execute("INSERT INTO refs VALUES (30, 3)").unwrap();
    activate_tenant(&db);
    policy(&db, "docs", "read_own", RLSCommand::All, "owner = 'alice'");

    // Both sides projected: doc 2 is hidden, so its ref must not join through.
    for (family, rows) in rows_by_family(
        &db,
        "SELECT d.id, r.id FROM docs d JOIN refs r ON r.doc_id = d.id ORDER BY d.id",
    ) {
        let pairs: Vec<(i64, i64)> = rows
            .iter()
            .map(|row| (as_i64(&row.values[0]), as_i64(&row.values[1])))
            .collect();
        assert_eq!(
            pairs,
            vec![(1, 10), (3, 30)],
            "[{family}] the join exposed a row the policy hides on the docs side"
        );
    }

    // Only the UNPOLICIED side projected: the policy column is not in the
    // output at all, and the policied input is a pruned join input.
    for (family, rows) in rows_by_family(
        &db,
        "SELECT r.id FROM docs d JOIN refs r ON r.doc_id = d.id ORDER BY r.id",
    ) {
        let ids: Vec<i64> = rows.iter().map(|row| as_i64(&row.values[0])).collect();
        assert_eq!(
            ids,
            vec![10, 30],
            "[{family}] a join projecting only the unpolicied side leaked a hidden row"
        );
    }
}

// ===========================================================================
// Caches and session state
// ===========================================================================

/// The text family caches OPTIMIZED plans and re-applies RLS to a CLONE on every
/// hit. This pins the whole round trip: a no-context read warms the caches with
/// a pruned plan and unfiltered rows, a context-active read of the SAME text
/// must return the filtered rows (not the cached ones, and not an error from the
/// pruned plan), and clearing the context must restore the unfiltered result —
/// proving the cached plan was never mutated in place.
#[test]
fn plan_cache_round_trip_does_not_poison_or_leak() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_docs(&db);

    // Warm plan + result caches with no context. Three reads: the result cache
    // is a seen-twice admission filter, so the third read is the cached one.
    let warm: Vec<usize> = (0..3)
        .map(|_| db.query("SELECT id FROM docs", &[]).unwrap().len())
        .collect();
    assert_eq!(warm, [3, 3, 3], "the no-context reads must see all three rows");

    // Now introduce the tenant and policy and read the IDENTICAL text. This is
    // the cached-optimized-plan path: the cached plan already has the pruned
    // projection, and RLS is applied to a clone of it.
    let tenant = activate_tenant(&db);
    policy(&db, "docs", "read_own", RLSCommand::All, "owner = 'alice'");

    for _ in 0..3 {
        let mut ids: Vec<i64> = db
            .query("SELECT id FROM docs", &[])
            .unwrap()
            .iter()
            .map(|row| as_i64(&row.values[0]))
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![1, 3],
            "a context-active read of a cached, already-pruned plan must be filtered"
        );
    }

    // Clearing the context must restore the unfiltered result: the rewrite runs
    // on a clone, so the cached plan itself must still be policy-free.
    db.tenant_manager.clear_current_context();
    let mut ids: Vec<i64> = db
        .query("SELECT id FROM docs", &[])
        .unwrap()
        .iter()
        .map(|row| as_i64(&row.values[0]))
        .collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![1, 2, 3],
        "the RLS rewrite mutated the CACHED plan instead of a clone"
    );

    // And back again, to pin that the no-context read did not poison it either.
    set_ctx(&db, tenant);
    assert_ids(&db, "SELECT id FROM docs", &[1, 3]);
}

/// The PG simple-query protocol routes through ONE function that branches on
/// whether the session has an open transaction: autocommit delegates to the
/// optimized text path, while inside `BEGIN` it plans WITHOUT the optimizer.
/// Before the fix those two branches DISAGREED on the same statement — the
/// autocommit half errored (its plan had been pruned) and the in-transaction
/// half returned rows. Both must now return the same filtered rows.
#[test]
fn simple_protocol_autocommit_and_in_transaction_agree() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded_with_policy(&db);

    let sid = db.create_session("rls_shapes", IsolationLevel::ReadCommitted).unwrap();

    // Autocommit branch: no session transaction is open.
    let (rows, columns) = db.query_with_columns_for_session(sid, "SELECT id FROM docs").unwrap();
    let mut autocommit_ids: Vec<i64> = rows.iter().map(|row| as_i64(&row.values[0])).collect();
    autocommit_ids.sort_unstable();
    assert_eq!(
        autocommit_ids,
        vec![1, 3],
        "the autocommit simple-query branch must return the filtered rows"
    );
    assert_eq!(columns, vec!["id".to_string()]);

    // In-transaction branch: same statement, same session, unoptimized plan.
    db.begin_transaction_for_session(sid).unwrap();
    let (rows, columns) = db.query_with_columns_for_session(sid, "SELECT id FROM docs").unwrap();
    let mut in_txn_ids: Vec<i64> = rows.iter().map(|row| as_i64(&row.values[0])).collect();
    in_txn_ids.sort_unstable();
    db.rollback_transaction_for_session(sid).unwrap();
    db.destroy_session(sid).unwrap();

    assert_eq!(
        in_txn_ids,
        vec![1, 3],
        "the in-transaction simple-query branch must return the filtered rows"
    );
    assert_eq!(columns, vec!["id".to_string()]);
    assert_eq!(
        autocommit_ids, in_txn_ids,
        "the SAME statement must not depend on whether a transaction is open"
    );
}

// ===========================================================================
// Fail-closed and degenerate pins
// ===========================================================================

/// A policy that cannot be parsed must make every read FAIL, on every family and
/// every shape — including the pruned-projection shapes this file exists for.
/// The parse now happens in one shared helper, so this pins that the helper
/// propagates the error rather than downgrading it to "no policy".
#[test]
fn unparseable_policy_fails_closed_on_pruned_shapes() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_docs(&db);
    activate_tenant(&db);
    policy(&db, "docs", "broken", RLSCommand::All, "owner = = 'alice'");

    assert_all_families_err(&db, "SELECT id FROM docs");
    assert_all_families_err(&db, "SELECT body FROM docs");
    assert_all_families_err(&db, "SELECT id FROM docs LIMIT 1");
    assert_all_families_err(&db, "SELECT * FROM docs");
    assert_all_families_err(&db, "SELECT COUNT(*) FROM docs");
}

/// An empty table returns zero rows without error. The original defect only
/// fired once a row reached the injected filter, so a suite that tested the
/// empty case alone would have reported the bug as fixed while it was live.
#[test]
fn empty_table_under_policy_returns_no_rows_without_error() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute(DOCS_DDL).unwrap();
    activate_tenant(&db);
    policy(&db, "docs", "read_own", RLSCommand::All, "owner = 'alice'");

    assert_ids(&db, "SELECT id FROM docs", &[]);
    assert_ids(&db, "SELECT * FROM docs", &[]);
    assert_texts(&db, "SELECT body FROM docs", &[]);

    for (family, rows) in rows_by_family(&db, "SELECT COUNT(*) FROM docs") {
        assert_eq!(rows.len(), 1, "[{family}] COUNT(*) always returns one row");
        assert_eq!(
            as_i64(&rows[0].values[0]),
            0,
            "[{family}] COUNT(*) over an empty policied table is 0"
        );
    }
}

/// A policy that matches NOTHING must return nothing — not everything. Pins the
/// direction a "predicate silently dropped" regression would fail in.
#[test]
fn policy_matching_no_rows_returns_no_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_docs(&db);
    activate_tenant(&db);
    policy(&db, "docs", "nobody", RLSCommand::All, "owner = 'nobody'");

    assert_ids(&db, "SELECT id FROM docs", &[]);
    assert_texts(&db, "SELECT body FROM docs", &[]);
    assert_ids(&db, "SELECT id FROM docs LIMIT 1", &[]);
}
