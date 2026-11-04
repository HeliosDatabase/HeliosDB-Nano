//! Spec R3b (Sprinter bbba90ddfc61): `SWITCHOVER TO '<standby>'` run on the
//! primary — Oracle's Data Guard `SWITCHOVER`: the standby is promoted online
//! and the old primary is demoted in-process into its standby — on real
//! managed daemons (`tests/support/physical_pair.rs`).
//!
//! * `SWITCHOVER CHECK TO` answers one row per check — every check, in
//!   order, whatever failed (all passing for a streaming standby, the failing
//!   one named for an unknown one) — and changes nothing on either node: no
//!   role recorded, no fence, and on the target no promotion identity kept
//!   and its replication port free again; a session of a role that is no
//!   superuser is refused (42501);
//! * a switchover with writes running on the primary: the target serves as
//!   the primary at epoch 2 holding every write the old primary acknowledged,
//!   the old primary is its streaming standby (`pg_is_in_recovery() = t`,
//!   `in_hot_standby = on`, `/ready` 200, epoch 2), idle sessions on both
//!   nodes and the caller's are ended with FATAL 57P01, each node's role is
//!   recorded beside its data directory (bound to it); a user's files in the
//!   old primary's data directory (`heliosdb-nano.toml`, `server.key`) and an
//!   idle keep-alive HTTP connection to it block nothing (the files are kept
//!   as they were); a second switchover back restores the original roles at
//!   epoch 3, and the two stores' replicated keyspaces are equal at the same
//!   sequence;
//! * a lagging target (paused past `max_standby_lag`) and a disconnected one
//!   are refused (55000) with nothing changed — the caller's session goes on;
//! * (`internal-tests`) a target killed after it fenced the primary leaves
//!   the old primary FENCED (reads served, writes 25006, up), the
//!   switchover's error naming the commands that finish it;
//! * (`internal-tests`) a target whose answer is lost after it took the
//!   request (held past the primary's bound): the switchover is never
//!   reported "nothing was changed" — it waits for the fence and completes;
//! * the same switchover over TLS, with the WAL archive on, keeps the pair on
//!   TLS (the old primary follows the new one over TLS); a restart of BOTH
//!   nodes with no role on their command lines starts each as the role
//!   recorded beside its directory (the demoted node still over TLS), and a
//!   command line that contradicts the record is refused naming it.
//!
//! Red on the base of this change: `SWITCHOVER TO` was parsed and refused.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;
#[path = "support/tls_certs.rs"]
mod tls_certs;

use heliosdb_nano::replication::physical::range_digest;
use physical_pair::*;
use rocksdb::{Options, DB};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_postgres::SimpleQueryMessage;

/// Both nodes archive nothing (the switchover is the subject) and report a
/// standby lagging past 2 s as not streaming.
const CONFIG: &str = "[replication]\nmax_standby_lag = \"2s\"\n[replication.archive]\nenabled = false\n";

/// The same, with the WAL archive on (its default).
const CONFIG_ARCHIVING: &str = "[replication]\nmax_standby_lag = \"2s\"\n";

/// The checks `SWITCHOVER CHECK` answers first, in order; the rows after
/// them name the other standbys ("other standby", one each), or say there is
/// none ("other standbys").
const CHECKS: [&str; 8] = [
    "primary",
    "demotable",
    "rewind",
    "target",
    "streaming",
    "online promotion",
    "target preflight",
    "follow",
];

/// The rows' check names after [`CHECKS`] are all about other standbys.
fn assert_check_rows(rows: &[(String, bool, String)]) {
    let names: Vec<&str> = rows.iter().map(|(check, _, _)| check.as_str()).collect();
    assert!(rows.len() > CHECKS.len(), "{rows:?}");
    assert_eq!(names[..CHECKS.len()], CHECKS, "{rows:?}");
    assert!(
        names[CHECKS.len()..]
            .iter()
            .all(|name| *name == "other standby" || *name == "other standbys"),
        "{rows:?}"
    );
}

fn text(path: &Path) -> String {
    path.display().to_string()
}

/// The digest of the keys under `prefix` of the stopped store at `dir`.
fn digest(dir: &Path, prefix: &[u8]) -> [u8; 32] {
    let families = DB::list_cf(&Options::default(), dir).unwrap();
    let db = DB::open_cf_for_read_only(&Options::default(), dir, families, false).unwrap();
    range_digest(&db, None, prefix).unwrap()
}

/// `<data dir>.role.toml`, as a switchover wrote it.
fn role_override(dir: &Path) -> Option<toml::Value> {
    let mut path = dir.as_os_str().to_os_string();
    path.push(".role.toml");
    let text = std::fs::read_to_string(PathBuf::from(path)).ok()?;
    toml::from_str(&text).ok()
}

async fn health(port: u16) -> serde_json::Value {
    http(port, "/health").await.map(|(_, body)| body).unwrap_or_default()
}

/// The status code `GET path` answers on `port` — the status line only, so a
/// response of any framing counts (the API router's compression layer sends
/// its bodies chunked, which the pair harness's JSON helper cannot read).
async fn http_status(port: u16, path: &'static str) -> Option<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .ok()?
    .ok()?;
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .ok()?;
    let mut head = vec![0u8; 64];
    let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut head))
        .await
        .ok()?
        .ok()?;
    String::from_utf8_lossy(&head[..read])
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// An HTTP/1.1 keep-alive connection to `port` that answered one `GET
/// /health`, then sits idle (its server-side task holds the router's
/// database while it lives).
async fn http_keep_alive(port: u16) -> tokio::net::TcpStream {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\n\r\n")
        .await
        .unwrap();
    let mut answer = vec![0u8; 4096];
    let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut answer))
        .await
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&answer[..read]).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&answer[..read])
    );
    stream
}

/// The node id of the one standby `http_port`'s primary lists as streaming.
async fn streaming_standby(http_port: u16, daemon: &Daemon) -> String {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let listed = listed_standbys(http_port).await;
        if let Some(standby) = listed
            .iter()
            .find(|standby| standby["state"] == "streaming" && standby["counts_for_sync"] == true)
        {
            return standby["node_id"].as_str().unwrap().to_string();
        }
        assert!(
            Instant::now() < deadline,
            "no streaming standby listed: {listed:?}; log tail: {}",
            daemon.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The rows of a `SWITCHOVER CHECK`: (check, ok, detail).
async fn check_rows(client: &tokio_postgres::Client, sql: &str) -> Vec<(String, bool, String)> {
    let messages = client.simple_query(sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    messages
        .iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some((
                row.get(0).unwrap().to_string(),
                row.get(1).unwrap() == "t",
                row.get(2).unwrap_or_default().to_string(),
            )),
            _ => None,
        })
        .collect()
}

/// The one row a `SWITCHOVER TO` answers, by column name.
fn answer_row(messages: &[SimpleQueryMessage]) -> std::collections::HashMap<String, String> {
    let rows: Vec<_> = messages
        .iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some(
                row.columns()
                    .iter()
                    .enumerate()
                    .map(|(index, column)| {
                        (
                            column.name().to_string(),
                            row.get(index).unwrap_or_default().to_string(),
                        )
                    })
                    .collect::<std::collections::HashMap<String, String>>(),
            ),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "SWITCHOVER TO answers one row");
    rows[0].clone()
}

/// Poll fresh connections until `pg_is_in_recovery()` answers `want`.
async fn wait_recovery(port: u16, want: bool, budget: Duration, daemon: &Daemon) {
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
                let seen = messages.iter().find_map(|message| match message {
                    SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
                    _ => None,
                });
                if seen.as_deref() == Some(if want { "true" } else { "false" }) {
                    return;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "{port} never answered pg_is_in_recovery() = {want}; log tail: {}",
            daemon.tail(16)
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A client writing one row per statement from `first` on, until a write is
/// refused (the primary was fenced, or its session ended) or it is stopped.
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
                    .batch_execute(&format!("INSERT INTO t VALUES ({id}, 'during the switchover')"))
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

    async fn finish(self) -> (Vec<i32>, bool) {
        self.stop.store(true, Ordering::Release);
        self.task.await.unwrap()
    }
}

/// The pair of these tests: P the primary, S its standby with a replication
/// port of its own for when it is promoted.
struct Switchable {
    pair: Pair,
    repl_s: u16,
    p_data: PathBuf,
    s_data: PathBuf,
}

impl Switchable {
    fn new() -> Self {
        let pair = pair();
        let repl_s = free_port();
        let dir = pair.dir.path().to_path_buf();
        Self {
            pair,
            repl_s,
            p_data: dir.join("primary"),
            s_data: dir.join("standby"),
        }
    }

    fn dir(&self) -> &Path {
        self.pair.dir.path()
    }

    fn spawn_primary(&self, name: &str, env: Vec<(&'static str, String)>) -> Daemon {
        Daemon::spawn_with(
            self.dir(),
            name,
            &self.p_data,
            &self.pair.token,
            &self.pair.primary_args,
            Some(CONFIG),
            &SpawnOptions {
                env,
                ..SpawnOptions::default()
            },
        )
    }

    fn spawn_standby(&self, name: &str, env: Vec<(&'static str, String)>) -> Daemon {
        let mut args = self.pair.standby_args.clone();
        args.extend(["--replication-port".to_string(), self.repl_s.to_string()]);
        Daemon::spawn_with(
            self.dir(),
            name,
            &self.s_data,
            &self.pair.token,
            &args,
            Some(CONFIG),
            &SpawnOptions {
                env,
                ..SpawnOptions::default()
            },
        )
    }

    /// P loaded with rows `1..=rows`, S streaming them.
    async fn start(&self, rows: i32) -> (Daemon, Daemon, String) {
        self.start_with(rows, Vec::new(), Vec::new()).await
    }

    async fn start_with(
        &self,
        rows: i32,
        primary_env: Vec<(&'static str, String)>,
        standby_env: Vec<(&'static str, String)>,
    ) -> (Daemon, Daemon, String) {
        let primary = self.spawn_primary("primary", primary_env);
        let writer = connect(self.pair.pg_p).await;
        create_table(&writer).await;
        assert_eq!(insert(&writer, 1..=rows, "before").await, 0);
        let standby = self.spawn_standby("standby", standby_env);
        wait_http(self.pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
        let want: Vec<i32> = (1..=rows).collect();
        assert_eq!(settle(self.pair.pg_s, &want, Duration::from_secs(60)).await, Ok(want));
        let target = streaming_standby(self.pair.http_p, &primary).await;
        (primary, standby, target)
    }
}

/// `SWITCHOVER CHECK TO` answers one row per check and changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switchover_check_answers_one_row_per_check_and_changes_nothing() {
    let pair = Switchable::new();
    let (primary, standby, target) = pair.start(20).await;
    let p = connect(pair.pair.pg_p).await;

    let rows = check_rows(&p, &format!("SWITCHOVER CHECK TO '{target}'")).await;
    assert_check_rows(&rows);
    assert!(rows.iter().all(|(_, ok, _)| *ok), "every check passes: {rows:?}");
    assert!(rows[2].2.contains("can be rewound in place"), "{rows:?}");
    let follow = &rows[7].2;
    assert!(
        follow.contains(&format!("127.0.0.1:{}", pair.repl_s)),
        "the old primary follows the target's replication port: {follow}"
    );
    assert_eq!(rows.len(), CHECKS.len() + 1, "{rows:?}");
    assert_eq!(rows[8].0, "other standbys", "{rows:?}");
    // The `TO` is optional; an unknown standby fails at `target`, and every
    // check still has its row (the standby that exists is named as another).
    let short = check_rows(&p, &format!("SWITCHOVER CHECK '{target}'")).await;
    assert!(short.iter().all(|(_, ok, _)| *ok), "{short:?}");
    let unknown = check_rows(&p, "SWITCHOVER CHECK TO 'no-such-standby'").await;
    assert_check_rows(&unknown);
    assert!(unknown[0].1 && unknown[2].1 && !unknown[3].1, "{unknown:?}");
    assert!(
        unknown[3]
            .2
            .contains("no standby of this primary is named 'no-such-standby'"),
        "{unknown:?}"
    );
    for (index, because) in [
        (4, "no target"),
        (5, "no target"),
        (6, "a check above failed"),
        (7, "a check above failed"),
    ] {
        assert!(
            !unknown[index].1 && unknown[index].2.contains(because),
            "{index}: {unknown:?}"
        );
    }
    assert_eq!(unknown[8].0, "other standby", "{unknown:?}");
    assert!(
        unknown[8].2.contains(&target) && unknown[8].2.contains("recover --from-primary"),
        "{unknown:?}"
    );
    // Review item 5: a role that is no superuser is refused, both forms.
    let (analyst, analyst_connection) = tokio_postgres::connect(
        &format!(
            "host=127.0.0.1 port={} user=analyst dbname=postgres connect_timeout=5",
            pair.pair.pg_p
        ),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();
    tokio::spawn(analyst_connection);
    for sql in [
        format!("SWITCHOVER CHECK TO '{target}'"),
        format!("SWITCHOVER TO '{target}'"),
    ] {
        let refused = analyst.simple_query(&sql).await.unwrap_err();
        assert_eq!(sqlstate(&refused), "42501", "{sql}: {refused}");
        assert!(
            message(&refused).contains("role \"analyst\" is not a superuser")
                && message(&refused).contains("authenticates with trust"),
            "{}",
            message(&refused)
        );
    }

    // Nothing changed, on either node.
    assert_eq!(
        value(&p, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("false")
    );
    assert_eq!(insert(&p, 21..=25, "after the check").await, 0);
    assert!(!pair.p_data.join("NANO-FENCED.json").exists());
    assert!(role_override(&pair.p_data).is_none() && role_override(&pair.s_data).is_none());
    let want: Vec<i32> = (1..=25).collect();
    assert_eq!(settle(pair.pair.pg_s, &want, Duration::from_secs(60)).await, Ok(want));
    let s = connect(pair.pair.pg_s).await;
    assert_eq!(
        value(&s, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("true")
    );
    assert_eq!(health(pair.pair.http_s).await["replication"]["role"], "standby");
    // Review item 6c: nothing changed on the target either — the replication
    // port its check bound is free again, and no promotion identity was
    // kept in its store.
    std::net::TcpListener::bind(("127.0.0.1", pair.repl_s))
        .unwrap_or_else(|e| panic!("the target's check kept its replication port {}: {e}", pair.repl_s));
    let mut standby = standby;
    standby.stop();
    let families = DB::list_cf(&Options::default(), &pair.s_data).unwrap();
    let store = DB::open_cf_for_read_only(&Options::default(), &pair.s_data, families, false).unwrap();
    let local = store.cf_handle(heliosdb_nano::replication::physical::LOCAL_CF).unwrap();
    assert!(
        store
            .get_cf(local, heliosdb_nano::replication::promote::PROMOTION_NODE_KEY)
            .unwrap()
            .is_none(),
        "a check keeps no promotion identity"
    );
    drop(store);
    drop(primary);
}

/// The switchover, with writes running on the primary, and the switchover
/// back (module documentation, second bullet).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switchover_swaps_the_roles_with_every_acknowledged_write_and_a_second_one_swaps_them_back() {
    let pair = Switchable::new();
    let (mut primary, mut standby, target) = pair.start(50).await;
    let (pg_p, pg_s, http_p, http_s) = (pair.pair.pg_p, pair.pair.pg_s, pair.pair.http_p, pair.pair.http_s);

    // Idle sessions on both nodes, ended by the switchover with 57P01.
    let (idle_p, idle_p_connection) = connect_watched(pg_p).await;
    assert_eq!(value(&idle_p, "SELECT 1").await.as_deref(), Some("1"));
    let (idle_s, idle_s_connection) = connect_watched(pg_s).await;
    assert_eq!(value(&idle_s, "SELECT 1").await.as_deref(), Some("1"));
    // A user's files in the primary's data directory (the engine tolerates
    // them; the rewind keeps them), and an idle keep-alive HTTP connection
    // to its full REST surface (which holds the store until it is closed).
    std::fs::write(pair.p_data.join("heliosdb-nano.toml"), b"[storage]\n").unwrap();
    std::fs::write(pair.p_data.join("server.key"), b"user key material").unwrap();
    let keep_alive = http_keep_alive(http_p).await;
    let writer = Writer::start(pg_p, 1000).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let started = Instant::now();
    let (caller, caller_connection) = connect_watched(pg_p).await;
    let answered = caller
        .simple_query(&format!("SWITCHOVER TO '{target}'"))
        .await
        .unwrap_or_else(|e| {
            panic!(
                "SWITCHOVER failed: {e}; P: {}; S: {}",
                primary.tail(20),
                standby.tail(20)
            )
        });
    let answer = answer_row(&answered);
    println!("switchover took {:?}: {answer:?}", started.elapsed());
    assert_eq!(
        answer["new_primary"],
        format!("127.0.0.1:{}", pair.repl_s),
        "{answer:?}"
    );
    assert_eq!(answer["epoch"], "2", "{answer:?}");
    assert!(answer["this_node"].starts_with("standby"), "{answer:?}");
    assert_eq!(answer["other_standbys"], "none", "{answer:?}");
    drop(keep_alive);
    assert_eq!(
        std::fs::read(pair.p_data.join("server.key")).unwrap(),
        b"user key material",
        "the user's files are kept as they were"
    );
    assert!(pair.p_data.join("heliosdb-nano.toml").is_file());
    let (acknowledged, refused) = writer.finish().await;
    assert!(refused, "the fence ended the writes on the old primary");
    assert!(!acknowledged.is_empty(), "writes were running during the switchover");
    // The caller's session, and the idle ones, ended with FATAL 57P01.
    for (what, connection) in [
        ("the caller", caller_connection),
        ("an idle session of the old primary", idle_p_connection),
        ("an idle session of the target", idle_s_connection),
    ] {
        let ended = connection_end(connection, Duration::from_secs(30))
            .await
            .expect_err("ended by the switchover");
        assert_eq!(sqlstate(&ended), "57P01", "{what}: {ended}");
    }

    // S is the primary at epoch 2 with every acknowledged write.
    let s = connect(pg_s).await;
    assert_eq!(
        value(&s, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("false")
    );
    assert_eq!(
        value(&s, "SELECT timeline_id::bigint FROM pg_control_checkpoint()")
            .await
            .as_deref(),
        Some("2"),
        "the new primary's timeline increased"
    );
    let held: BTreeSet<i32> = ids(&s).await.unwrap().into_iter().collect();
    for id in (1..=50).chain(acknowledged.iter().copied()) {
        assert!(held.contains(&id), "the new primary lacks the acknowledged write {id}");
    }
    assert_eq!(health(http_s).await["replication"]["role"], "primary");
    // P is its streaming standby.
    let ready = wait_http(http_p, "/ready", 200, Duration::from_secs(120), &primary).await;
    assert_eq!(ready["state"], "streaming", "{ready}");
    let p = connect(pg_p).await;
    assert_eq!(
        value(&p, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("true")
    );
    assert_eq!(value(&p, "SHOW in_hot_standby").await.as_deref(), Some("on"));
    let block = health(http_p).await["replication"].clone();
    assert_eq!(
        (block["role"].clone(), block["epoch"].clone()),
        ("standby".into(), 2.into()),
        "{block}"
    );
    assert!(primary.log().contains("rewound"), "{}", primary.tail(20));
    // Each node's role is recorded beside its directory.
    let p_role = role_override(&pair.p_data).expect("the old primary's role");
    assert_eq!(p_role["replication"]["role"].as_str(), Some("standby"));
    assert_eq!(
        p_role["replication"]["primary_host"].as_str(),
        Some(format!("127.0.0.1:{}", pair.repl_s).as_str())
    );
    assert_eq!(p_role["directory"]["epoch"].as_integer(), Some(2), "{p_role}");
    let s_role = role_override(&pair.s_data).expect("the new primary's role");
    assert_eq!(s_role["replication"]["role"].as_str(), Some("primary"));
    assert_eq!(s_role["directory"]["epoch"].as_integer(), Some(2), "{s_role}");
    assert_eq!(
        s_role["directory"]["history"], p_role["directory"]["history"],
        "one dataset"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut path = pair.p_data.as_os_str().to_os_string();
        path.push(".role.toml");
        let mode = std::fs::metadata(PathBuf::from(path)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the role record is the owner's only");
    }
    // New writes on S reach P.
    assert_eq!(insert(&s, 5000..=5010, "epoch-2").await, 0);
    let mut want: Vec<i32> = ids(&s).await.unwrap();
    assert_eq!(settle(pg_p, &want, Duration::from_secs(60)).await, Ok(want.clone()));
    drop((p, s, idle_p, idle_s, caller));

    // The switchover back restores the original roles, at epoch 3.
    let target_p = streaming_standby(http_s, &standby).await;
    let (back, back_connection) = connect_watched(pg_s).await;
    let answered = back
        .simple_query(&format!("SWITCHOVER TO '{target_p}'"))
        .await
        .unwrap_or_else(|e| {
            panic!(
                "SWITCHOVER back failed: {e}; P: {}; S: {}",
                primary.tail(20),
                standby.tail(20)
            )
        });
    let answer = answer_row(&answered);
    assert_eq!(
        answer["new_primary"],
        format!("127.0.0.1:{}", pair.pair.repl_p),
        "{answer:?}"
    );
    assert_eq!(answer["epoch"], "3", "{answer:?}");
    let ended = connection_end(back_connection, Duration::from_secs(30))
        .await
        .expect_err("ended by the switchover");
    assert_eq!(sqlstate(&ended), "57P01", "{ended}");
    wait_recovery(pg_p, false, Duration::from_secs(60), &primary).await;
    let p = connect(pg_p).await;
    assert_eq!(
        value(&p, "SELECT timeline_id::bigint FROM pg_control_checkpoint()")
            .await
            .as_deref(),
        Some("3")
    );
    assert_eq!(insert(&p, 6000..=6010, "epoch-3").await, 0);
    want = ids(&p).await.unwrap();
    let ready = wait_http(http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    assert_eq!(ready["state"], "streaming", "{ready}");
    assert_eq!(settle(pg_s, &want, Duration::from_secs(60)).await, Ok(want));
    assert_eq!(
        role_override(&pair.p_data).unwrap()["replication"]["role"].as_str(),
        Some("primary")
    );
    assert_eq!(
        role_override(&pair.s_data).unwrap()["replication"]["role"].as_str(),
        Some("standby")
    );
    drop(p);
    // The replicated keyspaces are equal at the same sequence: the standby
    // stopped first, holding everything the primary wrote.
    standby.stop();
    primary.stop();
    for prefix in [&b"data:"[..], &b"meta:table:"[..]] {
        assert_eq!(digest(&pair.p_data, prefix), digest(&pair.s_data, prefix));
    }
}

/// A lagging target and a disconnected one are refused with nothing changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lagging_or_disconnected_target_is_refused_with_nothing_changed() {
    let pair = Switchable::new();
    let (primary, mut standby, target) = pair.start(10).await;
    let p = connect(pair.pair.pg_p).await;
    let unchanged = |what: &'static str| {
        let data = pair.p_data.clone();
        async move {
            assert!(!data.join("NANO-FENCED.json").exists(), "{what}: not fenced");
            assert!(role_override(&data).is_none(), "{what}: no role recorded");
        }
    };

    // Lagging: paused past max_standby_lag (2 s) while the primary writes.
    standby.pause();
    assert_eq!(insert(&p, 11..=20, "while paused").await, 0);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let refused = p.simple_query(&format!("SWITCHOVER TO '{target}'")).await.unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    assert!(
        message(&refused).contains("SWITCHOVER refused (streaming)")
            && message(&refused).contains("nothing was changed"),
        "{}",
        message(&refused)
    );
    unchanged("lagging").await;
    // The caller's session goes on, on a writable primary.
    assert_eq!(
        value(&p, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("false")
    );
    assert_eq!(insert(&p, 21..=25, "after the refusal").await, 0);
    standby.resume();
    let want: Vec<i32> = (1..=25).collect();
    assert_eq!(settle(pair.pair.pg_s, &want, Duration::from_secs(60)).await, Ok(want));

    // Disconnected: the standby is stopped.
    standby.stop();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !listed_standbys(pair.pair.http_p).await.is_empty() {
        assert!(Instant::now() < deadline, "the stopped standby is still listed");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let refused = p.simple_query(&format!("SWITCHOVER TO '{target}'")).await.unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    assert!(
        message(&refused).contains("SWITCHOVER refused (target)")
            && message(&refused).contains("no standby of this primary is named"),
        "{}",
        message(&refused)
    );
    unchanged("disconnected").await;
    assert_eq!(insert(&p, 26..=30, "after the second refusal").await, 0);
    drop(primary);
}

/// (`internal-tests`) A target killed after it fenced the primary: the old
/// primary stays FENCED and up, and the switchover's error names the
/// commands that finish it.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_target_killed_after_it_fenced_the_primary_leaves_it_fenced_naming_what_to_run() {
    use heliosdb_nano::replication::physical_receiver::RECEIVER_HOLD_DIR_ENV;
    use heliosdb_nano::replication::promote::switchover::SERVE_SECS_ENV;
    let pair = Switchable::new();
    // The target's receiver holds right after the fence was answered (its
    // catch-up to the final sequence), and is killed there.
    let hold = pair.dir().join("hold");
    std::fs::create_dir_all(&hold).unwrap();
    std::fs::write(hold.join("after-fenced"), b"hold").unwrap();
    let (primary, mut standby, target) = pair
        .start_with(
            10,
            vec![(SERVE_SECS_ENV, "10".to_string())],
            vec![(RECEIVER_HOLD_DIR_ENV, text(&hold))],
        )
        .await;
    let (caller, caller_connection) = connect_watched(pair.pair.pg_p).await;
    let sql = format!("SWITCHOVER TO '{target}'");
    let switching = tokio::spawn(async move {
        let outcome = caller.simple_query(&sql).await;
        (caller, outcome)
    });
    let fenced = pair.p_data.join("NANO-FENCED.json");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !fenced.exists() {
        assert!(
            Instant::now() < deadline,
            "the target never fenced the primary; P: {}; S: {}",
            primary.tail(16),
            standby.tail(16)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    standby.kill();
    let (_caller, outcome) = switching.await.unwrap();
    let failed = outcome.expect_err("the switchover cannot finish");
    assert_eq!(sqlstate(&failed), "55000", "{failed}");
    let said = message(&failed);
    assert!(
        said.contains("FENCED")
            && said.contains(&format!(
                "recover --from-primary --data-dir {} --primary-host 127.0.0.1:{}",
                text(&pair.p_data),
                pair.repl_s
            ))
            && said.contains(&format!("start --data-dir {}", text(&pair.p_data))),
        "{said}"
    );
    let ended = connection_end(caller_connection, Duration::from_secs(30))
        .await
        .expect_err("the caller's session ends after the error");
    assert_eq!(sqlstate(&ended), "57P01", "{ended}");
    // The old primary is up, fenced: reads served, writes refused.
    let p = connect(pair.pair.pg_p).await;
    assert_eq!(
        ids(&p).await,
        Ok((1..=10).collect::<Vec<_>>()),
        "a fenced primary serves reads"
    );
    let refused = p
        .batch_execute("INSERT INTO t VALUES (99, 'after the fence')")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&refused), "25006", "{refused}");
    assert_eq!(health(pair.pair.http_p).await["replication"]["state"], "fenced");
    assert!(role_override(&pair.p_data).is_none(), "the demotion never ran");
    drop(primary);
}

/// (`internal-tests`) Review item 3: the target took the request and its
/// answer is lost — held past this primary's bound — while it already
/// promotes itself: the switchover is never reported "nothing was changed";
/// it waits for this primary's fence and completes.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_target_whose_answer_is_lost_after_it_took_the_request_still_completes_the_switchover() {
    use heliosdb_nano::replication::physical_wire::PROMOTE_ANSWER_DELAY_ENV;
    use heliosdb_nano::replication::promote::switchover::ANSWER_SECS_ENV;
    let pair = Switchable::new();
    let (primary, standby, target) = pair
        .start_with(
            10,
            vec![(ANSWER_SECS_ENV, "3".to_string())],
            vec![(PROMOTE_ANSWER_DELAY_ENV, "12".to_string())],
        )
        .await;
    let caller = connect(pair.pair.pg_p).await;
    let answered = caller
        .simple_query(&format!("SWITCHOVER TO '{target}'"))
        .await
        .unwrap_or_else(|e| {
            panic!(
                "a lost answer must not end the switchover: {e}; P: {}; S: {}",
                primary.tail(20),
                standby.tail(20)
            )
        });
    let answer = answer_row(&answered);
    assert_eq!(answer["epoch"], "2", "{answer:?}");
    assert!(
        primary.log_contains_within("its answer was lost", Duration::from_secs(10)),
        "{}",
        primary.tail(30)
    );
    // The caller was answered by the completed switchover, not refused
    // with "nothing was changed".
    assert!(answer["this_node"].starts_with("standby"), "{answer:?}");
    wait_recovery(pair.pair.pg_s, false, Duration::from_secs(60), &standby).await;
    let ready = wait_http(pair.pair.http_p, "/ready", 200, Duration::from_secs(120), &primary).await;
    assert_eq!(ready["state"], "streaming", "{ready}");
    drop(standby);
    drop(primary);
}

/// `heliosdb-nano <args>` run to completion (a `recover`, a `stop`).
fn cli(args: &[String]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .args(args)
        .output()
        .unwrap()
}

/// Stops the `--daemon` child named by its PID file when the test ends.
struct StopDaemon(PathBuf);

impl Drop for StopDaemon {
    fn drop(&mut self) {
        if self.0.exists() {
            let _ = cli(&["stop".to_string(), "--pid-file".to_string(), text(&self.0)]);
        }
    }
}

/// Review (third pass): a `--daemon` restart after a switchover, with no
/// role on its command line — the parent applies the record beside the data
/// directory and forwards the role (and primary) it resolved to its child,
/// which agrees with the record: the demoted node comes back as the new
/// primary's streaming standby.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_restart_after_a_switchover_forwards_the_recorded_role_to_its_child() {
    let pair = Switchable::new();
    let (mut primary, standby, target) = pair.start(10).await;
    let caller = connect(pair.pair.pg_p).await;
    caller
        .simple_query(&format!("SWITCHOVER TO '{target}'"))
        .await
        .unwrap_or_else(|e| {
            panic!(
                "SWITCHOVER failed: {e}; P: {}; S: {}",
                primary.tail(20),
                standby.tail(20)
            )
        });
    wait_http(pair.pair.http_p, "/ready", 200, Duration::from_secs(120), &primary).await;
    primary.stop();
    let pid_file = pair.dir().join("primary-daemon.pid");
    let _stop = StopDaemon(pid_file.clone());
    let args = vec![
        "--port".to_string(),
        pair.pair.pg_p.to_string(),
        "--http-port".into(),
        pair.pair.http_p.to_string(),
        "--replication-startup-timeout-secs".into(),
        "30".into(),
        "--daemon".into(),
        "--pid-file".into(),
        text(&pid_file),
    ];
    let mut parent = Daemon::spawn(
        pair.dir(),
        "primary-daemon",
        &pair.p_data,
        &pair.pair.token,
        &args,
        Some(CONFIG),
    );
    let exit = parent.wait_exit(Duration::from_secs(120));
    assert!(
        exit.is_some_and(|status| status.success()),
        "the daemon parent: {exit:?}; {}",
        parent.log()
    );
    assert!(pid_file.exists(), "{}", parent.log());
    let ready = wait_http(pair.pair.http_p, "/ready", 200, Duration::from_secs(120), &parent).await;
    assert_eq!(ready["state"], "streaming", "{ready}");
    let block = health(pair.pair.http_p).await["replication"].clone();
    assert_eq!(
        (block["role"].clone(), block["epoch"].clone()),
        ("standby".into(), 2.into()),
        "{block}"
    );
    let s = connect(pair.pair.pg_s).await;
    assert_eq!(insert(&s, 11..=15, "after the daemon restart").await, 0);
    let want: Vec<i32> = (1..=15).collect();
    assert_eq!(settle(pair.pair.pg_p, &want, Duration::from_secs(60)).await, Ok(want));
    drop(standby);
}

/// (`internal-tests`) Review (third pass): the in-process rewind stops after
/// the store was released and before its conversion changed anything — the
/// old primary's store is reopened FENCED and served again (reads, its full
/// HTTP surface), nothing of its directory changed (RocksDB's WAL archive
/// kept), and the command the error names finishes the switchover.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rewind_that_changed_nothing_reopens_the_old_primary_fenced_and_the_named_command_finishes_it() {
    use heliosdb_nano::replication::recover_primary::FAIL_REWIND_ENV;
    let pair = Switchable::new();
    let (mut primary, standby, target) = pair
        .start_with(10, vec![(FAIL_REWIND_ENV, "before-convert".to_string())], Vec::new())
        .await;
    let archived = pair.p_data.join("archive").is_dir();
    // The full REST surface answers `/openapi.json`; a health-only one (a
    // standby's, or this node's while it is being demoted) answers 404.
    assert_eq!(
        http_status(pair.pair.http_p, "/openapi.json").await,
        Some(200),
        "the primary serves its full surface"
    );
    let caller = connect(pair.pair.pg_p).await;
    let failed = caller
        .simple_query(&format!("SWITCHOVER TO '{target}'"))
        .await
        .expect_err("the rewind stops");
    assert_eq!(sqlstate(&failed), "55000", "{failed}");
    let said = message(&failed);
    assert!(
        said.contains("stopped before it changed the directory")
            && said.contains("FENCED primary again")
            && said.contains(&format!(
                "recover --from-primary --data-dir {} --primary-host 127.0.0.1:{}",
                text(&pair.p_data),
                pair.repl_s
            )),
        "{said}"
    );
    // Fenced, up, serving reads — and its full HTTP surface again.
    let p = connect(pair.pair.pg_p).await;
    assert_eq!(ids(&p).await, Ok((1..=10).collect::<Vec<_>>()));
    let refused = p
        .batch_execute("INSERT INTO t VALUES (99, 'after the stopped rewind')")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&refused), "25006", "{refused}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while http_status(pair.pair.http_p, "/openapi.json").await != Some(200) {
        assert!(
            Instant::now() < deadline,
            "the REST surface never came back; {}",
            primary.tail(20)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Nothing of the directory changed.
    assert!(pair.p_data.join("NANO-FENCED.json").exists());
    assert!(pair.p_data.join("NANO-PRIMARY-HISTORY.json").exists());
    assert!(!pair.p_data.join("NANO-PHYSICAL-REPLICA.json").exists());
    if archived {
        assert!(pair.p_data.join("archive").is_dir(), "RocksDB's WAL archive is kept");
    }
    assert!(
        role_override(&pair.p_data).is_none(),
        "the demotion did not get that far"
    );
    wait_recovery(pair.pair.pg_s, false, Duration::from_secs(60), &standby).await;
    drop((p, caller));

    // The command the error names finishes it, as named — whatever the plan
    // picks. This store is small (a few KiB, every leaf differs), so the plan
    // picks a full copy: `--replace` (named) swaps it in and moves the old
    // directory aside, never deleting it. The rewind proper on a store that
    // stays under the threshold is `physical_rewind.rs` and scenario 3 of
    // `physical_promote.rs`.
    let named = said
        .split("`heliosdb-nano recover ")
        .nth(1)
        .and_then(|rest| rest.split('`').next())
        .unwrap_or_else(|| panic!("no recover command named: {said}"));
    let mut args = vec!["recover".to_string()];
    args.extend(named.split_whitespace().map(str::to_string));
    assert!(args.iter().any(|arg| arg == "--replace"), "{args:?}");
    // It exits when its work is done (no server in the operator's shell), and
    // the error names the `start` that follows it.
    assert!(args.windows(2).any(|pair| pair == ["--then", "stop"]), "{args:?}");
    assert!(
        said.contains(&format!(
            "start --data-dir {} --replication-role standby --primary-host 127.0.0.1:{}",
            text(&pair.p_data),
            pair.repl_s
        )),
        "{said}"
    );
    primary.stop();
    let output = cli(&args);
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{args:?}: {out}");
    assert!(out.contains("a full copy of the primary"), "{out}");
    let replaced: Vec<_> = std::fs::read_dir(pair.dir())
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("primary.replaced-"))
        .collect();
    assert_eq!(
        replaced.len(),
        1,
        "the old directory is moved aside, never deleted: {out}"
    );
    assert!(replaced[0].path().join("NANO-FENCED.json").exists());
    let p_args = standby_args(pair.pair.pg_p, pair.repl_s, pair.pair.http_p);
    let p_standby = Daemon::spawn(
        pair.dir(),
        "primary-follows-s",
        &pair.p_data,
        &pair.pair.token,
        &p_args,
        Some(CONFIG),
    );
    let ready = wait_http(pair.pair.http_p, "/ready", 200, Duration::from_secs(120), &p_standby).await;
    assert_eq!(ready["state"], "streaming", "{ready}");
    drop(p_standby);
    drop(standby);
}

/// The switchover over TLS: the pair stays on TLS (module documentation,
/// last bullet). The two nodes run on the loopback aliases `127.0.0.2` (P)
/// and `127.0.0.3` (S), each with a server certificate naming both and the
/// CA to verify the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switchover_over_tls_keeps_the_pair_on_tls() {
    const P_HOST: &str = "127.0.0.2";
    const S_HOST: &str = "127.0.0.3";
    if !tls_certs::openssl_available() {
        eprintln!("skipped: the openssl CLI is not available");
        return;
    }
    let pair = Switchable::new();
    let dir = pair.dir().to_path_buf();
    let ca = tls_certs::ca(&dir, "switchover");
    let server = tls_certs::leaf(
        &dir,
        &ca,
        "switchover-server",
        &[&format!("IP:{P_HOST}"), &format!("IP:{S_HOST}"), "DNS:localhost"],
        "serverAuth",
    );
    let tls_args = |extra: &[String]| {
        let mut args = vec![
            "--replication-tls".to_string(),
            "--replication-tls-cert".into(),
            text(&server.0),
            "--replication-tls-key".into(),
            text(&server.1),
            "--primary-tls".into(),
            "--primary-ca".into(),
            text(&ca.0),
        ];
        args.extend(extra.iter().cloned());
        args
    };
    let mut p_args = pair.pair.primary_args.clone();
    p_args.extend(tls_args(&[]));
    let primary = Daemon::spawn_with(
        &dir,
        "primary",
        &pair.p_data,
        &pair.pair.token,
        &p_args,
        Some(CONFIG_ARCHIVING),
        &SpawnOptions {
            listen: Some(P_HOST),
            ..SpawnOptions::default()
        },
    );
    let writer = connect_to(P_HOST, pair.pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=10, "before").await, 0);
    let s_args = tls_args(&[
        "--port".into(),
        pair.pair.pg_s.to_string(),
        "--replication-role".into(),
        "standby".into(),
        "--primary-host".into(),
        format!("{P_HOST}:{}", pair.pair.repl_p),
        "--http-port".into(),
        pair.pair.http_s.to_string(),
        "--replication-port".into(),
        pair.repl_s.to_string(),
        "--replication-startup-timeout-secs".into(),
        "30".into(),
    ]);
    let standby = Daemon::spawn_with(
        &dir,
        "standby",
        &pair.s_data,
        &pair.pair.token,
        &s_args,
        Some(CONFIG_ARCHIVING),
        &SpawnOptions {
            listen: Some(S_HOST),
            ..SpawnOptions::default()
        },
    );
    wait_http(pair.pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    assert_eq!(health(pair.pair.http_s).await["replication"]["tls"], true);
    let target = streaming_standby(pair.pair.http_p, &primary).await;
    let caller = connect_to(P_HOST, pair.pair.pg_p).await;
    let rows = check_rows(&caller, &format!("SWITCHOVER CHECK TO '{target}'")).await;
    assert!(rows.iter().all(|(_, ok, _)| *ok), "{rows:?}");
    assert!(rows[7].2.contains("over TLS"), "{rows:?}");
    let answered = caller
        .simple_query(&format!("SWITCHOVER TO '{target}'"))
        .await
        .unwrap_or_else(|e| {
            panic!(
                "SWITCHOVER failed: {e}; P: {}; S: {}",
                primary.tail(20),
                standby.tail(20)
            )
        });
    let answer = answer_row(&answered);
    assert_eq!(answer["new_primary"], format!("{S_HOST}:{}", pair.repl_s), "{answer:?}");
    let ready = wait_http(pair.pair.http_p, "/ready", 200, Duration::from_secs(120), &primary).await;
    assert_eq!(ready["state"], "streaming", "{ready}");
    let block = health(pair.pair.http_p).await["replication"].clone();
    assert_eq!(block["role"], "standby", "{block}");
    assert_eq!(
        block["tls"], true,
        "the old primary follows the new one over TLS: {block}"
    );
    let s = connect_to(S_HOST, pair.pair.pg_s).await;
    assert_eq!(insert(&s, 11..=15, "epoch-2").await, 0);
    let want: Vec<i32> = (1..=15).collect();
    assert_eq!(
        settle_on(P_HOST, pair.pair.pg_p, &want, Duration::from_secs(60)).await,
        Ok(want)
    );
    let p_role = role_override(&pair.p_data).expect("the demoted node's role");
    assert_eq!(p_role["replication"]["primary_tls"].as_bool(), Some(true), "{p_role}");
    assert_eq!(
        p_role["replication"]["primary_tls_ca_path"].as_str(),
        Some(text(&ca.0).as_str()),
        "{p_role}"
    );
    drop(s);

    // Review item 2: a restart of BOTH nodes. A command line that
    // contradicts a node's record is refused, naming the record.
    let (mut standby, mut primary) = (standby, primary);
    standby.stop();
    primary.stop();
    let mut contradicting = Daemon::spawn_with(
        &dir,
        "primary-as-primary-again",
        &pair.p_data,
        &pair.pair.token,
        &p_args,
        Some(CONFIG_ARCHIVING),
        &SpawnOptions {
            listen: Some(P_HOST),
            ..SpawnOptions::default()
        },
    );
    let exit = contradicting.wait_exit(Duration::from_secs(60));
    assert_eq!(
        exit.and_then(|status| status.code()),
        Some(2),
        "{}",
        contradicting.log()
    );
    assert!(
        contradicting.log().contains("--replication-role primary contradicts")
            && contradicting.log().contains("primary.role.toml"),
        "{}",
        contradicting.log()
    );
    // With no role on their command lines, each starts as its record says:
    // S the primary (its serving keys on its command line), P its standby —
    // over TLS, with the CA its record names (none on its command line).
    let s_restart = vec![
        "--port".to_string(),
        pair.pair.pg_s.to_string(),
        "--replication-port".into(),
        pair.repl_s.to_string(),
        "--http-port".into(),
        pair.pair.http_s.to_string(),
        "--replication-tls".into(),
        "--replication-tls-cert".into(),
        text(&server.0),
        "--replication-tls-key".into(),
        text(&server.1),
    ];
    let s_primary = Daemon::spawn_with(
        &dir,
        "standby-restarted-as-primary",
        &pair.s_data,
        &pair.pair.token,
        &s_restart,
        Some(CONFIG_ARCHIVING),
        &SpawnOptions {
            listen: Some(S_HOST),
            ..SpawnOptions::default()
        },
    );
    let p_restart = vec![
        "--port".to_string(),
        pair.pair.pg_p.to_string(),
        "--http-port".into(),
        pair.pair.http_p.to_string(),
        "--replication-startup-timeout-secs".into(),
        "30".into(),
    ];
    let p_standby = Daemon::spawn_with(
        &dir,
        "primary-restarted-as-standby",
        &pair.p_data,
        &pair.pair.token,
        &p_restart,
        Some(CONFIG_ARCHIVING),
        &SpawnOptions {
            listen: Some(P_HOST),
            ..SpawnOptions::default()
        },
    );
    let s = connect_to(S_HOST, pair.pair.pg_s).await;
    assert_eq!(
        value(&s, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("false"),
        "{}",
        s_primary.tail(16)
    );
    let ready = wait_http(pair.pair.http_p, "/ready", 200, Duration::from_secs(120), &p_standby).await;
    assert_eq!(ready["state"], "streaming", "{ready}");
    let block = health(pair.pair.http_p).await["replication"].clone();
    assert_eq!(
        (block["role"].clone(), block["tls"].clone()),
        ("standby".into(), true.into()),
        "{block}"
    );
    assert!(p_standby.log().contains("primary.role.toml"), "{}", p_standby.tail(20));
    assert_eq!(insert(&s, 16..=20, "after both restarts").await, 0);
    let want: Vec<i32> = (1..=20).collect();
    assert_eq!(
        settle_on(P_HOST, pair.pair.pg_p, &want, Duration::from_secs(60)).await,
        Ok(want)
    );
    drop(p_standby);
    drop(s_primary);
}
