//! CPU-Aware Scheduler for Materialized View Refreshes
//!
//! This module implements intelligent scheduling of materialized view refreshes
//! based on CPU utilization and task priority. It provides:
//!
//! - Priority-based task queue with CRITICAL, HIGH, NORMAL, and LOW priorities
//! - CPU usage monitoring with exponential moving average smoothing
//! - Configurable CPU thresholds and concurrent task limits
//! - Adaptive batch sizing based on system load
//! - Automatic rescheduling on failure with priority degradation
//! - Integration with auto-refresh triggers on base table changes
//!
//! ## Architecture
//!
//! The scheduler consists of:
//! 1. **MVScheduler**: Main scheduler managing the refresh queue and worker pool
//! 2. **CpuMonitor**: Cross-platform CPU usage monitoring with smoothing
//! 3. **RefreshTask**: Priority queue entry with scheduling metadata
//! 4. **SchedulerConfig**: Configuration for thresholds and limits
//!
//! ## Usage
//!
//! ```rust,ignore
//! let config = SchedulerConfig::default();
//! let scheduler = MVScheduler::new(
//!     config,
//!     storage_engine,
//! );
//!
//! // Start background scheduler loop
//! tokio::spawn(async move {
//!     scheduler.run().await
//! });
//!
//! // Schedule MV refresh
//! scheduler.schedule_refresh("sales_summary", Priority::High)?;
//! ```

use super::mv_incremental::DeltaTracker;
use super::{IncrementalRefresher, StorageEngine};
use crate::{Error, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{debug, error, info, warn};

/// Priority level for materialized view refresh tasks
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    /// Rarely queried MVs, can wait for low system load
    Low = 0,
    /// Regular MVs with normal refresh requirements
    Normal = 1,
    /// Frequently queried MVs requiring timely updates
    High = 2,
    /// User-triggered or critical MVs requiring immediate refresh
    Critical = 3,
}

impl Default for Priority {
    fn default() -> Self {
        Priority::Normal
    }
}

/// Refresh task entry in the priority queue
///
/// Tasks are ordered by priority first (higher priority first),
/// then by scheduled time (earlier time first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshTask {
    /// Materialized view name
    pub mv_name: String,
    /// Task priority
    pub priority: Priority,
    /// Scheduled execution time
    pub scheduled_time: SystemTime,
    /// Estimated refresh duration (for scheduling optimization)
    pub estimated_duration: Duration,
}

impl Ord for RefreshTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority first (Critical=3 > High=2 > Normal=1 > Low=0)
        self.priority.cmp(&other.priority).then_with(|| {
            // Earlier scheduled time first (reverse time order)
            other.scheduled_time.cmp(&self.scheduled_time)
        })
    }
}

impl PartialOrd for RefreshTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Configuration for the materialized view scheduler
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// Maximum CPU utilization percentage before throttling (0.0 - 100.0)
    pub max_cpu_percent: f64,

    /// Interval between CPU checks and queue processing (seconds)
    pub check_interval_secs: u64,

    /// Number of MVs to consider per batch
    pub batch_size: usize,

    /// Maximum number of concurrent refresh operations
    pub max_concurrent: usize,

    /// Enable adaptive batch sizing based on CPU load
    pub adaptive_batch_sizing: bool,

    /// Retry failed refreshes automatically
    pub auto_retry: bool,

    /// Maximum number of retry attempts
    pub max_retries: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_cpu_percent: 70.0,
            check_interval_secs: 5,
            batch_size: 10,
            max_concurrent: 4,
            adaptive_batch_sizing: true,
            auto_retry: true,
            max_retries: 3,
        }
    }
}

impl SchedulerConfig {
    /// Create a new scheduler configuration
    pub fn new() -> Self {
        Self::default()
    }

    /// Set maximum CPU percentage
    pub fn with_max_cpu_percent(mut self, percent: f64) -> Self {
        self.max_cpu_percent = percent.clamp(0.0, 100.0);
        self
    }

    /// Set check interval
    pub fn with_check_interval(mut self, seconds: u64) -> Self {
        self.check_interval_secs = seconds.max(1);
        self
    }

    /// Set batch size
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = size.max(1);
        self
    }

    /// Set maximum concurrent refreshes
    pub fn with_max_concurrent(mut self, max: usize) -> Self {
        self.max_concurrent = max.max(1);
        self
    }

    /// Enable or disable adaptive batch sizing
    pub fn with_adaptive_batch_sizing(mut self, enabled: bool) -> Self {
        self.adaptive_batch_sizing = enabled;
        self
    }

    /// Enable or disable auto-retry
    pub fn with_auto_retry(mut self, enabled: bool) -> Self {
        self.auto_retry = enabled;
        self
    }
}

/// CPU usage monitor with exponential moving average smoothing
pub struct CpuMonitor {
    /// Last smoothed CPU usage percentage
    last_cpu_usage: Arc<Mutex<f64>>,
    /// System information instance (cached for efficiency)
    system: Arc<Mutex<sysinfo::System>>,
}

impl CpuMonitor {
    /// Create a new CPU monitor
    ///
    /// Uses `System::new()` instead of `new_all()` to avoid scanning all
    /// processes at startup (which opens 1000+ FDs in multi-instance containers).
    pub fn new() -> Self {
        Self {
            last_cpu_usage: Arc::new(Mutex::new(0.0)),
            system: Arc::new(Mutex::new(sysinfo::System::new())),
        }
    }

    /// Get current raw CPU usage percentage
    ///
    /// This method refreshes system CPU information and calculates
    /// the average CPU usage across all cores.
    ///
    /// NOTE: This is a blocking operation (~200ms). In async contexts,
    /// call via `tokio::task::spawn_blocking`.
    pub fn get_cpu_usage(&self) -> Result<f64> {
        let mut system = self.system.lock();
        system.refresh_cpu();

        // Brief pause for CPU measurements to stabilize, then re-sample
        std::thread::sleep(Duration::from_millis(100));
        system.refresh_cpu();

        let cpus = system.cpus();
        if cpus.is_empty() {
            return Err(Error::storage("No CPU information available"));
        }

        let total_cpu = cpus.iter().map(|cpu| cpu.cpu_usage()).sum::<f32>() / cpus.len() as f32;

        Ok(total_cpu as f64)
    }

    /// Get smoothed CPU usage with exponential moving average
    ///
    /// Uses a 70/30 weighting: 70% from last smoothed value, 30% from current.
    /// This reduces sensitivity to short-term spikes while remaining responsive.
    pub fn get_smoothed_cpu_usage(&self) -> Result<f64> {
        let current = self.get_cpu_usage()?;
        let mut last = self.last_cpu_usage.lock();

        // Exponential moving average: 70% previous, 30% current
        let smoothed = if *last == 0.0 {
            // First measurement, no smoothing
            current
        } else {
            0.7 * (*last) + 0.3 * current
        };

        *last = smoothed;
        debug!("CPU usage: raw={:.1}%, smoothed={:.1}%", current, smoothed);
        Ok(smoothed)
    }
}

impl Default for CpuMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Refresh strategy result information (for internal use)
#[derive(Debug, Clone)]
struct RefreshResult {
    /// Strategy used for refresh
    strategy_used: String,
    /// Number of rows affected
    rows_affected: u64,
    /// Refresh duration
    duration: Duration,
}

/// CPU-Aware Materialized View Scheduler
///
/// Manages a priority queue of materialized view refresh tasks and schedules
/// them intelligently based on CPU utilization and task priority.
pub struct MVScheduler {
    /// Scheduler configuration
    config: Arc<Mutex<SchedulerConfig>>,

    /// Storage engine reference
    storage: Arc<StorageEngine>,

    /// Delta tracker for incremental refresh.
    ///
    /// NOT on the auto-refresh path: no DML path feeds `mv_incremental::DeltaTracker`
    /// (see `perform_refresh`). Kept because `IncrementalRefresher` is public API.
    delta_tracker: Arc<DeltaTracker>,

    /// Incremental refresher.
    ///
    /// NOT on the auto-refresh path — see the note on `delta_tracker` and the
    /// `perform_refresh` doc comment.
    incremental_refresher: Arc<IncrementalRefresher>,

    /// Priority queue of pending refresh tasks
    refresh_queue: Arc<Mutex<BinaryHeap<RefreshTask>>>,

    /// Set of currently running MV names
    running_tasks: Arc<Mutex<HashSet<String>>>,

    /// CPU usage monitor
    cpu_monitor: Arc<CpuMonitor>,

    /// Retry count per MV name
    retry_counts: Arc<Mutex<std::collections::HashMap<String, usize>>>,
}

impl MVScheduler {
    /// Create a new MV scheduler
    pub fn new(config: SchedulerConfig, storage: Arc<StorageEngine>) -> Self {
        info!(
            "Initializing MVScheduler with config: max_cpu={}%, check_interval={}s, max_concurrent={}",
            config.max_cpu_percent, config.check_interval_secs, config.max_concurrent
        );

        let delta_tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));
        let incremental_refresher = Arc::new(IncrementalRefresher::new(
            Arc::clone(&storage),
            Arc::clone(&delta_tracker),
        ));

        Self {
            config: Arc::new(Mutex::new(config)),
            storage,
            delta_tracker,
            incremental_refresher,
            refresh_queue: Arc::new(Mutex::new(BinaryHeap::new())),
            running_tasks: Arc::new(Mutex::new(HashSet::new())),
            cpu_monitor: Arc::new(CpuMonitor::new()),
            retry_counts: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Schedule a materialized view for refresh
    ///
    /// Adds the MV to the priority queue for processing. If the MV is already
    /// in the queue or currently running, the task is not duplicated.
    pub fn schedule_refresh(&self, mv_name: &str, priority: Priority) -> Result<()> {
        // Check if already running
        {
            let running = self.running_tasks.lock();
            if running.contains(mv_name) {
                debug!("MV '{}' is already being refreshed, skipping schedule", mv_name);
                return Ok(());
            }
        }

        // Check if already in queue
        {
            let queue = self.refresh_queue.lock();
            if queue.iter().any(|task| task.mv_name == mv_name) {
                debug!("MV '{}' is already in refresh queue, skipping schedule", mv_name);
                return Ok(());
            }
        }

        let estimated_duration = self.estimate_duration(mv_name)?;

        let task = RefreshTask {
            mv_name: mv_name.to_string(),
            priority,
            scheduled_time: SystemTime::now(),
            estimated_duration,
        };

        info!("Scheduled MV '{}' for refresh with priority {:?}", mv_name, priority);
        self.refresh_queue.lock().push(task);

        Ok(())
    }

    /// Start the background scheduler loop
    ///
    /// This is the main scheduler loop that:
    /// 1. Periodically checks CPU usage
    /// 2. Processes tasks from the priority queue when CPU allows
    /// 3. Spawns worker tasks for concurrent execution
    /// 4. Adjusts batch size based on system load (if enabled)
    pub async fn run(&self) -> Result<()> {
        info!("Starting MVScheduler background loop");

        loop {
            let check_interval = self.config.lock().check_interval_secs;
            tokio::time::sleep(Duration::from_secs(check_interval)).await;

            // Check CPU usage (blocking sysinfo call — run off the tokio runtime)
            let monitor = self.cpu_monitor.clone();
            let cpu_usage = match tokio::task::spawn_blocking(move || monitor.get_smoothed_cpu_usage()).await {
                Ok(Ok(usage)) => usage,
                Ok(Err(e)) => {
                    warn!("Failed to get CPU usage: {}", e);
                    continue;
                }
                Err(e) => {
                    warn!("CPU monitor task panicked: {}", e);
                    continue;
                }
            };

            let max_cpu = self.config.lock().max_cpu_percent;
            if cpu_usage > max_cpu {
                debug!(
                    "CPU usage {:.1}% exceeds threshold {:.1}%, skipping refresh batch",
                    cpu_usage, max_cpu
                );
                continue;
            }

            // Adjust batch size if adaptive sizing is enabled
            if self.config.lock().adaptive_batch_sizing {
                self.adjust_batch_size(cpu_usage);
            }

            // Determine available capacity
            let max_concurrent = self.config.lock().max_concurrent;
            let available_capacity = max_concurrent.saturating_sub(self.running_tasks.lock().len());

            if available_capacity == 0 {
                debug!("No available capacity for new refresh tasks");
                continue;
            }

            // Pop tasks from queue
            let batch_size = self.config.lock().batch_size;
            let mut tasks_to_run = Vec::new();
            {
                let mut queue = self.refresh_queue.lock();
                for _ in 0..available_capacity.min(batch_size) {
                    if let Some(task) = queue.pop() {
                        tasks_to_run.push(task);
                    } else {
                        break;
                    }
                }
            }

            if tasks_to_run.is_empty() {
                debug!("No pending refresh tasks in queue");
                continue;
            }

            info!("Processing {} refresh tasks from queue", tasks_to_run.len());

            // Spawn refresh tasks
            for task in tasks_to_run {
                let scheduler = self.clone();
                let handle = tokio::spawn(async move {
                    if let Err(e) = scheduler.execute_refresh(task).await {
                        error!("Failed to execute refresh task: {}", e);
                    }
                });
                // Spawn a watcher to log panics from the refresh task
                tokio::spawn(async move {
                    if let Err(e) = handle.await {
                        if e.is_panic() {
                            error!("MV refresh task panicked: {}", e);
                        }
                    }
                });
            }
        }
    }

    /// Execute a single refresh task
    ///
    /// This method:
    /// 1. Marks the MV as running
    /// 2. Executes the refresh operation
    /// 3. Updates metadata and metrics
    /// 4. Handles errors with retry logic
    /// 5. Marks the MV as complete
    async fn execute_refresh(&self, task: RefreshTask) -> Result<()> {
        let mv_name = task.mv_name.clone();

        // Mark as running
        self.running_tasks.lock().insert(mv_name.clone());

        info!("Starting refresh for MV '{}' (priority: {:?})", mv_name, task.priority);
        let start = SystemTime::now();

        // Execute refresh
        let result = self.perform_refresh(&mv_name).await;

        let duration = start.elapsed().unwrap_or(Duration::from_secs(0));

        // Mark as complete
        self.running_tasks.lock().remove(&mv_name);

        match result {
            Ok(refresh_result) => {
                info!(
                    "Refreshed MV '{}' in {:?} using {} strategy, {} rows affected",
                    mv_name, duration, refresh_result.strategy_used, refresh_result.rows_affected
                );

                // Clear retry count on success
                self.retry_counts.lock().remove(&mv_name);

                Ok(())
            }
            Err(e) => {
                error!("Failed to refresh MV '{}': {}", mv_name, e);

                // Handle retry logic
                let should_retry = self.config.lock().auto_retry;
                if should_retry {
                    let mut retry_counts = self.retry_counts.lock();
                    let retry_count = retry_counts.entry(mv_name.clone()).or_insert(0);
                    *retry_count += 1;

                    let max_retries = self.config.lock().max_retries;
                    if *retry_count <= max_retries {
                        warn!(
                            "Rescheduling MV '{}' for retry (attempt {} of {})",
                            mv_name, retry_count, max_retries
                        );

                        // Reschedule with degraded priority
                        let new_priority = match task.priority {
                            Priority::Critical => Priority::High,
                            Priority::High => Priority::Normal,
                            Priority::Normal => Priority::Low,
                            Priority::Low => Priority::Low,
                        };

                        drop(retry_counts);
                        self.schedule_refresh(&mv_name, new_priority)?;
                    } else {
                        error!(
                            "MV '{}' exceeded maximum retry attempts ({}), giving up",
                            mv_name, max_retries
                        );
                        retry_counts.remove(&mv_name);
                    }
                }

                Err(e)
            }
        }
    }

    /// Perform the actual refresh operation
    ///
    /// Dispatches the SAME code path a user-issued
    /// `REFRESH MATERIALIZED VIEW … CONCURRENTLY` takes
    /// (`sql::executor::phase3::handle_refresh_materialized_view`): re-execute the
    /// stored optimized query plan against current data and rewrite the view rows.
    ///
    /// Why not `IncrementalRefresher` (what this used to call): its deltas come from
    /// `mv_incremental::DeltaTracker`, whose `record_insert` / `record_update` /
    /// `record_delete` recorders **no DML path ever calls**. Every scheduled
    /// "incremental refresh" therefore saw zero deltas, reported success, and reset
    /// the staleness clock while the view's content stayed stale — a staleness lie.
    /// The delta tracker DML actually feeds is `storage::mv_delta::DeltaTracker`,
    /// which is what the SQL REFRESH path consults. `IncrementalRefresher` remains a
    /// public API (see `tests/incremental_mv_integration_test.rs`); it is simply no
    /// longer on the auto-refresh path.
    async fn perform_refresh(&self, mv_name: &str) -> Result<RefreshResult> {
        let storage = Arc::clone(&self.storage);
        let name = mv_name.to_string();
        let start = std::time::Instant::now();

        // The refresh is synchronous storage work end to end; keep it off the async
        // runtime's worker threads (the previous body called a blocking refresher
        // directly from this async fn).
        let rows_affected = tokio::task::spawn_blocking(move || -> Result<u64> {
            let plan = crate::sql::LogicalPlan::RefreshMaterializedView {
                name: name.clone(),
                // Zero-downtime atomic swap, as this module's docs promise. It also
                // forces the handler's full-recompute branch (it disables incremental
                // whenever `concurrent` is set), which is the only branch ever proven
                // to change view content.
                concurrent: true,
                // Let the handler pick the strategy exactly like a manual REFRESH.
                incremental: false,
            };
            // A FRESH executor with no inherited transaction — the same pattern the
            // manual REFRESH and CREATE paths use so the re-materialization scans the
            // whole current table (issue #2 / Quirk J).
            let mut exec = crate::sql::Executor::with_storage(&storage);
            exec.execute(&plan)?;

            // The handler already persisted metadata (row count + last_refresh); read
            // it back rather than writing a second, racing update.
            let catalog = super::MaterializedViewCatalog::new(&storage);
            Ok(catalog.get_view(&name)?.row_count.unwrap_or(0))
        })
        .await
        .map_err(|e| Error::storage(format!("MV refresh task join error: {e}")))??;

        Ok(RefreshResult {
            strategy_used: "full(concurrent)".to_string(),
            rows_affected,
            duration: start.elapsed(),
        })
    }

    /// Estimate refresh duration for a materialized view
    ///
    /// Uses historical data or heuristics to estimate how long a refresh will take.
    /// This is used for scheduling optimization.
    fn estimate_duration(&self, mv_name: &str) -> Result<Duration> {
        use super::MaterializedViewCatalog;

        let catalog = MaterializedViewCatalog::new(&self.storage);

        // Try to get metadata for row count estimation
        match catalog.get_view(mv_name) {
            Ok(metadata) => {
                // Rough estimate: 10ms per 1000 rows
                let row_count = metadata.row_count.unwrap_or(1000);
                let estimated_ms = (row_count / 1000).max(1) * 10;
                Ok(Duration::from_millis(estimated_ms))
            }
            Err(_) => {
                // Default estimate for unknown MVs
                Ok(Duration::from_secs(5))
            }
        }
    }

    /// Adjust batch size based on CPU utilization
    ///
    /// Implements adaptive batch sizing:
    /// - Low CPU (<50%): Increase batch size
    /// - High CPU (>80%): Decrease batch size
    /// - Normal CPU: Keep current batch size
    fn adjust_batch_size(&self, cpu_usage: f64) {
        let mut config = self.config.lock();
        let old_batch_size = config.batch_size;

        if cpu_usage < 50.0 {
            // Low CPU: increase batch size (up to 50)
            config.batch_size = (config.batch_size + 5).min(50);
        } else if cpu_usage > 80.0 {
            // High CPU: decrease batch size (down to 1)
            config.batch_size = config.batch_size.saturating_sub(5).max(1);
        }

        if config.batch_size != old_batch_size {
            debug!(
                "Adjusted batch size from {} to {} (CPU: {:.1}%)",
                old_batch_size, config.batch_size, cpu_usage
            );
        }
    }

    /// Handle base table change event
    ///
    /// When a base table is modified, this method schedules all dependent
    /// materialized views for refresh with normal priority.
    pub fn on_base_table_change(&self, table_name: &str) -> Result<()> {
        use super::MaterializedViewCatalog;

        debug!("Base table '{}' changed, checking dependent MVs", table_name);

        let catalog = MaterializedViewCatalog::new(&self.storage);
        let all_mvs = catalog.list_views()?;

        let mut affected_count = 0;
        for mv_name in all_mvs {
            let metadata = catalog.get_view(&mv_name)?;

            // Check if this MV depends on the changed table
            if metadata.base_tables.contains(&table_name.to_string()) {
                self.schedule_refresh(&mv_name, Priority::Normal)?;
                affected_count += 1;
            }
        }

        if affected_count > 0 {
            info!(
                "Scheduled {} dependent MVs for refresh after table '{}' change",
                affected_count, table_name
            );
        }

        Ok(())
    }

    /// Get current scheduler statistics (non-blocking, uses cached CPU value)
    pub fn get_stats(&self) -> SchedulerStats {
        SchedulerStats {
            queue_size: self.refresh_queue.lock().len(),
            running_tasks: self.running_tasks.lock().len(),
            cpu_usage: *self.cpu_monitor.last_cpu_usage.lock(),
        }
    }
}

// Implement Clone for MVScheduler by sharing Arc references
impl Clone for MVScheduler {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            storage: Arc::clone(&self.storage),
            delta_tracker: Arc::clone(&self.delta_tracker),
            incremental_refresher: Arc::clone(&self.incremental_refresher),
            refresh_queue: Arc::clone(&self.refresh_queue),
            running_tasks: Arc::clone(&self.running_tasks),
            cpu_monitor: Arc::clone(&self.cpu_monitor),
            retry_counts: Arc::clone(&self.retry_counts),
        }
    }
}

/// Scheduler statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerStats {
    /// Number of tasks in queue
    pub queue_size: usize,
    /// Number of currently running tasks
    pub running_tasks: usize,
    /// Current CPU usage percentage
    pub cpu_usage: f64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{Column, Config, DataType, Schema};

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::Critical > Priority::High);
        assert!(Priority::High > Priority::Normal);
        assert!(Priority::Normal > Priority::Low);
    }

    #[test]
    fn test_task_priority_queue() {
        let mut queue = BinaryHeap::new();

        queue.push(RefreshTask {
            mv_name: "low_task".to_string(),
            priority: Priority::Low,
            scheduled_time: SystemTime::now(),
            estimated_duration: Duration::from_secs(1),
        });

        queue.push(RefreshTask {
            mv_name: "critical_task".to_string(),
            priority: Priority::Critical,
            scheduled_time: SystemTime::now(),
            estimated_duration: Duration::from_secs(1),
        });

        queue.push(RefreshTask {
            mv_name: "normal_task".to_string(),
            priority: Priority::Normal,
            scheduled_time: SystemTime::now(),
            estimated_duration: Duration::from_secs(1),
        });

        // Should pop in priority order: Critical, Normal, Low
        let task1 = queue.pop().unwrap();
        assert_eq!(task1.priority, Priority::Critical);

        let task2 = queue.pop().unwrap();
        assert_eq!(task2.priority, Priority::Normal);

        let task3 = queue.pop().unwrap();
        assert_eq!(task3.priority, Priority::Low);
    }

    #[test]
    fn test_cpu_monitor_creation() {
        let monitor = CpuMonitor::new();

        // Should be able to get CPU usage
        let usage = monitor.get_cpu_usage();
        assert!(usage.is_ok());

        let cpu = usage.unwrap();
        assert!(cpu >= 0.0 && cpu <= 100.0);
    }

    #[test]
    fn test_scheduler_config() {
        let config = SchedulerConfig::default()
            .with_max_cpu_percent(80.0)
            .with_check_interval(10)
            .with_batch_size(20)
            .with_max_concurrent(8);

        assert_eq!(config.max_cpu_percent, 80.0);
        assert_eq!(config.check_interval_secs, 10);
        assert_eq!(config.batch_size, 20);
        assert_eq!(config.max_concurrent, 8);
    }

    #[test]
    fn test_scheduler_creation() {
        let config = Config::in_memory();
        let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());

        let scheduler_config = SchedulerConfig::default();
        let scheduler = MVScheduler::new(scheduler_config, storage);

        let stats = scheduler.get_stats();
        assert_eq!(stats.queue_size, 0);
        assert_eq!(stats.running_tasks, 0);
    }

    #[test]
    fn test_schedule_refresh() {
        let config = Config::in_memory();
        let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());

        // Create a test MV
        use super::super::MaterializedViewCatalog;
        use crate::sql::LogicalPlan;

        let mv_catalog = MaterializedViewCatalog::new(&storage);
        let schema = Schema::new(vec![Column::new("count", DataType::Int8)]);

        let query_plan = LogicalPlan::Scan {
            alias: None,
            table_name: "test".to_string(),
            schema: std::sync::Arc::new(schema.clone()),
            projection: None,
            as_of: None,
        };
        let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

        let metadata = super::super::MaterializedViewMetadata::new(
            "test_mv".to_string(),
            "SELECT COUNT(*) FROM test".to_string(),
            query_plan_bytes,
            vec!["test".to_string()],
            schema,
        );

        mv_catalog.create_view(metadata).unwrap();

        // Schedule refresh
        let scheduler_config = SchedulerConfig::default();
        let scheduler = MVScheduler::new(scheduler_config, storage);

        let result = scheduler.schedule_refresh("test_mv", Priority::High);
        assert!(result.is_ok());

        let stats = scheduler.get_stats();
        assert_eq!(stats.queue_size, 1);
    }

    #[test]
    fn test_duplicate_scheduling_prevention() {
        let config = Config::in_memory();
        let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());

        // Create test MV
        use super::super::MaterializedViewCatalog;
        use crate::sql::LogicalPlan;

        let mv_catalog = MaterializedViewCatalog::new(&storage);
        let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);

        let query_plan = LogicalPlan::Scan {
            alias: None,
            table_name: "test".to_string(),
            schema: std::sync::Arc::new(schema.clone()),
            projection: None,
            as_of: None,
        };
        let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

        let metadata = super::super::MaterializedViewMetadata::new(
            "test_mv".to_string(),
            "SELECT * FROM test".to_string(),
            query_plan_bytes,
            vec!["test".to_string()],
            schema,
        );

        mv_catalog.create_view(metadata).unwrap();

        let scheduler_config = SchedulerConfig::default();
        let scheduler = MVScheduler::new(scheduler_config, storage);

        // Schedule twice
        scheduler.schedule_refresh("test_mv", Priority::High).unwrap();
        scheduler.schedule_refresh("test_mv", Priority::High).unwrap();

        // Should only have one task
        let stats = scheduler.get_stats();
        assert_eq!(stats.queue_size, 1);
    }
}
