//! Comprehensive WAL Crash Recovery Tests
//!
//! This test suite simulates various crash scenarios and verifies that the WAL
//! provides proper durability guarantees.

use heliosdb_lite::{
    Error, Result,
    storage::{
        WriteAheadLog, WalOperation, WalSyncMode,
        WalIntegrityReport, ReplayStats, CleanupStats, WalMetrics,
    },
};
use rocksdb::{DB, Options};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

/// Helper function to create a test database
fn create_test_db() -> (TempDir, Arc<DB>) {
    let temp_dir = TempDir::new().unwrap();
    let mut opts = Options::default();
    opts.create_if_missing(true);
    let db = DB::open(&opts, temp_dir.path()).unwrap();
    (temp_dir, Arc::new(db))
}

#[test]
fn test_wal_integrity_verification_clean() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Sync).unwrap();

    // Append some entries
    for i in 0..100 {
        wal.append(WalOperation::Insert {
            table: "test".to_string(),
            tuple: vec![i],
        })
        .unwrap();
    }

    // Verify integrity
    let report = wal.verify_integrity().unwrap();
    assert!(report.is_valid, "Clean WAL should pass integrity check");
    assert_eq!(report.total_entries, 100);
    assert_eq!(report.corrupted_entries.len(), 0);
    assert_eq!(report.missing_lsns.len(), 0);
    assert_eq!(report.duplicate_lsns.len(), 0);
}

#[test]
fn test_wal_integrity_verification_empty() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Sync).unwrap();

    // Verify empty WAL
    let report = wal.verify_integrity().unwrap();
    assert!(report.is_valid, "Empty WAL should be valid");
    assert_eq!(report.total_entries, 0);
}

#[test]
fn test_crash_recovery_basic() {
    let (temp, db) = create_test_db();

    // Phase 1: Write some data
    {
        let wal = WriteAheadLog::open(Arc::clone(&db), WalSyncMode::Sync).unwrap();

        for i in 0..50 {
            wal.append(WalOperation::Insert {
                table: "users".to_string(),
                tuple: vec![i],
            })
            .unwrap();
        }

        // Verify LSN is 50
        assert_eq!(wal.current_lsn(), 50);
    }

    // Phase 2: Simulate crash and recovery
    {
        let wal = WriteAheadLog::open(Arc::clone(&db), WalSyncMode::Sync).unwrap();

        // LSN should be recovered
        assert_eq!(wal.current_lsn(), 50);

        // Replay from beginning
        let stats = wal.replay_with_stats(0).unwrap();
        assert_eq!(stats.operations_replayed, 50);
        assert_eq!(stats.operations_skipped, 0);
        assert_eq!(stats.errors, 0);
        assert_eq!(stats.start_lsn, 0);
        assert_eq!(stats.end_lsn, 50);
    }
}

#[test]
fn test_crash_recovery_partial_replay() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Sync).unwrap();

    // Write 100 entries
    for i in 0..100 {
        wal.append(WalOperation::Insert {
            table: "test".to_string(),
            tuple: vec![i],
        })
        .unwrap();
    }

    // Replay only from LSN 50
    let stats = wal.replay_with_stats(50).unwrap();
    assert_eq!(stats.operations_replayed, 51); // 50-100 inclusive
    assert_eq!(stats.operations_skipped, 49); // 1-49
    assert_eq!(stats.errors, 0);
    assert_eq!(stats.start_lsn, 50);
    assert_eq!(stats.end_lsn, 100);
}

#[test]
fn test_crash_recovery_multiple_crashes() {
    let (temp, db) = create_test_db();

    // Crash 1: Write some data
    {
        let wal = WriteAheadLog::open(Arc::clone(&db), WalSyncMode::Sync).unwrap();
        for i in 0..25 {
            wal.append(WalOperation::Insert {
                table: "test".to_string(),
                tuple: vec![i],
            })
            .unwrap();
        }
    }

    // Crash 2: Recover and write more
    {
        let wal = WriteAheadLog::open(Arc::clone(&db), WalSyncMode::Sync).unwrap();
        assert_eq!(wal.current_lsn(), 25);

        for i in 0..25 {
            wal.append(WalOperation::Update {
                table: "test".to_string(),
                key: vec![i],
                tuple: vec![i + 100],
            })
            .unwrap();
        }
        assert_eq!(wal.current_lsn(), 50);
    }

    // Crash 3: Final recovery
    {
        let wal = WriteAheadLog::open(Arc::clone(&db), WalSyncMode::Sync).unwrap();
        assert_eq!(wal.current_lsn(), 50);

        // Verify replay is idempotent
        let stats1 = wal.replay_with_stats(0).unwrap();
        let stats2 = wal.replay_with_stats(0).unwrap();

        assert_eq!(stats1.operations_replayed, stats2.operations_replayed);
        assert_eq!(stats1.operations_replayed, 50);
    }
}

#[test]
fn test_wal_rotation() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Sync).unwrap();

    // Write some data
    for i in 0..100 {
        wal.append(WalOperation::Insert {
            table: "test".to_string(),
            tuple: vec![i; 1000], // Large tuples
        })
        .unwrap();
    }

    // Get metrics before rotation
    let metrics_before = wal.metrics().unwrap();
    assert_eq!(metrics_before.entry_count, 100);

    // Rotate WAL
    wal.rotate().unwrap();

    // Get metrics after rotation
    let metrics_after = wal.metrics().unwrap();
    assert_eq!(metrics_after.entry_count, 100); // Entries still there
    assert_eq!(metrics_after.current_lsn, metrics_before.current_lsn);
}

#[test]
#[ignore = "TODO: WAL cleanup by time needs verification"]
fn test_wal_cleanup_by_time() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Sync).unwrap();

    // Write some entries
    for i in 0..50 {
        wal.append(WalOperation::Insert {
            table: "test".to_string(),
            tuple: vec![i],
        })
        .unwrap();
    }

    // Sleep to ensure time passes
    thread::sleep(Duration::from_millis(100));

    // Write more entries
    for i in 50..100 {
        wal.append(WalOperation::Insert {
            table: "test".to_string(),
            tuple: vec![i],
        })
        .unwrap();
    }

    // Cleanup entries older than 10 seconds (should keep all)
    let stats = wal.cleanup_old_logs(10, 10).unwrap();
    assert_eq!(stats.entries_deleted, 0);

    // Cleanup entries older than 0 seconds, but keep at least 60
    let stats = wal.cleanup_old_logs(0, 60).unwrap();
    assert_eq!(stats.entries_deleted, 40); // Delete 40, keep 60
}

#[test]
fn test_wal_cleanup_respects_minimum() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Sync).unwrap();

    // Write 100 entries
    for i in 0..100 {
        wal.append(WalOperation::Insert {
            table: "test".to_string(),
            tuple: vec![i],
        })
        .unwrap();
    }

    // Try to cleanup all, but keep minimum 50
    let stats = wal.cleanup_old_logs(0, 50).unwrap();

    // Should keep at least 50 entries
    let metrics = wal.metrics().unwrap();
    assert!(metrics.entry_count >= 50);
}

#[test]
fn test_wal_metrics() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::GroupCommit).unwrap();

    // Initially empty
    let metrics = wal.metrics().unwrap();
    assert_eq!(metrics.entry_count, 0);
    assert_eq!(metrics.current_lsn, 0);
    assert_eq!(metrics.sync_mode, WalSyncMode::GroupCommit);

    // Write some entries
    for i in 0..50 {
        wal.append(WalOperation::Insert {
            table: "test".to_string(),
            tuple: vec![i; 100],
        })
        .unwrap();
    }

    // Check metrics
    let metrics = wal.metrics().unwrap();
    assert_eq!(metrics.entry_count, 50);
    assert_eq!(metrics.current_lsn, 50);
    assert_eq!(metrics.oldest_lsn, 1);
    assert_eq!(metrics.newest_lsn, 50);
    assert!(metrics.size_bytes > 0);
    assert!(metrics.oldest_timestamp > 0);
    assert!(metrics.newest_timestamp >= metrics.oldest_timestamp);
}

#[test]
fn test_wal_transaction_operations() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Sync).unwrap();

    // Log a complete transaction
    let tx_id = 42;

    wal.append(WalOperation::Begin { tx_id }).unwrap();
    wal.append(WalOperation::Insert {
        table: "users".to_string(),
        tuple: vec![1, 2, 3],
    })
    .unwrap();
    wal.append(WalOperation::Update {
        table: "users".to_string(),
        key: vec![1],
        tuple: vec![4, 5, 6],
    })
    .unwrap();
    wal.append(WalOperation::Commit { tx_id }).unwrap();

    // Verify all logged
    let entries = wal.replay().unwrap();
    assert_eq!(entries.len(), 4);

    match &entries[0].operation {
        WalOperation::Begin { tx_id: id } => assert_eq!(*id, 42),
        _ => panic!("Expected Begin"),
    }

    match &entries[3].operation {
        WalOperation::Commit { tx_id: id } => assert_eq!(*id, 42),
        _ => panic!("Expected Commit"),
    }
}

#[test]
fn test_wal_table_operations() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Sync).unwrap();

    // Log table operations
    wal.append(WalOperation::CreateTable {
        table: "users".to_string(),
        schema: vec![1, 2, 3],
    })
    .unwrap();

    wal.append(WalOperation::Insert {
        table: "users".to_string(),
        tuple: vec![10, 20, 30],
    })
    .unwrap();

    wal.append(WalOperation::DropTable {
        table: "users".to_string(),
    })
    .unwrap();

    // Verify operations
    let entries = wal.replay().unwrap();
    assert_eq!(entries.len(), 3);

    match &entries[0].operation {
        WalOperation::CreateTable { table, .. } => assert_eq!(table, "users"),
        _ => panic!("Expected CreateTable"),
    }

    match &entries[2].operation {
        WalOperation::DropTable { table } => assert_eq!(table, "users"),
        _ => panic!("Expected DropTable"),
    }
}

#[test]
fn test_wal_sync_mode_changes() {
    let (_temp, db) = create_test_db();
    let mut wal = WriteAheadLog::open(db, WalSyncMode::Async).unwrap();

    assert_eq!(wal.sync_mode(), WalSyncMode::Async);

    // Change to sync
    wal.set_sync_mode(WalSyncMode::Sync);
    assert_eq!(wal.sync_mode(), WalSyncMode::Sync);

    // Write with sync mode
    wal.append(WalOperation::Insert {
        table: "test".to_string(),
        tuple: vec![1, 2, 3],
    })
    .unwrap();

    // Change to group commit
    wal.set_sync_mode(WalSyncMode::GroupCommit);
    assert_eq!(wal.sync_mode(), WalSyncMode::GroupCommit);
}

#[test]
fn test_wal_flush_operations() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Async).unwrap();

    // Write some data
    for i in 0..10 {
        wal.append(WalOperation::Insert {
            table: "test".to_string(),
            tuple: vec![i],
        })
        .unwrap();
    }

    // Explicit flush
    wal.flush().unwrap();

    // Data should be durable now
    let entries = wal.replay().unwrap();
    assert_eq!(entries.len(), 10);
}

#[test]
fn test_wal_truncate() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Sync).unwrap();

    // Write 100 entries
    for i in 0..100 {
        wal.append(WalOperation::Insert {
            table: "test".to_string(),
            tuple: vec![i],
        })
        .unwrap();
    }

    // Truncate up to LSN 50
    wal.truncate(50).unwrap();

    // Verify only 51-100 remain
    let entries = wal.replay().unwrap();
    assert_eq!(entries.len(), 50);
    assert_eq!(entries[0].lsn, 51);
    assert_eq!(entries[49].lsn, 100);
}

#[test]
fn test_wal_concurrent_operations() {
    let (_temp, db) = create_test_db();
    let wal = Arc::new(WriteAheadLog::open(db, WalSyncMode::GroupCommit).unwrap());

    let mut handles = vec![];

    // Spawn multiple threads writing concurrently
    for thread_id in 0..5 {
        let wal_clone = Arc::clone(&wal);
        let handle = thread::spawn(move || {
            for i in 0..20 {
                wal_clone
                    .append(WalOperation::Insert {
                        table: format!("table_{}", thread_id),
                        tuple: vec![i],
                    })
                    .unwrap();
            }
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().unwrap();
    }

    // Verify all entries written
    let metrics = wal.metrics().unwrap();
    assert_eq!(metrics.entry_count, 100); // 5 threads * 20 entries

    // Verify integrity
    let report = wal.verify_integrity().unwrap();
    assert!(report.is_valid);
}

#[test]
fn test_wal_large_entries() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Sync).unwrap();

    // Write large entries (1MB each)
    let large_data = vec![0u8; 1024 * 1024];

    for i in 0..10 {
        wal.append(WalOperation::Insert {
            table: "large_table".to_string(),
            tuple: large_data.clone(),
        })
        .unwrap();
    }

    // Verify metrics
    let metrics = wal.metrics().unwrap();
    assert_eq!(metrics.entry_count, 10);
    assert!(metrics.size_bytes > 10 * 1024 * 1024); // At least 10MB

    // Verify replay
    let stats = wal.replay_with_stats(0).unwrap();
    assert_eq!(stats.operations_replayed, 10);
}

#[test]
fn test_wal_recovery_after_truncate() {
    let (temp, db) = create_test_db();

    {
        let wal = WriteAheadLog::open(Arc::clone(&db), WalSyncMode::Sync).unwrap();

        // Write 100 entries
        for i in 0..100 {
            wal.append(WalOperation::Insert {
                table: "test".to_string(),
                tuple: vec![i],
            })
            .unwrap();
        }

        // Truncate first 50
        wal.truncate(50).unwrap();
    }

    // Recover after crash
    {
        let wal = WriteAheadLog::open(Arc::clone(&db), WalSyncMode::Sync).unwrap();

        // LSN should continue from 100
        assert_eq!(wal.current_lsn(), 100);

        // Only 50 entries should remain
        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 50);
        assert_eq!(entries[0].lsn, 51);
    }
}

#[test]
#[ignore = "TODO: WAL metrics after cleanup needs verification"]
fn test_wal_metrics_after_cleanup() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Sync).unwrap();

    // Write entries
    for i in 0..100 {
        wal.append(WalOperation::Insert {
            table: "test".to_string(),
            tuple: vec![i; 100],
        })
        .unwrap();
    }

    let metrics_before = wal.metrics().unwrap();

    // Cleanup old entries
    let cleanup_stats = wal.cleanup_old_logs(0, 30).unwrap();
    assert_eq!(cleanup_stats.entries_deleted, 70);

    let metrics_after = wal.metrics().unwrap();
    assert_eq!(metrics_after.entry_count, 30);
    assert!(metrics_after.size_bytes < metrics_before.size_bytes);
}

#[test]
fn test_wal_empty_replay() {
    let (_temp, db) = create_test_db();
    let wal = WriteAheadLog::open(db, WalSyncMode::Sync).unwrap();

    // Replay empty WAL
    let stats = wal.replay_with_stats(0).unwrap();
    assert_eq!(stats.operations_replayed, 0);
    assert_eq!(stats.operations_skipped, 0);
    assert_eq!(stats.errors, 0);
}

#[test]
fn test_wal_all_sync_modes() {
    for mode in [WalSyncMode::Sync, WalSyncMode::Async, WalSyncMode::GroupCommit] {
        let (_temp, db) = create_test_db();
        let wal = WriteAheadLog::open(db, mode).unwrap();

        assert_eq!(wal.sync_mode(), mode);

        // Write and verify
        for i in 0..10 {
            wal.append(WalOperation::Insert {
                table: "test".to_string(),
                tuple: vec![i],
            })
            .unwrap();
        }

        wal.flush().unwrap();

        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 10);
    }
}
