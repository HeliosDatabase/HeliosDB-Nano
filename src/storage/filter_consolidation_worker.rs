//! Background Filter Index Consolidation Worker
//!
//! CPU-aware background worker that consolidates filter index deltas into base structures.
//! Inspired by the AutoRefreshWorker pattern for materialized views.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use tracing::{info, debug, error};

use crate::{Result, Error};
use super::filter_index_delta::FilterIndexDeltaTracker;
use super::bloom_filter::TableBloomFilters;
use super::zone_map::{TableZoneMap, BlockZoneMap, ColumnZoneMap};
use super::mv_scheduler::CpuMonitor;

/// Configuration for filter consolidation worker
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationConfig {
    /// Enable consolidation worker
    pub enabled: bool,
    /// Check interval in seconds
    pub check_interval_seconds: u64,
    /// Maximum CPU usage before throttling (0-100)
    pub max_cpu_percent: f64,
    /// Delta count threshold before consolidation
    pub delta_threshold: u64,
    /// Time threshold before consolidation (seconds since last)
    pub time_threshold_seconds: u64,
    /// Enable parallel consolidation
    pub parallel_consolidation: bool,
    /// Maximum concurrent consolidations
    pub max_concurrent: usize,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_seconds: 10,
            max_cpu_percent: 15.0,
            delta_threshold: 1000,
            time_threshold_seconds: 300,
            parallel_consolidation: true,
            max_concurrent: 4,
        }
    }
}

/// Entry in the consolidation history log
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationHistoryEntry {
    pub table_name: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub success: bool,
    pub error_message: Option<String>,
    pub deltas_processed: u64,
    pub bloom_filters_updated: usize,
    pub zone_maps_updated: usize,
}

/// Statistics for consolidation worker
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConsolidationStats {
    pub total_consolidations: u64,
    pub successful_consolidations: u64,
    pub failed_consolidations: u64,
    pub total_deltas_processed: u64,
    pub total_bloom_updates: u64,
    pub total_zone_updates: u64,
    pub last_consolidation_time: Option<DateTime<Utc>>,
    pub avg_consolidation_ms: f64,
    pub skipped_due_to_cpu: u64,
}

/// Background worker for filter index consolidation
pub struct FilterConsolidationWorker {
    /// Configuration
    config: ConsolidationConfig,
    /// CPU monitor (shared with MV scheduler)
    cpu_monitor: Arc<CpuMonitor>,
    /// Delta tracker reference
    delta_tracker: Arc<FilterIndexDeltaTracker>,
    /// Per-table bloom filters
    bloom_filters: Arc<RwLock<HashMap<String, TableBloomFilters>>>,
    /// Per-table zone maps
    zone_maps: Arc<RwLock<HashMap<String, TableZoneMap>>>,
    /// Shutdown signal
    shutdown: Arc<AtomicBool>,
    /// Running flag
    running: Arc<AtomicBool>,
    /// Statistics
    stats: Arc<RwLock<ConsolidationStats>>,
    /// History log
    history: Arc<Mutex<Vec<ConsolidationHistoryEntry>>>,
    /// Currently consolidating tables
    consolidating: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl FilterConsolidationWorker {
    /// Create a new consolidation worker
    pub fn new(
        config: ConsolidationConfig,
        delta_tracker: Arc<FilterIndexDeltaTracker>,
        cpu_monitor: Arc<CpuMonitor>,
    ) -> Self {
        Self {
            config,
            cpu_monitor,
            delta_tracker,
            bloom_filters: Arc::new(RwLock::new(HashMap::new())),
            zone_maps: Arc::new(RwLock::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(RwLock::new(ConsolidationStats::default())),
            history: Arc::new(Mutex::new(Vec::new())),
            consolidating: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Start the background worker
    pub fn start(&self) -> Result<()> {
        if !self.config.enabled {
            info!("Filter consolidation worker is disabled");
            return Ok(());
        }

        if self.running.swap(true, Ordering::SeqCst) {
            return Err(Error::storage("Consolidation worker already running"));
        }

        self.shutdown.store(false, Ordering::SeqCst);

        let config = self.config.clone();
        let cpu_monitor = self.cpu_monitor.clone();
        let delta_tracker = self.delta_tracker.clone();
        let bloom_filters = self.bloom_filters.clone();
        let zone_maps = self.zone_maps.clone();
        let shutdown = self.shutdown.clone();
        let running = self.running.clone();
        let stats = self.stats.clone();
        let history = self.history.clone();
        let consolidating = self.consolidating.clone();

        std::thread::spawn(move || {
            info!("Filter consolidation worker started");

            while !shutdown.load(Ordering::Relaxed) {
                // Check CPU usage
                let cpu_usage = cpu_monitor.get_cpu_usage().unwrap_or(0.0);
                if cpu_usage > config.max_cpu_percent {
                    debug!("CPU too high ({:.1}%), skipping consolidation", cpu_usage);
                    stats.write().skipped_due_to_cpu += 1;
                    std::thread::sleep(Duration::from_secs(config.check_interval_seconds));
                    continue;
                }

                // Find tables needing consolidation
                let tables = delta_tracker.tables_needing_consolidation();

                for table in tables.into_iter().take(config.max_concurrent) {
                    // Skip if already consolidating
                    {
                        let mut consolidating = consolidating.lock();
                        if consolidating.contains(&table) {
                            continue;
                        }
                        consolidating.insert(table.clone());
                    }

                    let start_time = Utc::now();
                    let result = Self::consolidate_table(
                        &table,
                        &delta_tracker,
                        &bloom_filters,
                        &zone_maps,
                        config.parallel_consolidation,
                    );

                    let end_time = Utc::now();
                    let duration_ms = (end_time - start_time).num_milliseconds() as f64;

                    // Update stats and history
                    {
                        let mut s = stats.write();
                        s.total_consolidations += 1;

                        match &result {
                            Ok((deltas, blooms, zones)) => {
                                s.successful_consolidations += 1;
                                s.total_deltas_processed += *deltas;
                                s.total_bloom_updates += *blooms as u64;
                                s.total_zone_updates += *zones as u64;
                                s.last_consolidation_time = Some(end_time);

                                // Running average
                                let total = s.successful_consolidations as f64;
                                s.avg_consolidation_ms =
                                    (s.avg_consolidation_ms * (total - 1.0) + duration_ms) / total;

                                let entry = ConsolidationHistoryEntry {
                                    table_name: table.clone(),
                                    start_time,
                                    end_time,
                                    success: true,
                                    error_message: None,
                                    deltas_processed: *deltas,
                                    bloom_filters_updated: *blooms,
                                    zone_maps_updated: *zones,
                                };
                                let mut h = history.lock();
                                h.push(entry);
                                if h.len() > 1000 {
                                    h.remove(0);
                                }
                            }
                            Err(e) => {
                                s.failed_consolidations += 1;
                                error!("Consolidation failed for {}: {}", table, e);

                                let entry = ConsolidationHistoryEntry {
                                    table_name: table.clone(),
                                    start_time,
                                    end_time,
                                    success: false,
                                    error_message: Some(e.to_string()),
                                    deltas_processed: 0,
                                    bloom_filters_updated: 0,
                                    zone_maps_updated: 0,
                                };
                                let mut h = history.lock();
                                h.push(entry);
                                if h.len() > 1000 {
                                    h.remove(0);
                                }
                            }
                        }
                    }

                    // Remove from consolidating set
                    consolidating.lock().remove(&table);
                }

                std::thread::sleep(Duration::from_secs(config.check_interval_seconds));
            }

            running.store(false, Ordering::SeqCst);
            info!("Filter consolidation worker stopped");
        });

        Ok(())
    }

    /// Stop the background worker
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Check if worker is running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Get statistics
    pub fn stats(&self) -> ConsolidationStats {
        self.stats.read().clone()
    }

    /// Get recent history
    pub fn history(&self, limit: usize) -> Vec<ConsolidationHistoryEntry> {
        let h = self.history.lock();
        h.iter().rev().take(limit).cloned().collect()
    }

    /// Register bloom filters for a table
    pub fn register_bloom_filters(&self, table: &str, filters: TableBloomFilters) {
        self.bloom_filters.write().insert(table.to_string(), filters);
    }

    /// Register zone maps for a table
    pub fn register_zone_maps(&self, table: &str, zone_map: TableZoneMap) {
        self.zone_maps.write().insert(table.to_string(), zone_map);
    }

    /// Get bloom filters for a table
    pub fn get_bloom_filters(&self, table: &str) -> Option<TableBloomFilters> {
        self.bloom_filters.read().get(table).cloned()
    }

    /// Get zone maps for a table
    pub fn get_zone_maps(&self, table: &str) -> Option<TableZoneMap> {
        self.zone_maps.read().get(table).cloned()
    }

    /// Consolidate a single table's deltas into base structures
    fn consolidate_table(
        table: &str,
        delta_tracker: &Arc<FilterIndexDeltaTracker>,
        bloom_filters: &Arc<RwLock<HashMap<String, TableBloomFilters>>>,
        zone_maps: &Arc<RwLock<HashMap<String, TableZoneMap>>>,
        parallel: bool,
    ) -> Result<(u64, usize, usize)> {
        // Get deltas for this table
        let deltas = delta_tracker.get_table_deltas(table)
            .ok_or_else(|| Error::storage("No deltas found for table"))?;

        let delta_count = deltas.delta_count;
        let mut bloom_updates = 0;
        let mut zone_updates = 0;

        // Update bloom filters
        if !deltas.bloom_deltas.is_empty() {
            let mut filters = bloom_filters.write();
            let table_filters = filters.entry(table.to_string()).or_insert_with(|| {
                TableBloomFilters::new(table.to_string(), 10000) // Default expected rows
            });

            if parallel {
                // Parallel bloom filter updates
                use rayon::prelude::*;
                let updates: Vec<_> = deltas.bloom_deltas.par_iter()
                    .map(|(col, delta)| (col.clone(), delta.clone()))
                    .collect();

                for (col, delta) in updates {
                    // Find or create filter for this column
                    if let Some(cf) = table_filters.column_filters.iter_mut()
                        .find(|cf| cf.column_name == col)
                    {
                        delta.apply_to(&mut cf.filter);
                    } else {
                        table_filters.add_column(col.clone(), 10000);
                        if let Some(cf) = table_filters.column_filters.last_mut() {
                            delta.apply_to(&mut cf.filter);
                        }
                    }
                    bloom_updates += 1;
                }
            } else {
                for (col, delta) in &deltas.bloom_deltas {
                    // Find or create filter for this column
                    if let Some(cf) = table_filters.column_filters.iter_mut()
                        .find(|cf| cf.column_name == *col)
                    {
                        delta.apply_to(&mut cf.filter);
                    } else {
                        table_filters.add_column(col.clone(), 10000);
                        if let Some(cf) = table_filters.column_filters.last_mut() {
                            delta.apply_to(&mut cf.filter);
                        }
                    }
                    bloom_updates += 1;
                }
            }
        }

        // Update zone maps
        if !deltas.zone_deltas.is_empty() {
            let mut maps = zone_maps.write();
            let table_map = maps.entry(table.to_string()).or_insert_with(|| {
                TableZoneMap::new(table.to_string(), 1000)
            });

            for (block_id, zone_delta) in &deltas.zone_deltas {
                let block_idx = *block_id as usize;

                // Ensure we have enough blocks in the Vec
                while table_map.blocks.len() <= block_idx {
                    let new_block_id = table_map.blocks.len() as u64;
                    let first_row_id = new_block_id * table_map.block_size as u64;
                    table_map.blocks.push(BlockZoneMap::new(new_block_id, first_row_id));
                }

                let Some(block_map) = table_map.blocks.get_mut(block_idx) else {
                    continue;
                };

                for update in &zone_delta.column_updates {
                    if update.is_null {
                        continue;
                    }

                    let col_map = block_map.columns.entry(update.column_name.clone())
                        .or_insert_with(|| ColumnZoneMap::new(update.column_name.clone()));

                    // Expand range if needed
                    col_map.update(&update.value);
                }

                block_map.row_count += zone_delta.rows_affected;
                zone_updates += 1;
            }
        }

        debug!(
            "Consolidated table {}: {} deltas, {} bloom updates, {} zone updates",
            table, delta_count, bloom_updates, zone_updates
        );

        Ok((delta_count, bloom_updates, zone_updates))
    }

    /// Force immediate consolidation of a table
    pub fn force_consolidate(&self, table: &str) -> Result<(u64, usize, usize)> {
        Self::consolidate_table(
            table,
            &self.delta_tracker,
            &self.bloom_filters,
            &self.zone_maps,
            self.config.parallel_consolidation,
        )
    }
}

impl Drop for FilterConsolidationWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::filter_index_delta::FilterIndexConfig;

    #[test]
    fn test_consolidation_config_default() {
        let config = ConsolidationConfig::default();
        assert!(config.enabled);
        assert_eq!(config.check_interval_seconds, 10);
        assert_eq!(config.max_cpu_percent, 15.0);
    }

    #[test]
    fn test_worker_creation() {
        let config = ConsolidationConfig::default();
        let delta_tracker = Arc::new(FilterIndexDeltaTracker::new(FilterIndexConfig::default()));
        let cpu_monitor = Arc::new(CpuMonitor::new());

        let worker = FilterConsolidationWorker::new(config, delta_tracker, cpu_monitor);
        assert!(!worker.is_running());
    }
}
