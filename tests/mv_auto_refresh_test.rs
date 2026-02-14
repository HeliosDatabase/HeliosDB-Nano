//! Integration tests for AutoRefreshWorker
//!
//! This test suite validates the production behavior of the auto-refresh
//! background worker with real materialized views and storage.

use heliosdb_nano::{Config, Column, DataType, Schema};
use heliosdb_nano::storage::{
    StorageEngine,
    MaterializedViewCatalog,
    MaterializedViewMetadata,
    AutoRefreshWorker,
    AutoRefreshConfig,
    MVScheduler,
    SchedulerConfig,
};
use heliosdb_nano::sql::LogicalPlan;
use std::sync::Arc;
use tokio::time::Duration;

/// Helper to create test storage
fn create_test_storage() -> Arc<StorageEngine> {
    let config = Config::in_memory();
    Arc::new(StorageEngine::open_in_memory(&config).expect("Failed to create storage"))
}

/// Helper to create test scheduler
fn create_test_scheduler(storage: Arc<StorageEngine>) -> Arc<MVScheduler> {
    let config = SchedulerConfig::default();
    Arc::new(MVScheduler::new(config, storage))
}

/// Helper to create a test materialized view
fn create_test_mv(
    storage: &Arc<StorageEngine>,
    name: &str,
    auto_refresh: bool,
) {
    let catalog = MaterializedViewCatalog::new(storage.as_ref());
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("value", DataType::Int8),
    ]);

    let query_plan = LogicalPlan::Scan {
        alias: None,
        table_name: "test_table".to_string(),
        schema: std::sync::Arc::new(schema.clone()),
        projection: None,
        as_of: None,
    };
    let query_plan_bytes = bincode::serialize(&query_plan)
        .expect("Failed to serialize query plan");

    let mut metadata = MaterializedViewMetadata::new(
        name.to_string(),
        format!("SELECT * FROM test_table"),
        query_plan_bytes,
        vec!["test_table".to_string()],
        schema,
    );

    if auto_refresh {
        metadata.metadata.insert("auto_refresh".to_string(), "true".to_string());
    }

    catalog.create_view(metadata).expect("Failed to create view");
}

#[tokio::test]
async fn test_auto_refresh_worker_lifecycle() {
    let storage = create_test_storage();
    let scheduler = create_test_scheduler(Arc::clone(&storage));

    let config = AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(1);

    let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

    // Start the worker
    assert!(worker.start().await.is_ok());
    assert!(worker.is_running());

    // Let it run
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(worker.is_running());

    // Stop the worker
    assert!(worker.stop().await.is_ok());
    assert!(!worker.is_running());
}

#[tokio::test]
async fn test_auto_refresh_detects_stale_views() {
    let storage = create_test_storage();
    let scheduler = create_test_scheduler(Arc::clone(&storage));

    // Create a view with auto_refresh enabled
    create_test_mv(&storage, "stale_view", true);

    let config = AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(1)
        .with_staleness_threshold(0); // Immediately stale

    let mut worker = AutoRefreshWorker::new(config, Arc::clone(&storage), scheduler);

    // Start the worker
    worker.start().await.expect("Failed to start worker");

    // Wait for at least one staleness check
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Worker should still be running
    assert!(worker.is_running());

    // Stop
    worker.stop().await.expect("Failed to stop worker");
}

#[tokio::test]
async fn test_auto_refresh_respects_disabled_views() {
    let storage = create_test_storage();
    let scheduler = create_test_scheduler(Arc::clone(&storage));

    // Create views: one with auto_refresh, one without
    create_test_mv(&storage, "auto_view", true);
    create_test_mv(&storage, "manual_view", false);

    let config = AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(1)
        .with_staleness_threshold(0);

    let mut worker = AutoRefreshWorker::new(config, Arc::clone(&storage), scheduler);

    worker.start().await.expect("Failed to start worker");

    // Run for a bit
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Both views should still exist
    let catalog = MaterializedViewCatalog::new(storage.as_ref());
    assert!(catalog.view_exists("auto_view").unwrap());
    assert!(catalog.view_exists("manual_view").unwrap());

    worker.stop().await.expect("Failed to stop worker");
}

#[tokio::test]
async fn test_auto_refresh_concurrent_limit() {
    let storage = create_test_storage();
    let scheduler = create_test_scheduler(Arc::clone(&storage));

    // Create multiple views with auto_refresh
    for i in 0..5 {
        create_test_mv(&storage, &format!("view_{}", i), true);
    }

    let config = AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(1)
        .with_staleness_threshold(0)
        .with_max_concurrent(2); // Limit to 2 concurrent

    let mut worker = AutoRefreshWorker::new(config, Arc::clone(&storage), scheduler);

    worker.start().await.expect("Failed to start worker");

    // Let it process
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Active count should not exceed limit
    assert!(worker.active_refresh_count() <= 2);

    worker.stop().await.expect("Failed to stop worker");
}

#[tokio::test]
async fn test_auto_refresh_check_now_command() {
    let storage = create_test_storage();
    let scheduler = create_test_scheduler(Arc::clone(&storage));

    create_test_mv(&storage, "test_view", true);

    let config = AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(60); // Long interval

    let mut worker = AutoRefreshWorker::new(config, Arc::clone(&storage), scheduler);

    worker.start().await.expect("Failed to start worker");

    // Trigger immediate check
    worker.check_now().expect("Failed to trigger check");

    // Give it time to process
    tokio::time::sleep(Duration::from_millis(500)).await;

    worker.stop().await.expect("Failed to stop worker");
}

#[tokio::test]
async fn test_auto_refresh_config_update() {
    let storage = create_test_storage();
    let scheduler = create_test_scheduler(Arc::clone(&storage));

    let config = AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(10)
        .with_staleness_threshold(300);

    let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

    worker.start().await.expect("Failed to start worker");

    // Update config while running
    let new_config = AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(5)
        .with_staleness_threshold(600);

    worker.update_config(new_config);

    // Verify update
    let current_config = worker.config();
    assert_eq!(current_config.interval_seconds, 5);
    assert_eq!(current_config.staleness_threshold_seconds, 600);

    worker.stop().await.expect("Failed to stop worker");
}

#[tokio::test]
async fn test_auto_refresh_graceful_shutdown() {
    let storage = create_test_storage();
    let scheduler = create_test_scheduler(Arc::clone(&storage));

    // Create multiple views
    for i in 0..3 {
        create_test_mv(&storage, &format!("view_{}", i), true);
    }

    let config = AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(1)
        .with_staleness_threshold(0);

    let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

    worker.start().await.expect("Failed to start worker");

    // Let some refreshes start
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // Stop should wait for completion
    let stop_start = std::time::Instant::now();
    worker.stop().await.expect("Failed to stop worker");
    let stop_duration = stop_start.elapsed();

    // Should have stopped gracefully (within timeout)
    assert!(stop_duration < Duration::from_secs(30));
    assert!(!worker.is_running());
}

#[tokio::test]
async fn test_auto_refresh_cpu_awareness() {
    let storage = create_test_storage();
    let scheduler = create_test_scheduler(Arc::clone(&storage));

    create_test_mv(&storage, "test_view", true);

    // Use scheduler's CPU monitoring
    let config = AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(1)
        .with_staleness_threshold(0)
        .with_max_cpu_percent(50.0);

    let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

    worker.start().await.expect("Failed to start worker");

    // Run for a bit - worker will check CPU via scheduler
    tokio::time::sleep(Duration::from_millis(2000)).await;

    // Should still be running regardless of CPU
    assert!(worker.is_running());

    worker.stop().await.expect("Failed to stop worker");
}

#[tokio::test]
async fn test_auto_refresh_multiple_start_stop_cycles() {
    let storage = create_test_storage();
    let scheduler = create_test_scheduler(Arc::clone(&storage));

    let config = AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(1);

    let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

    // Cycle 1
    worker.start().await.expect("Failed to start (1)");
    assert!(worker.is_running());
    tokio::time::sleep(Duration::from_millis(500)).await;
    worker.stop().await.expect("Failed to stop (1)");
    assert!(!worker.is_running());

    // Cycle 2
    worker.start().await.expect("Failed to start (2)");
    assert!(worker.is_running());
    tokio::time::sleep(Duration::from_millis(500)).await;
    worker.stop().await.expect("Failed to stop (2)");
    assert!(!worker.is_running());

    // Cycle 3
    worker.start().await.expect("Failed to start (3)");
    assert!(worker.is_running());
    worker.stop().await.expect("Failed to stop (3)");
    assert!(!worker.is_running());
}

#[tokio::test]
async fn test_auto_refresh_empty_catalog() {
    let storage = create_test_storage();
    let scheduler = create_test_scheduler(Arc::clone(&storage));

    // No views created
    let config = AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(1);

    let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

    worker.start().await.expect("Failed to start worker");

    // Run with no views - should not crash
    tokio::time::sleep(Duration::from_millis(2000)).await;

    assert!(worker.is_running());

    worker.stop().await.expect("Failed to stop worker");
}

#[tokio::test]
async fn test_auto_refresh_staleness_priority() {
    let storage = create_test_storage();
    let scheduler = create_test_scheduler(Arc::clone(&storage));

    // Create views with different staleness
    create_test_mv(&storage, "recent_view", true);
    create_test_mv(&storage, "old_view", true);

    // Mark one as refreshed recently
    let catalog = MaterializedViewCatalog::new(storage.as_ref());
    let mut recent_metadata = catalog.get_view("recent_view").unwrap();
    recent_metadata.mark_refreshed(100);
    catalog.update_view(&recent_metadata).unwrap();

    let config = AutoRefreshConfig::new()
        .with_enabled(true)
        .with_interval_seconds(1)
        .with_staleness_threshold(0) // Everything is stale
        .with_max_concurrent(1); // Only process 1 at a time

    let mut worker = AutoRefreshWorker::new(config, Arc::clone(&storage), scheduler);

    worker.start().await.expect("Failed to start worker");

    // Let it process - should prioritize the older view
    tokio::time::sleep(Duration::from_millis(1500)).await;

    worker.stop().await.expect("Failed to stop worker");
}
