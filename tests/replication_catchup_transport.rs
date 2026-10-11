//! Durable catch-up over the native transport (4.41.1 replication step 1).
//!
//! The primary plans catch-up from DURABLE history (`CatchupSource`), never from the
//! broadcast channel. These tests pin the three outcomes a standby can see:
//!   * entries it lacks arrive in order, then it reports CAUGHT UP only once its own
//!     durable cursor reaches the barrier the primary announced;
//!   * unavailable history is a TERMINAL "full resync required" refusal, never an
//!     empty successful batch;
//!   * a standby already current gets an immediate barrier.

#![cfg(feature = "ha-tier1")]

use heliosdb_nano::replication::{
    history::ReceiverHistory,
    streaming::{
        CatchupPlan, CatchupSource, StreamingClient, StreamingClientConfig, StreamingClientStatus, StreamingServer,
        StreamingServerConfig, FULL_RESYNC_REQUIRED_MESSAGE,
    },
    transport::SyncModeConfig,
    wal_replicator::{WalEntry, WalEntryType},
    wal_store::{WalStore, WalStoreConfig},
    Result,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::timeout;
use uuid::Uuid;

const LIMIT: Duration = Duration::from_secs(10);

/// The HA broadcast channel is process-global; servers must not overlap.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixed(Box<dyn Fn(u64) -> Result<CatchupPlan> + Send + Sync>);
impl CatchupSource for Fixed {
    fn plan(&self, after: u64) -> Result<CatchupPlan> {
        (self.0)(after)
    }
}

fn entry(lsn: u64) -> WalEntry {
    WalEntry {
        lsn,
        tx_id: None,
        entry_type: WalEntryType::Insert,
        data: vec![lsn as u8],
        checksum: 0,
    }
}

struct Primary {
    addr: SocketAddr,
    server: Arc<StreamingServer>,
    _dir: tempfile::TempDir,
}

async fn primary(source: Fixed) -> Primary {
    let dir = tempfile::tempdir().unwrap();
    let wal_store = Arc::new(WalStore::new(WalStoreConfig {
        wal_dir: dir.path().join("wal"),
        ..Default::default()
    }));
    wal_store.init().await.unwrap();
    let server = Arc::new(
        StreamingServer::new(
            StreamingServerConfig {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                ..Default::default()
            },
            Uuid::new_v4(),
            Uuid::new_v4(),
            wal_store,
        )
        .unwrap()
        .with_catchup_source(Arc::new(source)),
    );
    let listener = server.bind_listener().await.unwrap();
    let addr = listener.local_addr().unwrap();
    let running = server.clone();
    tokio::spawn(async move { running.start_with_listener(listener).await });
    Primary {
        addr,
        server,
        _dir: dir,
    }
}

fn client(addr: SocketAddr) -> (Arc<StreamingClient>, tokio::sync::mpsc::Receiver<WalEntry>) {
    let (client, entries) = StreamingClient::new(
        StreamingClientConfig {
            node_id: Uuid::new_v4(),
            primary_addr: addr,
            sync_mode: SyncModeConfig::Async,
            connect_timeout: Duration::from_secs(2),
            reconnect_delay: Duration::from_millis(50),
            max_reconnect_attempts: 0,
            auth: None,
        },
        Arc::new(ReceiverHistory::in_memory()),
    );
    (Arc::new(client), entries)
}

#[tokio::test]
async fn pruned_history_is_a_terminal_full_resync_refusal() {
    let _serial = SERIAL.lock().await;
    let p = primary(Fixed(Box::new(|after| {
        Ok(CatchupPlan::ResyncRequired(format!(
            "history before LSN 100 removed (standby at {after})"
        )))
    })))
    .await;
    let (client, mut entries) = client(p.addr);
    let status = client.subscribe_status();
    let result = timeout(LIMIT, client.start()).await.expect("must not retry forever");
    let error = result.expect_err("a standby that cannot be served must fail closed");
    assert!(error.to_string().contains(FULL_RESYNC_REQUIRED_MESSAGE), "got: {error}");
    assert!(
        matches!(&*status.borrow(), StreamingClientStatus::Failed { error } if error.contains(FULL_RESYNC_REQUIRED_MESSAGE))
    );
    assert!(
        entries.try_recv().is_err(),
        "no entry may be delivered with the refusal"
    );
    p.server.shutdown();
}

#[tokio::test]
async fn missing_entries_arrive_in_order_and_caught_up_waits_for_the_durable_cursor() {
    let _serial = SERIAL.lock().await;
    let p = primary(Fixed(Box::new(|after| {
        Ok(CatchupPlan::Entries {
            latest: 4,
            entries: (after + 1..=4).map(entry).collect(),
        })
    })))
    .await;
    let (client, mut entries) = client(p.addr);
    let cursor = Arc::new(AtomicU64::new(1)); // the standby already holds LSN 1
    let source = cursor.clone();
    client.set_applied_source(Arc::new(move || source.load(Ordering::SeqCst)));
    let mut status = client.subscribe_status();
    let running = client.clone();
    let task = tokio::spawn(async move { running.start().await });

    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(timeout(LIMIT, entries.recv()).await.unwrap().unwrap().lsn);
    }
    assert_eq!(seen, vec![2, 3, 4], "exactly what the standby lacked, oldest first");

    // Received is not applied: still not caught up.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !matches!(&*status.borrow(), StreamingClientStatus::CaughtUp { .. }),
        "caught up must wait for the durable cursor, not for receipt"
    );

    cursor.store(4, Ordering::SeqCst);
    timeout(LIMIT, async {
        loop {
            if matches!(&*status.borrow_and_update(), StreamingClientStatus::CaughtUp { .. }) {
                break;
            }
            status.changed().await.unwrap();
        }
    })
    .await
    .expect("caught up once the cursor reaches the barrier");

    client.shutdown();
    let _ = timeout(LIMIT, task).await;
    p.server.shutdown();
}

#[tokio::test]
async fn already_current_standby_gets_an_immediate_barrier() {
    let _serial = SERIAL.lock().await;
    let p = primary(Fixed(Box::new(|_| {
        Ok(CatchupPlan::Entries {
            latest: 7,
            entries: vec![],
        })
    })))
    .await;
    let (client, _entries) = client(p.addr);
    client.set_applied_source(Arc::new(|| 7));
    let mut status = client.subscribe_status();
    let running = client.clone();
    let task = tokio::spawn(async move { running.start().await });
    timeout(LIMIT, async {
        loop {
            if matches!(&*status.borrow_and_update(), StreamingClientStatus::CaughtUp { .. }) {
                break;
            }
            status.changed().await.unwrap();
        }
    })
    .await
    .expect("an empty catch-up still announces a barrier");
    client.shutdown();
    let _ = timeout(LIMIT, task).await;
    p.server.shutdown();
}
