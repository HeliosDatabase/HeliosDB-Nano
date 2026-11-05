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
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};
use tokio_postgres::{NoTls, SimpleQueryMessage};

#[path = "ports.rs"]
mod ports;

/// A private loopback port for a daemon, RESERVED for the life of the test
/// process so that no concurrent test (nor any other `bind(…:0)`) can take it
/// before the daemon binds it — see `support/ports.rs` for the choice (spec R6
/// §2(c), sprinter ffe805264aac).
pub fn free_port() -> u16 {
    ports::free_port()
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
    /// The spawn's name (`<name>.log`), for diagnostics.
    name: String,
    /// When the process was spawned (spec R6 §1: the time to its first HTTP
    /// answer is measured from here).
    spawned_at: Instant,
    /// When a wait helper first saw ANY HTTP answer from it.
    first_answer: Mutex<Option<Instant>>,
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
        let spawned_at = Instant::now();
        Self {
            child: Some(command.spawn().unwrap()),
            log,
            name: name.to_string(),
            spawned_at,
            first_answer: Mutex::new(None),
        }
    }

    /// `heliosdb-nano <args>` as written (not `start`): a command that goes on
    /// to serve in its own process, such as `recover --then primary`. Its own
    /// log (`<name>.log`); killed when dropped, like any daemon.
    pub fn spawn_cli(dir: &Path, name: &str, args: &[String]) -> Self {
        let log = dir.join(format!("{name}.log"));
        let file = std::fs::File::create(&log).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
            .args(args)
            .env(
                "RUST_LOG",
                std::env::var("HELIOSDB_TEST_DAEMON_LOG").unwrap_or_else(|_| "info".to_string()),
            )
            .env_remove("HELIOSDB_NANO_READY_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::from(file.try_clone().unwrap()))
            .stderr(Stdio::from(file))
            .spawn()
            .unwrap();
        Self {
            child: Some(child),
            log,
            name: name.to_string(),
            spawned_at: Instant::now(),
            first_answer: Mutex::new(None),
        }
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The OS pid while this handle owns the process (`None` once it was
    /// stopped or killed through it).
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// What the readiness diagnostics read of this daemon ([`Watched`]).
    pub fn watched(&self) -> Watched<'_> {
        Watched {
            name: &self.name,
            pid: self.pid(),
            log: &self.log,
            spawned_at: Some(self.spawned_at),
            first_answer: Some(&self.first_answer),
        }
    }

    /// The readiness-failure report for this daemon and `port` (spec R6 §1,
    /// [`readiness_report`]): for the panic message of a wait on its HTTP
    /// endpoint.
    pub fn diagnose(&self, port: u16) -> String {
        readiness_report(&self.watched(), port)
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

    /// SIGTERM, without waiting: the test watches what the daemon does with it.
    pub fn terminate(&self) {
        self.signal(libc::SIGTERM);
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

/// How long a probe waits for its TCP connect, and then for each read of the
/// response (the harness's values since P1.3).
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(3);
/// A response still incomplete after this long is a hung request, however
/// slowly its bytes trickle in.
const HTTP_REQUEST_BUDGET: Duration = Duration::from_secs(10);

/// What one `GET` to a daemon's HTTP port came back with (spec R6 §2(b)). The
/// wait helpers keep polling on every outcome but the answer they want, and
/// say at their deadline how often each was seen.
#[derive(Clone, Debug)]
pub enum HttpProbe {
    /// A complete response with a JSON body.
    Answered { code: u16, body: serde_json::Value },
    /// Nothing listens on the port (`ECONNREFUSED`): not bound yet, bound by
    /// nobody (a failed bind), or the process is gone.
    Refused,
    /// The TCP connect did not complete within [`HTTP_CONNECT_TIMEOUT`] (a
    /// full accept backlog, a stopped process).
    ConnectTimeout,
    /// Connected and asked, but no complete response within the request's
    /// timeouts: something accepted the connection (or the kernel queued it)
    /// and nothing answered — a stalled accept loop or handler.
    Hung { received: usize },
    /// The peer closed the connection before sending anything.
    Closed { received: usize },
    /// A response that is not HTTP with a JSON body (`head`: its first
    /// characters) — another process's listener on the port, say.
    Garbled {
        code: Option<u16>,
        received: usize,
        head: String,
    },
    /// Any other socket error.
    Failed(String),
}

impl HttpProbe {
    /// Its name in a wait's tally.
    fn kind(&self) -> &'static str {
        match self {
            Self::Answered { .. } => "answered",
            Self::Refused => "refused",
            Self::ConnectTimeout => "connect-timeout",
            Self::Hung { .. } => "hung",
            Self::Closed { .. } => "closed",
            Self::Garbled { .. } => "garbled",
            Self::Failed(_) => "failed",
        }
    }
}

/// One `GET path` on `127.0.0.1:port`, classified ([`HttpProbe`]).
pub fn http_probe_blocking(port: u16, path: &str) -> HttpProbe {
    use std::io::ErrorKind;
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = match TcpStream::connect_timeout(&address, HTTP_CONNECT_TIMEOUT) {
        Ok(stream) => stream,
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => return HttpProbe::Refused,
        Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
            return HttpProbe::ConnectTimeout
        }
        Err(error) => return HttpProbe::Failed(format!("connect: {error}")),
    };
    if let Err(error) = stream.set_read_timeout(Some(HTTP_READ_TIMEOUT)) {
        return HttpProbe::Failed(format!("read timeout: {error}"));
    }
    if let Err(error) = stream.set_write_timeout(Some(HTTP_READ_TIMEOUT)) {
        return HttpProbe::Failed(format!("write timeout: {error}"));
    }
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    if let Err(error) = stream.write_all(request.as_bytes()) {
        return match error.kind() {
            ErrorKind::TimedOut | ErrorKind::WouldBlock => HttpProbe::Hung { received: 0 },
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
                HttpProbe::Closed { received: 0 }
            }
            _ => HttpProbe::Failed(format!("write: {error}")),
        };
    }
    let asked = Instant::now();
    let mut response = Vec::new();
    let mut chunk = [0_u8; 4096];
    while !http_response_complete(&response) {
        if asked.elapsed() >= HTTP_REQUEST_BUDGET {
            return HttpProbe::Hung {
                received: response.len(),
            };
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => response.extend_from_slice(chunk.get(..read).unwrap_or_default()),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                return HttpProbe::Hung {
                    received: response.len(),
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted) => break,
            Err(error) => return HttpProbe::Failed(format!("read: {error}")),
        }
    }
    parse_http_response(&response)
}

/// Whether `response` holds a whole HTTP response: its head and, when the
/// head names one, `Content-Length` bytes of body (otherwise only the peer's
/// close ends it, as `Connection: close` asks).
fn http_response_complete(response: &[u8]) -> bool {
    let Some(end) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(response.get(..end).unwrap_or_default());
    let length = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    });
    length.is_some_and(|length| response.len().saturating_sub(end + 4) >= length)
}

/// The [`HttpProbe`] of the bytes one request read.
fn parse_http_response(response: &[u8]) -> HttpProbe {
    if response.is_empty() {
        return HttpProbe::Closed { received: 0 };
    }
    let text = String::from_utf8_lossy(response);
    let code = text.split_whitespace().nth(1).and_then(|code| code.parse::<u16>().ok());
    let body = text
        .split_once("\r\n\r\n")
        .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok());
    match (code, body) {
        (Some(code), Some(body)) => HttpProbe::Answered { code, body },
        (code, _) => HttpProbe::Garbled {
            code,
            received: response.len(),
            head: text.chars().take(160).collect(),
        },
    }
}

/// The status and JSON body of `GET path`, or `None` for every other
/// [`HttpProbe`] outcome.
pub fn http_get_blocking(port: u16, path: &str) -> Option<(u16, serde_json::Value)> {
    match http_probe_blocking(port, path) {
        HttpProbe::Answered { code, body } => Some((code, body)),
        _ => None,
    }
}

pub async fn http(port: u16, path: &'static str) -> Option<(u16, serde_json::Value)> {
    tokio::task::spawn_blocking(move || http_get_blocking(port, path))
        .await
        .unwrap()
}

/// [`http_probe_blocking`], off the async worker.
pub async fn http_probe(port: u16, path: &'static str) -> HttpProbe {
    tokio::task::spawn_blocking(move || http_probe_blocking(port, path))
        .await
        .unwrap()
}

/// How often a wait saw each [`HttpProbe`] outcome, and the last answer.
#[derive(Default)]
struct ProbeTally {
    counts: BTreeMap<&'static str, usize>,
    last_answer: Option<(u16, serde_json::Value)>,
}

impl ProbeTally {
    fn note(&mut self, probe: &HttpProbe) {
        *self.counts.entry(probe.kind()).or_default() += 1;
        if let HttpProbe::Answered { code, body } = probe {
            self.last_answer = Some((*code, body.clone()));
        }
    }

    fn summary(&self) -> String {
        let counts = self
            .counts
            .iter()
            .map(|(kind, count)| format!("{kind} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        match &self.last_answer {
            Some((code, body)) => format!("{counts}; last answer: {code} {body}"),
            None => format!("{counts}; no HTTP answer at all"),
        }
    }
}

/// Poll `path` until it answers `want`; panics with the last answer otherwise.
pub async fn wait_http(
    port: u16,
    path: &'static str,
    want: u16,
    budget: Duration,
    daemon: &Daemon,
) -> serde_json::Value {
    wait_http_watched(port, path, want, budget, &daemon.watched()).await
}

/// [`wait_http`] for any daemon a [`Watched`] describes (spec R6 §2(b)): a
/// refused connection (nothing bound yet), a hung request (bounded by its own
/// timeouts) and an answer with another status all keep polling; at the
/// deadline the panic says how often each outcome was seen, the last probe,
/// and the [`readiness_report`].
pub async fn wait_http_watched(
    port: u16,
    path: &'static str,
    want: u16,
    budget: Duration,
    daemon: &Watched<'_>,
) -> serde_json::Value {
    let deadline = Instant::now() + budget;
    let mut tally = ProbeTally::default();
    loop {
        let probe = http_probe(port, path).await;
        tally.note(&probe);
        if let HttpProbe::Answered { code, body } = &probe {
            daemon.note_answer(port, path);
            if *code == want {
                return body.clone();
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "{path} never answered {want} on port {port} within {budget:?}; last {probe:?}; probes: {}{}",
                tally.summary(),
                readiness_report(daemon, port)
            );
        }
        // A hung request or a connect timeout already waited its own timeout.
        if !matches!(probe, HttpProbe::Hung { .. } | HttpProbe::ConnectTimeout) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
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
            "the primary never listed a standby durable at {at_least}; listed {listed:?}{}",
            daemon.diagnose(http_port)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ---------------------------------------------------------------------------
// Readiness diagnostics (spec R6 §1)
// ---------------------------------------------------------------------------

/// How long a stack dump may take: attaching to a process stuck in the kernel
/// can hang.
const GDB_BUDGET: Duration = Duration::from_secs(30);
/// How much of a stack dump the report carries inline (head, tail); the whole
/// dump is saved beside the log copy.
const GDB_INLINE_HEAD: usize = 48 * 1024;
const GDB_INLINE_TAIL: usize = 16 * 1024;
/// How many of the log's last lines the report carries inline.
const REPORT_LOG_LINES: usize = 200;
/// Log lines that say a listener could not be bound (reported wherever they
/// are in the log: they come early, before the tail).
const BIND_FAILURES: [&str; 4] = ["Failed to bind", "Cannot bind", "Address already in use", "os error 98"];

/// What [`readiness_report`] knows of a daemon: [`Daemon::watched`], or one a
/// suite with its own daemon type builds from its child and log.
pub struct Watched<'a> {
    pub name: &'a str,
    /// The OS pid while the process is owned (so it cannot have been reused).
    pub pid: Option<u32>,
    pub log: &'a Path,
    pub spawned_at: Option<Instant>,
    /// Where the first HTTP answer is recorded, when the daemon tracks it.
    pub first_answer: Option<&'a Mutex<Option<Instant>>>,
}

impl Watched<'_> {
    /// A wait saw an HTTP answer: the first one is recorded, and said once on
    /// the test's stderr (shown with a failing test's output) with the time
    /// since the spawn (spec R6 §1).
    fn note_answer(&self, port: u16, path: &str) {
        let Some(slot) = self.first_answer else {
            return;
        };
        let mut first = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if first.is_some() {
            return;
        }
        let now = Instant::now();
        *first = Some(now);
        if let Some(spawned) = self.spawned_at {
            eprintln!(
                "[harness] {} (pid {}) first answered HTTP ({path} on port {port}) {:.2?} after its spawn",
                self.name,
                self.pid.map_or_else(|| "?".to_string(), |pid| pid.to_string()),
                now.saturating_duration_since(spawned)
            );
        }
    }

    fn first_answer_text(&self) -> String {
        let first = self
            .first_answer
            .and_then(|slot| *slot.lock().unwrap_or_else(PoisonError::into_inner));
        match (first, self.spawned_at) {
            (Some(first), Some(spawned)) => format!("{:.2?} after its spawn", first.saturating_duration_since(spawned)),
            (Some(_), None) => "yes".to_string(),
            (None, _) => "none yet (no HTTP answer of any status)".to_string(),
        }
    }
}

/// Everything a readiness or health failure says about `daemon` and `port`
/// (spec R6 §1), gathered inside the test process — nothing in the product
/// binary:
///
/// * the daemon: its pid and age, when a wait first saw it answer HTTP,
///   `/proc/<pid>/status` (`State`, `Threads`, `VmRSS`, context switches) and
///   its threads grouped by name, state and kernel wait channel;
/// * the port: its sockets in `/proc/net/tcp{,6}` — the listening one's
///   owner, this daemon (its `/proc/<pid>/fd` links) or which other process —
///   and every port this daemon listens on;
/// * when `gdb` is on `PATH`, `gdb -p <pid> -batch -ex 'thread apply all bt'`,
///   bounded by [`GDB_BUDGET`];
/// * its log: every bind failure in it and its last [`REPORT_LOG_LINES`]
///   lines. The whole log and the stack dump are copied under
///   `CARGO_TARGET_TMPDIR/readiness-diagnostics/`, which outlives the test's
///   temporary directory.
pub fn readiness_report(daemon: &Watched<'_>, port: u16) -> String {
    let mut report = String::new();
    let pid_text = daemon
        .pid
        .map_or_else(|| "none: no longer owned".to_string(), |pid| pid.to_string());
    let _ = writeln!(report);
    let _ = writeln!(
        report,
        "==== readiness diagnostics: {} (pid {pid_text}), port {port} ====",
        daemon.name
    );
    let age = daemon.spawned_at.map_or_else(
        || "at an unknown time".to_string(),
        |at| format!("{:.2?} ago", at.elapsed()),
    );
    let _ = writeln!(
        report,
        "spawned {age}; first HTTP answer a wait saw: {}",
        daemon.first_answer_text()
    );
    #[cfg(target_os = "linux")]
    {
        if let Some(pid) = daemon.pid {
            let _ = writeln!(report, "process: {}", process_status(pid));
            let _ = writeln!(report, "threads: {}", thread_summary(pid));
        }
        let _ = writeln!(report, "port {port}: {}", port_report(port, daemon.pid));
        if let Some(pid) = daemon.pid {
            let _ = writeln!(report, "the daemon listens on: {}", listening_ports(pid));
        }
    }
    let keep = diagnostics_dir();
    let stamp = format!("{}-{}-{}", daemon.name, daemon.pid.unwrap_or(0), unix_millis());
    if let Some(pid) = daemon.pid {
        let _ = writeln!(
            report,
            "stack dump: {}",
            gdb_dump(pid, &keep.join(format!("{stamp}.gdb.txt")))
        );
    }
    let log = std::fs::read(daemon.log)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let copy = keep.join(format!("{stamp}.log"));
    match std::fs::write(&copy, &log) {
        Ok(()) => {
            let _ = writeln!(
                report,
                "the whole log ({} bytes) is copied to {}",
                log.len(),
                copy.display()
            );
        }
        Err(error) => {
            let _ = writeln!(report, "the log could not be copied to {}: {error}", copy.display());
        }
    }
    let binds: Vec<&str> = log
        .lines()
        .filter(|line| BIND_FAILURES.iter().any(|needle| line.contains(needle)))
        .take(20)
        .collect();
    if binds.is_empty() {
        let _ = writeln!(report, "bind failures in the log: none");
    } else {
        let _ = writeln!(report, "bind failures in the log:");
        for line in binds {
            let _ = writeln!(report, "  ! {line}");
        }
    }
    let lines: Vec<&str> = log.lines().collect();
    let tail = lines
        .get(lines.len().saturating_sub(REPORT_LOG_LINES)..)
        .unwrap_or_default();
    let _ = writeln!(
        report,
        "log {} — its last {} of {} lines:",
        daemon.log.display(),
        tail.len(),
        lines.len()
    );
    for line in tail {
        let _ = writeln!(report, "  | {line}");
    }
    let _ = write!(report, "==== end of the readiness diagnostics of {} ====", daemon.name);
    report
}

/// Where a report keeps its copies: under `CARGO_TARGET_TMPDIR` (the target
/// directory's scratch space for integration tests), else the system's.
fn diagnostics_dir() -> PathBuf {
    let base = option_env!("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let dir = base.join("readiness-diagnostics");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis())
}

/// At most `head` bytes from the start and `tail` from the end of `text`.
fn bounded(text: &str, head: usize, tail: usize) -> String {
    if text.len() <= head + tail {
        return text.to_string();
    }
    let mut start = head;
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = text.len() - tail;
    while !text.is_char_boundary(end) {
        end += 1;
    }
    format!(
        "{}\n[… {} bytes omitted …]\n{}",
        text.get(..start).unwrap_or_default(),
        end - start,
        text.get(end..).unwrap_or_default()
    )
}

/// `gdb -p <pid> -batch -ex 'thread apply all bt'` into `out`, bounded by
/// [`GDB_BUDGET`]; skipped when `gdb` is not on `PATH`.
fn gdb_dump(pid: u32, out: &Path) -> String {
    let Some(gdb) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("gdb"))
            .find(|candidate| candidate.is_file())
    }) else {
        return "skipped: gdb is not on PATH".to_string();
    };
    let file = match std::fs::File::create(out) {
        Ok(file) => file,
        Err(error) => return format!("skipped: {} cannot be created ({error})", out.display()),
    };
    let errors = match file.try_clone() {
        Ok(errors) => errors,
        Err(error) => return format!("skipped: {error}"),
    };
    let spawned = Command::new(&gdb)
        .arg("-p")
        .arg(pid.to_string())
        .args(["-batch", "-nx", "-ex", "thread apply all bt"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(errors))
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return format!("gdb did not start: {error}"),
    };
    let deadline = Instant::now() + GDB_BUDGET;
    let finished = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let dump = std::fs::read(out)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    format!(
        "{} ({} bytes, saved to {})\n{}",
        if finished {
            "gdb thread apply all bt"
        } else {
            "gdb ran out of its budget and was killed: a PARTIAL dump"
        },
        dump.len(),
        out.display(),
        bounded(&dump, GDB_INLINE_HEAD, GDB_INLINE_TAIL)
    )
}

/// `/proc/<pid>/status`: the lines that say whether it runs, sleeps or is a
/// zombie (it exited and was not reaped yet), how many threads it has, and
/// whether it is being scheduled at all.
#[cfg(target_os = "linux")]
fn process_status(pid: u32) -> String {
    const FIELDS: [&str; 5] = [
        "State:",
        "Threads:",
        "VmRSS:",
        "voluntary_ctxt_switches:",
        "nonvoluntary_ctxt_switches:",
    ];
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(status) => status
            .lines()
            .filter(|line| FIELDS.iter().any(|field| line.starts_with(field)))
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("; "),
        Err(error) => format!("/proc/{pid}/status cannot be read ({error}): the process is gone"),
    }
}

/// The daemon's threads, grouped by name, scheduler state and kernel wait
/// channel (`/proc/<pid>/task/*/{comm,stat,wchan}`): a worker stuck in
/// `fsync` or on a futex shows here even without gdb.
#[cfg(target_os = "linux")]
fn thread_summary(pid: u32) -> String {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return "no task list (the process is gone)".to_string();
    };
    let mut groups: BTreeMap<String, usize> = BTreeMap::new();
    for task in tasks.flatten() {
        let dir = task.path();
        let read = |name: &str| {
            std::fs::read_to_string(dir.join(name)).map_or_else(|_| "?".to_string(), |text| text.trim().to_string())
        };
        let state = std::fs::read_to_string(dir.join("stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_string))
            })
            .unwrap_or_else(|| "?".to_string());
        *groups
            .entry(format!("{} [{state}] wchan={}", read("comm"), read("wchan")))
            .or_default() += 1;
    }
    groups
        .into_iter()
        .map(|(group, count)| format!("{count} x {group}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// One socket of `/proc/net/tcp{,6}`.
#[cfg(target_os = "linux")]
struct ProcSocket {
    local: String,
    port: u16,
    state: &'static str,
    uid: String,
    inode: u64,
}

/// Every TCP socket of this network namespace that is listening or
/// connected (the ones `/proc/net/tcp{,6}` list).
#[cfg(target_os = "linux")]
fn proc_sockets() -> Vec<ProcSocket> {
    let mut sockets = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (Some(local), Some(state), Some(uid), Some(inode)) =
                (fields.get(1), fields.get(3), fields.get(7), fields.get(9))
            else {
                continue;
            };
            let Some((address, port)) = local.rsplit_once(':') else {
                continue;
            };
            let Ok(port) = u16::from_str_radix(port, 16) else {
                continue;
            };
            sockets.push(ProcSocket {
                local: format!("{}:{port}", proc_address(address)),
                port,
                state: tcp_state(state),
                uid: (*uid).to_string(),
                inode: inode.parse().unwrap_or(0),
            });
        }
    }
    sockets
}

/// An address as `/proc/net/tcp{,6}` prints it: 32-bit words in host order.
#[cfg(target_os = "linux")]
fn proc_address(hex: &str) -> String {
    let words: Option<Vec<[u8; 4]>> = (0..hex.len() / 8)
        .map(|word| {
            hex.get(word * 8..word * 8 + 8)
                .and_then(|text| u32::from_str_radix(text, 16).ok())
                .map(u32::to_ne_bytes)
        })
        .collect();
    match words.as_deref() {
        Some([v4]) => std::net::Ipv4Addr::from(*v4).to_string(),
        Some(words) if words.len() == 4 => {
            let mut bytes = [0_u8; 16];
            for (chunk, word) in bytes.chunks_exact_mut(4).zip(words) {
                chunk.copy_from_slice(word);
            }
            format!("[{}]", std::net::Ipv6Addr::from(bytes))
        }
        _ => hex.to_string(),
    }
}

#[cfg(target_os = "linux")]
fn tcp_state(code: &str) -> &'static str {
    match code {
        "01" => "ESTABLISHED",
        "02" => "SYN_SENT",
        "03" => "SYN_RECV",
        "04" => "FIN_WAIT1",
        "05" => "FIN_WAIT2",
        "06" => "TIME_WAIT",
        "07" => "CLOSE",
        "08" => "CLOSE_WAIT",
        "09" => "LAST_ACK",
        "0A" => "LISTEN",
        "0B" => "CLOSING",
        _ => "UNKNOWN",
    }
}

/// The socket inodes `pid` holds (`/proc/<pid>/fd/* -> socket:[inode]`);
/// `None` when its descriptors cannot be read (gone, or another user's).
#[cfg(target_os = "linux")]
fn socket_inodes(pid: u32) -> Option<BTreeSet<u64>> {
    let entries = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    Some(
        entries
            .flatten()
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter_map(|target| {
                target
                    .to_str()?
                    .strip_prefix("socket:[")?
                    .strip_suffix(']')?
                    .parse::<u64>()
                    .ok()
            })
            .collect(),
    )
}

/// The process holding socket `inode` (pid and command line), searching
/// `/proc` for a few seconds at most.
#[cfg(target_os = "linux")]
fn socket_owner(inode: u64) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        if Instant::now() >= deadline {
            return Some("unknown: the /proc search ran out of time".to_string());
        }
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        if socket_inodes(pid).is_some_and(|inodes| inodes.contains(&inode)) {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
                .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
                .unwrap_or_default();
            return Some(format!(
                "pid {pid} ({})",
                cmdline.trim().chars().take(300).collect::<String>()
            ));
        }
    }
    None
}

/// Who holds `port` (spec R6 §1, an `ss`-style check): its listening
/// socket and whether `pid` owns it — or which process does — then how many
/// other sockets (accepted or closing connections) use it.
#[cfg(target_os = "linux")]
fn port_report(port: u16, pid: Option<u32>) -> String {
    let sockets: Vec<ProcSocket> = proc_sockets()
        .into_iter()
        .filter(|socket| socket.port == port)
        .collect();
    let ours = pid.and_then(socket_inodes).unwrap_or_default();
    let mut parts = Vec::new();
    let mut others: BTreeMap<&'static str, usize> = BTreeMap::new();
    for socket in &sockets {
        if socket.state != "LISTEN" {
            *others.entry(socket.state).or_default() += 1;
            continue;
        }
        let owner = if ours.contains(&socket.inode) {
            "THIS daemon's listener".to_string()
        } else {
            format!(
                "NOT this daemon's: held by {}",
                socket_owner(socket.inode).unwrap_or_else(|| "a process this user cannot inspect".to_string())
            )
        };
        parts.push(format!(
            "LISTEN on {} (uid {}, inode {}): {owner}",
            socket.local, socket.uid, socket.inode
        ));
    }
    if parts.is_empty() {
        parts.push("NOTHING listens on it (never bound, a failed bind, or already closed)".to_string());
    }
    if !others.is_empty() {
        parts.push(format!(
            "other sockets on it: {}",
            others
                .iter()
                .map(|(state, count)| format!("{count} {state}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    parts.join("; ")
}

/// Every address `pid` listens on.
#[cfg(target_os = "linux")]
fn listening_ports(pid: u32) -> String {
    let Some(ours) = socket_inodes(pid) else {
        return "unknown: its /proc/<pid>/fd cannot be read".to_string();
    };
    let listening: Vec<String> = proc_sockets()
        .into_iter()
        .filter(|socket| socket.state == "LISTEN" && ours.contains(&socket.inode))
        .map(|socket| socket.local)
        .collect();
    if listening.is_empty() {
        "nothing".to_string()
    } else {
        listening.join(", ")
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

/// [`connect`], keeping the connection's own future: a FATAL the server
/// PUSHES — an ErrorResponse no request waits for (spec P5.2 §1) — is what
/// that future ends with (`Err` with the frame's SQLSTATE), not an error of
/// any statement.
pub async fn connect_watched(
    port: u16,
) -> (
    tokio_postgres::Client,
    tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let url = format!("host=127.0.0.1 port={port} user=postgres dbname=postgres connect_timeout=2");
        match tokio_postgres::connect(&url, NoTls).await {
            Ok((client, connection)) => return (client, tokio::spawn(connection)),
            Err(e) if Instant::now() > deadline => panic!("no connection on {port}: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(300)).await,
        }
    }
}

/// The error a [`connect_watched`] connection ended with, within `budget`.
pub async fn connection_end(
    connection: tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
    budget: Duration,
) -> Result<(), tokio_postgres::Error> {
    tokio::time::timeout(budget, connection)
        .await
        .unwrap_or_else(|_| panic!("the connection did not end within {budget:?}"))
        .unwrap()
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
