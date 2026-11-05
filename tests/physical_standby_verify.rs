//! Spec R1 §1: `heliosdb-nano standby verify` on real managed daemons
//! (`tests/support/physical_pair.rs`).
//!
//! * an identical pair: the standby, stopped behind its primary, is brought
//!   to the primary's cut by the tail a start would apply and found
//!   identical (exit 0, "identical at sequence P", the primary's default
//!   column family byte for byte); a second run against the idle primary
//!   compares at the standby's own cursor — a receiver AT the cut — and finds
//!   it identical again with nothing applied; the standby still starts and
//!   follows afterwards;
//! * a standby whose replicated keyspace was altered directly (a test-only
//!   write to its default family that keeps its cursor accounted, so only a
//!   comparison can tell) differs in exactly the leaf holding the altered key:
//!   exit 1, that leaf named with both digests;
//! * a running standby is refused (exit 2) and keeps running;
//! * a primary whose WAL no longer holds the standby's next write group (its
//!   archive off) is refused with the `recover --from-primary` command
//!   (exit 2), and the standby is left where it was.
//!
//! Red before R1: the command did not exist (and a fast-forward session, the
//! leaf plan's carrier, refused a receiver at the primary's cut).

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use heliosdb_nano::replication::physical::{decode_cursor, range_digest, CURSOR_KEY, LOCAL_CF};
use heliosdb_nano::replication::recover_primary::format_count;
use physical_pair::*;
use rocksdb::{Options, DB};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

/// Small leaves, so a test-sized dataset has many of them.
const LEAF: &str = "64KiB";

fn verify_args(pair: &Pair, data: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        "verify".to_string(),
        "--data-dir".to_string(),
        data.display().to_string(),
        "--against".to_string(),
        format!("127.0.0.1:{}", pair.repl_p),
        "--replication-token-file".to_string(),
        pair.token.display().to_string(),
        "--leaf-bytes".to_string(),
        LEAF.to_string(),
    ];
    args.extend(extra.iter().map(|arg| arg.to_string()));
    args
}

/// `heliosdb-nano standby verify …` on a blocking thread, so the runtime
/// keeps serving the test meanwhile.
async fn verify(pair: &Pair, data: &Path, extra: &[&str]) -> Output {
    let args = verify_args(pair, data, extra);
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
            .arg("standby")
            .args(&args)
            .env("RUST_LOG", "warn")
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(stdout(output).trim())
        .unwrap_or_else(|error| panic!("not one JSON object ({error}): {}{}", stdout(output), stderr(output)))
}

/// A stopped standby's cursor: its source sequence.
fn cursor_of(path: &Path) -> u64 {
    let db = DB::open_cf_for_read_only(&Options::default(), path, ["default", LOCAL_CF], false).unwrap();
    let bytes = db.get_cf(db.cf_handle(LOCAL_CF).unwrap(), CURSOR_KEY).unwrap().unwrap();
    decode_cursor(&bytes).unwrap().source_sequence()
}

/// The default column family's digest of the store at `path` (read-only,
/// even under a running node), with the store's sequence.
fn default_digest(path: &Path) -> (u64, [u8; 32]) {
    let db = DB::open_for_read_only(&Options::default(), path, false).unwrap();
    (db.latest_sequence_number(), range_digest(&db, None, b"").unwrap())
}

/// Spec R1 §1's "test-only writer to the default family": put `key` into the
/// STOPPED standby's replicated keyspace and account for the write in its
/// cursor (the local sequence moved by this batch's two operations), as if
/// the receiver's own writer had made it. The stage reopens as before — its
/// cursor is consistent, its files are RocksDB's — so only a comparison of
/// the keyspace can tell the standby is no longer its primary's copy.
fn tamper(path: &Path, key: &[u8], value: &[u8]) {
    let mut options = Options::default();
    options.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(5));
    let db = DB::open_cf(&options, path, ["default", LOCAL_CF]).unwrap();
    let local = db.cf_handle(LOCAL_CF).unwrap();
    let stored = db.get_cf(local, CURSOR_KEY).unwrap().unwrap();
    let mut cursor: serde_json::Value = serde_json::from_slice(&stored).unwrap();
    let latest = db.latest_sequence_number();
    assert_eq!(
        cursor["local_sequence"].as_u64(),
        Some(latest),
        "precondition: an accounted store: {cursor}"
    );
    cursor["local_sequence"] = serde_json::json!(latest + 2);
    let mut batch = rocksdb::WriteBatch::default();
    batch.put(key, value);
    batch.put_cf(local, CURSOR_KEY, serde_json::to_vec(&cursor).unwrap());
    db.write(batch).unwrap();
    assert_eq!(db.latest_sequence_number(), latest + 2);
    decode_cursor(&db.get_cf(local, CURSOR_KEY).unwrap().unwrap()).unwrap();
    db.flush().unwrap();
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

/// A primary with `rows` rows and its standby, which followed it to the last
/// of them and is stopped. Returns the primary, its client and the standby's
/// directory.
async fn stopped_standby(pair: &Pair, rows: i32) -> (Daemon, tokio_postgres::Client, PathBuf) {
    let dir = pair.dir.path();
    let primary = Daemon::spawn(
        dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        None,
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    bulk_insert(&writer, 1..=rows, &"b".repeat(200)).await;
    let standby_dir = dir.join("standby");
    let mut standby = Daemon::spawn(dir, "standby", &standby_dir, &pair.token, &pair.standby_args, None);
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;
    let want: Vec<i32> = (1..=rows).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(60)).await, Ok(want));
    let through = primary_sequence(&writer).await;
    wait_listed(pair.http_p, through, Duration::from_secs(60), &primary).await;
    standby.stop();
    (primary, writer, standby_dir)
}

/// An identical pair: exit 0 with the tail applied, then exit 0 at the cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identical_standby_is_verified_at_the_primarys_cut() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let (_primary, writer, standby_dir) = stopped_standby(&pair, 2_000).await;
    // Written while the standby is stopped: the verification must bring it
    // to the primary's cut through the tail before it compares.
    assert_eq!(insert(&writer, 2_001..=2_100, "while stopped").await, 0);
    let before = cursor_of(&standby_dir);
    let through = primary_sequence(&writer).await;
    assert!(
        before < through,
        "precondition: the standby is behind ({before} < {through})"
    );

    let output = verify(&pair, &standby_dir, &["--json"]).await;
    assert!(output.status.success(), "{}{}", stdout(&output), stderr(&output));
    let verdict = json(&output);
    assert_eq!(verdict["result"], "identical", "{verdict}");
    assert_eq!(verdict["from"].as_u64(), Some(before), "{verdict}");
    let cut = verdict["sequence"].as_u64().unwrap();
    assert!(cut >= through, "the cut is the primary's sequence: {verdict}");
    assert!(
        verdict["tailed_groups"].as_u64().is_some_and(|groups| groups > 0),
        "{verdict}"
    );
    assert!(
        stderr(&output).contains("as a start would apply them"),
        "with --json the progress goes to stderr: {}",
        stderr(&output)
    );
    assert_eq!(
        cursor_of(&standby_dir),
        cut,
        "the standby was brought to the cut, and no further"
    );
    // The primary wrote nothing since: its store is the cut, byte for byte.
    let (primary_now, primary_digest) = default_digest(&dir.join("primary"));
    assert_eq!(
        primary_now, cut,
        "precondition: the primary wrote nothing since its cut"
    );
    assert_eq!(
        default_digest(&standby_dir).1,
        primary_digest,
        "the verdict agrees with a direct digest of both stores"
    );

    // Again, the primary idle: its new cut is the standby's cursor itself (a
    // receiver AT the cut), and nothing is applied.
    let output = verify(&pair, &standby_dir, &[]).await;
    let report = stdout(&output);
    assert!(output.status.success(), "{report}{}", stderr(&output));
    assert!(
        report.contains(&format!("identical at sequence {} (", format_count(cut))),
        "{report}"
    );
    assert!(report.contains("nothing applied"), "{report}");
    assert_eq!(cursor_of(&standby_dir), cut);

    // The verification left a standby that starts and follows as before.
    assert_eq!(insert(&writer, 2_101..=2_110, "after").await, 0);
    let standby = Daemon::spawn(&dir, "standby-2", &standby_dir, &pair.token, &pair.standby_args, None);
    let want: Vec<i32> = (1..=2_110).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(90)).await, Ok(want));
    assert!(
        !standby.log().contains("Bootstrapping physical standby"),
        "{}",
        standby.tail(20)
    );
}

/// A standby altered behind replication's back differs in exactly the leaf
/// holding the altered key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_standby_altered_outside_replication_differs_in_the_leaf_it_names() {
    let pair = pair();
    let (_primary, writer, standby_dir) = stopped_standby(&pair, 2_000).await;
    let key = b"data:t:999999".to_vec();
    tamper(&standby_dir, &key, b"not the primary's row");

    let output = verify(&pair, &standby_dir, &["--json"]).await;
    assert_eq!(output.status.code(), Some(1), "{}{}", stdout(&output), stderr(&output));
    let verdict = json(&output);
    assert_eq!(verdict["result"], "different", "{verdict}");
    assert_eq!(
        verdict["differing_leaves"], 1,
        "exactly one leaf holds the key: {verdict}"
    );
    let leaf = &verdict["differing"][0];
    let start = hex::decode(leaf["start"].as_str().unwrap()).unwrap();
    let end = hex::decode(leaf["end"].as_str().unwrap()).unwrap();
    assert!(
        start <= key && key < end,
        "the named range holds the altered key: {leaf}"
    );
    assert_ne!(leaf["primary"]["sha256"], leaf["standby"]["sha256"], "{leaf}");
    assert_eq!(
        leaf["standby"]["keys"].as_u64(),
        leaf["primary"]["keys"].as_u64().map(|keys| keys + 1),
        "the standby holds one key more there: {leaf}"
    );

    // The text verdict names it too.
    let output = verify(&pair, &standby_dir, &[]).await;
    let report = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "{report}{}", stderr(&output));
    assert!(report.contains("DIFFERENT at sequence"), "{report}");
    assert!(report.contains(&format!("leaf {} differs", leaf["index"])), "{report}");
    assert!(report.contains("recover --from-primary --full --replace"), "{report}");
    drop(writer);
}

/// A running standby is refused, untouched, and keeps running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_standby_is_refused() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let _primary = Daemon::spawn(
        &dir,
        "primary",
        &dir.join("primary"),
        &pair.token,
        &pair.primary_args,
        None,
    );
    let writer = connect(pair.pg_p).await;
    create_table(&writer).await;
    assert_eq!(insert(&writer, 1..=10, "row").await, 0);
    let standby_dir = dir.join("standby");
    let mut standby = Daemon::spawn(&dir, "standby", &standby_dir, &pair.token, &pair.standby_args, None);
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;

    let output = verify(&pair, &standby_dir, &[]).await;
    let report = stdout(&output);
    assert_eq!(output.status.code(), Some(2), "{report}{}", stderr(&output));
    assert!(report.contains("is running"), "{report}");
    assert!(report.contains("holds its LOCK"), "{report}");
    assert!(standby.alive(), "the running standby is left alone");
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(30), &standby).await;
    assert_eq!(insert(&writer, 11..=12, "row").await, 0);
    let want: Vec<i32> = (1..=12).collect();
    assert_eq!(settle(pair.pg_s, &want, Duration::from_secs(30)).await, Ok(want));
}

/// A primary that no longer holds the standby's next write group is refused
/// with the command that recovers the standby; nothing was applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_primary_that_pruned_the_tail_is_refused_with_the_recover_command() {
    let pair = pair();
    let (_primary, writer, _last) = standby_away_past_retention(&pair, false).await;
    let standby_dir = pair.dir.path().join("standby");
    let before = cursor_of(&standby_dir);

    let output = verify(&pair, &standby_dir, &[]).await;
    let report = stdout(&output);
    assert_eq!(output.status.code(), Some(2), "{report}{}", stderr(&output));
    assert!(
        report.contains("no longer serves this standby's next write group"),
        "{report}"
    );
    assert!(
        report.contains(&format!(
            "recover --from-primary --data-dir {}",
            std::fs::canonicalize(&standby_dir).unwrap().display()
        )),
        "{report}"
    );
    assert_eq!(cursor_of(&standby_dir), before, "nothing was applied");

    let output = verify(&pair, &standby_dir, &["--json"]).await;
    assert_eq!(output.status.code(), Some(2));
    let verdict = json(&output);
    assert_eq!(verdict["result"], "not_verified", "{verdict}");
    drop(writer);
}
