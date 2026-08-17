//! Integration tests for materialized view auto-refresh functionality
//!
//! Tests end-to-end auto-refresh workflow including:
//! - Worker startup and shutdown
//! - Automatic staleness detection
//! - Scheduler integration
//! - System view queries
//! - Configuration updates
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_nano::sql::LogicalPlan;
use heliosdb_nano::storage::{
    AutoRefreshConfig, AutoRefreshWorker, MVScheduler, MaterializedViewCatalog, MaterializedViewMetadata,
    MvSystemViews, SchedulerConfig, StorageEngine,
};
use heliosdb_nano::{Column, Config, DataType, Schema, Value};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn test_auto_refresh_worker_lifecycle() {
    // Create storage and scheduler
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
    let scheduler_config = SchedulerConfig::default();
    let scheduler = Arc::new(MVScheduler::new(scheduler_config, Arc::clone(&storage)));

    // Create worker (disabled)
    let worker_config = AutoRefreshConfig::default();
    let mut worker = AutoRefreshWorker::new(worker_config, Arc::clone(&storage), Arc::clone(&scheduler));

    // Should not start when disabled
    assert!(!worker.is_running());
    worker.start().await.unwrap();
    assert!(!worker.is_running());

    // Enable and start
    let enabled_config = AutoRefreshConfig::default().with_enabled(true);
    worker.update_config(enabled_config);
    worker.start().await.unwrap();
    assert!(worker.is_running());

    // Stop
    worker.stop().await.unwrap();
    assert!(!worker.is_running());
}

// `test_auto_refresh_registration` and `test_auto_refresh_policy_update` were
// deleted here: they exercised `AutoRefreshWorker::{register_mv, unregister_mv,
// get_policy, update_policy}` and the `AutoRefreshPolicy` struct, none of which
// exist any more. Per-MV auto-refresh opt-in is no longer a worker-side policy
// registry — it lives in `MaterializedViewMetadata.metadata["auto_refresh"]`,
// which `perform_staleness_check` reads from the MV catalog. There is no
// per-MV interval/priority/concurrency knob to assert on at all: the worker
// applies its single `AutoRefreshConfig` to every view and always schedules
// with `Priority::Normal`. `test_system_views_status` below covers the
// surviving opt-in path.

#[test]
fn test_system_views_status() {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
    let scheduler_config = SchedulerConfig::default();
    let scheduler = Arc::new(MVScheduler::new(scheduler_config, Arc::clone(&storage)));

    // Create MV catalog and add a test view
    let mv_catalog = MaterializedViewCatalog::new(&storage);
    let schema = Schema::new(vec![Column::new("count", DataType::Int8)]);

    let query_plan = LogicalPlan::Scan {
        table_name: "test".to_string(),
        alias: None,
        schema: std::sync::Arc::new(schema.clone()),
        projection: None,
        as_of: None,
    };
    let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

    let mut metadata = MaterializedViewMetadata::new(
        "test_mv".to_string(),
        "SELECT COUNT(*) FROM test".to_string(),
        query_plan_bytes,
        vec!["test".to_string()],
        schema,
    );

    // Enable auto-refresh in metadata
    metadata.metadata.insert("auto_refresh".to_string(), "true".to_string());
    mv_catalog.create_view(metadata).unwrap();

    // Create system views
    let system_views = MvSystemViews::new(Arc::clone(&storage), scheduler);

    // Query status
    let statuses = system_views.pg_mv_auto_refresh_status().unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].mv_name, "test_mv");
    assert_eq!(statuses[0].auto_refresh_enabled, true);
}

#[test]
fn test_system_views_cpu_usage() {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
    let scheduler_config = SchedulerConfig::default();
    let scheduler = Arc::new(MVScheduler::new(scheduler_config, Arc::clone(&storage)));

    let system_views = MvSystemViews::new(storage, scheduler);

    // Query CPU usage
    let cpu_info = system_views.pg_mv_cpu_usage().unwrap();
    assert!(cpu_info.current_cpu_percent >= 0.0);
    assert!(cpu_info.current_cpu_percent <= 100.0);
    assert_eq!(cpu_info.max_cpu_percent, 50.0); // Default config
}

#[test]
fn test_system_views_scheduler_stats() {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
    let scheduler_config = SchedulerConfig::default();
    let scheduler = Arc::new(MVScheduler::new(scheduler_config, Arc::clone(&storage)));

    let system_views = MvSystemViews::new(storage, scheduler);

    // Query scheduler stats
    let stats = system_views.pg_mv_scheduler_stats().unwrap();
    assert_eq!(stats.queue_size, 0);
    assert_eq!(stats.running_tasks, 0);
}

#[tokio::test]
async fn test_config_runtime_updates() {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
    let scheduler_config = SchedulerConfig::default();
    let scheduler = Arc::new(MVScheduler::new(scheduler_config, Arc::clone(&storage)));

    let worker_config = AutoRefreshConfig::default();
    let worker = AutoRefreshWorker::new(worker_config, storage, scheduler);

    // Update interval
    let new_config = AutoRefreshConfig::default()
        .with_interval_seconds(120)
        .with_staleness_threshold(600)
        .with_max_cpu_percent(75.0);

    worker.update_config(new_config);

    let current = worker.config();
    assert_eq!(current.interval_seconds, 120);
    assert_eq!(current.staleness_threshold_seconds, 600);
    assert_eq!(current.max_cpu_percent, 75.0);
}

#[test]
fn test_refresh_history_schema() {
    let schema = MvSystemViews::history_schema();
    assert_eq!(schema.columns.len(), 8);

    let col_names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(col_names.contains(&"mv_name"));
    assert!(col_names.contains(&"start_time"));
    assert!(col_names.contains(&"success"));
    assert!(col_names.contains(&"rows_affected"));
}

#[test]
fn test_status_to_tuple_conversion() {
    use heliosdb_nano::storage::AutoRefreshStatus;

    let status = AutoRefreshStatus {
        mv_name: "test_mv".to_string(),
        auto_refresh_enabled: true,
        last_refresh: None,
        staleness_seconds: Some(300),
        threshold_seconds: 600,
        is_refreshing: false,
        refresh_strategy: "incremental".to_string(),
        row_count: Some(1000),
        base_table_count: 2,
    };

    let tuple = MvSystemViews::status_to_tuple(&status);
    assert_eq!(tuple.values.len(), 9);

    // Verify first value is mv_name
    match &tuple.values[0] {
        Value::String(name) => assert_eq!(name, "test_mv"),
        _ => panic!("Expected String for mv_name"),
    }

    // Verify auto_refresh_enabled
    match &tuple.values[1] {
        Value::Boolean(enabled) => assert_eq!(*enabled, true),
        _ => panic!("Expected Boolean for auto_refresh_enabled"),
    }
}

#[test]
fn test_cpu_usage_to_tuple_conversion() {
    use heliosdb_nano::storage::CpuUsageInfo;

    let cpu_info = CpuUsageInfo {
        current_cpu_percent: 45.5,
        max_cpu_percent: 70.0,
        is_throttled: false,
        active_tasks: 2,
        queued_tasks: 5,
    };

    let tuple = MvSystemViews::cpu_usage_to_tuple(&cpu_info);
    assert_eq!(tuple.values.len(), 5);

    match &tuple.values[0] {
        Value::Float8(percent) => assert_eq!(*percent, 45.5),
        _ => panic!("Expected Float8"),
    }

    match &tuple.values[2] {
        Value::Boolean(throttled) => assert_eq!(*throttled, false),
        _ => panic!("Expected Boolean"),
    }
}

#[tokio::test]
async fn test_staleness_based_refresh_trigger() {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
    let scheduler_config = SchedulerConfig::default();
    let scheduler = Arc::new(MVScheduler::new(scheduler_config, Arc::clone(&storage)));

    // Create MV with metadata
    let mv_catalog = MaterializedViewCatalog::new(&storage);
    let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);

    let query_plan = LogicalPlan::Scan {
        table_name: "test".to_string(),
        alias: None,
        schema: std::sync::Arc::new(schema.clone()),
        projection: None,
        as_of: None,
    };
    let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

    let mut metadata = MaterializedViewMetadata::new(
        "stale_view".to_string(),
        "SELECT * FROM test".to_string(),
        query_plan_bytes,
        vec!["test".to_string()],
        schema,
    );
    metadata.metadata.insert("auto_refresh".to_string(), "true".to_string());
    mv_catalog.create_view(metadata).unwrap();

    // Create worker with short intervals for testing
    let worker_config = AutoRefreshConfig::default()
        .with_enabled(true)
        .with_interval_seconds(1)
        .with_staleness_threshold(0); // Immediate staleness

    let mut worker = AutoRefreshWorker::new(worker_config, Arc::clone(&storage), scheduler);

    // The MV opts in via its own catalog metadata (`auto_refresh = true`, set
    // above) — the worker no longer keeps a per-MV registration of its own.

    // Start worker
    worker.start().await.unwrap();

    // Wait for potential refresh check
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Stop worker
    worker.stop().await.unwrap();

    // Note: In a real test, we'd verify that a refresh was scheduled
    // For now, we're just testing that the worker runs without errors
}

// `test_list_registered_mvs` was deleted here: `AutoRefreshWorker::
// list_registered_mvs` no longer exists, and neither does the registry it
// listed. The set of views the worker acts on is now derived on every pass
// from `MaterializedViewCatalog::list_views()` filtered by the
// `auto_refresh` metadata key, so there is no worker-side list to enumerate.
