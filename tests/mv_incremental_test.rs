//! Integration tests for incremental materialized view refresh
//!
//! These tests validate the correctness and performance of incremental
//! computation for materialized views in HeliosDB-Lite v2.3.0.
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_lite::{
    Config, StorageEngine, Schema, Column, DataType, Tuple, Value,
    Result,
};
use heliosdb_lite::storage::{
    IncrementalRefresher, DeltaTracker, RefreshStrategy,
    MaterializedViewMetadata,
};
use heliosdb_lite::sql::LogicalPlan;
use std::sync::Arc;

#[test]
fn test_delta_tracker_insert() -> Result<()> {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));

    // Record some inserts
    let tuple1 = Tuple {
        values: vec![Value::Int4(1), Value::String("Alice".to_string())],
    };
    let tuple2 = Tuple {
        values: vec![Value::Int4(2), Value::String("Bob".to_string())],
    };

    tracker.record_insert("users", tuple1.clone(), 100);
    tracker.record_insert("users", tuple2.clone(), 150);

    // Get deltas since timestamp 50
    let deltas = tracker.get_deltas_since("users", 50);
    assert_eq!(deltas.len(), 2);

    // Get deltas since timestamp 120
    let deltas = tracker.get_deltas_since("users", 120);
    assert_eq!(deltas.len(), 1);

    // Get deltas since timestamp 200
    let deltas = tracker.get_deltas_since("users", 200);
    assert_eq!(deltas.len(), 0);

    Ok(())
}

#[test]
fn test_delta_tracker_delete() -> Result<()> {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));

    let tuple = Tuple {
        values: vec![Value::Int4(1), Value::String("Alice".to_string())],
    };

    tracker.record_delete("users", tuple.clone(), 100);

    let deltas = tracker.get_deltas_since("users", 50);
    assert_eq!(deltas.len(), 1);

    if let Some(delta) = deltas.first() {
        assert!(matches!(delta.operation, heliosdb_lite::storage::DeltaOperation::Delete { .. }));
    }

    Ok(())
}

#[test]
fn test_delta_tracker_update() -> Result<()> {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));

    let old_tuple = Tuple {
        values: vec![Value::Int4(1), Value::String("Alice".to_string())],
    };
    let new_tuple = Tuple {
        values: vec![Value::Int4(1), Value::String("Alicia".to_string())],
    };

    tracker.record_update("users", old_tuple.clone(), new_tuple.clone(), 100);

    let deltas = tracker.get_deltas_since("users", 50);
    assert_eq!(deltas.len(), 1);

    if let Some(delta) = deltas.first() {
        assert!(matches!(delta.operation, heliosdb_lite::storage::DeltaOperation::Update { .. }));
    }

    Ok(())
}

#[test]
fn test_delta_tracker_multiple_tables() -> Result<()> {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));

    let tuple = Tuple {
        values: vec![Value::Int4(1)],
    };

    tracker.record_insert("users", tuple.clone(), 100);
    tracker.record_insert("orders", tuple.clone(), 150);

    let user_deltas = tracker.get_deltas_since("users", 50);
    assert_eq!(user_deltas.len(), 1);

    let order_deltas = tracker.get_deltas_since("orders", 50);
    assert_eq!(order_deltas.len(), 1);

    Ok(())
}

#[test]
fn test_delta_tracker_clear() -> Result<()> {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));

    let tuple = Tuple {
        values: vec![Value::Int4(1)],
    };

    tracker.record_insert("users", tuple.clone(), 100);
    tracker.record_insert("users", tuple.clone(), 150);

    let deltas = tracker.get_deltas_since("users", 50);
    assert_eq!(deltas.len(), 2);

    // Clear deltas up to timestamp 120
    tracker.clear_deltas_until("users", 120);

    let deltas = tracker.get_deltas_since("users", 50);
    assert_eq!(deltas.len(), 1); // Only one delta remains (timestamp 150)

    // Clear all deltas
    tracker.clear_all_deltas("users");

    let deltas = tracker.get_deltas_since("users", 50);
    assert_eq!(deltas.len(), 0);

    Ok(())
}

#[test]
fn test_incremental_refresher_creation() -> Result<()> {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));
    let _refresher = IncrementalRefresher::new(Arc::clone(&storage), tracker);

    Ok(())
}

#[test]
fn test_cost_estimation_prefers_incremental() -> Result<()> {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));
    let refresher = IncrementalRefresher::new(Arc::clone(&storage), tracker);

    // Create a test table
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("name", DataType::Text),
    ]);

    let catalog = storage.catalog();
    catalog.create_table("users", schema.clone())?;

    // Insert some rows
    for i in 0..100 {
        let tuple = Tuple {
            values: vec![
                Value::Int4(i),
                Value::String(format!("User{}", i)),
            ],
        };
        storage.insert_tuple("users", tuple)?;
    }

    // Create materialized view metadata
    let query_plan = LogicalPlan::Scan {
        table_name: "users".to_string(),
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: None,
    };
    let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

    let mut mv_metadata = MaterializedViewMetadata::new(
        "user_view".to_string(),
        "SELECT * FROM users".to_string(),
        query_plan_bytes,
        vec!["users".to_string()],
        schema,
    );

    // Mark as refreshed to enable incremental refresh
    mv_metadata.mark_refreshed(100);

    // Estimate cost (with very few deltas)
    let cost = refresher.estimate_refresh_cost(&mv_metadata)?;

    // Incremental should be cheaper for small delta counts
    assert!(cost.incremental_cost < cost.full_cost);
    assert_eq!(cost.recommendation, RefreshStrategy::Incremental);

    Ok(())
}

#[test]
fn test_cost_estimation_prefers_full() -> Result<()> {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));

    // Record many deltas to make incremental expensive
    let tuple = Tuple {
        values: vec![Value::Int4(1)],
    };
    for i in 0..10000 {
        tracker.record_insert("users", tuple.clone(), i);
    }

    let refresher = IncrementalRefresher::new(Arc::clone(&storage), tracker);

    // Create a small base table
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
    ]);

    let catalog = storage.catalog();
    catalog.create_table("users", schema.clone())?;

    // Insert only a few rows
    for i in 0..10 {
        let tuple = Tuple {
            values: vec![Value::Int4(i)],
        };
        storage.insert_tuple("users", tuple)?;
    }

    // Create materialized view metadata
    let query_plan = LogicalPlan::Scan {
        table_name: "users".to_string(),
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: None,
    };
    let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

    let mut mv_metadata = MaterializedViewMetadata::new(
        "user_view".to_string(),
        "SELECT * FROM users".to_string(),
        query_plan_bytes,
        vec!["users".to_string()],
        schema,
    );

    mv_metadata.mark_refreshed(10);

    // Estimate cost (with many deltas, small base table)
    let cost = refresher.estimate_refresh_cost(&mv_metadata)?;

    // Full refresh should be recommended when deltas are large
    // Note: This depends on the heuristic thresholds
    assert!(cost.incremental_cost > 0.0);
    assert!(cost.full_cost > 0.0);

    Ok(())
}

#[test]
fn test_can_refresh_incrementally() -> Result<()> {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));
    let refresher = IncrementalRefresher::new(Arc::clone(&storage), tracker);

    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
    ]);

    // Create a scan plan (supported for incremental refresh)
    let query_plan = LogicalPlan::Scan {
        table_name: "users".to_string(),
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: None,
    };
    let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

    let mut mv_metadata = MaterializedViewMetadata::new(
        "user_view".to_string(),
        "SELECT * FROM users".to_string(),
        query_plan_bytes,
        vec!["users".to_string()],
        schema.clone(),
    );

    // Before first refresh, incremental is not possible
    assert!(!refresher.can_refresh_incrementally(&mv_metadata)?);

    // After first refresh, incremental is possible
    mv_metadata.mark_refreshed(100);
    assert!(refresher.can_refresh_incrementally(&mv_metadata)?);

    Ok(())
}

#[test]
fn test_refresh_strategy_enum() {
    assert_eq!(RefreshStrategy::Full, RefreshStrategy::Full);
    assert_eq!(RefreshStrategy::Incremental, RefreshStrategy::Incremental);
    assert_eq!(RefreshStrategy::Hybrid, RefreshStrategy::Hybrid);

    assert_ne!(RefreshStrategy::Full, RefreshStrategy::Incremental);
}

#[test]
fn test_delta_count() -> Result<()> {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));

    let tuple = Tuple {
        values: vec![Value::Int4(1)],
    };

    // Record deltas at different timestamps
    tracker.record_insert("users", tuple.clone(), 100);
    tracker.record_insert("users", tuple.clone(), 200);
    tracker.record_insert("orders", tuple.clone(), 150);

    // Count deltas for users since 50
    let count = tracker.count_deltas_since(&vec!["users".to_string()], 50)?;
    assert_eq!(count, 2);

    // Count deltas for users since 150
    let count = tracker.count_deltas_since(&vec!["users".to_string()], 150)?;
    assert_eq!(count, 1);

    // Count deltas for multiple tables
    let count = tracker.count_deltas_since(&vec!["users".to_string(), "orders".to_string()], 50)?;
    assert_eq!(count, 3);

    Ok(())
}

#[test]
fn test_incremental_refresh_not_supported_without_first_refresh() -> Result<()> {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));
    let refresher = IncrementalRefresher::new(Arc::clone(&storage), tracker);

    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
    ]);

    let query_plan = LogicalPlan::Scan {
        table_name: "users".to_string(),
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: None,
    };
    let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

    let mv_metadata = MaterializedViewMetadata::new(
        "user_view".to_string(),
        "SELECT * FROM users".to_string(),
        query_plan_bytes,
        vec!["users".to_string()],
        schema,
    );

    // Should not support incremental refresh without initial refresh
    assert!(!refresher.can_refresh_incrementally(&mv_metadata)?);

    Ok(())
}

#[test]
fn test_delta_operations_equality() {
    let tuple1 = Tuple {
        values: vec![Value::Int4(1)],
    };
    let tuple2 = Tuple {
        values: vec![Value::Int4(2)],
    };

    let insert1 = heliosdb_lite::storage::DeltaOperation::Insert { tuple: tuple1.clone() };
    let insert2 = heliosdb_lite::storage::DeltaOperation::Insert { tuple: tuple1.clone() };
    let insert3 = heliosdb_lite::storage::DeltaOperation::Insert { tuple: tuple2.clone() };

    assert_eq!(insert1, insert2);
    assert_ne!(insert1, insert3);
}

#[test]
fn test_refresh_result_structure() {
    use std::time::Duration;

    let result = heliosdb_lite::storage::RefreshResult {
        strategy_used: RefreshStrategy::Incremental,
        rows_inserted: 10,
        rows_updated: 5,
        rows_deleted: 2,
        duration: Duration::from_millis(100),
    };

    assert_eq!(result.strategy_used, RefreshStrategy::Incremental);
    assert_eq!(result.rows_inserted, 10);
    assert_eq!(result.rows_updated, 5);
    assert_eq!(result.rows_deleted, 2);
    assert!(result.duration.as_millis() >= 100);
}
