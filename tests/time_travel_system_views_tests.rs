//! Integration tests for time-travel system views
//!
//! Tests pg_snapshots, pg_transaction_map, and pg_scn_map system views.
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_nano::{Config, StorageEngine, Tuple, Value, Schema, Column, DataType};
use heliosdb_nano::sql::{LogicalPlan, Executor};
use heliosdb_nano::sql::logical_plan::AsOfClause;
use heliosdb_nano::sql::system_views::SystemViewRegistry;
use std::sync::Arc;

/// Helper to create a test storage engine with sample data
fn create_test_engine_with_snapshots() -> StorageEngine {
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = true;

    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    // Create a simple test table
    let schema = Schema {
        columns: vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
            },
            Column {
                name: "data".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
            },
        ],
    };

    let catalog = engine.catalog();
    catalog.create_table("test_table", schema.clone())
        .expect("Failed to create table");

    // Insert some data to create snapshots
    for i in 1..=5 {
        let tuple = Tuple {
            values: vec![
                Value::Int4(i),
                Value::String(format!("Data {}", i)),
            ],
            row_id: None,
        };
        engine.insert_tuple_versioned("test_table", tuple)
            .expect(&format!("Failed to insert tuple {}", i));
    }

    engine
}

#[test]
fn test_pg_snapshots_view() {
    let engine = create_test_engine_with_snapshots();
    let registry = SystemViewRegistry::new();

    // Execute pg_snapshots system view
    let tuples = registry.execute("pg_snapshots", &engine)
        .expect("Failed to execute pg_snapshots view");

    // Should have at least 5 snapshots (one per insert)
    assert!(tuples.len() >= 5, "Expected at least 5 snapshots, got {}", tuples.len());

    // Verify schema
    for tuple in &tuples {
        assert_eq!(tuple.values.len(), 7, "pg_snapshots should have 7 columns");

        // Verify column types
        assert!(matches!(tuple.values[0], Value::Int8(_)), "snapshot_id should be Int8");
        // created_at is Timestamp
        assert!(matches!(tuple.values[2], Value::Int8(_)), "scn should be Int8");
        assert!(matches!(tuple.values[3], Value::Int8(_)), "transaction_id should be Int8");
        assert!(matches!(tuple.values[4], Value::String(_)), "description should be String");
        assert!(matches!(tuple.values[5], Value::Int8(_)), "size_bytes should be Int8");
        assert!(matches!(tuple.values[6], Value::Boolean(_)), "is_automatic should be Boolean");
    }

    // Verify snapshots are in order (should be sorted by snapshot_id)
    let snapshot_ids: Vec<i64> = tuples.iter()
        .filter_map(|t| {
            if let Value::Int8(id) = t.values[0] {
                Some(id)
            } else {
                None
            }
        })
        .collect();

    for i in 1..snapshot_ids.len() {
        assert!(snapshot_ids[i] >= snapshot_ids[i-1],
            "Snapshots should be in ascending order");
    }
}

#[test]
fn test_pg_transaction_map_view() {
    let engine = create_test_engine_with_snapshots();
    let registry = SystemViewRegistry::new();

    // Execute pg_transaction_map system view
    let tuples = registry.execute("pg_transaction_map", &engine)
        .expect("Failed to execute pg_transaction_map view");

    // Should have at least 5 mappings (one per insert)
    assert!(tuples.len() >= 5, "Expected at least 5 transaction mappings, got {}", tuples.len());

    // Verify schema
    for tuple in &tuples {
        assert_eq!(tuple.values.len(), 4, "pg_transaction_map should have 4 columns");

        // Verify column types
        assert!(matches!(tuple.values[0], Value::Int8(_)), "transaction_id should be Int8");
        assert!(matches!(tuple.values[1], Value::Int8(_)), "snapshot_timestamp should be Int8");
        assert!(matches!(tuple.values[2], Value::Int8(_)), "scn should be Int8");
        // created_at is Timestamp
    }

    // Verify transactions are in order
    let txn_ids: Vec<i64> = tuples.iter()
        .filter_map(|t| {
            if let Value::Int8(id) = t.values[0] {
                Some(id)
            } else {
                None
            }
        })
        .collect();

    for i in 1..txn_ids.len() {
        assert!(txn_ids[i] > txn_ids[i-1],
            "Transaction IDs should be in strictly ascending order");
    }

    // Verify each transaction has a unique ID
    let unique_txns: std::collections::HashSet<_> = txn_ids.iter().collect();
    assert_eq!(unique_txns.len(), txn_ids.len(), "All transaction IDs should be unique");
}

#[test]
fn test_pg_scn_map_view() {
    let engine = create_test_engine_with_snapshots();
    let registry = SystemViewRegistry::new();

    // Execute pg_scn_map system view
    let tuples = registry.execute("pg_scn_map", &engine)
        .expect("Failed to execute pg_scn_map view");

    // Should have at least 5 mappings (one per insert)
    assert!(tuples.len() >= 5, "Expected at least 5 SCN mappings, got {}", tuples.len());

    // Verify schema
    for tuple in &tuples {
        assert_eq!(tuple.values.len(), 4, "pg_scn_map should have 4 columns");

        // Verify column types
        assert!(matches!(tuple.values[0], Value::Int8(_)), "scn should be Int8");
        assert!(matches!(tuple.values[1], Value::Int8(_)), "snapshot_timestamp should be Int8");
        assert!(matches!(tuple.values[2], Value::Int8(_)), "transaction_id should be Int8");
        // created_at is Timestamp
    }

    // Verify SCNs are in order
    let scns: Vec<i64> = tuples.iter()
        .filter_map(|t| {
            if let Value::Int8(scn) = t.values[0] {
                Some(scn)
            } else {
                None
            }
        })
        .collect();

    for i in 1..scns.len() {
        assert!(scns[i] > scns[i-1],
            "SCNs should be in strictly ascending order");
    }

    // Verify each SCN has a unique value
    let unique_scns: std::collections::HashSet<_> = scns.iter().collect();
    assert_eq!(unique_scns.len(), scns.len(), "All SCNs should be unique");
}

#[test]
fn test_system_view_consistency() {
    let engine = create_test_engine_with_snapshots();
    let registry = SystemViewRegistry::new();

    // Execute all three views
    let snapshots = registry.execute("pg_snapshots", &engine)
        .expect("Failed to execute pg_snapshots");
    let txn_map = registry.execute("pg_transaction_map", &engine)
        .expect("Failed to execute pg_transaction_map");
    let scn_map = registry.execute("pg_scn_map", &engine)
        .expect("Failed to execute pg_scn_map");

    // All three views should have the same number of rows
    assert_eq!(snapshots.len(), txn_map.len(),
        "pg_snapshots and pg_transaction_map should have same number of rows");
    assert_eq!(snapshots.len(), scn_map.len(),
        "pg_snapshots and pg_scn_map should have same number of rows");

    // Verify data consistency across views
    for i in 0..snapshots.len() {
        let snapshot = &snapshots[i];
        let txn = &txn_map[i];
        let scn = &scn_map[i];

        // Extract snapshot_id from pg_snapshots
        let snapshot_id = if let Value::Int8(id) = snapshot.values[0] {
            id
        } else {
            panic!("Expected Int8 for snapshot_id");
        };

        // Extract snapshot_timestamp from pg_transaction_map
        let txn_timestamp = if let Value::Int8(ts) = txn.values[1] {
            ts
        } else {
            panic!("Expected Int8 for snapshot_timestamp in pg_transaction_map");
        };

        // Extract snapshot_timestamp from pg_scn_map
        let scn_timestamp = if let Value::Int8(ts) = scn.values[1] {
            ts
        } else {
            panic!("Expected Int8 for snapshot_timestamp in pg_scn_map");
        };

        // All three should refer to the same snapshot
        assert_eq!(snapshot_id, txn_timestamp,
            "Snapshot ID mismatch between pg_snapshots and pg_transaction_map");
        assert_eq!(snapshot_id, scn_timestamp,
            "Snapshot ID mismatch between pg_snapshots and pg_scn_map");
    }
}

#[test]
fn test_time_travel_with_system_views() {
    let engine = create_test_engine_with_snapshots();
    let registry = SystemViewRegistry::new();

    // Get transaction map
    let txn_map = registry.execute("pg_transaction_map", &engine)
        .expect("Failed to execute pg_transaction_map");

    // Use transaction ID from the map
    let txn_id = if let Value::Int8(id) = txn_map[2].values[0] {
        id as u64
    } else {
        panic!("Expected Int8 for transaction_id");
    };

    // Execute time-travel query using this transaction ID
    let plan = LogicalPlan::Scan {
        table_name: "test_table".to_string(),
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Transaction(txn_id)),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan)
        .expect("Failed to execute AS OF TRANSACTION query");

    // Should see data up to this transaction
    assert!(results.len() >= 1, "Should have at least one row");
    assert!(results.len() <= 3, "Should not exceed 3 rows for transaction {}", txn_id);
}

#[test]
fn test_scn_based_time_travel() {
    let engine = create_test_engine_with_snapshots();
    let registry = SystemViewRegistry::new();

    // Get SCN map
    let scn_map = registry.execute("pg_scn_map", &engine)
        .expect("Failed to execute pg_scn_map");

    // Use SCN from the map
    let scn = if let Value::Int8(s) = scn_map[1].values[0] {
        s as u64
    } else {
        panic!("Expected Int8 for scn");
    };

    // Execute time-travel query using this SCN
    let plan = LogicalPlan::Scan {
        table_name: "test_table".to_string(),
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Scn(scn)),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan)
        .expect("Failed to execute AS OF SCN query");

    // Should see data up to this SCN
    assert!(results.len() >= 1, "Should have at least one row");
    assert!(results.len() <= 2, "Should not exceed 2 rows for SCN {}", scn);
}

#[test]
fn test_system_view_caching() {
    let engine = create_test_engine_with_snapshots();
    let registry = SystemViewRegistry::new();

    // Execute view first time
    let start = std::time::Instant::now();
    let first_result = registry.execute("pg_snapshots", &engine)
        .expect("Failed to execute first time");
    let first_duration = start.elapsed();

    // Execute view second time (should hit cache)
    let start = std::time::Instant::now();
    let second_result = registry.execute("pg_snapshots", &engine)
        .expect("Failed to execute second time");
    let second_duration = start.elapsed();

    // Results should be identical
    assert_eq!(first_result.len(), second_result.len(),
        "Cached results should be identical");

    // Second query should be faster (though this is a soft check)
    println!("First query: {:?}, Second query (cached): {:?}",
        first_duration, second_duration);
}

#[test]
fn test_view_cache_stats() {
    let engine = create_test_engine_with_snapshots();
    let registry = SystemViewRegistry::new();

    // Get initial cache stats
    let (initial_size, capacity) = registry.cache_stats()
        .expect("Failed to get cache stats");

    assert_eq!(initial_size, 0, "Cache should be empty initially");
    assert!(capacity > 0, "Cache capacity should be positive");

    // Execute a view to populate cache
    registry.execute("pg_snapshots", &engine)
        .expect("Failed to execute view");

    // Cache size should increase
    let (after_size, _) = registry.cache_stats()
        .expect("Failed to get cache stats after execution");

    assert_eq!(after_size, 1, "Cache should have 1 entry after one view execution");
}

#[test]
fn test_view_cache_invalidation() {
    let engine = create_test_engine_with_snapshots();
    let registry = SystemViewRegistry::new();

    // Execute view to populate cache
    registry.execute("pg_snapshots", &engine)
        .expect("Failed to execute view");

    let (size_before, _) = registry.cache_stats()
        .expect("Failed to get cache stats");
    assert_eq!(size_before, 1, "Cache should have 1 entry");

    // Invalidate cache
    registry.invalidate_view("pg_snapshots")
        .expect("Failed to invalidate view");

    let (size_after, _) = registry.cache_stats()
        .expect("Failed to get cache stats");
    assert_eq!(size_after, 0, "Cache should be empty after invalidation");
}

#[test]
fn test_snapshot_size_calculation() {
    let engine = create_test_engine_with_snapshots();
    let registry = SystemViewRegistry::new();

    // Execute pg_snapshots to get size information
    let snapshots = registry.execute("pg_snapshots", &engine)
        .expect("Failed to execute pg_snapshots");

    for snapshot in snapshots {
        // Extract size_bytes (6th column, index 5)
        if let Value::Int8(size) = snapshot.values[5] {
            // Size should be non-negative
            assert!(size >= 0, "Snapshot size should be non-negative");

            // For our test data, size should be reasonable (not zero, not huge)
            // Each tuple has an Int4 and a Text field, so minimum size is a few bytes
            println!("Snapshot size: {} bytes", size);
        } else {
            panic!("Expected Int8 for size_bytes");
        }
    }
}

#[test]
fn test_empty_database_system_views() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    let registry = SystemViewRegistry::new();

    // All views should return empty results for empty database
    let snapshots = registry.execute("pg_snapshots", &engine)
        .expect("Failed to execute pg_snapshots");
    let txn_map = registry.execute("pg_transaction_map", &engine)
        .expect("Failed to execute pg_transaction_map");
    let scn_map = registry.execute("pg_scn_map", &engine)
        .expect("Failed to execute pg_scn_map");

    assert_eq!(snapshots.len(), 0, "pg_snapshots should be empty");
    assert_eq!(txn_map.len(), 0, "pg_transaction_map should be empty");
    assert_eq!(scn_map.len(), 0, "pg_scn_map should be empty");
}

#[test]
fn test_system_view_error_handling() {
    let engine = create_test_engine_with_snapshots();
    let registry = SystemViewRegistry::new();

    // Try to execute non-existent view
    let result = registry.execute("pg_nonexistent_view", &engine);
    assert!(result.is_err(), "Should fail for non-existent view");

    let err = result.unwrap_err();
    assert!(err.to_string().contains("Unknown system view"),
        "Error message should mention unknown view");
}
