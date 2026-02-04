//! System Views for Materialized View Monitoring
//!
//! This module provides PostgreSQL-compatible system functions for monitoring
//! materialized views, auto-refresh status, and CPU usage.
//!
//! ## System Functions
//!
//! - `pg_mv_auto_refresh_status()` - Current status of all auto-refresh enabled MVs
//! - `pg_mv_refresh_history(limit?)` - Historical refresh operations
//! - `pg_mv_cpu_usage()` - Current CPU usage by the scheduler
//! - `pg_mv_scheduler_stats()` - Scheduler statistics
//!
//! ## Usage
//!
//! ```sql
//! -- Check auto-refresh status
//! SELECT * FROM pg_mv_auto_refresh_status();
//!
//! -- View recent refresh history
//! SELECT * FROM pg_mv_refresh_history(limit);
//!
//! -- Monitor CPU usage
//! SELECT * FROM pg_mv_cpu_usage();
//! ```

#![allow(unused_variables)]

use crate::{Result, Tuple, Value, Schema, Column, DataType};
use super::{
    StorageEngine, MaterializedViewCatalog, AutoRefreshWorker,
    MVScheduler, SchedulerStats, mv_auto_refresh::RefreshHistoryEntry,
};
use std::sync::Arc;
use serde::{Serialize, Deserialize};

/// Auto-refresh status for a single MV
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoRefreshStatus {
    /// MV name
    pub mv_name: String,
    /// Whether auto-refresh is enabled
    pub auto_refresh_enabled: bool,
    /// Last refresh timestamp
    pub last_refresh: Option<chrono::DateTime<chrono::Utc>>,
    /// Staleness in seconds
    pub staleness_seconds: Option<i64>,
    /// Staleness threshold
    pub threshold_seconds: i64,
    /// Whether currently refreshing
    pub is_refreshing: bool,
    /// Refresh strategy (manual, auto, incremental)
    pub refresh_strategy: String,
    /// Row count
    pub row_count: Option<u64>,
    /// Number of base tables
    pub base_table_count: usize,
}

/// CPU usage information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuUsageInfo {
    /// Current CPU usage percentage
    pub current_cpu_percent: f64,
    /// Max configured CPU percentage
    pub max_cpu_percent: f64,
    /// Whether CPU is above threshold
    pub is_throttled: bool,
    /// Number of active refresh tasks
    pub active_tasks: usize,
    /// Number of queued refresh tasks
    pub queued_tasks: usize,
}

/// System views manager
pub struct MvSystemViews {
    /// Storage engine
    storage: Arc<StorageEngine>,
    /// Auto-refresh worker (optional)
    auto_refresh_worker: Option<Arc<AutoRefreshWorker>>,
    /// MV scheduler
    scheduler: Arc<MVScheduler>,
}

impl MvSystemViews {
    /// Create a new system views manager
    pub fn new(
        storage: Arc<StorageEngine>,
        scheduler: Arc<MVScheduler>,
    ) -> Self {
        Self {
            storage,
            auto_refresh_worker: None,
            scheduler,
        }
    }

    /// Set the auto-refresh worker (for monitoring)
    pub fn with_auto_refresh_worker(mut self, worker: Arc<AutoRefreshWorker>) -> Self {
        self.auto_refresh_worker = Some(worker);
        self
    }

    /// Get auto-refresh status for all MVs
    pub fn pg_mv_auto_refresh_status(&self) -> Result<Vec<AutoRefreshStatus>> {
        let catalog = MaterializedViewCatalog::new(&self.storage);
        let all_mvs = catalog.list_views()?;

        let worker_config = self.auto_refresh_worker.as_ref()
            .map(|w| w.config())
            .unwrap_or_default();

        let scheduler_stats = self.scheduler.get_stats();
        let running_mvs: Vec<String> = vec![]; // Would track from scheduler in production

        let mut statuses = Vec::new();

        for mv_name in all_mvs {
            let metadata = catalog.get_view(&mv_name)?;

            // Check if auto-refresh is enabled
            let auto_refresh_enabled = metadata.metadata
                .get("auto_refresh")
                .and_then(|v| v.parse::<bool>().ok())
                .unwrap_or(false);

            let status = AutoRefreshStatus {
                mv_name: mv_name.clone(),
                auto_refresh_enabled,
                last_refresh: metadata.last_refresh,
                staleness_seconds: metadata.staleness_seconds(),
                threshold_seconds: worker_config.staleness_threshold_seconds,
                is_refreshing: running_mvs.contains(&mv_name),
                refresh_strategy: metadata.refresh_strategy.clone(),
                row_count: metadata.row_count,
                base_table_count: metadata.base_tables.len(),
            };

            statuses.push(status);
        }

        Ok(statuses)
    }

    /// Get refresh history
    ///
    /// Returns refresh operation history as tuples matching `history_schema()`.
    /// If no auto-refresh worker is configured, returns empty history.
    pub fn pg_mv_refresh_history(&self, limit: Option<usize>) -> Result<Vec<Tuple>> {
        // Get history from the auto-refresh worker if available
        if let Some(ref worker) = self.auto_refresh_worker {
            let entries = worker.get_refresh_history(limit);
            let tuples = entries.iter().map(Self::history_entry_to_tuple).collect();
            Ok(tuples)
        } else {
            // No worker configured, return empty history
            Ok(vec![])
        }
    }

    /// Convert a RefreshHistoryEntry to a tuple
    fn history_entry_to_tuple(entry: &RefreshHistoryEntry) -> Tuple {
        Tuple::new(vec![
            Value::String(entry.mv_name.clone()),
            Value::String(entry.start_time.to_rfc3339()),
            Value::String(entry.end_time.to_rfc3339()),
            Value::Boolean(entry.success),
            Value::String(entry.error_message.clone().unwrap_or_default()),
            Value::Int8(entry.rows_affected.unwrap_or(0)),
            Value::String(entry.strategy.clone()),
            Value::String(entry.trigger.clone()),
        ])
    }

    /// Get CPU usage information
    pub fn pg_mv_cpu_usage(&self) -> Result<CpuUsageInfo> {
        let stats = self.scheduler.get_stats();
        let worker_config = self.auto_refresh_worker.as_ref()
            .map(|w| w.config())
            .unwrap_or_default();

        let info = CpuUsageInfo {
            current_cpu_percent: stats.cpu_usage,
            max_cpu_percent: worker_config.max_cpu_percent,
            is_throttled: stats.cpu_usage > worker_config.max_cpu_percent,
            active_tasks: stats.running_tasks,
            queued_tasks: stats.queue_size,
        };

        Ok(info)
    }

    /// Get scheduler statistics
    pub fn pg_mv_scheduler_stats(&self) -> Result<SchedulerStats> {
        Ok(self.scheduler.get_stats())
    }

    /// Convert auto-refresh status to tuple
    pub fn status_to_tuple(status: &AutoRefreshStatus) -> Tuple {
        Tuple::new(vec![
            Value::String(status.mv_name.clone()),
            Value::Boolean(status.auto_refresh_enabled),
            Value::String(
                status.last_refresh
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "never".to_string())
            ),
            Value::Int8(status.staleness_seconds.unwrap_or(-1)),
            Value::Int8(status.threshold_seconds),
            Value::Boolean(status.is_refreshing),
            Value::String(status.refresh_strategy.clone()),
            Value::Int8(status.row_count.unwrap_or(0) as i64),
            Value::Int4(status.base_table_count as i32),
        ])
    }

    /// Get schema for pg_mv_auto_refresh_status
    pub fn status_schema() -> Schema {
        Schema::new(vec![
            Column::new("mv_name", DataType::Text),
            Column::new("auto_refresh_enabled", DataType::Boolean),
            Column::new("last_refresh", DataType::Text),
            Column::new("staleness_seconds", DataType::Int8),
            Column::new("threshold_seconds", DataType::Int8),
            Column::new("is_refreshing", DataType::Boolean),
            Column::new("refresh_strategy", DataType::Text),
            Column::new("row_count", DataType::Int8),
            Column::new("base_table_count", DataType::Int4),
        ])
    }

    /// Get schema for pg_mv_refresh_history
    pub fn history_schema() -> Schema {
        Schema::new(vec![
            Column::new("mv_name", DataType::Text),
            Column::new("start_time", DataType::Text),
            Column::new("end_time", DataType::Text),
            Column::new("success", DataType::Boolean),
            Column::new("error_message", DataType::Text),
            Column::new("rows_affected", DataType::Int8),
            Column::new("strategy", DataType::Text),
            Column::new("trigger", DataType::Text),
        ])
    }

    /// Get schema for pg_mv_cpu_usage
    pub fn cpu_usage_schema() -> Schema {
        Schema::new(vec![
            Column::new("current_cpu_percent", DataType::Float8),
            Column::new("max_cpu_percent", DataType::Float8),
            Column::new("is_throttled", DataType::Boolean),
            Column::new("active_tasks", DataType::Int4),
            Column::new("queued_tasks", DataType::Int4),
        ])
    }

    /// Convert CPU usage to tuple
    pub fn cpu_usage_to_tuple(info: &CpuUsageInfo) -> Tuple {
        Tuple::new(vec![
            Value::Float8(info.current_cpu_percent),
            Value::Float8(info.max_cpu_percent),
            Value::Boolean(info.is_throttled),
            Value::Int4(info.active_tasks as i32),
            Value::Int4(info.queued_tasks as i32),
        ])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Config;
    use super::super::mv_scheduler::SchedulerConfig;

    #[test]
    fn test_status_schema() {
        let schema = MvSystemViews::status_schema();
        assert_eq!(schema.columns.len(), 9);
        assert_eq!(schema.columns[0].name, "mv_name");
        assert_eq!(schema.columns[1].name, "auto_refresh_enabled");
    }

    #[test]
    fn test_history_schema() {
        let schema = MvSystemViews::history_schema();
        assert_eq!(schema.columns.len(), 8);
        assert_eq!(schema.columns[0].name, "mv_name");
        assert_eq!(schema.columns[2].name, "end_time");
    }

    #[test]
    fn test_cpu_usage_schema() {
        let schema = MvSystemViews::cpu_usage_schema();
        assert_eq!(schema.columns.len(), 5);
        assert_eq!(schema.columns[0].name, "current_cpu_percent");
        assert_eq!(schema.columns[2].name, "is_throttled");
    }

    #[test]
    fn test_system_views_creation() {
        let config = Config::in_memory();
        let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
        let scheduler_config = SchedulerConfig::default();
        let scheduler = Arc::new(MVScheduler::new(scheduler_config, Arc::clone(&storage)));

        let _system_views = MvSystemViews::new(storage, scheduler);
    }

    #[test]
    fn test_cpu_usage_info() {
        let info = CpuUsageInfo {
            current_cpu_percent: 45.5,
            max_cpu_percent: 70.0,
            is_throttled: false,
            active_tasks: 2,
            queued_tasks: 5,
        };

        let tuple = MvSystemViews::cpu_usage_to_tuple(&info);
        assert_eq!(tuple.values.len(), 5);

        match &tuple.values[0] {
            Value::Float8(v) => assert_eq!(*v, 45.5),
            _ => panic!("Expected Float8"),
        }
    }

    #[test]
    fn test_history_entry_to_tuple() {
        use chrono::Utc;

        let entry = RefreshHistoryEntry {
            mv_name: "test_mv".to_string(),
            start_time: Utc::now(),
            end_time: Utc::now(),
            success: true,
            error_message: None,
            rows_affected: Some(100),
            strategy: "incremental".to_string(),
            trigger: "staleness".to_string(),
        };

        let tuple = MvSystemViews::history_entry_to_tuple(&entry);
        assert_eq!(tuple.values.len(), 8);

        match &tuple.values[0] {
            Value::String(s) => assert_eq!(s, "test_mv"),
            _ => panic!("Expected String"),
        }
        match &tuple.values[3] {
            Value::Boolean(b) => assert!(*b),
            _ => panic!("Expected Boolean"),
        }
        match &tuple.values[5] {
            Value::Int8(n) => assert_eq!(*n, 100),
            _ => panic!("Expected Int8"),
        }
    }
}
