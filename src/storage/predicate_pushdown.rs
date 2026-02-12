//! Storage-Level Predicate Pushdown
//!
//! This module integrates bloom filters, zone maps, and SIMD filtering to enable
//! efficient predicate evaluation at the storage layer, minimizing data transfer
//! and tuple materialization.
//!
//! Architecture:
//! 1. Zone Maps - Skip entire blocks that can't match range predicates
//! 2. Bloom Filters - Skip blocks that definitely don't contain equality matches
//! 3. SIMD Filtering - Vectorized predicate evaluation for remaining data
//!
//! The pushdown optimizer analyzes predicates and determines the most efficient
//! filtering strategy based on predicate types and available indexes.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;

use crate::sql::logical_plan::{LogicalExpr, BinaryOperator};
use crate::storage::bloom_filter::TableBloomFilters;
use crate::storage::zone_map::{TableZoneMap, RangeOp, ZoneMapStats};
use crate::storage::simd_filter::{
    SimdPredicateFilteringEngine, FilterPredicate, FilterOp, CombinedPredicate,
};
use crate::{Schema, Tuple, Value};

/// Predicate pushdown capabilities
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushdownCapabilities {
    /// Support equality pushdown via bloom filters
    pub bloom_filter: bool,
    /// Support range pushdown via zone maps
    pub zone_map: bool,
    /// Support SIMD-accelerated filtering
    pub simd_filter: bool,
    /// Support early termination with LIMIT
    pub early_termination: bool,
    /// Support projection pushdown (column pruning)
    pub projection_pushdown: bool,
}

impl Default for PushdownCapabilities {
    fn default() -> Self {
        Self {
            bloom_filter: true,
            zone_map: true,
            simd_filter: true,
            early_termination: true,
            projection_pushdown: true,
        }
    }
}

/// Configuration for predicate pushdown
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushdownConfig {
    /// Enable/disable capabilities
    pub capabilities: PushdownCapabilities,
    /// Selectivity threshold for using bloom filters (0.0 - 1.0)
    /// Use bloom filter when estimated selectivity is below this threshold
    pub bloom_selectivity_threshold: f64,
    /// Minimum row count to use zone maps
    pub zone_map_min_rows: usize,
    /// Block size for zone maps
    pub zone_map_block_size: usize,
    /// Expected distinct values per column for bloom filters
    pub bloom_expected_distinct: usize,
    /// False positive rate for bloom filters
    pub bloom_fpr: f64,
}

impl Default for PushdownConfig {
    fn default() -> Self {
        Self {
            capabilities: PushdownCapabilities::default(),
            bloom_selectivity_threshold: 0.1, // Use bloom filter when < 10% selectivity expected
            zone_map_min_rows: 100,
            zone_map_block_size: 1000,
            bloom_expected_distinct: 1000,
            bloom_fpr: 0.01,
        }
    }
}

/// Statistics for pushdown operations
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PushdownStats {
    /// Total scans performed
    pub total_scans: u64,
    /// Scans that used pushdown
    pub pushdown_scans: u64,
    /// Rows skipped by bloom filters
    pub bloom_rows_skipped: u64,
    /// Blocks skipped by zone maps
    pub zone_blocks_skipped: u64,
    /// Rows filtered by SIMD engine
    pub simd_rows_filtered: u64,
    /// Total rows scanned without pushdown
    pub baseline_rows: u64,
    /// Rows returned after all filtering
    pub returned_rows: u64,
    /// Early terminations (LIMIT reached)
    pub early_terminations: u64,
    /// Time saved estimate (microseconds)
    pub time_saved_micros: u64,
}

impl PushdownStats {
    /// Calculate overall efficiency (rows skipped / total rows)
    pub fn efficiency(&self) -> f64 {
        if self.baseline_rows == 0 {
            0.0
        } else {
            let skipped = self.bloom_rows_skipped + (self.zone_blocks_skipped * 1000); // Estimate
            skipped as f64 / self.baseline_rows as f64
        }
    }

    /// Calculate pushdown usage rate
    pub fn pushdown_rate(&self) -> f64 {
        if self.total_scans == 0 {
            0.0
        } else {
            self.pushdown_scans as f64 / self.total_scans as f64
        }
    }
}

/// Analyzed predicate ready for pushdown
#[derive(Debug, Clone)]
pub struct AnalyzedPredicate {
    /// Column name
    pub column_name: String,
    /// Column index in schema
    pub column_index: usize,
    /// Operation type
    pub op: PredicateOp,
    /// Primary value
    pub value: Value,
    /// Secondary value (for BETWEEN)
    pub value2: Option<Value>,
    /// List of values (for IN)
    pub value_list: Vec<Value>,
    /// Estimated selectivity
    pub selectivity: f64,
    /// Can use bloom filter
    pub can_use_bloom: bool,
    /// Can use zone map
    pub can_use_zone_map: bool,
}

/// Predicate operation type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    IsNull,
    IsNotNull,
    Between,
    In,
    Like,
}

impl PredicateOp {
    /// Convert to zone map range operation
    pub fn to_range_op(self) -> Option<RangeOp> {
        match self {
            PredicateOp::Eq => Some(RangeOp::Eq),
            PredicateOp::NotEq => Some(RangeOp::NotEq),
            PredicateOp::Lt => Some(RangeOp::Lt),
            PredicateOp::LtEq => Some(RangeOp::LtEq),
            PredicateOp::Gt => Some(RangeOp::Gt),
            PredicateOp::GtEq => Some(RangeOp::GtEq),
            _ => None,
        }
    }

    /// Convert to SIMD filter operation
    pub fn to_filter_op(self) -> Option<FilterOp> {
        match self {
            PredicateOp::Eq => Some(FilterOp::Eq),
            PredicateOp::NotEq => Some(FilterOp::NotEq),
            PredicateOp::Lt => Some(FilterOp::Lt),
            PredicateOp::LtEq => Some(FilterOp::LtEq),
            PredicateOp::Gt => Some(FilterOp::Gt),
            PredicateOp::GtEq => Some(FilterOp::GtEq),
            PredicateOp::IsNull => Some(FilterOp::IsNull),
            PredicateOp::IsNotNull => Some(FilterOp::IsNotNull),
            PredicateOp::Between => Some(FilterOp::Between),
            PredicateOp::In => Some(FilterOp::In),
            PredicateOp::Like => Some(FilterOp::Like),
        }
    }
}

/// Storage-level predicate pushdown manager
pub struct PredicatePushdownManager {
    /// Configuration
    config: PushdownConfig,
    /// Per-table bloom filters
    bloom_filters: Arc<RwLock<HashMap<String, TableBloomFilters>>>,
    /// Per-table zone maps
    zone_maps: Arc<RwLock<HashMap<String, TableZoneMap>>>,
    /// SIMD filtering engine
    simd_engine: SimdPredicateFilteringEngine,
    /// Statistics
    stats: Arc<RwLock<PushdownStats>>,
}

impl PredicatePushdownManager {
    /// Create a new pushdown manager
    pub fn new(config: PushdownConfig) -> Self {
        Self {
            config,
            bloom_filters: Arc::new(RwLock::new(HashMap::new())),
            zone_maps: Arc::new(RwLock::new(HashMap::new())),
            simd_engine: SimdPredicateFilteringEngine::new(),
            stats: Arc::new(RwLock::new(PushdownStats::default())),
        }
    }

    /// Create with default configuration
    pub fn with_defaults() -> Self {
        Self::new(PushdownConfig::default())
    }

    /// Initialize indexes for a table
    pub fn initialize_table(
        &self,
        table_name: &str,
        columns: &[String],
        expected_rows: usize,
    ) {
        // Create bloom filters for columns
        let mut tbf = TableBloomFilters::new(table_name.to_string(), expected_rows);
        for col in columns {
            tbf.add_column(col.clone(), self.config.bloom_expected_distinct);
        }
        self.bloom_filters.write().insert(table_name.to_string(), tbf);

        // Create zone map
        let tzm = TableZoneMap::new(table_name.to_string(), self.config.zone_map_block_size);
        self.zone_maps.write().insert(table_name.to_string(), tzm);
    }

    /// Index a row (call when inserting/updating)
    pub fn index_row(&self, table_name: &str, row_id: u64, values: &[(String, Value)]) {
        // Update bloom filters
        if let Some(tbf) = self.bloom_filters.write().get_mut(table_name) {
            tbf.index_row(row_id, values);
        }

        // Update zone map
        if let Some(tzm) = self.zone_maps.write().get_mut(table_name) {
            tzm.add_row(row_id, values);
        }
    }

    /// Analyze a logical expression to extract pushable predicates
    pub fn analyze_predicate(
        &self,
        expr: &LogicalExpr,
        schema: &Schema,
    ) -> Vec<AnalyzedPredicate> {
        let mut predicates = Vec::new();
        self.extract_predicates(expr, schema, &mut predicates);
        predicates
    }

    fn extract_predicates(
        &self,
        expr: &LogicalExpr,
        schema: &Schema,
        predicates: &mut Vec<AnalyzedPredicate>,
    ) {
        match expr {
            LogicalExpr::BinaryExpr { left, op, right } => {
                match op {
                    BinaryOperator::And => {
                        // Recurse into AND branches
                        self.extract_predicates(left, schema, predicates);
                        self.extract_predicates(right, schema, predicates);
                    }
                    BinaryOperator::Or => {
                        // OR predicates are harder to push down
                        // For now, we don't extract them
                    }
                    BinaryOperator::Eq | BinaryOperator::NotEq |
                    BinaryOperator::Lt | BinaryOperator::LtEq |
                    BinaryOperator::Gt | BinaryOperator::GtEq => {
                        // Try to extract column = value predicates
                        if let Some(pred) = self.extract_comparison(left, right, op, schema) {
                            predicates.push(pred);
                        } else if let Some(pred) = self.extract_comparison(right, left, op, schema) {
                            predicates.push(pred);
                        }
                    }
                    BinaryOperator::Like => {
                        if let Some(pred) = self.extract_like(left, right, schema) {
                            predicates.push(pred);
                        }
                    }
                    _ => {}
                }
            }
            LogicalExpr::IsNull { expr, is_null } => {
                if let LogicalExpr::Column { name, .. } = expr.as_ref() {
                    if let Some(col_idx) = schema.get_column_index(name) {
                        predicates.push(AnalyzedPredicate {
                            column_name: name.clone(),
                            column_index: col_idx,
                            op: if *is_null { PredicateOp::IsNull } else { PredicateOp::IsNotNull },
                            value: Value::Null,
                            value2: None,
                            value_list: Vec::new(),
                            selectivity: 0.1, // Estimate
                            can_use_bloom: false,
                            can_use_zone_map: true,
                        });
                    }
                }
            }
            LogicalExpr::Between { expr, low, high, negated } => {
                if let LogicalExpr::Column { name, .. } = expr.as_ref() {
                    if let (LogicalExpr::Literal(low_val), LogicalExpr::Literal(high_val)) =
                        (low.as_ref(), high.as_ref())
                    {
                        if let Some(col_idx) = schema.get_column_index(name) {
                            predicates.push(AnalyzedPredicate {
                                column_name: name.clone(),
                                column_index: col_idx,
                                op: if *negated { PredicateOp::NotEq } else { PredicateOp::Between },
                                value: low_val.clone(),
                                value2: Some(high_val.clone()),
                                value_list: Vec::new(),
                                selectivity: 0.2, // Estimate
                                can_use_bloom: false,
                                can_use_zone_map: true,
                            });
                        }
                    }
                }
            }
            LogicalExpr::InList { expr, list, negated } => {
                if let LogicalExpr::Column { name, .. } = expr.as_ref() {
                    let values: Vec<Value> = list
                        .iter()
                        .filter_map(|e| {
                            if let LogicalExpr::Literal(v) = e {
                                Some(v.clone())
                            } else {
                                None
                            }
                        })
                        .collect();

                    if !values.is_empty() {
                        if let Some(col_idx) = schema.get_column_index(name) {
                            predicates.push(AnalyzedPredicate {
                                column_name: name.clone(),
                                column_index: col_idx,
                                op: if *negated { PredicateOp::NotEq } else { PredicateOp::In },
                                value: values.first().cloned().unwrap_or(Value::Null),
                                value2: None,
                                value_list: values,
                                selectivity: 0.1, // Estimate
                                can_use_bloom: !*negated,
                                can_use_zone_map: false,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn extract_comparison(
        &self,
        left: &LogicalExpr,
        right: &LogicalExpr,
        op: &BinaryOperator,
        schema: &Schema,
    ) -> Option<AnalyzedPredicate> {
        let (column_name, column_index) = match left {
            LogicalExpr::Column { name, .. } => {
                let idx = schema.get_column_index(name)?;
                (name.clone(), idx)
            }
            _ => return None,
        };

        let value = match right {
            LogicalExpr::Literal(v) => v.clone(),
            _ => return None,
        };

        let pred_op = match op {
            BinaryOperator::Eq => PredicateOp::Eq,
            BinaryOperator::NotEq => PredicateOp::NotEq,
            BinaryOperator::Lt => PredicateOp::Lt,
            BinaryOperator::LtEq => PredicateOp::LtEq,
            BinaryOperator::Gt => PredicateOp::Gt,
            BinaryOperator::GtEq => PredicateOp::GtEq,
            _ => return None,
        };

        Some(AnalyzedPredicate {
            column_name,
            column_index,
            op: pred_op,
            value,
            value2: None,
            value_list: Vec::new(),
            selectivity: match pred_op {
                PredicateOp::Eq => 0.01,  // Very selective
                PredicateOp::NotEq => 0.99,
                _ => 0.33, // Range predicates
            },
            can_use_bloom: pred_op == PredicateOp::Eq,
            can_use_zone_map: matches!(pred_op, PredicateOp::Eq | PredicateOp::Lt |
                PredicateOp::LtEq | PredicateOp::Gt | PredicateOp::GtEq),
        })
    }

    fn extract_like(
        &self,
        left: &LogicalExpr,
        right: &LogicalExpr,
        schema: &Schema,
    ) -> Option<AnalyzedPredicate> {
        let (column_name, column_index) = match left {
            LogicalExpr::Column { name, .. } => {
                let idx = schema.get_column_index(name)?;
                (name.clone(), idx)
            }
            _ => return None,
        };

        let pattern = match right {
            LogicalExpr::Literal(Value::String(s)) => s.clone(),
            _ => return None,
        };

        Some(AnalyzedPredicate {
            column_name,
            column_index,
            op: PredicateOp::Like,
            value: Value::String(pattern),
            value2: None,
            value_list: Vec::new(),
            selectivity: 0.1, // Estimate
            can_use_bloom: false,
            can_use_zone_map: false,
        })
    }

    /// Execute a filtered scan with pushdown
    pub fn scan_with_pushdown(
        &self,
        table_name: &str,
        tuples: Vec<Tuple>,
        predicates: &[AnalyzedPredicate],
        schema: &Schema,
        limit: Option<usize>,
    ) -> Vec<Tuple> {
        let mut stats = self.stats.write();
        stats.total_scans += 1;
        stats.baseline_rows += tuples.len() as u64;

        if predicates.is_empty() || !self.config.capabilities.simd_filter {
            // No pushdown - return all tuples (or apply limit)
            return match limit {
                Some(l) => tuples.into_iter().take(l).collect(),
                None => tuples,
            };
        }

        stats.pushdown_scans += 1;

        // Phase 1: Bloom filter pre-filtering (for equality predicates)
        let mut candidate_indices: Vec<usize> = (0..tuples.len()).collect();

        if self.config.capabilities.bloom_filter {
            if let Some(tbf) = self.bloom_filters.write().get_mut(table_name) {
                for pred in predicates.iter().filter(|p| p.can_use_bloom) {
                    if pred.op == PredicateOp::Eq {
                        // Check bloom filter - if definitely not present, skip
                        if !tbf.might_contain_value(&pred.column_name, &pred.value) {
                            stats.bloom_rows_skipped += tuples.len() as u64;
                            stats.returned_rows += 0;
                            return Vec::new();
                        }
                    } else if pred.op == PredicateOp::In {
                        // Check if any value in the IN list might exist
                        let might_exist = pred.value_list.iter()
                            .any(|v| tbf.might_contain_value(&pred.column_name, v));
                        if !might_exist {
                            stats.bloom_rows_skipped += tuples.len() as u64;
                            stats.returned_rows += 0;
                            return Vec::new();
                        }
                    }
                }
            }
        }

        // Phase 2: Zone map block skipping (for range predicates)
        if self.config.capabilities.zone_map && tuples.len() >= self.config.zone_map_min_rows {
            if let Some(tzm) = self.zone_maps.write().get_mut(table_name) {
                for pred in predicates.iter().filter(|p| p.can_use_zone_map) {
                    if let Some(range_op) = pred.op.to_range_op() {
                        let matching_blocks = if pred.op == PredicateOp::Between {
                            if let Some(high) = &pred.value2 {
                                tzm.get_matching_blocks_between(&pred.column_name, &pred.value, high)
                            } else {
                                continue;
                            }
                        } else {
                            tzm.get_matching_blocks_range(&pred.column_name, range_op, &pred.value)
                        };

                        // Filter candidate indices to only matching blocks
                        let block_size = self.config.zone_map_block_size;
                        let matching_set: std::collections::HashSet<u64> =
                            matching_blocks.into_iter().collect();

                        let before_count = candidate_indices.len();
                        candidate_indices.retain(|&idx| {
                            let block_id = (idx / block_size) as u64;
                            matching_set.contains(&block_id)
                        });
                        let skipped = before_count - candidate_indices.len();
                        stats.zone_blocks_skipped += (skipped / block_size) as u64;
                    }
                }
            }
        }

        // Phase 3: SIMD predicate filtering
        let filter_predicates: Vec<FilterPredicate> = predicates
            .iter()
            .filter_map(|p| {
                let filter_op = p.op.to_filter_op()?;
                Some(FilterPredicate {
                    column_index: p.column_index,
                    column_name: p.column_name.clone(),
                    op: filter_op,
                    value: p.value.clone(),
                    value2: p.value2.clone(),
                    value_list: p.value_list.clone(),
                    pattern: match &p.value {
                        Value::String(s) if p.op == PredicateOp::Like => Some(s.clone()),
                        _ => None,
                    },
                })
            })
            .collect();

        // Build candidate rows
        let candidate_rows: Vec<Vec<Value>> = candidate_indices
            .iter()
            .filter_map(|&idx| tuples.get(idx).map(|t| t.values.to_vec()))
            .collect();

        // Apply SIMD filtering
        let result = if let Some(lim) = limit {
            let combined = if filter_predicates.len() == 1 {
                // Length is 1, so next() will succeed; use map_or for safety
                filter_predicates.into_iter().next().map_or_else(
                    || CombinedPredicate::And(vec![]),
                    CombinedPredicate::Single
                )
            } else {
                CombinedPredicate::And(
                    filter_predicates.into_iter()
                        .map(CombinedPredicate::Single)
                        .collect()
                )
            };
            self.simd_engine.filter_rows_with_limit(&candidate_rows, &combined, lim)
        } else {
            self.simd_engine.filter_and_predicates(&candidate_rows, &filter_predicates)
        };

        stats.simd_rows_filtered += result.total_count as u64 - result.matched_count as u64;

        // Collect matching tuples
        let matching_tuples: Vec<Tuple> = result.matched_indices
            .iter()
            .filter_map(|&idx| {
                candidate_indices.get(idx).and_then(|&ci| tuples.get(ci)).cloned()
            })
            .collect();

        if result.matched_count < result.total_count && limit.is_some() {
            stats.early_terminations += 1;
        }

        stats.returned_rows += matching_tuples.len() as u64;
        matching_tuples
    }

    /// Get pushdown statistics
    pub fn get_stats(&self) -> PushdownStats {
        self.stats.read().clone()
    }

    /// Reset statistics
    pub fn reset_stats(&self) {
        *self.stats.write() = PushdownStats::default();
    }

    /// Get configuration
    pub fn config(&self) -> &PushdownConfig {
        &self.config
    }

    /// Update configuration
    pub fn set_config(&mut self, config: PushdownConfig) {
        self.config = config;
    }

    /// Get zone map statistics for a table
    pub fn get_zone_map_stats(&self, table_name: &str) -> Option<ZoneMapStats> {
        self.zone_maps.read().get(table_name).map(|zm| zm.stats().clone())
    }

    /// Get bloom filter memory usage
    pub fn bloom_filter_memory_usage(&self) -> usize {
        self.bloom_filters.read().values().map(|bf| bf.memory_usage()).sum()
    }

    /// Get zone map memory usage
    pub fn zone_map_memory_usage(&self) -> usize {
        self.zone_maps.read().values().map(|zm| zm.memory_usage()).sum()
    }

    /// Total memory usage
    pub fn total_memory_usage(&self) -> usize {
        self.bloom_filter_memory_usage() + self.zone_map_memory_usage()
    }

    /// Register bloom filters for a table
    pub fn register_bloom_filters(&self, table_name: String, filters: TableBloomFilters) {
        self.bloom_filters.write().insert(table_name, filters);
    }

    /// Register zone maps for a table
    pub fn register_zone_maps(&self, table_name: String, zone_map: TableZoneMap) {
        self.zone_maps.write().insert(table_name, zone_map);
    }
}

impl Default for PredicatePushdownManager {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// Result of pushdown analysis
#[derive(Debug, Clone)]
pub struct PushdownAnalysis {
    /// Predicates that can be pushed down
    pub pushable_predicates: Vec<AnalyzedPredicate>,
    /// Predicates that must remain at executor level
    pub remaining_predicates: Vec<LogicalExpr>,
    /// Estimated selectivity after pushdown
    pub estimated_selectivity: f64,
    /// Recommended limit pushdown
    pub limit_pushdown: Option<usize>,
}

/// Analyze a filter expression for pushdown opportunities
pub fn analyze_for_pushdown(
    expr: &LogicalExpr,
    schema: &Schema,
) -> PushdownAnalysis {
    let manager = PredicatePushdownManager::with_defaults();
    let predicates = manager.analyze_predicate(expr, schema);

    let estimated_selectivity = predicates
        .iter()
        .map(|p| p.selectivity)
        .product::<f64>()
        .max(0.001); // Minimum selectivity

    PushdownAnalysis {
        pushable_predicates: predicates,
        remaining_predicates: Vec::new(), // For complex expressions not yet supported
        estimated_selectivity,
        limit_pushdown: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Column, DataType};

    fn create_test_schema() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::Int8),
            Column::new("name", DataType::Text),
            Column::new("age", DataType::Int4),
            Column::new("status", DataType::Text),
        ])
    }

    #[test]
    fn test_predicate_analysis() {
        let schema = create_test_schema();
        let manager = PredicatePushdownManager::with_defaults();

        // Test simple equality
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "id".to_string()  }),
            op: BinaryOperator::Eq,
            right: Box::new(LogicalExpr::Literal(Value::Int8(42))),
        };

        let predicates = manager.analyze_predicate(&expr, &schema);
        assert_eq!(predicates.len(), 1);
        assert_eq!(predicates[0].column_name, "id");
        assert_eq!(predicates[0].op, PredicateOp::Eq);
        assert!(predicates[0].can_use_bloom);
    }

    #[test]
    fn test_and_predicates() {
        let schema = create_test_schema();
        let manager = PredicatePushdownManager::with_defaults();

        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column { table: None, name: "age".to_string()  }),
                op: BinaryOperator::GtEq,
                right: Box::new(LogicalExpr::Literal(Value::Int4(18))),
            }),
            op: BinaryOperator::And,
            right: Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column { table: None, name: "status".to_string()  }),
                op: BinaryOperator::Eq,
                right: Box::new(LogicalExpr::Literal(Value::String("active".to_string()))),
            }),
        };

        let predicates = manager.analyze_predicate(&expr, &schema);
        assert_eq!(predicates.len(), 2);
    }

    #[test]
    fn test_pushdown_manager_initialization() {
        let manager = PredicatePushdownManager::with_defaults();
        manager.initialize_table("users", &["id".to_string(), "name".to_string()], 1000);

        // Index some rows
        manager.index_row("users", 1, &[
            ("id".to_string(), Value::Int8(1)),
            ("name".to_string(), Value::String("Alice".to_string())),
        ]);
        manager.index_row("users", 2, &[
            ("id".to_string(), Value::Int8(2)),
            ("name".to_string(), Value::String("Bob".to_string())),
        ]);

        assert!(manager.bloom_filter_memory_usage() > 0);
    }

    #[test]
    fn test_scan_with_pushdown() {
        let schema = Arc::new(create_test_schema());
        let manager = PredicatePushdownManager::with_defaults();
        manager.initialize_table("test", &["id".to_string(), "status".to_string()], 100);

        // Create test tuples
        let tuples: Vec<Tuple> = (0..10)
            .map(|i| {
                Tuple::new(vec![
                    Value::Int8(i),
                    Value::String(format!("name_{}", i)),
                    Value::Int4((i * 5 + 20) as i32),
                    Value::String(if i % 2 == 0 { "active" } else { "inactive" }.to_string()),
                ])
            })
            .collect();

        // Index tuples
        for (idx, t) in tuples.iter().enumerate() {
            manager.index_row("test", idx as u64, &[
                ("id".to_string(), t.values[0].clone()),
                ("status".to_string(), t.values[3].clone()),
            ]);
        }

        // Create filter predicate
        let pred = AnalyzedPredicate {
            column_name: "status".to_string(),
            column_index: 3,
            op: PredicateOp::Eq,
            value: Value::String("active".to_string()),
            value2: None,
            value_list: Vec::new(),
            selectivity: 0.5,
            can_use_bloom: true,
            can_use_zone_map: true,
        };

        let result = manager.scan_with_pushdown("test", tuples, &[pred], &schema, None);
        assert_eq!(result.len(), 5); // 0, 2, 4, 6, 8 are active
    }
}
