//! Sprinter ccebc95977f1: a statement never stalls its server.
//!
//! Every daemon here runs with `TOKIO_WORKER_THREADS=1`, so its one worker
//! always owns the I/O and timer driver: a statement that ran ON that worker
//! (the defect) stranded every other task — no new connection was accepted,
//! no other session read, no replication frame served — every time, not
//! sometimes. What each test pins:
//!
//! * (`internal-tests`) a statement held inside the engine
//!   (`heliosdb_test_hold()`) through each entry point — the PostgreSQL
//!   simple and extended protocols, a PostgreSQL explicit `BEGIN … COMMIT`,
//!   the MySQL wire and REST `POST /v1/branches/main/query`: while it holds,
//!   a NEW PostgreSQL connection completes startup and answers `SELECT 1`
//!   within 2 s, an already-open one within 1 s, `GET /health` answers, and
//!   the held statement answers once released;
//! * (`internal-tests`) the same hold with `HELIOSDB_NANO_STATEMENT_OFFLOAD=off`:
//!   the new connection is NOT served while the statement holds — the stall,
//!   so the tests above can fail (the hook holds inline since this change);
//! * (default tier, no hook) a statement calibrated to run at least 6 s: a new
//!   connection's `SELECT 1` answers while it is STILL running (from
//!   timestamps; a statement that ran too short fails as "calibration", it
//!   never passes); with the switch off the same probe answers only once the
//!   statement ended;
//! * (`internal-tests`, `ha-tier1`) a physical standby of such a primary,
//!   through a 10 s statement held on the PRIMARY: a write another session
//!   makes DURING the hold is acknowledged and replicated within 2 s, and the
//!   standby stays `streaming` with `lag_secs` under 2 (the standby used to
//!   flap `reconnecting` behind a primary's long statement); with the switch
//!   off on the primary, that write is not even acknowledged while the
//!   statement holds and the standby falls behind — both checks detect the
//!   stall.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[cfg(all(feature = "ha-tier1", feature = "internal-tests", target_os = "linux"))]
#[path = "support/physical_pair.rs"]
mod physical_pair;
#[path = "support/ports.rs"]
mod ports;

use heliosdb_nano::runtime_offload::STATEMENT_OFFLOAD_ENV;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio_postgres::{NoTls, SimpleQueryMessage};

/// `internal-tests`: `SELECT heliosdb_test_hold()` stays inside the engine
/// while the file this names exists (`sql::evaluator`).
#[cfg(feature = "internal-tests")]
const HOLD_STATEMENT_ENV: &str = "HELIOSDB_NANO_TEST_HOLD_STATEMENT_FILE";
/// The statement that holds.
#[cfg(feature = "internal-tests")]
const HOLD_SQL: &str = "SELECT heliosdb_test_hold()";
/// The log line the hook writes once it holds.
#[cfg(feature = "internal-tests")]
const HOLDING: &str = "this statement holds while";
/// A write that holds: it evaluates the hook for its one row of `held`.
#[cfg(feature = "internal-tests")]
const HOLD_UPDATE_SQL: &str = "UPDATE held SET flag = heliosdb_test_hold() WHERE id = 1";

/// A loopback port RESERVED for the life of the test process (sprinter
/// ffe805264aac, `support/ports.rs`): a port bound and released at once could
/// be taken by a concurrent test before the daemon binds it.
fn free_port() -> u16 {
    ports::free_port()
}

/// A standalone daemon on private loopback ports with ONE async worker,
/// killed when dropped.
struct Server {
    child: Child,
    log: PathBuf,
    pg: u16,
    #[cfg_attr(not(feature = "internal-tests"), allow(dead_code))]
    http: u16,
    _dir: tempfile::TempDir,
}

impl Server {
    /// `start` with trust auth, a data directory, extra environment and extra
    /// arguments.
    fn start(env: &[(&str, String)], args: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (pg, http) = (free_port(), free_port());
        let log = dir.path().join("server.log");
        let file = std::fs::File::create(&log).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
        command
            .arg("start")
            .arg("--data-dir")
            .arg(dir.path().join("data"))
            .args(["--listen", "127.0.0.1", "--auth", "trust"])
            .arg("--port")
            .arg(pg.to_string())
            .arg("--http-port")
            .arg(http.to_string())
            .args(args)
            .env("RUST_LOG", "info")
            .env("TOKIO_WORKER_THREADS", "1")
            .env_remove(STATEMENT_OFFLOAD_ENV)
            .env_remove("HELIOSDB_NANO_READY_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::from(file.try_clone().unwrap()))
            .stderr(Stdio::from(file));
        for (key, value) in env {
            command.env(key, value);
        }
        Self {
            child: command.spawn().unwrap(),
            log,
            pg,
            http,
            _dir: dir,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn tail(&self) -> String {
        let log = self.log();
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(12)..].join(" | ")
    }

    /// A connection, retried while the daemon starts.
    async fn connect(&self) -> tokio_postgres::Client {
        pg_connect(self.pg).await
    }

    /// Until `needle` is in the log.
    #[cfg_attr(not(feature = "internal-tests"), allow(dead_code))]
    async fn wait_log(&self, needle: &str, budget: Duration) {
        let deadline = Instant::now() + budget;
        while !self.log().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "the log never said {needle:?}; {}",
                self.tail()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn url(port: u16) -> String {
    format!("host=127.0.0.1 port={port} user=postgres dbname=postgres connect_timeout=2")
}

/// A PostgreSQL connection to `port`, retried for up to 60 s while the
/// daemon starts.
async fn pg_connect(port: u16) -> tokio_postgres::Client {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match tokio_postgres::connect(&url(port), NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(connection);
                return client;
            }
            Err(e) if Instant::now() > deadline => panic!("no connection on {port}: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
}

/// The first column of the first row of a simple query.
fn first_value(messages: &[SimpleQueryMessage]) -> Option<String> {
    messages.iter().find_map(|message| match message {
        SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
        _ => None,
    })
}

/// ONE fresh connection (startup included) answering `SELECT 1`.
async fn select_one_on_a_new_connection(port: u16) -> Result<String, String> {
    let (client, connection) = tokio_postgres::connect(&url(port), NoTls)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    tokio::spawn(connection);
    let messages = client
        .simple_query("SELECT 1")
        .await
        .map_err(|e| format!("SELECT 1: {e}"))?;
    first_value(&messages).ok_or_else(|| "SELECT 1 returned no row".to_string())
}

/// One HTTP/1.1 request (`Connection: close`): the status code and the whole
/// response, or `None` when it does not answer within `timeout`.
#[cfg_attr(not(feature = "internal-tests"), allow(dead_code))]
fn http_request(port: u16, method: &str, path: &str, body: Option<&str>, timeout: Duration) -> Option<(u16, String)> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(3))).ok()?;
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some(body) = body {
        let length = body.len();
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {length}\r\n\r\n{body}"
        ));
    } else {
        request.push_str("\r\n");
    }
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let code = response.split_whitespace().nth(1)?.parse().ok()?;
    Some((code, response))
}

// ---------------------------------------------------------------------------
// A minimal MySQL text-protocol client (`internal-tests`)
// ---------------------------------------------------------------------------

#[cfg(feature = "internal-tests")]
struct MySqlClient {
    stream: tokio::net::TcpStream,
}

#[cfg(feature = "internal-tests")]
impl MySqlClient {
    const COM_QUERY: u8 = 0x03;
    const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
    const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;

    /// Log in as `root` with an empty auth response (the listener is
    /// trust-only), retrying while it starts.
    async fn connect(port: u16) -> Result<Self, String> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let stream = loop {
            match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                Ok(stream) => break stream,
                Err(e) if Instant::now() > deadline => return Err(format!("MySQL connect: {e}")),
                Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        };
        let mut client = Self { stream };
        client.read_packet().await?;
        let mut login = Vec::new();
        login.extend_from_slice(&(Self::CLIENT_PROTOCOL_41 | Self::CLIENT_SECURE_CONNECTION).to_le_bytes());
        login.extend_from_slice(&(1u32 << 24).to_le_bytes());
        login.push(45); // utf8mb4_general_ci
        login.extend_from_slice(&[0u8; 23]);
        login.extend_from_slice(b"root\0");
        login.push(0); // empty auth response
        client.write_packet(1, &login).await?;
        let ok = client.read_packet().await?;
        if ok.first() != Some(&0x00) {
            return Err(format!("MySQL login refused: {}", String::from_utf8_lossy(&ok)));
        }
        Ok(client)
    }

    async fn write_packet(&mut self, sequence: u8, payload: &[u8]) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let len = payload.len();
        let header = [
            (len & 0xFF) as u8,
            ((len >> 8) & 0xFF) as u8,
            ((len >> 16) & 0xFF) as u8,
            sequence,
        ];
        self.stream.write_all(&header).await.map_err(|e| e.to_string())?;
        self.stream.write_all(payload).await.map_err(|e| e.to_string())?;
        self.stream.flush().await.map_err(|e| e.to_string())
    }

    async fn read_packet(&mut self) -> Result<Vec<u8>, String> {
        use tokio::io::AsyncReadExt;
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header).await.map_err(|e| e.to_string())?;
        let len = usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).await.map_err(|e| e.to_string())?;
        Ok(payload)
    }

    /// `COM_QUERY`; the first column of the first row of its result set
    /// (short values only: a one-byte length prefix).
    async fn first_value(&mut self, sql: &str) -> Result<String, String> {
        let mut query = vec![Self::COM_QUERY];
        query.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &query).await?;
        let first = self.read_packet().await?;
        let columns = match first.first().copied() {
            Some(0xFF) => return Err(format!("MySQL error: {}", String::from_utf8_lossy(&first))),
            Some(count) if count > 0 && count < 0xFB => usize::from(count),
            _ => return Err(format!("not a result set: {first:?}")),
        };
        for _ in 0..columns {
            self.read_packet().await?;
        }
        self.read_packet().await?; // EOF after the column definitions
        let mut value = None;
        loop {
            let packet = self.read_packet().await?;
            if packet.first() == Some(&0xFE) && packet.len() < 9 {
                break;
            }
            if value.is_none() {
                let len = usize::from(packet[0]);
                // A NULL (0xFB) or a longer value reads as empty.
                let text = packet.get(1..1 + len).unwrap_or_default();
                value = Some(String::from_utf8_lossy(text).into_owned());
            }
        }
        value.ok_or_else(|| "the result set had no row".to_string())
    }
}

// ---------------------------------------------------------------------------
// A statement held inside the engine (`internal-tests`)
// ---------------------------------------------------------------------------

/// The entry point a held statement reaches the engine through.
#[cfg(feature = "internal-tests")]
#[derive(Clone, Copy, Debug)]
enum HeldVia {
    PgSimple,
    PgExtended,
    /// An autocommit UPDATE over the extended protocol: Execute's write arm,
    /// the route a statement psycopg prepares once `prepare_threshold` is
    /// reached takes (4.42.0 pre-release review, item P4).
    PgExtendedWrite,
    /// The same UPDATE with RETURNING: Execute's DML-RETURNING arm.
    PgExtendedWriteReturning,
    PgInTransaction,
    MySql,
    Rest,
}

/// Run [`HOLD_SQL`] through `via` on a connection of its own; its answer
/// once released (`t`, `true` or `1`, by protocol).
#[cfg(feature = "internal-tests")]
async fn hold_one_statement(via: HeldVia, pg: u16, http: u16, mysql: u16) -> Result<String, String> {
    match via {
        HeldVia::PgSimple => {
            let client = pg_connect(pg).await;
            let messages = client.simple_query(HOLD_SQL).await.map_err(|e| e.to_string())?;
            first_value(&messages).ok_or_else(|| "no row".to_string())
        }
        // Parse/Describe, then Bind/Execute: the extended protocol's
        // planning and execution entry points.
        HeldVia::PgExtended => {
            let client = pg_connect(pg).await;
            // The hook's result type is not inferred over the extended protocol
            // (a binary BOOLEAN sent as TEXT is refused), so read it as text.
            let row = client
                .query_one("SELECT heliosdb_test_hold()::text", &[])
                .await
                .map_err(|e| e.to_string())?;
            let answer = row.try_get::<_, bool>(0).map(|answer| answer.to_string());
            Ok(answer.or_else(|_| row.try_get::<_, String>(0)).unwrap_or_default())
        }
        // Parse/Bind/Execute of a write (no parameters, so no inferred
        // parameter type is involved): `t` once it updated its one row.
        HeldVia::PgExtendedWrite => {
            let client = pg_connect(pg).await;
            let updated = client.execute(HOLD_UPDATE_SQL, &[]).await.map_err(|e| e.to_string())?;
            if updated == 1 {
                Ok("t".to_string())
            } else {
                Err(format!("the held UPDATE updated {updated} rows"))
            }
        }
        HeldVia::PgExtendedWriteReturning => {
            let client = pg_connect(pg).await;
            let row = client
                .query_one(&format!("{HOLD_UPDATE_SQL} RETURNING id::text"), &[])
                .await
                .map_err(|e| e.to_string())?;
            row.try_get::<_, String>(0).map_err(|e| e.to_string())
        }
        // Inside an explicit transaction block, then committed.
        HeldVia::PgInTransaction => {
            let client = pg_connect(pg).await;
            client.batch_execute("BEGIN").await.map_err(|e| e.to_string())?;
            let messages = client.simple_query(HOLD_SQL).await.map_err(|e| e.to_string())?;
            client.batch_execute("COMMIT").await.map_err(|e| e.to_string())?;
            first_value(&messages).ok_or_else(|| "no row".to_string())
        }
        HeldVia::MySql => MySqlClient::connect(mysql).await?.first_value(HOLD_SQL).await,
        HeldVia::Rest => {
            let body = format!(r#"{{"sql":"{HOLD_SQL}"}}"#);
            let path = "/v1/branches/main/query";
            let request = move || http_request(http, "POST", path, Some(&body), Duration::from_secs(150));
            let response = tokio::task::spawn_blocking(request).await.map_err(|e| e.to_string())?;
            match response {
                Some((200, text)) if text.contains("\"row_count\":1") => Ok("t".to_string()),
                other => Err(format!("REST query answered {other:?}")),
            }
        }
    }
}

/// Until the daemon logs that the statement holds; a holder that ends first
/// fails the test with its outcome.
#[cfg(feature = "internal-tests")]
async fn wait_until_held(log: impl Fn() -> String, held: &mut tokio::task::JoinHandle<Result<String, String>>) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !log().contains(HOLDING) {
        if held.is_finished() {
            let outcome = held.await;
            panic!("the statement ended before it held: {outcome:?}");
        }
        assert!(Instant::now() < deadline, "the statement never held");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Hold one statement through `via`, probe the server while it holds,
/// release it and check its answer.
#[cfg(feature = "internal-tests")]
async fn the_server_serves_while_a_statement_holds(via: HeldVia) {
    let hold_dir = tempfile::tempdir().unwrap();
    let hold = hold_dir.path().join("hold");
    let mysql = free_port();
    let mysql_listen = format!("127.0.0.1:{mysql}");
    let server = Server::start(
        &[(HOLD_STATEMENT_ENV, hold.display().to_string())],
        &["--mysql", "--mysql-listen", &mysql_listen],
    );
    let other = server.connect().await;
    // The row the write variants hold an UPDATE of.
    other
        .batch_execute("CREATE TABLE held (id INT PRIMARY KEY, flag BOOLEAN)")
        .await
        .unwrap();
    other.batch_execute("INSERT INTO held VALUES (1, false)").await.unwrap();
    std::fs::write(&hold, b"").unwrap();
    let mut held = tokio::spawn(hold_one_statement(via, server.pg, server.http, mysql));
    wait_until_held(|| server.log(), &mut held).await;

    // A NEW connection: accepted, started up, answered.
    let started = Instant::now();
    let fresh = tokio::time::timeout(Duration::from_secs(2), select_one_on_a_new_connection(server.pg)).await;
    assert_eq!(
        fresh,
        Ok(Ok("1".to_string())),
        "{via:?}: a new connection must be served within 2 s while a statement holds (waited {:?}); {}",
        started.elapsed(),
        server.tail()
    );
    // An already-open connection.
    let open = tokio::time::timeout(Duration::from_secs(1), other.simple_query("SELECT 1")).await;
    let open = open.unwrap_or_else(|_| panic!("{via:?}: an open connection must answer in 1 s; {}", server.tail()));
    assert_eq!(first_value(&open.unwrap()).as_deref(), Some("1"));
    // The HTTP listener.
    let http = server.http;
    let health = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || http_request(http, "GET", "/health", None, Duration::from_secs(3))),
    )
    .await;
    let status = health.ok().and_then(Result::ok).flatten().map(|(code, _)| code);
    assert_eq!(
        status,
        Some(200),
        "{via:?}: GET /health must answer while a statement holds; {}",
        server.tail()
    );
    // Non-vacuity: everything above ran WHILE the statement was held.
    assert!(!held.is_finished(), "{via:?}: the statement stopped holding too early");

    std::fs::remove_file(&hold).unwrap();
    let answer = tokio::time::timeout(Duration::from_secs(60), held)
        .await
        .unwrap_or_else(|_| panic!("{via:?}: the released statement never answered; {}", server.tail()))
        .unwrap();
    assert!(
        matches!(answer.as_deref(), Ok("t" | "true" | "1")),
        "{via:?}: the released statement answers true: {answer:?}; {}",
        server.tail()
    );
}

#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simple_protocol_a_held_statement_leaves_the_server_serving() {
    the_server_serves_while_a_statement_holds(HeldVia::PgSimple).await;
}

#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extended_protocol_a_held_statement_leaves_the_server_serving() {
    the_server_serves_while_a_statement_holds(HeldVia::PgExtended).await;
}

#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extended_protocol_a_held_write_leaves_the_server_serving() {
    the_server_serves_while_a_statement_holds(HeldVia::PgExtendedWrite).await;
}

#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extended_protocol_a_held_write_returning_leaves_the_server_serving() {
    the_server_serves_while_a_statement_holds(HeldVia::PgExtendedWriteReturning).await;
}

#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_statement_inside_begin_commit_leaves_the_server_serving() {
    the_server_serves_while_a_statement_holds(HeldVia::PgInTransaction).await;
}

#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql_wire_a_held_statement_leaves_the_server_serving() {
    the_server_serves_while_a_statement_holds(HeldVia::MySql).await;
}

#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rest_query_a_held_statement_leaves_the_server_serving() {
    the_server_serves_while_a_statement_holds(HeldVia::Rest).await;
}

/// The switch off: the held statement keeps the only worker, and the server
/// does not serve a new connection until it is released — so the tests above
/// detect the stall, and `HELIOSDB_NANO_STATEMENT_OFFLOAD=off` works.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_offload_off_a_held_statement_stalls_a_new_connection() {
    let hold_dir = tempfile::tempdir().unwrap();
    let hold = hold_dir.path().join("hold");
    let server = Server::start(
        &[
            (HOLD_STATEMENT_ENV, hold.display().to_string()),
            (STATEMENT_OFFLOAD_ENV, "off".to_string()),
        ],
        &[],
    );
    server.connect().await;
    server.wait_log(STATEMENT_OFFLOAD_ENV, Duration::from_secs(10)).await;
    std::fs::write(&hold, b"").unwrap();
    let mut held = tokio::spawn(hold_one_statement(HeldVia::PgSimple, server.pg, 0, 0));
    wait_until_held(|| server.log(), &mut held).await;

    let probe = tokio::spawn(select_one_on_a_new_connection(server.pg));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !probe.is_finished(),
        "with the switch off a new connection was served while a statement held the only worker: the \
         held-statement tests cannot detect the stall; {}",
        server.tail()
    );
    assert!(!held.is_finished());

    std::fs::remove_file(&hold).unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(60), held)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answered.as_deref(), Ok("t"));
    let probed = tokio::time::timeout(Duration::from_secs(30), probe)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(probed, Ok("1".to_string()), "served once released");
}

// ---------------------------------------------------------------------------
// Which statements leave the worker (`internal-tests`)
// ---------------------------------------------------------------------------

/// The server's own count of handoffs (`heliosdb_test_offloads()`). The
/// reading statement is a general one, so it is itself counted once; a fresh
/// text each time keeps any cache out of it.
#[cfg(feature = "internal-tests")]
async fn handoffs(client: &tokio_postgres::Client, reads: &mut u32) -> u64 {
    *reads += 1;
    let messages = client
        .simple_query(&format!("SELECT heliosdb_test_offloads(), {reads}"))
        .await
        .unwrap();
    first_value(&messages).unwrap().parse().unwrap()
}

/// sprinter ccebc95977f1: a primary-key point read — simple protocol (the
/// `SELECT *` fast path and the A2 normalized plan) and a prepared statement
/// over the extended protocol — runs on the async worker
/// (a handoff costs a point read ~10–40 %), while a range read, an aggregate
/// and a write leave it. Counted by the server itself between two counter
/// reads: the second read's own handoff makes a bounded statement show 1 and
/// a general one 2.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn point_reads_stay_on_the_worker_and_general_statements_leave_it() {
    let server = Server::start(&[], &[]);
    let client = server.connect().await;
    client
        .batch_execute("CREATE TABLE bench (id INT PRIMARY KEY, v TEXT)")
        .await
        .unwrap();
    for id in 1..=20 {
        client
            .batch_execute(&format!("INSERT INTO bench VALUES ({id}, 'v{id}')"))
            .await
            .unwrap();
    }
    // Warm the A2 normalized plan (admitted to the plan cache on a repeat).
    for id in 1..=5 {
        let messages = client
            .simple_query(&format!("SELECT v FROM bench WHERE id = {id}"))
            .await
            .unwrap();
        assert_eq!(first_value(&messages), Some(format!("v{id}")));
    }
    let point = client.prepare("SELECT v FROM bench WHERE id = $1").await.unwrap();
    let range = client
        .prepare("SELECT v FROM bench WHERE id BETWEEN $1 AND $2")
        .await
        .unwrap();
    let mut reads = 0;

    for (sql, want) in [
        ("SELECT v FROM bench WHERE id = 7", "v7"),
        ("SELECT * FROM bench WHERE id = 8", "8"),
    ] {
        let before = handoffs(&client, &mut reads).await;
        let messages = client.simple_query(sql).await.unwrap();
        let after = handoffs(&client, &mut reads).await;
        assert_eq!(first_value(&messages).as_deref(), Some(want), "{sql}");
        assert_eq!(after - before, 1, "a point read must not leave the worker: {sql}");
    }
    let before = handoffs(&client, &mut reads).await;
    let rows = client.query(&point, &[&10i32]).await.unwrap();
    let after = handoffs(&client, &mut reads).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "v10");
    assert_eq!(after - before, 1, "a prepared point read must not leave the worker");

    for sql in [
        "SELECT v FROM bench WHERE id BETWEEN 2 AND 5",
        "SELECT count(*) FROM bench",
        "UPDATE bench SET v = 'w' WHERE id = 3",
    ] {
        let before = handoffs(&client, &mut reads).await;
        client.simple_query(sql).await.unwrap();
        let after = handoffs(&client, &mut reads).await;
        assert_eq!(after - before, 2, "a general statement leaves the worker: {sql}");
    }
    let before = handoffs(&client, &mut reads).await;
    let rows = client.query(&range, &[&2i32, &5i32]).await.unwrap();
    let after = handoffs(&client, &mut reads).await;
    assert_eq!(rows.len(), 4);
    assert_eq!(after - before, 2, "a prepared range read leaves the worker once");
}

// ---------------------------------------------------------------------------
// A long statement, no hook (default tier)
// ---------------------------------------------------------------------------

/// A read whose cost grows linearly with `width`: every row of the series
/// builds a `width`-fold string (no row matches). Not `LIKE`: its per-row cost
/// is a constant that dwarfs `width` today (sprinter 56e0ff79399b), so it
/// would not calibrate.
fn long_sql(width: u64) -> String {
    format!("SELECT count(*) FROM generate_series(1, 200000) AS g WHERE repeat(g::text, {width}) = 'x'")
}

/// The text of a statement that runs for at least 6 s on this build, found
/// by measuring; each run uses another text, so no result cache answers.
/// The margin over the 3 s the probes need absorbs load changes.
async fn calibrated_long_statement(client: &tokio_postgres::Client) -> (String, Duration) {
    let mut width: u64 = 2000;
    for _ in 0..12 {
        let sql = long_sql(width);
        let started = Instant::now();
        let messages = client
            .simple_query(&sql)
            .await
            .unwrap_or_else(|e| panic!("calibration: {sql}: {e}"));
        let took = started.elapsed();
        assert_eq!(first_value(&messages).as_deref(), Some("0"), "calibration: {sql}");
        if took >= Duration::from_secs(6) {
            return (long_sql(width + 1), took);
        }
        let factor = (7.0 / took.as_secs_f64().max(0.01)).clamp(1.5, 40.0);
        width = (width as f64 * factor).ceil() as u64;
    }
    panic!("calibration: the statement never ran for 6 s (width {width})");
}

/// Start the calibrated statement, probe with a new connection 0.5 s in.
/// Returns how long the statement ran, whether the probe was answered
/// before the statement ended, and the probe's own latency.
async fn probe_during_a_long_statement(server: &Server) -> (Duration, bool, Duration) {
    let long = server.connect().await;
    let (sql, calibrated) = calibrated_long_statement(&long).await;
    let started = Instant::now();
    let running = tokio::spawn(async move {
        let outcome = long.simple_query(&sql).await.map(|_| ()).map_err(|e| e.to_string());
        (outcome, Instant::now())
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let probe_started = Instant::now();
    let probed = tokio::time::timeout(Duration::from_secs(120), select_one_on_a_new_connection(server.pg))
        .await
        .unwrap_or_else(|_| panic!("the probe never answered; {}", server.tail()));
    let probe_answered = Instant::now();
    assert_eq!(probed, Ok("1".to_string()), "{}", server.tail());
    let (outcome, finished) = running.await.unwrap();
    outcome.unwrap_or_else(|e| panic!("the long statement failed: {e}; {}", server.tail()));
    let ran_for = finished - started;
    assert!(
        ran_for >= Duration::from_secs(3),
        "calibration: the long statement ran {ran_for:?} (it measured {calibrated:?}); it must run at least 3 s \
         for the probe to mean anything"
    );
    (ran_for, probe_answered < finished, probe_answered - probe_started)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_connection_is_served_while_a_long_statement_runs() {
    let server = Server::start(&[], &[]);
    let (ran_for, during, latency) = probe_during_a_long_statement(&server).await;
    assert!(
        during,
        "the new connection was answered only after the {ran_for:?} statement ended: it stalled the server; {}",
        server.tail()
    );
    assert!(
        latency < Duration::from_secs(2),
        "the new connection took {latency:?} while a statement ran; {}",
        server.tail()
    );
}

/// The switch off reproduces the stall with the same probe — so the test
/// above detects it, and the switch works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_offload_off_a_long_statement_stalls_a_new_connection() {
    let server = Server::start(&[(STATEMENT_OFFLOAD_ENV, "off".to_string())], &[]);
    let (ran_for, _, latency) = probe_during_a_long_statement(&server).await;
    // The probe started 0.5 s into the statement; stalled, it waited for the
    // rest of it (less scheduling slack).
    let rest = ran_for.saturating_sub(Duration::from_millis(500));
    assert!(
        latency + Duration::from_millis(500) >= rest,
        "with the switch off the new connection answered after {latency:?}, {rest:?} before the {ran_for:?} \
         statement ended: the probe cannot detect the stall; {}",
        server.tail()
    );
    assert!(server.log().contains(STATEMENT_OFFLOAD_ENV), "logs that it is off");
}

// ---------------------------------------------------------------------------
// Replication through a statement held on the primary
// ---------------------------------------------------------------------------

/// A primary with one worker (the switch off when `offload_off`) and its
/// standby; a statement holds the primary for 10 s.
///
/// Switch on: a write another session makes during the hold is acknowledged
/// and on the standby within 2 s, and every `/health` of the standby says
/// `streaming` with `lag_secs` under 2 — `lag_secs` is the time since the
/// standby last confirmed it held everything its primary had, which takes a
/// poll the primary answers, so it grows while the primary's I/O stalls.
///
/// Switch off (the negative control): the write is not even acknowledged
/// while the statement holds, and the standby falls behind or stops
/// streaming — so both checks above detect the stall.
#[cfg(all(feature = "ha-tier1", feature = "internal-tests", target_os = "linux"))]
async fn standby_through_a_statement_held_on_its_primary(offload_off: bool) {
    use physical_pair::{connect, create_table, http, insert, pair, settle, wait_http, Daemon, SpawnOptions};

    let hold_dir = tempfile::tempdir().unwrap();
    let hold = hold_dir.path().join("hold");
    let pair = pair();
    let dir = pair.dir.path();
    let mut env = vec![
        ("TOKIO_WORKER_THREADS", "1".to_string()),
        (HOLD_STATEMENT_ENV, hold.display().to_string()),
    ];
    if offload_off {
        env.push((STATEMENT_OFFLOAD_ENV, "off".to_string()));
    }
    let options = SpawnOptions { listen: None, env };
    let primary = Daemon::spawn_with(
        dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        None,
        &options,
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=1, "before").await, 0);
    let standby = Daemon::spawn(
        dir,
        "standby",
        &dir.join("standby"),
        &pair.token,
        &pair.standby_args,
        None,
    );
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    assert_eq!(settle(pair.pg_s, &[1], Duration::from_secs(30)).await, Ok(vec![1]));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let block = http(pair.http_s, "/health")
            .await
            .map(|(_, body)| body["replication"].clone());
        if block.as_ref().is_some_and(|block| block["state"] == "streaming") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the standby never streamed: {block:?}; {}",
            standby.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    std::fs::write(&hold, b"").unwrap();
    let mut held = tokio::spawn(hold_one_statement(HeldVia::PgSimple, pair.pg_p, 0, 0));
    wait_until_held(|| primary.log(), &mut held).await;
    let held_at = Instant::now();
    // A write on ANOTHER, already-open session of the primary, during the hold.
    let write = tokio::spawn(async move {
        writer
            .batch_execute("INSERT INTO t VALUES (2, 'during the hold')")
            .await
            .map_err(|e| e.to_string())
    });
    if offload_off {
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !write.is_finished(),
            "with the switch off the primary served another session's write while a statement held its only \
             worker: the write check cannot detect the stall; {}",
            primary.tail(12)
        );
    } else {
        let written = tokio::time::timeout(Duration::from_secs(2), write).await;
        assert!(
            matches!(written, Ok(Ok(Ok(())))),
            "a write on another session was not acknowledged within 2 s while a statement held the primary: \
             {written:?}; {}",
            primary.tail(12)
        );
        assert_eq!(
            settle(pair.pg_s, &[1, 2], Duration::from_secs(2)).await,
            Ok(vec![1, 2]),
            "the write made during the hold must reach the standby within 2 s"
        );
    }
    // The standby's own view, for the rest of the 10 s hold.
    let mut fell_behind = None;
    while held_at.elapsed() < Duration::from_secs(10) {
        let body = http(pair.http_s, "/health")
            .await
            .map(|(_, body)| body)
            .unwrap_or_default();
        let block = body["replication"].clone();
        let lag = block["lag_secs"].as_f64().unwrap_or(f64::MAX);
        if block["state"] != "streaming" || lag >= 2.0 {
            assert!(
                offload_off,
                "the standby fell behind (lag {lag} s) or stopped streaming while a statement held its \
                 primary: {block}; {}",
                standby.tail(12)
            );
            fell_behind.get_or_insert(block);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if offload_off {
        assert!(
            fell_behind.is_some(),
            "with the switch off the standby stayed streaming and current through a 10 s stall of its primary: \
             the lag check cannot detect the stall"
        );
    }
    assert!(!held.is_finished(), "the statement stopped holding too early");

    std::fs::remove_file(&hold).unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(60), held)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answered.as_deref(), Ok("t"));
    assert_eq!(
        settle(pair.pg_s, &[1, 2], Duration::from_secs(60)).await,
        Ok(vec![1, 2]),
        "once released, the write is on the standby"
    );
}

#[cfg(all(feature = "ha-tier1", feature = "internal-tests", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standby_keeps_streaming_through_a_statement_held_on_its_primary() {
    standby_through_a_statement_held_on_its_primary(false).await;
}

#[cfg(all(feature = "ha-tier1", feature = "internal-tests", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_offload_off_a_statement_held_on_the_primary_stalls_its_replication() {
    standby_through_a_statement_held_on_its_primary(true).await;
}
