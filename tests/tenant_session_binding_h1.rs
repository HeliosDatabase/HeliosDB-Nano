//! Batch H1 — **the tenant belongs to the CONNECTION** (sprinter d03de7fc3b22).
//!
//! # The defect
//!
//! `TenantManager::current_context` is ONE `RwLock<Option<TenantContext>>`
//! shared by every connection and every thread in the process. Fifty-two call
//! sites resolved a tenant through it — every RLS gate, every
//! `current_tenant()` evaluation, and the `max_qps` meter — and its only
//! production writer was the REPL's `\tenant use`. **No wire handler set it at
//! all.** Two consequences, both shipped:
//!
//! * On every wire path the answer was always "no tenant", so a tenant's
//!   `max_qps` was never charged and its RLS policies never gated anything,
//!   while the tenant admin surface displayed both as configured and in force.
//! * On the paths that DID set it — embedded callers, the REPL — it was ONE
//!   value for every concurrent connection, so enabling metering charged a
//!   connection's statements to whichever tenant somebody else last selected.
//!
//! That second consequence is why sprinter d03de7fc3b22's eighteen charge sites
//! were written, tested, and then REMOVED again in v4.39.0: enforcement that is
//! WRONG is worse than enforcement that is merely absent. The item was parked on
//! one blocker — a `SessionId -> TenantId` binding.
//!
//! # The keystone
//!
//! Nothing had to be invented. `EmbeddedDatabase::database_name_is_valid`
//! already resolves a requested database name against TENANT NAMES, and sprinter
//! c5afe5e41eac already moved that resolution to just AFTER authentication in
//! the PostgreSQL startup handler. So "database name == tenant name" is this
//! server's shipped definition, and the point at which a connection's tenant
//! becomes knowable already existed and already ran. The binding records the
//! answer that rule produces, onto `SessionScopedState` — the established home
//! for per-connection state (`backend_pid`, `application_name`, `lastval`, the
//! GUC overlay).
//!
//! # What these tests pin that the pre-fix tree fails
//!
//! The load-bearing cases are the ones about SEPARATION, not the happy paths:
//!
//! * [`two_connections_are_metered_against_their_own_tenants`] — one tenant's
//!   spent budget must not refuse another tenant's connection. This is the exact
//!   failure that caused the v4.39.0 back-out.
//! * [`a_session_binding_shadows_the_process_global`] — a bound connection is
//!   charged to ITS tenant even while an embedded caller in the same process has
//!   selected a different one.
//! * [`rls_gates_a_wire_connection_bound_to_an_rls_enabled_tenant`] — the
//!   highest-risk consequence, stated as a test: making the context
//!   per-connection changes which ROWS a connection sees.
//! * [`a_reserved_database_name_binds_no_tenant`] and
//!   [`the_embedded_api_still_resolves_the_process_global`] — the fallback layer.
//!   Deleting it would break the embedded library API and the REPL, which have
//!   no session and no database name and therefore nowhere else to put a tenant.
//! * [`max_connections_refuses_the_connection_that_exceeds_it`] — `max_connections`
//!   had NO enforcement path whatsoever: `check_quota(_, "connections")`'s only
//!   caller was `TenantManager::add_connection`, and `add_connection` /
//!   `remove_connection` had zero production callers anywhere in the tree.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::tenant::{IsolationMode, RLSCommand, ResourceLimits, TenantContext, TenantId};
use heliosdb_nano::{EmbeddedDatabase, Value};
use tokio_postgres::{Client, NoTls};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A live PG-wire server over an in-memory database, plus the handle the tests
/// drive it with. Held together so the listener task is aborted on drop.
struct Server {
    db: Arc<EmbeddedDatabase>,
    port: u16,
    listener: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

async fn serve() -> Server {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let listener = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    Server {
        db,
        port: addr.port(),
        listener,
    }
}

/// Connect as `user`, asking for database `dbname`. The connection task is
/// returned so the caller can keep it alive for as long as the client.
async fn connect(
    port: u16,
    dbname: &str,
) -> std::result::Result<(Client, tokio::task::JoinHandle<()>), tokio_postgres::Error> {
    let cs = format!("host=127.0.0.1 port={port} user=h1 dbname={dbname}");
    let (client, conn) = tokio_postgres::connect(&cs, NoTls).await?;
    let task = tokio::spawn(async move { conn.await.map(|_| ()).unwrap_or(()) });
    Ok((client, task))
}

/// Register `name` as a tenant with an explicit budget, and freeze the QPS
/// window so nothing but this test can refill it.
///
/// Explicit limits, never the plan defaults: `register_tenant` puts a tenant on
/// the DEFAULT plan (unlimited), which is right for production and useless as a
/// test budget.
fn tenant(db: &EmbeddedDatabase, name: &str, mode: IsolationMode, max_qps: usize, max_connections: usize) -> TenantId {
    db.tenant_manager.set_qps_window(Duration::from_secs(3600));
    let t = db.tenant_manager.register_tenant(name.to_string(), mode);
    db.tenant_manager
        .update_resource_limits(
            t.id,
            ResourceLimits {
                max_storage_bytes: 100_000_000,
                max_connections,
                max_qps,
            },
        )
        .expect("limits");
    t.id
}

/// The window counter for `tenant_id` right now.
fn charged(db: &EmbeddedDatabase, tenant_id: TenantId) -> usize {
    db.tenant_manager
        .get_quota_tracking(tenant_id)
        .expect("tracking")
        .queries_this_window
}

/// Live connection count for `tenant_id` — what `max_connections` is measured
/// against.
fn connections(db: &EmbeddedDatabase, tenant_id: TenantId) -> usize {
    db.tenant_manager
        .get_quota_tracking(tenant_id)
        .expect("tracking")
        .active_connections
}

// ---------------------------------------------------------------------------
// The binding
// ---------------------------------------------------------------------------

/// THE KEYSTONE, end to end: a connection that names a tenant's database is
/// bound to that tenant, and its statements are charged to it.
///
/// On the pre-fix tree this counter stays at ZERO no matter how many statements
/// run — not because the meter is broken, but because the meter asks a
/// process-global slot that no wire handler has ever written.
#[tokio::test]
async fn a_wire_connection_is_bound_to_the_tenant_named_by_its_database() {
    let s = serve().await;
    s.db.execute("CREATE TABLE h1_bound (id INT PRIMARY KEY)").unwrap();
    let acme = tenant(&s.db, "acme_h1", IsolationMode::DatabasePerTenant, 100, 50);

    let (client, task) = connect(s.port, "acme_h1").await.expect("acme_h1 is a tenant");
    let before = charged(&s.db, acme);
    client
        .execute("INSERT INTO h1_bound VALUES ($1)", &[&1i32])
        .await
        .unwrap();
    client.simple_query("SELECT count(*) FROM h1_bound").await.unwrap();

    assert!(
        charged(&s.db, acme) >= before + 2,
        "*** the connection was not bound to its database's tenant: {} statements charged, \
         expected at least 2 more than {before}. `database_name_is_valid` resolves the startup \
         `database` parameter against tenant NAMES; `bind_session_tenant` is what records the \
         answer onto the session. ***",
        charged(&s.db, acme)
    );

    drop(client);
    task.abort();
}

/// A reserved database name (`heliosdb`, `postgres`) binds NOTHING — and
/// "nothing" deliberately means "no decision", not "decided: no tenant", so the
/// connection still falls through to the process-global context.
///
/// This is the compatibility half of the design and it is load-bearing:
/// `postgres` is what libpq probes when the client names no database, and what
/// the `dbname` -> `user` fallback produces. Binding it to "definitely no
/// tenant" would silently cut the default connection string off from a context
/// an embedded caller in the same process had set.
#[tokio::test]
async fn a_reserved_database_name_binds_no_tenant() {
    let s = serve().await;
    s.db.execute("CREATE TABLE h1_reserved (id INT PRIMARY KEY)").unwrap();
    let t = tenant(&s.db, "reserved_probe_h1", IsolationMode::DatabasePerTenant, 100, 50);

    let (client, task) = connect(s.port, "postgres")
        .await
        .expect("postgres is reserved but valid");

    // Nothing bound: the tenant is untouched by this connection...
    client
        .execute("INSERT INTO h1_reserved VALUES ($1)", &[&1i32])
        .await
        .unwrap();
    assert_eq!(
        charged(&s.db, t),
        0,
        "a connection to a RESERVED database name must not be charged to some other tenant"
    );
    assert_eq!(
        connections(&s.db, t),
        0,
        "a reserved name must not consume a connection slot"
    );

    // ...but the process-global fallback still reaches it, exactly as before.
    s.db.tenant_manager.set_current_context(TenantContext {
        tenant_id: t,
        user_id: "h1".to_string(),
        roles: Vec::new(),
        isolation_mode: IsolationMode::DatabasePerTenant,
    });
    client
        .execute("INSERT INTO h1_reserved VALUES ($1)", &[&2i32])
        .await
        .unwrap();
    assert!(
        charged(&s.db, t) >= 1,
        "*** the fallback layer is gone: an UNBOUND session must still resolve the process-global \
         context, or the embedded API and the REPL lose their only way to express a tenant ***"
    );

    s.db.tenant_manager.clear_current_context();
    drop(client);
    task.abort();
}

/// THE case that caused the v4.39.0 back-out, now passing: one tenant's spent
/// budget must not refuse another tenant's queries.
///
/// With the one process-global slot this is unrepresentable — both connections
/// resolve the SAME tenant, so exhausting one exhausts both.
#[tokio::test]
async fn two_connections_are_metered_against_their_own_tenants() {
    let s = serve().await;
    s.db.execute("CREATE TABLE h1_split (id INT PRIMARY KEY, who TEXT)")
        .unwrap();
    let a = tenant(&s.db, "alpha_h1", IsolationMode::DatabasePerTenant, 2, 50);
    let b = tenant(&s.db, "bravo_h1", IsolationMode::DatabasePerTenant, 50, 50);

    let (ca, ta) = connect(s.port, "alpha_h1").await.unwrap();
    let (cb, tb) = connect(s.port, "bravo_h1").await.unwrap();

    // Spend alpha's entire budget.
    ca.execute("INSERT INTO h1_split VALUES ($1, $2)", &[&1i32, &"a"])
        .await
        .unwrap();
    ca.execute("INSERT INTO h1_split VALUES ($1, $2)", &[&2i32, &"a"])
        .await
        .unwrap();
    let refused = ca
        .execute("INSERT INTO h1_split VALUES ($1, $2)", &[&3i32, &"a"])
        .await
        .expect_err("alpha's budget is 2");
    let db_err = refused.as_db_error().expect("a DbError");
    assert_eq!(db_err.code().code(), "53400", "got {:?}", db_err.message());

    // Bravo is untouched — this is the whole point.
    for i in 10..16i32 {
        cb.execute("INSERT INTO h1_split VALUES ($1, $2)", &[&i, &"b"])
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "*** bravo was refused by ALPHA's spent budget — the two connections are \
                     sharing one tenant context, which is exactly the defect sprinter \
                     d03de7fc3b22 exists to fix. Error: {e} ***"
                )
            });
    }

    assert_eq!(charged(&s.db, a), 2, "alpha spent exactly its budget");
    assert!(
        charged(&s.db, b) >= 6,
        "bravo's own statements must be charged to bravo, got {}",
        charged(&s.db, b)
    );

    drop(ca);
    drop(cb);
    ta.abort();
    tb.abort();
}

/// A session binding SHADOWS the process-global context rather than merging with
/// it: an embedded caller selecting tenant A must not re-tenant a wire
/// connection bound to B.
///
/// The pre-fix tree cannot distinguish the two at all — there is one slot — so
/// this is the direct statement of what the resolver's precedence buys.
#[tokio::test]
async fn a_session_binding_shadows_the_process_global() {
    let s = serve().await;
    s.db.execute("CREATE TABLE h1_shadow (id INT PRIMARY KEY)").unwrap();
    let global = tenant(&s.db, "global_h1", IsolationMode::DatabasePerTenant, 100, 50);
    let bound = tenant(&s.db, "bound_h1", IsolationMode::DatabasePerTenant, 100, 50);

    // An embedded caller (or the REPL's `\tenant use`) selects `global_h1`.
    s.db.tenant_manager.set_current_context(TenantContext {
        tenant_id: global,
        user_id: "embedded".to_string(),
        roles: Vec::new(),
        isolation_mode: IsolationMode::DatabasePerTenant,
    });
    let global_before = charged(&s.db, global);

    let (client, task) = connect(s.port, "bound_h1").await.unwrap();
    client
        .execute("INSERT INTO h1_shadow VALUES ($1)", &[&1i32])
        .await
        .unwrap();
    client
        .execute("INSERT INTO h1_shadow VALUES ($1)", &[&2i32])
        .await
        .unwrap();

    assert!(
        charged(&s.db, bound) >= 2,
        "the bound connection's statements must be charged to ITS tenant, got {}",
        charged(&s.db, bound)
    );
    assert_eq!(
        charged(&s.db, global),
        global_before,
        "*** a wire connection's statements were charged to whatever tenant an EMBEDDED caller \
         last selected — the session binding is not shadowing the process global ***"
    );

    s.db.tenant_manager.clear_current_context();
    drop(client);
    task.abort();
}

/// The session-LESS callers keep working exactly as they did. Deleting the
/// fallback layer would silently disable `\tenant use` and every embedded
/// `set_current_context` caller, which is why it is not optional.
#[test]
fn the_embedded_api_still_resolves_the_process_global() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.tenant_manager.set_qps_window(Duration::from_secs(3600));
    db.execute("CREATE TABLE h1_embedded (id INT PRIMARY KEY)").unwrap();
    let t = tenant(&db, "embedded_h1", IsolationMode::DatabasePerTenant, 3, 50);

    // No session, no database name — the process global is the only channel.
    db.tenant_manager.set_current_context(TenantContext {
        tenant_id: t,
        user_id: "embedded".to_string(),
        roles: Vec::new(),
        isolation_mode: IsolationMode::DatabasePerTenant,
    });

    db.execute("INSERT INTO h1_embedded VALUES (1)").unwrap();
    db.execute_params("INSERT INTO h1_embedded VALUES ($1)", &[Value::Int4(2)])
        .unwrap();
    db.query("SELECT count(*) FROM h1_embedded", &[]).unwrap();
    assert_eq!(
        charged(&db, t),
        3,
        "the text write, the bound-parameter write and the READ must each be charged once"
    );

    let refused = db
        .execute("INSERT INTO h1_embedded VALUES (3)")
        .expect_err("the budget is 3");
    assert!(
        refused.to_string().to_lowercase().contains("quota exceeded"),
        "{refused}"
    );

    db.tenant_manager.clear_current_context();
}

// ---------------------------------------------------------------------------
// max_connections — enforced for the first time
// ---------------------------------------------------------------------------

/// `max_connections` had NO enforcement path at all: `add_connection` and
/// `remove_connection` had zero production callers anywhere in the tree, so
/// `check_quota(_, "connections")` was dead behind them. A tenant's connection
/// limit was stored, displayed and ignored.
///
/// It is charged where a connection first becomes attributable to a tenant (the
/// startup binding) and released in `EmbeddedDatabase::destroy_session`, the one
/// funnel every disconnect reaches — so the release half is proved here too, by
/// reconnecting after a drop.
#[tokio::test]
async fn max_connections_refuses_the_connection_that_exceeds_it() {
    let s = serve().await;
    let t = tenant(&s.db, "capped_h1", IsolationMode::DatabasePerTenant, 1000, 2);

    let (c1, t1) = connect(s.port, "capped_h1").await.expect("connection 1 of 2");
    let (c2, t2) = connect(s.port, "capped_h1").await.expect("connection 2 of 2");
    assert_eq!(connections(&s.db, t), 2, "both connections must be counted");

    let refused = connect(s.port, "capped_h1").await.err().expect(
        "*** max_connections is still unenforced: a third connection was accepted against \
                 a limit of 2. `TenantManager::add_connection` had no production caller at all ***",
    );
    let db_err = refused.as_db_error().expect("a DbError, not a transport failure");
    assert_eq!(
        db_err.code().code(),
        "53300",
        "a refused connection must be 53300 too_many_connections — PostgreSQL's own code, and the \
         one a pooler backs off on; got {} / {:?}",
        db_err.code().code(),
        db_err.message()
    );
    assert_eq!(
        connections(&s.db, t),
        2,
        "the refused connection must not consume a slot"
    );

    // Release: dropping a connection gives its slot back, so a replacement
    // connects. Without this the count ratchets up until the tenant is
    // permanently locked out.
    drop(c1);
    t1.abort();
    for _ in 0..50 {
        if connections(&s.db, t) < 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        connections(&s.db, t),
        1,
        "*** a disconnect did not release the tenant's connection slot — the count only ever \
         climbs, so the limit eventually refuses everybody ***"
    );

    let (c3, t3) = connect(s.port, "capped_h1")
        .await
        .expect("the freed slot must admit a new connection");

    drop(c2);
    drop(c3);
    t2.abort();
    t3.abort();
}

// ---------------------------------------------------------------------------
// RLS — the highest-risk consequence, stated as a test
// ---------------------------------------------------------------------------

/// Making the tenant context per-connection changes WHICH ROWS a connection
/// sees. That is the point of the item and its largest blast radius, so it is
/// pinned explicitly rather than left as a side effect.
///
/// Before the binding, an RLS-enabled tenant's policies were inert on every wire
/// connection — `should_apply_rls` asked the process-global slot, which no wire
/// handler wrote — so a client connecting to a tenant's database read the whole
/// table. It is filtered now.
///
/// Note what does NOT change: a tenant created by `CREATE DATABASE` is
/// `DatabasePerTenant`, for which `register_tenant_with_plan` sets
/// `rls_enabled: false`, so binding such a connection adds no filtering at all.
/// Only a `SharedSchema` tenant with policies gates rows.
#[tokio::test]
async fn rls_gates_a_wire_connection_bound_to_an_rls_enabled_tenant() {
    let s = serve().await;
    s.db.execute("CREATE TABLE h1_rls (id INT PRIMARY KEY, owner TEXT NOT NULL)")
        .unwrap();
    // Seeded BEFORE the policy exists, so the fixture itself is never filtered.
    s.db.execute("INSERT INTO h1_rls VALUES (1, 'h1')").unwrap();
    s.db.execute("INSERT INTO h1_rls VALUES (2, 'someone_else')").unwrap();

    let t = tenant(&s.db, "rls_h1", IsolationMode::SharedSchema, 1000, 50);
    assert!(
        s.db.tenant_manager.get_tenant(t).unwrap().rls_enabled,
        "a SharedSchema tenant is RLS-enabled; the rest of this test is vacuous otherwise"
    );
    s.db.tenant_manager.create_rls_policy(
        "h1_rls".to_string(),
        "own_rows".to_string(),
        "H1 read policy".to_string(),
        RLSCommand::Select,
        // The login name the connection string authenticates with.
        "owner = 'h1'".to_string(),
        None,
    );

    let (client, task) = connect(s.port, "rls_h1").await.unwrap();
    let rows = client.query("SELECT id FROM h1_rls", &[]).await.unwrap();
    assert_eq!(
        rows.len(),
        1,
        "*** the policy did not gate the wire connection: {} rows came back. A connection bound \
         to an RLS-enabled tenant must be filtered by that tenant's policies. ***",
        rows.len()
    );
    assert_eq!(rows[0].get::<_, i32>(0), 1);

    // Non-vacuity: the hidden row is really there. An UNBOUND reader (the
    // embedded handle, no context) sees both — so "1 row" above is "filtered",
    // not "never inserted".
    let all = s.db.query("SELECT id FROM h1_rls", &[]).unwrap();
    assert_eq!(all.len(), 2, "the second row must still exist in the table");

    drop(client);
    task.abort();
}

// ---------------------------------------------------------------------------
// CREATE DATABASE — the shipped path a wire client actually uses
// ---------------------------------------------------------------------------

/// `CREATE DATABASE foo` registers a tenant, and connecting to `foo` binds it —
/// the whole loop over the wire, with no library calls in the middle.
///
/// It also pins the plan: `handle_create_database` used to register on the
/// `"free"` plan, whose limits are `max_qps: 10` / `max_connections: 5`. That
/// was inert while nothing enforced either number; with this item it would have
/// capped every `CREATE DATABASE` database at ten statements a second. A plan is
/// an explicit choice, so the implicit path takes the DEFAULT plan.
#[tokio::test]
async fn create_database_registers_a_tenant_that_is_not_on_a_free_tier_budget() {
    let s = serve().await;
    let (admin, admin_task) = connect(s.port, "postgres").await.unwrap();
    admin.simple_query("CREATE DATABASE tenantdb_h1").await.unwrap();

    let created =
        s.db.tenant_manager
            .list_tenants()
            .into_iter()
            .find(|t| t.name.eq_ignore_ascii_case("tenantdb_h1"))
            .expect("CREATE DATABASE must register a tenant");
    assert!(
        created.limits.max_qps > 10,
        "*** a database created by plain `CREATE DATABASE` was put on the free tier's \
         max_qps={} — now that metering is enforced, that silently caps every client of it ***",
        created.limits.max_qps
    );
    assert!(created.limits.max_connections > 5, "same for max_connections");

    // And connecting to it binds it: the name the client asks for is the tenant.
    let (client, task) = connect(s.port, "tenantdb_h1")
        .await
        .expect("CREATE DATABASE makes the name connectable");
    assert_eq!(
        connections(&s.db, created.id),
        1,
        "*** the connection was not attributed to the tenant its database names ***"
    );
    client.simple_query("SELECT 1").await.unwrap();
    assert!(charged(&s.db, created.id) >= 1, "its statements are charged to it");

    drop(client);
    task.abort();
    drop(admin);
    admin_task.abort();
}
