//! Real managed daemons for the physical-standby tests (spec P2.1): a primary
//! and its standbys on private loopback ports, one replication token, each
//! standby following the primary's ONE replication port (`--primary-host` =
//! the primary's `--replication-port`).
//!
//! Included by path (`#[path = "support/physical_pair.rs"] mod physical_pair;`)
//! from each test target that drives daemons, so every target uses the same
//! harness; a target uses only part of it.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::replication::archive_catalog::{ArchiveCatalog, PartialPolicy};
use heliosdb_nano::replication::archive_segment::SegmentInfo;
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};
use tokio_postgres::{NoTls, SimpleQueryMessage};

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// An owner-only 32-byte token file of `byte`s.
pub fn token_file(dir: &Path, name: &str, byte: u8) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, [byte; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// Kills exactly the child it owns when dropped, so a failed assertion cannot
/// leave a daemon holding ports or a data directory.
pub struct Daemon {
    child: Option<Child>,
    log: PathBuf,
}

/// How [`Daemon::spawn_with`] starts a daemon beyond its arguments (spec
/// P2.2): another `--listen` address (a loopback alias such as `127.0.0.2`,
/// or `0.0.0.0`), and extra environment variables (test hooks).
#[derive(Default)]
pub struct SpawnOptions<'a> {
    /// `--listen`; `127.0.0.1` when `None`.
    pub listen: Option<&'a str>,
    pub env: Vec<(&'a str, String)>,
}

impl Daemon {
    /// `start` with trust auth on loopback, `token`, `args`, and an optional
    /// TOML `config`. Each spawn has its own log (`<name>.log`).
    pub fn spawn(dir: &Path, name: &str, data: &Path, token: &Path, args: &[String], config: Option<&str>) -> Self {
        Self::spawn_with(dir, name, data, token, args, config, &SpawnOptions::default())
    }

    /// [`Self::spawn`] with another `--listen` address and extra environment.
    pub fn spawn_with(
        dir: &Path,
        name: &str,
        data: &Path,
        token: &Path,
        args: &[String],
        config: Option<&str>,
        options: &SpawnOptions<'_>,
    ) -> Self {
        let log = dir.join(format!("{name}.log"));
        let file = std::fs::File::create(&log).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
        command
            .arg("start")
            .arg("--data-dir")
            .arg(data)
            .args(["--listen", options.listen.unwrap_or("127.0.0.1"), "--auth", "trust"])
            .arg("--replication-token-file")
            .arg(token)
            .args(args)
            // `HELIOSDB_TEST_DAEMON_LOG` lets a developer raise a daemon's log
            // filter (e.g. `heliosdb_nano::replication=trace`) without editing
            // the harness; the daemons' logs stay in the test directory.
            .env(
                "RUST_LOG",
                std::env::var("HELIOSDB_TEST_DAEMON_LOG").unwrap_or_else(|_| "info".to_string()),
            )
            .env_remove("HELIOSDB_NANO_READY_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::from(file.try_clone().unwrap()))
            .stderr(Stdio::from(file));
        for (key, value) in &options.env {
            command.env(key, value);
        }
        if let Some(config) = config {
            let path = dir.join(format!("{name}.toml"));
            std::fs::write(&path, config).unwrap();
            command.arg("--config").arg(path);
        }
        Self {
            child: Some(command.spawn().unwrap()),
            log,
        }
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// True once `needle` appears in the log, polling for up to `timeout`. A daemon
    /// answers a socket before its tracing line reaches the file, so a test that
    /// asserts on the log right after a reply must wait for it.
    pub fn log_contains_within(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.log().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The last `lines` lines of the log, for assertion messages.
    pub fn tail(&self, lines: usize) -> String {
        let log = self.log();
        let all: Vec<&str> = log.lines().collect();
        all[all.len().saturating_sub(lines)..].join(" | ")
    }

    pub fn alive(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| child.try_wait().unwrap().is_none())
    }

    fn signal(&self, signal: i32) {
        if let Some(child) = &self.child {
            // SAFETY: `kill` with a pid we own and a standard signal.
            unsafe { libc::kill(child.id() as i32, signal) };
        }
    }

    /// SIGSTOP: the process stops where it is; its sockets stay open.
    pub fn pause(&self) {
        self.signal(libc::SIGSTOP);
    }

    /// SIGCONT.
    pub fn resume(&self) {
        self.signal(libc::SIGCONT);
    }

    /// Wait up to `budget` for the daemon to exit on its own; `None` if it is still running.
    pub fn wait_exit(&mut self, budget: Duration) -> Option<ExitStatus> {
        let child = self.child.as_mut()?;
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                self.child = None;
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }

    /// SIGKILL: no shutdown path runs.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Graceful stop (SIGCONT, in case it is paused, then SIGTERM), then wait;
    /// a daemon that ignores it is killed.
    pub fn stop(&mut self) {
        self.resume();
        if let Some(mut child) = self.child.take() {
            // SAFETY: `kill` with a pid we own and a standard signal.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if child.try_wait().unwrap().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A primary and a standby on private ports, sharing one token. The standby's
/// `--primary-host` is the primary's `--replication-port`: one port, one
/// connection (spec P2.1).
pub struct Pair {
    pub dir: tempfile::TempDir,
    pub token: PathBuf,
    pub pg_p: u16,
    pub repl_p: u16,
    pub http_p: u16,
    pub pg_s: u16,
    pub http_s: u16,
    pub primary_args: Vec<String>,
    pub standby_args: Vec<String>,
}

pub fn pair() -> Pair {
    let dir = tempfile::tempdir().unwrap();
    let token = token_file(dir.path(), "replication.token", 0x42);
    let (pg_p, repl_p, http_p, pg_s, http_s) = (free_port(), free_port(), free_port(), free_port(), free_port());
    Pair {
        primary_args: primary_args(pg_p, repl_p, http_p),
        standby_args: standby_args(pg_s, repl_p, http_s),
        dir,
        token,
        pg_p,
        repl_p,
        http_p,
        pg_s,
        http_s,
    }
}

pub fn primary_args(pg: u16, replication: u16, http: u16) -> Vec<String> {
    vec![
        "--port".into(),
        pg.to_string(),
        "--replication-role".into(),
        "primary".into(),
        "--replication-port".into(),
        replication.to_string(),
        "--http-port".into(),
        http.to_string(),
    ]
}

pub fn standby_args(pg: u16, primary_replication: u16, http: u16) -> Vec<String> {
    vec![
        "--port".into(),
        pg.to_string(),
        "--replication-role".into(),
        "standby".into(),
        "--primary-host".into(),
        format!("127.0.0.1:{primary_replication}"),
        "--http-port".into(),
        http.to_string(),
        "--replication-startup-timeout-secs".into(),
        "30".into(),
    ]
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

pub fn http_get_blocking(port: u16, path: &str) -> Option<(u16, serde_json::Value)> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(3))).ok()?;
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes())
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let code = response.split_whitespace().nth(1)?.parse().ok()?;
    let body = response.split_once("\r\n\r\n")?.1;
    Some((code, serde_json::from_str(body).ok()?))
}

pub async fn http(port: u16, path: &'static str) -> Option<(u16, serde_json::Value)> {
    tokio::task::spawn_blocking(move || http_get_blocking(port, path))
        .await
        .unwrap()
}

/// Poll `path` until it answers `want`; panics with the last answer otherwise.
pub async fn wait_http(
    port: u16,
    path: &'static str,
    want: u16,
    budget: Duration,
    daemon: &Daemon,
) -> serde_json::Value {
    let deadline = Instant::now() + budget;
    loop {
        let seen = http(port, path).await;
        if let Some((code, body)) = &seen {
            if *code == want {
                return body.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "{path} never answered {want}; last {seen:?}; log tail: {}",
            daemon.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The standbys a primary's `/health` lists (spec P2.1).
pub async fn listed_standbys(http_port: u16) -> Vec<serde_json::Value> {
    match http(http_port, "/health").await {
        Some((200, body)) => body["standbys"].as_array().cloned().unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Wait until the primary lists a standby whose durable sequence reached `at_least`.
pub async fn wait_listed(http_port: u16, at_least: u64, budget: Duration, daemon: &Daemon) -> serde_json::Value {
    let deadline = Instant::now() + budget;
    loop {
        let listed = listed_standbys(http_port).await;
        if let Some(standby) = listed.iter().find(|standby| {
            standby["durable_sequence"]
                .as_u64()
                .is_some_and(|durable| durable >= at_least)
        }) {
            return standby.clone();
        }
        assert!(
            Instant::now() < deadline,
            "the primary never listed a standby durable at {at_least}; listed {listed:?}; log tail: {}",
            daemon.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL wire
// ---------------------------------------------------------------------------

pub async fn connect(port: u16) -> tokio_postgres::Client {
    connect_to("127.0.0.1", port).await
}

/// [`connect`] to a PostgreSQL listener on `host` (a loopback alias, spec P2.2).
pub async fn connect_to(host: &str, port: u16) -> tokio_postgres::Client {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let url = format!("host={host} port={port} user=postgres dbname=postgres connect_timeout=2");
        match tokio_postgres::connect(&url, NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(connection);
                return client;
            }
            Err(e) if Instant::now() > deadline => panic!("no connection on {port}: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(300)).await,
        }
    }
}

/// The one value of a one-row, one-column answer (simple protocol: the text
/// the wire carries).
pub async fn value(client: &tokio_postgres::Client, sql: &str) -> Option<String> {
    let messages = client.simple_query(sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    messages.iter().find_map(|message| match message {
        SimpleQueryMessage::Row(row) => Some(row.get(0).map(str::to_string)),
        _ => None,
    })?
}

pub fn sqlstate(error: &tokio_postgres::Error) -> String {
    error.code().map(|code| code.code().to_string()).unwrap_or_default()
}

pub fn message(error: &tokio_postgres::Error) -> String {
    error
        .as_db_error()
        .map(|db| db.message().to_string())
        .unwrap_or_default()
}

/// `HI/LO` hex, as PostgreSQL prints an LSN; returns the sequence.
pub fn lsn(text: &str) -> u64 {
    let (hi, lo) = text.split_once('/').unwrap_or_else(|| panic!("not an LSN: {text}"));
    (u64::from_str_radix(hi, 16).unwrap() << 32) | u64::from_str_radix(lo, 16).unwrap()
}

/// The primary's written physical sequence.
pub async fn primary_sequence(client: &tokio_postgres::Client) -> u64 {
    lsn(&value(client, "SELECT pg_current_wal_lsn()::text").await.unwrap())
}

pub async fn ids(client: &tokio_postgres::Client) -> Result<Vec<i32>, String> {
    let messages = client
        .simple_query("SELECT id FROM t ORDER BY id")
        .await
        .map_err(|e| format!("{}: {}", sqlstate(&e), e))?;
    Ok(messages
        .iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0).map(|id| id.parse().unwrap()),
            _ => None,
        })
        .collect())
}

/// Poll a fresh connection until the standby shows `want`; returns the last observation.
pub async fn settle(port: u16, want: &[i32], budget: Duration) -> Result<Vec<i32>, String> {
    settle_on("127.0.0.1", port, want, budget).await
}

/// [`settle`] on a PostgreSQL listener on `host`.
pub async fn settle_on(host: &str, port: u16, want: &[i32], budget: Duration) -> Result<Vec<i32>, String> {
    let deadline = Instant::now() + budget;
    loop {
        let seen = match tokio_postgres::connect(
            &format!("host={host} port={port} user=postgres dbname=postgres connect_timeout=2"),
            NoTls,
        )
        .await
        {
            Ok((client, connection)) => {
                tokio::spawn(connection);
                ids(&client).await
            }
            Err(e) => Err(e.to_string()),
        };
        if seen.as_deref() == Ok(want) || Instant::now() > deadline {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Every entry of `dir` with its size, so a refused directory can be shown untouched.
pub fn census(dir: &Path) -> BTreeSet<(String, u64)> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.metadata().unwrap().len(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// A standby away past its primary's WAL retention (spec P3.2 §5, P4.1 §5)
// ---------------------------------------------------------------------------

pub async fn create_table(client: &tokio_postgres::Client) {
    client
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .await
        .unwrap();
}

/// Insert `ids` one statement each; returns how many failed.
pub async fn insert(client: &tokio_postgres::Client, ids: std::ops::RangeInclusive<i32>, filler: &str) -> usize {
    let mut failed = 0;
    for id in ids {
        if client
            .batch_execute(&format!("INSERT INTO t VALUES ({id}, '{filler}')"))
            .await
            .is_err()
        {
            failed += 1;
        }
    }
    failed
}

/// Sealed, available segments of an archive directory, in sequence order.
pub fn sealed(dir: &Path) -> Vec<SegmentInfo> {
    let Ok((catalog, _)) = ArchiveCatalog::scan(dir, PartialPolicy::Leave) else {
        return Vec::new();
    };
    let mut segments: Vec<SegmentInfo> = catalog.entries().filter_map(|entry| entry.info.clone()).collect();
    segments.sort_by_key(SegmentInfo::first_sequence);
    segments
}

/// Wait until the archive's sealed segments reach `through`.
pub async fn wait_sealed_through(dir: &Path, through: u64, daemon: &Daemon) -> Vec<SegmentInfo> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let segments = sealed(dir);
        if segments.last().is_some_and(|last| last.last_sequence() >= through) {
            return segments;
        }
        assert!(
            Instant::now() < deadline,
            "{} never sealed through {through}: {:?}; log tail: {}",
            dir.display(),
            segments.last().map(SegmentInfo::last_sequence),
            daemon.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Drop every WAL file the primary archived: what RocksDB's retention does
/// to the WAL a standby that is away too long still needs. Returns how many
/// went.
pub fn prune_archived_wal(primary_data: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(primary_data.join("archive")) else {
        return 0;
    };
    let mut pruned = 0;
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|extension| extension == "log") && std::fs::remove_file(&path).is_ok() {
            pruned += 1;
        }
    }
    pruned
}

/// A primary with tiny memtables (each flush makes WAL files obsolete, so
/// retention can drop them while it runs) and the smallest WAL retention.
pub fn pruning_primary_config(archive: bool) -> String {
    format!(
        "[storage]\nrocksdb_write_buffer_size = 65536\nrocksdb_min_write_buffer_number_to_merge = 1\n\
         [replication.physical]\nwal_retention_secs = 60\nwal_retention_size_mb = 64\n\
         [replication.archive]\nenabled = {archive}\nsegment_max_secs = 1\n"
    )
}

/// The standby of `pair` follows its primary to row 1 and stops; the primary
/// writes on until its RocksDB WAL no longer holds the standby's next group
/// (the archived WAL files removed while it runs, after its own archive —
/// when on — sealed everything). Returns the primary, its client and the last
/// row id written.
pub async fn standby_away_past_retention(pair: &Pair, archive: bool) -> (Daemon, tokio_postgres::Client, i32) {
    standby_away_past_retention_with(pair, archive, None, &"w".repeat(400)).await
}

/// [`standby_away_past_retention`] with `extra` TOML appended to the
/// primary's and the standby's configuration (an `[encryption]` section, say)
/// and rows of `filler`.
pub async fn standby_away_past_retention_with(
    pair: &Pair,
    archive: bool,
    extra: Option<&str>,
    filler: &str,
) -> (Daemon, tokio_postgres::Client, i32) {
    let dir = pair.dir.path();
    let config = format!("{}{}", pruning_primary_config(archive), extra.unwrap_or_default());
    let primary_data = dir.join("primary");
    let primary = Daemon::spawn(
        dir,
        "primary",
        &primary_data,
        &pair.token,
        &pair.primary_args,
        Some(config.as_str()),
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=1, "first").await, 0);
    let mut standby = Daemon::spawn(
        dir,
        "standby-1",
        &dir.join("standby"),
        &pair.token,
        &pair.standby_args,
        extra,
    );
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    assert_eq!(settle(pair.pg_s, &[1], Duration::from_secs(30)).await, Ok(vec![1]));
    standby.stop();

    // Write until the WAL the standby needs is gone (tiny memtables flush
    // often; each flush moves WAL files to the retention archive, which
    // retention empties — here, the test).
    let mut last = 1;
    let mut pruned = 0;
    let deadline = Instant::now() + Duration::from_secs(120);
    while pruned == 0 {
        assert!(
            Instant::now() < deadline,
            "precondition: the primary never archived a WAL file; log tail: {}",
            primary.tail(12)
        );
        assert_eq!(insert(&writer, last + 1..=last + 150, filler).await, 0);
        last += 150;
        if archive {
            let through = primary_sequence(&writer).await;
            wait_sealed_through(&dir.join("primary.archive"), through, &primary).await;
        }
        pruned += prune_archived_wal(&primary_data);
    }
    (primary, writer, last)
}
