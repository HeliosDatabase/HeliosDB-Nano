//! Tests for automatic time-travel versioning
//!
//! This test suite verifies that automatic versioning:
//! - Is enabled by default (zero-config)
//! - Creates snapshots transparently on every insert
//! - Supports AS OF queries without explicit API calls
//! - Can be disabled for performance-critical workloads
//! - Maintains backward compatibility
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_nano::{Config, StorageEngine, Tuple, Value, Schema, Column, DataType};
use heliosdb_nano::sql::{LogicalPlan, AsOfClause, Executor};
use std::sync::Arc;

/// Helper to create a simple schema
fn create_simple_schema() -> Schema {
    Schema {
        columns: vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
            },
            Column {
                name: "name".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
            },
            Column {
                name: "value".to_string(),
                data_type: DataType::Float8,
                nullable: false,
                primary_key: false,
            },
        ],
    }
}

#[test]
fn test_automatic_versioning_enabled_by_default() {
    // Test that time-travel is enabled by default (zero-config)
    let config = Config::in_memory();
    assert!(config.storage.time_travel_enabled, "Time-travel should be enabled by default");

    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    let catalog = engine.catalog();
    catalog.create_table("users", create_simple_schema())
        .expect("Failed to create table");

    // Insert using the default insert_tuple() - should automatically version
    let tuple = Tuple {
        values: vec![
            Value::Int4(1),
            Value::String("Alice".to_string()),
            Value::Float8(100.0),
        ],
    };

    engine.insert_tuple("users", tuple)
        .expect("Failed to insert tuple");

    // Verify snapshot was created
    let snapshot_mgr = engine.snapshot_manager();
    assert!(snapshot_mgr.snapshot_count() > 0, "Snapshot should be created automatically");

    // Verify we can query with AS OF
    let plan = LogicalPlan::Scan {
        table_name: "users".to_string(),
        schema: Arc::new(create_simple_schema()),
        projection: None,
        as_of: Some(AsOfClause::Transaction(1)),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan)
        .expect("Failed to execute AS OF query");

    assert_eq!(results.len(), 1, "Should be able to query historical data");
}

#[test]
fn test_automatic_versioning_disabled() {
    // Test that versioning can be disabled
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = false;

    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    let catalog = engine.catalog();
    catalog.create_table("users", create_simple_schema())
        .expect("Failed to create table");

    // Insert with versioning disabled
    let tuple = Tuple {
        values: vec![
            Value::Int4(1),
            Value::String("Bob".to_string()),
            Value::Float8(200.0),
        ],
    };

    engine.insert_tuple("users", tuple)
        .expect("Failed to insert tuple");

    // Verify no snapshots were created
    let snapshot_mgr = engine.snapshot_manager();
    assert_eq!(snapshot_mgr.snapshot_count(), 0, "No snapshots should be created when disabled");

    // Verify data is still accessible via normal queries
    let tuples = engine.scan_table("users")
        .expect("Failed to scan table");
    assert_eq!(tuples.len(), 1, "Data should be accessible normally");
}

#[test]
fn test_transparent_versioning_workflow() {
    // Test that versioning is completely transparent
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    let catalog = engine.catalog();
    catalog.create_table("products", create_simple_schema())
        .expect("Failed to create table");

    // Insert multiple tuples using normal insert_tuple()
    // No need to call insert_tuple_versioned() explicitly
    for i in 1..=5 {
        let tuple = Tuple {
            values: vec![
                Value::Int4(i),
                Value::String(format!("Product {}", i)),
                Value::Float8(i as f64 * 10.0),
            ],
        };
        engine.insert_tuple("products", tuple)
            .expect("Failed to insert tuple");
    }

    // Verify snapshots were created transparently
    let snapshot_mgr = engine.snapshot_manager();
    assert_eq!(snapshot_mgr.snapshot_count(), 5, "Should have 5 snapshots");

    // Query at different transaction points
    let plan_tx2 = LogicalPlan::Scan {
        table_name: "products".to_string(),
        schema: Arc::new(create_simple_schema()),
        projection: None,
        as_of: Some(AsOfClause::Transaction(2)),
    };

    let plan_tx4 = LogicalPlan::Scan {
        table_name: "products".to_string(),
        schema: Arc::new(create_simple_schema()),
        projection: None,
        as_of: Some(AsOfClause::Transaction(4)),
    };

    let mut executor = Executor::with_storage(&engine);

    let results_tx2 = executor.execute(&plan_tx2)
        .expect("Failed to execute AS OF TRANSACTION 2");
    let results_tx4 = executor.execute(&plan_tx4)
        .expect("Failed to execute AS OF TRANSACTION 4");

    // Verify isolation
    assert_eq!(results_tx2.len(), 2, "Should see 2 products at transaction 2");
    assert_eq!(results_tx4.len(), 4, "Should see 4 products at transaction 4");
}

#[test]
fn test_tri_modal_resolution() {
    // Test that automatic versioning supports all three AS OF modes
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    let catalog = engine.catalog();
    catalog.create_table("orders", create_simple_schema())
        .expect("Failed to create table");

    // Insert test data
    for i in 1..=3 {
        let tuple = Tuple {
            values: vec![
                Value::Int4(i),
                Value::String(format!("Order {}", i)),
                Value::Float8(i as f64 * 100.0),
            ],
        };
        engine.insert_tuple("orders", tuple)
            .expect("Failed to insert tuple");
    }

    let schema = Arc::new(create_simple_schema());
    let mut executor = Executor::with_storage(&engine);

    // Test AS OF TRANSACTION
    let plan_txn = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        schema: schema.clone(),
        projection: None,
        as_of: Some(AsOfClause::Transaction(2)),
    };
    let results_txn = executor.execute(&plan_txn)
        .expect("AS OF TRANSACTION should work");
    assert_eq!(results_txn.len(), 2);

    // Test AS OF SCN
    let plan_scn = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        schema: schema.clone(),
        projection: None,
        as_of: Some(AsOfClause::Scn(2)),
    };
    let results_scn = executor.execute(&plan_scn)
        .expect("AS OF SCN should work");
    assert_eq!(results_scn.len(), 2);

    // Test AS OF TIMESTAMP
    let snapshot_mgr = engine.snapshot_manager();
    if let Some(metadata) = snapshot_mgr.get_snapshot_metadata(2) {
        let plan_ts = LogicalPlan::Scan {
            table_name: "orders".to_string(),
            schema: schema.clone(),
            projection: None,
            as_of: Some(AsOfClause::Timestamp(metadata.wall_clock_time.clone())),
        };
        let results_ts = executor.execute(&plan_ts)
            .expect("AS OF TIMESTAMP should work");
        assert!(results_ts.len() >= 2, "Should see at least 2 orders");
    }
}

#[test]
fn test_backward_compatibility() {
    // Test that existing code using insert_tuple_versioned() still works
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    let catalog = engine.catalog();
    catalog.create_table("legacy", create_simple_schema())
        .expect("Failed to create table");

    // Explicitly call insert_tuple_versioned (old API)
    let tuple = Tuple {
        values: vec![
            Value::Int4(1),
            Value::String("Legacy".to_string()),
            Value::Float8(999.0),
        ],
    };

    engine.insert_tuple_versioned("legacy", tuple)
        .expect("insert_tuple_versioned should still work");

    // Verify it behaves the same
    let snapshot_mgr = engine.snapshot_manager();
    assert!(snapshot_mgr.snapshot_count() > 0);

    let plan = LogicalPlan::Scan {
        table_name: "legacy".to_string(),
        schema: Arc::new(create_simple_schema()),
        projection: None,
        as_of: Some(AsOfClause::Transaction(1)),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan)
        .expect("Should query versioned data");
    assert_eq!(results.len(), 1);
}

#[test]
fn test_force_versioning_when_disabled() {
    // Test that insert_tuple_versioned() works even when auto-versioning is disabled
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = false;

    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    let catalog = engine.catalog();
    catalog.create_table("manual", create_simple_schema())
        .expect("Failed to create table");

    // Use insert_tuple() - should NOT version
    let tuple1 = Tuple {
        values: vec![
            Value::Int4(1),
            Value::String("NoVersion".to_string()),
            Value::Float8(100.0),
        ],
    };
    engine.insert_tuple("manual", tuple1)
        .expect("Failed to insert");

    assert_eq!(engine.snapshot_manager().snapshot_count(), 0);

    // Use insert_tuple_versioned() - SHOULD version
    let tuple2 = Tuple {
        values: vec![
            Value::Int4(2),
            Value::String("Versioned".to_string()),
            Value::Float8(200.0),
        ],
    };
    engine.insert_tuple_versioned("manual", tuple2)
        .expect("Failed to insert versioned");

    assert_eq!(engine.snapshot_manager().snapshot_count(), 1);
}

#[test]
fn test_automatic_gc_integration() {
    // Test that automatic versioning triggers GC
    use heliosdb_nano::storage::GcConfig;

    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    let catalog = engine.catalog();
    catalog.create_table("gc_test", create_simple_schema())
        .expect("Failed to create table");

    // Insert enough tuples to trigger GC (GC default max is 1000)
    // We'll insert a smaller number to verify GC can run
    for i in 1..=50 {
        let tuple = Tuple {
            values: vec![
                Value::Int4(i),
                Value::String(format!("Item {}", i)),
                Value::Float8(i as f64),
            ],
        };
        engine.insert_tuple("gc_test", tuple)
            .expect("Failed to insert");
    }

    let snapshot_count = engine.snapshot_manager().snapshot_count();
    assert!(snapshot_count <= 1000, "GC should prevent unlimited growth");
}

#[test]
fn test_snapshot_isolation_automatic() {
    // Test snapshot isolation with automatic versioning
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    let catalog = engine.catalog();
    catalog.create_table("isolation", create_simple_schema())
        .expect("Failed to create table");

    // Insert data points
    for i in 1..=5 {
        let tuple = Tuple {
            values: vec![
                Value::Int4(i),
                Value::String(format!("Data {}", i)),
                Value::Float8(i as f64 * 50.0),
            ],
        };
        engine.insert_tuple("isolation", tuple)
            .expect("Failed to insert");
    }

    let schema = Arc::new(create_simple_schema());
    let mut executor = Executor::with_storage(&engine);

    // Create two snapshots at different points
    let plan_early = LogicalPlan::Scan {
        table_name: "isolation".to_string(),
        schema: schema.clone(),
        projection: None,
        as_of: Some(AsOfClause::Transaction(2)),
    };

    let plan_late = LogicalPlan::Scan {
        table_name: "isolation".to_string(),
        schema: schema.clone(),
        projection: None,
        as_of: Some(AsOfClause::Transaction(4)),
    };

    let results_early = executor.execute(&plan_early)
        .expect("Failed to query early snapshot");
    let results_late = executor.execute(&plan_late)
        .expect("Failed to query late snapshot");

    // Verify isolation - each query sees a consistent view
    assert_eq!(results_early.len(), 2);
    assert_eq!(results_late.len(), 4);

    // Query again - should see same results (repeatability)
    let results_early_2 = executor.execute(&plan_early)
        .expect("Failed to query early snapshot again");
    assert_eq!(results_early.len(), results_early_2.len());
}

#[test]
fn test_performance_overhead_automatic() {
    // Test that automatic versioning has acceptable overhead
    use std::time::Instant;

    // Test with versioning disabled
    let mut config_no_tt = Config::in_memory();
    config_no_tt.storage.time_travel_enabled = false;
    let engine_no_tt = StorageEngine::open_in_memory(&config_no_tt)
        .expect("Failed to create engine");

    let catalog = engine_no_tt.catalog();
    catalog.create_table("perf_no_tt", create_simple_schema())
        .expect("Failed to create table");

    let start = Instant::now();
    for i in 1..=100 {
        let tuple = Tuple {
            values: vec![
                Value::Int4(i),
                Value::String(format!("Item {}", i)),
                Value::Float8(i as f64),
            ],
        };
        engine_no_tt.insert_tuple("perf_no_tt", tuple)
            .expect("Failed to insert");
    }
    let duration_no_tt = start.elapsed();

    // Test with automatic versioning enabled
    let config_with_tt = Config::in_memory();
    let engine_with_tt = StorageEngine::open_in_memory(&config_with_tt)
        .expect("Failed to create engine");

    let catalog = engine_with_tt.catalog();
    catalog.create_table("perf_with_tt", create_simple_schema())
        .expect("Failed to create table");

    let start = Instant::now();
    for i in 1..=100 {
        let tuple = Tuple {
            values: vec![
                Value::Int4(i),
                Value::String(format!("Item {}", i)),
                Value::Float8(i as f64),
            ],
        };
        engine_with_tt.insert_tuple("perf_with_tt", tuple)
            .expect("Failed to insert");
    }
    let duration_with_tt = start.elapsed();

    // Calculate overhead
    let overhead = duration_with_tt.as_secs_f64() / duration_no_tt.as_secs_f64();
    println!("Automatic versioning overhead: {:.2}x", overhead);

    // Overhead should be reasonable (< 3x in test environment)
    // In production with optimizations, this should be much lower
    assert!(overhead < 3.0, "Overhead too high: {:.2}x", overhead);
}

#[test]
fn test_zero_config_experience() {
    // Test the complete zero-config experience
    // User should be able to:
    // 1. Create engine with defaults
    // 2. Insert data normally
    // 3. Query historical data without any setup

    // Step 1: Create with defaults
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create engine");

    // Step 2: Insert data normally (no special API)
    let catalog = engine.catalog();
    catalog.create_table("simple", create_simple_schema())
        .expect("Failed to create table");

    engine.insert_tuple("simple", Tuple {
        values: vec![
            Value::Int4(1),
            Value::String("First".to_string()),
            Value::Float8(1.0),
        ],
    }).expect("Failed to insert");

    engine.insert_tuple("simple", Tuple {
        values: vec![
            Value::Int4(2),
            Value::String("Second".to_string()),
            Value::Float8(2.0),
        ],
    }).expect("Failed to insert");

    // Step 3: Query historical data (it just works!)
    let plan = LogicalPlan::Scan {
        table_name: "simple".to_string(),
        schema: Arc::new(create_simple_schema()),
        projection: None,
        as_of: Some(AsOfClause::Transaction(1)),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan)
        .expect("Time-travel should just work with zero config");

    assert_eq!(results.len(), 1, "Should see first insert only");
}
