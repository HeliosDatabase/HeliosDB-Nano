//! **Who is inside which transaction.**
//!
//! Two sprinter items, one theme, one suite:
//!
//! * `0d6695bf8a86` — session-less params / prepared statements arriving from the
//!   wire, REST and MCP join the **process-global embedded transaction**. One
//!   caller's `BEGIN` silently adopted every other caller of the same handle:
//!   their writes staged into it (and died with its `ROLLBACK`), their reads saw
//!   its uncommitted rows, and — since HDB-008 — their failures aborted it.
//! * `e4bb83a1afa0` — `CALL` inside a transaction ran its procedure body
//!   **outside** that transaction. `BEGIN; CALL p(); ROLLBACK;` left the
//!   procedure's writes committed while the client was told the block rolled
//!   back. (ROADMAP_V5 §2.11's "transaction semantics still open" residual.)
//!
//! ## The ownership model these tests pin
//!
//! The process-global `current_transaction` slot is the **embedded handle's
//! implicit connection**. A statement may resolve it only when BOTH hold:
//!
//! 1. **it runs on the thread that opened it** — `BEGIN` records the caller's
//!    thread token in `global_txn_owner`; a statement on any other thread sees
//!    no global transaction and runs autocommit; and
//! 2. **it is not session-bound** — a statement that entered through any
//!    `*_for_session` funnel belongs to that connection's session-transaction
//!    slot and never sees the global slot, even on the owner's own thread.
//!
//! Clause 2 is not redundant. A `#[tokio::test]` runtime is single-threaded, so
//! the wire handler below runs on the very thread that opened the embedded
//! `BEGIN` — which is exactly the shape a `current_thread` async server has in
//! production. Thread ownership alone does not save it; the session gate does.
//!
//! Transaction CONTROL on the global slot is owner-only too: a non-owner's
//! `COMMIT` / `ROLLBACK` must not touch the owner's transaction.
//!
//! And for `CALL`: a procedure body **always runs in its caller's transaction**
//! — the session's, the embedded global one, the RAII handle's, or, in
//! autocommit, the statement's own implicit transaction. Both executor families,
//! identically.
//!
//! ## How to maintain this file
//!
//! Every test asserts unconditionally. Never assert on `SELECT COUNT(*)` (one
//! row comes back whether the count is 0 or 10,000) — read the whole id set.
//! The `control_*` tests are the other half of the proof: they fail loudly if
//! the leak was "fixed" by severing something real.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::time::timeout;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

// ---------------------------------------------------------------- helpers --

fn int_of(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Int2(v)) => i64::from(*v),
        Some(Value::Int4(v)) => i64::from(*v),
        Some(Value::Int8(v)) => *v,
        other => panic!("unexpected integer value: {other:?}"),
    }
}

/// Every `id` physically in `table`, ascending — a full row set, never a count.
///
/// Read with `query_raw_unnormalized`-free plain `query()` on a handle with no
/// transaction open, so it reports COMMITTED state only.
fn ids(db: &EmbeddedDatabase, table: &str) -> Vec<i64> {
    db.query(&format!("SELECT id FROM {table} ORDER BY id"), &[])
        .expect("select ids")
        .iter()
        .map(|row| int_of(row.values.first()))
        .collect()
}

fn t_db(table: &str) -> Arc<EmbeddedDatabase> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("in-memory db"));
    db.execute(&format!("CREATE TABLE {table} (id integer PRIMARY KEY)"))
        .expect("create table");
    db
}

/// Run `f` against the same handle on a DIFFERENT thread and wait for it.
///
/// This is the REST/MCP shape: an axum handler or an MCP tool call reaches the
/// very same `Arc<EmbeddedDatabase>` an embedded caller is holding a `BEGIN`
/// open on, from a worker thread of its own.
fn on_another_thread<T, F>(db: &Arc<EmbeddedDatabase>, f: F) -> T
where
    T: Send + 'static,
    F: FnOnce(&EmbeddedDatabase) -> T + Send + 'static,
{
    let handle = Arc::clone(db);
    std::thread::spawn(move || f(&handle))
        .join()
        .expect("worker thread panicked")
}

/// `call_audit` plus the procedures the `CALL` half invokes.
///
/// Same shapes as `tests/call_parity_tests.rs` so the two suites cannot drift.
fn call_setup(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE call_audit (id integer, note text)")
        .expect("create call_audit");
    db.execute("CREATE TABLE call_src (id integer)")
        .expect("create call_src");
    db.execute("CREATE PROCEDURE p_zero() LANGUAGE sql AS $$INSERT INTO call_audit VALUES (0, 'zero')$$")
        .expect("CREATE PROCEDURE p_zero");
    db.execute(
        "CREATE PROCEDURE p_one(p_id integer) LANGUAGE sql \
         AS $$INSERT INTO call_audit VALUES ($p_id, 'one')$$",
    )
    .expect("CREATE PROCEDURE p_one");
    db.execute(
        "CREATE PROCEDURE p_pg(p_id integer) LANGUAGE plpgsql \
         AS $$BEGIN INSERT INTO call_audit VALUES ($p_id, 'pg'); END$$",
    )
    .expect("CREATE PROCEDURE p_pg");
    // Reads the caller's table and writes what it finds: only a body that joined
    // the caller's transaction can see rows that transaction has not committed.
    db.execute(
        "CREATE PROCEDURE p_copy() LANGUAGE sql \
         AS $$INSERT INTO call_audit SELECT id, 'copy' FROM call_src$$",
    )
    .expect("CREATE PROCEDURE p_copy");
}

/// Every `(id, note)` physically in `call_audit`, in insertion order.
fn audit(db: &EmbeddedDatabase) -> Vec<(i64, String)> {
    db.query("SELECT id, note FROM call_audit", &[])
        .expect("audit read")
        .iter()
        .map(|row| match row.values.get(1) {
            Some(Value::String(note)) => (int_of(row.values.first()), note.clone()),
            other => panic!("unexpected call_audit row shape: {other:?}"),
        })
        .collect()
}

// ===========================================================================
// PART 1 — 0d6695bf8a86: a session-less caller must not join, observe, commit
//          or lose work to another caller's open transaction.
// ===========================================================================

/// **The write direction.** A REST/MCP-shaped params write must not be staged
/// into an unrelated embedded `BEGIN`, and therefore must not be destroyed by
/// that caller's `ROLLBACK`.
///
/// FAILS on the unfixed tree: `execute_plan_with_params_inner`'s `Insert` arm
/// resolves the GLOBAL `current_transaction` slot whenever `session_txn` is
/// `None` (`src/lib.rs`, the `_txn_guard = Some(self.current_transaction.lock())`
/// sites), so row 2 staged into the embedded transaction and vanished with it.
#[test]
fn a_session_less_params_write_is_not_rolled_back_by_someone_elses_rollback() {
    let db = t_db("t1");

    db.execute("BEGIN").expect("embedded BEGIN");
    db.execute("INSERT INTO t1 VALUES (1)").expect("embedded insert");

    let written = on_another_thread(&db, |handle| {
        handle.execute_params("INSERT INTO t1 VALUES ($1)", &[Value::Int4(2)])
    })
    .expect("a session-less params write must succeed on its own");
    assert_eq!(written, 1, "the session-less write reported no row");

    db.execute("ROLLBACK").expect("embedded ROLLBACK");

    assert_eq!(
        ids(&db, "t1"),
        vec![2],
        "*** a session-less params write joined an unrelated embedded transaction \
         and was discarded by its ROLLBACK ***"
    );
}

/// **The read direction.** A session-less params read must not see rows an
/// unrelated caller has staged but not committed.
#[test]
fn a_session_less_params_read_cannot_see_another_callers_uncommitted_rows() {
    let db = t_db("t2");

    db.execute("BEGIN").expect("embedded BEGIN");
    db.execute("INSERT INTO t2 VALUES (1)").expect("embedded insert");

    let seen = on_another_thread(&db, |handle| {
        handle
            .query_params("SELECT id FROM t2 ORDER BY id", &[])
            .expect("session-less read")
            .iter()
            .map(|row| int_of(row.values.first()))
            .collect::<Vec<_>>()
    });

    db.execute("ROLLBACK").expect("embedded ROLLBACK");

    assert_eq!(
        seen,
        Vec::<i64>::new(),
        "*** a session-less read observed another caller's uncommitted rows ***"
    );
}

/// The same leak on the TEXT funnel — `db.execute()` is what
/// `execute_for_session` delegates to for a wire autocommit statement, and what
/// the REST `/execute` endpoint calls directly.
#[test]
fn a_session_less_text_write_is_not_rolled_back_by_someone_elses_rollback() {
    let db = t_db("t3");

    db.execute("BEGIN").expect("embedded BEGIN");
    db.execute("INSERT INTO t3 VALUES (1)").expect("embedded insert");

    on_another_thread(&db, |handle| handle.execute("INSERT INTO t3 VALUES (2)"))
        .expect("a session-less text write must succeed on its own");

    db.execute("ROLLBACK").expect("embedded ROLLBACK");

    assert_eq!(
        ids(&db, "t3"),
        vec![2],
        "*** a session-less text write joined an unrelated embedded transaction ***"
    );
}

/// **Nobody else's `ROLLBACK`.** A session-less caller must not be able to end
/// a transaction it did not open — the sharpest form of the cross-caller
/// hazard, because it destroys work the owner believes is still in flight.
#[test]
fn a_session_less_caller_cannot_roll_back_another_callers_transaction() {
    let db = t_db("t4");

    db.execute("BEGIN").expect("embedded BEGIN");
    db.execute("INSERT INTO t4 VALUES (1)").expect("embedded insert");

    let stolen = on_another_thread(&db, |handle| handle.execute("ROLLBACK").map_err(|e| e.to_string()));
    assert!(
        stolen.is_err(),
        "*** an unrelated caller rolled back somebody else's transaction ***"
    );

    let stolen_commit = on_another_thread(&db, |handle| handle.execute("COMMIT").map_err(|e| e.to_string()));
    assert!(
        stolen_commit.is_err(),
        "*** an unrelated caller committed somebody else's transaction ***"
    );

    db.execute("COMMIT").expect("the owner's COMMIT must still work");
    assert_eq!(ids(&db, "t4"), vec![1], "the owner's work must have survived");
}

/// The failure of a session-less statement must not abort a transaction it was
/// never part of (the HDB-008 half of the same leak: since aborted
/// transactions refuse every later statement with 25P02, one REST error could
/// wedge an embedded caller's whole block).
#[test]
fn a_session_less_failure_does_not_abort_another_callers_transaction() {
    let db = t_db("t5");

    db.execute("BEGIN").expect("embedded BEGIN");
    db.execute("INSERT INTO t5 VALUES (1)").expect("embedded insert");

    let failed = on_another_thread(&db, |handle| {
        handle
            .execute_params("INSERT INTO t5 VALUES ($1)", &[Value::String("not an integer".into())])
            .map_err(|e| e.to_string())
    });
    assert!(failed.is_err(), "the session-less statement was supposed to fail");

    db.execute("INSERT INTO t5 VALUES (3)")
        .expect("*** an unrelated caller's failure aborted this transaction ***");
    db.execute("COMMIT").expect("COMMIT after an unrelated failure");

    assert_eq!(ids(&db, "t5"), vec![1, 3]);
}

// ---- controls: the legitimate cases must keep working, loudly -------------

/// **CONTROL.** The embedded handle's OWN transaction — the whole reason the
/// global slot exists. Reads and writes on both executor families must stay
/// inside it, see its own uncommitted rows, and be discarded by its `ROLLBACK`.
///
/// If this test fails, the leak was closed by severing something real.
#[test]
fn control_an_embedded_callers_own_transaction_still_works() {
    let db = t_db("c1");

    db.execute("BEGIN").expect("BEGIN");
    db.execute("INSERT INTO c1 VALUES (1)").expect("text insert");
    db.execute_params("INSERT INTO c1 VALUES ($1)", &[Value::Int4(2)])
        .expect("params insert");

    let text_read = db
        .query("SELECT id FROM c1 ORDER BY id", &[])
        .expect("text read inside the transaction")
        .iter()
        .map(|row| int_of(row.values.first()))
        .collect::<Vec<_>>();
    assert_eq!(text_read, vec![1, 2], "read-your-own-writes, text funnel");

    let params_read = db
        .query_params("SELECT id FROM c1 ORDER BY id", &[])
        .expect("params read inside the transaction")
        .iter()
        .map(|row| int_of(row.values.first()))
        .collect::<Vec<_>>();
    assert_eq!(params_read, vec![1, 2], "read-your-own-writes, params funnel");

    db.execute("ROLLBACK").expect("ROLLBACK");
    assert_eq!(
        ids(&db, "c1"),
        Vec::<i64>::new(),
        "the owner's own ROLLBACK must still discard both families' writes"
    );
}

/// **CONTROL.** The committed half of the same statement.
#[test]
fn control_an_embedded_callers_own_transaction_still_commits() {
    let db = t_db("c2");

    db.execute("BEGIN").expect("BEGIN");
    db.execute("INSERT INTO c2 VALUES (1)").expect("text insert");
    db.execute_params("INSERT INTO c2 VALUES ($1)", &[Value::Int4(2)])
        .expect("params insert");
    db.execute("COMMIT").expect("COMMIT");

    assert_eq!(ids(&db, "c2"), vec![1, 2]);
}

/// **CONTROL.** A wire session's own `BEGIN … ROLLBACK` (the per-session slot)
/// is untouched by any of this.
#[test]
fn control_a_wire_sessions_own_transaction_still_works() {
    let db = t_db("c3");
    let session = db.create_session("alice", IsolationLevel::Snapshot).expect("session");

    db.begin_transaction_for_session(session).expect("session BEGIN");
    db.execute_for_session(session, "INSERT INTO c3 VALUES (1)")
        .expect("session text insert");
    db.execute_params_for_session(session, "INSERT INTO c3 VALUES ($1)", &[Value::Int4(2)])
        .expect("session params insert");

    let seen = db
        .query_params_for_session(session, "SELECT id FROM c3 ORDER BY id", &[])
        .expect("session read")
        .iter()
        .map(|row| int_of(row.values.first()))
        .collect::<Vec<_>>();
    assert_eq!(seen, vec![1, 2], "a session must still read its own writes");

    db.rollback_transaction_for_session(session).expect("session ROLLBACK");
    assert_eq!(ids(&db, "c3"), Vec::<i64>::new());
    db.destroy_session(session).expect("destroy session");
}

// ---- the session gate, on the owner's own thread --------------------------

/// A wire session's AUTOCOMMIT statement delegates into the session-LESS funnel
/// (`execute_for_session` → `execute()` when the session holds no transaction).
/// It must still not join the embedded global transaction — and here it runs on
/// the very thread that opened it, so only the session gate can stop it.
#[test]
fn a_session_autocommit_statement_on_the_owners_thread_does_not_join_the_global_slot() {
    let db = t_db("t6");
    let session = db.create_session("bob", IsolationLevel::Snapshot).expect("session");

    db.execute("BEGIN").expect("embedded BEGIN");
    db.execute("INSERT INTO t6 VALUES (1)").expect("embedded insert");

    // Same thread as the BEGIN above — thread ownership cannot help here.
    db.execute_for_session(session, "INSERT INTO t6 VALUES (2)")
        .expect("session autocommit text write");
    db.execute_params_for_session(session, "INSERT INTO t6 VALUES ($1)", &[Value::Int4(3)])
        .expect("session autocommit params write");

    let seen = db
        .query_params_for_session(session, "SELECT id FROM t6 ORDER BY id", &[])
        .expect("session autocommit read")
        .iter()
        .map(|row| int_of(row.values.first()))
        .collect::<Vec<_>>();
    assert_eq!(
        seen,
        vec![2, 3],
        "*** a session autocommit read saw the embedded transaction's uncommitted row ***"
    );

    db.execute("ROLLBACK").expect("embedded ROLLBACK");

    assert_eq!(
        ids(&db, "t6"),
        vec![2, 3],
        "*** session autocommit writes were staged into the embedded transaction \
         and discarded by its ROLLBACK ***"
    );
    db.destroy_session(session).expect("destroy session");
}

/// `ROLLBACK TO SAVEPOINT` is deliberately EXCLUDED from `is_transaction_control`
/// (a savepoint is not a block boundary — see `handle_transaction_control_for_session`),
/// so it is one of the very few statements a wire session sends that still
/// delegates into the session-LESS funnel when the session holds no transaction.
/// It therefore reached the embedded global slot's savepoint stack — and could
/// unwind an unrelated caller's transaction to a savepoint that caller took.
///
/// `tests/savepoint_scoping_i5.rs::the_global_slot_and_a_wire_session_do_not_share_a_stack`
/// only escapes this because it opens the session transaction FIRST; this is the
/// variant with no session transaction, which is the reachable shape.
#[test]
fn a_session_without_a_transaction_cannot_reach_the_global_slots_savepoint_stack() {
    let db = t_db("t7");
    let session = db.create_session("erin", IsolationLevel::Snapshot).expect("session");

    db.execute("BEGIN").expect("embedded BEGIN");
    db.execute("INSERT INTO t7 VALUES (1)").expect("before the savepoint");
    db.execute_returning("SAVEPOINT global_sp").expect("global SAVEPOINT");
    db.execute("INSERT INTO t7 VALUES (2)").expect("after the savepoint");

    // This session has no transaction of its own, so both families delegate.
    assert!(
        db.execute_for_session(session, "ROLLBACK TO SAVEPOINT global_sp")
            .is_err(),
        "*** a session reached the embedded global slot's savepoint stack (text family) ***"
    );
    assert!(
        db.execute_params_for_session(session, "ROLLBACK TO SAVEPOINT global_sp", &[])
            .is_err(),
        "*** a session reached the embedded global slot's savepoint stack (params family) ***"
    );

    db.execute("COMMIT").expect("embedded COMMIT");
    assert_eq!(
        ids(&db, "t7"),
        vec![1, 2],
        "*** an unrelated caller unwound the embedded transaction to its savepoint ***"
    );
    db.destroy_session(session).expect("destroy session");
}

// ---- the same thing over a real wire --------------------------------------

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Serve `db` on an ephemeral loopback port (no hardcoded test port) and return
/// the connection string.
async fn serve(db: Arc<EmbeddedDatabase>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr = listener.local_addr().expect("test addr");
    drop(listener);

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

async fn connect(conn_string: &str) -> Client {
    let (client, connection) = timeout(CONNECT_TIMEOUT, tokio_postgres::connect(conn_string, NoTls))
        .await
        .expect("connect timeout")
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn simple_ok(client: &Client, sql: &str) -> Vec<SimpleQueryMessage> {
    timeout(QUERY_TIMEOUT, client.simple_query(sql))
        .await
        .unwrap_or_else(|_| panic!("query timeout: {sql}"))
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e}"))
}

async fn wire_ids(client: &Client, table: &str) -> Vec<String> {
    simple_ok(client, &format!("SELECT id FROM {table} ORDER BY id"))
        .await
        .into_iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
            _ => None,
        })
        .collect()
}

/// The reported shape, over the PostgreSQL wire, on the SAME `EmbeddedDatabase`
/// the embedded caller holds a `BEGIN` open on.
///
/// `#[tokio::test]` is a current-thread runtime, so the wire handler runs on the
/// same OS thread as the embedded `BEGIN` below — a faithful model of a
/// single-threaded async server, and the case thread ownership alone misses.
#[tokio::test]
async fn pg_wire_autocommit_does_not_join_an_embedded_begin() {
    let db = t_db("w1");
    let (conn_string, server) = serve(Arc::clone(&db)).await;
    let client = connect(&conn_string).await;

    db.execute("BEGIN").expect("embedded BEGIN");
    db.execute("INSERT INTO w1 VALUES (1)").expect("embedded insert");

    // Simple query (text family) and extended protocol (params family).
    simple_ok(&client, "INSERT INTO w1 VALUES (2)").await;
    timeout(QUERY_TIMEOUT, client.execute("INSERT INTO w1 VALUES ($1)", &[&3i32]))
        .await
        .expect("extended insert timeout")
        .expect("extended insert");

    assert_eq!(
        wire_ids(&client, "w1").await,
        vec!["2".to_string(), "3".to_string()],
        "*** a wire client read the embedded transaction's uncommitted row ***"
    );

    db.execute("ROLLBACK").expect("embedded ROLLBACK");

    assert_eq!(
        ids(&db, "w1"),
        vec![2, 3],
        "*** wire autocommit writes were discarded by an unrelated embedded ROLLBACK ***"
    );
    server.abort();
}

// ===========================================================================
// PART 2 — e4bb83a1afa0: a CALL body runs in its caller's transaction.
// ===========================================================================

/// The headline: `BEGIN; CALL p(); ROLLBACK;` must leave no trace.
///
/// FAILS on the unfixed tree in both families — the text family refuses the
/// `CALL` outright (`GLOBAL_TXN_LOCK_HELD`) and the params family runs the body
/// but through a fresh `execute()`, which autocommits it.
#[test]
fn text_family_call_inside_a_transaction_is_undone_by_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);

    db.execute("BEGIN").expect("BEGIN");
    let affected = db.execute("CALL p_zero()").expect("CALL must run inside the block");
    assert_eq!(affected, 0, "CALL's own command tag carries no row count");
    db.execute("ROLLBACK").expect("ROLLBACK");

    assert_eq!(
        audit(&db),
        Vec::<(i64, String)>::new(),
        "*** the procedure body's write survived the ROLLBACK of the calling transaction ***"
    );
}

#[test]
fn text_family_call_inside_a_transaction_persists_exactly_once_on_commit() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);

    db.execute("BEGIN").expect("BEGIN");
    db.execute("CALL p_zero()").expect("CALL");
    db.execute("COMMIT").expect("COMMIT");

    assert_eq!(
        audit(&db),
        vec![(0, "zero".to_string())],
        "the body must have run exactly once"
    );
}

#[test]
fn params_family_call_inside_a_transaction_is_undone_by_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);

    db.execute("BEGIN").expect("BEGIN");
    let affected = db.execute_params("CALL p_zero()", &[]).expect("CALL");
    assert_eq!(affected, 0);
    db.execute("ROLLBACK").expect("ROLLBACK");

    assert_eq!(
        audit(&db),
        Vec::<(i64, String)>::new(),
        "*** the procedure body's write survived the ROLLBACK (params family) ***"
    );
}

#[test]
fn params_family_call_inside_a_transaction_persists_exactly_once_on_commit() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);

    db.execute("BEGIN").expect("BEGIN");
    db.execute_params("CALL p_zero()", &[]).expect("CALL");
    db.execute("COMMIT").expect("COMMIT");

    assert_eq!(audit(&db), vec![(0, "zero".to_string())]);
}

/// A **bound** argument, inside a transaction, rolled back. This is the shape
/// every extended-protocol driver sends.
#[test]
fn bound_parameter_call_inside_a_transaction_is_undone_by_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);

    db.execute("BEGIN").expect("BEGIN");
    db.execute_params("CALL p_one($1)", &[Value::Int4(7)])
        .expect("CALL p_one($1)");
    db.execute("ROLLBACK").expect("ROLLBACK");
    assert_eq!(audit(&db), Vec::<(i64, String)>::new());

    db.execute("BEGIN").expect("BEGIN");
    db.execute_params("CALL p_one($1)", &[Value::Int4(7)])
        .expect("CALL p_one($1)");
    db.execute("COMMIT").expect("COMMIT");
    assert_eq!(audit(&db), vec![(7, "one".to_string())]);
}

/// `LANGUAGE plpgsql` bodies go through a different runtime; they must honour
/// the same rule.
#[test]
fn plpgsql_call_inside_a_transaction_is_undone_by_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);

    db.execute("BEGIN").expect("BEGIN");
    db.execute("CALL p_pg(5)").expect("CALL p_pg (text family)");
    db.execute_params("CALL p_pg($1)", &[Value::Int4(6)])
        .expect("CALL p_pg (params family)");
    db.execute("ROLLBACK").expect("ROLLBACK");

    assert_eq!(audit(&db), Vec::<(i64, String)>::new());
}

/// **It really joined.** A body that can read rows the calling transaction has
/// staged but not committed is inside that transaction — nothing else explains
/// the result.
#[test]
fn a_call_body_reads_the_calling_transactions_uncommitted_rows() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);

    db.execute("BEGIN").expect("BEGIN");
    db.execute("INSERT INTO call_src VALUES (42)")
        .expect("stage a source row");
    db.execute("CALL p_copy()").expect("CALL p_copy");
    db.execute("COMMIT").expect("COMMIT");

    assert_eq!(
        audit(&db),
        vec![(42, "copy".to_string())],
        "*** the procedure body could not see the calling transaction's own rows ***"
    );
}

/// The RAII handle is a third transaction owner, and it had the same hole.
#[test]
fn raii_transaction_handle_call_is_undone_by_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);

    let tx = db.begin_transaction().expect("begin_transaction");
    tx.execute("CALL p_zero()").expect("CALL on the RAII handle");
    tx.rollback().expect("rollback");

    assert_eq!(
        audit(&db),
        Vec::<(i64, String)>::new(),
        "*** a CALL through the RAII transaction handle autocommitted ***"
    );
}

/// A wire SESSION transaction — the population every driver is in.
#[test]
fn session_transaction_call_is_undone_by_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);
    let session = db.create_session("carol", IsolationLevel::Snapshot).expect("session");

    db.begin_transaction_for_session(session).expect("session BEGIN");
    db.execute_for_session(session, "CALL p_zero()")
        .expect("CALL, session text family");
    db.execute_params_for_session(session, "CALL p_one($1)", &[Value::Int4(9)])
        .expect("CALL, session params family");
    db.rollback_transaction_for_session(session).expect("session ROLLBACK");

    assert_eq!(
        audit(&db),
        Vec::<(i64, String)>::new(),
        "*** a CALL inside a wire session transaction autocommitted its body ***"
    );
    db.destroy_session(session).expect("destroy session");
}

#[test]
fn session_transaction_call_persists_exactly_once_on_commit() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);
    let session = db.create_session("dave", IsolationLevel::Snapshot).expect("session");

    db.begin_transaction_for_session(session).expect("session BEGIN");
    db.execute_for_session(session, "CALL p_zero()").expect("CALL");
    db.commit_transaction_for_session(session).expect("session COMMIT");

    assert_eq!(audit(&db), vec![(0, "zero".to_string())]);
    db.destroy_session(session).expect("destroy session");
}

/// **CONTROL.** `CALL` in autocommit must behave as it always has: the body
/// runs, and its writes are durable when the statement returns — once, in both
/// families.
#[test]
fn control_call_in_autocommit_behaves_as_before() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);

    assert_eq!(db.execute("CALL p_zero()").expect("text family CALL"), 0);
    assert_eq!(
        db.execute_params("CALL p_one($1)", &[Value::Int4(3)])
            .expect("params CALL"),
        0
    );

    assert_eq!(
        audit(&db),
        vec![(0, "zero".to_string()), (3, "one".to_string())],
        "each autocommit CALL must have run its body exactly once"
    );
}

/// **CONTROL.** Calling a procedure that does not exist must still fail —
/// inside a transaction as well as outside it — and write nothing.
#[test]
fn control_a_missing_procedure_still_errors_inside_a_transaction() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);

    db.execute("BEGIN").expect("BEGIN");
    let text = db
        .execute("CALL no_such_proc()")
        .expect_err("a missing procedure must error, text family")
        .to_string();
    db.execute("ROLLBACK").expect("ROLLBACK");
    assert!(
        text.contains("no_such_proc"),
        "the error must name the procedure: {text}"
    );

    db.execute("BEGIN").expect("BEGIN");
    let params = db
        .execute_params("CALL no_such_proc()", &[])
        .expect_err("a missing procedure must error, params family")
        .to_string();
    db.execute("ROLLBACK").expect("ROLLBACK");
    assert!(
        params.contains("no_such_proc"),
        "the error must name the procedure: {params}"
    );

    assert_eq!(audit(&db), Vec::<(i64, String)>::new());
}

// ---- CALL over both wires -------------------------------------------------

/// `BEGIN; CALL p(); ROLLBACK;` over the PostgreSQL wire — simple query (text
/// family) and extended protocol (params family), in one connection each.
#[tokio::test]
async fn pg_wire_call_inside_a_transaction_is_undone_by_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);
    let (conn_string, server) = serve(Arc::clone(&db)).await;

    let simple = connect(&conn_string).await;
    simple_ok(&simple, "BEGIN").await;
    simple_ok(&simple, "CALL p_zero()").await;
    simple_ok(&simple, "ROLLBACK").await;
    assert_eq!(
        audit(&db),
        Vec::<(i64, String)>::new(),
        "*** PG simple-query CALL autocommitted its body inside a transaction ***"
    );

    let extended = connect(&conn_string).await;
    simple_ok(&extended, "BEGIN").await;
    timeout(QUERY_TIMEOUT, extended.execute("CALL p_zero()", &[]))
        .await
        .expect("extended CALL timeout")
        .expect("extended CALL");
    simple_ok(&extended, "ROLLBACK").await;
    assert_eq!(
        audit(&db),
        Vec::<(i64, String)>::new(),
        "*** PG extended-protocol CALL autocommitted its body inside a transaction ***"
    );

    simple_ok(&simple, "BEGIN").await;
    simple_ok(&simple, "CALL p_zero()").await;
    simple_ok(&simple, "COMMIT").await;
    assert_eq!(
        audit(&db),
        vec![(0, "zero".to_string())],
        "the committed CALL must have run its body exactly once"
    );

    server.abort();
}

// ---- MySQL wire: a minimal text-protocol client ---------------------------
//
// Same shape as `tests/security_hdb_008.rs` and `tests/mysql_stmt_execute_tests.rs`.

const COM_QUERY: u8 = 0x03;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;

struct MySqlTestClient {
    stream: DuplexStream,
}

impl MySqlTestClient {
    async fn login(db: Arc<EmbeddedDatabase>) -> Self {
        let (client, server) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = MySqlHandler::handle_connection(db, server, 1).await;
        });
        let mut this = Self { stream: client };

        let (_seq, _greeting) = this.read_packet().await;
        let mut p = Vec::new();
        p.extend_from_slice(&(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION).to_le_bytes());
        p.extend_from_slice(&(1u32 << 24).to_le_bytes());
        p.push(45); // utf8mb4_general_ci
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(b"root");
        p.push(0);
        p.push(0); // empty auth response (SECURE_CONNECTION framing)
        this.write_packet(1, &p).await;

        let (_seq, ok) = this.read_packet().await;
        assert_eq!(ok[0], 0x00, "expected auth OK, got {:?}", &ok[..ok.len().min(16)]);
        this
    }

    async fn write_packet(&mut self, seq: u8, payload: &[u8]) {
        let len = payload.len();
        let hdr = [
            (len & 0xFF) as u8,
            ((len >> 8) & 0xFF) as u8,
            ((len >> 16) & 0xFF) as u8,
            seq,
        ];
        self.stream.write_all(&hdr).await.unwrap();
        self.stream.write_all(payload).await.unwrap();
        self.stream.flush().await.unwrap();
    }

    async fn read_packet(&mut self) -> (u8, Vec<u8>) {
        let mut hdr = [0u8; 4];
        self.stream.read_exact(&mut hdr).await.unwrap();
        let len = (hdr[0] as usize) | ((hdr[1] as usize) << 8) | ((hdr[2] as usize) << 16);
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).await.unwrap();
        (hdr[3], payload)
    }

    /// COM_QUERY that must answer OK.
    async fn ok(&mut self, sql: &str) {
        let mut p = vec![COM_QUERY];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p).await;
        let (_seq, pkt) = self.read_packet().await;
        assert_eq!(
            pkt.first().copied(),
            Some(0x00),
            "expected OK for `{sql}`, got {}",
            String::from_utf8_lossy(&pkt)
        );
    }
}

/// The whole MySQL wire is the TEXT family, so it is the other half of the
/// `execute_in_transaction_inner` `Call` arm's coverage.
#[tokio::test]
async fn mysql_wire_call_inside_a_transaction_is_undone_by_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    call_setup(&db);

    let mut client = MySqlTestClient::login(Arc::clone(&db)).await;
    client.ok("BEGIN").await;
    client.ok("CALL p_zero()").await;
    client.ok("ROLLBACK").await;
    assert_eq!(
        audit(&db),
        Vec::<(i64, String)>::new(),
        "*** MySQL-wire CALL autocommitted its body inside a transaction ***"
    );

    client.ok("BEGIN").await;
    client.ok("CALL p_zero()").await;
    client.ok("COMMIT").await;
    assert_eq!(audit(&db), vec![(0, "zero".to_string())]);
}
