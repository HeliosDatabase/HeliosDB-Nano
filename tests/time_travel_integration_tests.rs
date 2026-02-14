//! Integration tests for time-travel queries
//!
//! Tests AS OF TIMESTAMP, AS OF TRANSACTION, and AS OF SCN queries.
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_nano::{Config, StorageEngine, Tuple, Value, Schema, Column, DataType};
use heliosdb_nano::sql::LogicalPlan;
use heliosdb_nano::sql::logical_plan::AsOfClause;
use heliosdb_nano::sql::Executor;
use std::sync::Arc;

/// Helper to create a test storage engine with sample data
fn create_test_engine_with_history() -> StorageEngine {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    // Create a simple orders table
    let schema = Schema {
        columns: vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
            },
            Column {
                name: "customer".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
            },
            Column {
                name: "amount".to_string(),
                data_type: DataType::Float8,
                nullable: false,
                primary_key: false,
            },
        ],
    };

    // Create table
    let catalog = engine.catalog();
    catalog.create_table("orders", schema.clone())
        .expect("Failed to create table");

    // Insert version 1 - Initial data
    let tuple1 = Tuple {
        values: vec![
            Value::Int4(1),
            Value::String("Alice".to_string()),
            Value::Float8(100.0),
        ],
    };
    engine.insert_tuple_versioned("orders", tuple1)
        .expect("Failed to insert tuple 1");

    // Insert version 2 - Add another order
    let tuple2 = Tuple {
        values: vec![
            Value::Int4(2),
            Value::String("Bob".to_string()),
            Value::Float8(200.0),
        ],
    };
    engine.insert_tuple_versioned("orders", tuple2)
        .expect("Failed to insert tuple 2");

    // Insert version 3 - Add third order
    let tuple3 = Tuple {
        values: vec![
            Value::Int4(3),
            Value::String("Charlie".to_string()),
            Value::Float8(300.0),
        ],
    };
    engine.insert_tuple_versioned("orders", tuple3)
        .expect("Failed to insert tuple 3");

    engine
}

#[test]
fn test_current_snapshot_query() {
    let engine = create_test_engine_with_history();

    // Query current state (no AS OF)
    let plan = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: None,
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan)
        .expect("Failed to execute query");

    // Should see all 3 orders
    assert_eq!(results.len(), 3);
}

#[test]
fn test_as_of_transaction() {
    let engine = create_test_engine_with_history();
    let snapshot_mgr = engine.snapshot_manager();

    // Get the transaction ID of the second insert
    // In our test, we inserted 3 tuples, so we have transactions 1, 2, 3
    let txn_id = 2;

    // Query AS OF TRANSACTION 2
    let plan = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Transaction(txn_id)),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan)
        .expect("Failed to execute AS OF TRANSACTION query");

    // Should see only first 2 orders (transaction 1 and 2)
    assert_eq!(results.len(), 2);

    // Verify we can see the correct data
    let customer_names: Vec<String> = results.iter()
        .filter_map(|t| {
            if let Value::String(name) = &t.values[1] {
                Some(name.clone())
            } else {
                None
            }
        })
        .collect();

    assert!(customer_names.contains(&"Alice".to_string()));
    assert!(customer_names.contains(&"Bob".to_string()));
    assert!(!customer_names.contains(&"Charlie".to_string()));
}

#[test]
fn test_as_of_scn() {
    let engine = create_test_engine_with_history();
    let snapshot_mgr = engine.snapshot_manager();

    // Get SCN of the first insert
    let scn = 1;

    // Query AS OF SCN 1
    let plan = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Scn(scn)),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan)
        .expect("Failed to execute AS OF SCN query");

    // Should see only first order (SCN 1)
    assert_eq!(results.len(), 1);

    // Verify it's Alice's order
    if let Value::String(name) = &results[0].values[1] {
        assert_eq!(name, "Alice");
    } else {
        panic!("Expected text value for customer name");
    }
}

#[test]
fn test_as_of_timestamp() {
    let engine = create_test_engine_with_history();
    let snapshot_mgr = engine.snapshot_manager();

    // Get metadata for second snapshot
    let snapshots: Vec<_> = (1..=10).filter_map(|i| {
        snapshot_mgr.get_snapshot_metadata(i)
    }).collect();

    assert!(snapshots.len() >= 2, "Need at least 2 snapshots for this test");

    // Use timestamp of second snapshot
    let timestamp_str = &snapshots[1].wall_clock_time;

    // Query AS OF TIMESTAMP
    let plan = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Timestamp(timestamp_str.clone())),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan)
        .expect("Failed to execute AS OF TIMESTAMP query");

    // Should see first 2 orders
    assert_eq!(results.len(), 2);
}

#[test]
fn test_as_of_now() {
    let engine = create_test_engine_with_history();

    // Query AS OF NOW
    let plan = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Now),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan)
        .expect("Failed to execute AS OF NOW query");

    // Should see all 3 orders (same as current)
    assert_eq!(results.len(), 3);
}

#[test]
fn test_snapshot_not_found() {
    let engine = create_test_engine_with_history();

    // Try to query with non-existent transaction ID
    let plan = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Transaction(99999)),
    };

    let mut executor = Executor::with_storage(&engine);
    let result = executor.execute(&plan);

    // Should fail with appropriate error
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("not found") || err.to_string().contains("garbage collected"));
}

#[test]
fn test_snapshot_isolation() {
    let engine = create_test_engine_with_history();

    // Get snapshot at transaction 1
    let plan1 = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Transaction(1)),
    };

    // Get snapshot at transaction 3
    let plan3 = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Transaction(3)),
    };

    let mut executor = Executor::with_storage(&engine);

    // Execute both queries
    let results1 = executor.execute(&plan1)
        .expect("Failed to execute first query");
    let results3 = executor.execute(&plan3)
        .expect("Failed to execute second query");

    // Verify isolation - results should be different
    assert_eq!(results1.len(), 1);
    assert_eq!(results3.len(), 3);

    // Both queries should return consistent results if executed again
    let results1_again = executor.execute(&plan1)
        .expect("Failed to execute first query again");
    assert_eq!(results1.len(), results1_again.len());
}

#[test]
fn test_multiple_tables_time_travel() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    // Create two tables
    let schema = Schema {
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
        ],
    };

    let catalog = engine.catalog();
    catalog.create_table("users", schema.clone())
        .expect("Failed to create users table");
    catalog.create_table("products", schema.clone())
        .expect("Failed to create products table");

    // Insert data into both tables
    engine.insert_tuple_versioned("users", Tuple {
        values: vec![Value::Int4(1), Value::String("Alice".to_string())],
    }).expect("Failed to insert user");

    engine.insert_tuple_versioned("products", Tuple {
        values: vec![Value::Int4(1), Value::String("Widget".to_string())],
    }).expect("Failed to insert product");

    // Query both tables at same snapshot
    let plan_users = LogicalPlan::Scan {
        table_name: "users".to_string(),
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: Some(AsOfClause::Transaction(1)),
    };

    let plan_products = LogicalPlan::Scan {
        table_name: "products".to_string(),
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: Some(AsOfClause::Transaction(1)),
    };

    let mut executor = Executor::with_storage(&engine);

    let users = executor.execute(&plan_users)
        .expect("Failed to query users");
    let products = executor.execute(&plan_products)
        .expect("Failed to query products");

    // Both should have data at transaction 1
    assert_eq!(users.len(), 1);
    assert_eq!(products.len(), 1);
}

#[test]
fn test_snapshot_gc() {
    use heliosdb_nano::storage::{SnapshotManager, GcConfig};

    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    let snapshot_mgr = engine.snapshot_manager();

    // Create many snapshots
    for i in 1..=20 {
        snapshot_mgr.register_snapshot(i * 100)
            .expect("Failed to register snapshot");
    }

    assert_eq!(snapshot_mgr.snapshot_count(), 20);

    // Run GC
    let removed = snapshot_mgr.gc_old_snapshots()
        .expect("Failed to run GC");

    // Should have removed some snapshots (exact count depends on GC config)
    assert!(removed > 0);
    assert!(snapshot_mgr.snapshot_count() < 20);
}

#[test]
fn test_snapshot_recovery() {
    use tempfile::tempdir;

    let temp_dir = tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path();

    // Create engine, insert data, and close
    {
        let config = Config::default();
        let engine = StorageEngine::open(db_path, &config)
            .expect("Failed to create storage engine");

        let schema = Schema {
            columns: vec![
                Column {
                    name: "id".to_string(),
                    data_type: DataType::Int4,
                    nullable: false,
                    primary_key: true,
                },
            ],
        };

        let catalog = engine.catalog();
        catalog.create_table("test", schema)
            .expect("Failed to create table");

        engine.insert_tuple_versioned("test", Tuple {
            values: vec![Value::Int4(1)],
        }).expect("Failed to insert");

        // Snapshots should be registered
        assert!(engine.snapshot_manager().snapshot_count() > 0);
    }

    // Reopen and verify snapshots were recovered
    {
        let config = Config::default();
        let engine = StorageEngine::open(db_path, &config)
            .expect("Failed to reopen storage engine");

        // Snapshots should be recovered
        assert!(engine.snapshot_manager().snapshot_count() > 0);
    }
}

#[test]
fn test_performance_overhead() {
    use std::time::Instant;

    let engine = create_test_engine_with_history();

    // Benchmark normal scan
    let plan_normal = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: None,
    };

    let mut executor = Executor::with_storage(&engine);

    let start = Instant::now();
    for _ in 0..100 {
        let _ = executor.execute(&plan_normal).expect("Normal scan failed");
    }
    let normal_duration = start.elapsed();

    // Benchmark time-travel scan
    let plan_timetravel = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Transaction(2)),
    };

    let start = Instant::now();
    for _ in 0..100 {
        let _ = executor.execute(&plan_timetravel).expect("Time-travel scan failed");
    }
    let timetravel_duration = start.elapsed();

    // Time-travel should be less than 2x overhead
    let overhead = timetravel_duration.as_secs_f64() / normal_duration.as_secs_f64();
    println!("Time-travel overhead: {:.2}x", overhead);

    // This is a soft check - in practice overhead should be <2x
    // but we allow up to 3x for test environment variability
    assert!(overhead < 3.0, "Time-travel overhead too high: {:.2}x", overhead);
}
