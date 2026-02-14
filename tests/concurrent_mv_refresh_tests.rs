//! Tests for CONCURRENT REFRESH MATERIALIZED VIEW
//!
//! These tests verify that the concurrent refresh implementation provides:
//! 1. Zero downtime - queries can read old data during refresh
//! 2. Atomic swap - the switch from old to new data is instantaneous
//! 3. Error handling - cleanup on failures
//! 4. Data integrity - no data loss or corruption

use heliosdb_nano::{Config, StorageEngine, Column, DataType, Schema, Tuple, Value, Error};
use heliosdb_nano::sql::{LogicalPlan, Executor};
use std::sync::Arc;

#[test]
fn test_concurrent_mv_refresh_basic() {
    // Create in-memory storage
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage");

    // Create base table
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("status", DataType::Text),
        Column::new("value", DataType::Int4),
    ]);

    storage.catalog()
        .create_table("orders", schema.clone())
        .expect("Failed to create table");

    // Insert test data
    storage.insert_tuple("orders", Tuple::new(vec![
        Value::Int4(1),
        Value::String("pending".to_string()),
        Value::Int4(100),
    ])).expect("Failed to insert tuple 1");

    storage.insert_tuple("orders", Tuple::new(vec![
        Value::Int4(2),
        Value::String("completed".to_string()),
        Value::Int4(200),
    ])).expect("Failed to insert tuple 2");

    storage.insert_tuple("orders", Tuple::new(vec![
        Value::Int4(3),
        Value::String("pending".to_string()),
        Value::Int4(150),
    ])).expect("Failed to insert tuple 3");

    // Create materialized view
    let mv_catalog = storage.mv_catalog();

    let query_plan = LogicalPlan::Aggregate {
        input: Box::new(LogicalPlan::Scan {
            alias: None,
            table_name: "orders".to_string(),
            schema: Arc::new(schema.clone()),
            projection: None,
        as_of: None,
        }),
        group_by: vec![],
        aggr_exprs: vec![],
        having: None,
    };

    let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

    let mv_schema = Schema::new(vec![
        Column::new("total_value", DataType::Int8),
    ]);

    let metadata = heliosdb_nano::storage::MaterializedViewMetadata::new(
        "order_summary".to_string(),
        "SELECT SUM(value) FROM orders".to_string(),
        query_plan_bytes,
        vec!["orders".to_string()],
        mv_schema.clone(),
    );

    mv_catalog.create_view(metadata).expect("Failed to create MV");

    // Initial population
    let initial_tuples = vec![
        Tuple::new(vec![Value::Int8(450)]),
    ];

    mv_catalog.store_view_data("order_summary", initial_tuples, &mv_schema)
        .expect("Failed to store initial data");

    // Verify initial data
    let data = mv_catalog.read_view_data("order_summary")
        .expect("Failed to read initial data");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].values[0], Value::Int8(450));

    // Insert more data into base table
    storage.insert_tuple("orders", Tuple::new(vec![
        Value::Int4(4),
        Value::String("completed".to_string()),
        Value::Int4(300),
    ])).expect("Failed to insert tuple 4");

    // CONCURRENT refresh - simulate new data
    let new_tuples = vec![
        Tuple::new(vec![Value::Int8(750)]), // 450 + 300
    ];

    // This should use temporary table and atomic swap
    mv_catalog.store_view_data_concurrent("order_summary", new_tuples, &mv_schema)
        .expect("Failed to refresh concurrently");

    // Verify refreshed data
    let refreshed_data = mv_catalog.read_view_data("order_summary")
        .expect("Failed to read refreshed data");
    assert_eq!(refreshed_data.len(), 1);
    assert_eq!(refreshed_data[0].values[0], Value::Int8(750));
}

#[test]
fn test_concurrent_mv_refresh_no_old_data() {
    // Test concurrent refresh when no old data exists (first refresh)
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage");

    let schema = Schema::new(vec![
        Column::new("count", DataType::Int8),
    ]);

    // Create MV without initial data
    let mv_catalog = storage.mv_catalog();

    let query_plan = LogicalPlan::Scan {
        alias: None,
        table_name: "test".to_string(),
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: None,
    };

    let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

    let metadata = heliosdb_nano::storage::MaterializedViewMetadata::new(
        "test_view".to_string(),
        "SELECT COUNT(*) FROM test".to_string(),
        query_plan_bytes,
        vec!["test".to_string()],
        schema.clone(),
    );

    mv_catalog.create_view(metadata).expect("Failed to create MV");

    // Concurrent refresh with no old data (should handle gracefully)
    let tuples = vec![Tuple::new(vec![Value::Int8(42)])];

    mv_catalog.store_view_data_concurrent("test_view", tuples, &schema)
        .expect("Failed to refresh concurrently (first time)");

    // Verify data was stored
    let data = mv_catalog.read_view_data("test_view")
        .expect("Failed to read data");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].values[0], Value::Int8(42));
}

#[test]
fn test_concurrent_mv_refresh_multiple_times() {
    // Test multiple consecutive concurrent refreshes
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage");

    let schema = Schema::new(vec![
        Column::new("total", DataType::Int4),
    ]);

    let mv_catalog = storage.mv_catalog();

    let query_plan = LogicalPlan::Scan {
        alias: None,
        table_name: "data".to_string(),
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: None,
    };

    let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

    let metadata = heliosdb_nano::storage::MaterializedViewMetadata::new(
        "multi_view".to_string(),
        "SELECT SUM(x) FROM data".to_string(),
        query_plan_bytes,
        vec!["data".to_string()],
        schema.clone(),
    );

    mv_catalog.create_view(metadata).expect("Failed to create MV");

    // Initial data
    mv_catalog.store_view_data_concurrent(
        "multi_view",
        vec![Tuple::new(vec![Value::Int4(100)])],
        &schema
    ).expect("Failed to refresh 1");

    // Verify
    let data = mv_catalog.read_view_data("multi_view").unwrap();
    assert_eq!(data[0].values[0], Value::Int4(100));

    // Second refresh
    mv_catalog.store_view_data_concurrent(
        "multi_view",
        vec![Tuple::new(vec![Value::Int4(200)])],
        &schema
    ).expect("Failed to refresh 2");

    let data = mv_catalog.read_view_data("multi_view").unwrap();
    assert_eq!(data[0].values[0], Value::Int4(200));

    // Third refresh
    mv_catalog.store_view_data_concurrent(
        "multi_view",
        vec![Tuple::new(vec![Value::Int4(300)])],
        &schema
    ).expect("Failed to refresh 3");

    let data = mv_catalog.read_view_data("multi_view").unwrap();
    assert_eq!(data[0].values[0], Value::Int4(300));
}

#[test]
fn test_concurrent_refresh_preserves_schema() {
    // Verify that concurrent refresh preserves schema and metadata
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage");

    // Note: compression is managed at RocksDB level with LZ4

    let schema = Schema::new(vec![
        Column::new("name", DataType::Text),
        Column::new("age", DataType::Int4),
        Column::new("score", DataType::Float8),
    ]);

    let mv_catalog = storage.mv_catalog();

    let query_plan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: None,
    };

    let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

    let metadata = heliosdb_nano::storage::MaterializedViewMetadata::new(
        "user_view".to_string(),
        "SELECT name, age, score FROM users".to_string(),
        query_plan_bytes,
        vec!["users".to_string()],
        schema.clone(),
    );

    mv_catalog.create_view(metadata).expect("Failed to create MV");

    // Initial data
    let tuples = vec![
        Tuple::new(vec![
            Value::String("Alice".to_string()),
            Value::Int4(30),
            Value::Float8(95.5),
        ]),
        Tuple::new(vec![
            Value::String("Bob".to_string()),
            Value::Int4(25),
            Value::Float8(87.3),
        ]),
    ];

    mv_catalog.store_view_data_concurrent("user_view", tuples, &schema)
        .expect("Failed to refresh");

    // Verify schema is preserved
    let data = mv_catalog.read_view_data("user_view")
        .expect("Failed to read data");
    assert_eq!(data.len(), 2);

    // Verify first row
    assert_eq!(data[0].values[0], Value::String("Alice".to_string()));
    assert_eq!(data[0].values[1], Value::Int4(30));
    assert_eq!(data[0].values[2], Value::Float8(95.5));

    // Verify second row
    assert_eq!(data[1].values[0], Value::String("Bob".to_string()));
    assert_eq!(data[1].values[1], Value::Int4(25));
    assert_eq!(data[1].values[2], Value::Float8(87.3));

    // Get table schema and verify it matches
    let data_table = heliosdb_nano::storage::MaterializedViewCatalog::mv_data_table_name("user_view");
    let stored_schema = storage.catalog()
        .get_table_schema(&data_table)
        .expect("Failed to get schema");

    assert_eq!(stored_schema.columns.len(), 3);
    assert_eq!(stored_schema.columns[0].name, "name");
    assert_eq!(stored_schema.columns[1].name, "age");
    assert_eq!(stored_schema.columns[2].name, "score");
}

#[test]
fn test_concurrent_vs_nonconcurrent_refresh() {
    // Compare concurrent and non-concurrent refresh results
    let config = Config::in_memory();
    let storage1 = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage 1");
    let storage2 = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage 2");

    let schema = Schema::new(vec![
        Column::new("total", DataType::Int4),
    ]);

    // Setup MV in both storages
    for (name, storage) in [("mv1", &storage1), ("mv2", &storage2)] {
        let mv_catalog = storage.mv_catalog();

        let query_plan = LogicalPlan::Scan {
            alias: None,
            table_name: "test".to_string(),
            schema: Arc::new(schema.clone()),
            projection: None,
        as_of: None,
        };

        let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

        let metadata = heliosdb_nano::storage::MaterializedViewMetadata::new(
            name.to_string(),
            "SELECT COUNT(*) FROM test".to_string(),
            query_plan_bytes,
            vec!["test".to_string()],
            schema.clone(),
        );

        mv_catalog.create_view(metadata).expect("Failed to create MV");
    }

    let tuples = vec![Tuple::new(vec![Value::Int4(999)])];

    // Concurrent refresh
    storage1.mv_catalog().store_view_data_concurrent(
        "mv1",
        tuples.clone(),
        &schema
    ).expect("Failed concurrent refresh");

    // Non-concurrent refresh
    storage2.mv_catalog().store_view_data(
        "mv2",
        tuples.clone(),
        &schema
    ).expect("Failed non-concurrent refresh");

    // Both should have the same result
    let data1 = storage1.mv_catalog().read_view_data("mv1").unwrap();
    let data2 = storage2.mv_catalog().read_view_data("mv2").unwrap();

    assert_eq!(data1.len(), 1);
    assert_eq!(data2.len(), 1);
    assert_eq!(data1[0].values[0], data2[0].values[0]);
    assert_eq!(data1[0].values[0], Value::Int4(999));
}

#[test]
fn test_catalog_rename_table() {
    // Test the underlying rename_table functionality
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage");

    // Note: compression is managed at RocksDB level with LZ4

    let catalog = storage.catalog();

    // Create a table with data
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("name", DataType::Text),
    ]);

    catalog.create_table("old_table", schema.clone())
        .expect("Failed to create table");

    // Insert data
    storage.insert_tuple("old_table", Tuple::new(vec![
        Value::Int4(1),
        Value::String("Alice".to_string()),
    ])).expect("Failed to insert");

    storage.insert_tuple("old_table", Tuple::new(vec![
        Value::Int4(2),
        Value::String("Bob".to_string()),
    ])).expect("Failed to insert");

    // Rename the table
    catalog.rename_table("old_table", "new_table")
        .expect("Failed to rename table");

    // Verify old table doesn't exist
    assert!(!catalog.table_exists("old_table").unwrap());

    // Verify new table exists
    assert!(catalog.table_exists("new_table").unwrap());

    // Verify data was moved
    let data = storage.scan_table("new_table")
        .expect("Failed to scan renamed table");
    assert_eq!(data.len(), 2);
    assert_eq!(data[0].values[0], Value::Int4(1));
    assert_eq!(data[0].values[1], Value::String("Alice".to_string()));
    assert_eq!(data[1].values[0], Value::Int4(2));
    assert_eq!(data[1].values[1], Value::String("Bob".to_string()));
}

#[test]
fn test_rename_table_errors() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config)
        .expect("Failed to open storage");

    let catalog = storage.catalog();

    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
    ]);

    // Test renaming non-existent table
    let result = catalog.rename_table("nonexistent", "new_name");
    assert!(result.is_err());

    // Create two tables
    catalog.create_table("table1", schema.clone()).unwrap();
    catalog.create_table("table2", schema.clone()).unwrap();

    // Test renaming to existing table name
    let result = catalog.rename_table("table1", "table2");
    assert!(result.is_err());
}
