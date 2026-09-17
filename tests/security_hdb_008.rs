//! HDB-008 — an embedded / session transaction stays committable after a
//! statement inside it has failed.
//!
//! Reported shape (v4.31.1, still present at d44d4eb):
//!
//! ```sql
//! CREATE TABLE accounts (id integer PRIMARY KEY);
//! BEGIN;
//! INSERT INTO accounts VALUES (1);
//! INSERT INTO accounts VALUES (1);   -- ERROR duplicate key
//! COMMIT;                            -- succeeded, and kept the first row
//! ```
//!
//! The PostgreSQL wire handler kept its own `TransactionStatus::Failed`, so a
//! psql client was safe; the ENGINE had no such state, so every embedded
//! caller — the Rust API, the REPL, the Python binding, the REST/MCP layers —
//! could commit partial work.
//!
//! The contract these tests pin:
//!
//! * any error inside an open transaction ABORTS it;
//! * every later statement is refused with SQLSTATE 25P02 and exactly
//!   `IN_FAILED_TRANSACTION_MESSAGE`;
//! * `COMMIT` / `commit()` rolls back and returns an error on the engine API
//!   (the PostgreSQL wire keeps answering with the `ROLLBACK` command tag, as
//!   PostgreSQL does);
//! * `ROLLBACK TO SAVEPOINT` is the one statement allowed through, and a
//!   successful one makes the transaction usable again;
//! * the state is per transaction, so one session's failure never touches
//!   another's.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{
    protocol::postgres::server::{PgServer, PgServerConfig},
    EmbeddedDatabase, Error, Tuple, Value, IN_FAILED_TRANSACTION_MESSAGE,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::time::timeout;
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

// ---------------------------------------------------------------- helpers --

/// Assert an error is EXACTLY the 25P02 refusal, comparing the inner message
/// so the `Transaction error: ` Display prefix cannot mask a different text.
fn assert_in_failed_transaction(err: &Error, what: &str) {
    assert!(
        err.is_failed_transaction(),
        "{what}: expected an aborted-transaction error, got {err:?}"
    );
    match err {
        Error::Transaction(message) => assert_eq!(
            message, IN_FAILED_TRANSACTION_MESSAGE,
            "{what}: wrong aborted-transaction message"
        ),
        other => panic!("{what}: expected Error::Transaction, got {other:?}"),
    }
}

fn id_of(tuple: &Tuple) -> i64 {
    match tuple.values.first() {
        Some(Value::Int2(v)) => i64::from(*v),
        Some(Value::Int4(v)) => i64::from(*v),
        Some(Value::Int8(v)) => *v,
        other => panic!("unexpected id value: {other:?}"),
    }
}

/// Every `id` in `table`, ascending. A full row set, not a count: a wrong row
/// set is only visible this way.
fn ids(db: &EmbeddedDatabase, table: &str) -> Vec<i64> {
    let rows = db
        .query(&format!("SELECT id FROM {table} ORDER BY id"), &[])
        .expect("select ids");
    rows.iter().map(id_of).collect()
}

fn accounts_db(table: &str) -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    db.execute(&format!("CREATE TABLE {table} (id integer PRIMARY KEY)"))
        .expect("create table");
    db
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

async fn setup_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr = listener.local_addr().expect("test addr");
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

async fn simple_err(client: &Client, sql: &str) -> tokio_postgres::Error {
    timeout(QUERY_TIMEOUT, client.simple_query(sql))
        .await
        .unwrap_or_else(|_| panic!("query timeout: {sql}"))
        .err()
        .unwrap_or_else(|| panic!("expected an error from: {sql}"))
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

// ------------------------------------------------------------- 1. the POC --

#[test]
fn hdb008_report_poc_commit_after_failed_insert_commits_nothing() {
    let db = accounts_db("accounts");

    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO accounts VALUES (1)").expect("first insert");
    let dup = db
        .execute("INSERT INTO accounts VALUES (1)")
        .expect_err("duplicate key must fail");
    assert!(!dup.is_failed_transaction(), "the FIRST failure is the duplicate key");

    let commit = db
        .execute("COMMIT")
        .expect_err("COMMIT of an aborted transaction must fail");
    assert!(
        commit.is_failed_transaction(),
        "COMMIT error must report the aborted transaction, got {commit:?}"
    );

    assert!(
        !db.in_transaction(),
        "the refused COMMIT must have ended the transaction"
    );
    assert_eq!(ids(&db, "accounts"), Vec::<i64>::new(), "nothing may be committed");
}

// --------------------------------- 2. refusal until the transaction ends ---

#[test]
fn hdb008_statements_after_a_failure_are_refused_until_rollback() {
    let db = accounts_db("acc2");

    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO acc2 VALUES (1)").expect("first insert");
    db.execute("INSERT INTO acc2 VALUES (1)")
        .expect_err("duplicate key must fail");

    let read = db.query("SELECT 1", &[]).expect_err("read must be refused");
    assert_in_failed_transaction(&read, "query after failure");

    let write = db
        .execute("INSERT INTO acc2 VALUES (2)")
        .expect_err("write must be refused");
    assert_in_failed_transaction(&write, "execute after failure");

    let savepoint = db.execute("SAVEPOINT s").expect_err("SAVEPOINT must be refused");
    assert_in_failed_transaction(&savepoint, "SAVEPOINT after failure");

    db.execute("ROLLBACK").expect("ROLLBACK always ends the block");
    assert!(!db.in_transaction());

    db.execute("INSERT INTO acc2 VALUES (7)")
        .expect("autocommit after rollback");
    assert_eq!(ids(&db, "acc2"), vec![7]);
}

// ------------------------------------------------- 3. the begin/commit API -

#[test]
fn hdb008_begin_commit_api_cannot_commit_failed_work() {
    let db = accounts_db("acc3");

    db.begin().expect("begin()");
    db.execute("INSERT INTO acc3 VALUES (1)").expect("first insert");
    db.execute("INSERT INTO acc3 VALUES (1)")
        .expect_err("duplicate key must fail");

    let commit = db.commit().expect_err("commit() of an aborted transaction must fail");
    assert!(commit.is_failed_transaction(), "got {commit:?}");
    assert!(!db.in_transaction());
    assert_eq!(ids(&db, "acc3"), Vec::<i64>::new());
}

// -------------------------------------------------- 4. savepoint recovery --

#[test]
fn hdb008_rollback_to_savepoint_recovers_and_keeps_earlier_work() {
    let db = accounts_db("acc4");

    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO acc4 VALUES (1)").expect("row 1");
    db.execute("SAVEPOINT s").expect("savepoint");
    db.execute("INSERT INTO acc4 VALUES (1)")
        .expect_err("duplicate key must fail");

    let refused = db
        .execute("INSERT INTO acc4 VALUES (2)")
        .expect_err("refused while aborted");
    assert_in_failed_transaction(&refused, "insert before the savepoint rollback");

    db.execute("ROLLBACK TO SAVEPOINT s")
        .expect("ROLLBACK TO SAVEPOINT is allowed inside an aborted block");

    db.execute("INSERT INTO acc4 VALUES (2)")
        .expect("the transaction is usable again");
    db.execute("COMMIT").expect("COMMIT of a recovered transaction");

    assert_eq!(ids(&db, "acc4"), vec![1, 2], "work before the savepoint must survive");
}

// -------------------------------------------------------- 5. the RAII handle

#[test]
fn hdb008_raii_transaction_handle_refuses_after_failure() {
    let db = accounts_db("acc5");

    #[allow(deprecated)]
    let tx = db.begin_transaction().expect("begin_transaction()");
    tx.execute("INSERT INTO acc5 VALUES (1)").expect("first insert");
    tx.execute("INSERT INTO acc5 VALUES (1)")
        .expect_err("duplicate key must fail");

    let read = tx.query("SELECT 1", &[]).expect_err("read must be refused");
    assert_in_failed_transaction(&read, "Transaction::query after failure");

    let commit = tx.commit().expect_err("Transaction::commit must fail");
    assert!(commit.is_failed_transaction(), "got {commit:?}");

    assert_eq!(ids(&db, "acc5"), Vec::<i64>::new());
}

// ------------------------------------------------------- 6. the session API

#[test]
fn hdb008_session_api_marks_the_session_transaction_failed() {
    let db = accounts_db("acc6");

    // The typed session API.
    let s = db.create_session("s", IsolationLevel::ReadCommitted).expect("session");
    db.begin_transaction_for_session(s).expect("begin");
    db.execute_in_session(s, "INSERT INTO acc6 VALUES (1)")
        .expect("first insert");
    db.execute_in_session(s, "INSERT INTO acc6 VALUES (1)")
        .expect_err("duplicate key must fail");

    let read = db
        .query_in_session(s, "SELECT 1", &[])
        .expect_err("read must be refused");
    assert_in_failed_transaction(&read, "query_in_session after failure");

    let commit = db
        .commit_transaction_for_session(s)
        .expect_err("COMMIT of an aborted session transaction must fail");
    assert!(commit.is_failed_transaction(), "got {commit:?}");
    assert!(!db.session_in_transaction(s), "the transaction must be over");
    assert_eq!(ids(&db, "acc6"), Vec::<i64>::new());
    db.destroy_session(s).expect("destroy session");

    // The SQL-text session path (`execute_for_session`), which is what the
    // wire protocols and the REST/MCP layers drive.
    let t = db.create_wire_session("t").expect("wire session");
    db.execute_for_session(t, "BEGIN").expect("BEGIN");
    db.execute_for_session(t, "INSERT INTO acc6 VALUES (2)")
        .expect("first insert");
    db.execute_for_session(t, "INSERT INTO acc6 VALUES (2)")
        .expect_err("duplicate key must fail");
    let commit = db
        .execute_for_session(t, "COMMIT")
        .expect_err("COMMIT of an aborted session transaction must fail");
    assert!(commit.is_failed_transaction(), "got {commit:?}");
    assert!(!db.session_in_transaction(t));
    assert_eq!(ids(&db, "acc6"), Vec::<i64>::new());
    db.destroy_session(t).expect("destroy session");
}

// ------------------------------------------- 7. a parse error aborts too ---

#[test]
fn hdb008_parse_error_inside_a_transaction_aborts_it() {
    let db = accounts_db("acc7");

    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO acc7 VALUES (1)").expect("row 1");
    db.execute("SELEC 1").expect_err("a syntax error must fail");

    let refused = db
        .execute("INSERT INTO acc7 VALUES (2)")
        .expect_err("refused after a parse error");
    assert_in_failed_transaction(&refused, "insert after a parse error");

    db.execute("ROLLBACK").expect("rollback");
    assert_eq!(ids(&db, "acc7"), Vec::<i64>::new());
}

// ------------------------------------------------------ 8. DDL is refused --

#[test]
fn hdb008_ddl_is_refused_inside_a_failed_transaction() {
    let db = accounts_db("acc8");

    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO acc8 VALUES (1)").expect("row 1");
    db.execute("INSERT INTO acc8 VALUES (1)")
        .expect_err("duplicate key must fail");

    let ddl = db
        .execute("CREATE TABLE acc8_t2 (id INT)")
        .expect_err("DDL must be refused inside an aborted block");
    assert_in_failed_transaction(&ddl, "CREATE TABLE after failure");

    db.execute("ROLLBACK").expect("rollback");
    db.execute("CREATE TABLE acc8_t2 (id INT)")
        .expect("DDL works again once the block is over");
}

// ------------------------------------------ 9. per-transaction, not global -

#[test]
fn hdb008_failure_in_one_session_does_not_touch_another() {
    let db = accounts_db("acc9");

    let a = db.create_wire_session("a").expect("session a");
    let b = db.create_wire_session("b").expect("session b");

    db.execute_for_session(a, "BEGIN").expect("a: begin");
    db.execute_for_session(b, "BEGIN").expect("b: begin");

    db.execute_for_session(a, "INSERT INTO acc9 VALUES (1)")
        .expect("a: row 1");
    db.execute_for_session(a, "INSERT INTO acc9 VALUES (1)")
        .expect_err("a: duplicate key must fail");

    // B is untouched: it can still write and commit.
    db.execute_for_session(b, "INSERT INTO acc9 VALUES (2)")
        .expect("b: must not be affected by a's failure");
    db.execute_for_session(b, "COMMIT").expect("b: commit");

    let commit_a = db
        .execute_for_session(a, "COMMIT")
        .expect_err("a: COMMIT of an aborted transaction must fail");
    assert!(commit_a.is_failed_transaction(), "got {commit_a:?}");

    assert_eq!(ids(&db, "acc9"), vec![2], "only B's row may survive");

    db.destroy_session(a).expect("destroy a");
    db.destroy_session(b).expect("destroy b");
}

// ----------------------------------------------- 10. wire savepoint recovery

#[tokio::test]
async fn hdb008_wire_rollback_to_savepoint_recovers_a_failed_block() {
    let (conn_string, server_handle) = setup_server().await;
    let client = connect(&conn_string).await;

    simple_ok(&client, "CREATE TABLE w10 (id integer PRIMARY KEY)").await;
    simple_ok(&client, "BEGIN").await;
    simple_ok(&client, "INSERT INTO w10 VALUES (1)").await;
    simple_ok(&client, "SAVEPOINT s").await;

    let dup = simple_err(&client, "INSERT INTO w10 VALUES (1)").await;
    assert_eq!(
        dup.code(),
        Some(&SqlState::UNIQUE_VIOLATION),
        "duplicate key must be 23505, got {dup:?}"
    );

    let refused = simple_err(&client, "SELECT 1").await;
    assert_eq!(
        refused.code(),
        Some(&SqlState::IN_FAILED_SQL_TRANSACTION),
        "statements inside an aborted block must be 25P02, got {refused:?}"
    );

    // The recovery PostgreSQL allows — and which the blanket 25P02 guard used
    // to refuse, leaving a wire client with no way back.
    simple_ok(&client, "ROLLBACK TO SAVEPOINT s").await;
    simple_ok(&client, "INSERT INTO w10 VALUES (2)").await;
    simple_ok(&client, "COMMIT").await;

    assert_eq!(wire_ids(&client, "w10").await, vec!["1".to_string(), "2".to_string()]);

    server_handle.abort();
}

// -------------------------------------------- 11. wire COMMIT stays PG-exact

#[tokio::test]
async fn hdb008_wire_commit_after_failure_answers_rollback() {
    let (conn_string, server_handle) = setup_server().await;
    let client = connect(&conn_string).await;

    simple_ok(&client, "CREATE TABLE w11 (id integer PRIMARY KEY)").await;
    simple_ok(&client, "BEGIN").await;
    simple_ok(&client, "INSERT INTO w11 VALUES (1)").await;
    let dup = simple_err(&client, "INSERT INTO w11 VALUES (1)").await;
    assert_eq!(dup.code(), Some(&SqlState::UNIQUE_VIOLATION));

    // CONTROL: on the wire, PostgreSQL answers a COMMIT of an aborted block
    // with the `ROLLBACK` command tag and NO error, and so must this build —
    // the engine's new error must not leak onto the wire. (tokio-postgres
    // surfaces `CommandComplete` without its tag text, so the assertion here is
    // "succeeds, and committed nothing"; the tag itself is covered by the
    // handler's own arm, which is deliberately unchanged.)
    let messages = simple_ok(&client, "COMMIT").await;
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, SimpleQueryMessage::CommandComplete(_))),
        "COMMIT of an aborted block must complete, not error"
    );

    assert_eq!(
        wire_ids(&client, "w11").await,
        Vec::<String>::new(),
        "nothing may be committed"
    );

    // …and the connection is usable again.
    simple_ok(&client, "INSERT INTO w11 VALUES (3)").await;
    assert_eq!(wire_ids(&client, "w11").await, vec!["3".to_string()]);

    server_handle.abort();
}

// --------------------------------------- 12. the batch params entry point --

/// BLOCK 1 of the adversarial review: `execute_many_params` — the entry point
/// the Python binding's `execute_many` routes to — staged straight into the
/// GLOBAL transaction slot with no boundary around it, so the reported bug
/// survived verbatim on a public API: a duplicate key inside the batch left
/// the transaction unmarked and the next `COMMIT` committed the partial work.
#[test]
fn hdb008_execute_many_params_cannot_commit_partial_work() {
    let db = accounts_db("acc12");

    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO acc12 VALUES (1)").expect("row 1");

    let dup = db
        .execute_many_params("INSERT INTO acc12 VALUES ($1)", &[vec![Value::Int4(1)]])
        .expect_err("execute_many_params must reject the duplicate key");
    assert!(
        !dup.is_failed_transaction(),
        "the duplicate itself is a constraint error, not the 25P02 refusal: {dup:?}"
    );

    // …and the transaction it failed inside is now aborted, so the batch entry
    // point is refused exactly like every other statement.
    let refused = db
        .execute_many_params("INSERT INTO acc12 VALUES ($1)", &[vec![Value::Int4(2)]])
        .expect_err("a batch issued inside an aborted transaction must be refused");
    assert_in_failed_transaction(&refused, "execute_many_params while aborted");

    let commit = db.execute("COMMIT").expect_err("COMMIT must fail");
    assert!(commit.is_failed_transaction(), "got {commit:?}");
    assert!(!db.in_transaction(), "the transaction must be over");
    assert_eq!(ids(&db, "acc12"), Vec::<i64>::new(), "nothing may be committed");
}

// ------------------------------- 13. the params family can END the block --

/// BLOCK 2 of the adversarial review: `execute_params_inner_unguarded` has no
/// transaction-control interception, so a text `COMMIT` / `ROLLBACK` / `BEGIN`
/// sent through the params family reaches the executor's plan arms — and the
/// first cut of the HDB-008 guard refused them with 25P02. A params-only
/// caller (the REST layer, the MCP tools, any binding that only ever binds
/// parameters) was then wedged for the process lifetime: the global slot
/// stayed occupied and aborted with no way to end it.
#[test]
fn hdb008_params_family_can_roll_back_and_commit_an_aborted_transaction() {
    // (a) COMMIT through the params family rolls the aborted block back and
    //     reports it, exactly as the text family does.
    let db = accounts_db("acc13a");
    db.execute_params("BEGIN", &[]).expect("params BEGIN");
    assert!(db.in_transaction(), "params BEGIN must open the global transaction");
    db.execute_params("INSERT INTO acc13a VALUES ($1)", &[Value::Int4(1)])
        .expect("row 1");
    db.execute_params("INSERT INTO acc13a VALUES ($1)", &[Value::Int4(1)])
        .expect_err("duplicate key must fail");

    let commit = db
        .execute_params("COMMIT", &[])
        .expect_err("COMMIT of an aborted block must report it, not 25P02");
    assert!(
        commit.is_failed_transaction(),
        "COMMIT must report the aborted block, got {commit:?}"
    );
    assert!(!db.in_transaction(), "the transaction must be over");
    assert_eq!(ids(&db, "acc13a"), Vec::<i64>::new(), "nothing may be committed");
    // The handle is usable again — this is the wedge the guard used to create.
    db.execute_params("INSERT INTO acc13a VALUES ($1)", &[Value::Int4(5)])
        .expect("the handle must still work");
    assert_eq!(ids(&db, "acc13a"), vec![5]);

    // (b) ROLLBACK through the params family simply succeeds.
    let db = accounts_db("acc13b");
    db.execute_params("BEGIN", &[]).expect("params BEGIN");
    db.execute_params("INSERT INTO acc13b VALUES ($1)", &[Value::Int4(1)])
        .expect("row 1");
    db.execute_params("INSERT INTO acc13b VALUES ($1)", &[Value::Int4(1)])
        .expect_err("duplicate key must fail");
    db.execute_params("ROLLBACK", &[])
        .expect("ROLLBACK of an aborted block must succeed");
    assert!(!db.in_transaction(), "the transaction must be over");
    assert_eq!(ids(&db, "acc13b"), Vec::<i64>::new());
}

// ------------------------------------ 14. SQL-level PREPARE / EXECUTE ------

/// FIX 5 of the adversarial review: `query()` and `query_with_schema` dispatch
/// `PREPARE` / `EXECUTE` / `DEALLOCATE` BEFORE their in-transaction branch, and
/// the `EXECUTE` arm attaches the global transaction itself — so an `EXECUTE`
/// inside an open `BEGIN` was neither refused while aborted nor recorded when
/// it failed.
#[test]
fn hdb008_execute_of_a_prepared_statement_is_bounded() {
    let db = accounts_db("acc14");
    db.query("PREPARE ins AS INSERT INTO acc14 VALUES ($1)", &[])
        .expect("prepare");

    db.execute("BEGIN").expect("begin");
    db.query("EXECUTE ins(1)", &[]).expect("first EXECUTE");

    let dup = db
        .query("EXECUTE ins(1)", &[])
        .expect_err("EXECUTE of a duplicate key must fail");
    assert!(
        !dup.is_failed_transaction(),
        "the duplicate itself is a constraint error: {dup:?}"
    );

    // The failure inside the block aborted it.
    let refused = db.query("SELECT 1", &[]).expect_err("must be refused");
    assert_in_failed_transaction(&refused, "SELECT after a failed EXECUTE");
    let refused_execute = db.query("EXECUTE ins(2)", &[]).expect_err("must be refused");
    assert_in_failed_transaction(&refused_execute, "EXECUTE while aborted");

    db.execute("ROLLBACK").expect("rollback");
    assert_eq!(ids(&db, "acc14"), Vec::<i64>::new(), "nothing may survive");
}

// ================== 15/16. MySQL wire — MySQL's own semantics ==============
//
// BLOCK 3 of the adversarial review. Two separate defects lived here:
//
// * `handle_commit` / `handle_rollback` returned the engine's error BEFORE
//   clearing `in_transaction` and `SERVER_STATUS_IN_TRANS`, while the engine
//   HAD ended the transaction — so the connection was wedged: `ROLLBACK` then
//   failed with "no active transaction", the next `BEGIN` opened no engine
//   transaction (because `in_transaction` was still true), and every following
//   statement silently autocommitted while the client was told it was inside a
//   block.
// * the engine's PostgreSQL contract (a statement error aborts the whole
//   transaction) is NOT MySQL's. InnoDB rolls back only the failed STATEMENT.
//   WordPress/PHP — the advertised MySQL path — depend on that, so the MySQL
//   listener clears the engine's mark once it has reported the error.
//
// Minimal text-protocol client, the same shape as
// `tests/security_hdb_009.rs` (case 8) and `tests/mysql_stmt_execute_tests.rs`.

const COM_QUERY: u8 = 0x03;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
/// `StatusFlags::SERVER_STATUS_IN_TRANS`.
const SERVER_STATUS_IN_TRANS: u16 = 0x0001;

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

    /// COM_QUERY; returns the first response packet.
    async fn send(&mut self, sql: &str) -> Vec<u8> {
        let mut p = vec![COM_QUERY];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p).await;
        let (_seq, first) = self.read_packet().await;
        first
    }

    /// COM_QUERY that must answer OK; returns the OK packet's status flags.
    /// (OK = `0x00`, lenenc affected, lenenc last_insert_id, u16 status.)
    async fn ok(&mut self, sql: &str) -> u16 {
        let pkt = self.send(sql).await;
        assert_eq!(
            pkt.first().copied(),
            Some(0x00),
            "expected OK for `{sql}`, got {}",
            String::from_utf8_lossy(&pkt)
        );
        assert!(pkt.len() >= 5, "short OK packet for `{sql}`");
        assert!(pkt[1] < 0xFB && pkt[2] < 0xFB, "test helper decodes short counts only");
        u16::from_le_bytes([pkt[3], pkt[4]])
    }

    /// COM_QUERY that must answer ERR; returns (error code, SQLSTATE, message).
    /// (ERR = `0xFF`, u16 code, `#`, 5-byte SQLSTATE, message.)
    async fn err(&mut self, sql: &str) -> (u16, String, String) {
        let pkt = self.send(sql).await;
        assert_eq!(pkt.first().copied(), Some(0xFF), "expected ERR for `{sql}`");
        assert!(pkt.len() >= 9, "short ERR packet for `{sql}`");
        (
            u16::from_le_bytes([pkt[1], pkt[2]]),
            String::from_utf8_lossy(&pkt[4..9]).into_owned(),
            String::from_utf8_lossy(&pkt[9..]).into_owned(),
        )
    }

    /// COM_QUERY for a one-column, one-row result set; returns that value.
    async fn scalar(&mut self, sql: &str) -> String {
        let first = self.send(sql).await;
        assert_ne!(
            first[0],
            0xFF,
            "server error for `{sql}`: {}",
            String::from_utf8_lossy(&first)
        );
        assert_eq!(first[0], 1, "expected a one-column result set for `{sql}`");
        let (_seq, _column_def) = self.read_packet().await;
        let (_seq, eof) = self.read_packet().await;
        assert_eq!(eof[0], 0xFE, "expected EOF after the column definition");

        let (_seq, row) = self.read_packet().await;
        assert_ne!(row[0], 0xFE, "expected a data row for `{sql}`");
        let len = row[0] as usize;
        assert!(len < 0xFB, "test helper only decodes short strings");
        let value = String::from_utf8_lossy(&row[1..1 + len]).into_owned();
        let (_seq, trailer) = self.read_packet().await;
        assert_eq!(trailer[0], 0xFE, "expected EOF after the row");
        value
    }
}

/// MySQL semantics: a failed statement does NOT abort the block.
#[tokio::test]
async fn hdb008_mysql_keeps_statement_level_transaction_semantics() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let mut c = MySqlTestClient::login(Arc::clone(&db)).await;

    c.ok("CREATE TABLE m15 (id INT PRIMARY KEY)").await;
    let status = c.ok("BEGIN").await;
    assert_eq!(
        status & SERVER_STATUS_IN_TRANS,
        SERVER_STATUS_IN_TRANS,
        "BEGIN must report SERVER_STATUS_IN_TRANS"
    );

    c.ok("INSERT INTO m15 VALUES (1)").await;
    let (code, _state, message) = c.err("INSERT INTO m15 VALUES (1)").await;
    assert_eq!(code, 1062, "duplicate key must be ER_DUP_ENTRY, got {message}");

    // InnoDB's contract: the block is still alive and still committable.
    let status = c.ok("INSERT INTO m15 VALUES (2)").await;
    assert_eq!(
        status & SERVER_STATUS_IN_TRANS,
        SERVER_STATUS_IN_TRANS,
        "the block must still be open after a statement error"
    );
    let status = c.ok("COMMIT").await;
    assert_eq!(
        status & SERVER_STATUS_IN_TRANS,
        0,
        "COMMIT must clear SERVER_STATUS_IN_TRANS"
    );

    assert_eq!(
        c.scalar("SELECT count(*) FROM m15").await,
        "2",
        "MySQL commits the statements that succeeded"
    );
}

/// The connection is never wedged: `COMMIT` / `ROLLBACK` / `BEGIN` all stay
/// usable after a statement error, and a later `BEGIN` really does open an
/// engine transaction (the wedge made it a silent no-op, after which every
/// write autocommitted while the client was told it was in a block).
#[tokio::test]
async fn hdb008_mysql_transaction_control_survives_a_statement_error() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let mut c = MySqlTestClient::login(Arc::clone(&db)).await;

    c.ok("CREATE TABLE m16 (id INT PRIMARY KEY)").await;

    // 1. error → ROLLBACK is accepted and really discards the block.
    c.ok("BEGIN").await;
    c.ok("INSERT INTO m16 VALUES (1)").await;
    c.err("INSERT INTO m16 VALUES (1)").await;
    let status = c.ok("ROLLBACK").await;
    assert_eq!(status & SERVER_STATUS_IN_TRANS, 0, "ROLLBACK must end the block");
    assert_eq!(c.scalar("SELECT count(*) FROM m16").await, "0", "ROLLBACK discards");

    // 2. the NEXT BEGIN must open a real engine transaction — the wedge left
    //    `in_transaction` true, so `handle_begin` skipped the engine and the
    //    following writes autocommitted.
    c.ok("BEGIN").await;
    c.ok("INSERT INTO m16 VALUES (7)").await;
    c.err("INSERT INTO m16 VALUES (7)").await;
    c.ok("ROLLBACK").await;
    assert_eq!(
        c.scalar("SELECT count(*) FROM m16").await,
        "0",
        "the second block must be a real transaction, not silent autocommit"
    );

    // 3. and COMMIT still works on a block that had an error in it — the
    //    wedge made this `ERROR 1105 (25000)` and then left the connection
    //    unable to end the block at all.
    c.ok("BEGIN").await;
    c.ok("INSERT INTO m16 VALUES (9)").await;
    c.err("INSERT INTO m16 VALUES (9)").await;
    let status = c.ok("COMMIT").await;
    assert_eq!(status & SERVER_STATUS_IN_TRANS, 0, "COMMIT must end the block");
    assert_eq!(c.scalar("SELECT count(*) FROM m16").await, "1");
}

// ------------------------------- 17. a caught DO … EXCEPTION is a recovery --

/// The idempotent-migration shape `handle_do_block` exists to serve:
///
/// ```sql
/// BEGIN;
/// DO $$ BEGIN CREATE TABLE t (id INT); EXCEPTION WHEN duplicate_table THEN null; END $$;
/// INSERT INTO t VALUES (1);
/// COMMIT;
/// ```
///
/// The inner statement FAILS on a re-run (the table already exists) and the
/// EXCEPTION clause swallows the error — so the client is told the block
/// completed (`DO`) and never sees an ErrorResponse. The engine, however, had
/// already marked the session transaction aborted, so every following
/// statement was refused `25P02` and `COMMIT` answered `ROLLBACK`: drizzle-kit
/// and Prisma silently discarded the whole migration, and the idempotency the
/// block was written for is exactly what triggered it.
///
/// A caught exception is a statement-level recovery, so the handler now undoes
/// the mark.
#[tokio::test]
async fn hdb008_do_block_exception_inside_a_transaction_keeps_the_block_usable() {
    let (conn_string, server_handle) = setup_server().await;
    let client = connect(&conn_string).await;

    simple_ok(&client, "CREATE TABLE w17 (id integer)").await;
    simple_ok(&client, "BEGIN").await;

    // The DO block itself must succeed — the exception is caught.
    simple_ok(
        &client,
        "DO $$ BEGIN CREATE TABLE w17 (id integer); EXCEPTION WHEN duplicate_table THEN null; END $$;",
    )
    .await;

    // …and the block must still be usable, and still committable.
    simple_ok(&client, "INSERT INTO w17 VALUES (1)").await;
    simple_ok(&client, "COMMIT").await;

    assert_eq!(
        wire_ids(&client, "w17").await,
        vec!["1".to_string()],
        "the migration after a caught DO exception must commit"
    );

    // CONTROL: an UNCAUGHT exception inside the same shape still aborts the
    // block — the clear is scoped to the exception the clause names.
    simple_ok(&client, "BEGIN").await;
    let uncaught = simple_err(
        &client,
        "DO $$ BEGIN CREATE TABLE w17 (id integer); EXCEPTION WHEN unique_violation THEN null; END $$;",
    )
    .await;
    assert!(
        uncaught.code().is_some(),
        "an unmatched exception must still reach the client, got {uncaught:?}"
    );
    let refused = simple_err(&client, "INSERT INTO w17 VALUES (2)").await;
    assert_eq!(
        refused.code(),
        Some(&SqlState::IN_FAILED_SQL_TRANSACTION),
        "an UNCAUGHT DO error must still abort the block, got {refused:?}"
    );
    simple_ok(&client, "ROLLBACK").await;
    assert_eq!(
        wire_ids(&client, "w17").await,
        vec!["1".to_string()],
        "the aborted block must have committed nothing"
    );

    server_handle.abort();
}

/// The same thing through the multi-statement `Q` batch form drizzle-kit
/// actually sends: one simple-query message carrying the DO block and the
/// statement after it. HDB-004 opens an implicit block in front of the DO
/// (`statement_can_write_data` classifies it as a write), so the aborted mark
/// made `close_implicit_block` fail the whole batch.
#[tokio::test]
async fn hdb008_do_block_exception_inside_a_multi_statement_batch_commits() {
    let (conn_string, server_handle) = setup_server().await;
    let client = connect(&conn_string).await;

    simple_ok(&client, "CREATE TABLE w18 (id integer)").await;

    simple_ok(
        &client,
        "DO $$ BEGIN CREATE TABLE w18 (id integer); EXCEPTION WHEN duplicate_table THEN null; END $$; \
         INSERT INTO w18 VALUES (1);",
    )
    .await;

    assert_eq!(
        wire_ids(&client, "w18").await,
        vec!["1".to_string()],
        "the batch's implicit block must commit after a caught DO exception"
    );

    server_handle.abort();
}
