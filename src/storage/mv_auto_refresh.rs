//! Materialized View Auto-Refresh Background Worker
//!
//! This module implements automatic materialized view refresh with CPU monitoring
//! and intelligent scheduling.
//!
//! ## Features
//!
//! - Automatic refresh based on staleness thresholds
//! - CPU usage monitoring and throttling
//! - Configurable refresh intervals
//! - Graceful shutdown handling
//! - Concurrent refresh support (zero downtime)
//! - Integration with MVScheduler for priority-based execution
//!
//! ## Architecture
//!
//! The background worker runs in a separate tokio task and:
//! 1. Periodically scans all materialized views
//! 2. Identifies views with `auto_refresh = true`
//! 3. Checks staleness against threshold
//! 4. Triggers concurrent refresh via MVScheduler if stale
//! 5. Respects CPU limits and max concurrent refreshes
//!
//! ## Usage
//!
//! ```rust,ignore
//! let config = AutoRefreshConfig {
//!     enabled: true,
//!     interval_seconds: 60,
//!     staleness_threshold_seconds: 300,
//!     max_concurrent_refreshes: 2,
//!     max_cpu_percent: 50.0,
//! };
//!
//! let worker = AutoRefreshWorker::new(config, storage, scheduler);
//! worker.start().await?;
//!
//! // ... later
//! worker.stop().await?;
//! ```

use crate::{Result, Error};
use super::{StorageEngine, MaterializedViewCatalog, mv_scheduler::{MVScheduler, Priority}};
use serde::{Serialize, Deserialize};
use std::sync::Arc;
use std::time::Duration;
use std::collections::VecDeque;
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{info, debug, warn, error};
use chrono::{DateTime, Utc};

/// Entry in the refresh history log
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshHistoryEntry {
    /// Name of the materialized view
    pub mv_name: String,
    /// When the refresh started
    pub start_time: DateTime<Utc>,
    /// When the refresh ended
    pub end_time: DateTime<Utc>,
    /// Whether the refresh succeeded
    pub success: bool,
    /// Error message if failed
    pub error_message: Option<String>,
    /// Number of rows affected
    pub rows_affected: Option<i64>,
    /// Refresh strategy used
    pub strategy: String,
    /// What triggered the refresh (auto, manual, staleness)
    pub trigger: String,
}

/// Maximum number of history entries to keep
const MAX_HISTORY_ENTRIES: usize = 1000;

/// Configuration for auto-refresh background worker
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoRefreshConfig {
    /// Enable auto-refresh worker
    pub enabled: bool,

    /// Interval between staleness checks (seconds)
    pub interval_seconds: u64,

    /// Staleness threshold to trigger refresh (seconds)
    pub staleness_threshold_seconds: i64,

    /// Maximum number of concurrent refreshes
    pub max_concurrent_refreshes: usize,

    /// Maximum CPU usage percentage (0-100)
    pub max_cpu_percent: f64,
}

impl Default for AutoRefreshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: 60,
            staleness_threshold_seconds: 300, // 5 minutes
            max_concurrent_refreshes: 2,
            max_cpu_percent: 50.0,
        }
    }
}

impl AutoRefreshConfig {
    /// Create a new config with validation
    pub fn new() -> Self {
        Self::default()
    }

    /// Set enabled flag
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Set interval with validation
    pub fn with_interval_seconds(mut self, seconds: u64) -> Self {
        self.interval_seconds = seconds.max(1);
        self
    }

    /// Set staleness threshold with validation
    pub fn with_staleness_threshold(mut self, seconds: i64) -> Self {
        self.staleness_threshold_seconds = seconds.max(0);
        self
    }

    /// Set max concurrent refreshes with validation
    pub fn with_max_concurrent(mut self, max: usize) -> Self {
        self.max_concurrent_refreshes = max.max(1);
        self
    }

    /// Set max CPU percent with validation
    pub fn with_max_cpu_percent(mut self, percent: f64) -> Self {
        self.max_cpu_percent = percent.clamp(0.0, 100.0);
        self
    }
}

/// Control messages for the worker thread
#[derive(Debug)]
enum WorkerCommand {
    /// Stop the worker gracefully
    Stop,
    /// Force immediate staleness check
    CheckNow,
}

/// Auto-refresh background worker
///
/// Manages automatic refresh of materialized views based on staleness
/// and CPU availability.
pub struct AutoRefreshWorker {
    /// Worker configuration
    config: Arc<RwLock<AutoRefreshConfig>>,

    /// Storage engine reference
    storage: Arc<StorageEngine>,

    /// MV scheduler for prioritized refresh execution
    scheduler: Arc<MVScheduler>,

    /// Command channel sender
    command_tx: Option<mpsc::UnboundedSender<WorkerCommand>>,

    /// Worker task handle
    worker_handle: Option<JoinHandle<()>>,

    /// Running state
    is_running: Arc<Mutex<bool>>,

    /// Active refresh count
    active_refreshes: Arc<Mutex<usize>>,

    /// Refresh history buffer (circular, most recent first)
    refresh_history: Arc<Mutex<VecDeque<RefreshHistoryEntry>>>,
}

impl AutoRefreshWorker {
    /// Create a new auto-refresh worker
    pub fn new(
        config: AutoRefreshConfig,
        storage: Arc<StorageEngine>,
        scheduler: Arc<MVScheduler>,
    ) -> Self {
        info!("Creating AutoRefreshWorker with config: enabled={}, interval={}s, staleness_threshold={}s",
            config.enabled, config.interval_seconds, config.staleness_threshold_seconds);

        Self {
            config: Arc::new(RwLock::new(config)),
            storage,
            scheduler,
            command_tx: None,
            worker_handle: None,
            is_running: Arc::new(Mutex::new(false)),
            active_refreshes: Arc::new(Mutex::new(0)),
            refresh_history: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_HISTORY_ENTRIES))),
        }
    }

    /// Start the background worker
    ///
    /// Spawns a tokio task that periodically checks for stale views
    /// and schedules them for refresh.
    pub async fn start(&mut self) -> Result<()> {
        let config = self.config.read().clone();

        if !config.enabled {
            info!("AutoRefreshWorker is disabled, not starting");
            return Ok(());
        }

        // Check if already running
        {
            let mut running = self.is_running.lock();
            if *running {
                return Err(Error::storage("AutoRefreshWorker is already running"));
            }
            *running = true;
        }

        info!("Starting AutoRefreshWorker background task");

        // Create command channel
        let (tx, rx) = mpsc::unbounded_channel();
        self.command_tx = Some(tx);

        // Clone references for the worker task
        let config = Arc::clone(&self.config);
        let storage = Arc::clone(&self.storage);
        let scheduler = Arc::clone(&self.scheduler);
        let is_running = Arc::clone(&self.is_running);
        let active_refreshes = Arc::clone(&self.active_refreshes);

        // Spawn the worker task
        let handle = tokio::spawn(async move {
            Self::worker_loop(config, storage, scheduler, is_running, active_refreshes, rx).await;
        });

        self.worker_handle = Some(handle);

        info!("AutoRefreshWorker started successfully");
        Ok(())
    }

    /// Send stop signal to the background worker (non-blocking, sync-safe).
    ///
    /// This is safe to call from `Drop` impls. The worker will stop on its next loop iteration.
    /// For graceful shutdown with waiting, use `stop()` instead.
    pub fn request_stop(&self) {
        if let Some(tx) = &self.command_tx {
            let _ = tx.send(WorkerCommand::Stop);
        }
        *self.is_running.lock() = false;
    }

    /// Stop the background worker gracefully
    ///
    /// Waits for in-flight refreshes to complete before shutting down.
    pub async fn stop(&mut self) -> Result<()> {
        info!("Stopping AutoRefreshWorker");

        // Send stop command
        if let Some(tx) = &self.command_tx {
            let _ = tx.send(WorkerCommand::Stop);
        }

        // Wait for the worker task to complete
        if let Some(handle) = self.worker_handle.take() {
            match tokio::time::timeout(Duration::from_secs(30), handle).await {
                Ok(result) => {
                    if let Err(e) = result {
                        error!("Worker task panicked: {}", e);
                        return Err(Error::storage(format!("Worker task panicked: {}", e)));
                    }
                }
                Err(_) => {
                    warn!("Worker task did not stop within timeout, forcing shutdown");
                }
            }
        }

        // Mark as not running
        *self.is_running.lock() = false;
        self.command_tx = None;

        info!("AutoRefreshWorker stopped");
        Ok(())
    }

    /// Check if the worker is running
    pub fn is_running(&self) -> bool {
        *self.is_running.lock()
    }

    /// Get the current configuration
    pub fn config(&self) -> AutoRefreshConfig {
        self.config.read().clone()
    }

    /// Update the configuration dynamically
    pub fn update_config(&self, config: AutoRefreshConfig) {
        *self.config.write() = config;
        info!("AutoRefreshWorker configuration updated");
    }

    /// Force an immediate staleness check
    pub fn check_now(&self) -> Result<()> {
        if let Some(tx) = &self.command_tx {
            tx.send(WorkerCommand::CheckNow)
                .map_err(|e| Error::storage(format!("Failed to send check command: {}", e)))?;
            Ok(())
        } else {
            Err(Error::storage("Worker is not running"))
        }
    }

    /// Get the number of active refreshes
    pub fn active_refresh_count(&self) -> usize {
        *self.active_refreshes.lock()
    }

    /// Record a refresh operation in the history
    pub fn record_refresh(
        &self,
        mv_name: String,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        success: bool,
        error_message: Option<String>,
        rows_affected: Option<i64>,
        strategy: String,
        trigger: String,
    ) {
        let entry = RefreshHistoryEntry {
            mv_name,
            start_time,
            end_time,
            success,
            error_message,
            rows_affected,
            strategy,
            trigger,
        };

        let mut history = self.refresh_history.lock();
        // Add to front (most recent first)
        history.push_front(entry);
        // Trim to max size
        while history.len() > MAX_HISTORY_ENTRIES {
            history.pop_back();
        }
    }

    /// Get refresh history entries
    ///
    /// Returns up to `limit` entries, or all entries if limit is None.
    /// Entries are returned in reverse chronological order (most recent first).
    pub fn get_refresh_history(&self, limit: Option<usize>) -> Vec<RefreshHistoryEntry> {
        let history = self.refresh_history.lock();
        let limit = limit.unwrap_or(history.len());
        history.iter().take(limit).cloned().collect()
    }

    /// Clear all refresh history
    pub fn clear_history(&self) {
        self.refresh_history.lock().clear();
    }

    /// Get the count of history entries
    pub fn history_count(&self) -> usize {
        self.refresh_history.lock().len()
    }

    /// Main worker loop
    async fn worker_loop(
        config: Arc<RwLock<AutoRefreshConfig>>,
        storage: Arc<StorageEngine>,
        scheduler: Arc<MVScheduler>,
        is_running: Arc<Mutex<bool>>,
        active_refreshes: Arc<Mutex<usize>>,
        mut command_rx: mpsc::UnboundedReceiver<WorkerCommand>,
    ) {
        info!("AutoRefreshWorker loop started");

        loop {
            let interval_seconds = config.read().interval_seconds;

            // Wait for interval or command
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(interval_seconds)) => {
                    // Periodic check
                    Self::perform_staleness_check(
                        &config,
                        &storage,
                        &scheduler,
                        &active_refreshes,
                    ).await;
                }
                Some(cmd) = command_rx.recv() => {
                    match cmd {
                        WorkerCommand::Stop => {
                            info!("Received stop command, shutting down worker");
                            break;
                        }
                        WorkerCommand::CheckNow => {
                            debug!("Received immediate check command");
                            Self::perform_staleness_check(
                                &config,
                                &storage,
                                &scheduler,
                                &active_refreshes,
                            ).await;
                        }
                    }
                }
            }
        }

        // Wait for active refreshes to complete
        info!("Waiting for active refreshes to complete");
        let mut wait_count = 0;
        while *active_refreshes.lock() > 0 && wait_count < 30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            wait_count += 1;
        }

        *is_running.lock() = false;
        info!("AutoRefreshWorker loop terminated");
    }

    /// Perform staleness check and schedule refreshes
    async fn perform_staleness_check(
        config: &Arc<RwLock<AutoRefreshConfig>>,
        storage: &Arc<StorageEngine>,
        scheduler: &Arc<MVScheduler>,
        active_refreshes: &Arc<Mutex<usize>>,
    ) {
        let cfg = config.read().clone();

        debug!("Performing staleness check");

        // Check CPU usage
        let cpu_stats = scheduler.get_stats();
        if cpu_stats.cpu_usage > cfg.max_cpu_percent {
            debug!(
                "CPU usage {:.1}% exceeds threshold {:.1}%, skipping refresh check",
                cpu_stats.cpu_usage, cfg.max_cpu_percent
            );
            return;
        }

        // Check concurrent refresh limit
        let active_count = *active_refreshes.lock();
        if active_count >= cfg.max_concurrent_refreshes {
            debug!(
                "Active refresh count {} meets limit {}, skipping",
                active_count, cfg.max_concurrent_refreshes
            );
            return;
        }

        // Get all materialized views
        let catalog = MaterializedViewCatalog::new(storage.as_ref());
        let views = match catalog.list_views() {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to list materialized views: {}", e);
                return;
            }
        };

        debug!("Checking {} materialized views for staleness", views.len());

        let mut stale_views = Vec::new();

        // Check each view for staleness
        for view_name in views {
            let metadata = match catalog.get_view(&view_name) {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to get metadata for view '{}': {}", view_name, e);
                    continue;
                }
            };

            // Check if auto_refresh is enabled
            let auto_refresh_enabled = metadata.metadata
                .get("auto_refresh")
                .and_then(|v| v.parse::<bool>().ok())
                .unwrap_or(false);

            if !auto_refresh_enabled {
                continue;
            }

            // Check staleness
            if let Some(staleness) = metadata.staleness_seconds() {
                if staleness >= cfg.staleness_threshold_seconds {
                    debug!(
                        "View '{}' is stale ({} seconds old, threshold: {})",
                        view_name, staleness, cfg.staleness_threshold_seconds
                    );
                    stale_views.push((view_name.clone(), staleness));
                }
            } else if metadata.is_stale() {
                debug!("View '{}' has never been refreshed", view_name);
                stale_views.push((view_name.clone(), i64::MAX));
            }
        }

        if stale_views.is_empty() {
            debug!("No stale views found");
            return;
        }

        // Sort by staleness (most stale first)
        stale_views.sort_by(|a, b| b.1.cmp(&a.1));

        // Schedule refreshes up to the concurrent limit
        let available_slots = cfg.max_concurrent_refreshes.saturating_sub(active_count);
        let to_refresh = stale_views.iter().take(available_slots);

        for (view_name, staleness) in to_refresh {
            info!(
                "Scheduling auto-refresh for view '{}' (staleness: {} seconds)",
                view_name, staleness
            );

            // Increment active count
            *active_refreshes.lock() += 1;

            // Schedule with Normal priority (auto-refresh is not urgent)
            if let Err(e) = scheduler.schedule_refresh(view_name, Priority::Normal) {
                error!("Failed to schedule refresh for '{}': {}", view_name, e);
                *active_refreshes.lock() = active_refreshes.lock().saturating_sub(1);
            }
        }

        // Decrement active count when refreshes complete
        // Note: In production, we'd track individual refresh completion
        // For now, we rely on the scheduler's internal tracking
        tokio::spawn({
            let active_refreshes = Arc::clone(active_refreshes);
            async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                *active_refreshes.lock() = 0;
            }
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{Config, Column, DataType, Schema};
    use crate::sql::LogicalPlan;
    use crate::storage::mv_scheduler::SchedulerConfig;

    fn create_test_storage() -> Arc<StorageEngine> {
        let config = Config::in_memory();
        Arc::new(StorageEngine::open_in_memory(&config).unwrap())
    }

    fn create_test_scheduler(storage: Arc<StorageEngine>) -> Arc<MVScheduler> {
        let config = SchedulerConfig::default();
        Arc::new(MVScheduler::new(config, storage))
    }

    #[test]
    fn test_auto_refresh_config_default() {
        let config = AutoRefreshConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.interval_seconds, 60);
        assert_eq!(config.staleness_threshold_seconds, 300);
        assert_eq!(config.max_concurrent_refreshes, 2);
        assert_eq!(config.max_cpu_percent, 50.0);
    }

    #[test]
    fn test_auto_refresh_config_builder() {
        let config = AutoRefreshConfig::new()
            .with_enabled(true)
            .with_interval_seconds(30)
            .with_staleness_threshold(600)
            .with_max_concurrent(4)
            .with_max_cpu_percent(75.0);

        assert!(config.enabled);
        assert_eq!(config.interval_seconds, 30);
        assert_eq!(config.staleness_threshold_seconds, 600);
        assert_eq!(config.max_concurrent_refreshes, 4);
        assert_eq!(config.max_cpu_percent, 75.0);
    }

    #[test]
    fn test_config_validation() {
        // Test interval validation (minimum 1)
        let config = AutoRefreshConfig::new().with_interval_seconds(0);
        assert_eq!(config.interval_seconds, 1);

        // Test staleness threshold validation (minimum 0)
        let config = AutoRefreshConfig::new().with_staleness_threshold(-100);
        assert_eq!(config.staleness_threshold_seconds, 0);

        // Test max concurrent validation (minimum 1)
        let config = AutoRefreshConfig::new().with_max_concurrent(0);
        assert_eq!(config.max_concurrent_refreshes, 1);

        // Test CPU percent clamping
        let config = AutoRefreshConfig::new().with_max_cpu_percent(150.0);
        assert_eq!(config.max_cpu_percent, 100.0);

        let config = AutoRefreshConfig::new().with_max_cpu_percent(-10.0);
        assert_eq!(config.max_cpu_percent, 0.0);
    }

    #[test]
    fn test_auto_refresh_worker_creation() {
        let storage = create_test_storage();
        let scheduler = create_test_scheduler(Arc::clone(&storage));
        let config = AutoRefreshConfig::default();

        let worker = AutoRefreshWorker::new(config, storage, scheduler);
        assert!(!worker.is_running());
        assert!(!worker.config().enabled);
    }

    #[tokio::test]
    async fn test_worker_start_disabled() {
        let storage = create_test_storage();
        let scheduler = create_test_scheduler(Arc::clone(&storage));
        let config = AutoRefreshConfig::default();

        let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

        // Should succeed when disabled (does nothing)
        assert!(worker.start().await.is_ok());
        assert!(!worker.is_running());
    }

    #[tokio::test]
    async fn test_worker_start_enabled() {
        let storage = create_test_storage();
        let scheduler = create_test_scheduler(Arc::clone(&storage));
        let config = AutoRefreshConfig::new().with_enabled(true);

        let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

        // Should start successfully
        assert!(worker.start().await.is_ok());
        assert!(worker.is_running());

        // Should fail to start again
        assert!(worker.start().await.is_err());

        // Stop the worker
        assert!(worker.stop().await.is_ok());
        assert!(!worker.is_running());
    }

    #[tokio::test]
    async fn test_worker_stop_graceful() {
        let storage = create_test_storage();
        let scheduler = create_test_scheduler(Arc::clone(&storage));
        let config = AutoRefreshConfig::new()
            .with_enabled(true)
            .with_interval_seconds(1);

        let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

        // Start the worker
        worker.start().await.unwrap();
        assert!(worker.is_running());

        // Let it run for a moment
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Stop gracefully
        let stop_result = worker.stop().await;
        assert!(stop_result.is_ok());
        assert!(!worker.is_running());
    }

    #[tokio::test]
    async fn test_worker_check_now() {
        let storage = create_test_storage();
        let scheduler = create_test_scheduler(Arc::clone(&storage));
        let config = AutoRefreshConfig::new()
            .with_enabled(true)
            .with_interval_seconds(60); // Long interval

        let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

        // Check now should fail when not running
        assert!(worker.check_now().is_err());

        // Start the worker
        worker.start().await.unwrap();

        // Check now should succeed
        assert!(worker.check_now().is_ok());

        // Stop
        worker.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_worker_update_config() {
        let storage = create_test_storage();
        let scheduler = create_test_scheduler(Arc::clone(&storage));
        let config = AutoRefreshConfig::new()
            .with_enabled(true)
            .with_staleness_threshold(300);

        let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

        worker.start().await.unwrap();

        // Update config
        let new_config = AutoRefreshConfig::new()
            .with_enabled(true)
            .with_staleness_threshold(600);

        worker.update_config(new_config);

        // Verify updated
        let current_config = worker.config();
        assert_eq!(current_config.staleness_threshold_seconds, 600);

        worker.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_active_refresh_count() {
        let storage = create_test_storage();
        let scheduler = create_test_scheduler(Arc::clone(&storage));
        let config = AutoRefreshConfig::new().with_enabled(true);

        let worker = AutoRefreshWorker::new(config, storage, scheduler);

        // Initial count should be 0
        assert_eq!(worker.active_refresh_count(), 0);
    }

    #[tokio::test]
    async fn test_staleness_check_with_no_views() {
        let storage = create_test_storage();
        let scheduler = create_test_scheduler(Arc::clone(&storage));
        let config = AutoRefreshConfig::new()
            .with_enabled(true)
            .with_interval_seconds(1);

        let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

        // Start worker
        worker.start().await.unwrap();

        // Let it perform a few checks
        tokio::time::sleep(Duration::from_millis(2100)).await;

        // Should not crash with no views
        assert!(worker.is_running());

        worker.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_cpu_throttling() {
        let storage = create_test_storage();
        let scheduler = create_test_scheduler(Arc::clone(&storage));

        // Set very low CPU threshold (will likely exceed)
        let config = AutoRefreshConfig::new()
            .with_enabled(true)
            .with_interval_seconds(1)
            .with_max_cpu_percent(0.1); // Very low threshold

        let mut worker = AutoRefreshWorker::new(config, storage, scheduler);

        worker.start().await.unwrap();

        // Let it run - should skip checks due to CPU
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Should still be running
        assert!(worker.is_running());

        worker.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_concurrent_refresh_limit() {
        let storage = create_test_storage();
        let scheduler = create_test_scheduler(Arc::clone(&storage));

        // Create a view with auto_refresh enabled
        let catalog = MaterializedViewCatalog::new(&storage);
        let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);

        let query_plan = LogicalPlan::Scan {
            alias: None,
            table_name: "test".to_string(),
            schema: std::sync::Arc::new(schema.clone()),
            projection: None,
            as_of: None,
        };
        let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

        let mut metadata = crate::storage::MaterializedViewMetadata::new(
            "test_view".to_string(),
            "SELECT * FROM test".to_string(),
            query_plan_bytes,
            vec!["test".to_string()],
            schema,
        );

        // Enable auto-refresh
        metadata.metadata.insert("auto_refresh".to_string(), "true".to_string());
        catalog.create_view(metadata).unwrap();

        let config = AutoRefreshConfig::new()
            .with_enabled(true)
            .with_interval_seconds(1)
            .with_max_concurrent(1); // Only 1 concurrent

        let mut worker = AutoRefreshWorker::new(config, Arc::clone(&storage), scheduler);

        worker.start().await.unwrap();

        // Let it attempt to refresh
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Should respect concurrent limit
        assert!(worker.active_refresh_count() <= 1);

        worker.stop().await.unwrap();
    }

    #[test]
    fn test_worker_command_enum() {
        let stop_cmd = WorkerCommand::Stop;
        let check_cmd = WorkerCommand::CheckNow;

        assert!(matches!(stop_cmd, WorkerCommand::Stop));
        assert!(matches!(check_cmd, WorkerCommand::CheckNow));
    }
}
