//! Spec P1.3: `start --replication-role standby` serves a PHYSICAL replica, and
//! answers the PostgreSQL-wire contract HeliosDB Proxy probes
//! (`docs/plans/specs/P1_3_standby_start_posture_pgwire.md`, plan §D8).
//!
//! Real managed daemons, real wire, as in `tests/replication_restart_gap.rs`:
//! a primary (which serves its replication port because it has a replication
//! token) and a standby bootstrapped from it into a directory that does not
//! exist yet. Spec P2.1: the standby's `--primary-host` IS the primary's
//! `--replication-port` — one port, one connection. Then:
//!
//! * the standby reaches `/ready` 200 and serves exactly the primary's rows;
//! * the probes answer as specified — `heliosdb_require_ready()`,
//!   `pg_is_in_recovery()` (and `::text`), both LSN functions (`::text`, `HI/LO`
//!   hex of the physical sequence), `timeline_id` from `pg_control_checkpoint()`,
//!   the view `heliosdb_standby_status`, and `in_hot_standby` in the raw
//!   startup frame (`on` / `off`);
//! * writes and `nextval` are SQLSTATE 25006;
//! * SIGKILL of the primary: the standby stays up, `/ready` stays 200 within
//!   `max_standby_lag` (3 s here), then 503 and data reads 55000 while the
//!   probes still answer; the restarted primary brings it back with every row;
//! * SIGKILL of the standby: the restart resumes from its durable cursor —
//!   no second copy;
//! * a 4.41.x logical standby directory is refused with the conversion message
//!   (and stays up by default); with `on_unrecoverable = "exit"` a refusal —
//!   here a primary that is now a different dataset — exits the process.
//!
//! Red on the base of this change (8ee68a2f): the standby ran the logical
//! applicator over a writable database, none of the probe functions, the view,
//! `/ready` or `in_hot_standby` existed, and every refusal exited.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};
use tokio_postgres::{NoTls, SimpleQueryMessage};

const LEGACY_REFUSAL: &str = "this directory is a logical standby from 4.41.x; physical standbys are rebuilt once: \
     start with a new, empty data directory (docs/guides/physical-standby.md)";

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Kills exactly the child it owns when dropped, so a failed assertion cannot
/// leave a daemon holding ports or a data directory.
struct Daemon {
    child: Option<Child>,
    log: PathBuf,
}

impl Daemon {
    /// `start` with trust auth on loopback, the pair's token, `args`, and an
    /// optional TOML `config`. Each spawn has its own log (`<name>.log`).
    fn spawn(dir: &Path, name: &str, data: &Path, token: &Path, args: &[String], config: Option<&str>) -> Self {
        let log = dir.join(format!("{name}.log"));
        let file = std::fs::File::create(&log).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
        command
            .arg("start")
            .arg("--data-dir")
            .arg(data)
            .args(["--listen", "127.0.0.1", "--auth", "trust"])
            .arg("--replication-token-file")
            .arg(token)
            .args(args)
            .env("RUST_LOG", "info")
            .env_remove("HELIOSDB_NANO_READY_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::from(file.try_clone().unwrap()))
            .stderr(Stdio::from(file));
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

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn alive(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| child.try_wait().unwrap().is_none())
    }

    /// Wait up to `budget` for the daemon to exit on its own; `None` if it is still running.
    fn wait_exit(&mut self, budget: Duration) -> Option<ExitStatus> {
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
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Graceful stop (SIGTERM), then wait; a daemon that ignores it is killed.
    fn stop(&mut self) {
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

/// A primary and a standby on private ports, sharing one token.
struct Pair {
    dir: tempfile::TempDir,
    token: PathBuf,
    pg_p: u16,
    pg_s: u16,
    http_p: u16,
    http_s: u16,
    primary_args: Vec<String>,
    standby_args: Vec<String>,
}

fn pair() -> Pair {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("replication.token");
    std::fs::write(&token, [0x42u8; 32]).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let (pg_p, repl_p, http_p, pg_s, http_s) = (free_port(), free_port(), free_port(), free_port(), free_port());
    let primary_args = vec![
        "--port".into(),
        pg_p.to_string(),
        "--replication-role".into(),
        "primary".into(),
        "--replication-port".into(),
        repl_p.to_string(),
        "--http-port".into(),
        http_p.to_string(),
    ];
    let standby_args = vec![
        "--port".into(),
        pg_s.to_string(),
        "--replication-role".into(),
        "standby".into(),
        "--primary-host".into(),
        format!("127.0.0.1:{repl_p}"),
        "--http-port".into(),
        http_s.to_string(),
        "--replication-startup-timeout-secs".into(),
        "30".into(),
    ];
    Pair {
        dir,
        token,
        pg_p,
        pg_s,
        http_p,
        http_s,
        primary_args,
        standby_args,
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn http_get_blocking(port: u16, path: &str) -> Option<(u16, serde_json::Value)> {
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

async fn http(port: u16, path: &'static str) -> Option<(u16, serde_json::Value)> {
    tokio::task::spawn_blocking(move || http_get_blocking(port, path))
        .await
        .unwrap()
}

/// Poll `path` until it answers `want`; panics with the last answer otherwise.
async fn wait_http(port: u16, path: &'static str, want: u16, budget: Duration, daemon: &Daemon) -> serde_json::Value {
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
            daemon.log().lines().rev().take(12).collect::<Vec<_>>().join(" | ")
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL wire
// ---------------------------------------------------------------------------

async fn connect(port: u16) -> tokio_postgres::Client {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let url = format!("host=127.0.0.1 port={port} user=postgres dbname=postgres connect_timeout=2");
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
/// the wire carries), and its column name.
async fn one(client: &tokio_postgres::Client, sql: &str) -> Result<(String, Option<String>), tokio_postgres::Error> {
    let messages = client.simple_query(sql).await?;
    let rows: Vec<_> = messages
        .iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 1, "{sql}: exactly one row");
    assert_eq!(rows[0].len(), 1, "{sql}: exactly one column");
    Ok((
        rows[0].columns()[0].name().to_string(),
        rows[0].get(0).map(str::to_string),
    ))
}

async fn value(client: &tokio_postgres::Client, sql: &str) -> Option<String> {
    one(client, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}")).1
}

fn sqlstate(error: &tokio_postgres::Error) -> String {
    error.code().map(|code| code.code().to_string()).unwrap_or_default()
}

fn message(error: &tokio_postgres::Error) -> String {
    error
        .as_db_error()
        .map(|db| db.message().to_string())
        .unwrap_or_default()
}

/// `HI/LO` hex, as PostgreSQL prints an LSN; returns the sequence.
fn lsn(text: &str) -> u64 {
    let (hi, lo) = text.split_once('/').unwrap_or_else(|| panic!("not an LSN: {text}"));
    assert!(
        !hi.is_empty() && !lo.is_empty() && hi.bytes().chain(lo.bytes()).all(|b| b.is_ascii_hexdigit()),
        "not HI/LO hex: {text}"
    );
    (u64::from_str_radix(hi, 16).unwrap() << 32) | u64::from_str_radix(lo, 16).unwrap()
}

async fn ids(client: &tokio_postgres::Client) -> Result<Vec<i32>, String> {
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
async fn settle(port: u16, want: &[i32], budget: Duration) -> Result<Vec<i32>, String> {
    let deadline = Instant::now() + budget;
    loop {
        let seen = match tokio_postgres::connect(
            &format!("host=127.0.0.1 port={port} user=postgres dbname=postgres connect_timeout=2"),
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

/// The raw ParameterStatus payloads of one startup as `application_name`,
/// as Proxy's health and topology logins send it (`postgres`/`postgres`,
/// trust). Panics on a startup ErrorResponse: readiness is never one.
fn startup_parameters_blocking(port: u16, application_name: &str) -> Vec<Vec<u8>> {
    let mut stream =
        TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(3)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut parameters = b"user\0postgres\0database\0postgres\0application_name\0".to_vec();
    parameters.extend_from_slice(application_name.as_bytes());
    parameters.extend_from_slice(b"\0\0");
    let mut startup = ((parameters.len() + 8) as u32).to_be_bytes().to_vec();
    startup.extend_from_slice(&196608u32.to_be_bytes());
    startup.extend_from_slice(&parameters);
    stream.write_all(&startup).unwrap();
    let mut statuses = Vec::new();
    for _ in 0..64 {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).unwrap();
        let size = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
        let mut payload = vec![0; size - 4];
        stream.read_exact(&mut payload).unwrap();
        assert_ne!(header[0], b'E', "startup error: {}", String::from_utf8_lossy(&payload));
        if header[0] == b'S' {
            statuses.push(payload);
        }
        if header[0] == b'Z' {
            return statuses;
        }
    }
    panic!("no ReadyForQuery received");
}

async fn startup_parameters(port: u16, application_name: &'static str) -> Vec<Vec<u8>> {
    tokio::task::spawn_blocking(move || startup_parameters_blocking(port, application_name))
        .await
        .unwrap()
}

/// Every entry of `dir` with its size, so a refused directory can be shown untouched.
fn census(dir: &Path) -> BTreeSet<(String, u64)> {
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

const FROZEN_FIELDS: [&str; 11] = [
    "role",
    "state",
    "ready",
    "cursor",
    "primary_cursor",
    "lag_bytes",
    "lag_secs",
    "primary",
    "sync_mode",
    "last_ack_sequence",
    "epoch",
];

/// Spec P2.2 added these after the frozen eleven (additive only).
const ADDED_FIELDS: [&str; 3] = ["tls", "reason", "progress"];

fn assert_frozen_block(block: &serde_json::Value) {
    let keys: BTreeSet<&str> = block
        .as_object()
        .unwrap_or_else(|| panic!("not a replication block: {block}"))
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        FROZEN_FIELDS.into_iter().chain(ADDED_FIELDS).collect::<BTreeSet<_>>(),
        "frozen field set, plus the additive P2.2 fields: {block}"
    );
    assert!(block["tls"].is_boolean(), "{block}");
    lsn(block["cursor"].as_str().unwrap());
    lsn(block["primary_cursor"].as_str().unwrap());
    assert!(block["lag_bytes"].is_u64(), "{block}");
    assert_eq!(block["epoch"], 1, "{block}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn physical_standby_serves_the_proxy_contract_and_survives_its_primary() {
    let p = pair();
    let standby_config = "[replication]\nmax_standby_lag = \"3s\"\n";
    let primary_data = p.dir.path().join("primary");
    let standby_data = p.dir.path().join("standby");

    let mut primary = Daemon::spawn(
        p.dir.path(),
        "primary-1",
        &primary_data,
        &p.token,
        &p.primary_args,
        None,
    );
    let writer = connect(p.pg_p).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .await
        .unwrap();
    writer
        .batch_execute("INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three')")
        .await
        .unwrap();

    let mut standby = Daemon::spawn(
        p.dir.path(),
        "standby-1",
        &standby_data,
        &p.token,
        &p.standby_args,
        Some(standby_config),
    );
    let ready = wait_http(p.http_s, "/ready", 200, Duration::from_secs(60), &standby).await;
    assert_frozen_block(&ready);
    assert_eq!(ready["role"], "standby");
    assert_eq!(ready["state"], "streaming");
    assert_eq!(ready["ready"], true);
    assert!(
        standby.log().contains("Bootstrapping physical standby"),
        "a fresh directory is bootstrapped from the primary"
    );

    let rows = ids(&writer).await.unwrap();
    assert_eq!(rows, vec![1, 2, 3]);
    assert_eq!(settle(p.pg_s, &rows, Duration::from_secs(30)).await, Ok(rows.clone()));

    // --- the PG-wire contract, on the standby ---
    let reader = connect(p.pg_s).await;
    assert_eq!(
        value(&reader, "SELECT heliosdb_require_ready()").await.as_deref(),
        Some("t")
    );
    assert_eq!(
        one(&reader, "SELECT pg_is_in_recovery()").await.unwrap(),
        ("pg_is_in_recovery".to_string(), Some("t".to_string()))
    );
    assert_eq!(
        one(&reader, "SELECT pg_is_in_recovery()::text").await.unwrap(),
        ("pg_is_in_recovery".to_string(), Some("true".to_string()))
    );
    let replay = lsn(&value(&reader, "SELECT pg_last_wal_replay_lsn()::text").await.unwrap());
    let current = lsn(&value(&reader, "SELECT pg_current_wal_lsn()::text").await.unwrap());
    assert!(
        replay > 0 && current >= replay,
        "standby: current {current} replay {replay}"
    );
    assert_eq!(
        value(&reader, "SELECT timeline_id::bigint FROM pg_control_checkpoint()")
            .await
            .as_deref(),
        Some("1")
    );
    let status = reader
        .simple_query("SELECT role, state, ready, epoch FROM heliosdb_standby_status")
        .await
        .unwrap();
    let row = status
        .iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (row.get(0), row.get(1), row.get(2), row.get(3)),
        (Some("standby"), Some("streaming"), Some("t"), Some("1"))
    );

    // --- and on the primary ---
    assert_eq!(value(&writer, "SELECT pg_is_in_recovery()").await.as_deref(), Some("f"));
    assert_eq!(value(&writer, "SELECT pg_last_wal_replay_lsn()::text").await, None);
    assert_eq!(
        value(&writer, "SELECT heliosdb_require_ready()").await.as_deref(),
        Some("t")
    );
    let written = lsn(&value(&writer, "SELECT pg_current_wal_lsn()::text").await.unwrap());
    assert!(written >= replay, "primary {written} behind its standby {replay}");

    // --- in_hot_standby, exact bytes of the startup frame ---
    let on = b"in_hot_standby\0on\0".to_vec();
    let off = b"in_hot_standby\0off\0".to_vec();
    assert!(startup_parameters(p.pg_s, "heliosdb-proxy-health").await.contains(&on));
    assert!(startup_parameters(p.pg_p, "heliosdb-proxy-health").await.contains(&off));

    // --- refusals ---
    let insert = reader
        .simple_query("INSERT INTO t VALUES (9, 'nine')")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&insert), "25006", "{insert}");
    let nextval = reader.simple_query("SELECT nextval('s')").await.unwrap_err();
    assert_eq!(sqlstate(&nextval), "25006", "{nextval}");

    // --- HTTP ---
    let (code, health) = http(p.http_s, "/health").await.unwrap();
    assert_eq!(code, 200);
    assert_eq!(health["status"], "ok");
    assert_frozen_block(&health["replication"]);
    assert_eq!(health["replication"]["role"], "standby");
    let (code, health) = http(p.http_p, "/health").await.unwrap();
    assert_eq!(code, 200);
    assert_frozen_block(&health["replication"]);
    assert_eq!(health["replication"]["role"], "primary");
    assert_eq!(health["replication"]["ready"], true);
    let (code, _) = http(p.http_p, "/ready").await.unwrap();
    assert_eq!(code, 200, "a primary is ready");

    // --- SIGKILL the primary: the standby stays up and keeps serving reads
    //     within max_standby_lag, then refuses them; the probes still answer ---
    primary.kill();
    assert_eq!(
        ids(&reader).await,
        Ok(rows.clone()),
        "reads are served within max_standby_lag"
    );
    let not_ready = wait_http(p.http_s, "/ready", 503, Duration::from_secs(30), &standby).await;
    assert_eq!(not_ready["ready"], false);
    assert_eq!(not_ready["state"], "reconnecting");
    assert!(standby.alive(), "the standby stays up without its primary");
    let refused = reader.simple_query("SELECT id FROM t").await.unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    assert!(
        message(&refused).starts_with("standby is not ready: reconnecting (cursor "),
        "{refused}"
    );
    let probe = reader
        .simple_query("SELECT heliosdb_require_ready()")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&probe), "55000", "{probe}");
    assert_eq!(value(&reader, "SELECT pg_is_in_recovery()").await.as_deref(), Some("t"));
    assert_eq!(value(&reader, "SELECT 1").await.as_deref(), Some("1"));
    assert!(
        startup_parameters(p.pg_s, "helios-topology").await.contains(&on),
        "a topology login is accepted while not ready"
    );

    // --- restart the primary: back to ready, with every row ---
    let _primary = Daemon::spawn(
        p.dir.path(),
        "primary-2",
        &primary_data,
        &p.token,
        &p.primary_args,
        None,
    );
    let writer = connect(p.pg_p).await;
    writer.batch_execute("INSERT INTO t VALUES (4, 'four')").await.unwrap();
    wait_http(p.http_s, "/ready", 200, Duration::from_secs(60), &standby).await;
    let rows = ids(&writer).await.unwrap();
    assert_eq!(rows, vec![1, 2, 3, 4]);
    assert_eq!(settle(p.pg_s, &rows, Duration::from_secs(30)).await, Ok(rows.clone()));

    // --- SIGKILL the standby: the restart resumes from its durable cursor ---
    standby.kill();
    writer.batch_execute("INSERT INTO t VALUES (5, 'five')").await.unwrap();
    let standby = Daemon::spawn(
        p.dir.path(),
        "standby-2",
        &standby_data,
        &p.token,
        &p.standby_args,
        Some(standby_config),
    );
    wait_http(p.http_s, "/ready", 200, Duration::from_secs(60), &standby).await;
    assert_eq!(
        settle(p.pg_s, &[1, 2, 3, 4, 5], Duration::from_secs(30)).await,
        Ok(vec![1, 2, 3, 4, 5])
    );
    let log = standby.log();
    assert!(log.contains("Resuming physical standby"), "the restart resumes: {log}");
    assert!(
        !log.contains("Bootstrapping physical standby"),
        "a restart never makes a second copy: {log}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_logical_standby_directory_is_refused_with_the_conversion_message() {
    let p = pair();
    let legacy = p.dir.path().join("legacy");
    std::fs::create_dir(&legacy).unwrap();
    std::fs::write(legacy.join("NANO-STANDBY-BINDING.json"), b"{}").unwrap();
    std::fs::write(legacy.join("CURRENT"), b"MANIFEST-000001\n").unwrap();
    let before = census(&legacy);

    // Default `on_unrecoverable = "stay-up"`: up, listening, needs_recovery.
    let mut standby = Daemon::spawn(p.dir.path(), "legacy-stay-up", &legacy, &p.token, &p.standby_args, None);
    let not_ready = wait_http(p.http_s, "/ready", 503, Duration::from_secs(30), &standby).await;
    assert_eq!(not_ready["state"], "needs_recovery");
    let client = connect(p.pg_s).await;
    let refused = client
        .simple_query("SELECT heliosdb_require_ready()")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    assert!(message(&refused).contains(LEGACY_REFUSAL), "{refused}");
    assert_eq!(value(&client, "SELECT pg_is_in_recovery()").await.as_deref(), Some("t"));
    assert!(standby.alive(), "a refused standby stays up by default");
    assert!(standby.log().contains(LEGACY_REFUSAL));
    drop(client);
    standby.stop();

    // `on_unrecoverable = "exit"`: the same refusal ends the process.
    let mut standby = Daemon::spawn(
        p.dir.path(),
        "legacy-exit",
        &legacy,
        &p.token,
        &p.standby_args,
        Some("[replication]\non_unrecoverable = \"exit\"\n"),
    );
    let status = standby.wait_exit(Duration::from_secs(30));
    assert!(status.is_some_and(|s| !s.success()), "must exit, got {status:?}");
    assert!(standby.log().contains(LEGACY_REFUSAL), "{}", standby.log());
    assert_eq!(census(&legacy), before, "a refused directory is never modified");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_mismatch_exits_when_on_unrecoverable_is_exit() {
    let p = pair();
    let standby_data = p.dir.path().join("standby");
    let mut primary = Daemon::spawn(
        p.dir.path(),
        "primary-a",
        &p.dir.path().join("primary-a"),
        &p.token,
        &p.primary_args,
        None,
    );
    let writer = connect(p.pg_p).await;
    writer
        .batch_execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .await
        .unwrap();
    writer.batch_execute("INSERT INTO t VALUES (1)").await.unwrap();
    let mut standby = Daemon::spawn(
        p.dir.path(),
        "standby-a",
        &standby_data,
        &p.token,
        &p.standby_args,
        None,
    );
    wait_http(p.http_s, "/ready", 200, Duration::from_secs(60), &standby).await;
    standby.stop();
    drop(writer);
    primary.stop();

    // A different dataset now answers on the same endpoints.
    let _other = Daemon::spawn(
        p.dir.path(),
        "primary-b",
        &p.dir.path().join("primary-b"),
        &p.token,
        &p.primary_args,
        None,
    );
    let _ = connect(p.pg_p).await;
    let mut standby = Daemon::spawn(
        p.dir.path(),
        "standby-exit",
        &standby_data,
        &p.token,
        &p.standby_args,
        Some("[replication]\non_unrecoverable = \"exit\"\n"),
    );
    let status = standby.wait_exit(Duration::from_secs(60));
    let log = standby.log();
    assert!(status.is_some_and(|s| !s.success()), "must exit, got {status:?}: {log}");
    assert!(log.contains("history mismatch"), "the refusal names the cause: {log}");
}
