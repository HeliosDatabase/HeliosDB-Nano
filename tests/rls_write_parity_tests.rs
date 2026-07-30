//! Row-level-security enforcement parity between the two DML executor families.
//!
//! HeliosDB Nano has two parallel DML executors:
//!   * "text family"   — `db.execute()`        → `execute_in_transaction_inner`
//!                        (psql simple-query, MySQL wire, COPY generic fallback)
//!   * "params family" — `db.execute_params()` → `execute_params_inner`
//!                        → `execute_plan_with_params_inner`
//!                        (PG extended protocol: psycopg server-side bind, JDBC,
//!                         sqlx, Drizzle, node-postgres; every `… RETURNING`
//!                         statement; trigger bodies; plus REST/BaaS writes)
//!
//! Until `RlsWriteGuard` landed, **neither** family enforced RLS on any write
//! (`docs/plans/ROADMAP_V5.md` §1.1): `SELECT` was correctly filtered by
//! `apply_rls_to_plan`, while `INSERT` ignored `WITH CHECK` outright and
//! `UPDATE`/`DELETE` ignored `USING` — so a session that could *see* exactly one
//! row could still delete a row it could not see.
//!
//! PostgreSQL's semantics here are asymmetric, and getting either half backwards
//! is a bug (spurious errors on legitimate statements, or silent
//! under-enforcement), so every test below asserts which of the two it is:
//!
//! | command | `USING` (pre-image row)      | `WITH CHECK` (new row) |
//! |---------|-----------------------------|------------------------|
//! | INSERT  | n/a                         | **error** on violation |
//! | UPDATE  | **silent filter**, no error | **error** on violation |
//! | DELETE  | **silent filter**, no error | n/a                    |
//!
//! Every case runs the SAME logical statement through BOTH families and asserts
//! they agree — and that the agreed-on answer is the correct one.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::tenant::{IsolationMode, RLSCommand, TenantContext, TenantId};
use heliosdb_nano::{EmbeddedDatabase, Value};

const ORDERS_DDL: &str = "CREATE TABLE orders (id INT PRIMARY KEY, owner TEXT NOT NULL, amount INT)";
const INSERT_PARAMS: &str = "INSERT INTO orders VALUES ($1, $2, $3)";
const UPSERT_HIDDEN: &str = "INSERT INTO orders VALUES (2, 'bob', 999) ON CONFLICT (id) DO UPDATE SET amount = 999";
const UPSERT_HIDDEN_P: &str = "INSERT INTO orders VALUES (2, 'bob', 999) ON CONFLICT (id) DO UPDATE SET amount = $1";
const UPSERT_OWN: &str = "INSERT INTO orders VALUES (1, 'alice', 555) ON CONFLICT (id) DO UPDATE SET amount = 555";
const UPSERT_OWN_P: &str = "INSERT INTO orders VALUES (1, 'alice', 555) ON CONFLICT (id) DO UPDATE SET amount = $1";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Register an RLS-enabled (`SharedSchema`) tenant and activate its context.
/// `should_apply_rls` needs all three: a context, an existing tenant, and
/// `rls_enabled` on that tenant.
fn activate_tenant(db: &EmbeddedDatabase) -> TenantId {
    let tenant = db
        .tenant_manager
        .register_tenant("rls-parity".to_string(), IsolationMode::SharedSchema);
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
/// is never filtered or rejected: id 1 is alice's, id 2 is bob's.
fn seed_orders(db: &EmbeddedDatabase) {
    db.execute(ORDERS_DDL).unwrap();
    db.execute("INSERT INTO orders VALUES (1, 'alice', 10)").unwrap();
    db.execute("INSERT INTO orders VALUES (2, 'bob', 20)").unwrap();
}

/// Add a policy to `table`. `with_check = None` exercises PG's
/// "no WITH CHECK → reuse this policy's USING" fallback.
fn policy(db: &EmbeddedDatabase, table: &str, name: &str, cmd: RLSCommand, using: &str, with_check: Option<&str>) {
    db.tenant_manager.create_rls_policy(
        table.to_string(),
        name.to_string(),
        format!("test policy {name}"),
        cmd,
        using.to_string(),
        with_check.map(str::to_string),
    );
}

// ---------------------------------------------------------------------------
// Ground-truth reads
// ---------------------------------------------------------------------------

/// Count what is ACTUALLY in the table, not what the policy would show.
///
/// RLS filters `SELECT` as well as writes, so a verification read taken *under*
/// an active context cannot distinguish "the row was deleted" from "the row is
/// hidden from me" — precisely the confusion that let the cross-tenant
/// UPDATE/DELETE tests in `tests/multi_tenancy_integration.rs` print a checkmark
/// while asserting nothing. So: clear the context, read, restore the context.
///
/// Predicates are pushed into the SQL (`… AND amount = 20`) rather than compared
/// as `Value`s, so the assertions do not depend on which integer width the
/// engine happens to store.
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
// INSERT — WITH CHECK errors, never filters
// ---------------------------------------------------------------------------

/// An INSERT whose new row fails `WITH CHECK` must be REJECTED (PG: `new row
/// violates row-level security policy for table "…"`), not silently dropped and
/// not silently accepted. Both families.
#[test]
fn rls_insert_with_check_errors() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        db.execute(ORDERS_DDL).unwrap();
        let tenant = activate_tenant(db);
        policy(
            db,
            "orders",
            "own",
            RLSCommand::All,
            "owner = 'alice'",
            Some("owner = 'alice'"),
        );
        tenant
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_res = db.execute("INSERT INTO orders VALUES (1, 'bob', 10)");
    let text_rows = count_unfiltered(&db, tenant, "SELECT id FROM orders");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let bob = [Value::Int4(1), Value::String("bob".into()), Value::Int4(10)];
    let params_res = db2.execute_params(INSERT_PARAMS, &bob);
    let params_rows = count_unfiltered(&db2, tenant2, "SELECT id FROM orders");

    assert_eq!(
        params_res.is_err(),
        text_res.is_err(),
        "DIVERGENCE: text family {text_res:?}, params family {params_res:?} — both must reject an \
         INSERT whose new row fails the policy's WITH CHECK"
    );
    assert!(
        text_res.is_err(),
        "INSERT violating WITH CHECK must be rejected, not accepted; got {text_res:?}"
    );
    let message = text_res.unwrap_err().to_string();
    assert!(
        message.contains("row-level security"),
        "the error must name the policy — that substring is also what maps the error to SQLSTATE \
         42501 in sqlstate_for_error; got {message}"
    );
    assert_eq!(text_rows, 0, "the rejected INSERT must not have written a row");
    assert_eq!(params_rows, 0, "the rejected INSERT must not have written a row");
}

/// The other direction: a row that DOES satisfy `WITH CHECK` must go in. Guards
/// against the enforcement being too aggressive. Both families.
#[test]
fn rls_insert_with_check_allows_matching_row() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        db.execute(ORDERS_DDL).unwrap();
        let tenant = activate_tenant(db);
        policy(
            db,
            "orders",
            "own",
            RLSCommand::All,
            "owner = 'alice'",
            Some("owner = 'alice'"),
        );
        tenant
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_res = db.execute("INSERT INTO orders VALUES (1, 'alice', 10)");
    let text_rows = count_unfiltered(&db, tenant, "SELECT id FROM orders");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let alice = [Value::Int4(1), Value::String("alice".into()), Value::Int4(10)];
    let params_res = db2.execute_params(INSERT_PARAMS, &alice);
    let params_rows = count_unfiltered(&db2, tenant2, "SELECT id FROM orders");

    assert!(
        text_res.is_ok(),
        "a conforming INSERT must still succeed — RLS enforcement must not over-reject; got {text_res:?}"
    );
    assert!(
        params_res.is_ok(),
        "a conforming INSERT must still succeed — RLS enforcement must not over-reject; got {params_res:?}"
    );
    assert_eq!(text_rows, 1, "the conforming INSERT must have written its row");
    assert_eq!(
        params_rows, text_rows,
        "DIVERGENCE: text family left {text_rows} row(s), params family left {params_rows}"
    );
}

/// PG: a policy that declares no `WITH CHECK` reuses its own `USING` as the
/// check. `get_rls_conditions` used to return `with_check_expr` verbatim, so
/// `None` stayed `None` and the new row was never validated at all. Both families.
#[test]
fn rls_insert_no_with_check_falls_back_to_using() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        db.execute(ORDERS_DDL).unwrap();
        let tenant = activate_tenant(db);
        // USING only — no explicit WITH CHECK.
        policy(db, "orders", "using_only", RLSCommand::All, "owner = 'alice'", None);
        tenant
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_res = db.execute("INSERT INTO orders VALUES (1, 'bob', 10)");
    let text_rows = count_unfiltered(&db, tenant, "SELECT id FROM orders");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let bob = [Value::Int4(1), Value::String("bob".into()), Value::Int4(10)];
    let params_res = db2.execute_params(INSERT_PARAMS, &bob);
    let params_rows = count_unfiltered(&db2, tenant2, "SELECT id FROM orders");

    assert!(
        text_res.is_err(),
        "a USING-only policy must still reject a non-conforming INSERT (PG falls back to USING); \
         got {text_res:?}"
    );
    assert!(
        params_res.is_err(),
        "a USING-only policy must still reject a non-conforming INSERT (PG falls back to USING); \
         got {params_res:?}"
    );
    assert_eq!(text_rows, 0, "the rejected INSERT must not have written a row");
    assert_eq!(params_rows, 0, "the rejected INSERT must not have written a row");
}

// ---------------------------------------------------------------------------
// UPDATE — USING filters silently, WITH CHECK errors
// ---------------------------------------------------------------------------

/// The case most likely to get the filter-vs-error distinction backwards: an
/// UPDATE aimed at a row this session cannot see is a **no-op**, not an error.
/// Asserted in both directions — `is_ok()` AND zero rows changed AND the target
/// row's value untouched.
#[test]
fn rls_update_using_silently_filters() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        seed_orders(db);
        let tenant = activate_tenant(db);
        policy(db, "orders", "upd_own", RLSCommand::Update, "owner = 'alice'", None);
        tenant
    }
    const UNTOUCHED: &str = "SELECT id FROM orders WHERE id = 2 AND amount = 20";

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_res = db.execute("UPDATE orders SET amount = 999 WHERE id = 2");
    let text_untouched = count_unfiltered(&db, tenant, UNTOUCHED);

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let params_res = db2.execute_params("UPDATE orders SET amount = 999 WHERE id = $1", &[Value::Int4(2)]);
    let params_untouched = count_unfiltered(&db2, tenant2, UNTOUCHED);

    assert!(
        text_res.is_ok(),
        "RLS USING FILTERS on UPDATE — it must not raise an error, it must report 0 rows; \
         got {text_res:?}"
    );
    assert!(
        params_res.is_ok(),
        "RLS USING FILTERS on UPDATE — it must not raise an error, it must report 0 rows; \
         got {params_res:?}"
    );
    assert_eq!(
        text_res.ok(),
        Some(0),
        "another tenant's row must not be counted as updated by the text family"
    );
    assert_eq!(
        params_res.ok(),
        Some(0),
        "another tenant's row must not be counted as updated by the params family"
    );
    assert_eq!(
        text_untouched, 1,
        "the invisible row must still hold amount=20 after the text-family UPDATE"
    );
    assert_eq!(
        params_untouched, 1,
        "the invisible row must still hold amount=20 after the params-family UPDATE"
    );
}

/// Two halves of the same policy: (a) the session's OWN row updates fine, and
/// (b) an assignment that would move that row out of the policy's reach is an
/// ERROR via `WITH CHECK` — the case the dead `execute_internal` UPDATE arm never
/// checked at all (it destructured `(using_expr, _)` and dropped the check).
#[test]
fn rls_update_using_allows_own_row_then_with_check_errors_on_bad_assignment() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        seed_orders(db);
        let tenant = activate_tenant(db);
        policy(
            db,
            "orders",
            "upd_own",
            RLSCommand::Update,
            "owner = 'alice'",
            Some("owner = 'alice'"),
        );
        tenant
    }
    const APPLIED: &str = "SELECT id FROM orders WHERE id = 1 AND amount = 111";
    const STILL_ALICE: &str = "SELECT id FROM orders WHERE id = 1 AND owner = 'alice'";

    // (a) the session's own row
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_ok = db.execute("UPDATE orders SET amount = 111 WHERE id = 1");
    let text_applied = count_unfiltered(&db, tenant, APPLIED);

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let params_ok = db2.execute_params("UPDATE orders SET amount = 111 WHERE id = $1", &[Value::Int4(1)]);
    let params_applied = count_unfiltered(&db2, tenant2, APPLIED);

    assert_eq!(
        text_ok.ok(),
        Some(1),
        "the session's own row must still be updatable through the text family"
    );
    assert_eq!(
        params_ok.ok(),
        Some(1),
        "the session's own row must still be updatable through the params family"
    );
    assert_eq!(
        text_applied, 1,
        "the own-row UPDATE must have taken effect (text family)"
    );
    assert_eq!(
        params_applied, 1,
        "the own-row UPDATE must have taken effect (params family)"
    );

    // (b) same row, assignment that violates WITH CHECK on the NEW row
    let text_bad = db.execute("UPDATE orders SET owner = 'bob' WHERE id = 1");
    let text_kept = count_unfiltered(&db, tenant, STILL_ALICE);

    let bob_row = [Value::String("bob".into()), Value::Int4(1)];
    let params_bad = db2.execute_params("UPDATE orders SET owner = $1 WHERE id = $2", &bob_row);
    let params_kept = count_unfiltered(&db2, tenant2, STILL_ALICE);

    assert!(
        text_bad.is_err(),
        "moving a row out of its own policy's reach must ERROR via WITH CHECK, not silently \
         succeed and not silently no-op; got {text_bad:?}"
    );
    assert!(
        params_bad.is_err(),
        "moving a row out of its own policy's reach must ERROR via WITH CHECK, not silently \
         succeed and not silently no-op; got {params_bad:?}"
    );
    assert_eq!(
        text_kept, 1,
        "the rejected UPDATE must have left the row owned by alice (text family)"
    );
    assert_eq!(
        params_kept, 1,
        "the rejected UPDATE must have left the row owned by alice (params family)"
    );
}

// ---------------------------------------------------------------------------
// DELETE — USING filters silently, no WITH CHECK
// ---------------------------------------------------------------------------

/// The live-verified vulnerability: a session that could see exactly one row
/// deleted a row it could not see. A DELETE against an invisible row is a no-op,
/// NOT an error.
#[test]
fn rls_delete_using_silently_filters() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        seed_orders(db);
        let tenant = activate_tenant(db);
        policy(db, "orders", "del_own", RLSCommand::Delete, "owner = 'alice'", None);
        tenant
    }
    const OTHER_ROW: &str = "SELECT id FROM orders WHERE id = 2";

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_res = db.execute("DELETE FROM orders WHERE id = 2");
    let text_left = count_unfiltered(&db, tenant, OTHER_ROW);

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let params_res = db2.execute_params("DELETE FROM orders WHERE id = $1", &[Value::Int4(2)]);
    let params_left = count_unfiltered(&db2, tenant2, OTHER_ROW);

    assert!(
        text_res.is_ok(),
        "RLS USING FILTERS on DELETE — it must not raise an error, it must report 0 rows; \
         got {text_res:?}"
    );
    assert!(
        params_res.is_ok(),
        "RLS USING FILTERS on DELETE — it must not raise an error, it must report 0 rows; \
         got {params_res:?}"
    );
    assert_eq!(
        text_res.ok(),
        Some(0),
        "another tenant's row must not be counted as deleted by the text family"
    );
    assert_eq!(
        params_res.ok(),
        Some(0),
        "another tenant's row must not be counted as deleted by the params family"
    );
    assert_eq!(
        text_left, 1,
        "another tenant's row must survive the text-family DELETE — this is the exact bypass"
    );
    assert_eq!(
        params_left, 1,
        "another tenant's row must survive the params-family DELETE — this is the exact bypass"
    );
}

/// And the session's own row must still be deletable.
#[test]
fn rls_delete_using_allows_own_row() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        seed_orders(db);
        let tenant = activate_tenant(db);
        policy(db, "orders", "del_own", RLSCommand::Delete, "owner = 'alice'", None);
        tenant
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_res = db.execute("DELETE FROM orders WHERE id = 1");
    let text_left = count_unfiltered(&db, tenant, "SELECT id FROM orders");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let params_res = db2.execute_params("DELETE FROM orders WHERE id = $1", &[Value::Int4(1)]);
    let params_left = count_unfiltered(&db2, tenant2, "SELECT id FROM orders");

    assert_eq!(
        text_res.ok(),
        Some(1),
        "the session's own row must still be deletable through the text family"
    );
    assert_eq!(
        params_res.ok(),
        Some(1),
        "the session's own row must still be deletable through the params family"
    );
    assert_eq!(text_left, 1, "only the other tenant's row may remain (text family)");
    assert_eq!(
        params_left, text_left,
        "DIVERGENCE: text family left {text_left} row(s), params family left {params_left}"
    );
}

// ---------------------------------------------------------------------------
// Policy combination
// ---------------------------------------------------------------------------

/// PG combines multiple permissive policies with OR. `get_rls_conditions` used
/// to take `applicable_policies.first()` and drop the rest, so whichever policy
/// happened to sit first in insertion order was the only one enforced and the
/// second policy's rows were silently NOT writable. Both families.
#[test]
fn rls_multi_policy_combines_with_or() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        seed_orders(db);
        db.execute("INSERT INTO orders VALUES (3, 'carol', 30)").unwrap();
        let tenant = activate_tenant(db);
        policy(db, "orders", "upd_alice", RLSCommand::Update, "owner = 'alice'", None);
        policy(db, "orders", "upd_bob", RLSCommand::Update, "owner = 'bob'", None);
        tenant
    }
    const CAROL_UNTOUCHED: &str = "SELECT id FROM orders WHERE id = 3 AND amount = 30";

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_first = db.execute("UPDATE orders SET amount = 101 WHERE id = 1");
    let text_second = db.execute("UPDATE orders SET amount = 102 WHERE id = 2");
    let text_neither = db.execute("UPDATE orders SET amount = 103 WHERE id = 3");
    let text_carol = count_unfiltered(&db, tenant, CAROL_UNTOUCHED);

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let sql = "UPDATE orders SET amount = 104 WHERE id = $1";
    let params_first = db2.execute_params(sql, &[Value::Int4(1)]);
    let params_second = db2.execute_params(sql, &[Value::Int4(2)]);
    let params_neither = db2.execute_params(sql, &[Value::Int4(3)]);
    let params_carol = count_unfiltered(&db2, tenant2, CAROL_UNTOUCHED);

    assert_eq!(
        text_first.ok(),
        Some(1),
        "a row matching the FIRST policy must be updatable (text family)"
    );
    assert_eq!(
        text_second.ok(),
        Some(1),
        "a row matching the SECOND policy must be updatable too — permissive policies OR together"
    );
    assert_eq!(
        text_neither.ok(),
        Some(0),
        "a row matching NEITHER policy must be filtered, silently, with no error"
    );
    assert_eq!(
        params_first.ok(),
        Some(1),
        "a row matching the FIRST policy must be updatable (params family)"
    );
    assert_eq!(
        params_second.ok(),
        Some(1),
        "a row matching the SECOND policy must be updatable through the params family too"
    );
    assert_eq!(
        params_neither.ok(),
        Some(0),
        "a row matching NEITHER policy must be filtered by the params family too"
    );
    assert_eq!(
        text_carol, 1,
        "the unmatched row's value must be untouched (text family)"
    );
    assert_eq!(
        params_carol, 1,
        "the unmatched row's value must be untouched (params family)"
    );
}

// ---------------------------------------------------------------------------
// The two must-stay-free paths
// ---------------------------------------------------------------------------

/// A table with NO policy is untouched even with a tenant context active: the
/// DML fast paths must keep bailing exactly as before (they gate on the bare
/// presence of a context, then on `should_apply_rls`), and the generic arms must
/// build no guard. Asserted on observable row counts — which fast path ran is
/// only visible through `tracing` and is not assertable here.
#[test]
fn rls_policy_less_table_unaffected() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        seed_orders(db);
        // Context active, but no policy anywhere on `orders`.
        activate_tenant(db)
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_insert = db.execute("INSERT INTO orders VALUES (3, 'carol', 30)");
    let text_update = db.execute("UPDATE orders SET amount = 999 WHERE id = 2");
    let text_delete = db.execute("DELETE FROM orders WHERE id = 1");
    let text_left = count_unfiltered(&db, tenant, "SELECT id FROM orders");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let carol = [Value::Int4(3), Value::String("carol".into()), Value::Int4(30)];
    let params_insert = db2.execute_params(INSERT_PARAMS, &carol);
    let params_update = db2.execute_params("UPDATE orders SET amount = 999 WHERE id = $1", &[Value::Int4(2)]);
    let params_delete = db2.execute_params("DELETE FROM orders WHERE id = $1", &[Value::Int4(1)]);
    let params_left = count_unfiltered(&db2, tenant2, "SELECT id FROM orders");

    assert_eq!(
        text_insert.ok(),
        Some(1),
        "a policy-less table must accept INSERT unchanged (text family)"
    );
    assert_eq!(
        text_update.ok(),
        Some(1),
        "a policy-less table must accept UPDATE unchanged (text family)"
    );
    assert_eq!(
        text_delete.ok(),
        Some(1),
        "a policy-less table must accept DELETE unchanged (text family)"
    );
    assert_eq!(
        params_insert.ok(),
        Some(1),
        "a policy-less table must accept INSERT unchanged (params family)"
    );
    assert_eq!(
        params_update.ok(),
        Some(1),
        "a policy-less table must accept UPDATE unchanged (params family)"
    );
    assert_eq!(
        params_delete.ok(),
        Some(1),
        "a policy-less table must accept DELETE unchanged (params family)"
    );
    assert_eq!(text_left, 2, "the INSERT and the DELETE must both have landed");
    assert_eq!(
        params_left, text_left,
        "DIVERGENCE: text family left {text_left} row(s), params family left {params_left}"
    );
}

/// Policies exist on the table, but no `TenantContext` is ever set: every
/// statement must behave exactly as it did before RLS write enforcement existed.
/// This is the "no-context path stays free" contract — `build_rls_write_guard`
/// returns `Ok(None)` after one `get_current_context()` probe, the same probe
/// `apply_rls_to_plan` already pays on the read path. The guard is private, so
/// this asserts the observable behavior rather than inventing a test hook into
/// the executor.
#[test]
fn rls_no_tenant_context_unaffected() {
    fn setup(db: &EmbeddedDatabase) {
        seed_orders(db);
        // A tenant and its policies exist; the context is deliberately never set.
        db.tenant_manager
            .register_tenant("unused".to_string(), IsolationMode::SharedSchema);
        policy(
            db,
            "orders",
            "own",
            RLSCommand::All,
            "owner = 'alice'",
            Some("owner = 'alice'"),
        );
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db);
    let text_insert = db.execute("INSERT INTO orders VALUES (3, 'carol', 30)");
    let text_update = db.execute("UPDATE orders SET amount = 999 WHERE id = 2");
    let text_delete = db.execute("DELETE FROM orders WHERE id = 2");
    let text_left = count_no_context(&db, "SELECT id FROM orders");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    setup(&db2);
    let carol = [Value::Int4(3), Value::String("carol".into()), Value::Int4(30)];
    let params_insert = db2.execute_params(INSERT_PARAMS, &carol);
    let params_update = db2.execute_params("UPDATE orders SET amount = 999 WHERE id = $1", &[Value::Int4(2)]);
    let params_delete = db2.execute_params("DELETE FROM orders WHERE id = $1", &[Value::Int4(2)]);
    let params_left = count_no_context(&db2, "SELECT id FROM orders");

    assert_eq!(
        text_insert.ok(),
        Some(1),
        "no tenant context means no WITH CHECK — a 'carol' row must insert (text family)"
    );
    assert_eq!(
        text_update.ok(),
        Some(1),
        "no tenant context means no USING filter — bob's row must update (text family)"
    );
    assert_eq!(
        text_delete.ok(),
        Some(1),
        "no tenant context means no USING filter — bob's row must delete (text family)"
    );
    assert_eq!(
        params_insert.ok(),
        Some(1),
        "no tenant context means no WITH CHECK — a 'carol' row must insert (params family)"
    );
    assert_eq!(
        params_update.ok(),
        Some(1),
        "no tenant context means no USING filter — bob's row must update (params family)"
    );
    assert_eq!(
        params_delete.ok(),
        Some(1),
        "no tenant context means no USING filter — bob's row must delete (params family)"
    );
    assert_eq!(text_left, 2, "carol inserted, bob deleted, alice untouched");
    assert_eq!(
        params_left, text_left,
        "DIVERGENCE: text family left {text_left} row(s), params family left {params_left}"
    );
}

// ---------------------------------------------------------------------------
// RETURNING
// ---------------------------------------------------------------------------

/// `RETURNING` must not hand back rows the policy hides.
///
/// Both surfaces below land in the PARAMS family: `execute_returning(sql)` is
/// `execute_params_returning(sql, &[])`, and the text family's `Update`/`Delete`
/// arms discard their local `returned_tuples` outright (`let _ =
/// returned_tuples; // RETURNING clause results handled separately`). So this is
/// the one and only RETURNING implementation, exercised twice — once without and
/// once with bound parameters.
#[test]
fn rls_returning_does_not_leak() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        seed_orders(db);
        let tenant = activate_tenant(db);
        policy(
            db,
            "orders",
            "own",
            RLSCommand::All,
            "owner = 'alice'",
            Some("owner = 'alice'"),
        );
        tenant
    }
    // No WHERE at all: the statement sweeps BOTH tenants' rows, so only the
    // policy can keep the hidden one out of the result set.
    const UPD: &str = "UPDATE orders SET amount = 111 RETURNING id, owner";
    const UPD_PARAMS: &str = "UPDATE orders SET amount = $1 RETURNING id, owner";
    // NB the `WHERE amount > 0` is load-bearing and matches every row: a bare
    // `DELETE FROM t RETURNING cols` does not parse — sqlparser consumes
    // `RETURNING` as a table alias and then rejects the column list. Filed
    // separately; unrelated to RLS. Every passing RETURNING test in this repo
    // has a WHERE clause, which is why the gap went unnoticed.
    const DEL: &str = "DELETE FROM orders WHERE amount > 0 RETURNING id, owner";
    const HIDDEN_INTACT: &str = "SELECT id FROM orders WHERE id = 2 AND amount = 20";

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let (text_count, text_rows) = db.execute_returning(UPD).unwrap();
    let text_shown = format!("{text_rows:?}");
    let text_hidden = count_unfiltered(&db, tenant, HIDDEN_INTACT);

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let (params_count, params_rows) = db2.execute_params_returning(UPD_PARAMS, &[Value::Int4(111)]).unwrap();
    let params_shown = format!("{params_rows:?}");
    let params_hidden = count_unfiltered(&db2, tenant2, HIDDEN_INTACT);

    assert_eq!(text_count, 1, "only the visible row may be updated (no-params surface)");
    assert_eq!(params_count, 1, "only the visible row may be updated (params surface)");
    assert_eq!(
        text_rows.len(),
        1,
        "UPDATE ... RETURNING leaked a hidden row (no-params surface): {text_shown}"
    );
    assert_eq!(
        params_rows.len(),
        1,
        "UPDATE ... RETURNING leaked a hidden row (params surface): {params_shown}"
    );
    assert!(
        text_shown.contains("alice") && !text_shown.contains("bob"),
        "RETURNING must project the visible row and only it (no-params surface): {text_shown}"
    );
    assert!(
        params_shown.contains("alice") && !params_shown.contains("bob"),
        "RETURNING must project the visible row and only it (params surface): {params_shown}"
    );
    assert_eq!(
        text_hidden, 1,
        "the hidden row must not have been updated (no-params surface)"
    );
    assert_eq!(
        params_hidden, 1,
        "the hidden row must not have been updated (params surface)"
    );

    // DELETE ... RETURNING, same shape.
    let (text_del_count, text_del_rows) = db.execute_returning(DEL).unwrap();
    let text_del_shown = format!("{text_del_rows:?}");
    let (params_del_count, params_del_rows) = db2.execute_params_returning(DEL, &[]).unwrap();
    let params_del_shown = format!("{params_del_rows:?}");

    assert_eq!(
        text_del_count, 1,
        "only the visible row may be deleted (no-params surface)"
    );
    assert_eq!(
        params_del_count, 1,
        "only the visible row may be deleted (params surface)"
    );
    assert!(
        text_del_rows.len() == 1 && text_del_shown.contains("alice") && !text_del_shown.contains("bob"),
        "DELETE ... RETURNING leaked a hidden row (no-params surface): {text_del_shown}"
    );
    assert!(
        params_del_rows.len() == 1 && params_del_shown.contains("alice") && !params_del_shown.contains("bob"),
        "DELETE ... RETURNING leaked a hidden row (params surface): {params_del_shown}"
    );
    assert_eq!(
        count_unfiltered(&db, tenant, "SELECT id FROM orders WHERE id = 2"),
        1,
        "the hidden row must survive a DELETE that returned only the visible row"
    );
    assert_eq!(
        count_unfiltered(&db2, tenant2, "SELECT id FROM orders WHERE id = 2"),
        1,
        "the hidden row must survive a DELETE that returned only the visible row"
    );
}

// ---------------------------------------------------------------------------
// INSERT ... SELECT
// ---------------------------------------------------------------------------

/// `INSERT INTO dst SELECT … FROM src` used to run its source plan through a
/// bare `sql::Executor` with no `apply_rls_to_plan` call, so an RLS-protected
/// source table could be drained wholesale through a write statement. Both
/// families had the same defect and now share the same fix.
#[test]
fn rls_insert_select_source_is_filtered() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        db.execute("CREATE TABLE src (id INT PRIMARY KEY, owner TEXT NOT NULL)")
            .unwrap();
        db.execute("CREATE TABLE dst (id INT PRIMARY KEY, owner TEXT NOT NULL)")
            .unwrap();
        db.execute("INSERT INTO src VALUES (1, 'alice')").unwrap();
        db.execute("INSERT INTO src VALUES (2, 'bob')").unwrap();
        let tenant = activate_tenant(db);
        // SELECT-only policy on the SOURCE; the target has none.
        policy(db, "src", "read_own", RLSCommand::Select, "owner = 'alice'", None);
        tenant
    }
    const COPY_SQL: &str = "INSERT INTO dst SELECT id, owner FROM src";
    const LEAKED: &str = "SELECT id FROM dst WHERE owner = 'bob'";

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_res = db.execute(COPY_SQL);
    let text_copied = count_unfiltered(&db, tenant, "SELECT id FROM dst");
    let text_leaked = count_unfiltered(&db, tenant, LEAKED);

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let params_res = db2.execute_params(COPY_SQL, &[]);
    let params_copied = count_unfiltered(&db2, tenant2, "SELECT id FROM dst");
    let params_leaked = count_unfiltered(&db2, tenant2, LEAKED);

    assert!(text_res.is_ok(), "the copy itself must succeed; got {text_res:?}");
    assert!(params_res.is_ok(), "the copy itself must succeed; got {params_res:?}");
    assert_eq!(
        text_leaked, 0,
        "INSERT ... SELECT copied a source row hidden from this session (text family)"
    );
    assert_eq!(
        params_leaked, 0,
        "INSERT ... SELECT copied a source row hidden from this session (params family)"
    );
    assert_eq!(text_copied, 1, "the one visible source row must still be copied");
    assert_eq!(
        params_copied, text_copied,
        "DIVERGENCE: text family copied {text_copied} row(s), params family copied {params_copied}"
    );
}

/// A visible source row that would violate the TARGET's `WITH CHECK` must be
/// rejected. Asserted as "the violating row is not in the target" rather than
/// "the target is unchanged": this arm writes each row straight to storage
/// (`insert_tuple_branch_aware_with_schema`) instead of staging it in the
/// statement's transaction, so rows accepted before the failing one survive the
/// abort — a pre-existing atomicity gap of `INSERT ... SELECT`, unrelated to RLS
/// and deliberately not asserted away here.
#[test]
fn rls_insert_select_target_with_check_errors() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        db.execute("CREATE TABLE src (id INT PRIMARY KEY, owner TEXT NOT NULL)")
            .unwrap();
        db.execute("CREATE TABLE dst (id INT PRIMARY KEY, owner TEXT NOT NULL)")
            .unwrap();
        db.execute("INSERT INTO src VALUES (1, 'alice')").unwrap();
        db.execute("INSERT INTO src VALUES (2, 'bob')").unwrap();
        let tenant = activate_tenant(db);
        // Policy on the TARGET only, so every source row stays readable.
        policy(
            db,
            "dst",
            "write_own",
            RLSCommand::Insert,
            "owner = 'alice'",
            Some("owner = 'alice'"),
        );
        tenant
    }
    const COPY_SQL: &str = "INSERT INTO dst SELECT id, owner FROM src";
    const VIOLATING: &str = "SELECT id FROM dst WHERE owner = 'bob'";

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_res = db.execute(COPY_SQL);
    let text_bad = count_unfiltered(&db, tenant, VIOLATING);

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let params_res = db2.execute_params(COPY_SQL, &[]);
    let params_bad = count_unfiltered(&db2, tenant2, VIOLATING);

    assert!(
        text_res.is_err(),
        "a source row violating the target's WITH CHECK must abort the statement; got {text_res:?}"
    );
    assert!(
        params_res.is_err(),
        "a source row violating the target's WITH CHECK must abort the statement; got {params_res:?}"
    );
    assert_eq!(
        text_bad, 0,
        "the violating row must not have been written (text family)"
    );
    assert_eq!(
        params_bad, 0,
        "the violating row must not have been written (params family)"
    );
}

// ---------------------------------------------------------------------------
// ON CONFLICT DO UPDATE
// ---------------------------------------------------------------------------

/// `INSERT ... ON CONFLICT (id) DO UPDATE` mutates an EXISTING row, so it is
/// governed by the UPDATE policy (its `USING` on the pre-image, its `WITH CHECK`
/// on the post-image), not by the INSERT policy.
///
/// The conflicting-row-invisible sub-case pins the behavior chosen in the design
/// spec's flagged OPEN QUESTION: treat it like an ordinary UPDATE's `USING`
/// filter and skip the row silently. That choice was NOT verified against a real
/// PostgreSQL server (the other candidate is an error). Both candidates are
/// equally safe — neither writes — so if upstream PG turns out to raise an error,
/// only this assertion changes, never the enforcement.
#[test]
fn rls_on_conflict_do_update_enforces_update_policy() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        seed_orders(db);
        let tenant = activate_tenant(db);
        policy(
            db,
            "orders",
            "own",
            RLSCommand::All,
            "owner = 'alice'",
            Some("owner = 'alice'"),
        );
        tenant
    }
    const HIDDEN_INTACT: &str = "SELECT id FROM orders WHERE id = 2 AND amount = 20";
    const OWN_APPLIED: &str = "SELECT id FROM orders WHERE id = 1 AND amount = 555";

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_hidden = db.execute(UPSERT_HIDDEN);
    let text_intact = count_unfiltered(&db, tenant, HIDDEN_INTACT);
    let text_own = db.execute(UPSERT_OWN);
    let text_applied = count_unfiltered(&db, tenant, OWN_APPLIED);

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let params_hidden = db2.execute_params(UPSERT_HIDDEN_P, &[Value::Int4(999)]);
    let params_intact = count_unfiltered(&db2, tenant2, HIDDEN_INTACT);
    let params_own = db2.execute_params(UPSERT_OWN_P, &[Value::Int4(555)]);
    let params_applied = count_unfiltered(&db2, tenant2, OWN_APPLIED);

    assert_eq!(
        text_hidden.ok(),
        Some(0),
        "an upsert must not update a row this session cannot see (text family)"
    );
    assert_eq!(
        params_hidden.ok(),
        Some(0),
        "an upsert must not update a row this session cannot see (params family)"
    );
    assert_eq!(
        text_intact, 1,
        "the hidden row must still hold amount=20 after the text-family upsert"
    );
    assert_eq!(
        params_intact, 1,
        "the hidden row must still hold amount=20 after the params-family upsert"
    );
    assert_eq!(
        text_own.ok(),
        Some(1),
        "an upsert over the session's OWN row must still work (text family)"
    );
    assert_eq!(
        params_own.ok(),
        Some(1),
        "an upsert over the session's OWN row must still work (params family)"
    );
    assert_eq!(
        text_applied, 1,
        "the own-row upsert must have taken effect (text family)"
    );
    assert_eq!(
        params_applied, 1,
        "the own-row upsert must have taken effect (params family)"
    );
}

// ---------------------------------------------------------------------------
// Cascades — the one case that must NOT be filtered
// ---------------------------------------------------------------------------

/// `ON DELETE CASCADE` is referential-integrity enforcement, not user DML: it
/// must remove the child row even when the deleting session's policy hides it,
/// exactly as PostgreSQL does. Filtering the cascade would leave a dangling FK
/// reference — PG resolves that conflict in favour of referential integrity, and
/// so do we (design decision recorded in the write-path RLS spec §4.5).
///
/// This is the one test in this file that asserts RLS is *not* applied.
#[test]
fn rls_cascade_delete_bypasses_child_rls() {
    fn setup(db: &EmbeddedDatabase) -> TenantId {
        db.execute("CREATE TABLE parents (id INT PRIMARY KEY)").unwrap();
        db.execute(
            "CREATE TABLE children (id INT PRIMARY KEY, p_id INT REFERENCES parents(id) \
             ON DELETE CASCADE, owner TEXT NOT NULL)",
        )
        .unwrap();
        db.execute("INSERT INTO parents VALUES (1)").unwrap();
        db.execute("INSERT INTO children VALUES (10, 1, 'bob')").unwrap();
        let tenant = activate_tenant(db);
        // The child row is invisible to this session; the parent has no policy.
        policy(
            db,
            "children",
            "child_own",
            RLSCommand::All,
            "owner = 'alice'",
            Some("owner = 'alice'"),
        );
        tenant
    }

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant = setup(&db);
    let text_res = db.execute("DELETE FROM parents WHERE id = 1");
    let text_children = count_unfiltered(&db, tenant, "SELECT id FROM children");

    let db2 = EmbeddedDatabase::new_in_memory().unwrap();
    let tenant2 = setup(&db2);
    let params_res = db2.execute_params("DELETE FROM parents WHERE id = $1", &[Value::Int4(1)]);
    let params_children = count_unfiltered(&db2, tenant2, "SELECT id FROM children");

    assert_eq!(
        text_res.ok(),
        Some(1),
        "the parent delete itself must succeed (parents has no policy)"
    );
    assert_eq!(
        params_res.ok(),
        Some(1),
        "the parent delete itself must succeed through the params family too"
    );
    assert_eq!(
        text_children, 0,
        "CASCADE must still remove the child row the policy hides, or the FK guarantee breaks \
         and an orphan survives (text family)"
    );
    assert_eq!(
        params_children, text_children,
        "DIVERGENCE: text family left {text_children} child row(s), params family left {params_children}"
    );
}
