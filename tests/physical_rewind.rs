//! Spec R3a (Sprinter 1c8963417a35): `heliosdb-nano recover --from-primary`
//! REWINDS a former primary into a standby of the primary promoted over it,
//! in place, by fast-forward — Oracle's `REINSTATE DATABASE`, PostgreSQL's
//! `pg_rewind` — on real managed daemons (`tests/support/physical_pair.rs`).
//!
//! * a forced promotion (`promote --no-fence`: the old primary P keeps
//!   running and takes writes after the fork, so the two DIVERGE), then the
//!   rewind of P against the new primary S: the route is the rewind (no full
//!   copy, nothing replaced beside it), the bytes shipped are a small share
//!   of the dataset, P's replicated keyspace is S's export at the cut byte for
//!   byte, its cursor is `{T, T}`, its stage reopens, its primary-only files
//!   are gone, and P starts as a STREAMING standby of S at epoch 2 — the rows
//!   P took after the fork gone, S's rows there, S's new writes arriving;
//! * on the way, the refusals that change nothing: a directory at a HIGHER
//!   epoch than its source (the source is the superseded one — and is not
//!   fenced by the refused attempt), a source at the directory's OWN epoch
//!   (a twin of P: not promoted over it), each leaving the directory as it
//!   was;
//! * (`internal-tests`) the rewind killed right after its conversion's store
//!   write — every `start` refuses the directory naming the command — and
//!   killed again during the leaf apply — a standby `start` stays up in
//!   `needs_recovery` naming the interrupted fast-forward — and a re-run
//!   finishes it to the same end state;
//! * a primary of ANOTHER history is refused and left as it is.
//!
//! The clean case (a fenced primary, no divergence) is
//! `tests/physical_promote.rs` scenario 3.
//!
//! Red on the base of this change: a primary's directory was rebuilt only
//! by a full copy beside it (`--replace`), never rewound in place.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use heliosdb_nano::replication::checkpoint_transfer::{physical_stage_history, ReopenedPhysicalStage, TransferLimits};
use heliosdb_nano::replication::epoch::{epoch_of, read_directory_epoch};
use heliosdb_nano::replication::physical::{decode_cursor, range_digest, CURSOR_KEY, LOCAL_CF, REWOUND_KEY};
use heliosdb_nano::Config;
use physical_pair::*;
use rocksdb::{Options, DB};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Small leaves, so a test-sized dataset has dozens of them.
const LEAF: &str = "64KiB";

fn cli(args: &[String], env: &[(String, String)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
    command.args(args).env("RUST_LOG", "warn");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

async fn cli_async(args: Vec<String>, env: Vec<(String, String)>) -> Output {
    tokio::task::spawn_blocking(move || cli(&args, &env)).await.unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn text(path: &Path) -> String {
    path.display().to_string()
}

/// `recover --from-primary` of `data` from the primary at `port`.
fn recover_args(data: &Path, port: u16, token: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        "recover".to_string(),
        "--data-dir".to_string(),
        text(data),
        "--from-primary".to_string(),
        "--primary-host".to_string(),
        format!("127.0.0.1:{port}"),
        "--replication-token-file".to_string(),
        text(token),
        "--leaf-bytes".to_string(),
        LEAF.to_string(),
        "--then".to_string(),
        "stop".to_string(),
    ];
    args.extend(extra.iter().map(|arg| arg.to_string()));
    args
}

/// Insert `ids` in multi-row statements of 500.
async fn bulk_insert(client: &tokio_postgres::Client, ids: std::ops::RangeInclusive<i32>, filler: &str) {
    let ids: Vec<i32> = ids.collect();
    for chunk in ids.chunks(500) {
        let values: Vec<String> = chunk.iter().map(|id| format!("({id}, '{filler}')")).collect();
        client
            .batch_execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
            .await
            .unwrap();
    }
}

fn families(dir: &Path) -> Vec<String> {
    DB::list_cf(&Options::default(), dir).unwrap()
}

fn store_epoch(dir: &Path) -> u64 {
    epoch_of(read_directory_epoch(dir).unwrap().as_ref())
}

/// A stopped standby's cursor: (snapshot sequence, source sequence).
fn cursor_of(path: &Path) -> (u64, u64) {
    let db = DB::open_cf_for_read_only(&Options::default(), path, ["default", LOCAL_CF], false).unwrap();
    let bytes = db.get_cf(db.cf_handle(LOCAL_CF).unwrap(), CURSOR_KEY).unwrap().unwrap();
    let cursor = decode_cursor(&bytes).unwrap();
    (cursor.snapshot_sequence(), cursor.source_sequence())
}

/// What the re-cut kept of the rewind (`REWOUND_KEY`), as JSON.
fn rewound(path: &Path) -> serde_json::Value {
    let db = DB::open_cf_for_read_only(&Options::default(), path, ["default", LOCAL_CF], false).unwrap();
    let bytes = db
        .get_cf(db.cf_handle(LOCAL_CF).unwrap(), REWOUND_KEY)
        .unwrap()
        .expect("the rewind's record is kept for diagnosis");
    serde_json::from_slice(&bytes).unwrap()
}

/// The default column family's digest of the store at `path`, read-only.
fn digest_of(path: &Path) -> [u8; 32] {
    let db = DB::open_for_read_only(&Options::default(), path, false).unwrap();
    range_digest(&db, None, b"").unwrap()
}

/// The digest of the export checkpoint of `source_data` at `sequence`: the
/// cut a fast-forward served (its lease keeps it after the session).
fn export_digest_at(source_data: &Path, sequence: u64) -> [u8; 32] {
    let mut exports = source_data.as_os_str().to_os_string();
    exports.push(".physical-export");
    let exports = PathBuf::from(exports);
    let mut seen = Vec::new();
    for entry in std::fs::read_dir(&exports).unwrap() {
        let checkpoint = entry.unwrap().path().join("checkpoint");
        if !checkpoint.is_dir() {
            continue;
        }
        let db = DB::open_for_read_only(&Options::default(), &checkpoint, false).unwrap();
        seen.push(db.latest_sequence_number());
        if db.latest_sequence_number() == sequence {
            return range_digest(&db, None, b"").unwrap();
        }
    }
    panic!("no export of {} at {sequence}: {seen:?}", source_data.display());
}

/// The percentage of the primary's bytes the report says differ.
fn differing_percent(report: &str) -> f64 {
    let line = report
        .lines()
        .find(|line| line.contains(" differ ("))
        .unwrap_or_else(|| panic!("no digest line: {report}"));
    line.split(" differ (")
        .nth(1)
        .and_then(|rest| rest.split('%').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("no percentage: {line}"))
}

/// The directory reopens as the consistent stage every `start` proves.
fn stage_reopens(path: &Path) {
    let history = physical_stage_history(path).unwrap();
    let stage = ReopenedPhysicalStage::open(path, history, TransferLimits::default(), &Config::default()).unwrap();
    stage.close().unwrap();
}

/// Copy the STOPPED store `from` to `to`, file by file.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &target);
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// The dataset history a primary's directory names.
fn primary_history(dir: &Path) -> String {
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("NANO-PRIMARY-HISTORY.json")).unwrap()).unwrap();
    record["history_id"].as_str().unwrap().to_string()
}

async fn health(port: u16) -> serde_json::Value {
    http(port, "/health").await.map(|(_, body)| body).unwrap_or_default()
}

/// Poll `/ready` until its body satisfies `want`.
#[cfg_attr(not(feature = "internal-tests"), allow(dead_code))]
async fn wait_ready_body(
    port: u16,
    budget: Duration,
    daemon: &Daemon,
    want: impl Fn(u16, &serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + budget;
    loop {
        let seen = http(port, "/ready").await;
        if let Some((code, body)) = &seen {
            if want(*code, body) {
                return body.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "/ready never answered as wanted; last {seen:?}; log tail: {}",
            daemon.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A daemon refused at start: its exit code and its log.
#[cfg_attr(not(feature = "internal-tests"), allow(dead_code))]
fn refused_start(dir: &Path, name: &str, data: &Path, token: &Path, args: &[String]) -> (Option<i32>, String) {
    let mut daemon = Daemon::spawn(dir, name, data, token, args, None);
    let status = daemon.wait_exit(Duration::from_secs(60));
    (status.and_then(|status| status.code()), daemon.log())
}

/// The forced promotion and the rewind of the diverged old primary (module
/// documentation, first three bullets).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_diverged_superseded_primary_is_rewound_in_place_into_a_standby_of_the_new_primary() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let token = pair.token.clone();
    let p_data = dir.join("primary");
    let s_data = dir.join("standby");
    let (repl_s, pg_twin, repl_twin, http_twin) = (free_port(), free_port(), free_port(), free_port());

    // P with a dataset of a hundred-odd 64 KiB leaves; S follows it, stops.
    let mut primary = Daemon::spawn(&dir, "primary", &p_data, &token, &pair.primary_args, None);
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    bulk_insert(&writer, 1..=20_000, &"b".repeat(200)).await;
    let mut standby = Daemon::spawn(&dir, "standby", &s_data, &token, &pair.standby_args, None);
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    let through = primary_sequence(&writer).await;
    wait_listed(pair.http_p, through, Duration::from_secs(90), &primary).await;
    standby.stop();
    let history = primary_history(&p_data);

    // A forced promotion that never contacts P: P is NOT fenced and keeps
    // taking writes — the two diverge from here.
    let output = cli_async(
        vec![
            "promote".to_string(),
            "--data-dir".to_string(),
            text(&s_data),
            "--no-fence".to_string(),
        ],
        Vec::new(),
    )
    .await;
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(store_epoch(&s_data), 2);
    // A copy of the promoted S, stopped: a primary's directory at epoch 2.
    let s_copy = dir.join("standby-copy");
    copy_dir(&s_data, &s_copy);
    assert_eq!(insert(&writer, 20_001..=20_050, "diverged-on-p").await, 0);

    // S serves as the primary of epoch 2 and takes writes of its own.
    let s_args = primary_args(pair.pg_s, repl_s, pair.http_s);
    let s_primary = Daemon::spawn(&dir, "s-primary", &s_data, &token, &s_args, None);
    let s = connect(pair.pg_s).await;
    assert_eq!(insert(&s, 30_001..=30_050, "epoch-2").await, 0);

    // --- A directory at a HIGHER epoch than its source is refused --------
    let copy_before = census(&s_copy);
    let output = cli_async(recover_args(&s_copy, pair.repl_p, &token, &[]), Vec::new()).await;
    assert_eq!(output.status.code(), Some(1), "{}", said(&output));
    assert!(
        said(&output).contains("at epoch 2") && said(&output).contains("the source is the superseded one"),
        "{}",
        said(&output)
    );
    assert_eq!(census(&s_copy), copy_before, "the refused directory is untouched");
    // The refused attempt announced no epoch-2 cursor to P: P is not fenced.
    assert_eq!(insert(&writer, 20_051..=20_060, "still-writable").await, 0);
    assert!(!p_data.join("NANO-FENCED.json").exists());
    drop(writer);
    // P dies (a crash, not a clean stop: no shutdown work is done on it).
    primary.kill();

    // --- A source at the directory's OWN epoch is refused ----------------
    let twin = dir.join("primary-twin");
    copy_dir(&p_data, &twin);
    let twin_args = primary_args(pg_twin, repl_twin, http_twin);
    let twin_primary = Daemon::spawn(&dir, "primary-twin", &twin, &token, &twin_args, None);
    wait_http(http_twin, "/health", 200, Duration::from_secs(60), &twin_primary).await;
    let p_before = census(&p_data);
    let output = cli_async(recover_args(&p_data, repl_twin, &token, &[]), Vec::new()).await;
    assert_eq!(output.status.code(), Some(1), "{}", said(&output));
    assert!(
        said(&output).contains("is at the same epoch 1") && said(&output).contains("--full --replace"),
        "{}",
        said(&output)
    );
    assert_eq!(census(&p_data), p_before, "the refused directory is untouched");
    drop(twin_primary);

    // --- (internal-tests) the rewind killed, refused, finished -----------
    #[cfg(feature = "internal-tests")]
    {
        use std::os::unix::process::ExitStatusExt;
        let kill = |at: &str| vec![("HELIOSDB_NANO_TEST_KILL_FAST_FORWARD_AT".to_string(), at.to_string())];
        // Killed right after the conversion's store write: no file of the
        // directory changed yet, and every start refuses it by name.
        let output = cli_async(
            recover_args(&p_data, repl_s, &token, &[]),
            kill("rewind-after-convert:0"),
        )
        .await;
        assert_eq!(output.status.signal(), Some(libc::SIGKILL), "{}", said(&output));
        assert!(p_data.join("NANO-PRIMARY-HISTORY.json").exists());
        for (name, args) in [
            ("as-primary", primary_args(free_port(), free_port(), free_port())),
            ("as-standby", standby_args(free_port(), repl_s, free_port())),
        ] {
            let (code, log) = refused_start(&dir, &format!("half-rewound-{name}"), &p_data, &token, &args);
            assert_eq!(code, Some(2), "{name}: {log}");
            assert!(
                log.contains("whose rewind into a standby") && log.contains("recover --from-primary"),
                "{name}: {log}"
            );
        }
        // Killed again during the leaf apply: the conversion is finished
        // (interim marker), the fast-forward is not; a standby start stays up
        // in needs_recovery naming the command.
        let output = cli_async(recover_args(&p_data, repl_s, &token, &[]), kill("after-delete:0")).await;
        assert_eq!(output.status.signal(), Some(libc::SIGKILL), "{}", said(&output));
        assert!(
            stdout(&output).contains("carries an interrupted rewind of a former primary"),
            "{}",
            said(&output)
        );
        assert!(!p_data.join("NANO-PRIMARY-HISTORY.json").exists());
        assert!(p_data.join("NANO-PHYSICAL-REPLICA.json").exists());
        let (pg, http) = (free_port(), free_port());
        let mut refused = Daemon::spawn(
            &dir,
            "rewinding-standby",
            &p_data,
            &token,
            &standby_args(pg, repl_s, http),
            None,
        );
        let block = wait_ready_body(http, Duration::from_secs(60), &refused, |code, body| {
            code == 503 && body["state"] == "needs_recovery"
        })
        .await;
        let reason = block["reason"].as_str().unwrap_or_default().to_string();
        assert!(
            reason.contains("fast-forward from the primary interrupted") && reason.contains("recover --from-primary"),
            "{reason}"
        );
        assert!(refused.alive(), "a refused standby stays up");
        refused.stop();
    }

    // --- The rewind ------------------------------------------------------
    let output = cli_async(recover_args(&p_data, repl_s, &token, &[]), Vec::new()).await;
    let report = stdout(&output);
    assert!(output.status.success(), "{}", said(&output));
    assert!(report.contains("fast-forward from the primary"), "{report}");
    assert!(!report.contains("a full copy of the primary is needed"), "{report}");
    assert!(
        report.contains("rewound the former primary into a standby at sequence"),
        "{report}"
    );
    assert!(
        report.contains("WARNING") && report.contains("WITHOUT fencing it"),
        "an unfenced old primary loses its writes after the fork, said loudly: {report}"
    );
    let percent = differing_percent(&report);
    assert!(
        percent < 20.0,
        "{percent}% of the bytes shipped for a divergence of under 1% of the rows: {report}"
    );
    let replaced = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_name().to_string_lossy().starts_with("primary.replaced-"));
    assert!(!replaced, "a rewind is in place");
    // P's replicated keyspace is S's export at the cut, byte for byte; its
    // cursor is the cut; its stage reopens; its primary-only state is gone.
    let (snapshot, source) = cursor_of(&p_data);
    assert_eq!(snapshot, source, "the cursor is re-cut at the cut");
    assert_eq!(
        digest_of(&p_data),
        export_digest_at(&s_data, source),
        "the new primary's default column family at the cut"
    );
    stage_reopens(&p_data);
    for gone in ["NANO-PRIMARY-HISTORY.json", "NANO-FENCED.json", "archive"] {
        assert!(!p_data.join(gone).exists(), "{gone} is primary-only state");
    }
    assert!(families(&p_data).iter().any(|family| family == LOCAL_CF));
    assert_eq!(store_epoch(&p_data), 2, "the primary's epoch arrived with the leaves");
    let kept = rewound(&p_data);
    assert_eq!(kept["from_epoch"], 1, "{kept}");
    assert_eq!(kept["to_epoch"], 2, "{kept}");
    assert!(kept["fence"].is_null(), "P was not fenced: {kept}");
    assert_eq!(kept["history"].as_str(), Some(history.as_str()), "{kept}");
    assert!(!dir.join("primary.recover-stage").exists());

    // P starts as a streaming standby of S: S's rows, not P's after the fork.
    let p_args = standby_args(pair.pg_p, repl_s, pair.http_p);
    let p_standby = Daemon::spawn(&dir, "primary-follows-s", &p_data, &token, &p_args, None);
    let mut want: Vec<i32> = (1..=20_000).collect();
    want.extend(30_001..=30_050);
    assert_eq!(
        settle(pair.pg_p, &want, Duration::from_secs(120)).await,
        Ok(want.clone())
    );
    assert!(
        !p_standby.log().contains("Bootstrapping physical standby"),
        "{}",
        p_standby.tail(20)
    );
    let block = wait_ready_body(pair.http_p, Duration::from_secs(60), &p_standby, |code, _| code == 200).await;
    assert_eq!(block["state"], "streaming", "{block}");
    assert_eq!(health(pair.http_p).await["replication"]["epoch"], 2);
    let in_recovery = connect(pair.pg_p).await;
    assert_eq!(
        value(&in_recovery, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("true")
    );
    assert_eq!(insert(&s, 30_051..=30_060, "after-the-rewind").await, 0);
    want.extend(30_051..=30_060);
    assert_eq!(settle(pair.pg_p, &want, Duration::from_secs(60)).await, Ok(want));
    drop(p_standby);
    drop(s_primary);
}

/// A primary of another dataset history is never rewound (nor replaced):
/// refused, naming both histories, and left as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_primary_of_another_history_is_refused_and_left_as_it_is() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let token = pair.token.clone();
    let source = Daemon::spawn(&dir, "source", &dir.join("source"), &token, &pair.primary_args, None);
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=5, "source").await, 0);

    let other = dir.join("other");
    let other_args = primary_args(pair.pg_s, free_port(), pair.http_s);
    let mut other_primary = Daemon::spawn(&dir, "other", &other, &token, &other_args, None);
    let other_client = connect(pair.pg_s).await;
    create_table(&other_client).await;
    assert_eq!(insert(&other_client, 1..=5, "other").await, 0);
    drop(other_client);
    other_primary.stop();
    let other_history = primary_history(&other);
    assert_ne!(other_history, primary_history(&dir.join("source")));

    let before = census(&other);
    let output = cli_async(recover_args(&other, pair.repl_p, &token, &[]), Vec::new()).await;
    assert_eq!(output.status.code(), Some(1), "{}", said(&output));
    assert!(
        said(&output).contains(&format!("of history {other_history}"))
            && said(&output).contains("only by its own dataset"),
        "{}",
        said(&output)
    );
    assert_eq!(census(&other), before, "the refused directory is untouched");
    assert!(!families(&other).iter().any(|family| family == LOCAL_CF));
    drop(writer);
    drop(source);
}
