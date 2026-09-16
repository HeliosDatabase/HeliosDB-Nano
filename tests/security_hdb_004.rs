//! HDB-004 — a multi-statement simple query is ONE implicit transaction and
//! stops at the first error.
//!
//! The report: the single `Q` message
//!
//! ```text
//! INSERT INTO items VALUES (1); COPY missing_table FROM STDIN; INSERT INTO items VALUES (2)
//! ```
//!
//! answered `CommandComplete INSERT 0 1`, `CopyInResponse`, `ErrorResponse`,
//! `CommandComplete INSERT 0 1`, `ReadyForQuery` — the batch kept running past
//! the COPY error, and BOTH rows stayed committed. Two defects behind it:
//!
//!   1. the batch loop stopped only on `Err`, so every path that answers its
//!      own error inline (`send_error(…)` then `return Ok(())`) let the rest of
//!      the message execute; and
//!   2. Nano autocommitted each statement, while PostgreSQL runs the whole
//!      message as one implicit transaction block unless the message itself
//!      contains explicit transaction control (protocol-flow, "Multiple
//!      Statements in a Simple Query").
//!
//! Plus the two COPY-specific halves of the same hole: COPY was dispatched
//! BEFORE the aborted-block guard, so it was the one statement a failed
//! transaction block still accepted (answering CopyInResponse!); and the
//! generic COPY fallback autocommitted every 500-row chunk, so a failure at row
//! 501+ left the earlier chunks committed.
//!
//! `client.simple_query(sql)` sends the whole string as ONE `Q` message, which
//! is the shape under test; the COPY cases need the raw wire, so they drive a
//! minimal frame-level client. EVERY batch response must contain exactly one
//! `Z` and at most one `E`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use bytes::{Buf, BufMut, BytesMut};
use heliosdb_nano::{
    protocol::postgres::server::{PgServer, PgServerConfig},
    EmbeddedDatabase,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

// ===========================================================================
// Server + tokio-postgres harness (same shape as
// tests/a14_postgres_transaction_error_recovery.rs)
// ===========================================================================

async fn setup_server() -> (SocketAddr, String, tokio::task::JoinHandle<()>) {
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
    let conn_string = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
    (addr, conn_string, handle)
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

async fn simple_scalar(client: &Client, sql: &str) -> String {
    let messages = timeout(QUERY_TIMEOUT, client.simple_query(sql))
        .await
        .expect("query timeout")
        .unwrap_or_else(|e| panic!("`{sql}` must succeed: {e}"));
    messages
        .into_iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
            _ => None,
        })
        .expect("scalar row")
}

async fn items_count(client: &Client) -> String {
    simple_scalar(client, "SELECT COUNT(*) FROM items").await
}

/// Run `sql` as one `Q` message; it must fail, and the driver's `DbError`
/// (SQLSTATE included) comes back.
async fn expect_batch_error(client: &Client, sql: &str) -> tokio_postgres::error::DbError {
    let err = timeout(QUERY_TIMEOUT, client.simple_query(sql))
        .await
        .expect("query timeout")
        .expect_err("the batch must fail");
    err.as_db_error()
        .cloned()
        .unwrap_or_else(|| panic!("expected a DbError, got: {err:?}"))
}

/// `CREATE TABLE items (id INT)` on a fresh server, plus a connected client.
async fn server_with_items() -> (SocketAddr, String, Client, tokio::task::JoinHandle<()>) {
    let (addr, conn_string, handle) = setup_server().await;
    let client = connect(&conn_string).await;
    client
        .batch_execute("CREATE TABLE items (id INT)")
        .await
        .expect("create items");
    (addr, conn_string, client, handle)
}

// ===========================================================================
// Raw frame-level client (same shape as
// tests/a15_postgres_binary_int4_transaction.rs)
// ===========================================================================

fn put_cstr(buf: &mut BytesMut, value: &str) {
    buf.extend_from_slice(value.as_bytes());
    buf.put_u8(0);
}

fn startup_message() -> BytesMut {
    let mut body = BytesMut::new();
    body.put_i32(196_608);
    put_cstr(&mut body, "user");
    put_cstr(&mut body, "postgres");
    put_cstr(&mut body, "database");
    put_cstr(&mut body, "postgres");
    body.put_u8(0);

    let mut msg = BytesMut::new();
    msg.put_i32((body.len() + 4) as i32);
    msg.extend_from_slice(&body);
    msg
}

fn frontend_message(tag: u8, body: BytesMut) -> BytesMut {
    let mut msg = BytesMut::new();
    msg.put_u8(tag);
    msg.put_i32((body.len() + 4) as i32);
    msg.extend_from_slice(&body);
    msg
}

fn query_message(sql: &str) -> BytesMut {
    let mut body = BytesMut::new();
    put_cstr(&mut body, sql);
    frontend_message(b'Q', body)
}

fn copy_data_message(payload: &str) -> BytesMut {
    let mut body = BytesMut::new();
    body.extend_from_slice(payload.as_bytes());
    frontend_message(b'd', body)
}

fn copy_done_message() -> BytesMut {
    frontend_message(b'c', BytesMut::new())
}

async fn read_backend_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    timeout(IO_TIMEOUT, stream.read_exact(&mut header))
        .await
        .expect("read header timeout")
        .expect("read header");
    let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len - 4];
    timeout(IO_TIMEOUT, stream.read_exact(&mut body))
        .await
        .expect("read body timeout")
        .expect("read body");
    (header[0], body)
}

async fn connect_wire(addr: SocketAddr) -> TcpStream {
    let mut stream = timeout(IO_TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    timeout(IO_TIMEOUT, stream.write_all(&startup_message()))
        .await
        .expect("startup write timeout")
        .expect("startup write");
    loop {
        if read_backend_message(&mut stream).await.0 == b'Z' {
            return stream;
        }
    }
}

/// Send `sql` as ONE `Q` message and collect every backend frame up to and
/// including ReadyForQuery. A `G` (CopyInResponse) is answered with `copy_rows`
/// as CopyData frames followed by CopyDone, so a case that does not expect a
/// CopyInResponse still cannot deadlock if one arrives.
async fn wire_query(stream: &mut TcpStream, sql: &str, copy_rows: &[String]) -> Vec<(u8, Vec<u8>)> {
    timeout(IO_TIMEOUT, stream.write_all(&query_message(sql)))
        .await
        .expect("query write timeout")
        .expect("query write");
    let mut frames = Vec::new();
    loop {
        let frame = read_backend_message(stream).await;
        let tag = frame.0;
        frames.push(frame);
        if tag == b'G' {
            let mut out = BytesMut::new();
            for row in copy_rows {
                out.extend_from_slice(&copy_data_message(row));
            }
            out.extend_from_slice(&copy_done_message());
            timeout(IO_TIMEOUT, stream.write_all(&out))
                .await
                .expect("copy write timeout")
                .expect("copy write");
        }
        if tag == b'Z' {
            return frames;
        }
    }
}

fn frame_tags(frames: &[(u8, Vec<u8>)]) -> Vec<u8> {
    frames.iter().map(|(tag, _)| *tag).collect()
}

/// The frame sequence as a readable string, e.g. `"C G E Z"`.
fn tag_trace(frames: &[(u8, Vec<u8>)]) -> String {
    frame_tags(frames)
        .iter()
        .map(|tag| (*tag as char).to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn count_tag(frames: &[(u8, Vec<u8>)], tag: u8) -> usize {
    frames.iter().filter(|(t, _)| *t == tag).count()
}

/// The status byte of the (single) trailing ReadyForQuery: `I`, `T` or `E`.
fn ready_status(frames: &[(u8, Vec<u8>)]) -> u8 {
    frames
        .iter()
        .rev()
        .find(|(tag, _)| *tag == b'Z')
        .map(|(_, body)| body[0])
        .expect("a ReadyForQuery frame")
}

/// One field of an ErrorResponse body (`C` = SQLSTATE, `M` = message).
fn error_field(body: &[u8], field: u8) -> Option<String> {
    let mut cursor = body;
    while let Some((&kind, rest)) = cursor.split_first() {
        if kind == 0 {
            return None;
        }
        let end = rest.iter().position(|b| *b == 0)?;
        let value = String::from_utf8_lossy(&rest[..end]).to_string();
        if kind == field {
            return Some(value);
        }
        cursor = &rest[end + 1..];
    }
    None
}

fn error_sqlstate(frames: &[(u8, Vec<u8>)]) -> Option<String> {
    frames
        .iter()
        .find(|(tag, _)| *tag == b'E')
        .and_then(|(_, body)| error_field(body, b'C'))
}

/// The first column of the first DataRow, as text.
fn first_data_row_text(frames: &[(u8, Vec<u8>)]) -> Option<String> {
    let (_, body) = frames.iter().find(|(tag, _)| *tag == b'D')?;
    let mut cursor: &[u8] = body;
    if cursor.get_i16() < 1 {
        return None;
    }
    let len = cursor.get_i32();
    if len < 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&cursor[..len as usize]).to_string())
}

async fn wire_count(stream: &mut TcpStream, table: &str) -> String {
    let frames = wire_query(stream, &format!("SELECT COUNT(*) FROM {table}"), &[]).await;
    assert_eq!(
        count_tag(&frames, b'E'),
        0,
        "COUNT(*) over {table} must not error: {}",
        tag_trace(&frames)
    );
    first_data_row_text(&frames).expect("count row")
}

/// Every batch answer owes the client exactly one ReadyForQuery and at most one
/// ErrorResponse — the invariant the whole item rests on.
fn assert_one_ready_at_most_one_error(frames: &[(u8, Vec<u8>)], what: &str) {
    assert_eq!(
        count_tag(frames, b'Z'),
        1,
        "{what} must end with exactly ONE ReadyForQuery: {}",
        tag_trace(frames)
    );
    assert!(
        count_tag(frames, b'E') <= 1,
        "{what} must report at most one ErrorResponse: {}",
        tag_trace(frames)
    );
    assert_eq!(
        frames.last().map(|(tag, _)| *tag),
        Some(b'Z'),
        "{what} must END with the ReadyForQuery: {}",
        tag_trace(frames)
    );
}

// ===========================================================================
// 1. The report, byte for byte, on the raw wire.
// ===========================================================================

#[tokio::test]
async fn hdb004_copy_error_mid_batch_abandons_the_message_and_rolls_it_back() {
    let (addr, _conn_string, _client, server_handle) = server_with_items().await;
    let mut stream = connect_wire(addr).await;

    let frames = wire_query(
        &mut stream,
        "INSERT INTO items VALUES (1); COPY missing_table FROM STDIN; INSERT INTO items VALUES (2)",
        &["1\n".to_string()],
    )
    .await;
    let trace = tag_trace(&frames);
    assert_one_ready_at_most_one_error(&frames, "the report's batch");
    assert_eq!(count_tag(&frames, b'E'), 1, "the COPY must report an error: {trace}");

    let tags = frame_tags(&frames);
    let first_complete = tags
        .iter()
        .position(|tag| *tag == b'C')
        .unwrap_or_else(|| panic!("the first INSERT must complete: {trace}"));
    let error_at = tags
        .iter()
        .position(|tag| *tag == b'E')
        .unwrap_or_else(|| panic!("the COPY must fail: {trace}"));
    assert!(
        first_complete < error_at,
        "the error must follow the first INSERT's CommandComplete: {trace}"
    );
    // The statement after the failing COPY must never run, so the first
    // CommandComplete is the ONLY one. This is the regression: the batch used
    // to answer a second `INSERT 0 1` after the ErrorResponse.
    assert_eq!(
        count_tag(&frames, b'C'),
        1,
        "the batch must be abandoned at the COPY error — no statement after it may complete: {trace}"
    );
    // A missing table may be rejected before or after CopyInResponse; nothing
    // else is allowed in between.
    for tag in &tags[first_complete + 1..error_at] {
        assert_eq!(
            *tag as char, 'G',
            "only a CopyInResponse may sit between the first CommandComplete and the ErrorResponse: {trace}"
        );
    }
    assert_eq!(
        ready_status(&frames) as char,
        'I',
        "an implicit transaction block reports `I`, like PostgreSQL: {trace}"
    );

    // ... and the first INSERT went with it: the message was ONE transaction.
    assert_eq!(
        wire_count(&mut stream, "items").await,
        "0",
        "the whole implicit transaction block must roll back"
    );

    server_handle.abort();
}

// ===========================================================================
// 2. An ordinary error mid-batch.
// ===========================================================================

#[tokio::test]
async fn hdb004_plain_error_mid_batch_rolls_the_whole_message_back() {
    let (_addr, _conn_string, client, server_handle) = server_with_items().await;

    let db_err = expect_batch_error(
        &client,
        "INSERT INTO items VALUES (1); INSERT INTO no_such_table VALUES (1); INSERT INTO items VALUES (2)",
    )
    .await;
    assert_eq!(*db_err.code(), SqlState::UNDEFINED_TABLE, "got: {db_err:?}");

    assert_eq!(
        items_count(&client).await,
        "0",
        "the statement before the failure must roll back with the implicit block"
    );
    // The connection is not wedged: one ErrorResponse, one ReadyForQuery.
    assert_eq!(simple_scalar(&client, "SELECT 1").await, "1");

    server_handle.abort();
}

// ===========================================================================
// 3. An error a statement answers INLINE (`send_error` + `Ok(())`).
// ===========================================================================

#[tokio::test]
async fn hdb004_inline_answered_error_mid_batch_still_stops_the_message() {
    let (_addr, _conn_string, client, server_handle) = server_with_items().await;

    // `SET TRANSACTION ISOLATION LEVEL <bogus>` reports 22023 itself and
    // returns Ok — the exact shape that used to let the batch run on.
    let batch = "INSERT INTO items VALUES (1); \
                 SET TRANSACTION ISOLATION LEVEL bogus; \
                 INSERT INTO items VALUES (2)";
    let result = timeout(QUERY_TIMEOUT, client.simple_query(batch))
        .await
        .expect("query timeout");
    assert!(
        result.is_err(),
        "an inline-answered error must still fail the batch, got: {result:?}"
    );

    assert_eq!(
        items_count(&client).await,
        "0",
        "an inline-answered error must roll the implicit block back too"
    );
    assert_eq!(simple_scalar(&client, "SELECT 1").await, "1");

    server_handle.abort();
}

// ===========================================================================
// 4. The happy path still commits — for everyone.
// ===========================================================================

#[tokio::test]
async fn hdb004_successful_batch_commits_the_implicit_block() {
    let (_addr, conn_string, client, server_handle) = server_with_items().await;

    client
        .simple_query("INSERT INTO items VALUES (1); INSERT INTO items VALUES (2)")
        .await
        .expect("a successful batch must succeed");

    assert_eq!(items_count(&client).await, "2");
    // A SECOND connection proves the implicit block COMMITTED rather than
    // merely being visible to its own session.
    let other = connect(&conn_string).await;
    assert_eq!(
        items_count(&other).await,
        "2",
        "the implicit block must be committed, not left open"
    );

    server_handle.abort();
}

// ===========================================================================
// 5. Explicit COMMIT inside the batch keeps what came before it.
// ===========================================================================

#[tokio::test]
async fn hdb004_commit_inside_the_batch_keeps_the_earlier_statements() {
    let (_addr, _conn_string, client, server_handle) = server_with_items().await;

    let db_err = expect_batch_error(
        &client,
        "INSERT INTO items VALUES (1); COMMIT; INSERT INTO items VALUES (2); INSERT INTO no_such_table VALUES (1)",
    )
    .await;
    assert_eq!(*db_err.code(), SqlState::UNDEFINED_TABLE, "got: {db_err:?}");

    assert_eq!(
        items_count(&client).await,
        "1",
        "an explicit COMMIT inside the message commits everything before it, and only the \
         statements after it roll back — exactly as PostgreSQL does"
    );

    server_handle.abort();
}

// ===========================================================================
// 6. BEGIN inside the batch converts the implicit block.
// ===========================================================================

#[tokio::test]
async fn hdb004_begin_inside_the_batch_converts_the_implicit_block() {
    let (_addr, _conn_string, client, server_handle) = server_with_items().await;

    client
        .simple_query("INSERT INTO items VALUES (1); BEGIN; INSERT INTO items VALUES (2); COMMIT")
        .await
        .expect("BEGIN must convert the implicit block, not warn about a nested one");

    assert_eq!(
        items_count(&client).await,
        "2",
        "the row written before BEGIN belongs to the converted block and commits with it"
    );
    assert_eq!(simple_scalar(&client, "SELECT 1").await, "1");

    server_handle.abort();
}

// ===========================================================================
// 7. An error inside an EXPLICIT block in the batch leaves a failed block.
// ===========================================================================

#[tokio::test]
async fn hdb004_error_inside_an_explicit_block_in_the_batch_needs_a_rollback() {
    let (_addr, _conn_string, client, server_handle) = server_with_items().await;

    let db_err = expect_batch_error(
        &client,
        "BEGIN; INSERT INTO items VALUES (1); INSERT INTO no_such_table VALUES (1); INSERT INTO items VALUES (2)",
    )
    .await;
    assert_eq!(*db_err.code(), SqlState::UNDEFINED_TABLE, "got: {db_err:?}");

    // The client's own block is still open and aborted: PostgreSQL requires the
    // client to ROLLBACK, and every statement until then is refused with 25P02.
    let blocked = timeout(QUERY_TIMEOUT, client.simple_query("SELECT 1"))
        .await
        .expect("query timeout")
        .expect_err("a failed explicit block must refuse further statements");
    let blocked_err = blocked
        .as_db_error()
        .cloned()
        .unwrap_or_else(|| panic!("expected a DbError, got: {blocked:?}"));
    assert_eq!(*blocked_err.code(), SqlState::IN_FAILED_SQL_TRANSACTION);

    client.simple_query("ROLLBACK").await.expect("ROLLBACK recovers");
    assert_eq!(items_count(&client).await, "0");

    server_handle.abort();
}

// ===========================================================================
// 8. DDL is not transactional; the DML around it still rolls back.
// ===========================================================================

#[tokio::test]
async fn hdb004_ddl_in_the_batch_survives_while_the_dml_rolls_back() {
    let (_addr, _conn_string, client, server_handle) = server_with_items().await;

    let db_err = expect_batch_error(
        &client,
        "INSERT INTO items VALUES (1); CREATE TABLE hdb004_side (id INT); INSERT INTO no_such_table VALUES (1)",
    )
    .await;
    assert_eq!(*db_err.code(), SqlState::UNDEFINED_TABLE, "got: {db_err:?}");

    // CREATE TABLE auto-commits in this engine even inside a session
    // transaction, so the table stays — pinned so the asymmetry is deliberate
    // and visible, not a surprise the next reader has to rediscover.
    assert_eq!(
        simple_scalar(&client, "SELECT COUNT(*) FROM hdb004_side").await,
        "0",
        "DDL is not transactional here: hdb004_side must still exist"
    );
    assert_eq!(
        items_count(&client).await,
        "0",
        "ordinary DML in the implicit block must still roll back"
    );

    server_handle.abort();
}

// ===========================================================================
// 9. The generic COPY fallback is atomic beyond one 500-row chunk.
// ===========================================================================

/// The fast `copy_bulk_insert` path is already all-or-nothing, so this case
/// needs a table shape it DECLINES: a `STORAGE DICTIONARY` column, rejected by
/// `EmbeddedDatabase::fast_insert_batch_can_use_direct_write` (src/lib.rs —
/// dictionary/CAS columns need the per-row transform), which `copy_bulk_insert`
/// consults right after resolving the insert spec. A CHECK constraint on a
/// plain column then fails at row 550 — the SECOND 500-row chunk, which is
/// where the old per-chunk autocommit left the first 500 rows committed.
#[tokio::test]
async fn hdb004_copy_fallback_is_atomic_across_chunks() {
    let (addr, conn_string, server_handle) = setup_server().await;
    let client = connect(&conn_string).await;
    let ddl = "CREATE TABLE hdb004_copy (id INT, tag TEXT STORAGE DICTIONARY, \
               note TEXT CHECK (note <> 'bad'))";
    client.batch_execute(ddl).await.expect("create the copy table");

    let mut payload = String::new();
    for i in 1..=600 {
        let note = if i == 550 { "bad" } else { "ok" };
        payload.push_str(&format!("{i}\tt{i}\t{note}\n"));
    }

    let mut stream = connect_wire(addr).await;
    let frames = wire_query(&mut stream, "COPY hdb004_copy FROM STDIN", &[payload]).await;
    let trace = tag_trace(&frames);
    assert_one_ready_at_most_one_error(&frames, "the failing COPY");
    assert_eq!(count_tag(&frames, b'E'), 1, "row 550 must fail the COPY: {trace}");
    assert_eq!(
        count_tag(&frames, b'C'),
        0,
        "a failed COPY must not also report CommandComplete: {trace}"
    );
    assert_eq!(error_sqlstate(&frames).as_deref(), Some("XX000"), "trace: {trace}");

    assert_eq!(
        wire_count(&mut stream, "hdb004_copy").await,
        "0",
        "the COPY fallback must be all-or-nothing: the first 500-row chunk may not survive"
    );

    server_handle.abort();
}

// ===========================================================================
// 10. COPY inside a failed block: 25P02, and NO CopyInResponse.
// ===========================================================================

#[tokio::test]
async fn hdb004_copy_in_a_failed_block_is_refused_without_copy_in_response() {
    let (addr, _conn_string, _client, server_handle) = server_with_items().await;
    let mut stream = connect_wire(addr).await;

    let failed = wire_query(&mut stream, "BEGIN; INSERT INTO no_such_table VALUES (1)", &[]).await;
    assert_one_ready_at_most_one_error(&failed, "the failing explicit block");
    assert_eq!(
        ready_status(&failed) as char,
        'E',
        "the block must be reported aborted: {}",
        tag_trace(&failed)
    );

    // A COPY is dispatched before the generic aborted-block guard, so this is
    // the statement that used to answer CopyInResponse and swallow a whole
    // file into a transaction that could never commit.
    let copy = wire_query(&mut stream, "COPY items FROM STDIN", &[]).await;
    let trace = tag_trace(&copy);
    assert_one_ready_at_most_one_error(&copy, "COPY in a failed block");
    assert_eq!(
        count_tag(&copy, b'G'),
        0,
        "a failed block must NOT invite the client to stream COPY data: {trace}"
    );
    assert_eq!(error_sqlstate(&copy).as_deref(), Some("25P02"), "trace: {trace}");

    let rollback = wire_query(&mut stream, "ROLLBACK", &[]).await;
    assert_eq!(count_tag(&rollback, b'E'), 0, "ROLLBACK must recover the connection");
    assert_eq!(ready_status(&rollback) as char, 'I');
    assert_eq!(wire_count(&mut stream, "items").await, "0");

    server_handle.abort();
}

// ===========================================================================
// 11-14. The implicit block opens LAZILY, in front of the first statement that
// can write table data. A block opened for `SELECT 1` costs a result-cache
// flush, raises the process-wide `session_txn_count` that gates OTHER sessions'
// autocommit fast paths, and — for a SERIALIZABLE session under
// `storage.serializable_policy = "error"` — fails a read-only batch outright,
// all to protect statements with nothing to roll back.
// ===========================================================================

/// A read-only prelude must not consume the batch's one chance to choose an
/// isolation level: with no block open at the `BEGIN`, the level is honoured.
#[tokio::test]
async fn hdb004_read_only_prelude_leaves_begin_isolation_level_usable() {
    let (_addr, _conn_string, client, server_handle) = server_with_items().await;

    client
        .simple_query("SELECT 1; BEGIN ISOLATION LEVEL SERIALIZABLE; INSERT INTO items VALUES (1); COMMIT")
        .await
        .expect("a read-only prelude must not open an implicit block in front of BEGIN");

    assert_eq!(
        items_count(&client).await,
        "1",
        "the explicit block the batch opened must commit its row"
    );
    assert_eq!(simple_scalar(&client, "SELECT 1").await, "1");

    server_handle.abort();
}

/// After a write HAS opened the implicit block, an isolation level cannot be
/// applied retroactively — PostgreSQL answers 25001 rather than silently
/// running the transaction at a level the client did not ask for.
#[tokio::test]
async fn hdb004_begin_with_isolation_level_inside_an_open_block_is_25001() {
    let (_addr, _conn_string, client, server_handle) = server_with_items().await;

    let db_err = expect_batch_error(
        &client,
        "INSERT INTO items VALUES (1); BEGIN ISOLATION LEVEL SERIALIZABLE; \
         INSERT INTO items VALUES (2); COMMIT",
    )
    .await;
    assert_eq!(*db_err.code(), SqlState::ACTIVE_SQL_TRANSACTION, "got: {db_err:?}");

    assert_eq!(
        items_count(&client).await,
        "0",
        "the refused BEGIN aborts the batch, so the implicit block rolls back"
    );
    // One ErrorResponse, one ReadyForQuery: the connection is not wedged.
    assert_eq!(simple_scalar(&client, "SELECT 1").await, "1");

    server_handle.abort();
}

/// A leading `SET` is not a write, so the block opens at the INSERT behind it —
/// and still covers everything from there on.
#[tokio::test]
async fn hdb004_a_leading_set_does_not_open_the_block_but_the_insert_does() {
    let (_addr, _conn_string, client, server_handle) = server_with_items().await;

    let db_err = expect_batch_error(
        &client,
        "SET application_name = 'hdb004'; INSERT INTO items VALUES (1); \
         INSERT INTO no_such_table VALUES (1)",
    )
    .await;
    assert_eq!(*db_err.code(), SqlState::UNDEFINED_TABLE, "got: {db_err:?}");

    assert_eq!(
        items_count(&client).await,
        "0",
        "the block opened lazily at the INSERT and must still roll it back"
    );

    server_handle.abort();
}

/// Same for leading DDL, which is non-transactional in this engine: the block
/// opens at the first INSERT, the table survives, the row does not.
#[tokio::test]
async fn hdb004_leading_ddl_does_not_open_the_block_but_the_insert_does() {
    let (_addr, _conn_string, client, server_handle) = server_with_items().await;

    let db_err = expect_batch_error(
        &client,
        "CREATE TABLE hdb004_lazy (id INT); INSERT INTO hdb004_lazy VALUES (1); \
         INSERT INTO no_such_table VALUES (1)",
    )
    .await;
    assert_eq!(*db_err.code(), SqlState::UNDEFINED_TABLE, "got: {db_err:?}");

    assert_eq!(
        simple_scalar(&client, "SELECT COUNT(*) FROM hdb004_lazy").await,
        "0",
        "the CREATE TABLE auto-committed (DDL is not transactional here), so the table exists \
         and the row it received rolled back with the implicit block"
    );

    server_handle.abort();
}
