//! Integration tests for Delta Tracking System
//!
//! These tests verify that the delta tracking system works correctly
//! with the storage engine and can track changes across multiple tables.
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_nano::{Config, StorageEngine, Value, Tuple, Column, DataType, Schema};
use heliosdb_nano::storage::{MvDeltaTracker, MvDeltaOperation, MvDelta};
use std::sync::Arc;
use std::time::{SystemTime, Duration};

#[test]
fn test_delta_tracking_basic_insert() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage engine");

    let delta_tracker = MvDeltaTracker::new(Arc::clone(&engine.db))
        .expect("Failed to create delta tracker");

    // Track the users table
    delta_tracker.track_table("users")
        .expect("Failed to track table");

    assert!(delta_tracker.is_tracked("users"));
    assert!(!delta_tracker.is_tracked("products"));

    // Create a delta
    let tuple = Tuple::new(vec![
        Value::Int4(1),
        Value::String("Alice".to_string()),
    ]);

    let delta = MvDelta::new(
        "users".to_string(),
        1,
        MvDeltaOperation::Insert { tuple: tuple.clone() },
        SystemTime::now(),
        100,
    );

    delta_tracker.record_delta(delta)
        .expect("Failed to record delta");

    // Retrieve deltas
    let since = SystemTime::now() - Duration::from_secs(60);
    let delta_set = delta_tracker.get_deltas_since("users", since)
        .expect("Failed to get deltas");

    assert_eq!(delta_set.len(), 1);
    assert_eq!(delta_set.deltas[0].row_id, 1);
}

#[test]
fn test_delta_tracking_multiple_operations() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage engine");

    let delta_tracker = MvDeltaTracker::new(Arc::clone(&engine.db))
        .expect("Failed to create delta tracker");

    delta_tracker.track_table("products")
        .expect("Failed to track table");

    let now = SystemTime::now();

    // Insert
    let insert_tuple = Tuple::new(vec![
        Value::Int4(1),
        Value::String("Widget".to_string()),
        Value::Float8(19.99),
    ]);

    delta_tracker.record_delta(MvDelta::new(
        "products".to_string(),
        1,
        MvDeltaOperation::Insert { tuple: insert_tuple.clone() },
        now,
        1,
    )).expect("Failed to record insert");

    // Update
    let updated_tuple = Tuple::new(vec![
        Value::Int4(1),
        Value::String("Widget Pro".to_string()),
        Value::Float8(24.99),
    ]);

    delta_tracker.record_delta(MvDelta::new(
        "products".to_string(),
        1,
        MvDeltaOperation::Update {
            old_tuple: insert_tuple.clone(),
            new_tuple: updated_tuple.clone(),
        },
        now + Duration::from_secs(1),
        2,
    )).expect("Failed to record update");

    // Delete
    delta_tracker.record_delta(MvDelta::new(
        "products".to_string(),
        1,
        MvDeltaOperation::Delete { tuple: updated_tuple },
        now + Duration::from_secs(2),
        3,
    )).expect("Failed to record delete");

    // Query deltas
    let since = now - Duration::from_secs(10);
    let delta_set = delta_tracker.get_deltas_since("products", since)
        .expect("Failed to get deltas");

    assert_eq!(delta_set.len(), 3);
    assert!(delta_set.deltas[0].operation.is_insert());
    assert!(delta_set.deltas[1].operation.is_update());
    assert!(delta_set.deltas[2].operation.is_delete());
}

#[test]
fn test_delta_tracking_multiple_tables() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage engine");

    let delta_tracker = MvDeltaTracker::new(Arc::clone(&engine.db))
        .expect("Failed to create delta tracker");

    // Track multiple tables
    delta_tracker.track_table("users").expect("Failed to track users");
    delta_tracker.track_table("orders").expect("Failed to track orders");

    let now = SystemTime::now();

    // Record deltas for users
    for i in 1..=3 {
        let tuple = Tuple::new(vec![Value::Int4(i), Value::String(format!("User {}", i))]);
        delta_tracker.record_delta(MvDelta::new(
            "users".to_string(),
            i as u64,
            MvDeltaOperation::Insert { tuple },
            now,
            i as u64,
        )).expect("Failed to record user delta");
    }

    // Record deltas for orders
    for i in 1..=2 {
        let tuple = Tuple::new(vec![Value::Int4(i), Value::Int4(i)]);
        delta_tracker.record_delta(MvDelta::new(
            "orders".to_string(),
            i as u64,
            MvDeltaOperation::Insert { tuple },
            now,
            (100 + i) as u64,
        )).expect("Failed to record order delta");
    }

    // Query deltas for multiple tables
    let since = now - Duration::from_secs(10);
    let delta_sets = delta_tracker.get_deltas_for_tables(&["users", "orders"], since)
        .expect("Failed to get deltas for tables");

    assert_eq!(delta_sets.len(), 2);
    assert_eq!(delta_sets[0].len(), 3); // 3 user deltas
    assert_eq!(delta_sets[1].len(), 2); // 2 order deltas
}

#[test]
fn test_delta_tracking_time_range() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage engine");

    let delta_tracker = MvDeltaTracker::new(Arc::clone(&engine.db))
        .expect("Failed to create delta tracker");

    delta_tracker.track_table("events")
        .expect("Failed to track table");

    let t0 = SystemTime::now() - Duration::from_secs(100);
    let t1 = SystemTime::now() - Duration::from_secs(50);
    let t2 = SystemTime::now();

    // Insert old delta
    let old_tuple = Tuple::new(vec![Value::Int4(1)]);
    delta_tracker.record_delta(MvDelta::new(
        "events".to_string(),
        1,
        MvDeltaOperation::Insert { tuple: old_tuple },
        t0,
        1,
    )).expect("Failed to record old delta");

    // Insert recent delta
    let recent_tuple = Tuple::new(vec![Value::Int4(2)]);
    delta_tracker.record_delta(MvDelta::new(
        "events".to_string(),
        2,
        MvDeltaOperation::Insert { tuple: recent_tuple },
        t2,
        2,
    )).expect("Failed to record recent delta");

    // Query from t1 (should only get recent delta)
    let delta_set = delta_tracker.get_deltas_since("events", t1)
        .expect("Failed to get deltas");

    assert_eq!(delta_set.len(), 1);
    assert_eq!(delta_set.deltas[0].row_id, 2);

    // Query from t0 (should get both deltas)
    let delta_set = delta_tracker.get_deltas_since("events", t0)
        .expect("Failed to get deltas");

    assert_eq!(delta_set.len(), 2);
}

#[test]
fn test_delta_tracking_compaction() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage engine");

    let delta_tracker = MvDeltaTracker::new(Arc::clone(&engine.db))
        .expect("Failed to create delta tracker");

    delta_tracker.track_table("logs")
        .expect("Failed to track table");

    let old_time = SystemTime::now() - Duration::from_secs(7200); // 2 hours ago
    let recent_time = SystemTime::now();

    // Insert old deltas
    for i in 1..=5 {
        let tuple = Tuple::new(vec![Value::Int4(i)]);
        delta_tracker.record_delta(MvDelta::new(
            "logs".to_string(),
            i as u64,
            MvDeltaOperation::Insert { tuple },
            old_time,
            i as u64,
        )).expect("Failed to record old delta");
    }

    // Insert recent deltas
    for i in 6..=10 {
        let tuple = Tuple::new(vec![Value::Int4(i)]);
        delta_tracker.record_delta(MvDelta::new(
            "logs".to_string(),
            i as u64,
            MvDeltaOperation::Insert { tuple },
            recent_time,
            i as u64,
        )).expect("Failed to record recent delta");
    }

    // Verify all deltas are present
    let since = old_time - Duration::from_secs(60);
    let before_compact = delta_tracker.get_deltas_since("logs", since)
        .expect("Failed to get deltas");
    assert_eq!(before_compact.len(), 10);

    // Compact deltas older than 1 hour
    let cutoff = SystemTime::now() - Duration::from_secs(3600);
    let deleted_count = delta_tracker.compact(cutoff)
        .expect("Failed to compact deltas");

    assert_eq!(deleted_count, 5); // Should delete 5 old deltas

    // Verify only recent deltas remain
    let after_compact = delta_tracker.get_deltas_since("logs", since)
        .expect("Failed to get deltas");
    assert_eq!(after_compact.len(), 5);
}

#[test]
fn test_delta_tracking_stats() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage engine");

    let delta_tracker = MvDeltaTracker::new(Arc::clone(&engine.db))
        .expect("Failed to create delta tracker");

    // Track multiple tables
    delta_tracker.track_table("table1").expect("Failed to track table1");
    delta_tracker.track_table("table2").expect("Failed to track table2");

    let now = SystemTime::now();

    // Insert deltas for table1
    for i in 1..=3 {
        let tuple = Tuple::new(vec![Value::Int4(i)]);
        delta_tracker.record_delta(MvDelta::new(
            "table1".to_string(),
            i as u64,
            MvDeltaOperation::Insert { tuple },
            now,
            i as u64,
        )).expect("Failed to record delta");
    }

    // Insert deltas for table2
    for i in 1..=2 {
        let tuple = Tuple::new(vec![Value::Int4(i)]);
        delta_tracker.record_delta(MvDelta::new(
            "table2".to_string(),
            i as u64,
            MvDeltaOperation::Insert { tuple },
            now,
            (10 + i) as u64,
        )).expect("Failed to record delta");
    }

    // Get stats
    let stats = delta_tracker.get_stats()
        .expect("Failed to get stats");

    assert_eq!(stats.total_deltas, 5);
    assert_eq!(*stats.deltas_by_table.get("table1").unwrap(), 3);
    assert_eq!(*stats.deltas_by_table.get("table2").unwrap(), 2);
    assert!(stats.oldest_delta.is_some());
    assert!(stats.newest_delta.is_some());
}

#[test]
fn test_delta_tracking_untrack() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage engine");

    let delta_tracker = MvDeltaTracker::new(Arc::clone(&engine.db))
        .expect("Failed to create delta tracker");

    // Track then untrack
    delta_tracker.track_table("temp_table")
        .expect("Failed to track table");
    assert!(delta_tracker.is_tracked("temp_table"));

    delta_tracker.untrack_table("temp_table")
        .expect("Failed to untrack table");
    assert!(!delta_tracker.is_tracked("temp_table"));

    // Recording delta for untracked table should succeed but be ignored
    let tuple = Tuple::new(vec![Value::Int4(1)]);
    let delta = MvDelta::new(
        "temp_table".to_string(),
        1,
        MvDeltaOperation::Insert { tuple },
        SystemTime::now(),
        1,
    );

    delta_tracker.record_delta(delta)
        .expect("Should succeed but ignore");

    // Query should return empty set
    let since = SystemTime::now() - Duration::from_secs(60);
    let delta_set = delta_tracker.get_deltas_since("temp_table", since)
        .expect("Failed to get deltas");

    assert_eq!(delta_set.len(), 0);
}
