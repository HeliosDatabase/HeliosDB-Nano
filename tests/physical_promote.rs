//! Spec P5.1 §8: `heliosdb-nano promote`, fencing and the nodes a promotion
//! leaves behind, on real managed daemons (`tests/support/physical_pair.rs`).
//!
//! * (1) a graceful promotion: the old primary P is fenced — writes refused
//!   25006 with the fence's text, `/ready` 503, `/health` state `fenced`,
//!   `NANO-FENCED.json` on disk, a session opened before the fence ended
//!   (FATAL 57P01, pushed at the fence since spec P5.2: the idle client's
//!   connection is closed before it issues anything) — and the promoted standby S holds
//!   everything through P's final sequence; S starts as the primary of the
//!   SAME history at epoch 2 (`pg_control_checkpoint`, `/health`), writes,
//!   and archives under `<H>/2/`;
//! * (2) a standby S2 of the old primary pointed at S needs recovery
//!   (`EpochStale`), `recover --from-primary` fast-forwards it, and it
//!   streams from S at epoch 2;
//! * (3) the fenced P restarted as a primary starts FENCED (spec §11.A:
//!   reads served, writes 25006, `transaction_read_only` on, `/ready` 503,
//!   state `fenced`), is refused as a standby (exit 2, the rebuild hint), and
//!   is rebuilt as a standby of S by `recover --from-primary --replace`;
//! * (4) a forced promotion with P killed: refused without `--force` within
//!   the fence timeout, done with it (`fenced = false`), and P restarted at
//!   epoch 1 fences itself the moment a receiver of epoch 2 talks to it,
//!   which stays `reconnecting` with the `SourceStale` reason;
//! * (5) a promotion killed after each of its steps (`internal-tests`) leaves
//!   a directory `start` refuses naming the command, and a re-run finishes it;
//! * (6) a running standby is refused with the stop hint, untouched;
//! * (7) a second promotion of the same epoch is refused: P names the
//!   promotion that fenced it first, the refused standby's replicated
//!   (default) family is unchanged — its node-local family gains the
//!   promotion's identity by design — and `pg_promote` on the new primary
//!   is refused 55000 "already the primary (epoch 2)" (spec P5.2; it was an
//!   undefined function, 42883, before);
//! * (8) the promoted store's first open replays none of the old primary's
//!   logical WAL: P runs with the logical WAL's checkpoint never advanced
//!   (interval 0) after a restart left one, so the standby's copy holds
//!   entries ABOVE its redo floor — a trigger-rewritten, sequence-keyed
//!   insert workload and DDL — and `promote` says it raised the floor from
//!   below the highest entry; the rows are unchanged, the trigger's rewrite
//!   is not applied twice, a sequence continues, and nothing is replayed;
//! * (10) the archives: `<H>/1/` and `<H>/2/` side by side, `recover
//!   --from-archive` of an epoch-1 standby from epoch-1 segments still works,
//!   and epoch-2 segments are refused for it naming both epochs.
//!
//! (9) is `tests/promotion_refusal_tests.rs`.
//!
//! Red on the base of this change (P4, `18085df7`): the epoch was a constant,
//! there was no `promote` command, both ends refused a fence, and a standby
//! of a promoted primary could only be rebuilt by a full copy.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use heliosdb_nano::replication::epoch::{epoch_of, read_directory_epoch};
use heliosdb_nano::replication::physical::{range_digest, LOCAL_CF};
use physical_pair::*;
use rocksdb::{Options, DB};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Every node archives, with segments sealed every second, so the epochs'
/// runs of segments show up quickly.
const ARCHIVE: &str = "[replication.archive]\nenabled = true\nsegment_max_secs = 1\n";
/// Scenario 8's primary: as [`ARCHIVE`], and its logical WAL's checkpoint is
/// never advanced while it runs (both intervals 0), so every entry after its
/// last clean close stays ABOVE the redo floor — what a replay at the
/// promoted store's first open would re-execute.
const NO_CHECKPOINT_PRIMARY: &str = "[replication.archive]\nenabled = true\nsegment_max_secs = 1\n[storage]\n\
                                     wal_checkpoint_interval_entries = 0\nwal_checkpoint_interval_secs = 0\n";

fn cli(args: &[String], env: &[(&str, String)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"));
    command.args(args).env("RUST_LOG", "warn");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

async fn cli_async(args: Vec<String>, env: Vec<(&'static str, String)>) -> Output {
    tokio::task::spawn_blocking(move || cli(&args, &env)).await.unwrap()
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

/// `promote` of `data`, fencing the primary at `primary_port`.
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

/// Spec P5.1 §11.I: what `promote` says it did to the logical WAL — the
/// redo floor it found (`None`: absent) and the highest retained entry.
fn redo_floor(out: &str) -> (Option<u64>, u64) {
    let number = |text: &str| -> u64 {
        text.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or_else(|_| panic!("no number in {text:?}: {out}"))
    };
    let was = out
        .split("(it was ")
        .nth(1)
        .unwrap_or_else(|| panic!("promote names no redo floor it found: {out}"));
    let before = (!was.starts_with("absent")).then(|| number(was));
    let highest = out
        .split("retained entries up to ")
        .nth(1)
        .unwrap_or_else(|| panic!("promote names no highest entry: {out}"));
    (before, number(highest))
}

/// The dataset history a primary's directory names.
fn primary_history(dir: &Path) -> String {
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("NANO-PRIMARY-HISTORY.json")).unwrap()).unwrap();
    record["history_id"].as_str().unwrap().to_string()
}

/// The digest of the keys under `prefix` of the store at `dir`, read-only
/// (every family opened: a standby's directory holds the node-local one).
fn digest(dir: &Path, prefix: &[u8]) -> [u8; 32] {
    let families = DB::list_cf(&Options::default(), dir).unwrap();
    let db = DB::open_cf_for_read_only(&Options::default(), dir, families, false).unwrap();
    range_digest(&db, None, prefix).unwrap()
}

fn families(dir: &Path) -> Vec<String> {
    DB::list_cf(&Options::default(), dir).unwrap()
}

fn store_epoch(dir: &Path) -> u64 {
    epoch_of(read_directory_epoch(dir).unwrap().as_ref())
}

async fn health(port: u16) -> serde_json::Value {
    http(port, "/health").await.map(|(_, body)| body).unwrap_or_default()
}

/// Poll `/health` until its replication block satisfies `want`.
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

/// Wait until `path` exists.
async fn wait_file(path: &Path, budget: Duration, daemon: &Daemon) {
    let deadline = Instant::now() + budget;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared; log tail: {}",
            path.display(),
            daemon.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The sealed segments of `archive` at `epoch`.
fn segments_at(archive: &Path, epoch: u64) -> usize {
    sealed(archive)
        .iter()
        .filter(|segment| segment.epoch() == epoch)
        .count()
}

/// A daemon refused at start: its exit code and its log.
fn refused_start(dir: &Path, name: &str, data: &Path, token: &Path, args: &[String]) -> (Option<i32>, String) {
    let mut daemon = Daemon::spawn(dir, name, data, token, args, Some(ARCHIVE));
    let status = daemon.wait_exit(Duration::from_secs(60));
    (status.and_then(|status| status.code()), daemon.log())
}

/// The cluster of scenarios 1, 2, 3, 7, 8 and 10: primary P, standbys S and
/// S2. S2 stops early (before P's last rows), S just before the promotion.
struct Cluster {
    pair: Pair,
    primary: Daemon,
    p_data: PathBuf,
    s_data: PathBuf,
    s2_data: PathBuf,
    /// Ports of S once it is a primary, and of S2.
    repl_s: u16,
    pg_s2: u16,
    http_s2: u16,
    /// The rows P holds at the promotion.
    rows: Vec<i32>,
    /// The last value P's sequence handed out.
    sequence: i64,
}

async fn cluster() -> Cluster {
    let pair = pair();
    let dir = pair.dir.path();
    let p_data = dir.join("primary");
    let s_data = dir.join("standby");
    let s2_data = dir.join("standby2");
    let (repl_s, pg_s2, http_s2) = (free_port(), free_port(), free_port());
    let mut first_run = Daemon::spawn(
        dir,
        "primary",
        &p_data,
        &pair.token,
        &pair.primary_args,
        Some(NO_CHECKPOINT_PRIMARY),
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=40, "before").await, 0);
    drop(writer);
    // Scenario 8: a clean close records the logical WAL's checkpoint; from
    // the restart on it is never advanced, so what follows stays above it.
    first_run.stop();
    let primary = Daemon::spawn(
        dir,
        "primary-2",
        &p_data,
        &pair.token,
        &pair.primary_args,
        Some(NO_CHECKPOINT_PRIMARY),
    );
    let writer = connect(pair.pg_p).await;
    // Logical-WAL entries a replay would re-execute: DDL, a BEFORE INSERT
    // trigger's rewrite, sequence-keyed inserts, and a sequence whose value
    // must continue after the promotion.
    writer
        .batch_execute("CREATE TABLE ddl_probe (id INT PRIMARY KEY); ALTER TABLE ddl_probe ADD COLUMN note TEXT")
        .await
        .unwrap();
    writer.batch_execute("CREATE SEQUENCE promo_seq").await.unwrap();
    writer
        .batch_execute(
            "CREATE TABLE trig_probe (id BIGINT PRIMARY KEY, n INT, note TEXT); CREATE FUNCTION trig_probe_rewrite() \
             RETURNS TRIGGER AS $$ BEGIN NEW.note = 'rewritten'; RETURN NEW; END $$ LANGUAGE plpgsql; CREATE \
             TRIGGER trig_probe_t BEFORE INSERT ON trig_probe FOR EACH ROW EXECUTE FUNCTION trig_probe_rewrite()",
        )
        .await
        .unwrap();
    let mut sequence = 0;
    for n in 0..5 {
        sequence = value(&writer, "SELECT nextval('promo_seq')")
            .await
            .unwrap()
            .parse()
            .unwrap();
        writer
            .batch_execute(&format!("INSERT INTO trig_probe (id, n) VALUES ({sequence}, {n})"))
            .await
            .unwrap();
    }
    assert_eq!(
        value(&writer, "SELECT count(*) FROM trig_probe WHERE note = 'rewritten'")
            .await
            .as_deref(),
        Some("5"),
        "the trigger rewrote every insert once"
    );

    let mut standby = Daemon::spawn(dir, "standby", &s_data, &pair.token, &pair.standby_args, Some(ARCHIVE));
    let s2_args = standby_args(pg_s2, pair.repl_p, http_s2);
    let mut standby2 = Daemon::spawn(dir, "standby2", &s2_data, &pair.token, &s2_args, Some(ARCHIVE));
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    wait_http(http_s2, "/ready", 200, Duration::from_secs(120), &standby2).await;
    let first: Vec<i32> = (1..=40).collect();
    assert_eq!(
        settle(pair.pg_s, &first, Duration::from_secs(60)).await,
        Ok(first.clone())
    );
    assert_eq!(settle(pg_s2, &first, Duration::from_secs(60)).await, Ok(first.clone()));
    // S2 stops behind: P's rows 41..=50 reach S only.
    standby2.stop();
    assert_eq!(insert(&writer, 41..=50, "s-only").await, 0);
    let middle: Vec<i32> = (1..=50).collect();
    assert_eq!(
        settle(pair.pg_s, &middle, Duration::from_secs(60)).await,
        Ok(middle.clone())
    );
    // S stops; P takes rows 51..=60 that only the promotion's tail brings.
    standby.stop();
    assert_eq!(insert(&writer, 51..=60, "tail").await, 0);
    let rows: Vec<i32> = (1..=60).collect();
    assert_eq!(ids(&writer).await, Ok(rows.clone()));
    Cluster {
        primary,
        p_data,
        s_data,
        s2_data,
        repl_s,
        pg_s2,
        http_s2,
        rows,
        sequence,
        pair,
    }
}

/// Scenarios 1, 2, 3, 7, 8, 10, in the order a cluster lives through them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_graceful_promotion_fences_the_primary_and_every_node_left_behind_recovers() {
    let mut c = cluster().await;
    let dir = c.pair.dir.path().to_path_buf();
    let token = c.pair.token.clone();
    let history = primary_history(&c.p_data);
    assert_eq!(
        store_epoch(&c.s_data),
        1,
        "a standby of a never-promoted primary is at epoch 1"
    );
    // A session opened before the fence (its connection future kept: the
    // pushed FATAL ends it, spec P5.2 §1).
    let (before_fence, before_fence_connection) = connect_watched(c.pair.pg_p).await;
    assert_eq!(value(&before_fence, "SELECT 1").await.as_deref(), Some("1"));

    // --- (1) the graceful promotion --------------------------------------
    let started = Instant::now();
    let output = cli_async(
        promote_args(&c.s_data, c.pair.repl_p, &token, &["--fence-timeout", "60"]),
        Vec::new(),
    )
    .await;
    let out = stdout(&output);
    assert!(output.status.success(), "promote failed: {out}{}", stderr(&output));
    assert!(
        out.contains(&format!("is the primary of history {history} at epoch 2 (from 1;"))
            && out.contains("was fenced at its sequence")
            && out.contains("start it with heliosdb-nano start --replication-role primary --data-dir"),
        "{out}"
    );
    assert!(
        out.contains("tailed the fenced primary"),
        "the rows 51..=60 came by the tail: {out}"
    );
    // (8) the copy held logical-WAL entries ABOVE its redo floor (a replay
    // would have re-executed them), and the conversion raised the floor.
    let (before, highest) = redo_floor(&out);
    assert!(
        before.is_some_and(|before| before < highest),
        "the standby's copy held entries above its redo floor (before {before:?}, highest {highest}): {out}"
    );
    println!("graceful promotion took {:?}", started.elapsed());

    // P is fenced: writes refused 25006 with the fence's text, reads served.
    let after_fence = connect(c.pair.pg_p).await;
    let refused = after_fence
        .batch_execute("INSERT INTO t VALUES (999, 'after the fence')")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&refused), "25006", "{refused}");
    assert!(
        message(&refused).contains("write refused: this primary was fenced") && message(&refused).contains("epoch 2"),
        "{}",
        message(&refused)
    );
    assert_eq!(
        ids(&after_fence).await,
        Ok(c.rows.clone()),
        "a fenced primary serves reads"
    );
    let ready = wait_http(c.pair.http_p, "/ready", 503, Duration::from_secs(30), &c.primary).await;
    assert_eq!(ready["state"], "fenced", "{ready}");
    let block = health(c.pair.http_p).await["replication"].clone();
    assert_eq!(block["state"], "fenced", "{block}");
    assert_eq!(block["ready"], false, "{block}");
    assert_eq!(
        block["epoch"], 1,
        "the fenced primary is still at its own epoch: {block}"
    );
    assert!(c.p_data.join("NANO-FENCED.json").exists());
    let ready_check = after_fence
        .simple_query("SELECT heliosdb_require_ready()")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&ready_check), "55000", "{ready_check}");
    // The session opened before the fence is ended with FATAL 57P01. Spec
    // P5.2 §1: PUSHED at the fence, so this idle client received it with no
    // request pending — the error its connection future ended with, which
    // carries the SQLSTATE; a statement now fails without reaching the
    // server. `tests/physical_promote_online.rs` reads the pushed frame
    // itself.
    let ended = connection_end(before_fence_connection, Duration::from_secs(10))
        .await
        .expect_err("the pre-fence session is ended by the fence");
    assert_eq!(sqlstate(&ended), "57P01", "{ended}; {}", c.primary.tail(12));
    assert!(
        message(&ended).contains("this primary was fenced by epoch 2"),
        "{}",
        message(&ended)
    );
    assert!(before_fence.is_closed());
    assert!(before_fence.simple_query("SELECT 1").await.is_err());
    assert!(
        c.primary
            .log_contains_within("was FENCED by epoch 2", Duration::from_secs(10)),
        "{}",
        c.primary.tail(12)
    );

    // S holds everything through P's final sequence.
    assert_eq!(digest(&c.s_data, b"data:"), digest(&c.p_data, b"data:"));
    assert_eq!(digest(&c.s_data, b"meta:table:"), digest(&c.p_data, b"meta:table:"));
    assert!(!families(&c.s_data).iter().any(|family| family == LOCAL_CF));
    assert!(!c.s_data.join("NANO-PHYSICAL-REPLICA.json").exists());
    assert!(!c.s_data.join("NANO-PROMOTION.json").exists());
    assert_eq!(primary_history(&c.s_data), history, "a promotion never mints a history");
    let record = read_directory_epoch(&c.s_data).unwrap().unwrap();
    assert_eq!((record.epoch, record.previous, record.fenced), (2, 1, true));
    assert!(record.old_primary_final_sequence.is_some());

    // S starts as the primary at epoch 2.
    let s_args = primary_args(c.pair.pg_s, c.repl_s, c.pair.http_s);
    let s_primary = Daemon::spawn(&dir, "s-primary", &c.s_data, &token, &s_args, Some(ARCHIVE));
    let s = connect(c.pair.pg_s).await;
    assert_eq!(
        value(&s, "SELECT pg_is_in_recovery()::text").await.as_deref(),
        Some("false")
    );
    assert_eq!(value(&s, "SHOW in_hot_standby").await.as_deref(), Some("off"));
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
    let block = wait_health(c.pair.http_s, Duration::from_secs(30), &s_primary, |block| {
        block["epoch"] == 2
    })
    .await;
    assert_eq!(block["replication"]["role"], "primary", "{block}");
    // (8) nothing of the old primary's logical WAL was replayed: the rows are
    // P's, the probe tables are P's (the trigger's rewrite applied once, no
    // row twice), and the sequence continues.
    assert_eq!(ids(&s).await, Ok(c.rows.clone()));
    assert_eq!(
        value(&s, "SELECT count(*) FROM ddl_probe").await.as_deref(),
        Some("0"),
        "the DDL ran once"
    );
    assert_eq!(
        value(&s, "SELECT count(*) FROM trig_probe").await.as_deref(),
        Some("5"),
        "the trigger workload's rows, once each"
    );
    assert_eq!(
        value(&s, "SELECT count(*) FROM trig_probe WHERE note = 'rewritten'")
            .await
            .as_deref(),
        Some("5")
    );
    let next: i64 = value(&s, "SELECT nextval('promo_seq')").await.unwrap().parse().unwrap();
    assert!(next > c.sequence, "the sequence continues past {}: {next}", c.sequence);
    assert!(
        !s_primary.log().contains("WAL crash recovery: replayed"),
        "the promoted store replayed logical WAL: {}",
        s_primary.tail(20)
    );
    // Writes succeed, and S archives its epoch.
    assert_eq!(insert(&s, 61..=65, "epoch-2").await, 0);
    let mut rows = c.rows.clone();
    rows.extend(61..=65);
    let s_archive = dir.join("standby.archive");
    let deadline = Instant::now() + Duration::from_secs(90);
    while segments_at(&s_archive, 2) == 0 {
        assert!(
            Instant::now() < deadline,
            "S never sealed an epoch-2 segment: {}",
            s_primary.tail(12)
        );
        assert_eq!(insert(&s, 1000..=1000, "nudge").await, 0);
        s.batch_execute("DELETE FROM t WHERE id = 1000").await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // --- (10) the archives ------------------------------------------------
    assert!(segments_at(&s_archive, 1) > 0, "S archived epoch 1 as a standby");
    let listed = cli(&strings(&["archive", "list", "--archive-dir", &text(&s_archive)]), &[]);
    let listing = stdout(&listed);
    assert!(
        listing.contains(&format!("{history}/1 ")) && listing.contains(&format!("{history}/2 ")),
        "archive list shows both epochs: {listing}"
    );

    // --- (7) a second promotion of the same epoch is refused --------------
    // The replicated (default) family only: the refused promotion keeps its
    // identity in S2's node-local family, by design (spec P5.1 §11.C).
    let s2_before = digest(&c.s2_data, b"");
    let output = cli_async(
        promote_args(&c.s2_data, c.pair.repl_p, &token, &["--fence-timeout", "30"]),
        Vec::new(),
    )
    .await;
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(2), "{out}{}", stderr(&output));
    assert!(
        out.contains("was already fenced by epoch 2 (node") && out.contains("a primary at epoch 2 exists"),
        "{out}"
    );
    assert!(!c.s2_data.join("NANO-PROMOTION.json").exists());
    assert!(c.s2_data.join("NANO-PHYSICAL-REPLICA.json").exists());
    assert_eq!(
        digest(&c.s2_data, b""),
        s2_before,
        "S2's replicated (default-family) keyspace is unchanged"
    );
    assert_eq!(store_epoch(&c.s2_data), 1);
    // Spec P5.2 §2: `pg_promote` on a primary — the new one — is refused:
    // 55000, "this node is already the primary (epoch 2)".
    let refused = s.simple_query("SELECT pg_promote(true, 5)").await.unwrap_err();
    assert_eq!(sqlstate(&refused), "55000", "{refused}");
    assert!(
        message(&refused).contains("this node is already the primary (epoch 2)"),
        "{}",
        message(&refused)
    );

    // (10, continued) an epoch-1 standby recovers from epoch-1 segments
    // still (P's archive: rows 41..=60), and is refused epoch-2 segments
    // naming both epochs.
    let p_archive = dir.join("primary.archive");
    let output = cli(
        &strings(&[
            "recover",
            "--data-dir",
            &text(&c.s2_data),
            "--from-archive",
            &text(&p_archive),
            "--then",
            "stop",
        ]),
        &[],
    );
    assert!(output.status.success(), "{}{}", stdout(&output), stderr(&output));
    assert_eq!(digest(&c.s2_data, b"data:"), digest(&c.p_data, b"data:"));
    let epoch2 = s_archive.join(&history).join("2");
    let output = cli(
        &strings(&[
            "recover",
            "--data-dir",
            &text(&c.s2_data),
            "--from-archive",
            &text(&epoch2),
            "--then",
            "stop",
        ]),
        &[],
    );
    let said = format!("{}{}", stdout(&output), stderr(&output));
    assert!(!output.status.success(), "{said}");
    assert!(
        said.contains("another epoch of this dataset")
            && said.contains("epoch 2 of history")
            && said.contains("at epoch 1"),
        "{said}"
    );

    // --- (2) S2, a standby of the old primary, pointed at S --------------
    let s2_args = standby_args(c.pg_s2, c.repl_s, c.http_s2);
    let mut s2 = Daemon::spawn(&dir, "standby2-at-s", &c.s2_data, &token, &s2_args, Some(ARCHIVE));
    let block = wait_health(c.http_s2, Duration::from_secs(60), &s2, |block| {
        block["state"] == "needs_recovery"
    })
    .await;
    let reason = block["replication"]["reason"].as_str().unwrap_or_default().to_string();
    assert!(
        reason.contains("epoch 2") && reason.contains("recover --from-primary"),
        "the EpochStale reason names the fix: {reason}"
    );
    s2.stop();
    let output = cli_async(
        recover_args(&c.s2_data, c.repl_s, &token, &["--full-copy-threshold", "100"]),
        Vec::new(),
    )
    .await;
    let out = stdout(&output);
    assert!(output.status.success(), "{out}{}", stderr(&output));
    assert!(out.contains("fast-forward"), "route 2: {out}");
    assert!(
        out.contains("promoted to epoch 2"),
        "the recover output says what the promotion did: {out}"
    );
    assert_eq!(store_epoch(&c.s2_data), 2);
    let mut s2 = Daemon::spawn(&dir, "standby2-follows-s", &c.s2_data, &token, &s2_args, Some(ARCHIVE));
    wait_http(c.http_s2, "/ready", 200, Duration::from_secs(120), &s2).await;
    assert_eq!(settle(c.pg_s2, &rows, Duration::from_secs(60)).await, Ok(rows.clone()));
    assert_eq!(health(c.http_s2).await["replication"]["epoch"], 2);
    s2.stop();

    // --- (3) the fenced P starts fenced, never as a standby; it is rebuilt ---
    c.primary.stop();
    // Spec P5.1 §11.A: restarted as a primary it is fenced from its first
    // second — reads served, every write refused, read-only to clients, not
    // ready — in no time a client could write.
    let mut restarted = Daemon::spawn(
        &dir,
        "primary-restart",
        &c.p_data,
        &token,
        &c.pair.primary_args,
        Some(ARCHIVE),
    );
    let ready = wait_http(c.pair.http_p, "/ready", 503, Duration::from_secs(60), &restarted).await;
    assert_eq!(ready["state"], "fenced", "{ready}");
    let p = connect(c.pair.pg_p).await;
    assert_eq!(ids(&p).await, Ok(c.rows.clone()), "a fenced primary serves reads");
    let refused = p
        .batch_execute("INSERT INTO t VALUES (998, 'after the restart')")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&refused), "25006", "{refused}");
    assert!(
        message(&refused).contains("write refused: this primary was fenced at"),
        "{}",
        message(&refused)
    );
    assert_eq!(value(&p, "SHOW transaction_read_only").await.as_deref(), Some("on"));
    assert_eq!(value(&p, "SHOW in_hot_standby").await.as_deref(), Some("off"));
    let block = health(c.pair.http_p).await["replication"].clone();
    assert_eq!(block["state"], "fenced", "{block}");
    assert_eq!(block["ready"], false, "{block}");
    drop(p);
    restarted.stop();
    // As a standby it is refused: it is rebuilt as one, never followed over.
    let (code, log) = refused_start(&dir, "primary-as-standby", &c.p_data, &token, &c.pair.standby_args);
    assert_eq!(code, Some(2), "a fenced directory is never started as a standby: {log}");
    assert!(
        log.contains("start refused")
            && log.contains("cannot be started as one")
            && log.contains("recover --from-primary"),
        "{log}"
    );
    let output = cli_async(recover_args(&c.p_data, c.repl_s, &token, &["--replace"]), Vec::new()).await;
    let out = stdout(&output);
    assert!(output.status.success(), "{out}{}", stderr(&output));
    assert!(out.contains("full copy") && out.contains("fenced primary"), "{out}");
    let replaced = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_name().to_string_lossy().starts_with("primary.replaced-"));
    assert!(replaced, "the old directory is kept beside it");
    assert!(!c.p_data.join("NANO-FENCED.json").exists());
    let p_args = standby_args(c.pair.pg_p, c.repl_s, c.pair.http_p);
    let p_standby = Daemon::spawn(&dir, "primary-follows-s", &c.p_data, &token, &p_args, Some(ARCHIVE));
    wait_http(c.pair.http_p, "/ready", 200, Duration::from_secs(120), &p_standby).await;
    assert_eq!(
        settle(c.pair.pg_p, &rows, Duration::from_secs(60)).await,
        Ok(rows.clone())
    );
    assert_eq!(health(c.pair.http_p).await["replication"]["epoch"], 2);
    drop(p_standby);
    drop(s_primary);
}

/// Scenario 4: a forced promotion with the primary killed, and the old
/// primary restarted at epoch 1 fencing itself when a receiver of epoch 2
/// reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forced_promotion_and_a_superseded_primary_that_fences_itself() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let p_data = dir.join("primary");
    let s_data = dir.join("standby");
    let s4_data = dir.join("standby4");
    let (repl_s, pg_4, http_4) = (free_port(), free_port(), free_port());
    let mut primary = Daemon::spawn(&dir, "primary", &p_data, &pair.token, &pair.primary_args, Some(ARCHIVE));
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=20, "before").await, 0);
    let mut standby = Daemon::spawn(&dir, "standby", &s_data, &pair.token, &pair.standby_args, Some(ARCHIVE));
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    let rows: Vec<i32> = (1..=20).collect();
    assert_eq!(
        settle(pair.pg_s, &rows, Duration::from_secs(60)).await,
        Ok(rows.clone())
    );
    standby.stop();
    drop(writer);
    primary.kill();

    // Without --force: refused within the fence timeout, nothing changed.
    let started = Instant::now();
    let output = cli_async(
        promote_args(&s_data, pair.repl_p, &pair.token, &["--fence-timeout", "3"]),
        Vec::new(),
    )
    .await;
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(2), "{out}{}", stderr(&output));
    assert!(
        out.contains("did not answer the fence within 3 s") && out.contains("re-run with --force"),
        "{out}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "refused within the timeout: {:?}",
        started.elapsed()
    );
    assert!(!s_data.join("NANO-PROMOTION.json").exists());
    assert!(s_data.join("NANO-PHYSICAL-REPLICA.json").exists());

    // With --force: promoted, not fenced, said as a WARNING.
    let output = cli_async(
        promote_args(&s_data, pair.repl_p, &pair.token, &["--fence-timeout", "3", "--force"]),
        Vec::new(),
    )
    .await;
    let out = stdout(&output);
    assert!(output.status.success(), "{out}{}", stderr(&output));
    assert!(
        out.contains("WARNING:") && out.contains("NOT fenced (--force)"),
        "{out}"
    );
    let record = read_directory_epoch(&s_data).unwrap().unwrap();
    assert_eq!(
        (record.epoch, record.fenced, record.old_primary_final_sequence),
        (2, false, None)
    );

    // S serves as the primary of epoch 2; S4, a standby of epoch 2, follows it.
    let s_args = primary_args(pair.pg_s, repl_s, pair.http_s);
    let mut s_primary = Daemon::spawn(&dir, "s-primary", &s_data, &pair.token, &s_args, Some(ARCHIVE));
    let s4_args = standby_args(pg_4, repl_s, http_4);
    let s4 = Daemon::spawn(&dir, "standby4", &s4_data, &pair.token, &s4_args, Some(ARCHIVE));
    wait_http(http_4, "/ready", 200, Duration::from_secs(120), &s4).await;
    assert_eq!(health(http_4).await["replication"]["epoch"], 2);

    // S goes away, and the OLD primary comes back (epoch 1) on S's
    // replication port: S4 reconnects to it with its epoch-2 `Hello`, P
    // fences itself, and S4 stays reconnecting with the SourceStale reason.
    s_primary.stop();
    let p_args = primary_args(pair.pg_p, repl_s, pair.http_p);
    let old = Daemon::spawn(&dir, "old-primary", &p_data, &pair.token, &p_args, Some(ARCHIVE));
    wait_file(&p_data.join("NANO-FENCED.json"), Duration::from_secs(120), &old).await;
    wait_health(pair.http_p, Duration::from_secs(30), &old, |block| {
        block["state"] == "fenced"
    })
    .await;
    let block = wait_health(http_4, Duration::from_secs(60), &s4, |block| {
        block["state"] == "reconnecting"
            && block["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("superseded"))
    })
    .await;
    assert_eq!(block["replication"]["epoch"], 2, "{block}");
    assert!(
        s4.log_contains_within("is a superseded primary", Duration::from_secs(10)),
        "{}",
        s4.tail(12)
    );
    let fenced: serde_json::Value =
        serde_json::from_slice(&std::fs::read(p_data.join("NANO-FENCED.json")).unwrap()).unwrap();
    assert_eq!(
        (fenced["epoch"].as_u64(), fenced["fenced_by_epoch"].as_u64()),
        (Some(1), Some(2))
    );
    let p = connect(pair.pg_p).await;
    let refused = p
        .batch_execute("INSERT INTO t VALUES (100, 'split brain')")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&refused), "25006", "{refused}");
}

/// Scenario 6: a running standby is refused with the stop hint and keeps
/// running untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_standby_is_refused_and_left_as_it_is() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let s_data = dir.join("standby");
    let _primary = Daemon::spawn(
        &dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        Some(ARCHIVE),
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=5, "x").await, 0);
    let standby = Daemon::spawn(&dir, "standby", &s_data, &pair.token, &pair.standby_args, Some(ARCHIVE));
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(120), &standby).await;
    let output = cli_async(promote_args(&s_data, pair.repl_p, &pair.token, &[]), Vec::new()).await;
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(2), "{out}{}", stderr(&output));
    assert!(
        out.contains("the standby is running") && out.contains("stop it first (`heliosdb-nano stop`)"),
        "{out}"
    );
    assert!(!s_data.join("NANO-PROMOTION.json").exists());
    assert!(s_data.join("NANO-PHYSICAL-REPLICA.json").exists());
    assert_eq!(insert(&writer, 6..=6, "after").await, 0);
    let rows: Vec<i32> = (1..=6).collect();
    assert_eq!(settle(pair.pg_s, &rows, Duration::from_secs(60)).await, Ok(rows));
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(30), &standby).await;
}

/// Scenario 5: a promotion SIGKILLed after each of its steps leaves a
/// directory every `start` refuses naming the command; a re-run finishes it
/// to the state a promotion that was never interrupted leaves.
#[cfg(feature = "internal-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupted_promotion_is_refused_by_start_and_finished_by_a_re_run() {
    use heliosdb_nano::replication::promote::KILL_PROMOTE_AT_ENV;
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let p_data = dir.join("primary");
    let mut primary = Daemon::spawn(&dir, "primary", &p_data, &pair.token, &pair.primary_args, Some(ARCHIVE));
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=10, "x").await, 0);
    let history = primary_history(&p_data);
    let rows: Vec<i32> = (1..=10).collect();
    let steps = ["after-record", "after-epoch", "after-drop", "after-identity"];
    let mut standbys = Vec::new();
    for step in steps {
        let data = dir.join(format!("standby-{step}"));
        let (pg, http) = (free_port(), free_port());
        let args = standby_args(pg, pair.repl_p, http);
        let mut standby = Daemon::spawn(
            &dir,
            &format!("standby-{step}"),
            &data,
            &pair.token,
            &args,
            Some(ARCHIVE),
        );
        wait_http(http, "/ready", 200, Duration::from_secs(120), &standby).await;
        assert_eq!(settle(pg, &rows, Duration::from_secs(60)).await, Ok(rows.clone()));
        standby.stop();
        standbys.push((step, data));
    }
    drop(writer);
    // The primary is down: the promotions are forced (the hook points are
    // after the fence), and none fences the others' primary.
    primary.stop();
    for (step, data) in standbys {
        let args = promote_args(&data, pair.repl_p, &pair.token, &["--force", "--fence-timeout", "3"]);
        let killed = cli_async(args.clone(), vec![(KILL_PROMOTE_AT_ENV, step.to_string())]).await;
        assert!(
            !killed.status.success(),
            "{step}: the hook did not kill promote: {}",
            stdout(&killed)
        );
        assert!(
            data.join("NANO-PROMOTION.json").exists(),
            "{step}: the record survives the kill"
        );
        for (name, role_args) in [
            ("as-primary", primary_args(free_port(), free_port(), free_port())),
            ("as-standby", standby_args(free_port(), pair.repl_p, free_port())),
        ] {
            let (code, log) = refused_start(&dir, &format!("{step}-{name}"), &data, &pair.token, &role_args);
            assert_eq!(code, Some(2), "{step} {name}: {log}");
            assert!(
                log.contains("promotion interrupted: finish it with `heliosdb-nano promote --data-dir"),
                "{step} {name}: {log}"
            );
        }
        let rerun = cli_async(args, Vec::new()).await;
        let out = stdout(&rerun);
        assert!(rerun.status.success(), "{step}: {out}{}", stderr(&rerun));
        assert!(out.contains("carries an interrupted promotion"), "{step}: {out}");
        // The state an uninterrupted promotion leaves.
        assert_eq!(store_epoch(&data), 2, "{step}");
        assert_eq!(primary_history(&data), history, "{step}");
        assert!(!families(&data).iter().any(|family| family == LOCAL_CF), "{step}");
        for name in [
            "NANO-PHYSICAL-REPLICA.json",
            "NANO-PHYSICAL-MANIFEST.json",
            "NANO-PROMOTION.json",
        ] {
            assert!(!data.join(name).exists(), "{step}: {name}");
        }
        assert_eq!(digest(&data, b"data:"), digest(&p_data, b"data:"), "{step}");
    }
}
