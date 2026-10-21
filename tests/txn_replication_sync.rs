//! Explicit-transaction replication tests (ROADMAP_V5.md §1.8; spec P2.1).
//!
//! A sync/semi-sync COMMIT must BLOCK until a standby acknowledges the
//! PHYSICAL sequence that covers it. Before §1.8 an explicit transaction
//! appended nothing loggable, so there was nothing to wait on; before spec
//! P2.1 the wait was on a LOGICAL LSN the standby acknowledged before applying
//! anything. Now the commit path reads the RocksDB sequence right after its
//! data write and `HAStateRegistry::wait_for_sequence` waits until the
//! standby registry says enough standbys hold it.
//!
//! The registry entry here is the one a replication session registers from a
//! standby's `Hello` (`HAStateRegistry::register_standby`), acknowledged from
//! a thread as a heartbeat would be. No network, no daemon: what is under
//! test is the commit path's wait, on the primary.
//!
//! # Global-state warning
//!
//! `ha_state()` is a process-wide singleton and this test mutates its role,
//! sync mode, history, standby registry and serving flag. It restores the
//! default state on the way out, but restoring afterwards does nothing for
//! tests that race it *during* the window — and tests inside one binary run
//! in parallel. **This is why this file is its own integration-test target**,
//! with ONE state-mutating test: a separate target is a separate process, so
//! the singleton it mutates is its own (with the role flipped to Primary and
//! a standby registered, every commit of a neighbouring test would wait for
//! acknowledgements). Do not add a second state-mutating test here without
//! serializing the two — there is no `serial_test` harness in this repo.
//!
//! Note `ha-tier1` is a DEFAULT feature, so this target builds and runs under a
//! plain `cargo test --tests`; the `#![cfg]` below is for `--no-default-features`
//! builds, not an opt-in.

#![cfg(feature = "ha-tier1")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};
use uuid::Uuid;

use heliosdb_nano::replication::ha_state::{
    ha_state, HARole, NodeConfig, StandbyRegistration, StandbyState, SyncMode, SyncTimeoutAction,
};
use heliosdb_nano::{Config, EmbeddedDatabase};

/// How long the mock standby waits before acknowledging. Long enough to be
/// unambiguous against scheduler noise, short enough not to slow the suite.
const ACK_DELAY_MS: u64 = 250;

/// Lower bound asserted on the commit duration. Deliberately below
/// `ACK_DELAY_MS` so a slow-to-start ACK thread cannot make this flaky, while
/// still being far above the sub-millisecond cost of a commit that did not
/// wait at all. The acker is started immediately before `COMMIT`, not before
/// `BEGIN`, so work preceding the measured window cannot eat the delay
/// (observed 2026-08-16: a loaded host ran `BEGIN`/`INSERT` past the delay
/// and COMMIT correctly returned at once).
const MIN_OBSERVED_WAIT_MS: u64 = 150;

#[test]
fn semi_sync_commit_waits_for_the_standby_to_acknowledge_its_physical_sequence() {
    let mut config = Config::in_memory();
    config.storage.wal_enabled = true;
    let db = EmbeddedDatabase::with_config(config).expect("in-memory db with logical WAL");
    db.execute("CREATE TABLE sync_txn (id INT PRIMARY KEY, v INT)")
        .expect("create table");

    // Arm HA only now, so the DDL above never waited on a standby: a
    // semi-sync primary that serves its replication port, its dataset, and
    // one standby of that dataset, durable at the primary's current sequence.
    let history = Uuid::new_v4();
    ha_state().set_config(NodeConfig {
        role: HARole::Primary,
        sync_mode: SyncMode::SemiSync,
        min_acks: 1,
        sync_timeout: Duration::from_secs(30),
        sync_timeout_action: SyncTimeoutAction::Warn,
        ..NodeConfig::default()
    });
    ha_state().set_history(Some(history));
    let before = db.storage.db().latest_sequence_number();
    let standby = ha_state().register_standby(StandbyRegistration {
        node_id: Uuid::new_v4(),
        address: "127.0.0.1:0".to_string(),
        history: Some(history),
        pg_port: 0,
        sync_mode: SyncMode::SemiSync,
        cursor: Some(before),
        state: StandbyState::Streaming,
        counts_for_sync: true,
    });
    ha_state().set_replication_serving(true);

    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO sync_txn (id, v) VALUES (1, 10)")
        .expect("staged insert");

    // Mock standby: acknowledges everything — as a heartbeat would — but
    // only after a delay. SPAWNED HERE, NOT EARLIER (see MIN_OBSERVED_WAIT_MS).
    let acker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ACK_DELAY_MS));
        standby.record_ack(u64::MAX >> 8, u64::MAX >> 8);
        standby
    });

    let started = Instant::now();
    let commit = db.execute("COMMIT");
    let elapsed = started.elapsed();
    let after = db.storage.db().latest_sequence_number();

    let standby = acker.join().expect("ack thread");
    // Restore global state before asserting, so a failure here does not leave
    // the rest of this binary running as a Primary with a phantom standby.
    ha_state().set_replication_serving(false);
    drop(standby);
    ha_state().set_history(None);
    ha_state().set_config(NodeConfig::default());

    commit.expect("commit");
    assert!(
        after > before,
        "the COMMIT wrote: its physical sequence moved {before} -> {after}"
    );
    assert!(
        elapsed >= Duration::from_millis(MIN_OBSERVED_WAIT_MS),
        "semi-sync COMMIT returned in {elapsed:?} — it did not wait for the standby to acknowledge physical \
         sequence {after}"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the acknowledgement released it, not the timeout: {elapsed:?}"
    );
}
