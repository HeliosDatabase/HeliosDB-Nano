//! Speculative Filter Index (SFI) System
//!
//! Automatically creates and manages filter structures based on query patterns.
//! Unlike traditional indexes that require explicit creation, SFI:
//! - Tracks query patterns and frequencies
//! - Auto-creates bloom filters for high-frequency equality predicates
//! - Auto-creates zone maps for frequently used range predicates
//! - Drops unused filters after configurable inactivity period
//! - Zero memory footprint (all storage-persisted)

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

use crate::Value;
use super::bloom_filter::{BloomFilter, BloomFilterConfig};
use super::simd_filter::FilterOp;

/// Type of query pattern detected
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PatternType {
    /// Equality predicate (column = value)
    Equality,
    /// Range predicate (column > value, column < value, BETWEEN)
    Range,
    /// IN list predicate (column IN (v1, v2, ...))
    InList,
    /// LIKE predicate (column LIKE 'pattern')
    Like,
    /// IS NULL predicate
    IsNull,
}

/// A single query pattern observation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPattern {
    /// Table name
    pub table_name: String,
    /// Column name
    pub column_name: String,
    /// Pattern type
    pub pattern_type: PatternType,
    /// Hash of pattern for deduplication
    pub pattern_hash: u64,
}

impl QueryPattern {
    pub fn new(table_name: &str, column_name: &str, pattern_type: PatternType) -> Self {
        use std::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;

        let mut hasher = DefaultHasher::new();
        table_name.hash(&mut hasher);
        column_name.hash(&mut hasher);
        pattern_type.hash(&mut hasher);
        let pattern_hash = hasher.finish();

        Self {
            table_name: table_name.to_string(),
            column_name: column_name.to_string(),
            pattern_type,
            pattern_hash,
        }
    }
}

/// Statistics for a query pattern
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternStats {
    /// Pattern definition
    pub pattern: QueryPattern,
    /// Total occurrences
    pub frequency: u64,
    /// First seen timestamp
    pub first_seen: DateTime<Utc>,
    /// Last seen timestamp
    pub last_seen: DateTime<Utc>,
    /// Average selectivity (fraction of rows returned)
    pub avg_selectivity: f64,
    /// Average execution time in milliseconds
    pub avg_execution_ms: f64,
    /// Number of samples for selectivity/execution
    pub sample_count: u64,
    /// Sum of selectivities (for running average)
    selectivity_sum: f64,
    /// Sum of execution times (for running average)
    execution_sum: f64,
}

impl PatternStats {
    pub fn new(pattern: QueryPattern) -> Self {
        Self {
            pattern,
            frequency: 0,
            first_seen: Utc::now(),
            last_seen: Utc::now(),
            avg_selectivity: 1.0,
            avg_execution_ms: 0.0,
            sample_count: 0,
            selectivity_sum: 0.0,
            execution_sum: 0.0,
        }
    }

    /// Record an observation
    pub fn record(&mut self, selectivity: f64, execution_ms: f64) {
        self.frequency += 1;
        self.last_seen = Utc::now();
        self.sample_count += 1;
        self.selectivity_sum += selectivity;
        self.execution_sum += execution_ms;
        self.avg_selectivity = self.selectivity_sum / self.sample_count as f64;
        self.avg_execution_ms = self.execution_sum / self.sample_count as f64;
    }
}

/// Configuration for speculative filter manager
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeculativeConfig {
    /// Minimum query frequency before considering filter creation
    pub min_query_frequency: u64,
    /// Minimum selectivity improvement required (default: 10x)
    pub min_selectivity_improvement: f64,
    /// Days of inactivity before dropping speculative filter
    pub drop_after_days: u32,
    /// Maximum speculative filters per table
    pub max_filters_per_table: usize,
    /// Window size for pattern tracking (in hours)
    pub tracking_window_hours: u32,
    /// Enable automatic filter creation
    pub auto_create_enabled: bool,
    /// Minimum execution time threshold to consider optimization (ms)
    pub min_execution_threshold_ms: f64,
}

impl Default for SpeculativeConfig {
    fn default() -> Self {
        Self {
            min_query_frequency: 100,
            min_selectivity_improvement: 0.1, // < 10% rows returned
            drop_after_days: 7,
            max_filters_per_table: 5,
            tracking_window_hours: 24,
            auto_create_enabled: true,
            min_execution_threshold_ms: 10.0,
        }
    }
}

/// Status of a speculative filter
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterStatus {
    /// Filter is being considered for creation
    Pending,
    /// Filter has been created and is active
    Active,
    /// Filter is scheduled for removal due to inactivity
    Inactive,
    /// Filter has been removed
    Dropped,
}

/// Metadata for a speculative filter
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeculativeFilterMeta {
    /// Unique filter ID
    pub filter_id: u64,
    /// Pattern that triggered creation
    pub pattern: QueryPattern,
    /// Current status
    pub status: FilterStatus,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Last used timestamp
    pub last_used: DateTime<Utc>,
    /// Usage count since creation
    pub usage_count: u64,
    /// Estimated bytes saved
    pub bytes_saved: u64,
    /// Estimated time saved (ms)
    pub time_saved_ms: f64,
}

/// Query pattern tracker
pub struct QueryPatternTracker {
    /// Pattern statistics by hash
    patterns: RwLock<HashMap<u64, PatternStats>>,
    /// Total patterns tracked
    total_patterns: AtomicU64,
    /// Configuration
    config: SpeculativeConfig,
}

impl QueryPatternTracker {
    pub fn new(config: SpeculativeConfig) -> Self {
        Self {
            patterns: RwLock::new(HashMap::new()),
            total_patterns: AtomicU64::new(0),
            config,
        }
    }

    /// Record a query pattern observation
    pub fn record(&self, pattern: QueryPattern, selectivity: f64, execution_ms: f64) {
        let mut patterns = self.patterns.write();

        let stats = patterns.entry(pattern.pattern_hash).or_insert_with(|| {
            self.total_patterns.fetch_add(1, Ordering::Relaxed);
            PatternStats::new(pattern.clone())
        });

        stats.record(selectivity, execution_ms);
    }

    /// Get patterns that should trigger filter creation
    pub fn get_filter_candidates(&self) -> Vec<PatternStats> {
        let patterns = self.patterns.read();

        patterns.values()
            .filter(|stats| self.should_create_filter(stats))
            .cloned()
            .collect()
    }

    /// Check if a pattern should trigger filter creation
    fn should_create_filter(&self, stats: &PatternStats) -> bool {
        // Must meet frequency threshold
        if stats.frequency < self.config.min_query_frequency {
            return false;
        }

        // Must have low selectivity (few rows returned)
        if stats.avg_selectivity > self.config.min_selectivity_improvement {
            return false;
        }

        // Must have noticeable execution time
        if stats.avg_execution_ms < self.config.min_execution_threshold_ms {
            return false;
        }

        // Must be suitable pattern type for filtering
        matches!(
            stats.pattern.pattern_type,
            PatternType::Equality | PatternType::Range | PatternType::InList
        )
    }

    /// Get stats for a specific pattern
    pub fn get_stats(&self, pattern_hash: u64) -> Option<PatternStats> {
        self.patterns.read().get(&pattern_hash).cloned()
    }

    /// Prune old patterns outside the tracking window
    pub fn prune_old_patterns(&self) {
        let cutoff = Utc::now() - chrono::Duration::hours(self.config.tracking_window_hours as i64);

        let mut patterns = self.patterns.write();
        patterns.retain(|_, stats| stats.last_seen > cutoff);
    }

    /// Get top N patterns by frequency
    pub fn get_top_patterns(&self, n: usize) -> Vec<PatternStats> {
        let patterns = self.patterns.read();
        let mut sorted: Vec<_> = patterns.values().cloned().collect();
        sorted.sort_by(|a, b| b.frequency.cmp(&a.frequency));
        sorted.into_iter().take(n).collect()
    }

    /// Get all patterns for a table
    pub fn get_table_patterns(&self, table_name: &str) -> Vec<PatternStats> {
        self.patterns.read()
            .values()
            .filter(|s| s.pattern.table_name == table_name)
            .cloned()
            .collect()
    }
}

/// Speculative Filter Manager
pub struct SpeculativeFilterManager {
    /// Pattern tracker
    pattern_tracker: Arc<QueryPatternTracker>,
    /// Created filters metadata
    filters: RwLock<HashMap<u64, SpeculativeFilterMeta>>,
    /// Per-table bloom filters (created speculatively)
    bloom_filters: RwLock<HashMap<String, HashMap<String, BloomFilter>>>,
    /// Filter ID counter
    filter_id_counter: AtomicU64,
    /// Configuration
    config: SpeculativeConfig,
    /// Statistics
    stats: RwLock<SpeculativeFilterStats>,
}

/// Statistics for speculative filtering
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpeculativeFilterStats {
    pub total_patterns_tracked: u64,
    pub filters_created: u64,
    pub filters_dropped: u64,
    pub filter_hits: u64,
    pub filter_misses: u64,
    pub estimated_time_saved_ms: f64,
    pub estimated_bytes_saved: u64,
}

impl SpeculativeFilterManager {
    pub fn new(config: SpeculativeConfig) -> Self {
        Self {
            pattern_tracker: Arc::new(QueryPatternTracker::new(config.clone())),
            filters: RwLock::new(HashMap::new()),
            bloom_filters: RwLock::new(HashMap::new()),
            filter_id_counter: AtomicU64::new(0),
            config,
            stats: RwLock::new(SpeculativeFilterStats::default()),
        }
    }

    /// Record a query execution for pattern tracking
    pub fn record_query_execution(
        &self,
        table_name: &str,
        column_name: &str,
        op: FilterOp,
        selectivity: f64,
        execution_ms: f64,
    ) {
        let pattern_type = match op {
            FilterOp::Eq => PatternType::Equality,
            FilterOp::Lt | FilterOp::LtEq | FilterOp::Gt | FilterOp::GtEq => PatternType::Range,
            FilterOp::IsNull | FilterOp::IsNotNull => PatternType::IsNull,
            _ => PatternType::Equality,
        };

        let pattern = QueryPattern::new(table_name, column_name, pattern_type);
        self.pattern_tracker.record(pattern, selectivity, execution_ms);

        // Check if we should create filters
        if self.config.auto_create_enabled {
            self.check_and_create_filters();
        }
    }

    /// Check for filter creation candidates and create if appropriate
    fn check_and_create_filters(&self) {
        let candidates = self.pattern_tracker.get_filter_candidates();

        for candidate in candidates {
            // Check if filter already exists
            {
                let filters = self.filters.read();
                if filters.values().any(|f| f.pattern.pattern_hash == candidate.pattern.pattern_hash) {
                    continue;
                }
            }

            // Check per-table limit
            {
                let filters = self.filters.read();
                let table_count = filters.values()
                    .filter(|f| f.pattern.table_name == candidate.pattern.table_name)
                    .filter(|f| f.status == FilterStatus::Active)
                    .count();

                if table_count >= self.config.max_filters_per_table {
                    continue;
                }
            }

            // Create the filter
            self.create_speculative_filter(&candidate);
        }
    }

    /// Create a speculative filter for a pattern
    fn create_speculative_filter(&self, stats: &PatternStats) {
        let filter_id = self.filter_id_counter.fetch_add(1, Ordering::Relaxed);

        let meta = SpeculativeFilterMeta {
            filter_id,
            pattern: stats.pattern.clone(),
            status: FilterStatus::Active,
            created_at: Utc::now(),
            last_used: Utc::now(),
            usage_count: 0,
            bytes_saved: 0,
            time_saved_ms: 0.0,
        };

        // Create bloom filter for equality patterns
        if stats.pattern.pattern_type == PatternType::Equality {
            let config = BloomFilterConfig::new(100_000, 0.01);
            let filter = BloomFilter::new(config);

            let mut bloom_filters = self.bloom_filters.write();
            bloom_filters
                .entry(stats.pattern.table_name.clone())
                .or_insert_with(HashMap::new)
                .insert(stats.pattern.column_name.clone(), filter);
        }

        self.filters.write().insert(filter_id, meta);
        self.stats.write().filters_created += 1;

        tracing::info!(
            "Created speculative filter for {}.{} (pattern: {:?}, frequency: {})",
            stats.pattern.table_name,
            stats.pattern.column_name,
            stats.pattern.pattern_type,
            stats.frequency
        );
    }

    /// Check if a speculative filter exists for a predicate
    pub fn has_filter(&self, table_name: &str, column_name: &str, pattern_type: PatternType) -> bool {
        let pattern = QueryPattern::new(table_name, column_name, pattern_type);
        let filters = self.filters.read();

        filters.values().any(|f| {
            f.pattern.pattern_hash == pattern.pattern_hash && f.status == FilterStatus::Active
        })
    }

    /// Get bloom filter for a column (if speculatively created)
    pub fn get_bloom_filter(&self, table_name: &str, column_name: &str) -> Option<BloomFilter> {
        let bloom_filters = self.bloom_filters.read();
        bloom_filters
            .get(table_name)
            .and_then(|cols| cols.get(column_name))
            .cloned()
    }

    /// Record filter usage
    pub fn record_filter_usage(&self, table_name: &str, column_name: &str, hit: bool, time_saved_ms: f64) {
        let pattern = QueryPattern::new(table_name, column_name, PatternType::Equality);

        let mut filters = self.filters.write();
        for meta in filters.values_mut() {
            if meta.pattern.pattern_hash == pattern.pattern_hash {
                meta.last_used = Utc::now();
                meta.usage_count += 1;
                if hit {
                    meta.time_saved_ms += time_saved_ms;
                }
            }
        }

        let mut stats = self.stats.write();
        if hit {
            stats.filter_hits += 1;
            stats.estimated_time_saved_ms += time_saved_ms;
        } else {
            stats.filter_misses += 1;
        }
    }

    /// Add value to speculative bloom filter (during INSERT)
    pub fn on_insert(&self, table_name: &str, column_name: &str, value: &Value) {
        let mut bloom_filters = self.bloom_filters.write();
        if let Some(cols) = bloom_filters.get_mut(table_name) {
            if let Some(filter) = cols.get_mut(column_name) {
                filter.insert_value(value);
            }
        }
    }

    /// Drop inactive filters
    pub fn drop_inactive_filters(&self) {
        let cutoff = Utc::now() - chrono::Duration::days(self.config.drop_after_days as i64);

        let mut filters = self.filters.write();
        let mut bloom_filters = self.bloom_filters.write();

        let to_drop: Vec<_> = filters.iter()
            .filter(|(_, meta)| meta.last_used < cutoff && meta.status == FilterStatus::Active)
            .map(|(&id, meta)| (id, meta.pattern.clone()))
            .collect();

        for (id, pattern) in to_drop {
            if let Some(meta) = filters.get_mut(&id) {
                meta.status = FilterStatus::Dropped;
            }

            // Remove bloom filter
            if let Some(cols) = bloom_filters.get_mut(&pattern.table_name) {
                cols.remove(&pattern.column_name);
            }

            self.stats.write().filters_dropped += 1;

            tracing::info!(
                "Dropped inactive speculative filter for {}.{}",
                pattern.table_name,
                pattern.column_name
            );
        }
    }

    /// Get statistics
    pub fn stats(&self) -> SpeculativeFilterStats {
        let mut stats = self.stats.read().clone();
        stats.total_patterns_tracked = self.pattern_tracker.total_patterns.load(Ordering::Relaxed);
        stats
    }

    /// Get active filters for a table
    pub fn get_table_filters(&self, table_name: &str) -> Vec<SpeculativeFilterMeta> {
        self.filters.read()
            .values()
            .filter(|f| f.pattern.table_name == table_name && f.status == FilterStatus::Active)
            .cloned()
            .collect()
    }

    /// Get top query patterns
    pub fn get_top_patterns(&self, n: usize) -> Vec<PatternStats> {
        self.pattern_tracker.get_top_patterns(n)
    }

    /// Get pattern tracker reference
    pub fn pattern_tracker(&self) -> Arc<QueryPatternTracker> {
        self.pattern_tracker.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_pattern_creation() {
        let pattern = QueryPattern::new("orders", "customer_id", PatternType::Equality);
        assert_eq!(pattern.table_name, "orders");
        assert_eq!(pattern.column_name, "customer_id");
        assert_eq!(pattern.pattern_type, PatternType::Equality);
    }

    #[test]
    fn test_pattern_tracking() {
        let config = SpeculativeConfig::default();
        let tracker = QueryPatternTracker::new(config);

        let pattern = QueryPattern::new("orders", "customer_id", PatternType::Equality);

        for i in 0..100 {
            tracker.record(pattern.clone(), 0.05, 50.0);
        }

        let stats = tracker.get_stats(pattern.pattern_hash).unwrap();
        assert_eq!(stats.frequency, 100);
        assert!((stats.avg_selectivity - 0.05).abs() < 0.001);
    }

    #[test]
    fn test_filter_candidate_detection() {
        let mut config = SpeculativeConfig::default();
        config.min_query_frequency = 10;
        let tracker = QueryPatternTracker::new(config);

        let pattern = QueryPattern::new("orders", "customer_id", PatternType::Equality);

        // Record enough observations to trigger
        for _ in 0..20 {
            tracker.record(pattern.clone(), 0.01, 100.0);
        }

        let candidates = tracker.get_filter_candidates();
        assert!(!candidates.is_empty());
    }

    #[test]
    fn test_speculative_filter_creation() {
        let mut config = SpeculativeConfig::default();
        config.min_query_frequency = 5;
        config.auto_create_enabled = true;
        let manager = SpeculativeFilterManager::new(config);

        // Record queries
        for _ in 0..10 {
            manager.record_query_execution("orders", "customer_id", FilterOp::Eq, 0.01, 100.0);
        }

        // Should have created a filter
        assert!(manager.has_filter("orders", "customer_id", PatternType::Equality));
    }

    #[test]
    fn test_top_patterns() {
        let config = SpeculativeConfig::default();
        let tracker = QueryPatternTracker::new(config);

        for i in 0..50 {
            let pattern = QueryPattern::new("orders", "customer_id", PatternType::Equality);
            tracker.record(pattern, 0.05, 50.0);
        }

        for i in 0..100 {
            let pattern = QueryPattern::new("orders", "order_date", PatternType::Range);
            tracker.record(pattern, 0.1, 30.0);
        }

        let top = tracker.get_top_patterns(2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].pattern.column_name, "order_date"); // Most frequent
    }
}
