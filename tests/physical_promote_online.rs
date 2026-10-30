//! Spec P5.2 §5 and §8.K: `SELECT pg_promote(wait, wait_seconds)` — the
//! ONLINE promotion of a running standby — on real managed daemons
//! (`tests/support/physical_pair.rs`).
//!
//! * (1) a graceful online promotion WHILE THE PRIMARY TAKES WRITES:
//!   `SELECT pg_promote(true, 60)` on S answers one row `t`; an idle session
//!   on S and an idle session on the old primary P are both pushed FATAL
//!   57P01 without issuing a statement (a client that reads unsolicited
//!   frames); a fresh connection to S starts with `in_hot_standby = off`,
//!   answers `pg_is_in_recovery() = f`, `timeline_id = 2`,
//!   `prev_timeline_id = 1`, the history is P's, writes succeed; P is fenced
//!   (state `fenced`, `/ready` 503, writes 25006); S holds exactly what P
//!   holds — every write P acknowledged during the promotion included — and
//!   its `repl:epoch` says `fenced = true` at P's recorded final sequence;
//!   `/health` on S says epoch 2, role primary; a standby S2 of P pointed at
//!   S is `needs_recovery` — and `pg_promote` on it is refused 55000 naming
//!   the recover command — then `recover --from-primary` fast-forwards it
//!   (route 2) and it streams from S's replication port;
//! * (2) the exact forms HeliosDB Proxy sends, with writes on P meanwhile:
//!   `SELECT pg_promote(true, 5)` answers the text `t` in a column named
//!   `pg_promote`, a fresh connection right after answers `SELECT
//!   pg_is_in_recovery()::text` = `false`, S holds what P holds, and the epoch
//!   record says fenced at P's final;
//! * (3) `wait = false` answers `t` at once and the promotion completes;
//!   (`internal-tests`) `wait = true` with a budget too short answers `f`, the
//!   promotion still completes, and a second `pg_promote` meanwhile is
//!   refused 55000 "in progress" — the promotion held on a FILE, not a timer;
//! * (4) refusals: on a primary 55000 "already the primary"; on a
//!   `needs_recovery` standby (scenario 1); during a promotion (scenario 3);
//! * (5) a primary dead BEFORE the call: with `promote_without_fence =
//!   "refuse"` the promotion is refused 55000 naming the offline command and
//!   saying the old primary is NOT fenced, and S stays a standby whose
//!   receiver keeps reconnecting; with `"allow"` it answers `t`, the epoch
//!   record says `fenced = false`, and the WARNING is logged;
//! * (6) (`internal-tests`) a failure after the record (an injected failure
//!   before the conversion, a SIGKILL inside it, a failure of the reopen):
//!   the process ends (exit 2 for a failure), `start` refuses the directory
//!   until `heliosdb-nano promote` finished it, and the node then serves
//!   epoch 2 as the primary;
//! * (7) the fence pushes its termination: the idle sessions of a primary
//!   fenced by the OFFLINE promotion receive FATAL 57P01 within a second of
//!   the fence, without a statement;
//! * (8) (`internal-tests`, spec §8.A) a statement held INSIDE the engine on
//!   S: with a budget it outlasts, the promotion aborts naming the session
//!   and the old primary stays unfenced (it takes writes), S stays a standby,
//!   and the statement still answers; with a budget it does not outlast, the
//!   promotion waits — S `promoting`, P unfenced and taking writes — and
//!   completes, fenced, once the statement answered;
//! * (9) (`internal-tests`, spec §8.C, §8.E) a primary that dies AFTER it
//!   answered `Fenced` and before S holds its final sequence: with `allow`,
//!   the promotion still aborts — naming the fence, both sequences and
//!   `promote --force` — and nothing is converted; the old primary restarted
//!   (fenced) and `pg_promote` asked again: the cached `Fenced` is
//!   re-reported, the catch-up completes and S is promoted, fenced;
//! * (10) (`internal-tests`, spec §8.J) a promotion aborted after its
//!   replication stopped: the receiver starts again and the cascading port
//!   serves its downstream standby again (`/health` `cascade`), and both
//!   follow P once it is back;
//! * (11) the extended protocol: `pg_promote` is described as one `boolean`
//!   (OID 16), answered with a DataRow and CommandComplete, and the session
//!   is ended with FATAL 57P01 after its Sync;
//! * (12) a CASCADING standby promoted online: its replication port, its
//!   cascading source's until then, serves epoch 2 as the primary's;
//!   `/health` no longer says `cascade`; its cascaded standby is refused (it
//!   follows epoch 1) and recovers from it.
//!
//! The always-answered list is a handler unit test
//! (`pg_promote_is_always_answered_by_the_guard`); `SWITCHOVER` is
//! `tests/promotion_refusal_tests.rs`.
//!
//! Red on the base of this change (P5.1, `91c5908e`): `pg_promote` was an
//! undefined function (42883), sessions were ended only at their next
//! statement, and a standby could not be promoted while it ran.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use heliosdb_nano::replication::epoch::read_directory_epoch;
use physical_pair::*;
use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_postgres::SimpleQueryMessage;

/// Every node archives, with segments sealed every second.
const ARCHIVE: &str = "[replication.archive]\nenabled = true\nsegment_max_secs = 1\n";
/// Scenario 5: a standby that refuses to promote without the fence (and
/// gives up on an unreachable primary after 3 s).
const REFUSE_UNFENCED: &str = "[replication]\npromote_without_fence = \"refuse\"\npromote_fence_timeout = \"3s\"\n\
                               [replication.archive]\nenabled = true\nsegment_max_secs = 1\n";
/// Scenarios 5 and 9: the default (`allow`), with the same 3 s fence timeout.
const ALLOW_UNFENCED: &str =
    "[replication]\npromote_fence_timeout = \"3s\"\n[replication.archive]\nenabled = true\nsegment_max_secs = 1\n";
/// `internal-tests`: `SELECT heliosdb_test_hold()` stays inside the engine
/// while the file this names exists (`sql::evaluator`).
#[cfg(feature = "internal-tests")]
const HOLD_STATEMENT_ENV: &str = "HELIOSDB_NANO_TEST_HOLD_STATEMENT_FILE";

fn cli(args: &[String], env: &[(&str, String)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
    command.args(args).env("RUST_LOG", "warn");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

async fn cli_async(args: Vec<String>) -> Output {
    tokio::task::spawn_blocking(move || cli(&args, &[])).await.unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn text(path: &Path) -> String {
    path.display().to_string()
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// A standby that can be promoted online: its own `--replication-port`, the
/// port it serves once it is the primary.
fn promotable_standby_args(pg: u16, primary_replication: u16, http: u16, replication: u16) -> Vec<String> {
    let mut args = standby_args(pg, primary_replication, http);
    args.extend(["--replication-port".to_string(), replication.to_string()]);
    args
}

/// `promote` (offline) of `data`, the primary at `primary_port`.
fn promote_args(data: &Path, primary_port: u16, token: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        "promote".to_string(),
        "--data-dir".to_string(),
        text(data),
        "--primary-host".to_string(),
        format!("127.0.0.1:{primary_port}"),
        "--replication-token-file".to_string(),
        text(token),
    ];
    args.extend(strings(extra));
    args
}

/// `recover --from-primary` of `data` from the primary at `primary_port`.
fn recover_args(data: &Path, primary_port: u16, token: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        "recover".to_string(),
        "--data-dir".to_string(),
        text(data),
        "--from-primary".to_string(),
        "--primary-host".to_string(),
        format!("127.0.0.1:{primary_port}"),
        "--replication-token-file".to_string(),
        text(token),
        "--leaf-bytes".to_string(),
        "64KiB".to_string(),
        "--then".to_string(),
        "stop".to_string(),
    ];
    args.extend(strings(extra));
    args
}

/// The dataset history a primary's directory names.
fn primary_history(dir: &Path) -> String {
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("NANO-PRIMARY-HISTORY.json")).unwrap()).unwrap();
    record["history_id"].as_str().unwrap().to_string()
}

/// The final sequence a fenced primary's directory records
/// (`NANO-FENCED.json`), once it is there.
async fn fenced_final(dir: &Path, daemon: &Daemon) -> u64 {
    let file = dir.join("NANO-FENCED.json");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !file.exists() {
        assert!(
            Instant::now() < deadline,
            "{} was never fenced; log tail: {}",
            dir.display(),
            daemon.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    record["final_sequence"].as_u64().unwrap()
}

/// `(epoch, fenced, old_primary_final_sequence)` of a store's `repl:epoch`
/// record (a read-only open: the node may be running); epoch 1 and no fence
/// without one.
fn epoch_record(dir: &Path) -> (u64, bool, Option<u64>) {
    read_directory_epoch(dir).unwrap().map_or((1, false, None), |record| {
        (record.epoch, record.fenced, record.old_primary_final_sequence)
    })
}

async fn health(port: u16) -> serde_json::Value {
    http(port, "/health").await.map(|(_, body)| body).unwrap_or_default()
}

/// Poll `/health` until its replication block satisfies `want`; the whole
/// body.
async fn wait_health(
    port: u16,
    budget: Duration,
    daemon: &Daemon,
    want: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + budget;
    loop {
        let body = health(port).await;
        if want(&body["replication"]) {
            return body;
        }
        assert!(
            Instant::now() < deadline,
            "/health never showed what was wanted; last {body}; log tail: {}",
            daemon.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Until `needle` appears at least `times` times in the daemon's log.
#[cfg_attr(not(feature = "internal-tests"), allow(dead_code))]
async fn wait_log(daemon: &Daemon, needle: &str, times: usize, budget: Duration) {
    let deadline = Instant::now() + budget;
    while daemon.log().matches(needle).count() < times {
        assert!(
            Instant::now() < deadline,
            "the log never said {needle:?} {times} time(s); tail: {}",
            daemon.tail(16)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The one row of `SELECT pg_promote(…)`: its column's name and its text.
async fn promote_answer(client: &tokio_postgres::Client, sql: &str) -> Result<(String, String), tokio_postgres::Error> {
    let messages = client.simple_query(sql).await?;
    let rows: Vec<(String, String)> = messages
        .iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some((
                row.columns()[0].name().to_string(),
                row.get(0).unwrap_or_default().to_string(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 1, "{sql}: one row");
    Ok(rows[0].clone())
}

/// The texts of the first column of a simple query's rows.
fn row_texts(messages: &[SimpleQueryMessage]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
            _ => None,
        })
        .collect()
}

/// Poll fresh connections until `pg_is_in_recovery()` answers `false`.
async fn wait_primary(port: u16, budget: Duration, daemon: &Daemon) {
    let deadline = Instant::now() + budget;
    loop {
        if let Ok((client, connection)) = tokio_postgres::connect(
            &format!("host=127.0.0.1 port={port} user=postgres dbname=postgres connect_timeout=2"),
            tokio_postgres::NoTls,
        )
        .await
        {
            tokio::spawn(connection);
            if let Ok(messages) = client.simple_query("SELECT pg_is_in_recovery()::text").await {
                if row_texts(&messages).first().map(String::as_str) == Some("false") {
                    return;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "{port} never served as the primary; log tail: {}",
            daemon.tail(16)
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ---------------------------------------------------------------------------
// Writes on the primary while a standby is promoted
// ---------------------------------------------------------------------------

/// A client writing one row per statement on a primary, from `first` on,
/// until a write is refused (the primary was fenced) or it is told to stop.
struct Writer {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<(Vec<i32>, bool)>,
}

impl Writer {
    async fn start(port: u16, first: i32) -> Self {
        let client = connect(port).await;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let task = tokio::spawn(async move {
            let mut acknowledged = Vec::new();
            let mut id = first;
            while !stopping.load(Ordering::Acquire) {
                if client
                    .batch_execute(&format!("INSERT INTO t VALUES ({id}, 'during the promotion')"))
                    .await
                    .is_err()
                {
                    return (acknowledged, true);
                }
                acknowledged.push(id);
                id += 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            (acknowledged, false)
        });
        Self { stop, task }
    }

    /// The ids the primary acknowledged, and whether a write was refused.
    async fn finish(self) -> (Vec<i32>, bool) {
        self.stop.store(true, Ordering::Release);
        self.task.await.unwrap()
    }
}

/// Spec P5.2 §8.K: the writes the primary acknowledged during the
/// promotion are all on the promoted standby, which holds exactly what the
/// fenced primary holds.
async fn assert_holds_the_fenced_primary(
    p: &tokio_postgres::Client,
    s: &tokio_postgres::Client,
    acknowledged: &[i32],
) -> Vec<i32> {
    let held = ids(p).await.unwrap();
    let on_p: BTreeSet<i32> = held.iter().copied().collect();
    for id in acknowledged {
        assert!(on_p.contains(id), "P acknowledged {id} but does not hold it");
    }
    assert_eq!(
        ids(s).await,
        Ok(held.clone()),
        "S holds exactly what the fenced P holds"
    );
    held
}

// ---------------------------------------------------------------------------
// A client that reads unsolicited frames
// ---------------------------------------------------------------------------

/// A PostgreSQL wire client that authenticates (trust) and then only READS:
/// what it receives while it issues no statement was pushed by the server.
struct RawSession {
    stream: TcpStream,
    /// The startup's ParameterStatus frames.
    parameters: HashMap<String, String>,
}

/// What an ErrorResponse said (`S`, `V`, `C`, `M`), and whether the server
/// closed the connection after it.
#[derive(Debug)]
struct Pushed {
    fields: HashMap<char, String>,
    closed: bool,
    at: Instant,
}

impl Pushed {
    fn field(&self, name: char) -> &str {
        self.fields.get(&name).map_or("", String::as_str)
    }
}

impl RawSession {
    fn connect(port: u16) -> Self {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match Self::try_connect(port) {
                Ok(session) => return session,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    std::thread::sleep(Duration::from_millis(300));
                }
                Err(error) => panic!("no raw session on {port}: {error}"),
            }
        }
    }

    fn try_connect(port: u16) -> std::io::Result<Self> {
        let mut stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut body = 196_608_i32.to_be_bytes().to_vec();
        for (key, value) in [("user", "postgres"), ("database", "postgres")] {
            body.extend_from_slice(key.as_bytes());
            body.push(0);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut startup = i32::try_from(body.len() + 4).unwrap().to_be_bytes().to_vec();
        startup.extend(body);
        stream.write_all(&startup)?;
        let mut session = Self {
            stream,
            parameters: HashMap::new(),
        };
        loop {
            let (kind, payload) = session.read_message()?;
            match kind {
                b'R' if payload.get(..4) == Some(&[0, 0, 0, 0]) => {}
                b'S' => {
                    let mut parts = payload.split(|byte| *byte == 0);
                    let name = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
                    let value = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
                    session.parameters.insert(name, value);
                }
                b'K' | b'N' => {}
                b'Z' => return Ok(session),
                other => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("startup answered {:?}: {:?}", other as char, error_fields(&payload)),
                    ))
                }
            }
        }
    }

    fn read_message(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        let mut header = [0_u8; 5];
        self.stream.read_exact(&mut header)?;
        let length = usize::try_from(i32::from_be_bytes([header[1], header[2], header[3], header[4]])).unwrap();
        let mut payload = vec![0_u8; length.saturating_sub(4)];
        self.stream.read_exact(&mut payload)?;
        Ok((header[0], payload))
    }

    fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters.get(name).map(String::as_str)
    }

    /// The frame the server pushes this idle session within `timeout`: an
    /// ErrorResponse (anything else panics), and whether the connection was
    /// closed after it.
    fn pushed(mut self, timeout: Duration) -> Result<Pushed, String> {
        self.stream.set_read_timeout(Some(timeout)).unwrap();
        let (kind, payload) = self
            .read_message()
            .map_err(|error| format!("nothing was pushed within {timeout:?}: {error}"))?;
        let at = Instant::now();
        if kind != b'E' {
            return Err(format!("an unsolicited frame of type {:?}", kind as char));
        }
        self.stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut rest = [0_u8; 1];
        let closed = matches!(self.stream.read(&mut rest), Ok(0));
        Ok(Pushed {
            fields: error_fields(&payload),
            closed,
            at,
        })
    }
}

/// An ErrorResponse's fields: (type byte, NUL-terminated text)…, then 0.
fn error_fields(payload: &[u8]) -> HashMap<char, String> {
    let mut fields = HashMap::new();
    let mut index = 0;
    while index < payload.len() && payload[index] != 0 {
        let name = payload[index] as char;
        index += 1;
        let end = payload[index..]
            .iter()
            .position(|byte| *byte == 0)
            .map_or(payload.len(), |offset| index + offset);
        fields.insert(name, String::from_utf8_lossy(&payload[index..end]).into_owned());
        index = end + 1;
    }
    fields
}

async fn raw_session(port: u16) -> RawSession {
    tokio::task::spawn_blocking(move || RawSession::connect(port))
        .await
        .unwrap()
}

/// The FATAL 57P01 pushed to `session` (it issued no statement).
async fn pushed_fatal(session: RawSession, timeout: Duration) -> Pushed {
    let pushed = tokio::task::spawn_blocking(move || session.pushed(timeout))
        .await
        .unwrap()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(pushed.field('S'), "FATAL", "{pushed:?}");
    assert_eq!(pushed.field('V'), "FATAL", "{pushed:?}");
    assert_eq!(pushed.field('C'), "57P01", "{pushed:?}");
    assert!(pushed.closed, "the connection is closed after the FATAL: {pushed:?}");
    pushed
}

// ---------------------------------------------------------------------------
// A primary with one promotable standby
// ---------------------------------------------------------------------------

struct Promotable {
    pair: Pair,
    primary: Daemon,
    standby: Daemon,
    p_data: PathBuf,
    s_data: PathBuf,
    /// S's replication port once it is the primary.
    repl_s: u16,
    rows: Vec<i32>,
}

/// P with rows `1..=rows`, and S following it (ready, settled); S runs with
/// `config` and the environment `env`.
async fn promotable(rows: i32, config: &str, env: Vec<(&'static str, String)>) -> Promotable {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let p_data = dir.join("primary");
    let s_data = dir.join("standby");
    let repl_s = free_port();
    let primary = Daemon::spawn(&dir, "primary", &p_data, &pair.token, &pair.primary_args, Some(ARCHIVE));
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=rows, "before").await, 0);
    let s_args = promotable_standby_args(pair.pg_s, pair.repl_p, pair.http_s, repl_s);
    let standby = Daemon::spawn_with(
        &dir,
        "standby",
        &s_data,
        &pair.token,
        &s_args,
        Some(config),
        &SpawnOptions { listen: None, env },
    );
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    let rows: Vec<i32> = (1..=rows).collect();
    assert_eq!(
        settle(pair.pg_s, &rows, Duration::from_secs(60)).await,
        Ok(rows.clone())
    );
    Promotable {
        pair,
        primary,
        standby,
        p_data,
        s_data,
        repl_s,
        rows,
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Scenarios 1 and 4 (needs_recovery): the graceful online promotion, with
/// writes on the primary during it, and the nodes it leaves behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_standby_is_promoted_online_and_every_session_redials() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let token = pair.token.clone();
    let p_data = dir.join("primary");
    let s_data = dir.join("standby");
    let s2_data = dir.join("standby2");
    let (repl_s, pg_s2, http_s2) = (free_port(), free_port(), free_port());
    let primary = Daemon::spawn(&dir, "primary", &p_data, &token, &pair.primary_args, Some(ARCHIVE));
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=30, "before").await, 0);
    let s_args = promotable_standby_args(pair.pg_s, pair.repl_p, pair.http_s, repl_s);
    let standby = Daemon::spawn(&dir, "standby", &s_data, &token, &s_args, Some(ARCHIVE));
    let s2_at_p = standby_args(pg_s2, pair.repl_p, http_s2);
    let mut standby2 = Daemon::spawn(&dir, "standby2", &s2_data, &token, &s2_at_p, Some(ARCHIVE));
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    wait_http(http_s2, "/ready", 200, Duration::from_secs(120), &standby2).await;
    let first: Vec<i32> = (1..=30).collect();
    assert_eq!(
        settle(pair.pg_s, &first, Duration::from_secs(60)).await,
        Ok(first.clone())
    );
    assert_eq!(settle(pg_s2, &first, Duration::from_secs(60)).await, Ok(first.clone()));
    // S2 stops behind; P takes rows only S (and the fence's tail) bring.
    standby2.stop();
    assert_eq!(insert(&writer, 31..=40, "s-only").await, 0);
    let rows: Vec<i32> = (1..=40).collect();
    assert_eq!(
        settle(pair.pg_s, &rows, Duration::from_secs(60)).await,
        Ok(rows.clone())
    );
    let history = primary_history(&p_data);

    // Idle sessions that never issue a statement: one on S, one on P.
    let idle_s = raw_session(pair.pg_s).await;
    assert_eq!(idle_s.parameter("in_hot_standby"), Some("on"));
    let idle_p = raw_session(pair.pg_p).await;
    assert_eq!(idle_p.parameter("in_hot_standby"), Some("off"));

    // --- the online promotion, P taking writes until it is fenced -------
    let during = Writer::start(pair.pg_p, 1000).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let caller = connect(pair.pg_s).await;
    let started = Instant::now();
    let (column, answer) = promote_answer(&caller, "SELECT pg_promote(true, 60)")
        .await
        .unwrap_or_else(|error| panic!("pg_promote failed: {error}; log tail: {}", standby.tail(20)));
    let took = started.elapsed();
    println!("pg_promote(true, 60) answered after {took:?}");
    assert_eq!((column.as_str(), answer.as_str()), ("pg_promote", "t"));
    assert!(took < Duration::from_secs(60), "within the budget: {took:?}");
    let (acknowledged, refused) = during.finish().await;
    assert!(refused, "the fence refused P's next write");
    assert!(!acknowledged.is_empty(), "P took writes while S was promoted");
    // The caller is closed after its answer: it held `in_hot_standby = on`.
    let after = caller.simple_query("SELECT 1").await;
    assert!(after.is_err(), "the caller's session is ended after its answer");

    // The idle sessions were PUSHED FATAL 57P01, without a statement.
    let pushed_s = pushed_fatal(idle_s, Duration::from_secs(10)).await;
    assert!(
        pushed_s
            .field('M')
            .contains("this standby is being promoted to primary (epoch 2); reconnect"),
        "{pushed_s:?}"
    );
    let pushed_p = pushed_fatal(idle_p, Duration::from_secs(10)).await;
    assert!(
        pushed_p.field('M').contains("this primary was fenced by epoch 2"),
        "{pushed_p:?}"
    );

    // A fresh connection to S: the primary of epoch 2.
    let fresh = raw_session(pair.pg_s).await;
    assert_eq!(fresh.parameter("in_hot_standby"), Some("off"), "{:?}", fresh.parameters);
    assert_eq!(fresh.parameter("default_transaction_read_only"), Some("off"));
    drop(fresh);
    let s = connect(pair.pg_s).await;
    assert_eq!(
        value(&s, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("false")
    );
    assert_eq!(
        value(&s, "SELECT timeline_id::bigint FROM pg_control_checkpoint()")
            .await
            .as_deref(),
        Some("2")
    );
    assert_eq!(
        value(&s, "SELECT prev_timeline_id::bigint FROM pg_control_checkpoint()")
            .await
            .as_deref(),
        Some("1")
    );
    assert_eq!(primary_history(&s_data), history, "a promotion never mints a history");
    assert!(!s_data.join("NANO-PHYSICAL-REPLICA.json").exists());
    assert!(!s_data.join("NANO-PROMOTION.json").exists());

    // P is fenced, and S holds exactly what P holds — the writes P took
    // during the promotion included — at P's final sequence.
    let ready = wait_http(pair.http_p, "/ready", 503, Duration::from_secs(30), &primary).await;
    assert_eq!(ready["state"], "fenced", "{ready}");
    let p = connect(pair.pg_p).await;
    let refused = p
        .batch_execute("INSERT INTO t VALUES (999, 'after the fence')")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&refused), "25006", "{refused}");
    let held = assert_holds_the_fenced_primary(&p, &s, &acknowledged).await;
    assert!(held.starts_with(&rows), "{held:?}");
    let final_sequence = fenced_final(&p_data, &primary).await;
    assert_eq!(
        epoch_record(&s_data),
        (2, true, Some(final_sequence)),
        "S's epoch record: promoted fenced, at P's final sequence"
    );
    drop(p);

    // S writes.
    assert_eq!(insert(&s, 41..=45, "epoch-2").await, 0);
    let mut all: Vec<i32> = held.clone();
    all.extend(41..=45);
    all.sort_unstable();
    assert_eq!(ids(&s).await, Ok(all.clone()));
    let body = wait_health(pair.http_s, Duration::from_secs(30), &standby, |block| {
        block["epoch"] == 2 && block["role"] == "primary"
    })
    .await;
    assert_eq!(body["replication"]["state"], "streaming", "{body}");
    assert!(
        standby.log_contains_within("is the PRIMARY of history", Duration::from_secs(10)),
        "{}",
        standby.tail(20)
    );

    // --- (4) S2, a standby of P pointed at S, needs recovery ------------
    let s2_at_s = standby_args(pg_s2, repl_s, http_s2);
    let mut standby2 = Daemon::spawn(&dir, "standby2-at-s", &s2_data, &token, &s2_at_s, Some(ARCHIVE));
    wait_health(http_s2, Duration::from_secs(60), &standby2, |block| {
        block["state"] == "needs_recovery"
    })
    .await;
    let s2 = connect(pg_s2).await;
    let refused = s2.simple_query("SELECT pg_promote(true, 5)").await.unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    assert!(
        message(&refused).contains("not ready to be promoted")
            && message(&refused).contains("needs_recovery")
            && message(&refused).contains("heliosdb-nano recover --from-primary"),
        "{}",
        message(&refused)
    );
    assert_eq!(
        value(&s2, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("true"),
        "the refusal changed nothing"
    );
    drop(s2);
    standby2.stop();
    // `recover --from-primary` fast-forwards it from S's replication port.
    let output = cli_async(recover_args(
        &s2_data,
        repl_s,
        &token,
        &["--full-copy-threshold", "100"],
    ))
    .await;
    let out = stdout(&output);
    assert!(output.status.success(), "{out}{}", stderr(&output));
    assert!(out.contains("fast-forward"), "route 2: {out}");
    let standby2 = Daemon::spawn(&dir, "standby2-follows-s", &s2_data, &token, &s2_at_s, Some(ARCHIVE));
    wait_http(http_s2, "/ready", 200, Duration::from_secs(120), &standby2).await;
    assert_eq!(settle(pg_s2, &all, Duration::from_secs(60)).await, Ok(all.clone()));
    assert_eq!(health(http_s2).await["replication"]["epoch"], 2);
    drop(standby2);
    drop(standby);
    drop(primary);
}

/// Scenarios 2 and 4 (on a primary): the exact forms Proxy sends, with
/// writes on the primary during the promotion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_exact_forms_proxy_sends_are_answered() {
    let c = promotable(10, ARCHIVE, Vec::new()).await;
    // On a primary: refused, naming its epoch.
    let p = connect(c.pair.pg_p).await;
    let refused = p.simple_query("SELECT pg_promote(true, 5)").await.unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    assert!(
        message(&refused).contains("this node is already the primary (epoch 1)"),
        "{}",
        message(&refused)
    );
    // Out of range: 22023, nothing changed.
    let s = connect(c.pair.pg_s).await;
    let refused = s.simple_query("SELECT pg_promote(true, 0)").await.unwrap_err();
    assert_eq!(sqlstate(&refused), "22023", "{refused}");
    assert_eq!(
        value(&s, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("true")
    );
    // Proxy's form while P takes writes: the text `t` in a column
    // `pg_promote`, then at once a fresh connection that answers `false`.
    let during = Writer::start(c.pair.pg_p, 1000).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (column, answer) = promote_answer(&s, "SELECT pg_promote(true, 5)")
        .await
        .unwrap_or_else(|error| panic!("pg_promote failed: {error}; log tail: {}", c.standby.tail(20)));
    assert_eq!((column.as_str(), answer.as_str()), ("pg_promote", "t"));
    let fresh = connect(c.pair.pg_s).await;
    assert_eq!(
        value(&fresh, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("false"),
        "a fresh connection right after the `t` sees the primary"
    );
    let (acknowledged, refused) = during.finish().await;
    assert!(refused && !acknowledged.is_empty(), "{acknowledged:?}");
    let p = connect(c.pair.pg_p).await;
    let held = assert_holds_the_fenced_primary(&p, &fresh, &acknowledged).await;
    assert!(held.starts_with(&c.rows), "{held:?}");
    let final_sequence = fenced_final(&c.p_data, &c.primary).await;
    assert_eq!(epoch_record(&c.s_data), (2, true, Some(final_sequence)));
}

/// Scenario 3 (`wait = false`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_false_answers_at_once_and_the_promotion_completes() {
    let c = promotable(10, ARCHIVE, Vec::new()).await;
    let s = connect(c.pair.pg_s).await;
    let started = Instant::now();
    let (_, answer) = promote_answer(&s, "SELECT pg_promote(false, 60)").await.unwrap();
    assert_eq!(answer, "t");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "answered at once: {:?}",
        started.elapsed()
    );
    wait_primary(c.pair.pg_s, Duration::from_secs(60), &c.standby).await;
    let fresh = connect(c.pair.pg_s).await;
    assert_eq!(
        value(&fresh, "SELECT timeline_id::bigint FROM pg_control_checkpoint()")
            .await
            .as_deref(),
        Some("2")
    );
    assert_eq!(ids(&fresh).await, Ok(c.rows.clone()));
    let final_sequence = fenced_final(&c.p_data, &c.primary).await;
    assert_eq!(epoch_record(&c.s_data), (2, true, Some(final_sequence)));
}

/// Scenarios 3 (a budget too short) and 4 (a second call during a
/// promotion), the promotion held before it ends any session for as long as
/// a file exists — gated on the file, never on a timer.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_budget_too_short_answers_f_and_a_second_call_is_refused() {
    use heliosdb_nano::replication::promote::online::PAUSE_ONLINE_PROMOTE_FILE_ENV;
    let pause_dir = tempfile::tempdir().unwrap();
    let pause = pause_dir.path().join("pause");
    std::fs::write(&pause, b"").unwrap();
    let c = promotable(10, ARCHIVE, vec![(PAUSE_ONLINE_PROMOTE_FILE_ENV, text(&pause))]).await;
    let first = connect(c.pair.pg_s).await;
    let started = Instant::now();
    let (_, answer) = promote_answer(&first, "SELECT pg_promote(true, 1)").await.unwrap();
    assert_eq!(answer, "f", "the budget ran out while the promotion runs");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    // Still under way (held): a second call is refused.
    let second = connect(c.pair.pg_s).await;
    let refused = second.simple_query("SELECT pg_promote(true, 5)").await.unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    assert!(
        message(&refused).contains("a promotion is in progress since"),
        "{}",
        message(&refused)
    );
    assert!(
        c.standby
            .log_contains_within("the promotion waits while", Duration::from_secs(5)),
        "{}",
        c.standby.tail(12)
    );
    // The pause outlasted the budget, so the drain gets no second chance:
    // the refused caller's session must be gone before the promotion goes
    // on (the budget was `wait_seconds`, spent while it waited).
    drop(second);
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Released: it completes.
    std::fs::remove_file(&pause).unwrap();
    wait_primary(c.pair.pg_s, Duration::from_secs(60), &c.standby).await;
    let body = wait_health(c.pair.http_s, Duration::from_secs(30), &c.standby, |block| {
        block["epoch"] == 2
    })
    .await;
    assert_eq!(body["replication"]["role"], "primary", "{body}");
    let final_sequence = fenced_final(&c.p_data, &c.primary).await;
    assert_eq!(epoch_record(&c.s_data), (2, true, Some(final_sequence)));
}

/// Scenario 5: a primary dead BEFORE the call — `refuse` keeps the standby,
/// `allow` promotes it unfenced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_primary_is_promoted_past_only_when_allowed() {
    let mut c = promotable(10, REFUSE_UNFENCED, Vec::new()).await;
    let dir = c.pair.dir.path().to_path_buf();
    c.primary.kill();
    // `refuse`: 55000 naming the offline command; S stays a standby.
    let s = connect(c.pair.pg_s).await;
    let refused = s.simple_query("SELECT pg_promote(true, 20)").await.unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    let text_of = message(&refused);
    assert!(
        text_of.contains("did not answer the fence")
            && text_of.contains("could not be reached")
            && text_of.contains("heliosdb-nano promote --force --data-dir")
            && text_of.contains("promote_without_fence = \"allow\"")
            && text_of.contains("is NOT fenced")
            && text_of.contains("this node stays a standby"),
        "{text_of}"
    );
    let fresh = connect(c.pair.pg_s).await;
    assert_eq!(
        value(&fresh, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("true"),
        "the aborted promotion left a standby"
    );
    let body = wait_health(c.pair.http_s, Duration::from_secs(30), &c.standby, |block| {
        block["role"] == "standby" && block["state"] != "promoting"
    })
    .await;
    assert!(
        ["reconnecting", "connecting", "streaming"].contains(&body["replication"]["state"].as_str().unwrap_or("")),
        "the receiver is back: {body}"
    );
    assert!(!c.s_data.join("NANO-PROMOTION.json").exists());
    assert!(c.s_data.join("NANO-PHYSICAL-REPLICA.json").exists());

    // `allow`: P comes back, S streams again with the default setting, P is
    // killed again, and the promotion goes through unfenced.
    c.standby.stop();
    let mut primary = Daemon::spawn(
        &dir,
        "primary-again",
        &c.p_data,
        &c.pair.token,
        &c.pair.primary_args,
        Some(ARCHIVE),
    );
    let s_args = promotable_standby_args(c.pair.pg_s, c.pair.repl_p, c.pair.http_s, c.repl_s);
    let mut standby = Daemon::spawn(
        &dir,
        "standby-allow",
        &c.s_data,
        &c.pair.token,
        &s_args,
        Some(ALLOW_UNFENCED),
    );
    wait_http(c.pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    primary.kill();
    let s = connect(c.pair.pg_s).await;
    let (_, answer) = promote_answer(&s, "SELECT pg_promote(true, 30)")
        .await
        .unwrap_or_else(|error| panic!("pg_promote failed: {error}; log tail: {}", standby.tail(20)));
    assert_eq!(answer, "t");
    wait_primary(c.pair.pg_s, Duration::from_secs(30), &standby).await;
    assert!(
        standby.log_contains_within("WITHOUT the fence", Duration::from_secs(10)),
        "{}",
        standby.tail(20)
    );
    let fresh = connect(c.pair.pg_s).await;
    assert_eq!(ids(&fresh).await, Ok(c.rows.clone()));
    drop(fresh);
    standby.stop();
    assert_eq!(epoch_record(&c.s_data), (2, false, None));
}

/// Scenario 6: a failure (or a kill) after the promotion's record ends the
/// process; `start` refuses the directory until `promote` finished it.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failure_after_the_record_ends_the_process_and_promote_finishes_it() {
    use heliosdb_nano::replication::promote::online::FAIL_ONLINE_PROMOTE_AT_ENV;
    use heliosdb_nano::replication::promote::KILL_PROMOTE_AT_ENV;
    for (name, env, step) in [
        ("fail-after-record", FAIL_ONLINE_PROMOTE_AT_ENV, "after-record"),
        ("kill-after-epoch", KILL_PROMOTE_AT_ENV, "after-epoch"),
        ("fail-reopen", FAIL_ONLINE_PROMOTE_AT_ENV, "reopen"),
    ] {
        let mut c = promotable(10, ARCHIVE, vec![(env, step.to_string())]).await;
        let dir = c.pair.dir.path().to_path_buf();
        let s = connect(c.pair.pg_s).await;
        let answer = s.simple_query("SELECT pg_promote(true, 30)").await;
        assert!(
            answer.is_err(),
            "{name}: the process ended before it answered: {answer:?}"
        );
        let status = c
            .standby
            .wait_exit(Duration::from_secs(60))
            .unwrap_or_else(|| panic!("{name}: the standby process did not end; {}", c.standby.tail(20)));
        let log = c.standby.log();
        if env == FAIL_ONLINE_PROMOTE_AT_ENV {
            assert_eq!(status.code(), Some(2), "{name}: exit 2; {log}");
        }
        if step == "reopen" {
            // Converted, not served: the node starts as the primary.
            assert!(
                log.contains("start the node with `heliosdb-nano start --replication-role primary"),
                "{name}: {log}"
            );
            assert!(!c.s_data.join("NANO-PROMOTION.json").exists(), "{name}");
        } else {
            if env == FAIL_ONLINE_PROMOTE_AT_ENV {
                assert!(
                    log.contains("finish the promotion with `heliosdb-nano promote --data-dir"),
                    "{name}: {log}"
                );
            }
            assert!(
                c.s_data.join("NANO-PROMOTION.json").exists(),
                "{name}: the record survives"
            );
            // `start` refuses the directory, naming the command.
            let s_args = primary_args(c.pair.pg_s, c.repl_s, c.pair.http_s);
            let mut refused = Daemon::spawn(
                &dir,
                &format!("{name}-start"),
                &c.s_data,
                &c.pair.token,
                &s_args,
                Some(ARCHIVE),
            );
            let exit = refused.wait_exit(Duration::from_secs(60));
            assert_eq!(
                exit.and_then(|status| status.code()),
                Some(2),
                "{name}: {}",
                refused.log()
            );
            assert!(
                refused
                    .log()
                    .contains("promotion interrupted: finish it with `heliosdb-nano promote --data-dir"),
                "{name}: {}",
                refused.log()
            );
            // The offline command finishes it with the record's values.
            let output = cli_async(promote_args(
                &c.s_data,
                c.pair.repl_p,
                &c.pair.token,
                &["--fence-timeout", "10"],
            ))
            .await;
            let out = stdout(&output);
            assert!(output.status.success(), "{name}: {out}{}", stderr(&output));
            assert!(out.contains("carries an interrupted promotion"), "{name}: {out}");
        }
        // The node serves epoch 2 as the primary.
        let s_args = primary_args(c.pair.pg_s, c.repl_s, c.pair.http_s);
        let s_primary = Daemon::spawn(
            &dir,
            &format!("{name}-primary"),
            &c.s_data,
            &c.pair.token,
            &s_args,
            Some(ARCHIVE),
        );
        let s = connect(c.pair.pg_s).await;
        assert_eq!(
            value(&s, "SELECT timeline_id::bigint FROM pg_control_checkpoint()")
                .await
                .as_deref(),
            Some("2"),
            "{name}"
        );
        assert_eq!(ids(&s).await, Ok(c.rows.clone()), "{name}");
        assert_eq!(insert(&s, 100..=101, "after").await, 0, "{name}");
        drop(s);
        drop(s_primary);
        c.primary.stop();
    }
}

/// Scenario 7: the fence pushes its termination — the idle sessions of a
/// primary fenced by the OFFLINE promotion receive FATAL 57P01 within a
/// second of the fence, without a statement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fence_pushes_its_termination_to_idle_sessions() {
    let mut c = promotable(10, ARCHIVE, Vec::new()).await;
    c.standby.stop();
    let idle = raw_session(c.pair.pg_p).await;
    let reader = tokio::task::spawn_blocking(move || idle.pushed(Duration::from_secs(120)));
    let promote = tokio::spawn(cli_async(promote_args(
        &c.s_data,
        c.pair.repl_p,
        &c.pair.token,
        &["--fence-timeout", "60"],
    )));
    // When the primary is first seen fencing (or fenced).
    let deadline = Instant::now() + Duration::from_secs(120);
    let fenced_seen = loop {
        let block = health(c.pair.http_p).await["replication"].clone();
        if block["state"] == "fenced" || block["state"] == "fencing" {
            break Instant::now();
        }
        assert!(Instant::now() < deadline, "P was never fenced: {}", c.primary.tail(12));
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let pushed = reader.await.unwrap().unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        (pushed.field('S'), pushed.field('V'), pushed.field('C')),
        ("FATAL", "FATAL", "57P01"),
        "{pushed:?}"
    );
    assert!(
        pushed.field('M').contains("this primary was fenced by epoch 2"),
        "{pushed:?}"
    );
    assert!(pushed.closed, "{pushed:?}");
    assert!(
        pushed.at.saturating_duration_since(fenced_seen) <= Duration::from_secs(1),
        "pushed within a second of the fence: {:?} after it was seen",
        pushed.at.saturating_duration_since(fenced_seen)
    );
    let output = promote.await.unwrap();
    assert!(output.status.success(), "{}{}", stdout(&output), stderr(&output));
}

/// Scenario 8 (spec §8.A): a statement held inside the engine on the
/// standby. Its session was told to end, but it holds the store until the
/// statement answers: the promotion waits for it — and fences the primary
/// only after — or, with a budget it outlasts, aborts with the primary
/// still unfenced.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_statement_holds_the_promotion_and_the_primary_stays_unfenced() {
    let hold_dir = tempfile::tempdir().unwrap();
    let hold = hold_dir.path().join("hold");
    let c = promotable(10, ARCHIVE, vec![(HOLD_STATEMENT_ENV, text(&hold))]).await;

    // --- a budget the statement outlasts: aborted, P unfenced -----------
    std::fs::write(&hold, b"").unwrap();
    let holder = connect(c.pair.pg_s).await;
    let held = tokio::spawn(async move { holder.simple_query("SELECT heliosdb_test_hold()").await });
    wait_log(&c.standby, "this statement holds while", 1, Duration::from_secs(30)).await;
    let caller = connect(c.pair.pg_s).await;
    let refused = caller.simple_query("SELECT pg_promote(true, 3)").await.unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    let text_of = message(&refused);
    assert!(
        text_of.contains("still use the standby's store")
            && text_of.contains("never cut short")
            && text_of.contains("is NOT fenced")
            && text_of.contains("with a larger N"),
        "{text_of}"
    );
    // P was never fenced: it takes writes; S is a standby serving its store.
    assert!(!c.p_data.join("NANO-FENCED.json").exists());
    assert_eq!(health(c.pair.http_p).await["replication"]["state"], "streaming");
    let p = connect(c.pair.pg_p).await;
    assert_eq!(insert(&p, 11..=11, "unfenced").await, 0);
    let mut rows = c.rows.clone();
    rows.push(11);
    assert_eq!(
        settle(c.pair.pg_s, &rows, Duration::from_secs(60)).await,
        Ok(rows.clone())
    );
    let fresh = connect(c.pair.pg_s).await;
    assert_eq!(
        value(&fresh, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("true")
    );
    drop(fresh);
    // The statement was never cut short: released, it answers.
    std::fs::remove_file(&hold).unwrap();
    let answered = held.await.unwrap().expect("the held statement answers");
    assert_eq!(row_texts(&answered), vec!["t".to_string()]);

    // --- a budget it does not outlast: the promotion waits for it --------
    wait_http(c.pair.http_s, "/ready", 200, Duration::from_secs(60), &c.standby).await;
    std::fs::write(&hold, b"").unwrap();
    let holder = connect(c.pair.pg_s).await;
    let held = tokio::spawn(async move { holder.simple_query("SELECT heliosdb_test_hold()").await });
    wait_log(&c.standby, "this statement holds while", 2, Duration::from_secs(30)).await;
    let caller = connect(c.pair.pg_s).await;
    let promote = tokio::spawn(async move {
        promote_answer(&caller, "SELECT pg_promote(true, 60)")
            .await
            .map(|(_, answer)| answer)
    });
    // For 3 s: S is promoting, P is unfenced and takes writes.
    wait_health(c.pair.http_s, Duration::from_secs(10), &c.standby, |block| {
        block["state"] == "promoting"
    })
    .await;
    let until = Instant::now() + Duration::from_secs(2);
    let mut id = 12;
    while Instant::now() < until {
        let s_block = health(c.pair.http_s).await["replication"].clone();
        assert_eq!(s_block["state"], "promoting", "{s_block}");
        let p_block = health(c.pair.http_p).await["replication"].clone();
        assert_eq!(
            p_block["state"], "streaming",
            "the primary is not fenced while a session still holds the standby's store: {p_block}"
        );
        assert_eq!(insert(&p, id..=id, "while-held").await, 0, "P takes writes");
        id += 1;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(!c.p_data.join("NANO-FENCED.json").exists());
    // Released: the statement answers, then the promotion fences P and
    // completes.
    std::fs::remove_file(&hold).unwrap();
    let answered = held.await.unwrap().expect("the held statement answers");
    assert_eq!(row_texts(&answered), vec!["t".to_string()]);
    let answer = promote
        .await
        .unwrap()
        .unwrap_or_else(|error| panic!("pg_promote failed: {error}; {}", c.standby.tail(20)));
    assert_eq!(answer, "t");
    let final_sequence = fenced_final(&c.p_data, &c.primary).await;
    assert_eq!(epoch_record(&c.s_data), (2, true, Some(final_sequence)));
    let s = connect(c.pair.pg_s).await;
    let all: Vec<i32> = (1..id).collect();
    assert_eq!(
        ids(&s).await,
        Ok(all),
        "every write P took while the statement held is here"
    );
}

/// Scenario 9 (spec §8.C, §8.E): the old primary dies after it answered the
/// fence, before the standby holds its final sequence. `allow` does not
/// apply after `Fenced`: the promotion aborts naming both sequences and
/// `promote --force`. The old primary restarted (it stays fenced), the
/// same promotion asked again is answered the cached `Fenced`, the catch-up
/// completes, and the standby is promoted, fenced.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_primary_that_dies_after_fenced_aborts_the_promotion_and_a_rerun_finishes_it() {
    use heliosdb_nano::replication::physical_receiver::RECEIVER_HOLD_DIR_ENV;
    let hold_dir = tempfile::tempdir().unwrap();
    let before = hold_dir.path().join("before-fence");
    let after = hold_dir.path().join("after-fenced");
    std::fs::write(&before, b"").unwrap();
    std::fs::write(&after, b"").unwrap();
    let mut c = promotable(10, ALLOW_UNFENCED, vec![(RECEIVER_HOLD_DIR_ENV, text(hold_dir.path()))]).await;
    let dir = c.pair.dir.path().to_path_buf();
    let caller = connect(c.pair.pg_s).await;
    let promote = tokio::spawn(async move { caller.simple_query("SELECT pg_promote(true, 60)").await });
    wait_log(&c.standby, "holds at before-fence", 1, Duration::from_secs(60)).await;
    // The old primary still takes writes: the standby must catch up to them.
    let writer = connect(c.pair.pg_p).await;
    assert_eq!(insert(&writer, 11..=15, "before-the-fence").await, 0);
    drop(writer);
    std::fs::remove_file(&before).unwrap();
    wait_log(&c.standby, "holds at after-fenced", 1, Duration::from_secs(60)).await;
    let final_sequence = fenced_final(&c.p_data, &c.primary).await;
    // Dead after `Fenced`, before the standby holds its final sequence.
    c.primary.kill();
    std::fs::remove_file(&after).unwrap();
    let refused = promote.await.unwrap().unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    let text_of = message(&refused);
    assert!(
        text_of.contains(&format!("IS fenced by epoch 2 at its sequence {final_sequence}"))
            && text_of.contains("holds its writes through sequence")
            && text_of.contains("accept losing its sequences")
            && text_of.contains("heliosdb-nano promote --force --data-dir")
            && !text_of.contains("WITHOUT the fence"),
        "{text_of}"
    );
    // Nothing was promoted unfenced.
    let fresh = connect(c.pair.pg_s).await;
    assert_eq!(
        value(&fresh, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("true")
    );
    drop(fresh);
    assert!(!c.s_data.join("NANO-PROMOTION.json").exists());
    assert_eq!(epoch_record(&c.s_data), (1, false, None));

    // The old primary comes back (fenced); asked again, the promotion is
    // answered the cached `Fenced` and completes.
    let primary = Daemon::spawn(
        &dir,
        "primary-fenced",
        &c.p_data,
        &c.pair.token,
        &c.pair.primary_args,
        Some(ARCHIVE),
    );
    wait_health(c.pair.http_s, Duration::from_secs(120), &c.standby, |block| {
        block["state"] == "streaming"
    })
    .await;
    let caller = connect(c.pair.pg_s).await;
    let (_, answer) = promote_answer(&caller, "SELECT pg_promote(true, 60)")
        .await
        .unwrap_or_else(|error| panic!("pg_promote failed: {error}; {}", c.standby.tail(20)));
    assert_eq!(answer, "t");
    assert!(
        c.standby
            .log_contains_within("asks again for the fence", Duration::from_secs(10)),
        "{}",
        c.standby.tail(20)
    );
    assert_eq!(epoch_record(&c.s_data), (2, true, Some(final_sequence)));
    let s = connect(c.pair.pg_s).await;
    let all: Vec<i32> = (1..=15).collect();
    assert_eq!(ids(&s).await, Ok(all));
    drop(primary);
}

/// Scenario 10 (spec §8.J): a promotion aborted once its replication had
/// stopped (the primary is dead, `allow` went ahead, an injected failure at
/// the release). The standby's receiver starts again once its store is
/// closed, its cascading port serves its downstream standby again
/// (`/health` `cascade`), and both follow the primary once it is back.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aborted_promotion_restores_the_receiver_and_the_cascading_port() {
    use heliosdb_nano::replication::promote::online::FAIL_ONLINE_PROMOTE_AT_ENV;
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let token = pair.token.clone();
    let p_data = dir.join("primary");
    let (repl_s, pg_s3, http_s3) = (free_port(), free_port(), free_port());
    let mut primary = Daemon::spawn(&dir, "primary", &p_data, &token, &pair.primary_args, Some(ARCHIVE));
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=10, "before").await, 0);
    drop(writer);
    let s_args = promotable_standby_args(pair.pg_s, pair.repl_p, pair.http_s, repl_s);
    let config = "[replication]\npromote_fence_timeout = \"3s\"\n[replication.physical]\ncascade = true\n\
                  [replication.archive]\nenabled = true\nsegment_max_secs = 1\n";
    let standby = Daemon::spawn_with(
        &dir,
        "standby",
        &dir.join("standby"),
        &token,
        &s_args,
        Some(config),
        &SpawnOptions {
            listen: None,
            env: vec![(FAIL_ONLINE_PROMOTE_AT_ENV, "release".to_string())],
        },
    );
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    let standby3 = Daemon::spawn(
        &dir,
        "standby3",
        &dir.join("standby3"),
        &token,
        &standby_args(pg_s3, repl_s, http_s3),
        Some(ARCHIVE),
    );
    wait_http(http_s3, "/ready", 200, Duration::from_secs(120), &standby3).await;
    let first: Vec<i32> = (1..=10).collect();
    assert_eq!(settle(pg_s3, &first, Duration::from_secs(60)).await, Ok(first.clone()));
    assert_eq!(health(pair.http_s).await["cascade"], true);

    primary.kill();
    let s = connect(pair.pg_s).await;
    let refused = s.simple_query("SELECT pg_promote(true, 30)").await.unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    let text_of = message(&refused);
    assert!(
        text_of.contains("an injected failure") && text_of.contains("is NOT fenced") && text_of.contains("Next:"),
        "{text_of}"
    );
    // Restored: the receiver started again, the cascading port serves again.
    assert!(
        standby.log_contains_within("follows its primary again", Duration::from_secs(60)),
        "{}",
        standby.tail(20)
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    while health(pair.http_s).await["cascade"] != true {
        assert!(
            Instant::now() < deadline,
            "the cascading port was not restored: {}",
            standby.tail(20)
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(!dir.join("standby").join("NANO-PROMOTION.json").exists());
    assert_eq!(epoch_record(&dir.join("standby")), (1, false, None));

    // P is back: S follows it again, and S3 follows S.
    let primary = Daemon::spawn(
        &dir,
        "primary-again",
        &p_data,
        &token,
        &pair.primary_args,
        Some(ARCHIVE),
    );
    wait_health(pair.http_s, Duration::from_secs(120), &standby, |block| {
        block["state"] == "streaming"
    })
    .await;
    let writer = connect(pair.pg_p).await;
    assert_eq!(insert(&writer, 11..=20, "after").await, 0);
    let all: Vec<i32> = (1..=20).collect();
    assert_eq!(settle(pair.pg_s, &all, Duration::from_secs(60)).await, Ok(all.clone()));
    assert_eq!(settle(pg_s3, &all, Duration::from_secs(60)).await, Ok(all.clone()));
    drop(standby3);
    drop(standby);
    drop(primary);
}

/// Scenario 11: `pg_promote` over the extended protocol — described as one
/// `boolean` (OID 16), answered with a DataRow and CommandComplete, then
/// the session is ended with FATAL 57P01 once its Sync was answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_extended_protocol_pg_promote_is_answered_then_the_session_ends() {
    let c = promotable(10, ARCHIVE, Vec::new()).await;
    let (client, connection) = connect_watched(c.pair.pg_s).await;
    let statement = client.prepare("SELECT pg_promote(true, 60)").await.unwrap();
    assert_eq!(statement.columns().len(), 1);
    assert_eq!(statement.columns()[0].name(), "pg_promote");
    assert_eq!(
        statement.columns()[0].type_(),
        &tokio_postgres::types::Type::BOOL,
        "Describe says boolean"
    );
    assert_eq!(statement.columns()[0].type_().oid(), 16);
    let rows = client
        .query(&statement, &[])
        .await
        .unwrap_or_else(|error| panic!("pg_promote failed: {error}; {}", c.standby.tail(20)));
    assert_eq!(rows.len(), 1);
    let promoted: bool = rows[0].get(0);
    assert!(promoted, "t once it serves as the primary");
    // After its Sync was answered the session is ended (pushed).
    let ended = connection_end(connection, Duration::from_secs(30))
        .await
        .expect_err("the caller's session is ended after its answer");
    assert_eq!(sqlstate(&ended), "57P01", "{ended}");
    assert!(
        message(&ended).contains("this standby is being promoted to primary (epoch 2)"),
        "{}",
        message(&ended)
    );
    wait_primary(c.pair.pg_s, Duration::from_secs(10), &c.standby).await;
    let final_sequence = fenced_final(&c.p_data, &c.primary).await;
    assert_eq!(epoch_record(&c.s_data), (2, true, Some(final_sequence)));
}

/// Scenario 12: a CASCADING standby promoted online. Its replication port —
/// its cascading source's until replication stopped — serves epoch 2 as the
/// primary's; `/health` no longer says `cascade`; its cascaded standby,
/// which follows epoch 1, needs recovery and recovers from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cascading_standby_is_promoted_online_and_serves_its_port_as_the_primary() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let token = pair.token.clone();
    let p_data = dir.join("primary");
    let s_data = dir.join("standby");
    let s3_data = dir.join("standby3");
    let (repl_s, pg_s3, http_s3) = (free_port(), free_port(), free_port());
    let primary = Daemon::spawn(&dir, "primary", &p_data, &token, &pair.primary_args, Some(ARCHIVE));
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=20, "before").await, 0);
    drop(writer);
    let s_args = promotable_standby_args(pair.pg_s, pair.repl_p, pair.http_s, repl_s);
    let cascade = format!("[replication.physical]\ncascade = true\n{ARCHIVE}");
    let standby = Daemon::spawn(&dir, "standby", &s_data, &token, &s_args, Some(&cascade));
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    let s3_args = standby_args(pg_s3, repl_s, http_s3);
    let mut standby3 = Daemon::spawn(&dir, "standby3", &s3_data, &token, &s3_args, Some(ARCHIVE));
    wait_http(http_s3, "/ready", 200, Duration::from_secs(120), &standby3).await;
    let rows: Vec<i32> = (1..=20).collect();
    assert_eq!(settle(pg_s3, &rows, Duration::from_secs(60)).await, Ok(rows.clone()));
    assert_eq!(health(pair.http_s).await["cascade"], true);

    let caller = connect(pair.pg_s).await;
    let (_, answer) = promote_answer(&caller, "SELECT pg_promote(true, 60)")
        .await
        .unwrap_or_else(|error| panic!("pg_promote failed: {error}; {}", standby.tail(20)));
    assert_eq!(answer, "t");
    wait_primary(pair.pg_s, Duration::from_secs(10), &standby).await;
    let body = wait_health(pair.http_s, Duration::from_secs(30), &standby, |block| {
        block["role"] == "primary" && block["epoch"] == 2
    })
    .await;
    assert!(body["cascade"].is_null(), "a primary does not cascade: {body}");
    assert!(
        standby.log_contains_within(
            &format!("Replication port listening on 127.0.0.1:{repl_s} (NPHYS004"),
            Duration::from_secs(10)
        ),
        "{}",
        standby.tail(20)
    );
    let final_sequence = fenced_final(&p_data, &primary).await;
    assert_eq!(epoch_record(&s_data), (2, true, Some(final_sequence)));

    // S3 follows epoch 1: refused by the primary of epoch 2, it recovers.
    wait_health(http_s3, Duration::from_secs(60), &standby3, |block| {
        block["state"] == "needs_recovery"
    })
    .await;
    standby3.stop();
    let output = cli_async(recover_args(
        &s3_data,
        repl_s,
        &token,
        &["--full-copy-threshold", "100"],
    ))
    .await;
    assert!(output.status.success(), "{}{}", stdout(&output), stderr(&output));
    let s = connect(pair.pg_s).await;
    assert_eq!(insert(&s, 21..=25, "epoch-2").await, 0);
    let standby3 = Daemon::spawn(&dir, "standby3-follows-s", &s3_data, &token, &s3_args, Some(ARCHIVE));
    wait_http(http_s3, "/ready", 200, Duration::from_secs(120), &standby3).await;
    let all: Vec<i32> = (1..=25).collect();
    assert_eq!(settle(pg_s3, &all, Duration::from_secs(60)).await, Ok(all));
    assert_eq!(health(http_s3).await["replication"]["epoch"], 2);
    drop(standby3);
    drop(standby);
    drop(primary);
}
