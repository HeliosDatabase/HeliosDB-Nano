//! Integration tests for time-travel queries
//!
//! Tests AS OF TIMESTAMP, AS OF TRANSACTION, and AS OF SCN queries.
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_nano::sql::logical_plan::AsOfClause;
use heliosdb_nano::sql::Executor;
use heliosdb_nano::sql::LogicalPlan;
use heliosdb_nano::{Column, Config, DataType, Schema, StorageEngine, Tuple, Value};
use std::sync::Arc;

/// Helper to create a test storage engine with sample data
fn create_test_engine_with_history() -> StorageEngine {
    create_test_engine_with_history_inner(false)
}

/// As `create_test_engine_with_history`, but `space_last_insert` sleeps >1s
/// before the third insert so that it lands in a strictly LATER wall-clock
/// second than the first two. Only `test_as_of_timestamp` needs that; see the
/// comment there for why.
fn create_test_engine_with_history_inner(space_last_insert: bool) -> StorageEngine {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).expect("Failed to create storage engine");

    // Create a simple orders table
    let schema = Schema {
        columns: vec![
            Column::new("id".to_string(), DataType::Int4).primary_key(),
            Column::new("customer".to_string(), DataType::Text).not_null(),
            Column::new("amount".to_string(), DataType::Float8).not_null(),
        ],
    };

    // Create table
    let catalog = engine.catalog();
    catalog
        .create_table("orders", schema.clone())
        .expect("Failed to create table");

    // Insert version 1 - Initial data
    let tuple1 = Tuple::new(vec![
        Value::Int4(1),
        Value::String("Alice".to_string()),
        Value::Float8(100.0),
    ]);
    engine
        .insert_tuple_versioned("orders", tuple1)
        .expect("Failed to insert tuple 1");

    // Insert version 2 - Add another order
    let tuple2 = Tuple::new(vec![
        Value::Int4(2),
        Value::String("Bob".to_string()),
        Value::Float8(200.0),
    ]);
    engine
        .insert_tuple_versioned("orders", tuple2)
        .expect("Failed to insert tuple 2");

    if space_last_insert {
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }

    // Insert version 3 - Add third order
    let tuple3 = Tuple::new(vec![
        Value::Int4(3),
        Value::String("Charlie".to_string()),
        Value::Float8(300.0),
    ]);
    engine
        .insert_tuple_versioned("orders", tuple3)
        .expect("Failed to insert tuple 3");

    engine
}

#[test]
fn test_current_snapshot_query() {
    let engine = create_test_engine_with_history();

    // Query current state (no AS OF)
    let plan = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        alias: None,
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: None,
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan).expect("Failed to execute query");

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
        alias: None,
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Transaction(txn_id)),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor
        .execute(&plan)
        .expect("Failed to execute AS OF TRANSACTION query");

    // Should see only first 2 orders (transaction 1 and 2)
    assert_eq!(results.len(), 2);

    // Verify we can see the correct data
    let customer_names: Vec<String> = results
        .iter()
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
        alias: None,
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Scn(scn)),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan).expect("Failed to execute AS OF SCN query");

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
    // AS OF TIMESTAMP resolution is WHOLE-SECOND granular: `resolve_timestamp`
    // parses the RFC3339 string with `DateTime::timestamp()` (seconds) and
    // compares it against `SnapshotMetadata::wall_clock_unix_secs()` (also
    // seconds). Among snapshots in the SAME second the tie is broken FORWARD,
    // toward the newest one. So if all three inserts land in one second, asking
    // for the second snapshot's timestamp resolves to the THIRD snapshot and
    // returns 3 rows — the query would look broken but is behaving as designed.
    //
    // We therefore space the third insert into a later second. That makes this
    // the one test that proves AS OF TIMESTAMP actually discriminates between
    // points in time, rather than always resolving to "latest".
    let engine = create_test_engine_with_history_inner(true);
    let snapshot_mgr = engine.snapshot_manager();

    // Get metadata for second snapshot
    let snapshots: Vec<_> = (1..=10).filter_map(|i| snapshot_mgr.get_snapshot_metadata(i)).collect();

    assert!(snapshots.len() >= 2, "Need at least 2 snapshots for this test");

    // Use timestamp of second snapshot (W2.2(b): reconstructed RFC3339)
    let timestamp_str = snapshots[1].wall_clock_rfc3339();

    // Query AS OF TIMESTAMP
    let plan = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        alias: None,
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Timestamp(timestamp_str.clone())),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor
        .execute(&plan)
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
        alias: None,
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Now),
    };

    let mut executor = Executor::with_storage(&engine);
    let results = executor.execute(&plan).expect("Failed to execute AS OF NOW query");

    // Should see all 3 orders (same as current)
    assert_eq!(results.len(), 3);
}

#[test]
fn test_snapshot_not_found() {
    let engine = create_test_engine_with_history();

    // Try to query with non-existent transaction ID
    let plan = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        alias: None,
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
        alias: None,
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Transaction(1)),
    };

    // Get snapshot at transaction 3
    let plan3 = LogicalPlan::Scan {
        table_name: "orders".to_string(),
        alias: None,
        schema: Arc::new(Schema { columns: vec![] }),
        projection: None,
        as_of: Some(AsOfClause::Transaction(3)),
    };

    let mut executor = Executor::with_storage(&engine);

    // Execute both queries
    let results1 = executor.execute(&plan1).expect("Failed to execute first query");
    let results3 = executor.execute(&plan3).expect("Failed to execute second query");

    // Verify isolation - results should be different
    assert_eq!(results1.len(), 1);
    assert_eq!(results3.len(), 3);

    // Both queries should return consistent results if executed again
    let results1_again = executor.execute(&plan1).expect("Failed to execute first query again");
    assert_eq!(results1.len(), results1_again.len());
}

#[test]
fn test_multiple_tables_time_travel() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).expect("Failed to create storage engine");

    // Create two tables
    let schema = Schema {
        columns: vec![
            Column::new("id".to_string(), DataType::Int4).primary_key(),
            Column::new("name".to_string(), DataType::Text).not_null(),
        ],
    };

    let catalog = engine.catalog();
    catalog
        .create_table("users", schema.clone())
        .expect("Failed to create users table");
    catalog
        .create_table("products", schema.clone())
        .expect("Failed to create products table");

    // Insert data into both tables
    engine
        .insert_tuple_versioned(
            "users",
            Tuple::new(vec![Value::Int4(1), Value::String("Alice".to_string())]),
        )
        .expect("Failed to insert user");

    engine
        .insert_tuple_versioned(
            "products",
            Tuple::new(vec![Value::Int4(1), Value::String("Widget".to_string())]),
        )
        .expect("Failed to insert product");

    // Transaction IDs are GLOBAL, not per-table: the `users` insert is txn 1 and
    // the `products` insert is txn 2. So the first snapshot at which BOTH tables
    // are populated is txn 2 — `users` was already there at txn 1 and remains
    // visible at txn 2.
    let plan_users = LogicalPlan::Scan {
        table_name: "users".to_string(),
        alias: None,
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: Some(AsOfClause::Transaction(2)),
    };

    let plan_products = LogicalPlan::Scan {
        table_name: "products".to_string(),
        alias: None,
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: Some(AsOfClause::Transaction(2)),
    };

    let mut executor = Executor::with_storage(&engine);

    let users = executor.execute(&plan_users).expect("Failed to query users");
    let products = executor.execute(&plan_products).expect("Failed to query products");

    // Both tables have data as of the global transaction 2
    assert_eq!(users.len(), 1);
    assert_eq!(products.len(), 1);

    // Pin the global-transaction semantics explicitly: at txn 1 only the `users`
    // insert had happened, so `products` must be EMPTY there. If transaction IDs
    // were ever made per-table, txn 1 would name the `products` insert and this
    // would return 1 row instead.
    let plan_products_txn1 = LogicalPlan::Scan {
        table_name: "products".to_string(),
        alias: None,
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: Some(AsOfClause::Transaction(1)),
    };
    let products_txn1 = executor
        .execute(&plan_products_txn1)
        .expect("Failed to query products at txn 1");
    assert_eq!(
        products_txn1.len(),
        0,
        "transaction IDs are global: txn 1 is the `users` insert, so `products` must be empty there"
    );
}

#[test]
fn test_snapshot_gc_retains_recent_snapshots() {
    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).expect("Failed to create storage engine");

    // The engine-owned SnapshotManager is hardwired to `GcConfig::default()`:
    // 3600 s minimum retention and a 1000-snapshot cap. There is no way to
    // inject a different GcConfig through `StorageEngine`, so this test pins
    // the retention side of that contract: freshly created snapshots are
    // NOT collectable.
    let snapshot_mgr = engine.snapshot_manager();

    // Create many snapshots, all milliseconds old.
    for i in 1..=20 {
        snapshot_mgr
            .register_snapshot(i * 100)
            .expect("Failed to register snapshot");
    }

    assert_eq!(snapshot_mgr.snapshot_count(), 20);

    // Run GC
    let removed = snapshot_mgr.gc_old_snapshots().expect("Failed to run GC");

    // Nothing is eligible: every snapshot is far younger than the 3600 s
    // minimum retention, and 20 is far below the 1000-snapshot cap.
    assert_eq!(
        removed, 0,
        "snapshots younger than min_retention_seconds (3600) must not be collected"
    );
    assert_eq!(
        snapshot_mgr.snapshot_count(),
        20,
        "GC must leave all 20 recent snapshots in place"
    );
}

#[test]
fn test_snapshot_recovery() {
    use tempfile::tempdir;

    let temp_dir = tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path();

    // Create engine, insert data, and close
    {
        let config = Config::default();
        let engine = StorageEngine::open(db_path, &config).expect("Failed to create storage engine");

        let schema = Schema {
            columns: vec![Column::new("id".to_string(), DataType::Int4).primary_key()],
        };

        let catalog = engine.catalog();
        catalog.create_table("test", schema).expect("Failed to create table");

        engine
            .insert_tuple_versioned("test", Tuple::new(vec![Value::Int4(1)]))
            .expect("Failed to insert");

        // Snapshots should be registered
        assert!(engine.snapshot_manager().snapshot_count() > 0);
    }

    // Reopen and verify snapshots were recovered
    {
        let config = Config::default();
        let engine = StorageEngine::open(db_path, &config).expect("Failed to reopen storage engine");

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
        alias: None,
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
        alias: None,
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
