//! Query statistics collection for cost-based optimizer
//!
//! Provides table and column statistics to improve query planning accuracy.
//! Statistics are collected during data modification operations and used by
//! the query planner for cardinality estimation and selectivity calculation.

use crate::{Result, Error, DataType, Value};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use chrono::{DateTime, Utc};
use lru::LruCache;
use std::time::{Duration, Instant};
use std::num::NonZeroUsize;

/// Table-level statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableStatistics {
    /// Table name
    pub table_name: String,

    /// Total number of rows
    pub row_count: u64,

    /// Average row size in bytes
    pub avg_row_size: u64,

    /// Total table size in bytes
    pub total_size: u64,

    /// Last time statistics were analyzed
    pub last_analyzed: DateTime<Utc>,

    /// Column-level statistics
    pub columns: HashMap<String, ColumnStatistics>,
}

impl TableStatistics {
    /// Create new table statistics
    pub fn new(table_name: String) -> Self {
        Self {
            table_name,
            row_count: 0,
            avg_row_size: 0,
            total_size: 0,
            last_analyzed: Utc::now(),
            columns: HashMap::new(),
        }
    }

    /// Update table statistics after analyzing data
    pub fn update(&mut self, row_count: u64, total_size: u64) {
        self.row_count = row_count;
        self.total_size = total_size;
        self.avg_row_size = if row_count > 0 {
            total_size / row_count
        } else {
            0
        };
        self.last_analyzed = Utc::now();
    }
}

/// Column-level statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnStatistics {
    /// Column name
    pub column_name: String,

    /// Data type
    pub data_type: DataType,

    /// Number of distinct values (cardinality)
    pub n_distinct: u64,

    /// Fraction of NULL values (0.0 to 1.0)
    pub null_frac: f64,

    /// Average column width in bytes
    pub avg_width: u64,

    /// Minimum value (for ordered types)
    pub min_value: Option<Value>,

    /// Maximum value (for ordered types)
    pub max_value: Option<Value>,

    /// Most common values (for selectivity estimation)
    pub most_common_values: Vec<Value>,

    /// Frequencies of most common values
    pub most_common_freqs: Vec<f64>,

    /// Histogram bounds for range queries
    pub histogram_bounds: Vec<Value>,
}

impl ColumnStatistics {
    /// Create new column statistics
    pub fn new(column_name: String, data_type: DataType) -> Self {
        Self {
            column_name,
            data_type,
            n_distinct: 0,
            null_frac: 0.0,
            avg_width: 0,
            min_value: None,
            max_value: None,
            most_common_values: Vec::new(),
            most_common_freqs: Vec::new(),
            histogram_bounds: Vec::new(),
        }
    }

    /// Estimate selectivity for equality predicate
    pub fn estimate_equality_selectivity(&self, _value: &Value) -> f64 {
        // Simple estimation: 1 / n_distinct
        if self.n_distinct > 0 {
            1.0 / self.n_distinct as f64
        } else {
            0.1 // Default estimate
        }
    }

    /// Estimate selectivity for range predicate (column < value or column > value)
    ///
    /// Uses histogram-based estimation when histogram bounds are available,
    /// falling back to uniform distribution assumption otherwise.
    ///
    /// # Histogram-Based Estimation (v3.3.0)
    ///
    /// When histogram bounds are populated (from ANALYZE):
    /// - Uses equi-depth histogram buckets for accurate selectivity
    /// - Applies linear interpolation within buckets
    /// - Handles operator-specific logic for <, <=, >, >=
    ///
    /// # Fallback Behavior
    ///
    /// When no histogram is available, uses uniform distribution:
    /// - Returns 0.33 as conservative estimate (1/3 of rows)
    /// - Works well for unknown distributions and small tables
    ///
    /// # Arguments
    ///
    /// * `value` - The comparison value
    /// * `operator` - The comparison operator: "<", "<=", ">", ">="
    ///
    /// # Returns
    ///
    /// Selectivity estimate between 0.0 and 1.0
    pub fn estimate_range_selectivity(&self, value: &Value, operator: &str) -> f64 {
        // Fall back to uniform distribution if no histogram available
        if self.histogram_bounds.is_empty() {
            // Use min/max if available for better estimate
            if let (Some(min_val), Some(max_val)) = (&self.min_value, &self.max_value) {
                return self.estimate_range_with_minmax(value, operator, min_val, max_val);
            }
            return 0.33; // Default estimate (1/3 of rows)
        }

        // Histogram-based estimation
        let num_buckets = self.histogram_bounds.len() - 1;
        if num_buckets == 0 {
            return 0.33;
        }

        // Find the bucket containing the value
        let (bucket_idx, position_in_bucket) = self.find_histogram_bucket(value);

        // Calculate selectivity based on operator and bucket position
        // Each bucket represents 1/num_buckets of the data (equi-depth)
        let bucket_selectivity = 1.0 / num_buckets as f64;

        match operator {
            "<" | "<=" => {
                // Selectivity = (complete buckets before) + (fraction of current bucket)
                let complete_buckets = bucket_idx as f64;
                let partial = if operator == "<" {
                    position_in_bucket
                } else {
                    // For <=, include a small epsilon to account for equality
                    (position_in_bucket + 0.001).min(1.0)
                };
                (complete_buckets + partial) * bucket_selectivity
            }
            ">" | ">=" => {
                // Selectivity = (complete buckets after) + (remaining fraction of current bucket)
                let complete_buckets_after = (num_buckets - bucket_idx - 1) as f64;
                let partial = if operator == ">" {
                    1.0 - position_in_bucket
                } else {
                    // For >=, include the value itself
                    (1.0 - position_in_bucket + 0.001).min(1.0)
                };
                (complete_buckets_after + partial) * bucket_selectivity
            }
            _ => 0.33, // Unknown operator, use default
        }
    }

    /// Estimate range selectivity using only min/max values (no histogram)
    fn estimate_range_with_minmax(&self, value: &Value, operator: &str, min_val: &Value, max_val: &Value) -> f64 {
        // Try to compute numeric position within [min, max] range
        let position = match (value, min_val, max_val) {
            (Value::Int4(v), Value::Int4(min), Value::Int4(max)) => {
                if max == min { 0.5 } else {
                    (*v as f64 - *min as f64) / (*max as f64 - *min as f64)
                }
            }
            (Value::Int8(v), Value::Int8(min), Value::Int8(max)) => {
                if max == min { 0.5 } else {
                    (*v as f64 - *min as f64) / (*max as f64 - *min as f64)
                }
            }
            (Value::Float4(v), Value::Float4(min), Value::Float4(max)) => {
                if (max - min).abs() < f32::EPSILON { 0.5 } else {
                    (*v as f64 - *min as f64) / (*max as f64 - *min as f64)
                }
            }
            (Value::Float8(v), Value::Float8(min), Value::Float8(max)) => {
                if (max - min).abs() < f64::EPSILON { 0.5 } else {
                    (v - min) / (max - min)
                }
            }
            (Value::Timestamp(v), Value::Timestamp(min), Value::Timestamp(max)) => {
                if max == min { 0.5 } else {
                    let v_ts = v.timestamp_millis() as f64;
                    let min_ts = min.timestamp_millis() as f64;
                    let max_ts = max.timestamp_millis() as f64;
                    (v_ts - min_ts) / (max_ts - min_ts)
                }
            }
            _ => return 0.33, // Can't compare, use default
        };

        // Clamp position to [0, 1]
        let position = position.clamp(0.0, 1.0);

        match operator {
            "<" | "<=" => position,
            ">" | ">=" => 1.0 - position,
            _ => 0.33,
        }
    }

    /// Find which histogram bucket contains the value and position within bucket
    ///
    /// Returns (bucket_index, position_within_bucket)
    /// - bucket_index: 0-based index of the bucket
    /// - position_within_bucket: 0.0 to 1.0 indicating position within bucket
    fn find_histogram_bucket(&self, value: &Value) -> (usize, f64) {
        if self.histogram_bounds.is_empty() {
            return (0, 0.5);
        }

        let num_buckets = self.histogram_bounds.len() - 1;

        // Find the first bound that is >= value
        for i in 0..self.histogram_bounds.len() {
            let Some(bound) = self.histogram_bounds.get(i) else { break };
            let cmp = StatisticsAnalyzer::compare_values(value, bound);
            if cmp < 0 {
                // Value is less than this bound
                if i == 0 {
                    return (0, 0.0); // Before first bucket
                }
                // Value is in bucket i-1
                if let (Some(lower), Some(upper)) = (
                    self.histogram_bounds.get(i - 1),
                    self.histogram_bounds.get(i),
                ) {
                    let position = self.interpolate_position(value, lower, upper);
                    return (i - 1, position);
                }
                return (i - 1, 0.5); // Fallback if bounds missing
            } else if cmp == 0 {
                // Value equals this bound
                if i >= num_buckets {
                    return (num_buckets - 1, 1.0); // At or beyond last bucket
                }
                return (i, 0.0); // At start of bucket i
            }
        }

        // Value is beyond all bounds
        (num_buckets - 1, 1.0)
    }

    /// Interpolate position of value between two bounds
    fn interpolate_position(&self, value: &Value, lower: &Value, upper: &Value) -> f64 {
        match (value, lower, upper) {
            (Value::Int4(v), Value::Int4(lo), Value::Int4(hi)) => {
                if hi == lo { 0.5 } else {
                    (*v as f64 - *lo as f64) / (*hi as f64 - *lo as f64)
                }
            }
            (Value::Int8(v), Value::Int8(lo), Value::Int8(hi)) => {
                if hi == lo { 0.5 } else {
                    (*v as f64 - *lo as f64) / (*hi as f64 - *lo as f64)
                }
            }
            (Value::Float8(v), Value::Float8(lo), Value::Float8(hi)) => {
                if (hi - lo).abs() < f64::EPSILON { 0.5 } else {
                    (v - lo) / (hi - lo)
                }
            }
            (Value::Float4(v), Value::Float4(lo), Value::Float4(hi)) => {
                if (hi - lo).abs() < f32::EPSILON { 0.5 } else {
                    (*v as f64 - *lo as f64) / (*hi as f64 - *lo as f64)
                }
            }
            (Value::Timestamp(v), Value::Timestamp(lo), Value::Timestamp(hi)) => {
                if hi == lo { 0.5 } else {
                    let v_ts = v.timestamp_millis() as f64;
                    let lo_ts = lo.timestamp_millis() as f64;
                    let hi_ts = hi.timestamp_millis() as f64;
                    (v_ts - lo_ts) / (hi_ts - lo_ts)
                }
            }
            (Value::String(v), Value::String(lo), Value::String(hi)) => {
                // For strings, use lexicographic comparison
                if lo == hi { 0.5 } else if v <= lo { 0.0 } else if v >= hi { 1.0 } else { 0.5 }
            }
            _ => 0.5, // Default to middle of bucket
        }
        .clamp(0.0, 1.0)
    }

    /// Estimate selectivity for IS NULL predicate
    pub fn estimate_null_selectivity(&self) -> f64 {
        self.null_frac
    }

    /// Estimate selectivity for IS NOT NULL predicate
    pub fn estimate_not_null_selectivity(&self) -> f64 {
        1.0 - self.null_frac
    }
}

/// Cached statistics with adaptive TTL
#[derive(Debug, Clone)]
struct CachedStatistics {
    /// Cached table statistics
    stats: Arc<TableStatistics>,
    /// Timestamp when cached
    cached_at: Instant,
    /// Time-to-live duration (adaptive based on mutation rate)
    ttl: Duration,
    /// Number of mutations tracked at cache time
    mutations_at_cache: u64,
}

impl CachedStatistics {
    /// Create a new cached statistics entry with adaptive TTL
    fn new(stats: TableStatistics, base_ttl: Duration, mutation_count: u64) -> Self {
        Self {
            stats: Arc::new(stats),
            cached_at: Instant::now(),
            ttl: base_ttl,
            mutations_at_cache: mutation_count,
        }
    }

    /// Check if the cached statistics are still valid
    fn is_valid(&self) -> bool {
        self.cached_at.elapsed() < self.ttl
    }

    /// Check validity considering mutation count (adaptive invalidation)
    fn is_valid_with_mutations(&self, current_mutations: u64, invalidation_threshold: u64) -> bool {
        // Invalidate if TTL expired
        if !self.is_valid() {
            return false;
        }
        // Adaptive: invalidate early if many mutations occurred since caching
        let mutations_since_cache = current_mutations.saturating_sub(self.mutations_at_cache);
        mutations_since_cache < invalidation_threshold
    }
}

/// Mutation tracking for adaptive TTL
#[derive(Debug, Default)]
struct MutationTracker {
    /// Mutation counts per table
    counts: HashMap<String, u64>,
}

impl MutationTracker {
    fn new() -> Self {
        Self { counts: HashMap::new() }
    }

    fn increment(&mut self, table_name: &str) {
        *self.counts.entry(table_name.to_string()).or_insert(0) += 1;
    }

    fn get(&self, table_name: &str) -> u64 {
        self.counts.get(table_name).copied().unwrap_or(0)
    }
}

/// Statistics cache manager with adaptive TTL
///
/// Performance optimization: Implements adaptive TTL that adjusts cache duration
/// based on table mutation frequency. Frequently modified tables have shorter TTLs
/// while stable tables retain cached statistics longer, reducing unnecessary recomputation.
pub struct StatisticsCache {
    /// LRU cache for table statistics
    cache: Arc<Mutex<LruCache<String, CachedStatistics>>>,
    /// Default TTL for statistics cache (30 seconds as per spec)
    default_ttl: Duration,
    /// Minimum TTL (for frequently mutated tables)
    min_ttl: Duration,
    /// Maximum TTL (for stable tables)
    max_ttl: Duration,
    /// Mutation tracker for adaptive TTL
    mutations: Arc<Mutex<MutationTracker>>,
    /// Mutation threshold for early invalidation
    mutation_invalidation_threshold: u64,
}

impl StatisticsCache {
    /// Create a new statistics cache with default settings
    pub fn new() -> Self {
        // Performance optimization: Increased cache size from 100 to 256 entries
        // for better hit rates in larger deployments
        match Self::with_config(256, 30) {
            Ok(cache) => cache,
            Err(_) => unreachable!("default cache size of 256 is non-zero"),
        }
    }

    /// Create with custom cache configuration
    ///
    /// # Errors
    ///
    /// Returns an error if cache_size is zero.
    pub fn with_config(cache_size: usize, ttl_seconds: u64) -> Result<Self> {
        let cache_size_nz = NonZeroUsize::new(cache_size)
            .ok_or_else(|| Error::config("Cache size must be non-zero"))?;
        Ok(Self {
            cache: Arc::new(Mutex::new(LruCache::new(cache_size_nz))),
            default_ttl: Duration::from_secs(ttl_seconds),
            min_ttl: Duration::from_secs(5),      // Minimum 5 seconds for hot tables
            max_ttl: Duration::from_secs(120),    // Maximum 2 minutes for stable tables
            mutations: Arc::new(Mutex::new(MutationTracker::new())),
            mutation_invalidation_threshold: 100, // Invalidate after 100 mutations
        })
    }

    /// Get statistics from cache with adaptive invalidation
    pub fn get(&self, table_name: &str) -> Result<Option<Arc<TableStatistics>>> {
        let cache_guard = self.cache.lock().map_err(|e| {
            Error::storage(format!("Statistics cache lock error: {}", e))
        })?;

        // Get current mutation count for adaptive invalidation
        let current_mutations = self.mutations.lock()
            .map(|m| m.get(table_name))
            .unwrap_or(0);

        if let Some(cached) = cache_guard.peek(table_name) {
            if cached.is_valid_with_mutations(current_mutations, self.mutation_invalidation_threshold) {
                tracing::debug!(
                    "Statistics cache HIT for '{}' (age: {:?}, ttl: {:?}, mutations_since: {})",
                    table_name,
                    cached.cached_at.elapsed(),
                    cached.ttl,
                    current_mutations.saturating_sub(cached.mutations_at_cache)
                );
                return Ok(Some(Arc::clone(&cached.stats)));
            } else {
                tracing::debug!(
                    "Statistics cache INVALIDATED for '{}' (age: {:?}, ttl: {:?}, mutations_since: {})",
                    table_name,
                    cached.cached_at.elapsed(),
                    cached.ttl,
                    current_mutations.saturating_sub(cached.mutations_at_cache)
                );
            }
        } else {
            tracing::debug!("Statistics cache MISS for '{}'", table_name);
        }

        Ok(None)
    }

    /// Put statistics into cache with adaptive TTL
    pub fn put(&self, table_name: String, stats: TableStatistics) -> Result<()> {
        let mut cache_guard = self.cache.lock().map_err(|e| {
            Error::storage(format!("Statistics cache lock error: {}", e))
        })?;

        // Get current mutation count
        let mutation_count = self.mutations.lock()
            .map(|m| m.get(&table_name))
            .unwrap_or(0);

        // Calculate adaptive TTL based on recent mutation activity
        let adaptive_ttl = self.calculate_adaptive_ttl(&table_name, mutation_count);

        let cached = CachedStatistics::new(stats, adaptive_ttl, mutation_count);
        cache_guard.put(table_name.clone(), cached);

        tracing::debug!(
            "Statistics cached for '{}' (adaptive_ttl: {:?}, mutations: {})",
            table_name,
            adaptive_ttl,
            mutation_count
        );

        Ok(())
    }

    /// Calculate adaptive TTL based on mutation frequency
    fn calculate_adaptive_ttl(&self, table_name: &str, mutation_count: u64) -> Duration {
        // Simple heuristic: more mutations = shorter TTL
        // - 0-10 mutations: max TTL (stable table)
        // - 10-100 mutations: default TTL
        // - 100-1000 mutations: shorter TTL
        // - 1000+ mutations: min TTL (hot table)
        let ttl = if mutation_count < 10 {
            self.max_ttl
        } else if mutation_count < 100 {
            self.default_ttl
        } else if mutation_count < 1000 {
            Duration::from_secs(15) // 15 seconds for moderately active tables
        } else {
            self.min_ttl
        };

        tracing::trace!(
            "Adaptive TTL for '{}': {:?} (mutation_count: {})",
            table_name,
            ttl,
            mutation_count
        );

        ttl
    }

    /// Record a mutation for adaptive TTL tracking
    ///
    /// Call this method after INSERT, UPDATE, or DELETE operations
    /// to help the cache adjust TTL for frequently modified tables.
    pub fn record_mutation(&self, table_name: &str) -> Result<()> {
        let mut mutations = self.mutations.lock().map_err(|e| {
            Error::storage(format!("Mutation tracker lock error: {}", e))
        })?;
        mutations.increment(table_name);
        Ok(())
    }

    /// Invalidate statistics for a specific table
    pub fn invalidate(&self, table_name: &str) -> Result<()> {
        let mut cache_guard = self.cache.lock().map_err(|e| {
            Error::storage(format!("Statistics cache lock error: {}", e))
        })?;

        cache_guard.pop(table_name);
        tracing::debug!("Invalidated statistics cache for '{}'", table_name);
        Ok(())
    }

    /// Invalidate all statistics (e.g., after ANALYZE command)
    pub fn invalidate_all(&self) -> Result<()> {
        let mut cache_guard = self.cache.lock().map_err(|e| {
            Error::storage(format!("Statistics cache lock error: {}", e))
        })?;

        cache_guard.clear();
        tracing::info!("Invalidated entire statistics cache");
        Ok(())
    }

    /// Get cache statistics
    pub fn cache_stats(&self) -> Result<(usize, usize)> {
        let cache_guard = self.cache.lock().map_err(|e| {
            Error::storage(format!("Statistics cache lock error: {}", e))
        })?;

        Ok((cache_guard.len(), cache_guard.cap().get()))
    }
}

impl Default for StatisticsCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics analyzer that collects statistics from table data
pub struct StatisticsAnalyzer;

impl StatisticsAnalyzer {
    /// Analyze a table and collect statistics
    ///
    /// This performs a full table scan and computes:
    /// - Row count
    /// - Average row size
    /// - Per-column statistics (distinct values, nulls, min/max)
    pub fn analyze_table(
        table_name: &str,
        tuples: &[crate::Tuple],
        schema: &crate::Schema,
    ) -> Result<TableStatistics> {
        let mut stats = TableStatistics::new(table_name.to_string());

        if tuples.is_empty() {
            return Ok(stats);
        }

        // Initialize column statistics
        for column in &schema.columns {
            let col_stats = ColumnStatistics::new(
                column.name.clone(),
                column.data_type.clone(),
            );
            stats.columns.insert(column.name.clone(), col_stats);
        }

        // Collect statistics by scanning tuples
        let row_count = tuples.len() as u64;
        let mut total_size = 0u64;
        let mut column_distinct_values: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
        let mut column_null_counts: HashMap<String, u64> = HashMap::new();
        let mut column_sizes: HashMap<String, Vec<u64>> = HashMap::new();
        let mut column_values: HashMap<String, Vec<Value>> = HashMap::new();

        // Initialize tracking structures
        for column in &schema.columns {
            column_distinct_values.insert(column.name.clone(), std::collections::HashSet::new());
            column_null_counts.insert(column.name.clone(), 0);
            column_sizes.insert(column.name.clone(), Vec::new());
            column_values.insert(column.name.clone(), Vec::new());
        }

        // Scan all tuples
        for tuple in tuples {
            // Estimate tuple size
            let tuple_size = Self::estimate_tuple_size(tuple);
            total_size += tuple_size;

            // Process each column value
            for (i, value) in tuple.values.iter().enumerate() {
                let column = match schema.columns.get(i) {
                    Some(col) => col,
                    None => continue,
                };

                let column_name = &column.name;
                let value_size = Self::estimate_value_size(value);

                // Track column sizes
                if let Some(sizes) = column_sizes.get_mut(column_name) {
                    sizes.push(value_size);
                }

                // Track NULL values
                if matches!(value, Value::Null) {
                    if let Some(count) = column_null_counts.get_mut(column_name) {
                        *count += 1;
                    }
                    continue;
                }

                // Track distinct values (simplified: use string representation)
                if let Some(distinct_set) = column_distinct_values.get_mut(column_name) {
                    let value_str = format!("{:?}", value);
                    distinct_set.insert(value_str);
                }

                // Collect values for histogram generation (only for orderable types)
                if Self::is_orderable(value) {
                    if let Some(values) = column_values.get_mut(column_name) {
                        values.push(value.clone());
                    }
                }

                // Update min/max values
                if let Some(col_stats) = stats.columns.get_mut(column_name) {
                    let should_update_min = col_stats.min_value.as_ref()
                        .is_none_or(|min_val| Self::compare_values(value, min_val) < 0);
                    if should_update_min {
                        col_stats.min_value = Some(value.clone());
                    }
                    let should_update_max = col_stats.max_value.as_ref()
                        .is_none_or(|max_val| Self::compare_values(value, max_val) > 0);
                    if should_update_max {
                        col_stats.max_value = Some(value.clone());
                    }
                }
            }
        }

        // Finalize statistics
        stats.update(row_count, total_size);

        // Update column statistics
        for (column_name, col_stats) in &mut stats.columns {
            // Set distinct count
            if let Some(distinct_set) = column_distinct_values.get(column_name) {
                col_stats.n_distinct = distinct_set.len() as u64;
            }

            // Set NULL fraction
            if let Some(null_count) = column_null_counts.get(column_name) {
                col_stats.null_frac = *null_count as f64 / row_count as f64;
            }

            // Set average width
            if let Some(sizes) = column_sizes.get(column_name) {
                if !sizes.is_empty() {
                    let total: u64 = sizes.iter().sum();
                    col_stats.avg_width = total / sizes.len() as u64;
                }
            }

            // Generate histogram bounds for orderable columns
            if let Some(values) = column_values.get_mut(column_name) {
                if values.len() >= 10 {
                    // Sort values for histogram generation
                    values.sort_by(|a, b| {
                        match Self::compare_values(a, b) {
                            -1 => std::cmp::Ordering::Less,
                            1 => std::cmp::Ordering::Greater,
                            _ => std::cmp::Ordering::Equal,
                        }
                    });

                    // Create equi-depth histogram with ~100 buckets (or fewer if less data)
                    let num_buckets = (values.len() / 10).min(100).max(1);
                    let bucket_size = values.len() / (num_buckets + 1);

                    let mut bounds = Vec::with_capacity(num_buckets + 1);

                    // First bound is the minimum value
                    if let Some(first) = values.first() {
                        bounds.push(first.clone());
                    }

                    // Add bucket boundaries
                    for i in 1..=num_buckets {
                        let idx = (i * bucket_size).min(values.len() - 1);
                        if let Some(val) = values.get(idx) {
                            bounds.push(val.clone());
                        }
                    }

                    // Ensure last bound is the maximum value
                    if bounds.last() != values.last() {
                        if let Some(last) = values.last() {
                            bounds.push(last.clone());
                        }
                    }

                    col_stats.histogram_bounds = bounds;
                }
            }
        }

        Ok(stats)
    }

    /// Check if a value type is orderable for histogram generation
    fn is_orderable(value: &Value) -> bool {
        matches!(
            value,
            Value::Int2(_)
                | Value::Int4(_)
                | Value::Int8(_)
                | Value::Float4(_)
                | Value::Float8(_)
                | Value::Timestamp(_)
                | Value::String(_)
        )
    }

    /// Estimate tuple size in bytes
    fn estimate_tuple_size(tuple: &crate::Tuple) -> u64 {
        let mut size = 0u64;
        for value in &tuple.values {
            size += Self::estimate_value_size(value);
        }
        size
    }

    /// Estimate value size in bytes
    fn estimate_value_size(value: &Value) -> u64 {
        match value {
            Value::Null => 1,
            Value::Boolean(_) => 1,
            Value::Int2(_) => 2,
            Value::Int4(_) => 4,
            Value::Int8(_) => 8,
            Value::Float4(_) => 4,
            Value::Float8(_) => 8,
            Value::Numeric(n) => n.len() as u64,
            Value::String(s) | Value::Json(s) => s.len() as u64,
            Value::Bytes(b) => b.len() as u64,
            Value::Timestamp(_) => 8,
            Value::Date(_) => 4, // NaiveDate typically 4 bytes
            Value::Time(_) => 8, // NaiveTime typically 8 bytes
            Value::Uuid(_) => 16,
            Value::Array(arr) => arr.iter().map(Self::estimate_value_size).sum(),
            Value::Vector(vec) => (vec.len() * 4) as u64, // f32 = 4 bytes each
            // Storage references (dict_id is u32, hash is 32 bytes)
            Value::DictRef { .. } => 4,
            Value::CasRef { .. } => 32,
            Value::ColumnarRef => 1,
            Value::Interval(_) => 16, // Interval contains months, days, microseconds
        }
    }

    /// Compare two values (returns -1, 0, or 1)
    fn compare_values(a: &Value, b: &Value) -> i32 {
        match (a, b) {
            (Value::Int4(x), Value::Int4(y)) => {
                match x.cmp(y) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Greater => 1,
                    std::cmp::Ordering::Equal => 0,
                }
            }
            (Value::Int8(x), Value::Int8(y)) => {
                match x.cmp(y) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Greater => 1,
                    std::cmp::Ordering::Equal => 0,
                }
            }
            (Value::Float8(x), Value::Float8(y)) => {
                if x < y { -1 } else if x > y { 1 } else { 0 }
            }
            (Value::String(x), Value::String(y)) => {
                match x.cmp(y) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Greater => 1,
                    std::cmp::Ordering::Equal => 0,
                }
            }
            (Value::Timestamp(x), Value::Timestamp(y)) => {
                match x.cmp(y) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Greater => 1,
                    std::cmp::Ordering::Equal => 0,
                }
            }
            _ => 0, // Default: consider equal
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{Column, Schema, Tuple, Value};

    #[test]
    fn test_analyze_empty_table() {
        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
        ]);

        let tuples = vec![];
        let stats = StatisticsAnalyzer::analyze_table("test", &tuples, &schema).unwrap();

        assert_eq!(stats.row_count, 0);
        assert_eq!(stats.avg_row_size, 0);
        assert_eq!(stats.total_size, 0);
    }

    #[test]
    fn test_analyze_simple_table() {
        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("name", DataType::Text),
        ]);

        let tuples = vec![
            Tuple::new(vec![Value::Int4(1), Value::String("Alice".to_string())]),
            Tuple::new(vec![Value::Int4(2), Value::String("Bob".to_string())]),
            Tuple::new(vec![Value::Int4(3), Value::String("Charlie".to_string())]),
        ];

        let stats = StatisticsAnalyzer::analyze_table("test", &tuples, &schema).unwrap();

        assert_eq!(stats.row_count, 3);
        assert!(stats.avg_row_size > 0);
        assert!(stats.total_size > 0);

        // Check column statistics
        let id_stats = stats.columns.get("id").unwrap();
        assert_eq!(id_stats.n_distinct, 3);
        assert_eq!(id_stats.null_frac, 0.0);

        let name_stats = stats.columns.get("name").unwrap();
        assert_eq!(name_stats.n_distinct, 3);
        assert_eq!(name_stats.null_frac, 0.0);
    }

    #[test]
    fn test_analyze_with_nulls() {
        let schema = Schema::new(vec![
            Column::new("value", DataType::Int4),
        ]);

        let tuples = vec![
            Tuple::new(vec![Value::Int4(1)]),
            Tuple::new(vec![Value::Null]),
            Tuple::new(vec![Value::Int4(2)]),
            Tuple::new(vec![Value::Null]),
        ];

        let stats = StatisticsAnalyzer::analyze_table("test", &tuples, &schema).unwrap();

        let col_stats = stats.columns.get("value").unwrap();
        assert_eq!(col_stats.n_distinct, 2); // 1 and 2
        assert_eq!(col_stats.null_frac, 0.5); // 2 out of 4
    }

    #[test]
    fn test_analyze_distinct_count() {
        let schema = Schema::new(vec![
            Column::new("category", DataType::Text),
        ]);

        let tuples = vec![
            Tuple::new(vec![Value::String("A".to_string())]),
            Tuple::new(vec![Value::String("B".to_string())]),
            Tuple::new(vec![Value::String("A".to_string())]),
            Tuple::new(vec![Value::String("C".to_string())]),
            Tuple::new(vec![Value::String("B".to_string())]),
        ];

        let stats = StatisticsAnalyzer::analyze_table("test", &tuples, &schema).unwrap();

        let col_stats = stats.columns.get("category").unwrap();
        assert_eq!(col_stats.n_distinct, 3); // A, B, C
    }

    #[test]
    fn test_selectivity_estimation() {
        let mut col_stats = ColumnStatistics::new("test".to_string(), DataType::Int4);
        col_stats.n_distinct = 100;
        col_stats.null_frac = 0.1;

        // Equality selectivity
        let eq_sel = col_stats.estimate_equality_selectivity(&Value::Int4(42));
        assert_eq!(eq_sel, 0.01); // 1/100

        // NULL selectivity
        let null_sel = col_stats.estimate_null_selectivity();
        assert_eq!(null_sel, 0.1);

        // NOT NULL selectivity
        let not_null_sel = col_stats.estimate_not_null_selectivity();
        assert_eq!(not_null_sel, 0.9);
    }

    #[test]
    fn test_histogram_generation() {
        let schema = Schema::new(vec![
            Column::new("value", DataType::Int4),
        ]);

        // Create 100 values to trigger histogram generation (needs >= 10)
        let tuples: Vec<Tuple> = (1..=100)
            .map(|i| Tuple::new(vec![Value::Int4(i)]))
            .collect();

        let stats = StatisticsAnalyzer::analyze_table("test", &tuples, &schema).unwrap();
        let col_stats = stats.columns.get("value").unwrap();

        // Histogram should be generated
        assert!(!col_stats.histogram_bounds.is_empty());
        // First bound should be min value (1)
        assert_eq!(col_stats.histogram_bounds[0], Value::Int4(1));
        // Last bound should be max value (100)
        assert_eq!(col_stats.histogram_bounds.last().unwrap(), &Value::Int4(100));
    }

    #[test]
    fn test_histogram_range_selectivity() {
        let mut col_stats = ColumnStatistics::new("value".to_string(), DataType::Int4);

        // Create histogram bounds: 0, 25, 50, 75, 100 (4 buckets of equal depth)
        col_stats.histogram_bounds = vec![
            Value::Int4(0),
            Value::Int4(25),
            Value::Int4(50),
            Value::Int4(75),
            Value::Int4(100),
        ];

        // Value 50 is at the middle of the range
        // For "<" operator, should be ~0.5 selectivity
        let sel_less = col_stats.estimate_range_selectivity(&Value::Int4(50), "<");
        assert!(sel_less > 0.4 && sel_less < 0.6, "Expected ~0.5, got {}", sel_less);

        // For ">" operator, should also be ~0.5 selectivity
        let sel_greater = col_stats.estimate_range_selectivity(&Value::Int4(50), ">");
        assert!(sel_greater > 0.4 && sel_greater < 0.6, "Expected ~0.5, got {}", sel_greater);

        // Value 25 is at the 1/4 mark
        // For "<" operator, should be ~0.25 selectivity
        let sel_q1 = col_stats.estimate_range_selectivity(&Value::Int4(25), "<");
        assert!(sel_q1 >= 0.0 && sel_q1 <= 0.35, "Expected ~0.25, got {}", sel_q1);

        // Value 75 is at the 3/4 mark
        // For "<" operator, should be ~0.75 selectivity
        let sel_q3 = col_stats.estimate_range_selectivity(&Value::Int4(75), "<");
        assert!(sel_q3 >= 0.65 && sel_q3 <= 0.85, "Expected ~0.75, got {}", sel_q3);
    }

    #[test]
    fn test_minmax_range_selectivity() {
        let mut col_stats = ColumnStatistics::new("value".to_string(), DataType::Int4);

        // No histogram, but set min/max
        col_stats.min_value = Some(Value::Int4(0));
        col_stats.max_value = Some(Value::Int4(100));

        // Value 50 should give ~0.5 selectivity for "<"
        let sel = col_stats.estimate_range_selectivity(&Value::Int4(50), "<");
        assert!((sel - 0.5).abs() < 0.01, "Expected 0.5, got {}", sel);

        // Value 25 should give ~0.25 selectivity for "<"
        let sel_q1 = col_stats.estimate_range_selectivity(&Value::Int4(25), "<");
        assert!((sel_q1 - 0.25).abs() < 0.01, "Expected 0.25, got {}", sel_q1);

        // Value 75 should give ~0.25 selectivity for ">"
        let sel_q3_gt = col_stats.estimate_range_selectivity(&Value::Int4(75), ">");
        assert!((sel_q3_gt - 0.25).abs() < 0.01, "Expected 0.25, got {}", sel_q3_gt);
    }

    #[test]
    fn test_range_selectivity_fallback() {
        // No histogram, no min/max - should return default 0.33
        let col_stats = ColumnStatistics::new("value".to_string(), DataType::Int4);

        let sel = col_stats.estimate_range_selectivity(&Value::Int4(50), "<");
        assert_eq!(sel, 0.33);
    }
}
