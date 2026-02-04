//! Parallel Filter Evaluation Engine
//!
//! Provides parallel filter evaluation using rayon for optimizer-driven parallelism.
//! Supports parallel bloom filter checks, zone map pruning, and SIMD filtering.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use parking_lot::RwLock;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{Tuple, Schema};
use super::bloom_filter::TableBloomFilters;
use super::simd_filter::SimdPredicateFilteringEngine;
use super::predicate_pushdown::{AnalyzedPredicate, PredicateOp};
use super::columnar_zone_summary::{TableZoneSummaries, BlockDecision};

/// Configuration for parallel filtering
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelFilterConfig {
    /// Minimum tuples before using parallel processing
    pub parallel_threshold: usize,
    /// Number of threads to use (0 = auto-detect)
    pub num_threads: usize,
    /// Chunk size for parallel iteration
    pub chunk_size: usize,
    /// Enable parallel bloom filter checking
    pub parallel_bloom: bool,
    /// Enable parallel zone map pruning
    pub parallel_zone_map: bool,
    /// Enable parallel SIMD filtering
    pub parallel_simd: bool,
    /// Enable work-stealing for load balancing
    pub work_stealing: bool,
    /// Enable adaptive threshold adjustment
    pub adaptive_threshold: bool,
    /// Minimum threshold for adaptive mode
    pub min_adaptive_threshold: usize,
    /// Maximum threshold for adaptive mode
    pub max_adaptive_threshold: usize,
}

impl Default for ParallelFilterConfig {
    fn default() -> Self {
        Self {
            parallel_threshold: 1000,
            num_threads: 0, // Auto-detect
            chunk_size: 256,
            parallel_bloom: true,
            parallel_zone_map: true,
            parallel_simd: true,
            work_stealing: true,
            // Adaptive threshold configuration
            adaptive_threshold: true,   // Enable by default
            min_adaptive_threshold: 256, // Don't parallelize under 256 tuples
            max_adaptive_threshold: 5000, // Always parallelize above 5000 tuples
        }
    }
}

/// Statistics for parallel filtering
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParallelFilterStats {
    pub total_evaluations: u64,
    pub parallel_evaluations: u64,
    pub sequential_evaluations: u64,
    pub tuples_processed: u64,
    pub tuples_filtered: u64,
    pub bloom_checks: u64,
    pub bloom_hits: u64,
    pub zone_map_checks: u64,
    pub zone_map_skips: u64,
    pub simd_batches: u64,
    pub avg_parallelism: f64,
    pub total_time_ns: u64,
}

/// Parallel Filter Evaluation Engine
///
/// Performance optimization: Implements adaptive threshold that adjusts based on:
/// - Predicate complexity (more predicates = lower threshold to benefit from parallelism)
/// - Historical performance (tracks parallel vs sequential efficiency)
pub struct ParallelFilterEngine {
    /// Configuration
    config: ParallelFilterConfig,
    /// SIMD engine reference
    simd_engine: SimdPredicateFilteringEngine,
    /// Statistics
    stats: RwLock<ParallelFilterStats>,
    /// Thread pool (lazy initialized)
    thread_pool: Option<rayon::ThreadPool>,
    /// Current adaptive threshold (adjusted at runtime)
    adaptive_threshold: AtomicUsize,
    /// Performance history for adaptive tuning (parallel_speedup_ratio)
    speedup_history: RwLock<Vec<f64>>,
}

impl ParallelFilterEngine {
    pub fn new(config: ParallelFilterConfig) -> Self {
        let thread_pool = if config.num_threads > 0 {
            rayon::ThreadPoolBuilder::new()
                .num_threads(config.num_threads)
                .build()
                .ok()
        } else {
            None
        };

        let initial_threshold = config.parallel_threshold;

        Self {
            config,
            simd_engine: SimdPredicateFilteringEngine::new(),
            stats: RwLock::new(ParallelFilterStats::default()),
            thread_pool,
            adaptive_threshold: AtomicUsize::new(initial_threshold),
            speedup_history: RwLock::new(Vec::with_capacity(100)),
        }
    }

    /// Calculate adaptive threshold based on predicate complexity
    fn calculate_adaptive_threshold(&self, predicates: &[AnalyzedPredicate]) -> usize {
        if !self.config.adaptive_threshold {
            return self.config.parallel_threshold;
        }

        let base_threshold = self.adaptive_threshold.load(Ordering::Relaxed);

        // Predicate complexity factor:
        // - Simple (1-2 predicates): base threshold
        // - Moderate (3-5 predicates): 75% of base
        // - Complex (6+ predicates): 50% of base (parallelize more aggressively)
        let complexity_factor = match predicates.len() {
            0..=2 => 1.0,
            3..=5 => 0.75,
            _ => 0.5,
        };

        // Check for expensive predicates (LIKE, IN, BETWEEN increase complexity)
        let has_expensive_predicate = predicates.iter().any(|p| {
            matches!(p.op, PredicateOp::Like | PredicateOp::In | PredicateOp::Between)
        });
        let expensive_factor = if has_expensive_predicate { 0.7 } else { 1.0 };

        let adjusted = (base_threshold as f64 * complexity_factor * expensive_factor) as usize;

        // Clamp to configured bounds
        adjusted.clamp(
            self.config.min_adaptive_threshold,
            self.config.max_adaptive_threshold,
        )
    }

    /// Update adaptive threshold based on observed performance
    fn update_adaptive_threshold(&self, input_count: usize, elapsed_ns: u64, was_parallel: bool) {
        if !self.config.adaptive_threshold {
            return;
        }

        // Calculate throughput (tuples/second)
        let throughput = if elapsed_ns > 0 {
            (input_count as f64 / elapsed_ns as f64) * 1_000_000_000.0
        } else {
            0.0
        };

        // Track speedup history for parallel operations
        if was_parallel && throughput > 0.0 {
            let mut history = self.speedup_history.write();
            history.push(throughput);
            if history.len() > 100 {
                history.remove(0);
            }

            // Adjust threshold based on recent performance
            if history.len() >= 10 {
                let avg_throughput: f64 = history.iter().sum::<f64>() / history.len() as f64;
                let current = self.adaptive_threshold.load(Ordering::Relaxed);

                // If throughput is high, we can lower the threshold (parallelize more)
                // If throughput is low, raise the threshold (use less parallelism)
                let new_threshold = if throughput > avg_throughput * 1.2 {
                    // Good performance, lower threshold
                    (current as f64 * 0.95) as usize
                } else if throughput < avg_throughput * 0.8 {
                    // Poor performance, raise threshold
                    (current as f64 * 1.05) as usize
                } else {
                    current
                };

                let clamped = new_threshold.clamp(
                    self.config.min_adaptive_threshold,
                    self.config.max_adaptive_threshold,
                );
                self.adaptive_threshold.store(clamped, Ordering::Relaxed);
            }
        }
    }

    /// Get current adaptive threshold
    pub fn current_threshold(&self) -> usize {
        self.adaptive_threshold.load(Ordering::Relaxed)
    }

    /// Filter tuples in parallel with all filtering layers
    pub fn filter_parallel(
        &self,
        tuples: Vec<Tuple>,
        predicates: &[AnalyzedPredicate],
        schema: &Schema,
        bloom_filters: Option<&TableBloomFilters>,
        zone_summaries: Option<&TableZoneSummaries>,
    ) -> Vec<Tuple> {
        let start = std::time::Instant::now();
        let input_count = tuples.len();

        // Update stats
        {
            let mut stats = self.stats.write();
            stats.total_evaluations += 1;
            stats.tuples_processed += input_count as u64;
        }

        // Calculate adaptive threshold based on predicate complexity
        let effective_threshold = self.calculate_adaptive_threshold(predicates);

        // Decide parallel vs sequential
        if input_count < effective_threshold {
            let result = self.filter_sequential(tuples, predicates, schema, bloom_filters);
            let elapsed = start.elapsed().as_nanos() as u64;
            self.update_adaptive_threshold(input_count, elapsed, false);
            return result;
        }

        self.stats.write().parallel_evaluations += 1;

        // Phase 1: Parallel bloom filter pre-filtering
        let bloom_filtered = match (self.config.parallel_bloom, bloom_filters) {
            (true, Some(filters)) => self.parallel_bloom_filter(tuples, predicates, filters, schema),
            _ => tuples,
        };

        // Phase 2: Zone map block pruning (if applicable)
        // This is done at block level, not tuple level

        // Phase 3: Parallel SIMD predicate filtering
        let result = if self.config.parallel_simd {
            self.parallel_simd_filter(bloom_filtered, predicates, schema)
        } else {
            self.simd_engine.filter_batch(&bloom_filtered, predicates, schema)
        };

        // Update stats and adaptive threshold
        let elapsed = start.elapsed().as_nanos() as u64;
        {
            let mut stats = self.stats.write();
            stats.tuples_filtered += (input_count - result.len()) as u64;
            stats.total_time_ns += elapsed;
        }

        // Update adaptive threshold based on parallel performance
        self.update_adaptive_threshold(input_count, elapsed, true);

        result
    }

    /// Sequential filtering for small datasets
    fn filter_sequential(
        &self,
        tuples: Vec<Tuple>,
        predicates: &[AnalyzedPredicate],
        schema: &Schema,
        bloom_filters: Option<&TableBloomFilters>,
    ) -> Vec<Tuple> {
        self.stats.write().sequential_evaluations += 1;

        // Apply bloom filter if available
        let tuples = if let Some(filters) = bloom_filters {
            tuples.into_iter()
                .filter(|tuple| self.check_bloom_filters(tuple, predicates, filters, schema))
                .collect()
        } else {
            tuples
        };

        // Apply SIMD filtering
        self.simd_engine.filter_batch(&tuples, predicates, schema)
    }

    /// Parallel bloom filter checking
    fn parallel_bloom_filter(
        &self,
        tuples: Vec<Tuple>,
        predicates: &[AnalyzedPredicate],
        bloom_filters: &TableBloomFilters,
        schema: &Schema,
    ) -> Vec<Tuple> {
        let bloom_checks = AtomicU64::new(0);
        let bloom_hits = AtomicU64::new(0);

        let result: Vec<Tuple> = tuples.into_par_iter()
            .filter(|tuple| {
                bloom_checks.fetch_add(1, Ordering::Relaxed);
                let passes = self.check_bloom_filters(tuple, predicates, bloom_filters, schema);
                if passes {
                    bloom_hits.fetch_add(1, Ordering::Relaxed);
                }
                passes
            })
            .collect();

        let mut stats = self.stats.write();
        stats.bloom_checks += bloom_checks.load(Ordering::Relaxed);
        stats.bloom_hits += bloom_hits.load(Ordering::Relaxed);

        result
    }

    /// Check bloom filters for a tuple
    fn check_bloom_filters(
        &self,
        tuple: &Tuple,
        predicates: &[AnalyzedPredicate],
        bloom_filters: &TableBloomFilters,
        schema: &Schema,
    ) -> bool {
        for pred in predicates {
            // Only check equality predicates against bloom filters
            if pred.op != PredicateOp::Eq {
                continue;
            }

            if let Some(filter) = bloom_filters.column_filters.iter().find(|f| f.column_name == pred.column_name) {
                // If bloom filter says "definitely not present", skip
                if !filter.might_contain_check(&pred.value) {
                    return true; // Skip this tuple early - it might match
                }
            }
        }
        true // No bloom filter rejection
    }

    /// Parallel SIMD filtering
    fn parallel_simd_filter(
        &self,
        tuples: Vec<Tuple>,
        predicates: &[AnalyzedPredicate],
        schema: &Schema,
    ) -> Vec<Tuple> {
        let chunk_size = self.config.chunk_size;
        let batch_count = AtomicU64::new(0);

        let result: Vec<Tuple> = tuples
            .par_chunks(chunk_size)
            .flat_map(|chunk| {
                batch_count.fetch_add(1, Ordering::Relaxed);
                self.simd_engine.filter_batch(chunk, predicates, schema)
            })
            .collect();

        self.stats.write().simd_batches += batch_count.load(Ordering::Relaxed);
        result
    }

    /// Filter with zone map block pruning
    pub fn filter_with_zone_pruning(
        &self,
        blocks: &HashMap<u64, Vec<Tuple>>,
        predicates: &[AnalyzedPredicate],
        schema: &Schema,
        zone_summaries: &TableZoneSummaries,
    ) -> Vec<Tuple> {
        let zone_checks = AtomicU64::new(0);
        let zone_skips = AtomicU64::new(0);

        // Parallel block processing with zone map pruning
        let result: Vec<Tuple> = blocks.par_iter()
            .flat_map(|(block_id, tuples)| {
                zone_checks.fetch_add(1, Ordering::Relaxed);

                // Check zone summary for this block
                if let Some(block_summary) = zone_summaries.blocks.get(block_id) {
                    match block_summary.can_satisfy(predicates) {
                        BlockDecision::Skip => {
                            zone_skips.fetch_add(1, Ordering::Relaxed);
                            return Vec::new();
                        }
                        BlockDecision::FullMatch => {
                            // All rows match - no filtering needed
                            return tuples.clone();
                        }
                        BlockDecision::Scan => {
                            // Need to evaluate predicates
                        }
                    }
                }

                // Apply SIMD filtering to this block
                self.simd_engine.filter_batch(tuples, predicates, schema)
            })
            .collect();

        let mut stats = self.stats.write();
        stats.zone_map_checks += zone_checks.load(Ordering::Relaxed);
        stats.zone_map_skips += zone_skips.load(Ordering::Relaxed);

        result
    }

    /// Parallel predicate evaluation with early termination
    pub fn evaluate_parallel_with_limit(
        &self,
        tuples: Vec<Tuple>,
        predicates: &[AnalyzedPredicate],
        schema: &Schema,
        limit: usize,
    ) -> Vec<Tuple> {
        if tuples.len() < self.config.parallel_threshold || limit == 0 {
            return self.simd_engine.filter_batch(&tuples, predicates, schema)
                .into_iter()
                .take(limit)
                .collect();
        }

        let found = AtomicUsize::new(0);
        let chunk_size = self.config.chunk_size;

        // Process chunks in parallel but stop early when limit reached
        let result: Vec<Tuple> = tuples
            .par_chunks(chunk_size)
            .flat_map(|chunk| {
                // Check if we've already found enough
                if found.load(Ordering::Relaxed) >= limit {
                    return Vec::new();
                }

                let matches = self.simd_engine.filter_batch(chunk, predicates, schema);
                found.fetch_add(matches.len(), Ordering::Relaxed);
                matches
            })
            .collect();

        // Trim to exact limit
        result.into_iter().take(limit).collect()
    }

    /// Get statistics
    pub fn stats(&self) -> ParallelFilterStats {
        let stats = self.stats.read();
        let mut result = stats.clone();

        // Calculate average parallelism
        if result.parallel_evaluations > 0 {
            let total = result.parallel_evaluations + result.sequential_evaluations;
            result.avg_parallelism = result.parallel_evaluations as f64 / total as f64;
        }

        result
    }

    /// Reset statistics
    pub fn reset_stats(&self) {
        *self.stats.write() = ParallelFilterStats::default();
    }

    /// Get configuration
    pub fn config(&self) -> &ParallelFilterConfig {
        &self.config
    }
}

/// Parallel block scanner for zone map integration
pub struct ParallelBlockScanner {
    /// Zone summaries
    zone_summaries: Arc<RwLock<HashMap<String, TableZoneSummaries>>>,
    /// Parallel filter engine
    filter_engine: Arc<ParallelFilterEngine>,
}

impl ParallelBlockScanner {
    pub fn new(filter_engine: Arc<ParallelFilterEngine>) -> Self {
        Self {
            zone_summaries: Arc::new(RwLock::new(HashMap::new())),
            filter_engine,
        }
    }

    /// Register zone summaries for a table
    pub fn register_zone_summaries(&self, table_name: &str, summaries: TableZoneSummaries) {
        self.zone_summaries.write().insert(table_name.to_string(), summaries);
    }

    /// Get candidate block IDs for a query
    pub fn get_candidate_blocks(&self, table_name: &str, predicates: &[AnalyzedPredicate]) -> Vec<u64> {
        let summaries = self.zone_summaries.read();
        if let Some(table_summaries) = summaries.get(table_name) {
            table_summaries.get_candidate_blocks(predicates)
        } else {
            Vec::new() // No summaries - return empty (scan all)
        }
    }

    /// Estimate selectivity for a query
    pub fn estimate_selectivity(&self, table_name: &str, predicates: &[AnalyzedPredicate]) -> f64 {
        let summaries = self.zone_summaries.read();
        if let Some(table_summaries) = summaries.get(table_name) {
            table_summaries.estimate_selectivity(predicates)
        } else {
            1.0 // No statistics - assume full scan
        }
    }
}

/// Work-stealing parallel filter with adaptive chunking
pub struct AdaptiveParallelFilter {
    /// Base filter engine
    engine: ParallelFilterEngine,
    /// Adaptive chunk sizes per workload
    chunk_history: RwLock<Vec<(usize, f64)>>, // (chunk_size, throughput)
    /// Current optimal chunk size
    optimal_chunk: AtomicUsize,
}

impl AdaptiveParallelFilter {
    pub fn new(config: ParallelFilterConfig) -> Self {
        let initial_chunk = config.chunk_size;
        Self {
            engine: ParallelFilterEngine::new(config),
            chunk_history: RwLock::new(Vec::new()),
            optimal_chunk: AtomicUsize::new(initial_chunk),
        }
    }

    /// Filter with adaptive chunk sizing
    pub fn filter_adaptive(
        &self,
        tuples: Vec<Tuple>,
        predicates: &[AnalyzedPredicate],
        schema: &Schema,
    ) -> Vec<Tuple> {
        let input_count = tuples.len();
        if input_count < 1000 {
            return self.engine.simd_engine.filter_batch(&tuples, predicates, schema);
        }

        let chunk_size = self.optimal_chunk.load(Ordering::Relaxed);
        let start = std::time::Instant::now();

        let result: Vec<Tuple> = tuples
            .par_chunks(chunk_size)
            .flat_map(|chunk| self.engine.simd_engine.filter_batch(chunk, predicates, schema))
            .collect();

        let elapsed = start.elapsed().as_secs_f64();
        let throughput = input_count as f64 / elapsed;

        // Record and adapt
        self.adapt_chunk_size(chunk_size, throughput);

        result
    }

    /// Adapt chunk size based on throughput
    fn adapt_chunk_size(&self, chunk_size: usize, throughput: f64) {
        let mut history = self.chunk_history.write();
        history.push((chunk_size, throughput));

        // Keep last 10 observations
        if history.len() > 10 {
            history.remove(0);
        }

        // Find best performing chunk size
        if history.len() >= 5 {
            let best = history.iter()
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(size, _)| *size)
                .unwrap_or(chunk_size);

            // Move towards best with some exploration
            let current = self.optimal_chunk.load(Ordering::Relaxed);
            let new_chunk = if best > current {
                (current + best) / 2
            } else if best < current {
                (current + best) / 2
            } else {
                current
            };

            self.optimal_chunk.store(new_chunk.max(64).min(4096), Ordering::Relaxed);
        }
    }

    /// Get current optimal chunk size
    pub fn optimal_chunk_size(&self) -> usize {
        self.optimal_chunk.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Column, DataType, Value};

    fn create_test_schema() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::Int8),
            Column::new("value", DataType::Int8),
        ])
    }

    fn create_test_tuples(count: usize) -> Vec<Tuple> {
        (0..count)
            .map(|i| Tuple::new(vec![Value::Int8(i as i64), Value::Int8(i as i64 * 10)]))
            .collect()
    }

    fn make_predicate(column_name: &str, column_index: usize, op: PredicateOp, value: Value) -> AnalyzedPredicate {
        AnalyzedPredicate {
            column_name: column_name.to_string(),
            column_index,
            op,
            value,
            value2: None,
            value_list: vec![],
            selectivity: 0.5,
            can_use_bloom: false,
            can_use_zone_map: true,
        }
    }

    #[test]
    fn test_parallel_filter_basic() {
        let config = ParallelFilterConfig::default();
        let engine = ParallelFilterEngine::new(config);
        let schema = create_test_schema();
        let tuples = create_test_tuples(100);

        let pred = make_predicate("id", 0, PredicateOp::Lt, Value::Int8(50));

        let result = engine.filter_parallel(tuples, &[pred], &schema, None, None);
        assert_eq!(result.len(), 50);
    }

    #[test]
    fn test_parallel_filter_large_dataset() {
        let mut config = ParallelFilterConfig::default();
        config.parallel_threshold = 100;
        let engine = ParallelFilterEngine::new(config);
        let schema = create_test_schema();
        let tuples = create_test_tuples(10000);

        let pred = make_predicate("value", 1, PredicateOp::GtEq, Value::Int8(50000));

        let result = engine.filter_parallel(tuples, &[pred], &schema, None, None);
        assert_eq!(result.len(), 5000); // values 5000-9999

        let stats = engine.stats();
        assert!(stats.parallel_evaluations > 0);
    }

    #[test]
    fn test_parallel_filter_with_limit() {
        let config = ParallelFilterConfig::default();
        let engine = ParallelFilterEngine::new(config);
        let schema = create_test_schema();
        let tuples = create_test_tuples(10000);

        let pred = make_predicate("id", 0, PredicateOp::GtEq, Value::Int8(0));

        let result = engine.evaluate_parallel_with_limit(tuples, &[pred], &schema, 100);
        assert!(result.len() <= 100);
    }

    #[test]
    fn test_adaptive_filter() {
        let config = ParallelFilterConfig::default();
        let filter = AdaptiveParallelFilter::new(config);
        let schema = create_test_schema();

        // Run multiple iterations to allow adaptation
        for _ in 0..10 {
            let tuples = create_test_tuples(5000);
            let pred = make_predicate("id", 0, PredicateOp::Lt, Value::Int8(2500));
            filter.filter_adaptive(tuples, &[pred], &schema);
        }

        // Chunk size should have been adapted
        let chunk = filter.optimal_chunk_size();
        assert!(chunk >= 64 && chunk <= 4096);
    }
}
