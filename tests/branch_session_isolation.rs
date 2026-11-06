//! sprinter 6bc4a4040eb7, phase B1 — the active branch is SESSION state.
//!
//! Plan: `docs/plans/BRANCH_ISOLATION.md` (D1, and the dropped-branch lookup of
//! D6). Spec: `docs/plans/specs/B1_session_branch.md`.
//!
//! On the pre-B1 tree the only branch selector was `StorageEngine.current_branch`,
//! ONE `Mutex<Option<String>>` for the whole process:
//!
//! * one connection's `USE BRANCH` moved every other connection — PostgreSQL
//!   wire, embedded sessions, REST and MCP alike;
//! * the REST branch routes validated `:name` and then ran on main;
//! * MCP's branch scope was a global switch-and-restore;
//! * a dropped branch could still be `USE`d (and written), its name could not
//!   be reused, and `CREATE BRANCH IF NOT EXISTS` no-opped onto its rows.
//!
//! Every test here fails on that tree. The cross-session ones are the
//! load-bearing ones: each asserts what a SECOND session sees after the first
//! one moved, which a global selector cannot get right.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::tenant::IsolationMode;
use heliosdb_nano::{
    EmbeddedDatabase, Tuple, Value, BRANCH_BINDING_GONE_MARKER, BRANCH_CLOSING_MARKER, BRANCH_IN_USE_MARKER,
    BRANCH_NOT_ACTIVE_MARKER, BRANCH_SWITCH_IN_TRANSACTION_MARKER,
};
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// `t(id INT PRIMARY KEY, v TEXT)`:
///
/// * main:     1 = `main1`, 2 = `main2`
/// * branch b: 1 = `b1` (updated on the branch), 2 inherited, 3 = `b3` (branch-only)
///
/// Seeded through the embedded handle's OWN session, which ends on main.
fn seed(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'main1')").unwrap();
    db.execute("INSERT INTO t VALUES (2, 'main2')").unwrap();
    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    db.execute("USE BRANCH b").unwrap();
    db.execute("UPDATE t SET v = 'b1' WHERE id = 1").unwrap();
    db.execute("INSERT INTO t VALUES (3, 'b3')").unwrap();
    db.execute("USE BRANCH main").unwrap();
}

fn seeded() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    db
}

fn text(tuple: &Tuple, index: usize) -> String {
    match tuple.values.get(index) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected a text value at {index}, got {other:?}"),
    }
}

/// `v` of row `id`, read by the embedded handle's own session.
fn handle_v(db: &EmbeddedDatabase, id: i32) -> Option<String> {
    db.query(&format!("SELECT v FROM t WHERE id = {id}"), &[])
        .unwrap()
        .first()
        .map(|row| text(row, 0))
}

fn session(db: &EmbeddedDatabase, name: &str) -> SessionId {
    db.create_session(name, IsolationLevel::ReadCommitted).unwrap()
}

fn session_rows(db: &EmbeddedDatabase, sid: SessionId, sql: &str) -> heliosdb_nano::Result<Vec<Tuple>> {
    db.query_with_columns_for_session(sid, sql).map(|(rows, _)| rows)
}

/// `v` of row `id`, read by session `sid`.
fn session_v(db: &EmbeddedDatabase, sid: SessionId, id: i32) -> Option<String> {
    session_rows(db, sid, &format!("SELECT v FROM t WHERE id = {id}"))
        .unwrap()
        .first()
        .map(|row| text(row, 0))
}

fn main_row_count(db: &EmbeddedDatabase) -> usize {
    db.query("SELECT id FROM t", &[]).unwrap().len()
}

// ---------------------------------------------------------------------------
// PostgreSQL wire harness (the `session_scoping_batch_g2.rs` pattern)
// ---------------------------------------------------------------------------

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

async fn serve(db: EmbeddedDatabase) -> Server {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let db = Arc::new(db);
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(32),
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

async fn connect_with(
    port: u16,
    extra: &str,
) -> std::result::Result<(Client, tokio::task::JoinHandle<()>), tokio_postgres::Error> {
    let cs = format!("host=127.0.0.1 port={port} user=postgres {extra}");
    let (client, conn) = tokio_postgres::connect(&cs, NoTls).await?;
    let task = tokio::spawn(async move { conn.await.map(|_| ()).unwrap_or(()) });
    Ok((client, task))
}

async fn connect(port: u16, dbname: &str) -> (Client, tokio::task::JoinHandle<()>) {
    connect_with(port, &format!("dbname={dbname}"))
        .await
        .unwrap_or_else(|e| panic!("connect dbname={dbname}: {e}"))
}

/// The first column of every row of a simple-protocol query.
async fn simple_col(client: &Client, sql: &str) -> Vec<String> {
    client
        .simple_query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(row.get(0).unwrap_or("NULL").to_string()),
            _ => None,
        })
        .collect()
}

/// `v` of row `id` over the simple protocol.
async fn wire_v(client: &Client, id: i32) -> Option<String> {
    simple_col(client, &format!("SELECT v FROM t WHERE id = {id}"))
        .await
        .into_iter()
        .next()
}

fn sqlstate(e: &tokio_postgres::Error) -> String {
    e.as_db_error()
        .map(|d| d.code().code().to_string())
        .unwrap_or_else(|| format!("<not a DbError: {e}>"))
}

// ---------------------------------------------------------------------------
// PostgreSQL wire: two connections, simple protocol
// ---------------------------------------------------------------------------

/// A `USE BRANCH b` moves connection A only. B keeps reading main, B's INSERT
/// lands on main (proved after a third connection drops `b`), and B's own
/// `USE BRANCH main` does not move A back.
#[tokio::test]
async fn two_pg_connections_keep_their_own_branch_simple_protocol() {
    let s = serve(seeded()).await;
    let (a, ta) = connect(s.port, "postgres").await;
    let (b, tb) = connect(s.port, "postgres").await;
    let (c, tc) = connect(s.port, "postgres").await;

    a.simple_query("USE BRANCH b").await.unwrap();
    assert_eq!(wire_v(&a, 1).await.as_deref(), Some("b1"), "A must read its branch");
    assert_eq!(
        wire_v(&a, 3).await.as_deref(),
        Some("b3"),
        "A must see the branch-only row"
    );

    assert_eq!(
        wire_v(&b, 1).await.as_deref(),
        Some("main1"),
        "*** connection A's USE BRANCH moved connection B ***"
    );
    assert_eq!(wire_v(&b, 3).await, None, "B must not see the branch-only row");

    b.simple_query("INSERT INTO t VALUES (10, 'from_b')").await.unwrap();
    b.simple_query("USE BRANCH main").await.unwrap();
    assert_eq!(
        wire_v(&a, 1).await.as_deref(),
        Some("b1"),
        "*** connection B's USE BRANCH main moved connection A ***"
    );

    // A third connection drops the branch; B's row is still on main, so it
    // never went to the branch overlay.
    c.simple_query("DROP BRANCH b").await.unwrap();
    assert_eq!(
        wire_v(&c, 10).await.as_deref(),
        Some("from_b"),
        "B's INSERT must land on main"
    );
    assert_eq!(handle_v(&s.db, 10).as_deref(), Some("from_b"));
    assert_eq!(
        handle_v(&s.db, 1).as_deref(),
        Some("main1"),
        "main is untouched by the branch"
    );

    drop((a, b, c));
    ta.abort();
    tb.abort();
    tc.abort();
}

/// Same, extended protocol: a statement PREPARED on main and executed after
/// `USE BRANCH b` reads `b`; one prepared on `b` and executed after `USE BRANCH
/// main` reads main. A plan carries no branch decision — the binding is read at
/// execution.
#[tokio::test]
async fn extended_protocol_statements_read_the_branch_bound_at_execution() {
    let s = serve(seeded()).await;
    let (a, ta) = connect(s.port, "postgres").await;
    let (b, tb) = connect(s.port, "postgres").await;

    let on_main = a.prepare("SELECT v FROM t WHERE id = $1").await.unwrap();
    a.simple_query("USE BRANCH b").await.unwrap();
    let got: String = a.query_one(&on_main, &[&1i32]).await.unwrap().get(0);
    assert_eq!(
        got, "b1",
        "a statement prepared on main must read the branch bound when it runs"
    );
    // An INHERITED row (no overlay key on the branch) and a BRANCH-ONLY row:
    // a point lookup that probed main's index, or only the overlay key, gets
    // one of these wrong.
    let got: String = a.query_one(&on_main, &[&2i32]).await.unwrap().get(0);
    assert_eq!(got, "main2", "the branch must see the row it inherited unchanged");
    let got: String = a.query_one(&on_main, &[&3i32]).await.unwrap().get(0);
    assert_eq!(got, "b3", "the branch must see its branch-only row");

    // B is untouched by A, on the extended protocol too.
    let got: String = b
        .query_one("SELECT v FROM t WHERE id = $1", &[&1i32])
        .await
        .unwrap()
        .get(0);
    assert_eq!(got, "main1", "*** connection A's USE BRANCH moved connection B ***");

    let on_branch = a.prepare("SELECT v AS w FROM t WHERE id = $1").await.unwrap();
    a.simple_query("USE BRANCH main").await.unwrap();
    let got: String = a.query_one(&on_branch, &[&1i32]).await.unwrap().get(0);
    assert_eq!(
        got, "main1",
        "a statement prepared on b must read main after USE BRANCH main"
    );
    assert!(
        a.query_opt(&on_branch, &[&3i32]).await.unwrap().is_none(),
        "main must not see the branch-only row through a statement prepared on b"
    );

    // The binding statements themselves work over the extended protocol.
    a.execute("SET heliosdb.branch = 'b'", &[]).await.unwrap();
    let got: String = a.query_one(&on_main, &[&1i32]).await.unwrap().get(0);
    assert_eq!(
        got, "b1",
        "SET heliosdb.branch over the extended protocol must bind the branch"
    );
    a.execute("USE BRANCH main", &[]).await.unwrap();
    let got: String = a.query_one(&on_main, &[&1i32]).await.unwrap().get(0);
    assert_eq!(
        got, "main1",
        "USE BRANCH over the extended protocol must bind the branch"
    );

    drop((a, b));
    ta.abort();
    tb.abort();
}

// ---------------------------------------------------------------------------
// Caches
// ---------------------------------------------------------------------------

/// The result cache is keyed by SQL text with no branch dimension: the same
/// text on main and on a branch must return each its own rows, in both orders,
/// however often it repeats.
#[test]
fn the_result_cache_never_crosses_branches_in_either_order() {
    let db = seeded();
    let on_main = session(&db, "rc_main");
    let on_b = session(&db, "rc_b");
    db.execute_for_session(on_b, "USE BRANCH b").unwrap();

    let column = |sid: SessionId, sql: &str| -> Vec<String> {
        session_rows(&db, sid, sql)
            .unwrap()
            .iter()
            .map(|row| text(row, 0))
            .collect()
    };

    // main first, warmed well past any admission threshold, then the branch.
    let sql = "SELECT v FROM t ORDER BY id";
    for _ in 0..4 {
        assert_eq!(column(on_main, sql), vec!["main1", "main2"]);
    }
    for _ in 0..4 {
        assert_eq!(
            column(on_b, sql),
            vec!["b1", "main2", "b3"],
            "*** the branch session was served main's cached rows ***"
        );
    }

    // branch first, then main.
    let sql = "SELECT v FROM t ORDER BY id DESC";
    for _ in 0..4 {
        assert_eq!(column(on_b, sql), vec!["b3", "main2", "b1"]);
    }
    for _ in 0..4 {
        assert_eq!(
            column(on_main, sql),
            vec!["main2", "main1"],
            "*** the main session was served the branch's cached rows ***"
        );
    }
}

/// The row cache is keyed `(table, row_id)` with no branch dimension: a point
/// SELECT by primary key after an UPDATE on the branch returns the branch's
/// value on the branch session and main's value on a main session.
#[test]
fn the_row_cache_never_crosses_branches() {
    let db = seeded();
    let on_main = session(&db, "row_main");
    let on_b = session(&db, "row_b");
    db.execute_for_session(on_b, "USE BRANCH b").unwrap();

    let point = |sid: SessionId| -> String {
        let rows = session_rows(&db, sid, "SELECT * FROM t WHERE id = 1").unwrap();
        assert_eq!(rows.len(), 1, "the point lookup must find row 1");
        text(&rows[0], 1)
    };

    // Warm main's row cache through the point-lookup fast path.
    for _ in 0..3 {
        assert_eq!(point(on_main), "main1");
    }
    db.execute_for_session(on_b, "UPDATE t SET v = 'b1x' WHERE id = 1")
        .unwrap();
    assert_eq!(point(on_b), "b1x", "the branch session must read the branch's value");
    assert_eq!(
        point(on_main),
        "main1",
        "*** the main session read the branch's update ***"
    );

    // And the reverse: the branch reads first, main afterwards.
    for _ in 0..3 {
        assert_eq!(point(on_b), "b1x");
    }
    assert_eq!(point(on_main), "main1");
}

// ---------------------------------------------------------------------------
// PostgreSQL startup: `database=<db>@<branch>` and `options=-c heliosdb.branch=…`
// ---------------------------------------------------------------------------

fn tenant_connections(db: &EmbeddedDatabase, name: &str) -> usize {
    let tenant = db
        .tenant_manager
        .list_tenants()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("tenant {name} must exist"));
    db.tenant_manager
        .get_quota_tracking(tenant.id)
        .expect("tracking")
        .active_connections
}

#[tokio::test]
async fn startup_database_at_branch_selects_the_branch() {
    let db = seeded();
    db.execute("CREATE DATABASE tb1").unwrap();
    // A tenant whose OWN name contains `@` — and names a real branch after it.
    db.tenant_manager
        .register_tenant("acme@b".to_string(), IsolationMode::DatabasePerTenant);
    let s = serve(db).await;

    let (on_b, t1) = connect(s.port, "postgres@b").await;
    let (on_main, t2) = connect(s.port, "postgres").await;
    assert_eq!(
        wire_v(&on_b, 1).await.as_deref(),
        Some("b1"),
        "dbname=postgres@b must read b"
    );
    assert_eq!(simple_col(&on_b, "SHOW heliosdb.branch").await, vec!["b"]);
    assert_eq!(
        wire_v(&on_main, 1).await.as_deref(),
        Some("main1"),
        "dbname=postgres reads main"
    );
    assert_eq!(simple_col(&on_main, "SHOW heliosdb.branch").await, vec!["main"]);

    // A tenant AND a branch.
    let (tenant_b, t3) = connect(s.port, "tb1@b").await;
    assert_eq!(
        tenant_connections(&s.db, "tb1"),
        1,
        "dbname=tb1@b must bind the tenant tb1"
    );
    assert_eq!(wire_v(&tenant_b, 1).await.as_deref(), Some("b1"), "…and the branch b");

    // A tenant whose name contains `@` still resolves as itself and selects no
    // branch: the WHOLE value is resolved before any split.
    let (at_tenant, t4) = connect(s.port, "acme@b").await;
    assert_eq!(
        tenant_connections(&s.db, "acme@b"),
        1,
        "dbname=acme@b is the tenant acme@b"
    );
    assert_eq!(wire_v(&at_tenant, 1).await.as_deref(), Some("main1"), "…on main");
    assert_eq!(simple_col(&at_tenant, "SHOW heliosdb.branch").await, vec!["main"]);

    // An unknown branch, or an unknown database part, is FATAL 3D000.
    for dbname in ["postgres@nosuch", "nosuchdb@b", "postgres@"] {
        let refused = connect_with(s.port, &format!("dbname={dbname}"))
            .await
            .err()
            .unwrap_or_else(|| panic!("dbname={dbname} must be refused"));
        assert_eq!(sqlstate(&refused), "3D000", "dbname={dbname}");
    }

    drop((on_b, on_main, tenant_b, at_tenant));
    for t in [t1, t2, t3, t4] {
        t.abort();
    }
}

#[tokio::test]
async fn startup_options_select_the_branch() {
    let db = seeded();
    db.execute("CREATE BRANCH c AS OF NOW").unwrap();
    let s = serve(db).await;

    let (on_b, t1) = connect_with(s.port, "dbname=postgres options='-c heliosdb.branch=b'")
        .await
        .unwrap();
    assert_eq!(
        wire_v(&on_b, 1).await.as_deref(),
        Some("b1"),
        "options -c heliosdb.branch=b reads b"
    );
    assert_eq!(simple_col(&on_b, "SHOW heliosdb.branch").await, vec!["b"]);

    // Both forms naming the SAME branch are fine.
    let (both, t2) = connect_with(s.port, "dbname=postgres@b options='-c heliosdb.branch=b'")
        .await
        .unwrap();
    assert_eq!(wire_v(&both, 1).await.as_deref(), Some("b1"));

    // Unknown branch in options: 3D000.
    let refused = connect_with(s.port, "dbname=postgres options='-c heliosdb.branch=nosuch'")
        .await
        .err()
        .expect("an unknown branch in options must be refused");
    assert_eq!(sqlstate(&refused), "3D000");

    // The two forms disagreeing: 3D000.
    let refused = connect_with(s.port, "dbname=postgres@b options='-c heliosdb.branch=c'")
        .await
        .err()
        .expect("disagreeing startup branches must be refused");
    assert_eq!(sqlstate(&refused), "3D000");

    drop((on_b, both));
    t1.abort();
    t2.abort();
}

/// `SET heliosdb.branch`, `SHOW heliosdb.branch`, `RESET heliosdb.branch` and
/// `current_setting('heliosdb.branch')` are the same binding `USE BRANCH` makes.
#[tokio::test]
async fn set_show_reset_heliosdb_branch_agree_with_use_branch() {
    let s = serve(seeded()).await;
    let (a, ta) = connect(s.port, "postgres").await;
    let (other, tb) = connect(s.port, "postgres").await;

    assert_eq!(simple_col(&a, "SHOW heliosdb.branch").await, vec!["main"]);

    a.simple_query("SET heliosdb.branch = 'b'").await.unwrap();
    assert_eq!(
        wire_v(&a, 1).await.as_deref(),
        Some("b1"),
        "SET heliosdb.branch binds the branch"
    );
    assert_eq!(simple_col(&a, "SHOW heliosdb.branch").await, vec!["b"]);
    assert_eq!(
        simple_col(&a, "SELECT current_setting('heliosdb.branch')").await,
        vec!["b"]
    );
    assert_eq!(
        simple_col(&other, "SHOW heliosdb.branch").await,
        vec!["main"],
        "*** one connection's SET heliosdb.branch reached another ***"
    );

    a.simple_query("RESET heliosdb.branch").await.unwrap();
    assert_eq!(wire_v(&a, 1).await.as_deref(), Some("main1"), "RESET returns to main");
    assert_eq!(simple_col(&a, "SHOW heliosdb.branch").await, vec!["main"]);

    a.simple_query("USE BRANCH b").await.unwrap();
    let shown: String = a.query_one("SHOW heliosdb.branch", &[]).await.unwrap().get(0);
    assert_eq!(
        shown, "b",
        "SHOW over the extended protocol must report the USE BRANCH binding"
    );
    assert_eq!(
        simple_col(&a, "SELECT current_setting('heliosdb.branch')").await,
        vec!["b"]
    );

    a.simple_query("SET heliosdb.branch TO 'main'").await.unwrap();
    assert_eq!(wire_v(&a, 1).await.as_deref(), Some("main1"));

    let refused = a
        .simple_query("SET heliosdb.branch = 'nosuch'")
        .await
        .err()
        .expect("an unknown branch must be refused");
    assert_eq!(sqlstate(&refused), "3D000");
    assert_eq!(
        simple_col(&a, "SHOW heliosdb.branch").await,
        vec!["main"],
        "a refusal binds nothing"
    );

    drop((a, other));
    ta.abort();
    tb.abort();
}

// ---------------------------------------------------------------------------
// Embedded sessions
// ---------------------------------------------------------------------------

#[test]
fn two_embedded_sessions_on_different_branches() {
    let db = seeded();
    db.execute("CREATE BRANCH c AS OF NOW").unwrap();
    db.execute("USE BRANCH c").unwrap();
    db.execute("UPDATE t SET v = 'c1' WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();

    let s1 = session(&db, "emb_b");
    let s2 = session(&db, "emb_c");
    db.execute_for_session(s1, "USE BRANCH b").unwrap();
    db.execute_for_session(s2, "USE BRANCH c").unwrap();

    assert_eq!(session_v(&db, s1, 1).as_deref(), Some("b1"));
    assert_eq!(
        session_v(&db, s2, 1).as_deref(),
        Some("c1"),
        "*** session 1's branch reached session 2 ***"
    );
    assert_eq!(
        handle_v(&db, 1).as_deref(),
        Some("main1"),
        "*** a session's branch reached the handle ***"
    );
    assert_eq!(db.current_branch(), "main");
    assert_eq!(db.session_branch(s1).unwrap(), "b");
    assert_eq!(db.session_branch(s2).unwrap(), "c");

    db.execute_for_session(s2, "INSERT INTO t VALUES (4, 'c4')").unwrap();
    assert_eq!(session_v(&db, s2, 4).as_deref(), Some("c4"));
    assert_eq!(session_v(&db, s1, 4), None, "a write on c must not appear on b");
    assert_eq!(handle_v(&db, 4), None, "a write on c must not appear on main");
}

// ---------------------------------------------------------------------------
// Dropped and merged branches
// ---------------------------------------------------------------------------

#[test]
fn a_dropped_branch_cannot_be_used() {
    let db = seeded();
    db.execute("CREATE BRANCH gone AS OF NOW").unwrap();
    db.execute("DROP BRANCH gone").unwrap();

    let err = db
        .execute("USE BRANCH gone")
        .expect_err("USE of a dropped branch must be refused");
    assert!(err.to_string().contains(BRANCH_NOT_ACTIVE_MARKER), "got: {err}");
    let err = db
        .switch_branch("gone")
        .expect_err("switch_branch to a dropped branch must be refused");
    assert!(err.to_string().contains(BRANCH_NOT_ACTIVE_MARKER), "got: {err}");
    assert_eq!(db.current_branch(), "main", "a refusal binds nothing");

    let err = db.execute("USE BRANCH never_existed").expect_err("unknown branch");
    assert!(err.to_string().contains(BRANCH_NOT_ACTIVE_MARKER), "got: {err}");
}

/// A session bound to `b` while ANOTHER session drops `b` fails closed: its
/// next SELECT and INSERT are refused and main's rows are unchanged. The
/// binding stays until the session changes it.
#[test]
fn a_session_bound_to_a_dropped_branch_fails_closed() {
    let db = seeded();
    let bound = session(&db, "bound");
    let dropper = session(&db, "dropper");
    db.execute_for_session(bound, "USE BRANCH b").unwrap();
    assert_eq!(session_v(&db, bound, 1).as_deref(), Some("b1"));
    let before = main_row_count(&db);

    db.execute_for_session(dropper, "DROP BRANCH b")
        .expect("another session's binding does not block the drop");

    let err = session_rows(&db, bound, "SELECT v FROM t WHERE id = 1").expect_err("SELECT must be refused");
    assert!(err.to_string().contains(BRANCH_BINDING_GONE_MARKER), "got: {err}");
    let err = db
        .execute_for_session(bound, "INSERT INTO t VALUES (50, 'leak')")
        .expect_err("INSERT must be refused");
    assert!(err.to_string().contains(BRANCH_BINDING_GONE_MARKER), "got: {err}");
    let params = [Value::Int4(51), Value::String("leak".into())];
    let err = db
        .execute_params_for_session(bound, "INSERT INTO t VALUES ($1, $2)", &params)
        .expect_err("the params family must refuse too");
    assert!(err.to_string().contains(BRANCH_BINDING_GONE_MARKER), "got: {err}");

    assert_eq!(
        main_row_count(&db),
        before,
        "*** a write from a dropped-branch session reached main ***"
    );
    assert_eq!(handle_v(&db, 50), None);
    assert_eq!(handle_v(&db, 51), None);

    // The binding stays: still refused on the next statement.
    assert!(
        session_rows(&db, bound, "SELECT 1").is_err(),
        "the binding must stay until changed"
    );
    // The session gets out by changing its own binding.
    db.execute_for_session(bound, "USE BRANCH main").unwrap();
    assert_eq!(session_v(&db, bound, 1).as_deref(), Some("main1"));
}

#[test]
fn dropping_the_sessions_own_branch_is_refused_55006() {
    let db = seeded();
    let bound = session(&db, "own");
    db.execute_for_session(bound, "USE BRANCH b").unwrap();
    let err = db
        .execute_for_session(bound, "DROP BRANCH b")
        .expect_err("DROP BRANCH of the session's own branch must be refused");
    assert!(err.to_string().contains(BRANCH_IN_USE_MARKER), "got: {err}");
    assert!(
        db.storage.active_branch_exists("b"),
        "the refused drop must leave b active"
    );
    assert_eq!(session_v(&db, bound, 1).as_deref(), Some("b1"));
}

/// `CREATE BRANCH b` after a drop makes a NEW branch with a NEW id that sees
/// none of the old branch's rows — and a session still bound to the OLD `b`
/// stays refused (the new branch is not the one it bound).
#[test]
fn a_recreated_branch_starts_fresh_with_a_new_id() {
    let db = seeded();
    let old_id = db.storage.get_branch("b").unwrap().branch_id;
    let stale = session(&db, "stale");
    db.execute_for_session(stale, "USE BRANCH b").unwrap();
    db.execute("DROP BRANCH b").unwrap();

    db.execute("CREATE BRANCH b AS OF NOW")
        .expect("a dropped branch's name must be free again");
    let new_id = db.storage.get_branch("b").unwrap().branch_id;
    assert_ne!(new_id, old_id, "a new branch must get a new id");

    db.execute("USE BRANCH b").unwrap();
    assert_eq!(
        handle_v(&db, 1).as_deref(),
        Some("main1"),
        "the old branch's update must not resurrect"
    );
    assert_eq!(handle_v(&db, 3), None, "the old branch's row must not resurrect");
    db.execute("USE BRANCH main").unwrap();

    let err = session_rows(&db, stale, "SELECT v FROM t WHERE id = 1")
        .expect_err("a session bound to the OLD b must stay refused");
    assert!(err.to_string().contains(BRANCH_BINDING_GONE_MARKER), "got: {err}");
}

#[test]
fn create_branch_if_not_exists_over_a_dropped_branch_creates_a_fresh_one() {
    let db = seeded();
    db.execute("CREATE BRANCH e AS OF NOW").unwrap();
    let old_id = db.storage.get_branch("e").unwrap().branch_id;
    db.execute("USE BRANCH e").unwrap();
    db.execute("INSERT INTO t VALUES (60, 'e60')").unwrap();
    db.execute("USE BRANCH main").unwrap();
    db.execute("DROP BRANCH e").unwrap();

    db.execute("CREATE BRANCH IF NOT EXISTS e AS OF NOW").unwrap();
    assert!(
        db.storage.active_branch_exists("e"),
        "IF NOT EXISTS over a dropped name must create"
    );
    assert_ne!(db.storage.get_branch("e").unwrap().branch_id, old_id);
    db.execute("USE BRANCH e").unwrap();
    assert_eq!(
        handle_v(&db, 60),
        None,
        "*** IF NOT EXISTS resurrected the dropped branch's rows ***"
    );
    db.execute("USE BRANCH main").unwrap();
}

#[test]
fn branch_ids_are_never_reused_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let old_id = {
        let db = EmbeddedDatabase::new(dir.path()).unwrap();
        db.execute("CREATE BRANCH r AS OF NOW").unwrap();
        let id = db.storage.get_branch("r").unwrap().branch_id;
        db.execute("DROP BRANCH r").unwrap();
        id
    };
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    assert!(
        !db.storage.active_branch_exists("r"),
        "a dropped branch stays dropped across a restart"
    );
    db.execute("CREATE BRANCH r AS OF NOW").unwrap();
    let new_id = db.storage.get_branch("r").unwrap().branch_id;
    assert!(new_id > old_id, "id {new_id} reused (old {old_id})");
}

/// A merged branch is not active either: a session bound to it fails closed,
/// and it cannot be bound again — while the system views keep listing it.
#[test]
fn a_session_bound_to_a_merged_branch_fails_closed() {
    let db = seeded();
    db.execute("CREATE BRANCH m AS OF NOW").unwrap();
    let bound = session(&db, "merged");
    db.execute_for_session(bound, "USE BRANCH m").unwrap();
    db.execute("MERGE BRANCH m INTO main").unwrap();

    let err = session_rows(&db, bound, "SELECT v FROM t").expect_err("a merged branch must fail closed");
    assert!(err.to_string().contains(BRANCH_BINDING_GONE_MARKER), "got: {err}");
    let err = db.execute("USE BRANCH m").expect_err("a merged branch cannot be bound");
    assert!(err.to_string().contains(BRANCH_NOT_ACTIVE_MARKER), "got: {err}");
    let listed: Vec<String> = db
        .query("SHOW BRANCHES", &[])
        .unwrap()
        .iter()
        .map(|row| text(row, 0))
        .collect();
    assert!(
        listed.contains(&"m".to_string()),
        "merged branches stay listed: {listed:?}"
    );
}

/// The wire codes: 3D000 for a branch that is gone or unknown, 55006 for
/// dropping the connection's own branch.
#[tokio::test]
async fn dropped_and_in_use_branches_carry_postgresql_sqlstates() {
    let s = serve(seeded()).await;
    let (a, ta) = connect(s.port, "postgres").await;
    let (c, tc) = connect(s.port, "postgres").await;

    a.simple_query("USE BRANCH b").await.unwrap();
    let refused = a.simple_query("DROP BRANCH b").await.err().expect("own branch");
    assert_eq!(sqlstate(&refused), "55006");

    c.simple_query("DROP BRANCH b").await.unwrap();
    let refused = a.simple_query("SELECT v FROM t").await.err().expect("dropped binding");
    assert_eq!(sqlstate(&refused), "3D000");
    let refused = a
        .execute("INSERT INTO t VALUES ($1, $2)", &[&70i32, &"leak"])
        .await
        .err()
        .expect("dropped binding, extended protocol");
    assert_eq!(sqlstate(&refused), "3D000");
    let refused = a.simple_query("USE BRANCH b").await.err().expect("dropped branch");
    assert_eq!(sqlstate(&refused), "3D000");
    assert_eq!(handle_v(&s.db, 70), None, "nothing reached main");

    a.simple_query("USE BRANCH main").await.unwrap();
    assert_eq!(wire_v(&a, 1).await.as_deref(), Some("main1"));

    drop((a, c));
    ta.abort();
    tc.abort();
}

// ---------------------------------------------------------------------------
// CREATE BRANCH without PARENT, REPL
// ---------------------------------------------------------------------------

#[test]
fn create_branch_without_parent_forks_the_sessions_branch() {
    let db = seeded();
    let on_b = session(&db, "parent_b");
    let on_main = session(&db, "parent_main");
    db.execute_for_session(on_b, "USE BRANCH b").unwrap();

    db.execute_for_session(on_b, "CREATE BRANCH child_of_b AS OF NOW")
        .unwrap();
    db.execute_for_session(on_main, "CREATE BRANCH child_of_main AS OF NOW")
        .unwrap();

    let b_id = db.storage.get_branch("b").unwrap().branch_id;
    let main_id = db.storage.get_branch("main").unwrap().branch_id;
    assert_eq!(db.storage.get_branch("child_of_b").unwrap().parent_id, Some(b_id));
    assert_eq!(
        db.storage.get_branch("child_of_main").unwrap().parent_id,
        Some(main_id),
        "*** a main session's CREATE BRANCH forked another session's branch ***"
    );
}

#[test]
fn repl_use_of_a_branch_that_is_not_active_is_refused() {
    use heliosdb_nano::repl::{MetaCommand, MetaCommandResult};
    let db = seeded();
    db.execute("CREATE BRANCH gone AS OF NOW").unwrap();
    db.execute("DROP BRANCH gone").unwrap();

    for name in ["nosuch", "gone"] {
        let command = MetaCommand::parse(&format!("\\use {name}")).expect("\\use parses");
        let result = command.execute(&db, false, None).unwrap();
        assert!(
            matches!(result, MetaCommandResult::Continue),
            "\\use {name} must be refused, not switched"
        );
        assert_eq!(db.current_branch(), "main");
    }

    let command = MetaCommand::parse("\\use b").expect("\\use parses");
    let result = command.execute(&db, false, None).unwrap();
    assert!(matches!(result, MetaCommandResult::SwitchBranch(ref name) if name == "b"));
    assert_eq!(db.current_branch(), "b", "\\use b binds the REPL's session");
    assert_eq!(handle_v(&db, 1).as_deref(), Some("b1"));
}

// ---------------------------------------------------------------------------
// REST
// ---------------------------------------------------------------------------

fn rest_state(db: Arc<EmbeddedDatabase>) -> heliosdb_nano::api::server::AppState {
    heliosdb_nano::api::server::AppState {
        db,
        query_registry: Arc::new(heliosdb_nano::compute::QueryRegistry::new()),
        auth_bridge: None,
        oauth_registry: None,
        change_notifier: None,
    }
}

async fn rest_v(state: heliosdb_nano::api::server::AppState, branch: &str, id: i32) -> Option<String> {
    use axum::extract::{Path, State};
    use axum::Json;
    let request = heliosdb_nano::api::models::query::QueryRequest {
        sql: format!("SELECT v FROM t WHERE id = {id}"),
        params: None,
        as_of: None,
        timeout_ms: None,
    };
    use heliosdb_nano::api::handlers::query_handler::execute_query;
    let Json(response) = execute_query(State(state), Path(branch.to_string()), Json(request))
        .await
        .ok()
        .unwrap_or_else(|| panic!("REST query on {branch} failed"));
    response
        .rows
        .first()
        .and_then(|row| row.values().next().cloned())
        .and_then(|value| value.as_str().map(str::to_string))
}

/// 40 concurrent requests alternating `/v1/branches/b/query` and
/// `/v1/branches/main/query` each read their own branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rest_branch_routes_run_on_their_branch_under_concurrency() {
    let db = Arc::new(seeded());
    let state = rest_state(Arc::clone(&db));
    let mut tasks = Vec::new();
    for i in 0..40 {
        let state = state.clone();
        tasks.push(tokio::spawn(async move {
            let branch = if i % 2 == 0 { "b" } else { "main" };
            let got = rest_v(state, branch, 1).await;
            (branch, got)
        }));
    }
    for task in tasks {
        let (branch, got) = task.await.unwrap();
        let expected = if branch == "b" { "b1" } else { "main1" };
        assert_eq!(
            got.as_deref(),
            Some(expected),
            "*** /v1/branches/{branch}/query read the wrong branch ***"
        );
    }
    assert_eq!(db.current_branch(), "main", "a REST request must not move the handle");
}

#[tokio::test]
async fn rest_write_through_a_branch_route_lands_on_the_branch_only() {
    use axum::extract::{Path, State};
    use axum::Json;
    let db = Arc::new(seeded());
    let state = rest_state(Arc::clone(&db));
    let request = heliosdb_nano::api::models::query::ExecuteRequest {
        sql: "INSERT INTO t VALUES (20, 'rest_b')".to_string(),
        params: None,
        timeout_ms: None,
    };
    heliosdb_nano::api::handlers::query_handler::execute_statement(
        State(state.clone()),
        Path("b".to_string()),
        Json(request),
    )
    .await
    .ok()
    .expect("the branch write must succeed");

    assert_eq!(
        handle_v(&db, 20),
        None,
        "*** a write through /v1/branches/b landed on main ***"
    );
    assert_eq!(rest_v(state.clone(), "b", 20).await.as_deref(), Some("rest_b"));
    assert_eq!(rest_v(state.clone(), "main", 20).await, None);

    // A branch that is not active is a 404, not a run on main.
    let request = heliosdb_nano::api::models::query::QueryRequest {
        sql: "SELECT v FROM t".to_string(),
        params: None,
        as_of: None,
        timeout_ms: None,
    };
    let refused = heliosdb_nano::api::handlers::query_handler::execute_query(
        State(state),
        Path("nosuch".to_string()),
        Json(request),
    )
    .await;
    assert!(refused.is_err(), "an unknown branch route must be refused");
}

// ---------------------------------------------------------------------------
// B1 fix pass
// ---------------------------------------------------------------------------

/// `SELECT COUNT(*)` on a branch counts the rows the BRANCH sees (main's two,
/// one updated, plus its branch-only row), not main's.
#[test]
fn count_star_on_a_branch_counts_the_branch() {
    let db = seeded();
    let count = |db: &EmbeddedDatabase| -> i64 {
        match db.query("SELECT COUNT(*) FROM t", &[]).unwrap()[0].values[0] {
            Value::Int8(n) => n,
            ref other => panic!("count: {other:?}"),
        }
    };
    assert_eq!(count(&db), 2, "main has two rows");
    db.execute("USE BRANCH b").unwrap();
    assert_eq!(count(&db), 3, "*** COUNT(*) on the branch answered main's count ***");
    let on_b = session(&db, "count_b");
    db.execute_for_session(on_b, "USE BRANCH b").unwrap();
    let rows = session_rows(&db, on_b, "SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(rows[0].values[0], Value::Int8(3));
    db.execute("USE BRANCH main").unwrap();
    assert_eq!(count(&db), 2);
}

/// SQL-level PREPARE / EXECUTE: a statement PREPAREd on main and EXECUTEd on a
/// branch reads the branch — an inherited row, a branch-only row, and an
/// updated one — and the reverse.
#[test]
fn sql_prepare_execute_reads_the_branch_at_execute() {
    let db = seeded();
    db.query("PREPARE pq AS SELECT v FROM t WHERE id = $1", &[]).unwrap();
    let execute = |db: &EmbeddedDatabase, id: i32| -> Option<String> {
        db.query(&format!("EXECUTE pq({id})"), &[])
            .unwrap()
            .first()
            .map(|row| text(row, 0))
    };
    assert_eq!(execute(&db, 1).as_deref(), Some("main1"));
    db.execute("USE BRANCH b").unwrap();
    assert_eq!(execute(&db, 1).as_deref(), Some("b1"), "updated on the branch");
    assert_eq!(execute(&db, 2).as_deref(), Some("main2"), "inherited, no overlay key");
    assert_eq!(execute(&db, 3).as_deref(), Some("b3"), "branch-only");
    db.execute("USE BRANCH main").unwrap();
    assert_eq!(execute(&db, 1).as_deref(), Some("main1"));
    assert_eq!(execute(&db, 3), None);
}

/// sprinter 6bc4a4040eb7 (phase B2 — B1 refused these with 0A000): an
/// explicit transaction on a branch reads the branch and its own staged
/// writes, ROLLBACK discards them, COMMIT lands them on the branch only, and
/// changing the branch inside it is still refused (25001). Embedded handle
/// and embedded session.
#[test]
fn explicit_transactions_on_a_branch_work() {
    let db = seeded();
    db.execute("USE BRANCH b").unwrap();
    db.execute("BEGIN").unwrap();
    assert_eq!(
        handle_v(&db, 1).as_deref(),
        Some("b1"),
        "a read in a branch txn reads the branch"
    );
    db.execute("INSERT INTO t VALUES (30, 'txn')").unwrap();
    assert_eq!(
        handle_v(&db, 30).as_deref(),
        Some("txn"),
        "read-your-own-writes on a branch"
    );
    let err = db.execute("USE BRANCH main").expect_err("switch inside a txn");
    assert!(
        err.to_string().contains(BRANCH_SWITCH_IN_TRANSACTION_MARKER),
        "got: {err}"
    );
    db.execute("ROLLBACK").unwrap();
    assert_eq!(handle_v(&db, 30), None, "ROLLBACK discarded the branch write");
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (33, 'kept')").unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(
        handle_v(&db, 33).as_deref(),
        Some("kept"),
        "COMMIT landed it on the branch"
    );
    db.execute("USE BRANCH main").unwrap();
    assert_eq!(handle_v(&db, 33), None, "*** a branch COMMIT landed on main ***");

    let sid = session(&db, "txn_b");
    db.execute_for_session(sid, "USE BRANCH b").unwrap();
    db.begin_transaction_for_session(sid).unwrap();
    assert_eq!(session_v(&db, sid, 1).as_deref(), Some("b1"));
    let params = [Value::Int4(31), Value::String("p".into())];
    db.execute_params_for_session(sid, "INSERT INTO t VALUES ($1, $2)", &params)
        .expect("params write in a session branch txn");
    assert_eq!(session_v(&db, sid, 31).as_deref(), Some("p"));
    let err = db
        .execute_for_session(sid, "SET heliosdb.branch = 'main'")
        .expect_err("switch inside a session txn");
    assert!(
        err.to_string().contains(BRANCH_SWITCH_IN_TRANSACTION_MARKER),
        "got: {err}"
    );
    db.rollback_transaction_for_session(sid).unwrap();
    assert_eq!(session_v(&db, sid, 31), None, "the session ROLLBACK discarded it");
    assert_eq!(session_v(&db, sid, 1).as_deref(), Some("b1"));

    // A transaction on MAIN is untouched by any of this.
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (32, 'main txn')").unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(handle_v(&db, 32).as_deref(), Some("main txn"));
}

/// The same over the PostgreSQL wire: a driver-style transaction (`BEGIN`
/// first, as psycopg / JDBC autocommit-off do) with the extended protocol, and
/// a multi-statement simple query, whose implicit block is a transaction too —
/// atomic on a branch (a failing statement takes the whole block back).
#[tokio::test]
async fn explicit_transactions_on_a_branch_work_on_the_wire() {
    let s = serve(seeded()).await;
    let (mut a, ta) = connect(s.port, "postgres").await;
    a.simple_query("USE BRANCH b").await.unwrap();

    {
        let tx = a.transaction().await.unwrap();
        let rows = tx.query("SELECT v FROM t WHERE id = $1", &[&1i32]).await.unwrap();
        assert_eq!(rows.first().map(|r| r.get::<_, String>(0)).as_deref(), Some("b1"));
        tx.execute("INSERT INTO t VALUES ($1, $2)", &[&42i32, &"wire txn"])
            .await
            .unwrap();
        let rows = tx.query("SELECT v FROM t WHERE id = $1", &[&42i32]).await.unwrap();
        assert_eq!(rows.len(), 1, "read-your-own-writes over the extended protocol");
        tx.rollback().await.unwrap();
    }
    assert_eq!(wire_v(&a, 42).await, None, "the wire ROLLBACK discarded it");

    a.simple_query("BEGIN").await.unwrap();
    let refused = a.simple_query("USE BRANCH main").await.err().expect("switch in a txn");
    assert_eq!(sqlstate(&refused), "25001");
    a.simple_query("ROLLBACK").await.unwrap();

    a.simple_query("INSERT INTO t VALUES (40, 'x'); INSERT INTO t VALUES (41, 'y')")
        .await
        .expect("an implicit block on a branch");
    assert_eq!(wire_v(&a, 40).await.as_deref(), Some("x"));
    assert_eq!(wire_v(&a, 41).await.as_deref(), Some("y"));
    let refused = a
        .simple_query("INSERT INTO t VALUES (43, 'z'); INSERT INTO t VALUES (40, 'dup')")
        .await
        .err()
        .expect("a duplicate key inside an implicit block on a branch");
    assert_eq!(sqlstate(&refused), "23505");
    assert_eq!(
        wire_v(&a, 43).await,
        None,
        "the failed implicit block took its first row back"
    );

    let (main, tm) = connect(s.port, "postgres").await;
    assert_eq!(wire_v(&main, 40).await, None, "*** a branch block landed on main ***");

    drop((a, main));
    ta.abort();
    tm.abort();
}

/// A transaction whose branch is merged (or dropped) by another session while
/// it is open: its COMMIT rolls back and is refused (3D000), on the embedded
/// session API and on the PostgreSQL wire — so a staged branch write is reported
/// lost, never committed into an overlay nobody can use, and never on main.
/// (B3 second fix pass: a MERGE refuses, 55006, while the transaction HAS staged
/// writes: they would land after the merge validated its keys. A DROP does not.)
#[tokio::test]
async fn a_commit_across_a_concurrent_merge_or_drop_is_refused() {
    let db = seeded();
    db.execute("CREATE BRANCH c AS OF NOW").unwrap();
    let s1 = session(&db, "commit_b");
    let s2 = session(&db, "merger");
    db.execute_for_session(s1, "USE BRANCH b").unwrap();
    db.begin_transaction_for_session(s1).unwrap();
    db.execute_for_session(s1, "INSERT INTO t VALUES (50, 'in txn')")
        .expect("a write inside a branch transaction");
    // Spec R5.1: it holds the table on the source's level, and the merge waits for it.
    db.storage
        .debug_set_table_lock_timeout(std::time::Duration::from_millis(50));
    let refused = db
        .execute_for_session(s2, "MERGE BRANCH b INTO main")
        .expect_err("the transaction has staged writes on the source");
    assert!(
        refused.to_string().contains(heliosdb_nano::TABLE_LOCK_TIMEOUT_MARKER),
        "got: {refused}"
    );
    db.rollback_transaction_for_session(s1).unwrap();
    db.begin_transaction_for_session(s1).unwrap();
    db.execute_for_session(s2, "MERGE BRANCH b INTO main").unwrap();
    let err = db
        .commit_transaction_for_session(s1)
        .expect_err("COMMIT on a merged branch must be refused");
    assert!(err.to_string().contains(BRANCH_BINDING_GONE_MARKER), "got: {err}");
    assert!(
        !db.session_in_transaction(s1),
        "the refused COMMIT rolled the transaction back"
    );
    assert_eq!(handle_v(&db, 50), None);

    // The wire: a connection on c, in a transaction, while c is dropped.
    let s = serve(db).await;
    let (a, ta) = connect(s.port, "postgres").await;
    let (other, tb) = connect(s.port, "postgres").await;
    a.simple_query("USE BRANCH c").await.unwrap();
    a.simple_query("BEGIN").await.unwrap();
    other.simple_query("DROP BRANCH c").await.unwrap();
    let refused = a
        .simple_query("COMMIT")
        .await
        .err()
        .expect("COMMIT on a dropped branch");
    assert_eq!(sqlstate(&refused), "3D000");
    // The transaction is gone; the session is still (fail-closed) bound to c.
    let refused = a.simple_query("SELECT 1 FROM t").await.err().expect("still bound to c");
    assert_eq!(sqlstate(&refused), "3D000");
    a.simple_query("USE BRANCH main").await.unwrap();

    drop((a, other));
    ta.abort();
    tb.abort();
}

/// Rows of `t` written to branch `branch_id`'s overlay, read straight from the
/// store — the branch itself may be gone.
fn overlay_rows(db: &EmbeddedDatabase, branch_id: u64) -> usize {
    let prefix = format!("bdata:{branch_id}:t:");
    let raw = db.storage.db();
    raw.iterator(rocksdb::IteratorMode::From(
        prefix.as_bytes(),
        rocksdb::Direction::Forward,
    ))
    .map_while(|item| item.ok())
    .take_while(|(key, _)| key.starts_with(prefix.as_bytes()))
    .count()
}

/// THE DRAIN: a session writing to branch `d` in a loop while another session
/// drops `d`. Every write that succeeded did so before the DROP returned, and
/// nothing is written to the dropped branch after it.
#[test]
fn nothing_is_written_to_a_branch_after_its_drop_returns() {
    let db = Arc::new(seeded());
    db.execute("CREATE BRANCH d AS OF NOW").unwrap();
    let branch_id = db.storage.get_branch("d").unwrap().branch_id;
    let writer = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            let sid = db
                .create_session("drain_writer", IsolationLevel::ReadCommitted)
                .unwrap();
            db.execute_for_session(sid, "USE BRANCH d").unwrap();
            let mut written = 0usize;
            for id in 1000..200_000 {
                match db.execute_for_session(sid, &format!("INSERT INTO t VALUES ({id}, 'd')")) {
                    Ok(_) => written += 1,
                    // B1 second fix pass: refused at once (55006) while the
                    // DROP is closing the branch — retry, it is not gone yet.
                    Err(e) if e.to_string().contains(BRANCH_CLOSING_MARKER) => std::thread::yield_now(),
                    Err(e) => {
                        assert!(e.to_string().contains(BRANCH_BINDING_GONE_MARKER), "got: {e}");
                        return written;
                    }
                }
            }
            panic!("the writer never saw its branch dropped");
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    db.execute("DROP BRANCH d").unwrap();
    let at_drop = overlay_rows(&db, branch_id);
    let written = writer.join().unwrap();
    assert_eq!(
        overlay_rows(&db, branch_id),
        at_drop,
        "*** a statement bound to the dropped branch wrote after the DROP returned ***"
    );
    assert_eq!(at_drop, written, "every acknowledged write landed before the drop");
}

/// The same for MERGE: the source is drained and marked merged BEFORE its rows
/// are copied, so every write that was acknowledged on the source is in the
/// target afterwards — none written into an overlay that had already been read.
#[test]
fn a_merge_never_loses_a_concurrent_branch_write() {
    let db = Arc::new(seeded());
    db.execute("CREATE BRANCH m2 AS OF NOW").unwrap();
    let writer = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            let sid = db
                .create_session("merge_writer", IsolationLevel::ReadCommitted)
                .unwrap();
            db.execute_for_session(sid, "USE BRANCH m2").unwrap();
            let mut acknowledged = Vec::new();
            for id in 3000..200_000 {
                match db.execute_for_session(sid, &format!("INSERT INTO t VALUES ({id}, 'm2')")) {
                    Ok(_) => acknowledged.push(id),
                    // B1 second fix pass: refused at once (55006) while the
                    // MERGE is closing the branch — retry.
                    Err(e) if e.to_string().contains(BRANCH_CLOSING_MARKER) => std::thread::yield_now(),
                    Err(e) => {
                        assert!(e.to_string().contains(BRANCH_BINDING_GONE_MARKER), "got: {e}");
                        return acknowledged;
                    }
                }
            }
            panic!("the writer never saw its branch merged");
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    db.execute("MERGE BRANCH m2 INTO main").unwrap();
    let acknowledged = writer.join().unwrap();
    let on_main: std::collections::HashSet<i32> = db
        .query("SELECT id FROM t", &[])
        .unwrap()
        .iter()
        .map(|row| match row.values[0] {
            Value::Int4(n) => n,
            ref other => panic!("id: {other:?}"),
        })
        .collect();
    let lost: Vec<i32> = acknowledged.into_iter().filter(|id| !on_main.contains(id)).collect();
    assert!(
        lost.is_empty(),
        "*** acknowledged branch writes lost by the merge: {lost:?} ***"
    );
}

/// Two embedded sessions switching branches concurrently never see each
/// other's binding.
#[test]
fn concurrent_sessions_switching_branches_never_cross() {
    let db = Arc::new(seeded());
    let worker = |name: &'static str, first: &'static str, second: &'static str| {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            let sid = db.create_session(name, IsolationLevel::ReadCommitted).unwrap();
            for _ in 0..150 {
                for branch in [first, second] {
                    db.execute_for_session(sid, &format!("USE BRANCH {branch}")).unwrap();
                    let expected = if branch == "b" { "b1" } else { "main1" };
                    assert_eq!(
                        session_v(&db, sid, 1).as_deref(),
                        Some(expected),
                        "*** a session read another session's branch ***"
                    );
                }
            }
        })
    };
    let one = worker("switch_one", "b", "main");
    let two = worker("switch_two", "main", "b");
    one.join().unwrap();
    two.join().unwrap();
}

/// The embedded handle's own session is shared by every thread that uses the
/// handle. A statement snapshots its branch ONCE, so a `switch_branch` racing
/// it cannot split it: every row lands wholly on one branch — never a row in
/// the branch overlay with its index entry on main.
#[test]
fn a_concurrent_switch_never_splits_a_statement() {
    let db = Arc::new(seeded());
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, code INT)").unwrap();
    db.execute("CREATE INDEX s_code ON s (code)").unwrap();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let switcher = {
        let db = Arc::clone(&db);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = db.switch_branch("b");
                let _ = db.switch_branch("main");
            }
        })
    };
    for id in 0..300 {
        db.execute(&format!("INSERT INTO s VALUES ({id}, {})", id + 10_000))
            .unwrap();
    }
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    switcher.join().unwrap();
    db.switch_branch("main").unwrap();

    let on_main: std::collections::HashSet<i32> = db
        .query("SELECT id FROM s", &[])
        .unwrap()
        .iter()
        .map(|row| match row.values[0] {
            Value::Int4(n) => n,
            ref other => panic!("id: {other:?}"),
        })
        .collect();
    for id in 0..300 {
        let probed = db
            .query(&format!("SELECT id FROM s WHERE code = {}", id + 10_000), &[])
            .unwrap()
            .len();
        assert_eq!(
            probed,
            usize::from(on_main.contains(&id)),
            "*** row {id}: the index on main disagrees with main's rows — one statement was split ***"
        );
    }
}

/// `DISCARD ALL` and `RESET ALL` return a connection to the branch it was
/// OPENED on — by id: once that branch is dropped (even if re-created under
/// the same name) the connection fails closed instead of binding a different
/// branch, and the reset itself still succeeds.
#[tokio::test]
async fn resets_return_to_the_startup_branch_by_id() {
    let s = serve(seeded()).await;
    let (a, ta) = connect(s.port, "postgres@b").await;
    let (other, tb) = connect(s.port, "postgres").await;

    a.simple_query("USE BRANCH main").await.unwrap();
    a.simple_query("DISCARD ALL").await.unwrap();
    assert_eq!(simple_col(&a, "SHOW heliosdb.branch").await, vec!["b"]);
    a.simple_query("USE BRANCH main").await.unwrap();
    a.simple_query("RESET ALL").await.unwrap();
    assert_eq!(
        simple_col(&a, "SHOW heliosdb.branch").await,
        vec!["b"],
        "RESET ALL resets the branch"
    );
    a.simple_query("USE BRANCH main").await.unwrap();

    other.simple_query("DROP BRANCH b").await.unwrap();
    other.simple_query("CREATE BRANCH b AS OF NOW").await.unwrap();
    a.simple_query("DISCARD ALL")
        .await
        .expect("the reset itself must not fail");
    let refused = a
        .simple_query("SELECT v FROM t")
        .await
        .err()
        .expect("the home branch is gone: fail closed, never the new b");
    assert_eq!(sqlstate(&refused), "3D000");

    drop((a, other));
    ta.abort();
    tb.abort();
}

/// A tenant whose name contains `@` can still select a branch; a value two
/// different splits resolve is ambiguous and refused.
#[tokio::test]
async fn startup_split_accepts_exactly_one_resolving_split() {
    let db = seeded();
    db.tenant_manager
        .register_tenant("a@b".to_string(), IsolationMode::DatabasePerTenant);
    db.execute("CREATE BRANCH dev AS OF NOW").unwrap();
    db.execute("USE BRANCH dev").unwrap();
    db.execute("UPDATE t SET v = 'dev1' WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();
    // Ambiguity: tenant `x` + branch `y@z`, and tenant `x@y` + branch `z`.
    db.tenant_manager
        .register_tenant("x".to_string(), IsolationMode::DatabasePerTenant);
    db.tenant_manager
        .register_tenant("x@y".to_string(), IsolationMode::DatabasePerTenant);
    db.execute("CREATE BRANCH \"y@z\" AS OF NOW").unwrap();
    db.execute("CREATE BRANCH z AS OF NOW").unwrap();
    let s = serve(db).await;

    let (c, tc) = connect(s.port, "a@b@dev").await;
    assert_eq!(tenant_connections(&s.db, "a@b"), 1, "the tenant a@b is bound");
    assert_eq!(wire_v(&c, 1).await.as_deref(), Some("dev1"), "and the branch dev");

    let refused = connect_with(s.port, "dbname=x@y@z")
        .await
        .err()
        .expect("two resolving splits are ambiguous");
    assert_eq!(sqlstate(&refused), "3D000");

    drop(c);
    tc.abort();
}

/// The BaaS routes (`/rest/v1/…`) are not branch routes: they run on MAIN even
/// while an in-process caller has moved the handle's own session.
#[test]
fn baas_routes_run_on_main_whatever_the_handle_is_on() {
    let db = Arc::new(seeded());
    db.switch_branch("b").unwrap();
    let executor = heliosdb_nano::api::rest_executor::RestExecutor::new(Arc::clone(&db));
    let (rows, _columns) = executor.select("t", "id,v", &[], None, None, None).unwrap();
    assert_eq!(rows.len(), 2, "*** a BaaS route ran on the handle's branch ***");
    assert_eq!(db.current_branch(), "b", "and the handle's own session is left alone");
}

// ---------------------------------------------------------------------------
// B1 second fix pass
// ---------------------------------------------------------------------------

/// Poll `condition` (at most 20 s) — the deterministic tests wait on drain
/// STATE, never on a sleep.
fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Item 1: the REST merge route copies the branch's TABLE rows into main. It
/// used to call `BranchManager::merge_branch`, whose key format SQL never
/// writes: 0 rows merged, success reported. A strategy the last-writer-wins
/// merge cannot honour is refused instead of ignored.
#[tokio::test]
async fn rest_merge_route_copies_the_branch_rows_into_main() {
    use axum::extract::{Path, State};
    use axum::Json;
    use heliosdb_nano::api::handlers::branch_handler::merge_branch;
    use heliosdb_nano::api::handlers::query_handler::execute_statement;
    use heliosdb_nano::api::models::branch::MergeStrategyDto;
    use heliosdb_nano::api::models::MergeBranchRequest;

    let db = Arc::new(seeded());
    db.execute("CREATE BRANCH rm AS OF NOW").unwrap();
    let state = rest_state(Arc::clone(&db));
    for sql in [
        "INSERT INTO t VALUES (70, 'rest_merged')",
        "UPDATE t SET v = 'rest_updated' WHERE id = 2",
    ] {
        let request = heliosdb_nano::api::models::query::ExecuteRequest {
            sql: sql.to_string(),
            params: None,
            timeout_ms: None,
        };
        execute_statement(State(state.clone()), Path("rm".to_string()), Json(request))
            .await
            .ok()
            .unwrap_or_else(|| panic!("REST write on rm: {sql}"));
    }
    assert_eq!(handle_v(&db, 70), None, "not on main before the merge");

    let refused = merge_branch(
        State(state.clone()),
        Path("rm".to_string()),
        Json(MergeBranchRequest {
            target: "main".to_string(),
            strategy: MergeStrategyDto::Ours,
        }),
    )
    .await;
    assert!(refused.is_err(), "a target-wins merge cannot be honoured: refused");
    assert!(
        db.storage.active_branch_exists("rm"),
        "and the refused merge changed nothing"
    );

    let Json(merged) = merge_branch(
        State(state.clone()),
        Path("rm".to_string()),
        Json(MergeBranchRequest {
            target: "main".to_string(),
            strategy: MergeStrategyDto::Auto,
        }),
    )
    .await
    .ok()
    .expect("the REST merge must succeed");
    assert!(merged.completed);
    assert!(merged.merged_keys >= 2, "merged {} keys", merged.merged_keys);
    assert_eq!(
        handle_v(&db, 70).as_deref(),
        Some("rest_merged"),
        "*** the REST merge route did not copy the branch's inserted row ***"
    );
    assert_eq!(
        handle_v(&db, 2).as_deref(),
        Some("rest_updated"),
        "*** the REST merge route did not copy the branch's updated row ***"
    );
    assert_eq!(
        handle_v(&db, 1).as_deref(),
        Some("main1"),
        "rows the branch never touched stay"
    );
    assert!(!db.storage.active_branch_exists("rm"), "the source is merged");
}

/// A minimal frame-level PostgreSQL client — `COPY … FROM STDIN` (simple
/// protocol only; the shape of `tests/security_hdb_004.rs`).
mod raw_wire {
    use bytes::{BufMut, BytesMut};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn put_cstr(buf: &mut BytesMut, value: &str) {
        buf.extend_from_slice(value.as_bytes());
        buf.put_u8(0);
    }

    fn message(tag: u8, body: &[u8]) -> BytesMut {
        let mut msg = BytesMut::new();
        msg.put_u8(tag);
        msg.put_i32((body.len() + 4) as i32);
        msg.extend_from_slice(body);
        msg
    }

    async fn read_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).await.unwrap();
        let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut body = vec![0u8; len - 4];
        stream.read_exact(&mut body).await.unwrap();
        (header[0], body)
    }

    pub async fn connect(port: u16) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut body = BytesMut::new();
        body.put_i32(196_608);
        put_cstr(&mut body, "user");
        put_cstr(&mut body, "postgres");
        put_cstr(&mut body, "database");
        put_cstr(&mut body, "postgres");
        body.put_u8(0);
        let mut startup = BytesMut::new();
        startup.put_i32((body.len() + 4) as i32);
        startup.extend_from_slice(&body);
        stream.write_all(&startup).await.unwrap();
        while read_frame(&mut stream).await.0 != b'Z' {}
        stream
    }

    /// Run `sql` as one `Q`; a CopyInResponse is answered with `copy_rows`
    /// and CopyDone. The ErrorResponse's message, if any.
    pub async fn query(stream: &mut TcpStream, sql: &str, copy_rows: &[&str]) -> Option<String> {
        let mut body = BytesMut::new();
        put_cstr(&mut body, sql);
        stream.write_all(&message(b'Q', &body)).await.unwrap();
        let mut error = None;
        loop {
            let (tag, body) = read_frame(stream).await;
            match tag {
                b'G' => {
                    let mut out = BytesMut::new();
                    for row in copy_rows {
                        out.extend_from_slice(&message(b'd', row.as_bytes()));
                    }
                    out.extend_from_slice(&message(b'c', &[]));
                    stream.write_all(&out).await.unwrap();
                }
                b'E' => error = Some(String::from_utf8_lossy(&body).to_string()),
                b'Z' => return error,
                _ => {}
            }
        }
    }
}

/// Item 4: `COPY … FROM STDIN` on a branch lands on the branch only — and,
/// since B2, is atomic there as on main: one bad row (a duplicate of an
/// inherited key) takes the whole COPY back. (B1 had to autocommit it chunk by
/// chunk while transactions on a branch were refused.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_from_stdin_on_a_branch_lands_on_the_branch() {
    let s = serve(seeded()).await;
    let mut wire = raw_wire::connect(s.port).await;
    assert_eq!(raw_wire::query(&mut wire, "USE BRANCH b", &[]).await, None);
    let failed = raw_wire::query(&mut wire, "COPY t FROM STDIN", &["60\tcopied_a\n", "61\tcopied_b\n"]).await;
    assert_eq!(failed, None, "*** COPY FROM STDIN on a branch failed ***");

    let on_b = session(&s.db, "copy_reader");
    s.db.execute_for_session(on_b, "USE BRANCH b").unwrap();
    assert_eq!(session_v(&s.db, on_b, 60).as_deref(), Some("copied_a"));
    assert_eq!(session_v(&s.db, on_b, 61).as_deref(), Some("copied_b"));
    assert_eq!(handle_v(&s.db, 60), None, "*** COPY on a branch landed on main ***");

    // Atomic: row 2 is inherited from main, so it is a duplicate. The COPY
    // handler applies rows in 500-row chunks; the duplicate sits in the THIRD
    // chunk (after 1,100 good rows), so two whole chunks were applied before
    // it and must be taken back with it (B2 fix pass: a 2-row COPY could not
    // tell an atomic COPY from a chunk-by-chunk one).
    let mut lines: Vec<String> = (1000..2200).map(|id| format!("{id}\tcopied_{id}\n")).collect();
    lines.insert(1100, "2\tdup\n".to_string());
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let failed = raw_wire::query(&mut wire, "COPY t FROM STDIN", &refs).await;
    assert!(failed.is_some(), "a COPY with a duplicate key on a branch must fail");
    for id in [1000, 1499, 1500, 1999] {
        assert_eq!(
            session_v(&s.db, on_b, id),
            None,
            "*** a failed COPY on a branch left row {id} (an earlier chunk) behind ***"
        );
    }
    assert_eq!(
        session_v(&s.db, on_b, 2).as_deref(),
        Some("main2"),
        "the inherited row is untouched"
    );
}

/// Item 4: `execute_batch` on a branch is ONE transaction on the branch again
/// (B2; B1 autocommitted it statement by statement): it lands on the branch
/// only, and a failing statement takes the earlier ones back.
#[test]
fn execute_batch_on_a_branch_is_atomic_on_the_branch() {
    let db = seeded();
    db.execute("USE BRANCH b").unwrap();
    db.execute_batch(&[
        "INSERT INTO t VALUES (80, 'batch_a')",
        "UPDATE t SET v = 'batch_b' WHERE id = 3",
    ])
    .expect("a batch on a branch");
    assert_eq!(handle_v(&db, 80).as_deref(), Some("batch_a"));
    assert_eq!(handle_v(&db, 3).as_deref(), Some("batch_b"));

    let refused = db
        .execute_batch(&["INSERT INTO t VALUES (81, 'first')", "INSERT INTO t VALUES (80, 'dup')"])
        .expect_err("a duplicate key in a batch on a branch");
    assert!(
        refused.to_string().to_ascii_lowercase().contains("duplicate key"),
        "got: {refused}"
    );
    assert_eq!(
        handle_v(&db, 81),
        None,
        "*** a failed batch on a branch left its first row ***"
    );

    db.execute("USE BRANCH main").unwrap();
    assert_eq!(handle_v(&db, 80), None, "*** a batch on a branch landed on main ***");
}

/// Item 6: the handle's SHARED session cannot change branch while ANY
/// transaction is open on it — another thread's `begin()`, or a RAII
/// `Transaction` — not only the calling thread's own.
#[test]
#[allow(deprecated)]
fn the_shared_sessions_branch_is_pinned_while_a_transaction_is_open_on_it() {
    let db = Arc::new(seeded());

    let tx = db.begin_transaction().unwrap();
    let refused = db.switch_branch("b").expect_err("a RAII transaction is open");
    assert!(
        refused.to_string().contains(BRANCH_SWITCH_IN_TRANSACTION_MARKER),
        "got: {refused}"
    );
    let refused = db.execute("USE BRANCH b").expect_err("a RAII transaction is open");
    assert!(
        refused.to_string().contains(BRANCH_SWITCH_IN_TRANSACTION_MARKER),
        "got: {refused}"
    );
    tx.rollback().unwrap();
    db.switch_branch("b").expect("no transaction open any more");
    db.switch_branch("main").unwrap();

    db.begin().unwrap();
    let other_thread = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || db.switch_branch("b").err().map(|e| e.to_string()))
    };
    let refused = other_thread
        .join()
        .unwrap()
        .expect("*** another thread moved the session under an open transaction ***");
    assert!(refused.contains(BRANCH_SWITCH_IN_TRANSACTION_MARKER), "got: {refused}");
    db.rollback().unwrap();
    assert_eq!(db.current_branch(), "main");
}

/// Item 6: `commit()` (the API, not only the SQL `COMMIT`) and a RAII
/// `Transaction::commit` re-validate the branch captured at BEGIN: a
/// transaction whose branch was dropped meanwhile rolls back with 3D000.
#[test]
#[allow(deprecated)]
fn the_commit_api_refuses_a_transaction_whose_branch_was_dropped() {
    let db = seeded();
    let dropper = session(&db, "commit_api_dropper");

    db.execute("CREATE BRANCH c1 AS OF NOW").unwrap();
    db.switch_branch("c1").unwrap();
    db.begin().unwrap();
    db.execute_for_session(dropper, "DROP BRANCH c1").unwrap();
    let refused = db.commit().expect_err("*** commit() committed on a dropped branch ***");
    assert!(
        refused.to_string().contains(BRANCH_BINDING_GONE_MARKER),
        "got: {refused}"
    );
    assert!(!db.in_transaction(), "the refused commit rolled back");
    db.switch_branch("main").unwrap();

    db.execute("CREATE BRANCH c2 AS OF NOW").unwrap();
    db.switch_branch("c2").unwrap();
    let tx = db.begin_transaction().unwrap();
    db.execute_for_session(dropper, "DROP BRANCH c2").unwrap();
    let refused = tx
        .commit()
        .expect_err("*** a RAII commit committed on a dropped branch ***");
    assert!(
        refused.to_string().contains(BRANCH_BINDING_GONE_MARKER),
        "got: {refused}"
    );
    db.switch_branch("main").unwrap();
}

/// Item 2, race (b), at the engine: a statement running on `d` (its drain
/// hold, taken here on the test thread) creates a child of `d` while a DROP of
/// `d` waits for it. The DROP re-reads `d` once the statement is done and
/// refuses — it used to drop the parent of a live branch. A create from
/// ANOTHER thread while the DROP is closing `d` is refused at once (55006).
#[test]
fn a_child_created_while_a_drop_waits_makes_the_drop_refuse() {
    use heliosdb_nano::storage::{BranchOptions, BranchState};
    let db = seeded();
    db.execute("CREATE BRANCH d AS OF NOW").unwrap();
    let engine = Arc::clone(&db.storage);
    let d_id = engine.get_branch("d").unwrap().branch_id;
    let drains = Arc::clone(engine.branch_manager().unwrap().branch_drains());

    let hold = drains.enter(d_id, "d").unwrap();
    let dropper = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || engine.drop_branch("d", false))
    };
    wait_until("the DROP to start closing d", || {
        engine.branch_drain_state(d_id).closing
    });

    let elsewhere = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || engine.create_branch("c_elsewhere", Some("d"), BranchOptions::default()))
    };
    let refused = elsewhere.join().unwrap().expect_err("a create from a closing branch");
    assert!(refused.to_string().contains(BRANCH_CLOSING_MARKER), "got: {refused}");

    // The statement holding d forks a child of it (its own hold covers it).
    engine
        .create_branch("c_child", Some("d"), BranchOptions::default())
        .expect("the running statement's create");
    assert!(!dropper.is_finished(), "the DROP still waits for the statement");
    drop(hold);

    let refused = dropper
        .join()
        .unwrap()
        .expect_err("*** DROP dropped the parent of a branch created while it waited ***");
    assert!(refused.to_string().contains("child"), "got: {refused}");
    assert!(matches!(engine.get_branch("d").unwrap().state, BranchState::Active));
    let child = engine.get_branch("c_child").unwrap();
    assert!(matches!(child.state, BranchState::Active));
    assert_eq!(child.parent_id, Some(d_id));
}

/// Item 2, race (d): two DROPs of one branch. The second queues behind the
/// first (counted as a waiting closer), re-reads the branch when the first is
/// done, finds it dropped, and does nothing.
#[test]
fn a_drop_queued_behind_another_drop_finds_the_branch_gone() {
    let db = seeded();
    db.execute("CREATE BRANCH d2 AS OF NOW").unwrap();
    let engine = Arc::clone(&db.storage);
    let id = engine.get_branch("d2").unwrap().branch_id;
    let drains = Arc::clone(engine.branch_manager().unwrap().branch_drains());

    let hold = drains.enter(id, "d2").unwrap();
    let first = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || engine.drop_branch("d2", false))
    };
    wait_until("the first DROP to close d2", || engine.branch_drain_state(id).closing);
    let second = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || engine.drop_branch("d2", false))
    };
    wait_until("the second DROP to queue", || {
        engine.branch_drain_state(id).waiting_closers == 1
    });
    drop(hold);
    first.join().unwrap().expect("the first DROP drops d2");
    let refused = second.join().unwrap().expect_err("the second finds d2 already dropped");
    assert!(refused.to_string().contains("not found"), "got: {refused}");
}

/// Item 5: a code-graph `… ON BRANCH 'b'` inside a transaction opened on
/// another branch is refused — a transaction belongs to one branch, so this is
/// `USE BRANCH` inside a transaction (25001). (B1 refused it 0A000 because the
/// in-transaction read path answered from main.)
#[cfg(feature = "code-graph")]
#[test]
fn on_branch_inside_a_transaction_is_refused() {
    let db = seeded();
    db.begin().unwrap();
    let refused = db
        .query("SELECT * FROM lsp_definition('x') ON BRANCH 'b'", &[])
        .expect_err("ON BRANCH inside a transaction");
    assert!(
        refused.to_string().contains(BRANCH_SWITCH_IN_TRANSACTION_MARKER),
        "got: {refused}"
    );
    db.rollback().unwrap();
}

/// Item 9: the deterministic drain tests. `heliosdb_test_hold()`
/// (`internal-tests` builds) keeps a statement INSIDE the engine — holding its
/// branch's drain — while the file named by
/// `HELIOSDB_NANO_TEST_HOLD_STATEMENT_FILE` exists. The variable is process
/// state, so these tests are serialized.
#[cfg(feature = "internal-tests")]
mod held {
    use super::*;

    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The hold file: present from `arm` until `release` (or drop).
    struct Hold {
        path: std::path::PathBuf,
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    impl Hold {
        fn arm() -> Self {
            let serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let path = std::env::temp_dir().join(format!("heliosdb_b1_hold_{}", std::process::id()));
            std::fs::write(&path, b"hold").unwrap();
            std::env::set_var("HELIOSDB_NANO_TEST_HOLD_STATEMENT_FILE", &path);
            Self { path, _serial: serial }
        }

        fn release(&self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            self.release();
        }
    }

    const HELD_INSERT: &str =
        "INSERT INTO t VALUES (5000, CASE WHEN heliosdb_test_hold() THEN 'held' ELSE 'not held' END)";

    /// A statement running on `d`, held inside the engine. Returns the branch
    /// id once the statement provably holds `d`'s drain.
    fn hold_a_statement_on_d(db: &Arc<EmbeddedDatabase>) -> (u64, std::thread::JoinHandle<heliosdb_nano::Result<u64>>) {
        db.execute("CREATE BRANCH d AS OF NOW").unwrap();
        let d_id = db.storage.get_branch("d").unwrap().branch_id;
        let writer = session(db, "held_writer");
        db.execute_for_session(writer, "USE BRANCH d").unwrap();
        let held = {
            let db = Arc::clone(db);
            std::thread::spawn(move || db.execute_for_session(writer, HELD_INSERT))
        };
        wait_until("the held statement to hold d", || {
            db.storage.branch_drain_state(d_id).in_flight == 1
        });
        (d_id, held)
    }

    /// DROP waits for the statement in flight (it does not return while the
    /// statement is held), a NEW statement on `d` during the close is refused
    /// immediately (55006) instead of queueing, and the held statement's row
    /// was written before the DROP returned.
    #[test]
    fn a_drop_waits_for_the_statement_in_flight_and_refuses_newcomers() {
        let hold = Hold::arm();
        let db = Arc::new(seeded());
        let probe = session(&db, "probe");
        let (d_id, held) = hold_a_statement_on_d(&db);
        db.execute_for_session(probe, "USE BRANCH d").unwrap();

        let dropper = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || db.execute("DROP BRANCH d"))
        };
        wait_until("the DROP to start closing d", || {
            db.storage.branch_drain_state(d_id).closing
        });

        let asked = std::time::Instant::now();
        let refused =
            session_rows(&db, probe, "SELECT v FROM t WHERE id = 1").expect_err("a new statement on a closing branch");
        assert!(refused.to_string().contains(BRANCH_CLOSING_MARKER), "got: {refused}");
        assert!(
            asked.elapsed() < Duration::from_secs(5),
            "the newcomer was refused at once, not queued behind the close"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !dropper.is_finished(),
            "*** DROP BRANCH returned while a statement was still running on the branch ***"
        );

        hold.release();
        assert_eq!(held.join().unwrap().expect("the held INSERT completes"), 1);
        dropper
            .join()
            .unwrap()
            .expect("the DROP completes once the statement is done");
        assert_eq!(
            overlay_rows(&db, d_id),
            1,
            "the held statement's row was written before the DROP returned"
        );
        let gone = session_rows(&db, probe, "SELECT v FROM t").expect_err("d is dropped");
        assert!(gone.to_string().contains(BRANCH_BINDING_GONE_MARKER), "got: {gone}");
    }

    /// Item 2, race (a): a MERGE of `d` arriving while a DROP of `d` waits.
    /// The MERGE queues behind the DROP's close, re-reads `d` when it is done,
    /// finds it dropped and refuses — it used to proceed on its stale read and
    /// mark the dropped branch merged.
    #[test]
    fn a_merge_into_a_branch_waits_for_the_statements_on_it() {
        let hold = Hold::arm();
        let db = Arc::new(seeded());
        db.execute("CREATE BRANCH donor AS OF NOW").unwrap();
        db.execute("USE BRANCH donor").unwrap();
        db.execute("INSERT INTO t VALUES (6000, 'donor')").unwrap();
        db.execute("USE BRANCH main").unwrap();
        let (d_id, held) = hold_a_statement_on_d(&db);

        let merge = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || db.execute("MERGE BRANCH donor INTO d"))
        };
        // Its section is claimed, and it waits for the statement to leave: it
        // cannot finish while that statement is held, so this is no race.
        wait_until("the merge to claim d's exclusive section", || {
            db.storage.branch_drain_state(d_id).exclusive
        });
        assert!(!merge.is_finished(), "the merge runs on d alone");
        hold.release();
        held.join().unwrap().expect("the held INSERT completes");
        merge.join().unwrap().expect("the merge then runs");
    }

    /// A statement that changes nothing takes no exclusive section: it neither
    /// waits for the statement running on `d`, nor holds anyone else up.
    #[test]
    fn a_noop_schema_statement_takes_no_section() {
        let hold = Hold::arm();
        let db = Arc::new(seeded());
        let probe = session(&db, "probe");
        let (_d_id, held) = hold_a_statement_on_d(&db);
        db.execute_for_session(probe, "USE BRANCH d").unwrap();
        db.execute_for_session(probe, "CREATE TABLE IF NOT EXISTS t (other INT)")
            .expect("a no-op, at once");
        db.execute_for_session(probe, "DROP TABLE IF EXISTS never_was")
            .expect("a no-op, at once");
        db.execute_for_session(probe, "ALTER TABLE t ADD COLUMN IF NOT EXISTS v TEXT")
            .expect("a no-op, at once");
        hold.release();
        held.join().unwrap().expect("the held INSERT completes");
    }

    #[test]
    fn a_merge_arriving_while_a_drop_waits_is_refused() {
        let hold = Hold::arm();
        let db = Arc::new(seeded());
        let merger = session(&db, "merger");
        let (d_id, held) = hold_a_statement_on_d(&db);

        let dropper = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || db.execute("DROP BRANCH d"))
        };
        wait_until("the DROP to start closing d", || {
            db.storage.branch_drain_state(d_id).closing
        });
        let merge = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || db.execute_for_session(merger, "MERGE BRANCH d INTO main"))
        };
        wait_until("the MERGE to queue behind the DROP", || {
            db.storage.branch_drain_state(d_id).waiting_closers == 1
        });

        hold.release();
        held.join().unwrap().expect("the held INSERT completes");
        dropper.join().unwrap().expect("the DROP wins");
        merge
            .join()
            .unwrap()
            .expect_err("*** a MERGE of a branch dropped while it waited went ahead ***");
        assert!(
            matches!(
                db.storage.get_branch("d").unwrap().state,
                heliosdb_nano::storage::BranchState::Dropped { .. }
            ),
            "d stays dropped"
        );
        assert_eq!(
            handle_v(&db, 5000),
            None,
            "*** the dropped branch's row was merged into main ***"
        );
    }
}

// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

#[cfg(feature = "mcp-endpoint")]
fn mcp_v(db: &EmbeddedDatabase, branch: &str) -> String {
    let outcome = heliosdb_nano::mcp::call_tool(
        Some(db),
        "heliosdb_query",
        serde_json::json!({ "sql": "SELECT v FROM t WHERE id = 1", "branch": branch }),
    );
    assert!(!outcome.is_error, "heliosdb_query on {branch}: {:?}", outcome.payload);
    outcome.payload["rows"][0][0].as_str().unwrap_or_default().to_string()
}

/// A branch-scoped tool call reads the branch while a concurrent main call
/// reads main — the old switch-and-restore made them read each other's.
#[cfg(feature = "mcp-endpoint")]
#[test]
fn mcp_branch_scoped_calls_never_cross_concurrent_calls() {
    let db = Arc::new(seeded());
    let on_branch = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            for _ in 0..200 {
                assert_eq!(mcp_v(&db, "b"), "b1", "*** a branch call read main ***");
            }
        })
    };
    let on_main = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            for _ in 0..200 {
                assert_eq!(mcp_v(&db, "main"), "main1", "*** a main call read the branch ***");
            }
        })
    };
    on_branch.join().unwrap();
    on_main.join().unwrap();
    assert_eq!(db.current_branch(), "main", "an MCP call must not move the handle");
}

#[cfg(feature = "mcp-endpoint")]
#[test]
fn mcp_branch_drop_drops_and_refuses_main() {
    let db = seeded();
    db.execute("CREATE BRANCH dropme AS OF NOW").unwrap();

    let outcome =
        heliosdb_nano::mcp::call_tool(Some(&db), "heliosdb_branch_drop", serde_json::json!({ "name": "main" }));
    assert!(outcome.is_error, "heliosdb_branch_drop must refuse main");
    assert!(db.storage.active_branch_exists("main"));

    let outcome = heliosdb_nano::mcp::call_tool(
        Some(&db),
        "heliosdb_branch_drop",
        serde_json::json!({ "name": "dropme" }),
    );
    assert!(!outcome.is_error, "heliosdb_branch_drop: {:?}", outcome.payload);
    assert!(!db.storage.active_branch_exists("dropme"), "the branch must be dropped");

    let names: Vec<String> = heliosdb_nano::mcp::list_tools()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(names.contains(&"heliosdb_branch_drop".to_string()));
}
