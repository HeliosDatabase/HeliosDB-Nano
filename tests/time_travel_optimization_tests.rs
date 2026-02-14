//! Tests for Time-Travel Query Optimization
//!
//! Verifies correctness of the reverse timestamp index and indexed lookups.
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_nano::storage::time_travel::SnapshotManager;
use rocksdb::DB;
use std::sync::Arc;
use tempfile::tempdir;

/// Helper to create a test database
fn create_test_db() -> (Arc<DB>, tempfile::TempDir) {
    let temp_dir = tempdir().unwrap();
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(true);

    // Enable bloom filters for index performance
    let mut block_opts = rocksdb::BlockBasedOptions::default();
    block_opts.set_bloom_filter(10.0, false);
    opts.set_block_based_table_factory(&block_opts);

    let db = DB::open(&opts, temp_dir.path()).unwrap();
    (Arc::new(db), temp_dir)
}

#[test]
fn test_indexed_lookup_single_version() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db);

    let table = "users";
    let row_id = 1;
    let timestamp = 1000;
    let value = b"alice".to_vec();

    // Write a version
    manager.write_version(table, row_id, timestamp, &value).unwrap();

    // Query at exact timestamp
    let result = manager.read_at_snapshot(table, row_id, timestamp).unwrap();
    assert_eq!(result, Some(value.clone()));

    // Query after timestamp
    let result = manager.read_at_snapshot(table, row_id, timestamp + 500).unwrap();
    assert_eq!(result, Some(value));

    // Query before timestamp
    let result = manager.read_at_snapshot(table, row_id, timestamp - 500).unwrap();
    assert_eq!(result, None);
}

#[test]
fn test_indexed_lookup_multiple_versions() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db);

    let table = "users";
    let row_id = 1;

    // Write versions at different timestamps
    let v1 = b"version1".to_vec();
    let v2 = b"version2".to_vec();
    let v3 = b"version3".to_vec();

    manager.write_version(table, row_id, 1000, &v1).unwrap();
    manager.write_version(table, row_id, 2000, &v2).unwrap();
    manager.write_version(table, row_id, 3000, &v3).unwrap();

    // Query at different points in time
    assert_eq!(manager.read_at_snapshot(table, row_id, 500).unwrap(), None);
    assert_eq!(manager.read_at_snapshot(table, row_id, 1000).unwrap(), Some(v1.clone()));
    assert_eq!(manager.read_at_snapshot(table, row_id, 1500).unwrap(), Some(v1.clone()));
    assert_eq!(manager.read_at_snapshot(table, row_id, 2000).unwrap(), Some(v2.clone()));
    assert_eq!(manager.read_at_snapshot(table, row_id, 2500).unwrap(), Some(v2.clone()));
    assert_eq!(manager.read_at_snapshot(table, row_id, 3000).unwrap(), Some(v3.clone()));
    assert_eq!(manager.read_at_snapshot(table, row_id, 4000).unwrap(), Some(v3));
}

#[test]
fn test_indexed_vs_linear_consistency() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db);

    let table = "test";
    let row_id = 42;

    // Create 100 versions
    for i in 1..=100 {
        let timestamp = i * 1000;
        let value = format!("value_{}", i);
        manager.write_version(table, row_id, timestamp, value.as_bytes()).unwrap();
    }

    // Verify both methods return the same results at various points
    for i in [0, 5, 25, 50, 75, 99, 150] {
        let query_ts = i * 1000;
        let indexed_result = manager.read_at_snapshot(table, row_id, query_ts).unwrap();
        let linear_result = manager.read_at_snapshot_linear(table, row_id, query_ts).unwrap();

        assert_eq!(
            indexed_result, linear_result,
            "Mismatch at timestamp {}. Indexed: {:?}, Linear: {:?}",
            query_ts,
            indexed_result.as_ref().map(|v| String::from_utf8_lossy(v)),
            linear_result.as_ref().map(|v| String::from_utf8_lossy(v))
        );
    }
}

#[test]
fn test_reverse_index_creation() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db.clone());

    let table = "test";
    let row_id = 1;
    let timestamp = 5000;
    let value = b"test_value".to_vec();

    // Write version (should create index)
    manager.write_version(table, row_id, timestamp, &value).unwrap();

    // Manually verify the index exists
    let reverse_ts = u64::MAX - timestamp;
    let index_key = format!("v_idx:{}:{}:{:020}", table, row_id, reverse_ts);

    let index_value = db.get(index_key.as_bytes()).unwrap();
    assert!(index_value.is_some(), "Reverse index should exist");

    // Verify the index contains the correct timestamp
    let stored_ts = u64::from_be_bytes(index_value.unwrap()[0..8].try_into().unwrap());
    assert_eq!(stored_ts, timestamp);
}

#[test]
fn test_multiple_rows_same_table() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db);

    let table = "users";

    // Write versions for different rows
    manager.write_version(table, 1, 1000, b"alice_v1").unwrap();
    manager.write_version(table, 1, 2000, b"alice_v2").unwrap();
    manager.write_version(table, 2, 1500, b"bob_v1").unwrap();
    manager.write_version(table, 2, 2500, b"bob_v2").unwrap();

    // Query different rows at different timestamps
    assert_eq!(
        manager.read_at_snapshot(table, 1, 1000).unwrap(),
        Some(b"alice_v1".to_vec())
    );
    assert_eq!(
        manager.read_at_snapshot(table, 1, 2000).unwrap(),
        Some(b"alice_v2".to_vec())
    );
    assert_eq!(
        manager.read_at_snapshot(table, 2, 1500).unwrap(),
        Some(b"bob_v1".to_vec())
    );
    assert_eq!(
        manager.read_at_snapshot(table, 2, 2500).unwrap(),
        Some(b"bob_v2".to_vec())
    );

    // Query at intermediate times
    assert_eq!(
        manager.read_at_snapshot(table, 1, 1700).unwrap(),
        Some(b"alice_v1".to_vec())
    );
    assert_eq!(
        manager.read_at_snapshot(table, 2, 2000).unwrap(),
        Some(b"bob_v1".to_vec())
    );
}

#[test]
fn test_multiple_tables() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db);

    // Write to different tables
    manager.write_version("users", 1, 1000, b"user_data").unwrap();
    manager.write_version("orders", 1, 1000, b"order_data").unwrap();

    // Verify isolation between tables
    assert_eq!(
        manager.read_at_snapshot("users", 1, 1000).unwrap(),
        Some(b"user_data".to_vec())
    );
    assert_eq!(
        manager.read_at_snapshot("orders", 1, 1000).unwrap(),
        Some(b"order_data".to_vec())
    );

    // Non-existent table/row combinations
    assert_eq!(
        manager.read_at_snapshot("users", 1, 500).unwrap(),
        None
    );
    assert_eq!(
        manager.read_at_snapshot("orders", 2, 1000).unwrap(),
        None
    );
}

#[test]
fn test_large_number_of_versions() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db);

    let table = "test";
    let row_id = 1;
    let num_versions = 10000;

    // Write many versions
    for i in 1..=num_versions {
        let timestamp = i * 100;
        let value = format!("version_{}", i);
        manager.write_version(table, row_id, timestamp, value.as_bytes()).unwrap();
    }

    // Verify we can query efficiently at various points
    let test_points = [
        (50, None),                        // Before first
        (100, Some("version_1")),          // First
        (500000, Some("version_5000")),    // Middle
        (1000000, Some("version_10000")),  // Last
        (2000000, Some("version_10000")),  // After last
    ];

    for (ts, expected) in test_points {
        let result = manager.read_at_snapshot(table, row_id, ts).unwrap();
        let result_str = result.as_ref().map(|v| String::from_utf8_lossy(v).to_string());
        let expected_str = expected.map(String::from);
        assert_eq!(
            result_str, expected_str,
            "Mismatch at timestamp {}", ts
        );
    }
}

#[test]
fn test_boundary_timestamps() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db);

    let table = "test";
    let row_id = 1;

    // Test with extreme timestamp values
    manager.write_version(table, row_id, 0, b"zero").unwrap();
    manager.write_version(table, row_id, u64::MAX / 2, b"middle").unwrap();

    assert_eq!(
        manager.read_at_snapshot(table, row_id, 0).unwrap(),
        Some(b"zero".to_vec())
    );
    assert_eq!(
        manager.read_at_snapshot(table, row_id, u64::MAX / 2).unwrap(),
        Some(b"middle".to_vec())
    );
    assert_eq!(
        manager.read_at_snapshot(table, row_id, u64::MAX).unwrap(),
        Some(b"middle".to_vec())
    );
}

#[test]
fn test_index_persistence() {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path();

    // Create versions and close
    {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);

        let mut block_opts = rocksdb::BlockBasedOptions::default();
        block_opts.set_bloom_filter(10.0, false);
        opts.set_block_based_table_factory(&block_opts);

        let db = Arc::new(DB::open(&opts, db_path).unwrap());
        let manager = SnapshotManager::new(db);

        manager.write_version("test", 1, 1000, b"v1").unwrap();
        manager.write_version("test", 1, 2000, b"v2").unwrap();
    }

    // Reopen and verify index still works
    {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);

        let mut block_opts = rocksdb::BlockBasedOptions::default();
        block_opts.set_bloom_filter(10.0, false);
        opts.set_block_based_table_factory(&block_opts);

        let db = Arc::new(DB::open(&opts, db_path).unwrap());
        let manager = SnapshotManager::new(db);

        // Queries should still work using the persisted index
        assert_eq!(
            manager.read_at_snapshot("test", 1, 1500).unwrap(),
            Some(b"v1".to_vec())
        );
        assert_eq!(
            manager.read_at_snapshot("test", 1, 2500).unwrap(),
            Some(b"v2".to_vec())
        );
    }
}

#[test]
fn test_concurrent_queries() {
    use std::thread;

    let (db, _temp) = create_test_db();
    let manager = Arc::new(SnapshotManager::new(db));

    let table = "concurrent_test";
    let row_id = 1;

    // Write versions
    for i in 1..=100 {
        manager.write_version(table, row_id, i * 1000, format!("v{}", i).as_bytes()).unwrap();
    }

    // Spawn multiple threads doing concurrent queries
    let mut handles = vec![];
    for thread_id in 0..10 {
        let manager_clone = Arc::clone(&manager);
        let handle = thread::spawn(move || {
            for i in 1..=100 {
                let ts = i * 1000 + thread_id * 10;
                let result = manager_clone.read_at_snapshot(table, row_id, ts).unwrap();
                assert!(result.is_some(), "Expected version at timestamp {}", ts);
            }
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().unwrap();
    }
}

#[test]
fn test_empty_table() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db);

    // Query non-existent row
    let result = manager.read_at_snapshot("nonexistent", 1, 1000).unwrap();
    assert_eq!(result, None);
}

#[test]
fn test_version_overwrite() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db);

    let table = "test";
    let row_id = 1;
    let timestamp = 1000;

    // Write initial version
    manager.write_version(table, row_id, timestamp, b"v1").unwrap();
    assert_eq!(
        manager.read_at_snapshot(table, row_id, timestamp).unwrap(),
        Some(b"v1".to_vec())
    );

    // Overwrite with same timestamp (should update)
    manager.write_version(table, row_id, timestamp, b"v2").unwrap();
    assert_eq!(
        manager.read_at_snapshot(table, row_id, timestamp).unwrap(),
        Some(b"v2".to_vec())
    );
}

#[test]
fn test_out_of_order_writes() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db);

    let table = "test";
    let row_id = 1;

    // Write versions out of chronological order
    manager.write_version(table, row_id, 3000, b"v3").unwrap();
    manager.write_version(table, row_id, 1000, b"v1").unwrap();
    manager.write_version(table, row_id, 2000, b"v2").unwrap();

    // Queries should still return correct versions
    assert_eq!(
        manager.read_at_snapshot(table, row_id, 1500).unwrap(),
        Some(b"v1".to_vec())
    );
    assert_eq!(
        manager.read_at_snapshot(table, row_id, 2500).unwrap(),
        Some(b"v2".to_vec())
    );
    assert_eq!(
        manager.read_at_snapshot(table, row_id, 3500).unwrap(),
        Some(b"v3".to_vec())
    );
}

#[test]
fn test_large_values() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db);

    let table = "test";
    let row_id = 1;

    // Create a large value (1MB)
    let large_value: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();

    manager.write_version(table, row_id, 1000, &large_value).unwrap();

    let result = manager.read_at_snapshot(table, row_id, 1000).unwrap();
    assert_eq!(result, Some(large_value));
}

#[test]
fn test_special_characters_in_table_name() {
    let (db, _temp) = create_test_db();
    let manager = SnapshotManager::new(db);

    // Tables with special characters in name
    let tables = [
        "table_with_underscores",
        "table-with-dashes",
        "table.with.dots",
        "table123",
    ];

    for table in &tables {
        manager.write_version(table, 1, 1000, b"test").unwrap();
        let result = manager.read_at_snapshot(table, 1, 1000).unwrap();
        assert_eq!(result, Some(b"test".to_vec()), "Failed for table: {}", table);
    }
}
