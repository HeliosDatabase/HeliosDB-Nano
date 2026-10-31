//! Sprinter a3ffb9ef3c2c (spec R1 §4): explicit multi-statement transactions
//! on the primary reach a physical standby, on real managed daemons
//! (`tests/support/physical_pair.rs`).
//!
//! The defect this item tracked was the LOGICAL standby's: an explicit
//! transaction emitted no logical WAL, so a standby never saw it. A physical
//! standby replicates the primary's RocksDB write groups, whatever API made
//! them, so the question is now only whether every interface's transactions
//! commit as write groups the standby applies. Over the PostgreSQL wire, both
//! protocols:
//!
//! * simple protocol (`batch_execute`): `BEGIN … COMMIT` with an insert, an
//!   update and a delete; `BEGIN … ROLLBACK`; a `SAVEPOINT` whose work is
//!   undone by `ROLLBACK TO SAVEPOINT` before the transaction commits;
//! * extended protocol (`transaction()`, bound parameters): a committed
//!   transaction, a rolled-back one, and a savepoint rolled back inside a
//!   committed one;
//!
//! then the standby holds exactly the committed rows, and the `range_digest`
//! of its default column family equals the primary's at the same sequence.

#![cfg(all(feature = "ha-tier1", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/physical_pair.rs"]
mod physical_pair;

use heliosdb_nano::replication::physical::{decode_cursor, range_digest, CURSOR_KEY, LOCAL_CF};
use physical_pair::*;
use rocksdb::{Options, DB};
use std::path::Path;
use std::time::{Duration, Instant};

/// A stopped standby's durable cursor (its source sequence), read offline.
fn standby_cursor(path: &Path) -> u64 {
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

/// Poll the standby's `/health` until its cursor reaches `through`.
async fn follow(http_port: u16, through: u64, daemon: &Daemon) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some((_, health)) = http(http_port, "/health").await {
            let cursor = health["replication"]["cursor"].as_str().map(lsn);
            if cursor.is_some_and(|cursor| cursor >= through) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the standby never reached {through}; log tail: {}",
            daemon.tail(12)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_transactions_over_both_protocols_reach_the_standby_byte_for_byte() {
    let pair = pair();
    let dir = pair.dir.path().to_path_buf();
    let primary_data = dir.join("primary");
    let primary = Daemon::spawn(&dir, "primary", &primary_data, &pair.token, &pair.primary_args, None);
    let simple = connect(pair.pg_p).await;
    create_table(&simple).await;
    let standby_data = dir.join("standby");
    let mut standby = Daemon::spawn(&dir, "standby", &standby_data, &pair.token, &pair.standby_args, None);
    wait_http(pair.http_s, "/ready", 200, Duration::from_secs(90), &standby).await;

    // Simple protocol.
    simple
        .batch_execute(
            "BEGIN; INSERT INTO t VALUES (1, 'a'); INSERT INTO t VALUES (2, 'b'); \
             UPDATE t SET v = 'a2' WHERE id = 1; INSERT INTO t VALUES (3, 'c'); DELETE FROM t WHERE id = 3; COMMIT",
        )
        .await
        .unwrap();
    simple
        .batch_execute("BEGIN; INSERT INTO t VALUES (4, 'rolled back'); UPDATE t SET v = 'no' WHERE id = 1; ROLLBACK")
        .await
        .unwrap();
    simple
        .batch_execute(
            "BEGIN; INSERT INTO t VALUES (5, 'kept'); SAVEPOINT s1; INSERT INTO t VALUES (6, 'undone'); \
             UPDATE t SET v = 'undone' WHERE id = 2; ROLLBACK TO SAVEPOINT s1; INSERT INTO t VALUES (7, 'after'); \
             RELEASE SAVEPOINT s1; COMMIT",
        )
        .await
        .unwrap();

    // Extended protocol, bound parameters.
    let mut extended = connect(pair.pg_p).await;
    {
        let transaction = extended.transaction().await.unwrap();
        transaction
            .execute("INSERT INTO t VALUES ($1, $2)", &[&8_i32, &"bound"])
            .await
            .unwrap();
        transaction
            .execute("UPDATE t SET v = $1 WHERE id = $2", &[&"b2", &2_i32])
            .await
            .unwrap();
        transaction
            .execute("DELETE FROM t WHERE id = $1", &[&5_i32])
            .await
            .unwrap();
        transaction.commit().await.unwrap();
    }
    {
        let transaction = extended.transaction().await.unwrap();
        transaction
            .execute("INSERT INTO t VALUES ($1, $2)", &[&9_i32, &"rolled back"])
            .await
            .unwrap();
        transaction.rollback().await.unwrap();
    }
    {
        let mut transaction = extended.transaction().await.unwrap();
        transaction
            .execute("INSERT INTO t VALUES ($1, $2)", &[&10_i32, &"kept"])
            .await
            .unwrap();
        {
            let savepoint = transaction.savepoint("sp").await.unwrap();
            savepoint
                .execute("INSERT INTO t VALUES ($1, $2)", &[&11_i32, &"undone"])
                .await
                .unwrap();
            savepoint.rollback().await.unwrap();
        }
        transaction
            .execute("INSERT INTO t VALUES ($1, $2)", &[&12_i32, &"after"])
            .await
            .unwrap();
        transaction.commit().await.unwrap();
    }

    let committed = vec![1, 2, 7, 8, 10, 12];
    assert_eq!(
        ids(&simple).await,
        Ok(committed.clone()),
        "the primary's committed rows"
    );
    let through = primary_sequence(&simple).await;
    follow(pair.http_s, through, &standby).await;
    assert_eq!(
        settle(pair.pg_s, &committed, Duration::from_secs(30)).await,
        Ok(committed.clone()),
        "the standby holds exactly the committed rows"
    );
    let reader = connect(pair.pg_s).await;
    assert_eq!(
        value(&reader, "SELECT v FROM t WHERE id = 1").await.as_deref(),
        Some("a2")
    );
    assert_eq!(
        value(&reader, "SELECT v FROM t WHERE id = 2").await.as_deref(),
        Some("b2")
    );
    drop(reader);

    // Byte identity at the same sequence.
    standby.stop();
    let (primary_now, primary_digest) = default_digest(&primary_data);
    assert_eq!(primary_now, through, "precondition: the primary wrote nothing since");
    assert_eq!(standby_cursor(&standby_data), through);
    assert_eq!(
        default_digest(&standby_data).1,
        primary_digest,
        "the standby's default column family is the primary's at {through}"
    );
    drop(simple);
    drop(extended);
    drop(primary);
}
