//! A REPEATABLE READ snapshot must not change under its reader while other clients
//! commit rows.
//!
//! The defect (fixed in 4.42.2): three commit forms took a commit timestamp that was
//! not announced in flight, so a snapshot allocated after that timestamp did not wait
//! for the write. A REPEATABLE READ transaction that began between the timestamp and
//! the apply counted the table without the rows, and later with them:
//!
//! * an autocommit statement committed without row locks, at its snapshot timestamp
//!   (multi-row INSERT … VALUES, INSERT … SELECT);
//! * the fast batch funnel of COPY and batched parameterized INSERT;
//! * a commit that staged only INSERT rows (`insert_log`), which skipped the
//!   announcement.
//!
//! The deterministic tests pause the writer with `debug_next_commit_stamped`, after its
//! commit timestamp is taken and before the write is applied, begin a REPEATABLE READ
//! transaction in another session during the pause, and count twice: once during the
//! pause (or as soon as the snapshot may proceed) and once after the write returned.
//! The two counts must be equal. Before the fix the first count missed the rows.
//!
//! The last test is a short PostgreSQL wire stress run of the reported shape.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::storage::debug_next_commit_stamped;
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The commit-stamp hook is process-wide: one deterministic case at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// How long the paused writer waits for the reader's first count. With the fix the
/// reader's snapshot waits for the writer, so this always elapses.
const PAUSE: Duration = Duration::from_millis(300);

const BATCH: i64 = 10;

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn int(value: &Value) -> i64 {
    match value {
        Value::Int8(v) => *v,
        Value::Int4(v) => i64::from(*v),
        other => panic!("count(*) returned {other:?}"),
    }
}

fn session_count(db: &EmbeddedDatabase, session: SessionId, table: &str) -> i64 {
    let (rows, _) = db
        .query_with_columns_for_session(session, &format!("SELECT count(*) FROM {table}"))
        .expect("count");
    int(rows.first().and_then(|row| row.values.first()).expect("count row"))
}

/// Committed rows, read in a session of its own (not the embedded handle's
/// transaction, which one test keeps open across the write).
fn count(db: &EmbeddedDatabase, table: &str) -> i64 {
    let probe = db.create_wire_session("postgres").expect("probe session");
    let n = session_count(db, probe, table);
    let _ = db.destroy_session(probe);
    n
}

fn values(first: i64, n: i64) -> String {
    (first..first + n)
        .map(|i| format!("({i}, {})", i % 7))
        .collect::<Vec<_>>()
        .join(", ")
}

struct Observed {
    site: &'static str,
    before: i64,
    first: i64,
    second: i64,
    after: i64,
}

/// Run `write` with the commit-stamp hook armed. During the pause a REPEATABLE READ
/// transaction begins in its own session and counts `table`; it counts again after
/// `write` returned.
fn observe(db: &Arc<EmbeddedDatabase>, table: &str, write: impl FnOnce()) -> Observed {
    let before = count(db, table);
    let reader = db.create_wire_session("postgres").expect("reader session");
    db.set_session_isolation(reader, IsolationLevel::RepeatableRead)
        .expect("isolation");

    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (first_tx, first_rx) = mpsc::channel::<i64>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let site = Arc::new(Mutex::new(None::<&'static str>));

    let reader_db = Arc::clone(db);
    let reader_table = table.to_string();
    let reader_thread = std::thread::spawn(move || -> Option<(i64, i64)> {
        go_rx.recv().ok()?;
        reader_db.begin_transaction_for_session(reader).expect("begin");
        let first = session_count(&reader_db, reader, &reader_table);
        let _ = first_tx.send(first);
        let _ = done_rx.recv();
        let second = session_count(&reader_db, reader, &reader_table);
        reader_db.commit_transaction_for_session(reader).expect("commit");
        Some((first, second))
    });

    let hook_site = Arc::clone(&site);
    debug_next_commit_stamped(move |name, _ts| {
        *hook_site.lock().unwrap() = Some(name);
        let _ = go_tx.send(());
        // Without the fix the reader counts here, before the write is applied.
        let _ = first_rx.recv_timeout(PAUSE);
    });
    write();
    let fired = site.lock().unwrap().take();
    if fired.is_none() {
        // Disarm: dropping the armed hook drops `go_tx`, which releases the reader.
        debug_next_commit_stamped(|_, _| {});
    }
    let _ = done_tx.send(());
    let counts = reader_thread.join().expect("reader thread");
    let _ = db.destroy_session(reader);
    let site = fired.expect("the write never reached a commit-stamp hook");
    let (first, second) = counts.expect("reader did not run");
    Observed {
        site,
        before,
        first,
        second,
        after: count(db, table),
    }
}

fn assert_stable(what: &str, expected_site: &str, o: &Observed, added: i64) {
    assert_eq!(o.site, expected_site, "{what}: commit funnel");
    assert_eq!(o.after, o.before + added, "{what}: rows written");
    assert_eq!(
        o.first, o.second,
        "{what}: the REPEATABLE READ snapshot changed under its reader \
         (count {} then {}; {} rows before the write, {} after)",
        o.first, o.second, o.before, o.after
    );
    assert!(
        o.first == o.before || o.first == o.after,
        "{what}: the snapshot saw a partial write ({} of {}..{})",
        o.first,
        o.before,
        o.after
    );
}

fn db_with_table(table: &str) -> Arc<EmbeddedDatabase> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, v INT)"))
        .expect("create");
    db.execute(&format!("INSERT INTO {table} VALUES {}", values(0, BATCH)))
        .expect("seed");
    db
}

/// Autocommit multi-row INSERT on a wire session (PostgreSQL simple query and MySQL
/// both run `execute_for_session`): an autocommit statement that stages only INSERT
/// rows and holds no row lock. It used to commit at its snapshot timestamp,
/// unannounced. This is the reported case.
#[test]
fn rr_snapshot_stable_across_autocommit_multi_row_insert() {
    let _serial = serial();
    let db = db_with_table("t");
    let writer = db.create_wire_session("postgres").expect("writer session");
    let o = observe(&db, "t", || {
        db.execute_for_session(writer, &format!("INSERT INTO t VALUES {}", values(1_000, BATCH)))
            .expect("insert");
    });
    assert_stable("multi-row INSERT", "transaction", &o, BATCH);
}

/// Autocommit multi-row INSERT on the embedded API.
#[test]
fn rr_snapshot_stable_across_embedded_multi_row_insert() {
    let _serial = serial();
    let db = db_with_table("t");
    let o = observe(&db, "t", || {
        db.execute(&format!("INSERT INTO t VALUES {}", values(1_000, BATCH)))
            .expect("insert");
    });
    assert_stable("embedded multi-row INSERT", "transaction", &o, BATCH);
}

/// A parameterized INSERT executed for many rows at once on the embedded API: the
/// fast batch funnel (shared with COPY).
#[test]
fn rr_snapshot_stable_across_embedded_batch_insert() {
    let _serial = serial();
    let db = db_with_table("t");
    let rows: Vec<Vec<Value>> = (1_000..1_000 + BATCH)
        .map(|i| vec![Value::Int8(i), Value::Int8(i % 7)])
        .collect();
    let o = observe(&db, "t", || {
        db.execute_many_params("INSERT INTO t (id, v) VALUES ($1, $2)", &rows)
            .expect("insert many");
    });
    assert_stable("embedded batch INSERT", "fast_batch", &o, BATCH);
}

/// An autocommit statement that takes no row lock commits through
/// `Transaction::commit` (here INSERT … SELECT, which no fast path serves).
#[test]
fn rr_snapshot_stable_across_statement_commit_without_row_locks() {
    let _serial = serial();
    let db = db_with_table("t");
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, v INT)")
        .expect("create src");
    db.execute(&format!("INSERT INTO src VALUES {}", values(5_000, BATCH)))
        .expect("seed src");
    let writer = db.create_wire_session("postgres").expect("writer session");
    let o = observe(&db, "t", || {
        db.execute_for_session(writer, "INSERT INTO t SELECT id, v FROM src")
            .expect("insert select");
    });
    assert_stable("statement commit", "transaction", &o, BATCH);
}

/// A session transaction that staged only INSERT rows (`insert_log`): BEGIN; INSERT;
/// COMMIT on a wire session.
#[test]
fn rr_snapshot_stable_across_insert_only_session_commit() {
    let _serial = serial();
    let db = db_with_table("t");
    let writer = db.create_wire_session("postgres").expect("writer session");
    db.begin_transaction_for_session(writer).expect("begin");
    db.execute_for_session(writer, &format!("INSERT INTO t VALUES {}", values(1_000, BATCH)))
        .expect("insert");
    let o = observe(&db, "t", || {
        db.commit_transaction_for_session(writer).expect("commit");
    });
    assert_stable("insert-only session commit", "transaction", &o, BATCH);
}

/// The same on the embedded API's own transaction (BEGIN; INSERT; COMMIT).
#[test]
fn rr_snapshot_stable_across_insert_only_embedded_commit() {
    let _serial = serial();
    let db = db_with_table("t");
    db.execute("BEGIN").expect("begin");
    db.execute(&format!("INSERT INTO t VALUES {}", values(1_000, BATCH)))
        .expect("insert");
    let o = observe(&db, "t", || {
        db.execute("COMMIT").expect("commit");
    });
    assert_stable("insert-only embedded commit", "transaction", &o, BATCH);
}

fn start_server(db: &Arc<EmbeddedDatabase>, rt: &tokio::runtime::Runtime) -> (String, std::net::SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    let server = PgServer::new(PgServerConfig::with_address(addr), Arc::clone(db)).expect("server");
    rt.spawn(async move {
        let _ = server.serve().await;
    });
    let conn = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    (conn, addr)
}

async fn connect(conn: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(conn, tokio_postgres::NoTls)
        .await
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// A protocol message length: the body plus the length field itself.
fn frame_len(body: usize) -> [u8; 4] {
    i32::try_from(body + 4).expect("message length").to_be_bytes()
}

/// COPY FROM STDIN on the simple-query protocol, with a blocking socket
/// (`tokio_postgres::Client::copy_in` prepares the statement, which this server runs
/// only as a simple query).
struct CopyClient(std::net::TcpStream);

impl CopyClient {
    fn connect(addr: std::net::SocketAddr) -> Self {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(30))).expect("timeout");
        let mut body = 196_608_i32.to_be_bytes().to_vec();
        for part in ["user", "postgres", "database", "postgres", ""] {
            body.extend_from_slice(part.as_bytes());
            body.push(0);
        }
        let mut msg = frame_len(body.len()).to_vec();
        msg.extend_from_slice(&body);
        stream.write_all(&msg).expect("startup");
        let mut client = Self(stream);
        client.read_until(b'Z');
        client
    }

    fn frame(&mut self, tag: u8, body: &[u8]) {
        let mut msg = vec![tag];
        msg.extend_from_slice(&frame_len(body.len()));
        msg.extend_from_slice(body);
        self.0.write_all(&msg).expect("write");
    }

    /// Read backend messages through the first `want`; panic on an error message.
    fn read_until(&mut self, want: u8) {
        loop {
            let mut header = [0u8; 5];
            self.0.read_exact(&mut header).expect("read header");
            let len =
                usize::try_from(i32::from_be_bytes([header[1], header[2], header[3], header[4]])).expect("length");
            let mut body = vec![0u8; len - 4];
            self.0.read_exact(&mut body).expect("read body");
            assert!(
                header[0] != b'E',
                "server error: {}",
                String::from_utf8_lossy(&body).replace('\0', " ")
            );
            if header[0] == want {
                return;
            }
        }
    }

    fn copy(&mut self, table: &str, first: i64, n: i64) {
        let mut sql = format!("COPY {table} FROM STDIN").into_bytes();
        sql.push(0);
        self.frame(b'Q', &sql);
        self.read_until(b'G');
        let mut text = String::new();
        for i in first..first + n {
            let _ = writeln!(text, "{i}\t{}", i % 7);
        }
        self.frame(b'd', text.as_bytes());
        self.frame(b'c', &[]);
        self.read_until(b'Z');
    }
}

async fn wire_count(client: &tokio_postgres::Client, table: &str) -> i64 {
    let messages = client
        .simple_query(&format!("SELECT count(*) FROM {table}"))
        .await
        .expect("count");
    messages
        .iter()
        .find_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => r.get(0).map(|v| v.parse().expect("int")),
            _ => None,
        })
        .expect("count row")
}

/// COPY FROM STDIN on the PostgreSQL wire: the fast batch funnel.
#[test]
fn rr_snapshot_stable_across_copy() {
    let _serial = serial();
    let db = db_with_table("t");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let (_, addr) = start_server(&db, &rt);
    let mut client = CopyClient::connect(addr);
    let o = observe(&db, "t", || client.copy("t", 1_000, BATCH));
    assert_stable("COPY", "fast_batch", &o, BATCH);
}

/// The reported shape, short: PostgreSQL wire clients commit autocommit multi-row
/// INSERTs and COPYs while a REPEATABLE READ reader counts three times per
/// transaction. Every write adds exactly `BATCH` rows.
#[test]
fn rr_snapshot_stable_under_wire_writers() {
    const RUN: Duration = Duration::from_secs(3);
    const MAX_ROWS: i64 = 40_000;
    let _serial = serial();
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE w (id INT PRIMARY KEY, v INT)")
        .expect("create");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(6)
        .enable_all()
        .build()
        .expect("runtime");
    let (conn, addr) = start_server(&db, &rt);

    let (anomalies, txns, total) = rt.block_on(async move {
        let stop = Arc::new(AtomicBool::new(false));
        let mut writers = Vec::new();
        for w in 0..3i64 {
            let conn = conn.clone();
            let stop = Arc::clone(&stop);
            writers.push(tokio::spawn(async move {
                let mut next = w * 100_000_000;
                let mut written = 0;
                if w == 2 {
                    return tokio::task::spawn_blocking(move || {
                        let mut copier = CopyClient::connect(addr);
                        while !stop.load(Ordering::Relaxed) && written < MAX_ROWS / 3 {
                            copier.copy("w", next, BATCH);
                            next += BATCH;
                            written += BATCH;
                        }
                    })
                    .await
                    .expect("copy writer");
                }
                let c = connect(&conn).await;
                while !stop.load(Ordering::Relaxed) && written < MAX_ROWS / 3 {
                    {
                        c.batch_execute(&format!("INSERT INTO w VALUES {}", values(next, BATCH)))
                            .await
                            .expect("insert");
                    }
                    next += BATCH;
                    written += BATCH;
                }
            }));
        }

        let reader = connect(&conn).await;
        let mut anomalies = Vec::new();
        let mut txns = 0u64;
        let start = Instant::now();
        while start.elapsed() < RUN {
            reader
                .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
                .await
                .expect("begin");
            let mut seen = [0i64; 3];
            for (i, slot) in seen.iter_mut().enumerate() {
                if i == 2 {
                    tokio::time::sleep(Duration::from_micros(200)).await;
                } else {
                    tokio::task::yield_now().await;
                }
                *slot = wire_count(&reader, "w").await;
            }
            reader.batch_execute("COMMIT").await.expect("commit");
            txns += 1;
            if seen[0] != seen[1] || seen[1] != seen[2] || seen.iter().any(|c| c % BATCH != 0) {
                anomalies.push(seen);
            }
        }
        stop.store(true, Ordering::Relaxed);
        for w in writers {
            w.await.expect("writer");
        }
        let total = wire_count(&reader, "w").await;
        (anomalies, txns, total)
    });
    eprintln!("reader transactions: {txns}; rows at end: {total}; anomalies: {anomalies:?}");
    assert!(txns > 0, "the reader ran no transaction");
    assert!(
        anomalies.is_empty(),
        "REPEATABLE READ snapshot changed or saw a partial write: {anomalies:?}"
    );
}
