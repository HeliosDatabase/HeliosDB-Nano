//! Row-level-security enforcement on the READ paths.
//!
//! The v4.9.0 write-path fix (`bfe9115`) left a residual on reads
//! (`docs/plans/ROADMAP_V5.md` §1.1, "Residual"): `apply_rls_to_plan` was always
//! correct, but a set of execution sites never called it, and the shared result
//! cache was keyed on SQL text alone with no tenant component. Concretely:
//!
//!   * **Hole 1** — `execute()` / `execute_params()` on a SELECT counted rows
//!     through a catch-all arm that skipped RLS, so the returned `u64` was the
//!     raw count. Two executor families, same defect, one call apart.
//!   * **Hole 2** — the result cache leaked in BOTH directions: an unfiltered
//!     entry written by a no-context caller was served verbatim to a
//!     context-active reader, AND a filtered entry written under a context was
//!     served to a later no-context reader. Measured, both ways.
//!   * **Hole 3** — `query_with_columns` (the wire simple-query surface) never
//!     called `apply_rls_to_plan` on either of its execution branches.
//!   * **Hole 4** — not a live bug: `try_normalized_query_with_columns` already
//!     bails when a context is active. Pinned below anyway, because it is one
//!     boolean in an `||` chain and nothing else explains why it is there.
//!   * **Hole 5** — every transaction-scoped read (`Transaction::query`, the
//!     in-txn branches of `query`/`query_in_session`/`query_*_for_session`, the
//!     PREPARE/EXECUTE emulation) hand-rolled a `sql::Executor` with no RLS
//!     anywhere. Unlike 1–4 this needs no cache state and no particular query
//!     shape — it fires on the first read inside any transaction, which for the
//!     documented RAII `Transaction<'_>` pattern is the steady state.
//!
//! Every test below ASSERTS. The bug class this file exists for shipped behind
//! unconditional `println!("✓ … (RLS protected)")` in a suite of 58 green tests,
//! so a test here that reports without asserting is itself a defect.
//!
//! Verification reads clear the tenant context first: RLS filters SELECT too, so
//! a read taken *under* an active policy cannot distinguish "absent" from
//! "hidden" — the exact confusion that let the earlier suite print checkmarks.
//!
//! Every database here is `new_in_memory()`; nothing touches the repo's `data/`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::tenant::{IsolationMode, RLSCommand, TenantContext, TenantId};
use heliosdb_nano::{EmbeddedDatabase, Value};

const ORDERS_DDL: &str = "CREATE TABLE orders (id INT PRIMARY KEY, owner TEXT NOT NULL, amount INT)";
/// The workhorse read. No WHERE clause, so it cannot take a literal fast path —
/// only the policy can keep bob's row out of the result.
const SELECT_ALL: &str = "SELECT id, owner FROM orders";
/// Parameterized twin. `amount > $1` matches both seeded rows.
const SELECT_ALL_PARAMS: &str = "SELECT id, owner FROM orders WHERE amount > $1";

// ---------------------------------------------------------------------------
// Fixtures (mirroring tests/rls_write_parity_tests.rs)
// ---------------------------------------------------------------------------

/// Register an RLS-enabled (`SharedSchema`) tenant and activate its context.
/// `should_apply_rls` needs all three: a context, an existing tenant, and
/// `rls_enabled` on that tenant.
fn activate_tenant(db: &EmbeddedDatabase) -> TenantId {
    let tenant = db
        .tenant_manager
        .register_tenant("rls-read".to_string(), IsolationMode::SharedSchema);
    set_ctx(db, tenant.id);
    tenant.id
}

fn set_ctx(db: &EmbeddedDatabase, tenant_id: TenantId) {
    db.tenant_manager.set_current_context(TenantContext {
        tenant_id,
        user_id: "alice".to_string(),
        roles: vec!["member".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });
}

/// `orders` with two rows, seeded BEFORE any policy exists so the fixture itself
/// is never filtered: id 1 is alice's, id 2 is bob's.
fn seed_orders(db: &EmbeddedDatabase) {
    db.execute(ORDERS_DDL).unwrap();
    db.execute("INSERT INTO orders VALUES (1, 'alice', 10)").unwrap();
    db.execute("INSERT INTO orders VALUES (2, 'bob', 20)").unwrap();
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

/// Seeded table + activated tenant + a SELECT policy hiding bob's row.
fn seeded_with_policy(db: &EmbeddedDatabase) -> TenantId {
    seed_orders(db);
    let tenant = activate_tenant(db);
    policy(db, "orders", "read_own", RLSCommand::Select, "owner = 'alice'");
    tenant
}

// ---------------------------------------------------------------------------
// Ground-truth reads
// ---------------------------------------------------------------------------

/// Count what is ACTUALLY in the table, not what the policy would show: clear
/// the context, read, restore it. Under an active policy a read cannot tell
/// "the row is gone" from "the row is hidden from me".
fn count_unfiltered(db: &EmbeddedDatabase, tenant: TenantId, sql: &str) -> usize {
    db.tenant_manager.clear_current_context();
    let rows = db.query(sql, &[]).unwrap();
    set_ctx(db, tenant);
    rows.len()
}

/// Same, for a database whose tenant context was never set.
fn count_no_context(db: &EmbeddedDatabase, sql: &str) -> usize {
    db.query(sql, &[]).unwrap().len()
}

// ---------------------------------------------------------------------------
// Every read entry point, in one place
// ---------------------------------------------------------------------------

/// Run the same logical read through every entry point this fix touches and
/// return `(label, row_count)` for each.
///
/// Transaction-scoped entry points are exercised with their transaction OPEN —
/// that is the hole-5 shape, and the only shape in which those branches run at
/// all. Each opens and closes its own transaction so the entry points do not
/// perturb one another (an open global `BEGIN` changes `in_transaction()`, which
/// several of the autocommit paths gate their caching on).
///
/// Used by the two "must stay free" regressions and by the fail-closed pin, so
/// that a new bypass cannot be introduced on a path no test walks.
fn read_counts(db: &EmbeddedDatabase) -> Vec<(&'static str, usize)> {
    // --- autocommit ---
    // Hole 1: the `execute*` pair return a COUNT, and that count is the disclosure.
    let mut out: Vec<(&'static str, usize)> = vec![
        ("query", db.query(SELECT_ALL, &[]).unwrap().len()),
        ("query_with_columns", db.query_with_columns(SELECT_ALL).unwrap().0.len()),
        (
            "query_params",
            db.query_params(SELECT_ALL_PARAMS, &[Value::Int4(0)]).unwrap().len(),
        ),
        (
            "query_params_with_columns",
            db.query_params_with_columns(SELECT_ALL_PARAMS, &[Value::Int4(0)])
                .unwrap()
                .0
                .len(),
        ),
        ("execute", db.execute(SELECT_ALL).unwrap() as usize),
        ("execute_params", db.execute_params(SELECT_ALL, &[]).unwrap() as usize),
    ];

    // --- Hole 5f: SQL-text PREPARE / EXECUTE emulation ---
    db.query("PREPARE rp AS SELECT id, owner FROM orders", &[]).unwrap();
    out.push(("prepared_execute", db.query("EXECUTE rp", &[]).unwrap().len()));
    db.query("DEALLOCATE rp", &[]).unwrap();

    // --- Hole 5a: SQL-text BEGIN, global transaction slot ---
    db.execute("BEGIN").unwrap();
    out.push(("query_in_begin", db.query(SELECT_ALL, &[]).unwrap().len()));
    db.execute("COMMIT").unwrap();

    // --- Hole 5b: the documented RAII handle ---
    {
        let tx = db.begin_transaction().unwrap();
        out.push(("transaction_query", tx.query(SELECT_ALL, &[]).unwrap().len()));
        tx.rollback().unwrap();
    }

    // --- Holes 5c/5d/5e/5g: session transactions ---
    let sid = db.create_session("rls_reader", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(sid).unwrap();
    out.push((
        "query_with_columns_for_session",
        db.query_with_columns_for_session(sid, SELECT_ALL).unwrap().0.len(),
    ));
    out.push((
        "query_params_for_session",
        db.query_params_for_session(sid, SELECT_ALL_PARAMS, &[Value::Int4(0)])
            .unwrap()
            .len(),
    ));
    out.push((
        "query_params_with_columns_for_session",
        db.query_params_with_columns_for_session(sid, SELECT_ALL_PARAMS, &[Value::Int4(0)])
            .unwrap()
            .0
            .len(),
    ));
    out.push((
        "query_in_session",
        db.query_in_session(sid, SELECT_ALL, &[]).unwrap().len(),
    ));
    db.rollback_transaction_for_session(sid).unwrap();

    // Session with NO open transaction — delegates to the autocommit path.
    out.push((
        "query_with_columns_for_session_autocommit",
        db.query_with_columns_for_session(sid, SELECT_ALL).unwrap().0.len(),
    ));
    db.destroy_session(sid).unwrap();

    out
}

/// Same entry points as [`read_counts`], but recording whether each one
/// SUCCEEDED. Used by the fail-closed pin: under a policy that cannot be
/// parsed, every one of these must return `Err`, so the returned list of
/// still-succeeding labels must be empty.
fn read_entry_points_that_succeeded(db: &EmbeddedDatabase) -> Vec<&'static str> {
    // --- autocommit ---
    let mut checked: Vec<(&'static str, bool)> = vec![
        ("query", db.query(SELECT_ALL, &[]).is_ok()),
        ("query_with_columns", db.query_with_columns(SELECT_ALL).is_ok()),
        (
            "query_params",
            db.query_params(SELECT_ALL_PARAMS, &[Value::Int4(0)]).is_ok(),
        ),
        (
            "query_params_with_columns",
            db.query_params_with_columns(SELECT_ALL_PARAMS, &[Value::Int4(0)])
                .is_ok(),
        ),
        ("execute", db.execute(SELECT_ALL).is_ok()),
        ("execute_params", db.execute_params(SELECT_ALL, &[]).is_ok()),
    ];

    // --- PREPARE / EXECUTE emulation ---
    db.query("PREPARE fp AS SELECT id, owner FROM orders", &[]).unwrap();
    checked.push(("prepared_execute", db.query("EXECUTE fp", &[]).is_ok()));
    db.query("DEALLOCATE fp", &[]).unwrap();

    // --- SQL-text BEGIN ---
    db.execute("BEGIN").unwrap();
    checked.push(("query_in_begin", db.query(SELECT_ALL, &[]).is_ok()));
    db.execute("ROLLBACK").unwrap();

    // --- RAII transaction handle ---
    {
        let tx = db.begin_transaction().unwrap();
        checked.push(("transaction_query", tx.query(SELECT_ALL, &[]).is_ok()));
        tx.rollback().unwrap();
    }

    // --- session transaction ---
    let sid = db
        .create_session("rls_failclosed", IsolationLevel::ReadCommitted)
        .unwrap();
    db.begin_transaction_for_session(sid).unwrap();
    checked.push((
        "query_with_columns_for_session",
        db.query_with_columns_for_session(sid, SELECT_ALL).is_ok(),
    ));
    checked.push((
        "query_params_for_session",
        db.query_params_for_session(sid, SELECT_ALL_PARAMS, &[Value::Int4(0)])
            .is_ok(),
    ));
    checked.push((
        "query_params_with_columns_for_session",
        db.query_params_with_columns_for_session(sid, SELECT_ALL_PARAMS, &[Value::Int4(0)])
            .is_ok(),
    ));
    checked.push(("query_in_session", db.query_in_session(sid, SELECT_ALL, &[]).is_ok()));
    db.rollback_transaction_for_session(sid).unwrap();
    db.destroy_session(sid).unwrap();

    checked
        .into_iter()
        .filter_map(|(label, succeeded)| succeeded.then_some(label))
        .collect()
}

// ---------------------------------------------------------------------------
// Hole 2 — the result cache, both measured directions
// ---------------------------------------------------------------------------

/// Direction A: a no-context read warms the SQL-text-keyed cache with UNFILTERED
/// rows; a later context-active read of the same text must not be served that
/// entry. This is the "cache seeded with no context served 2 rows to a
/// policy-bound reader where 1 is correct" measurement.
///
/// Also the "already-poisoned entry" case in its strongest form: the entry is in
/// the cache *before* the fix's write gate can have any say, so only the READ
/// gate can save it. A write-only fix passes nothing here.
#[test]
fn cache_no_context_write_not_served_to_tenant_reader() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_orders(&db);

    // Warm the cache with no context active. `cache_admits` is a seen-twice
    // filter, so the entry is only admitted on the second sighting; the third
    // read is the one served from the cache.
    let warm: Vec<usize> = (0..3).map(|_| db.query(SELECT_ALL, &[]).unwrap().len()).collect();
    assert_eq!(
        warm,
        [2, 2, 2],
        "the no-context reads that seed the cache must see both rows"
    );

    // NOW introduce the tenant and the policy, and read the identical text.
    let tenant = activate_tenant(&db);
    policy(&db, "orders", "read_own", RLSCommand::Select, "owner = 'alice'");

    let under_ctx = db.query(SELECT_ALL, &[]).unwrap();
    let shown = format!("{under_ctx:?}");
    assert_eq!(
        under_ctx.len(),
        1,
        "a context-active read was served the pre-existing UNFILTERED cache entry \
         (result cache is keyed on SQL text alone, with no tenant component): {shown}"
    );
    assert!(
        shown.contains("alice") && !shown.contains("bob"),
        "the filtered read must return alice's row and only it: {shown}"
    );
    assert_eq!(
        count_unfiltered(&db, tenant, SELECT_ALL),
        2,
        "ground truth: both rows are still in the table — the read was filtered, not destructive"
    );
}

/// Direction B: a context-active read must not leave a FILTERED result in the
/// shared cache for a later no-context (or different-tenant) reader. This is the
/// "a tenant-context read cached its filtered rows under the bare SQL key and a
/// later no-context read got 1 row where 2 exist" measurement.
#[test]
fn cache_tenant_write_not_served_to_later_reader() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);

    // Three reads under the context: enough for the seen-twice admission filter
    // to fire, which is when the pre-fix code wrote the filtered rows into the
    // shared cache under the bare SQL key.
    let filtered: Vec<usize> = (0..3).map(|_| db.query(SELECT_ALL, &[]).unwrap().len()).collect();
    assert_eq!(
        filtered,
        [1, 1, 1],
        "every read under the policy must see exactly alice's row"
    );

    // The same text, read with no context at all.
    db.tenant_manager.clear_current_context();
    let unfiltered = db.query(SELECT_ALL, &[]).unwrap();
    let shown = format!("{unfiltered:?}");
    assert_eq!(
        unfiltered.len(),
        2,
        "a no-context reader was served the tenant's FILTERED cache entry: {shown}"
    );
    assert!(
        shown.contains("bob"),
        "the no-context read must include the row the tenant's policy hid: {shown}"
    );

    // And the policy still applies once the context comes back — the no-context
    // read must not have poisoned the cache in the other direction either.
    set_ctx(&db, tenant);
    assert_eq!(
        db.query(SELECT_ALL, &[]).unwrap().len(),
        1,
        "restoring the context must restore the filter; the no-context read must not have \
         left an unfiltered entry the policy-bound reader can pick up"
    );
}

/// The single-slot hot result cache is a SEPARATE call site from the sharded
/// LRU: `query()` probes `hot_cached_query_result` directly before it ever
/// reaches `cached_query_result`, so it needs its own gate rather than a
/// rely-on-the-caller contract. Warm it immediately before switching context so
/// the hot slot — not the LRU — is what holds the pre-existing entry.
#[test]
fn cache_pre_existing_entry_inert_under_context() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_orders(&db);

    // Two back-to-back reads of the same text: the second admits and installs
    // the rows in the hot single-slot entry, which is what the next read of the
    // same text hits first.
    assert_eq!(db.query(SELECT_ALL, &[]).unwrap().len(), 2);
    assert_eq!(db.query(SELECT_ALL, &[]).unwrap().len(), 2);

    let tenant = activate_tenant(&db);
    policy(&db, "orders", "read_own", RLSCommand::Select, "owner = 'alice'");

    assert_eq!(
        db.query(SELECT_ALL, &[]).unwrap().len(),
        1,
        "the hot single-slot cache entry, written before the policy existed, was served to a \
         policy-bound reader"
    );
    assert_eq!(
        count_unfiltered(&db, tenant, SELECT_ALL),
        2,
        "ground truth: both rows are still present"
    );
}

// ---------------------------------------------------------------------------
// Hole 1 — execute() / execute_params() on a SELECT
// ---------------------------------------------------------------------------

/// `execute()` accepts arbitrary SQL by design (its catch-all comment says so),
/// and returns a row count. Under a policy that count must be the FILTERED one:
/// a row count is exactly the kind of aggregate a `SELECT count(*)` audit would
/// also need filtered.
#[test]
fn execute_on_select_returns_filtered_count() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);

    assert_eq!(
        db.execute(SELECT_ALL).ok(),
        Some(1),
        "execute() on a SELECT returned the RAW row count, disclosing the existence of a row \
         the policy hides (text family catch-all)"
    );
    assert_eq!(
        count_unfiltered(&db, tenant, SELECT_ALL),
        2,
        "ground truth: the table really does hold two rows"
    );
}

/// The params-family twin, one call away: `execute_params` → `execute_params_inner`
/// → `execute_plan_with_params_inner`'s catch-all. Same defect, second executor
/// family — closing only `execute()` would reopen the identical hole.
#[test]
fn execute_params_on_select_returns_filtered_count() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);

    assert_eq!(
        db.execute_params(SELECT_ALL, &[]).ok(),
        Some(1),
        "execute_params() on a SELECT returned the RAW row count (params family catch-all)"
    );
    assert_eq!(
        db.execute_params(SELECT_ALL_PARAMS, &[Value::Int4(0)]).ok(),
        Some(1),
        "the same holds with bound parameters"
    );
    assert_eq!(
        count_unfiltered(&db, tenant, SELECT_ALL),
        2,
        "ground truth: the table really does hold two rows"
    );
}

// ---------------------------------------------------------------------------
// Hole 3 — query_with_columns
// ---------------------------------------------------------------------------

/// `query_with_columns` is the wire simple-query surface and had no
/// `apply_rls_to_plan` call on EITHER of its execution branches. Called three
/// times: the first is the cold path, the second admits the plan to the cache
/// (seen-twice), the third takes the plan-cache-hit branch. Both branches must
/// filter.
#[test]
fn query_with_columns_applies_rls() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);

    let counts: Vec<usize> = (0..3)
        .map(|_| db.query_with_columns(SELECT_ALL).unwrap().0.len())
        .collect();
    assert_eq!(
        counts,
        [1, 1, 1],
        "query_with_columns must filter on the cold path AND on the plan-cache-hit branch"
    );

    let (rows, columns) = db.query_with_columns(SELECT_ALL).unwrap();
    let shown = format!("{rows:?}");
    assert!(
        shown.contains("alice") && !shown.contains("bob"),
        "query_with_columns leaked a hidden row: {shown}"
    );
    assert_eq!(
        columns,
        ["id", "owner"],
        "the RLS rewrite must not disturb the projected column names"
    );
    assert_eq!(
        count_unfiltered(&db, tenant, SELECT_ALL),
        2,
        "ground truth: both rows are still present"
    );
}

/// The session delegate with no open transaction routes straight into
/// `query_with_columns`, so it inherits the same fix.
#[test]
fn query_with_columns_for_session_autocommit_applies_rls() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);
    let sid = db
        .create_session("rls_autocommit", IsolationLevel::ReadCommitted)
        .unwrap();

    let (rows, _) = db.query_with_columns_for_session(sid, SELECT_ALL).unwrap();
    let shown = format!("{rows:?}");
    assert_eq!(rows.len(), 1, "an autocommit session read must be filtered: {shown}");
    assert!(
        !shown.contains("bob"),
        "an autocommit session read leaked a hidden row: {shown}"
    );
    db.destroy_session(sid).unwrap();
    assert_eq!(count_unfiltered(&db, tenant, SELECT_ALL), 2, "ground truth");
}

// ---------------------------------------------------------------------------
// Hole 4 — correction pin, not a live bug
// ---------------------------------------------------------------------------

/// `try_normalized_query_with_columns` bails when a tenant context is active, so
/// the normalized-plan path never serves an unfiltered result and never caches
/// rows under a raw key on behalf of a context-active reader.
///
/// This is a REGRESSION GUARD, not a reproduction: the guard is one boolean in
/// an `||` chain, exactly the kind a future "simplify this condition" pass
/// deletes without noticing why it is there. If it is ever removed, the
/// rotating-literal reads below start returning rows the policy hides.
#[test]
fn normalized_path_still_defers_under_context() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);

    // Rotating literals: exactly the shape literal normalization exists for.
    // Each amount threshold matches BOTH seeded rows (10 and 20), so only the
    // policy can reduce the result to one.
    for threshold in [1, 2, 3, 4, 5] {
        let sql = format!("SELECT id, owner FROM orders WHERE amount > {threshold}");
        let rows = db.query(&sql, &[]).unwrap();
        let shown = format!("{rows:?}");
        assert_eq!(
            rows.len(),
            1,
            "the normalized-plan path served an unfiltered result for `{sql}`: {shown}"
        );
        assert!(
            !shown.contains("bob"),
            "the normalized-plan path leaked a hidden row for `{sql}`: {shown}"
        );
    }

    // Same texts read with no context must see both rows — proving the loop
    // above did not simply leave a filtered result cached under each raw key.
    db.tenant_manager.clear_current_context();
    for threshold in [1, 2, 3, 4, 5] {
        let sql = format!("SELECT id, owner FROM orders WHERE amount > {threshold}");
        assert_eq!(
            db.query(&sql, &[]).unwrap().len(),
            2,
            "a no-context read of `{sql}` was served the context-active reader's filtered rows"
        );
    }
    set_ctx(&db, tenant);
    assert_eq!(count_unfiltered(&db, tenant, SELECT_ALL), 2, "ground truth");
}

// ---------------------------------------------------------------------------
// Hole 5 — transaction-scoped reads, site by site
// ---------------------------------------------------------------------------

/// 5b: the documented embedded pattern — `let tx = db.begin_transaction()?;
/// tx.query(...)`. `heliosdb-nano-transactions` teaches this as the normal way
/// to group reads and writes, and it was unfiltered on the very first read.
#[test]
fn transaction_query_applies_rls() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);

    let shown = {
        let tx = db.begin_transaction().unwrap();
        let rows = tx.query(SELECT_ALL, &[]).unwrap();
        let shown = format!("{rows:?}");
        assert_eq!(rows.len(), 1, "Transaction::query was unfiltered: {shown}");
        tx.rollback().unwrap();
        shown
    };
    assert!(
        !shown.contains("bob"),
        "Transaction::query leaked a hidden row: {shown}"
    );
    assert_eq!(count_unfiltered(&db, tenant, SELECT_ALL), 2, "ground truth");
}

/// 5b, second half: a read inside the transaction must still see the
/// transaction's OWN writes. The fix routes this through a shared choke point,
/// which must not cost read-your-own-writes.
#[test]
fn transaction_query_still_sees_own_writes_under_rls() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seeded_with_policy(&db);

    let tx = db.begin_transaction().unwrap();
    tx.execute("INSERT INTO orders VALUES (3, 'alice', 30)").unwrap();
    let rows = tx.query(SELECT_ALL, &[]).unwrap();
    let shown = format!("{rows:?}");
    assert_eq!(
        rows.len(),
        2,
        "the transaction must see its own uncommitted, policy-VISIBLE insert alongside the \
         pre-existing visible row: {shown}"
    );
    assert!(
        !shown.contains("bob"),
        "read-your-own-writes must not come at the cost of the filter: {shown}"
    );
    tx.rollback().unwrap();
}

/// 5a: SQL-text `BEGIN` opens the process-global transaction slot; `query()`
/// then takes a separate top-level branch that hand-rolled its own executor.
#[test]
fn query_in_explicit_begin_applies_rls() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);

    db.execute("BEGIN").unwrap();
    let rows = db.query(SELECT_ALL, &[]).unwrap();
    let shown = format!("{rows:?}");
    db.execute("COMMIT").unwrap();

    assert_eq!(rows.len(), 1, "query() inside a SQL-text BEGIN was unfiltered: {shown}");
    assert!(
        !shown.contains("bob"),
        "query() inside BEGIN leaked a hidden row: {shown}"
    );
    assert_eq!(count_unfiltered(&db, tenant, SELECT_ALL), 2, "ground truth");
}

/// 5c: wire simple-query (PG/MySQL) once the session has an open transaction.
#[test]
fn session_query_with_columns_in_txn_applies_rls() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);
    let sid = db.create_session("rls_5c", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(sid).unwrap();

    let (rows, columns) = db.query_with_columns_for_session(sid, SELECT_ALL).unwrap();
    let shown = format!("{rows:?}");
    db.rollback_transaction_for_session(sid).unwrap();
    db.destroy_session(sid).unwrap();

    assert_eq!(
        rows.len(),
        1,
        "query_with_columns_for_session in a session txn was unfiltered: {shown}"
    );
    assert!(!shown.contains("bob"), "leaked a hidden row: {shown}");
    assert_eq!(
        columns,
        ["id", "owner"],
        "the RLS rewrite must not disturb the wire result-set metadata"
    );
    assert_eq!(count_unfiltered(&db, tenant, SELECT_ALL), 2, "ground truth");
}

/// 5d: PG extended/prepared protocol once the session has an open transaction.
/// Fixed "for free" by the choke point — this site already called
/// `query_plan_with_params`, it just never pre-rewrote the plan.
#[test]
fn session_query_params_in_txn_applies_rls() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);
    let sid = db.create_session("rls_5d", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(sid).unwrap();

    let rows = db
        .query_params_for_session(sid, SELECT_ALL_PARAMS, &[Value::Int4(0)])
        .unwrap();
    let shown = format!("{rows:?}");
    db.rollback_transaction_for_session(sid).unwrap();
    db.destroy_session(sid).unwrap();

    assert_eq!(
        rows.len(),
        1,
        "query_params_for_session in a session txn was unfiltered: {shown}"
    );
    assert!(!shown.contains("bob"), "leaked a hidden row: {shown}");
    assert_eq!(count_unfiltered(&db, tenant, SELECT_ALL), 2, "ground truth");
}

/// 5e: MySQL binary `COM_STMT_EXECUTE` once the session has an open transaction.
#[test]
fn session_query_params_with_columns_in_txn_applies_rls() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);
    let sid = db.create_session("rls_5e", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(sid).unwrap();

    let (rows, columns) = db
        .query_params_with_columns_for_session(sid, SELECT_ALL_PARAMS, &[Value::Int4(0)])
        .unwrap();
    let shown = format!("{rows:?}");
    db.rollback_transaction_for_session(sid).unwrap();
    db.destroy_session(sid).unwrap();

    assert_eq!(
        rows.len(),
        1,
        "query_params_with_columns_for_session in a session txn was unfiltered: {shown}"
    );
    assert!(!shown.contains("bob"), "leaked a hidden row: {shown}");
    assert_eq!(
        columns,
        ["id", "owner"],
        "the RLS rewrite must not disturb the wire result-set metadata"
    );
    assert_eq!(count_unfiltered(&db, tenant, SELECT_ALL), 2, "ground truth");
}

/// 5f: the SQL-text `PREPARE … ; EXECUTE …` emulation. Also fixed for free by
/// the choke point.
#[test]
fn prepared_execute_applies_rls() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);

    db.query("PREPARE p_read AS SELECT id, owner FROM orders", &[]).unwrap();
    let rows = db.query("EXECUTE p_read", &[]).unwrap();
    let shown = format!("{rows:?}");
    assert_eq!(rows.len(), 1, "PREPARE/EXECUTE was unfiltered: {shown}");
    assert!(!shown.contains("bob"), "PREPARE/EXECUTE leaked a hidden row: {shown}");

    // The column-aware surface reaches the same arm through a different caller.
    let (rows_wc, _) = db.query_with_columns("EXECUTE p_read").unwrap();
    assert_eq!(
        rows_wc.len(),
        1,
        "the column-aware PREPARE/EXECUTE surface must filter too"
    );

    assert_eq!(count_unfiltered(&db, tenant, SELECT_ALL), 2, "ground truth");
}

/// 5g: the third, older public session-query entry point.
#[test]
fn query_in_session_txn_applies_rls() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = seeded_with_policy(&db);
    let sid = db.create_session("rls_5g", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(sid).unwrap();

    let rows = db.query_in_session(sid, SELECT_ALL, &[]).unwrap();
    let shown = format!("{rows:?}");
    db.rollback_transaction_for_session(sid).unwrap();
    db.destroy_session(sid).unwrap();

    assert_eq!(
        rows.len(),
        1,
        "query_in_session in a session txn was unfiltered: {shown}"
    );
    assert!(!shown.contains("bob"), "leaked a hidden row: {shown}");
    assert_eq!(count_unfiltered(&db, tenant, SELECT_ALL), 2, "ground truth");
}

// ---------------------------------------------------------------------------
// The two must-stay-free paths (read variants of the write suite's pair)
// ---------------------------------------------------------------------------

/// A table with NO policy is untouched even with a tenant context active: every
/// read entry point must return the full result. Guards against the enforcement
/// being too aggressive — and against the cache gates degrading into "a context
/// is active, therefore filter something".
#[test]
fn rls_policy_less_table_unaffected() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_orders(&db);
    // Context active, but no policy anywhere on `orders`.
    activate_tenant(&db);

    for (label, count) in read_counts(&db) {
        assert_eq!(
            count, 2,
            "{label} returned {count} row(s) on a policy-less table under an active context; \
             a table with no policy must never be filtered"
        );
    }
}

/// Policies exist on the table, but no `TenantContext` is ever set: every read
/// entry point must behave exactly as it did before this fix. This is the
/// "no-context path stays free" contract — the gates cost one `RwLock::read()`
/// and then get out of the way.
#[test]
fn rls_no_tenant_context_unaffected() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_orders(&db);
    // A tenant and its policy exist; the context is deliberately never set.
    db.tenant_manager
        .register_tenant("unused".to_string(), IsolationMode::SharedSchema);
    policy(&db, "orders", "read_own", RLSCommand::Select, "owner = 'alice'");

    for (label, count) in read_counts(&db) {
        assert_eq!(
            count, 2,
            "{label} returned {count} row(s) with no tenant context set; without a context \
             there is no policy to apply"
        );
    }
    assert_eq!(count_no_context(&db, SELECT_ALL), 2, "ground truth");
}

/// With no context, the result cache must still WORK — the gates must not have
/// disabled caching outright. Asserted through observable behavior: a warmed
/// entry survives, and a write invalidates it.
#[test]
fn rls_no_tenant_context_result_cache_still_serves() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_orders(&db);

    for _ in 0..3 {
        assert_eq!(db.query(SELECT_ALL, &[]).unwrap().len(), 2);
    }
    // A write must still invalidate the entry the reads above installed.
    db.execute("INSERT INTO orders VALUES (3, 'carol', 30)").unwrap();
    assert_eq!(
        db.query(SELECT_ALL, &[]).unwrap().len(),
        3,
        "the no-context result cache served a stale entry across a write — the R1.1 gates must \
         compose with cache invalidation, not replace it"
    );
}

// ---------------------------------------------------------------------------
// Q4 — fail-closed
// ---------------------------------------------------------------------------

/// If the policy cannot be parsed, a read must ERROR. Never unfiltered rows,
/// never a silently empty result.
///
/// This is the assertion a future "make RLS more robust" change would break: the
/// tempting `.unwrap_or(plan)` / `.ok()` around `apply_rls_to_plan` turns an
/// unparseable policy into "no policy" and serves the whole table. Every entry
/// point is checked, because the fix funnels them through shared helpers and a
/// fallback added in one of those helpers would open all of them at once.
#[test]
fn rls_parse_error_on_read_fails_closed() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed_orders(&db);
    let tenant = activate_tenant(&db);
    // Syntactically invalid: `RLSExpressionEvaluator::parse` wraps this in
    // `SELECT * FROM dummy WHERE …` and sqlparser rejects it, so the `?` in
    // `apply_rls_to_plan_recursive` propagates.
    policy(&db, "orders", "broken", RLSCommand::Select, "owner = = 'alice'");

    let leaked = read_entry_points_that_succeeded(&db);
    assert!(
        leaked.is_empty(),
        "these read entry points returned Ok() under an UNPARSEABLE policy instead of failing \
         closed: {leaked:?} — an RLS expression that cannot be parsed must never degrade to \
         'no policy'"
    );

    // And the rows really are still there: the reads failed, they did not
    // quietly return an empty set from an emptied table.
    assert_eq!(
        count_unfiltered(&db, tenant, SELECT_ALL),
        2,
        "ground truth: fail-closed must not have destroyed or hidden data, only refused to read it"
    );
}
