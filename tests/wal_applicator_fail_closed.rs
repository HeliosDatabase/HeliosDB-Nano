//! A rejected WAL entry is a terminal gap, never permission to apply later LSNs.
#![cfg(feature = "ha-tier1")]

use heliosdb_nano::{
    replication::{
        config::PrimaryConfig,
        wal_applicator::{ApplicatorState, WalApplicator},
        wal_replicator::{WalEntry, WalEntryType},
    },
    storage::{WalOperation, WalSyncMode, WriteAheadLog},
    Config, EmbeddedDatabase,
};
use std::time::Duration;
use tokio::time::timeout;

fn database() -> EmbeddedDatabase {
    let mut config = Config::in_memory();
    config.storage.wal_enabled = true;
    config.storage.logical_wal_per_statement = true;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE applicator_rows (id INT PRIMARY KEY, v INT)")
        .unwrap();
    db
}

fn capture_insert(primary: &EmbeddedDatabase, id: i32) -> WalOperation {
    let before = primary.storage.wal_lsn().unwrap();
    primary
        .execute(&format!("INSERT INTO applicator_rows VALUES ({id}, {})", id * 10))
        .unwrap();
    // Async open/replay is read-only: no group-commit worker, append, or Drop write.
    WriteAheadLog::open(primary.storage.db(), WalSyncMode::Async)
        .unwrap()
        .replay_from(before)
        .unwrap()
        .into_iter()
        .find_map(|entry| match entry.operation {
            operation @ WalOperation::Insert { .. } => Some(operation),
            _ => None,
        })
        .expect("SQL fixture must emit an actual Insert WAL operation")
}

fn entry(lsn: u64, operation: &WalOperation) -> WalEntry {
    WalEntry {
        lsn,
        tx_id: None,
        entry_type: WalEntryType::Insert,
        data: bincode::serialize(operation).unwrap(),
        checksum: 0,
    }
}

fn applicator() -> WalApplicator {
    WalApplicator::new(PrimaryConfig {
        host: "127.0.0.1".to_string(),
        port: 1, // no connection is made: these tests drive the owned queue directly
        connect_timeout: Duration::from_secs(1),
        use_tls: false,
    })
}

async fn assert_terminal_gap(malformed_serialization: bool) {
    let primary = database();
    let standby = database();
    let first = entry(1, &capture_insert(&primary, 1));
    let later = entry(3, &capture_insert(&primary, 3));
    let mut failing = entry(
        2,
        &WalOperation::Insert {
            table: "applicator_rows".to_string(),
            // This is valid serialized WAL but an invalid target key, rejected
            // by StorageEngine's live-replay validation before mutating rows.
            key: b"data:another_table:1".to_vec(),
            tuple: vec![],
        },
    );
    if malformed_serialization {
        failing.data = vec![0xff];
    }
    let applicator = applicator();
    // Queue all three before starting, so the successor is definitely pending
    // when the middle entry fails rather than merely refused by a closed queue.
    applicator.queue_entry(first).await.unwrap();
    applicator.queue_entry(failing).await.unwrap();
    applicator.queue_entry(later.clone()).await.unwrap();
    let sender = applicator.get_queue_sender();
    applicator.start_with_storage(standby.storage.clone()).await.unwrap();
    timeout(Duration::from_secs(3), sender.closed())
        .await
        .expect("failure must close the incoming queue promptly");

    assert_eq!(applicator.state().await, ApplicatorState::Error);
    assert_eq!(applicator.applied_lsn().await, 1);
    assert_eq!(applicator.lag(3).await, 2);
    assert_eq!(applicator.stats().await, (1, 0, 1));
    let rows = standby.storage.scan_table("applicator_rows").unwrap();
    assert_eq!(rows.len(), 1, "a later entry must never cross the failed LSN");
    assert_eq!(
        rows[0].values.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["1", "10"]
    );
    assert!(sender.is_closed());
    assert!(sender.send(later.clone()).await.is_err());
    assert!(applicator.queue_entry(later.clone()).await.is_err());

    assert!(applicator.pause().await.is_err());
    assert!(applicator.resume().await.is_err());
    assert!(applicator.promote().await.is_err());
    assert!(applicator.start().await.is_err());
    assert!(applicator.start_with_storage(standby.storage.clone()).await.is_err());
    assert!(applicator.apply(later).await.is_err());
    applicator.stop().await.unwrap();
    assert_eq!(
        applicator.state().await,
        ApplicatorState::Error,
        "stop must preserve the failed state"
    );
    assert_eq!(applicator.applied_lsn().await, 1);
    assert!(applicator.resume().await.is_err());
    assert!(applicator.promote().await.is_err());
}

#[tokio::test]
async fn malformed_wal_closes_queue_and_poison_is_sticky() {
    assert_terminal_gap(true).await;
}

#[tokio::test]
async fn storage_rejection_closes_queue_and_poison_is_sticky() {
    assert_terminal_gap(false).await;
}

#[tokio::test]
async fn legacy_lsn_only_methods_cannot_bypass_a_storage_worker() {
    let primary = database();
    let standby = database();
    let operation = capture_insert(&primary, 1);
    let applicator = applicator();
    applicator.start_with_storage(standby.storage.clone()).await.unwrap();
    assert!(applicator.apply(entry(99, &operation)).await.is_err());
    assert!(applicator.start().await.is_err());
    assert_eq!(applicator.applied_lsn().await, 0);
    applicator.queue_entry(entry(1, &operation)).await.unwrap();
    timeout(Duration::from_secs(3), async {
        while applicator.applied_lsn().await != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("storage worker should still apply valid entries");
    assert_eq!(applicator.state().await, ApplicatorState::Streaming);
    assert_eq!(standby.storage.scan_table("applicator_rows").unwrap().len(), 1);
    applicator.stop().await.unwrap();
    timeout(Duration::from_secs(3), applicator.get_queue_sender().closed())
        .await
        .expect("stop should close the worker queue");
}
