//! Explicit-transaction replication tests (ROADMAP_V5.md §1.8).
//!
//! Writes staged inside an explicit or session transaction never reached the
//! logical WAL: `skip_fast_paths` suppresses the per-statement
//! `log_data_insert`/`log_data_update`/`log_data_delete` calls, and
//! `Transaction::commit_with_timestamp` wrote straight to RocksDB. So an HA
//! standby silently missed every transactional write. `commit_with_timestamp`
//! now emits the transaction's operations through `WriteAheadLog::append_batch`
//! before the RocksDB batch write.
//!
//! # Scope of this file
//!
//! The core correctness of that fix — parity with autocommit, ROLLBACK,
//! ROLLBACK TO SAVEPOINT, cascading FK actions, branch exclusion, DDL
//! non-duplication, row counters, and replica convergence through
//! `apply_replicated_operation` — is covered by crate-internal unit tests in
//! `src/lib.rs` (they need `wal_entries_for_tests()`, which is `pub(crate)`).
//! Those run under `cargo test --lib`, need no network and no standby.
//!
//! What is left here is the one property that genuinely needs the HA state
//! registry: that a sync/semi-sync COMMIT actually BLOCKS on a standby ACK.
//! Before the fix it could not have — the commit appended nothing, so there
//! was no LSN to wait on, and "synchronous replication" was silently a no-op
//! for every transaction.
//!
//! # Global-state warning
//!
//! `ha_state()` is a process-wide singleton and this test mutates its role,
//! sync mode, standby registry and broadcast channel. It restores the default
//! config on the way out, but restoring afterwards does nothing for tests that
//! race it *during* the window — and tests inside one binary run in parallel.
//!
//! **This is why this file is its own integration-test target rather than a
//! module under `tests/ha_tests/`.** It was originally `ha_tests::
//! txn_replication_tests`, and in that position it poisoned the shared
//! `ha_state()` for its neighbours: with the role flipped to Primary, a
//! SemiSync mode set and a mock standby registered, `cluster_tests::
//! test_wal_entry_broadcasting` (and two siblings) broadcast and then blocked
//! waiting on an ACK from a standby that had stopped answering, so the whole
//! suite stopped completing (4748 passed before, 2941 and a SIGTERM after).
//! A separate target means a separate process, so the singleton it mutates is
//! its own. **Do not move this back into `ha_tests/`**, and do not add a second
//! state-mutating test to this file without serializing the two — there is no
//! `serial_test` harness in this repo.
//!
//! Note `ha-tier1` is a DEFAULT feature, so this target builds and runs under a
//! plain `cargo test --tests`; the `#![cfg]` below is for `--no-default-features`
//! builds, not an opt-in.

#![cfg(feature = "ha-tier1")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};
use uuid::Uuid;

use heliosdb_nano::replication::ha_state::{ha_state, HARole, NodeConfig, StandbyInfo, StandbyState, SyncMode};
use heliosdb_nano::{Config, EmbeddedDatabase};

/// How long the mock standby waits before acknowledging. Long enough to be
/// unambiguous against `wait_for_sync`'s 10 ms poll interval and scheduler
/// noise, short enough not to slow the suite.
const ACK_DELAY_MS: u64 = 250;

/// Lower bound asserted on the commit duration. Deliberately below
/// `ACK_DELAY_MS` so a slow-to-start ACK thread cannot make this flaky, while
/// still being far above the sub-millisecond cost of a commit that did not
/// wait at all.
const MIN_OBSERVED_WAIT_MS: u64 = 150;

fn mock_standby(node_id: Uuid) -> StandbyInfo {
    StandbyInfo {
        node_id,
        address: "127.0.0.1:0".to_string(),
        connected_at: 0,
        last_heartbeat: 0,
        sync_mode: SyncMode::SemiSync,
        current_lsn: 0,
        flush_lsn: 0,
        apply_lsn: 0,
        lag_bytes: 0,
        lag_ms: 0,
        state: StandbyState::Streaming,
    }
}

/// A semi-sync COMMIT must block until a standby acknowledges the
/// transaction's WAL entries.
///
/// Sequencing matters: the schema is created while this node is still
/// `Standalone`, so the DDL's own WAL append does not broadcast or wait. HA is
/// armed only afterwards, and `BEGIN`/`INSERT` append nothing inside a
/// transaction, so the COMMIT's `append_batch` is the first — and only —
/// broadcast in the measured window.
#[test]
fn sync_mode_commit_waits_for_standby_ack() {
    let mut config = Config::in_memory();
    config.storage.wal_enabled = true;
    let db = EmbeddedDatabase::with_config(config).expect("in-memory db with logical WAL");
    db.execute("CREATE TABLE sync_txn (id INT PRIMARY KEY, v INT)")
        .expect("create table");

    // Arm HA only now, so the DDL above never waited on a standby.
    // The receiver must stay alive: `broadcast::Sender::send` fails with no
    // subscribers, and `broadcast_wal_entry` then reports "not replicated",
    // which would skip `wait_for_sync` entirely and make this test vacuous.
    let (wal_tx, _wal_rx) = tokio::sync::broadcast::channel(256);
    ha_state().set_wal_broadcast(wal_tx);
    ha_state().set_config(NodeConfig {
        role: HARole::Primary,
        sync_mode: SyncMode::SemiSync,
        ..NodeConfig::default()
    });
    let standby_id = Uuid::new_v4();
    ha_state().register_standby(mock_standby(standby_id));

    // Mock standby: acknowledges everything, but only after a delay.
    let acker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ACK_DELAY_MS));
        ha_state().update_standby(standby_id, |standby| {
            standby.flush_lsn = u64::MAX;
            standby.apply_lsn = u64::MAX;
        });
    });

    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO sync_txn (id, v) VALUES (1, 10)")
        .expect("staged insert");
    let started = Instant::now();
    let commit = db.execute("COMMIT");
    let elapsed = started.elapsed();

    acker.join().expect("ack thread");
    // Restore global state before asserting, so a failure here does not leave
    // the rest of this binary running as a Primary with a phantom standby.
    ha_state().remove_standby(standby_id);
    ha_state().clear_wal_broadcast();
    ha_state().set_config(NodeConfig::default());

    commit.expect("commit");
    assert!(
        elapsed >= Duration::from_millis(MIN_OBSERVED_WAIT_MS),
        "semi-sync COMMIT returned in {elapsed:?} — it did not wait for the standby ACK, \
         so the transaction's writes were either never appended to the logical WAL or never broadcast"
    );
}
