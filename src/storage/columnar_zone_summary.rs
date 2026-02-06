//! Columnar Zone Summaries (CZS) for Enhanced Per-Block Statistics
//!
//! Provides rich per-block column statistics including:
//! - Min/max/null counts
//! - HyperLogLog for approximate distinct counts
//! - Running averages and sums
//! - Most Common Values (MCV) tracking
//! - Histogram buckets for selectivity estimation

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use serde::{Deserialize, Serialize};
use crate::Value;
use super::predicate_pushdown::{AnalyzedPredicate, PredicateOp};

/// HyperLogLog implementation for approximate distinct counting
/// Uses the HyperLogLog++ algorithm with bias correction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyperLogLog {
    /// Number of registers (2^precision)
    precision: u8,
    /// Register array storing maximum leading zeros
    registers: Vec<u8>,
    /// Count of items added
    count: u64,
}

// SAFETY: register_idx is derived from hash bits modulo register count,
// and merge loop indices are bounded by self.registers.len().
#[allow(clippy::indexing_slicing)]
impl HyperLogLog {
    /// Create a new HyperLogLog with given precision (4-18)
    /// Higher precision = more accuracy but more memory
    /// Precision 12 uses 4KB memory with ~1.625% error
    pub fn new(precision: u8) -> Self {
        let precision = precision.clamp(4, 18);
        let num_registers = 1 << precision;
        Self {
            precision,
            registers: vec![0; num_registers],
            count: 0,
        }
    }

    /// Add a value to the HyperLogLog
    pub fn add(&mut self, value: &Value) {
        let hash = self.hash_value(value);
        let register_idx = (hash >> (64 - self.precision)) as usize;
        let remaining_bits = hash << self.precision;
        let leading_zeros = remaining_bits.leading_zeros() as u8 + 1;

        if leading_zeros > self.registers[register_idx] {
            self.registers[register_idx] = leading_zeros;
        }
        self.count += 1;
    }

    /// Estimate the cardinality (distinct count)
    pub fn estimate(&self) -> u64 {
        let m = self.registers.len() as f64;
        let alpha = self.alpha_m();

        // Calculate raw estimate
        let sum: f64 = self.registers.iter()
            .map(|&r| 2.0_f64.powi(-(r as i32)))
            .sum();

        let raw_estimate = alpha * m * m / sum;

        // Apply corrections
        if raw_estimate <= 2.5 * m {
            // Small range correction
            let zeros = self.registers.iter().filter(|&&r| r == 0).count() as f64;
            if zeros > 0.0 {
                (m * (m / zeros).ln()) as u64
            } else {
                raw_estimate as u64
            }
        } else if raw_estimate <= (1u64 << 32) as f64 / 30.0 {
            // No correction needed
            raw_estimate as u64
        } else {
            // Large range correction
            let neg_two_pow_32 = -((1u64 << 32) as f64);
            (neg_two_pow_32 * (1.0 - raw_estimate / ((1u64 << 32) as f64)).ln()) as u64
        }
    }

    /// Merge another HyperLogLog into this one
    pub fn merge(&mut self, other: &HyperLogLog) {
        if self.precision != other.precision {
            return; // Can't merge different precisions
        }
        for i in 0..self.registers.len() {
            if other.registers[i] > self.registers[i] {
                self.registers[i] = other.registers[i];
            }
        }
        self.count += other.count;
    }

    /// Get alpha_m constant based on number of registers
    fn alpha_m(&self) -> f64 {
        let m = self.registers.len() as f64;
        match self.registers.len() {
            16 => 0.673,
            32 => 0.697,
            64 => 0.709,
            _ => 0.7213 / (1.0 + 1.079 / m),
        }
    }

    /// Hash a value for HyperLogLog
    fn hash_value(&self, value: &Value) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();

        match value {
            Value::Int2(i) => i.hash(&mut hasher),
            Value::Int4(i) => i.hash(&mut hasher),
            Value::Int8(i) => i.hash(&mut hasher),
            Value::Float4(f) => f.to_bits().hash(&mut hasher),
            Value::Float8(f) => f.to_bits().hash(&mut hasher),
            Value::String(s) => s.hash(&mut hasher),
            Value::Boolean(b) => b.hash(&mut hasher),
            Value::Bytes(b) => b.hash(&mut hasher),
            Value::Null => 0u64.hash(&mut hasher),
            Value::Timestamp(t) => t.hash(&mut hasher),
            Value::Date(d) => d.hash(&mut hasher),
            Value::Time(t) => t.hash(&mut hasher),
            Value::Numeric(d) => d.hash(&mut hasher),
            Value::Uuid(u) => u.hash(&mut hasher),
            Value::Json(j) => j.hash(&mut hasher),
            Value::Array(arr) => {
                for v in arr {
                    format!("{:?}", v).hash(&mut hasher);
                }
            }
            Value::Vector(v) => {
                for f in v {
                    f.to_bits().hash(&mut hasher);
                }
            }
            // Storage references
            Value::DictRef { dict_id } => dict_id.hash(&mut hasher),
            Value::CasRef { hash } => hash.hash(&mut hasher),
            Value::ColumnarRef => 0u64.hash(&mut hasher),
            Value::Interval(iv) => iv.hash(&mut hasher), // Hash interval microseconds
        }

        hasher.finish()
    }
}

impl Default for HyperLogLog {
    fn default() -> Self {
        Self::new(12) // ~1.625% error with 4KB memory
    }
}

/// Most Common Value entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McvEntry {
    pub value: Value,
    pub frequency: u64,
}

/// Histogram bucket for selectivity estimation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistogramBucket {
    pub lower_bound: Value,
    pub upper_bound: Value,
    pub count: u64,
    pub distinct_count: u64,
}

/// Histogram for a column
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Histogram {
    pub buckets: Vec<HistogramBucket>,
    pub num_buckets: usize,
    pub total_count: u64,
}

impl Histogram {
    pub fn new(num_buckets: usize) -> Self {
        Self {
            buckets: Vec::with_capacity(num_buckets),
            num_buckets,
            total_count: 0,
        }
    }

    /// Estimate selectivity for a range predicate
    pub fn estimate_selectivity(&self, lower: Option<&Value>, upper: Option<&Value>) -> f64 {
        if self.total_count == 0 {
            return 1.0;
        }

        let mut matching_count = 0u64;

        for bucket in &self.buckets {
            let in_range = match (lower, upper) {
                (Some(l), Some(u)) => {
                    Self::value_le(&bucket.upper_bound, u) && Self::value_ge(&bucket.lower_bound, l)
                }
                (Some(l), None) => Self::value_ge(&bucket.upper_bound, l),
                (None, Some(u)) => Self::value_le(&bucket.lower_bound, u),
                (None, None) => true,
            };

            if in_range {
                matching_count += bucket.count;
            }
        }

        matching_count as f64 / self.total_count as f64
    }

    fn value_le(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => a <= b,
            (Value::Float8(a), Value::Float8(b)) => a <= b,
            (Value::String(a), Value::String(b)) => a <= b,
            _ => false,
        }
    }

    fn value_ge(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => a >= b,
            (Value::Float8(a), Value::Float8(b)) => a >= b,
            (Value::String(a), Value::String(b)) => a >= b,
            _ => false,
        }
    }
}

/// Enhanced per-column statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnZoneSummary {
    /// Column name
    pub column_name: String,
    /// Minimum value
    pub min: Option<Value>,
    /// Maximum value
    pub max: Option<Value>,
    /// Null count
    pub null_count: u64,
    /// Row count
    pub row_count: u64,
    /// Approximate distinct count (via HyperLogLog)
    pub hll: HyperLogLog,
    /// Average (for numeric columns)
    pub avg: Option<f64>,
    /// Sum (for numeric columns)
    pub sum: Option<f64>,
    /// Most common values (top 10)
    pub mcv: Vec<McvEntry>,
    /// Histogram for selectivity estimation
    pub histogram: Option<Histogram>,
    /// MCV frequency map for incremental updates
    #[serde(skip)]
    mcv_frequency: HashMap<String, u64>,
}

impl ColumnZoneSummary {
    pub fn new(column_name: &str) -> Self {
        Self {
            column_name: column_name.to_string(),
            min: None,
            max: None,
            null_count: 0,
            row_count: 0,
            hll: HyperLogLog::default(),
            avg: None,
            sum: None,
            mcv: Vec::new(),
            histogram: None,
            mcv_frequency: HashMap::new(),
        }
    }

    /// Incremental update on INSERT (O(1) per column)
    pub fn update_incremental(&mut self, value: &Value) {
        self.row_count += 1;

        if matches!(value, Value::Null) {
            self.null_count += 1;
            return;
        }

        // Update min/max
        self.update_min_max(value);

        // Update HyperLogLog for distinct count
        self.hll.add(value);

        // Update sum/avg for numeric types
        self.update_numeric_stats(value);

        // Update MCV tracking
        self.update_mcv(value);
    }

    fn update_min_max(&mut self, value: &Value) {
        // Update minimum
        match (&self.min, value) {
            (None, v) => self.min = Some(v.clone()),
            (Some(current_min), v) => {
                if Self::value_lt(v, current_min) {
                    self.min = Some(v.clone());
                }
            }
        }

        // Update maximum
        match (&self.max, value) {
            (None, v) => self.max = Some(v.clone()),
            (Some(current_max), v) => {
                if Self::value_gt(v, current_max) {
                    self.max = Some(v.clone());
                }
            }
        }
    }

    fn update_numeric_stats(&mut self, value: &Value) {
        let numeric_value = match value {
            Value::Int8(i) => Some(*i as f64),
            Value::Float8(f) => Some(*f),
            Value::Numeric(d) => d.parse::<f64>().ok(),
            _ => None,
        };

        if let Some(v) = numeric_value {
            let current_sum = self.sum.unwrap_or(0.0);
            let new_sum = current_sum + v;
            self.sum = Some(new_sum);

            // Running average
            let non_null_count = self.row_count - self.null_count;
            if non_null_count > 0 {
                self.avg = Some(new_sum / non_null_count as f64);
            }
        }
    }

    fn update_mcv(&mut self, value: &Value) {
        let key = format!("{:?}", value);
        *self.mcv_frequency.entry(key).or_insert(0) += 1;

        // Rebuild MCV list periodically (every 1000 rows)
        if self.row_count % 1000 == 0 {
            self.rebuild_mcv();
        }
    }

    fn rebuild_mcv(&mut self) {
        let mut entries: Vec<_> = self.mcv_frequency.iter()
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1));

        self.mcv = entries.into_iter()
            .take(10)
            .filter_map(|(key, freq)| {
                // Try to parse back to Value (simplified)
                if let Ok(i) = key.trim_start_matches("Int8(").trim_end_matches(")").parse::<i64>() {
                    Some(McvEntry {
                        value: Value::Int8(i),
                        frequency: freq,
                    })
                } else if key.starts_with("String(") {
                    let s = key.trim_start_matches("String(\"").trim_end_matches("\")");
                    Some(McvEntry {
                        value: Value::String(s.to_string()),
                        frequency: freq,
                    })
                } else {
                    None
                }
            })
            .collect();
    }

    fn value_lt(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => a < b,
            (Value::Float8(a), Value::Float8(b)) => a < b,
            (Value::String(a), Value::String(b)) => a < b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a < b,
            _ => false,
        }
    }

    fn value_gt(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => a > b,
            (Value::Float8(a), Value::Float8(b)) => a > b,
            (Value::String(a), Value::String(b)) => a > b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a > b,
            _ => false,
        }
    }

    fn value_le(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => a <= b,
            (Value::Float8(a), Value::Float8(b)) => a <= b,
            (Value::String(a), Value::String(b)) => a <= b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a <= b,
            _ => false,
        }
    }

    fn value_ge(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => a >= b,
            (Value::Float8(a), Value::Float8(b)) => a >= b,
            (Value::String(a), Value::String(b)) => a >= b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a >= b,
            _ => false,
        }
    }

    /// Get approximate distinct count
    pub fn approx_distinct(&self) -> u64 {
        self.hll.estimate()
    }

    /// Get selectivity estimate for an equality predicate
    pub fn estimate_equality_selectivity(&self, value: &Value) -> f64 {
        if self.row_count == 0 {
            return 1.0;
        }

        // Check MCV first
        for mcv_entry in &self.mcv {
            if Self::values_equal(&mcv_entry.value, value) {
                return mcv_entry.frequency as f64 / self.row_count as f64;
            }
        }

        // Fall back to uniform distribution assumption
        let distinct = self.approx_distinct().max(1);
        1.0 / distinct as f64
    }

    /// Get selectivity estimate for a range predicate
    pub fn estimate_range_selectivity(&self, lower: Option<&Value>, upper: Option<&Value>) -> f64 {
        if self.row_count == 0 {
            return 1.0;
        }

        // Use histogram if available
        if let Some(ref hist) = self.histogram {
            return hist.estimate_selectivity(lower, upper);
        }

        // Fall back to min/max range estimation
        match (&self.min, &self.max, lower, upper) {
            (Some(min), Some(max), Some(l), Some(u)) => {
                let total_range = Self::value_diff(max, min).unwrap_or(1.0);
                let query_range = Self::value_diff(u, l).unwrap_or(0.0);
                (query_range / total_range).clamp(0.0, 1.0)
            }
            _ => 0.5, // Conservative estimate
        }
    }

    fn values_equal(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => a == b,
            (Value::Float8(a), Value::Float8(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Boolean(a), Value::Boolean(b)) => a == b,
            _ => false,
        }
    }

    fn value_diff(a: &Value, b: &Value) -> Option<f64> {
        match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => Some((*a - *b) as f64),
            (Value::Float8(a), Value::Float8(b)) => Some(*a - *b),
            _ => None,
        }
    }
}

/// Block decision based on zone summary analysis
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockDecision {
    /// Skip the entire block
    Skip,
    /// Scan the block
    Scan,
    /// Block contains all matching rows (optimization hint)
    FullMatch,
}

/// Summary match result
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryMatch {
    /// Impossible - no rows can match
    Impossible,
    /// Partial - some rows might match
    Partial,
    /// Full - all rows match
    Full,
}

/// Block-level columnar zone summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockZoneSummary {
    pub block_id: u64,
    pub first_row_id: u64,
    pub last_row_id: u64,
    pub row_count: u64,
    pub columns: HashMap<String, ColumnZoneSummary>,
}

impl BlockZoneSummary {
    pub fn new(block_id: u64, first_row_id: u64) -> Self {
        Self {
            block_id,
            first_row_id,
            last_row_id: first_row_id,
            row_count: 0,
            columns: HashMap::new(),
        }
    }

    /// Incremental update on INSERT
    pub fn update_on_insert(&mut self, row_id: u64, values: &[(&str, &Value)]) {
        self.row_count += 1;
        self.last_row_id = row_id;

        for (col_name, value) in values {
            let summary = self.columns.entry(col_name.to_string())
                .or_insert_with(|| ColumnZoneSummary::new(col_name));
            summary.update_incremental(value);
        }
    }

    /// Can this block satisfy the predicates?
    pub fn can_satisfy(&self, predicates: &[AnalyzedPredicate]) -> BlockDecision {
        let mut all_full_match = true;

        for pred in predicates {
            if let Some(summary) = self.columns.get(&pred.column_name) {
                match self.evaluate_predicate_against_summary(pred, summary) {
                    SummaryMatch::Impossible => return BlockDecision::Skip,
                    SummaryMatch::Partial => all_full_match = false,
                    SummaryMatch::Full => {}
                }
            } else {
                // No summary for this column - must scan
                all_full_match = false;
            }
        }

        if all_full_match {
            BlockDecision::FullMatch
        } else {
            BlockDecision::Scan
        }
    }

    fn evaluate_predicate_against_summary(
        &self,
        pred: &AnalyzedPredicate,
        summary: &ColumnZoneSummary,
    ) -> SummaryMatch {
        use super::predicate_pushdown::PredicateOp;

        match pred.op {
            PredicateOp::Eq => {
                // Check if value is within range
                if let (Some(min), Some(max)) = (&summary.min, &summary.max) {
                    if ColumnZoneSummary::value_lt(&pred.value, min)
                        || ColumnZoneSummary::value_gt(&pred.value, max) {
                        return SummaryMatch::Impossible;
                    }
                }
                SummaryMatch::Partial
            }
            PredicateOp::Lt => {
                if let Some(min) = &summary.min {
                    if ColumnZoneSummary::value_le(&pred.value, min) {
                        return SummaryMatch::Impossible;
                    }
                }
                if let Some(max) = &summary.max {
                    if ColumnZoneSummary::value_gt(&pred.value, max) {
                        return SummaryMatch::Full;
                    }
                }
                SummaryMatch::Partial
            }
            PredicateOp::LtEq => {
                if let Some(min) = &summary.min {
                    if ColumnZoneSummary::value_lt(&pred.value, min) {
                        return SummaryMatch::Impossible;
                    }
                }
                if let Some(max) = &summary.max {
                    if ColumnZoneSummary::value_ge(&pred.value, max) {
                        return SummaryMatch::Full;
                    }
                }
                SummaryMatch::Partial
            }
            PredicateOp::Gt => {
                if let Some(max) = &summary.max {
                    if ColumnZoneSummary::value_ge(&pred.value, max) {
                        return SummaryMatch::Impossible;
                    }
                }
                if let Some(min) = &summary.min {
                    if ColumnZoneSummary::value_lt(&pred.value, min) {
                        return SummaryMatch::Full;
                    }
                }
                SummaryMatch::Partial
            }
            PredicateOp::GtEq => {
                if let Some(max) = &summary.max {
                    if ColumnZoneSummary::value_gt(&pred.value, max) {
                        return SummaryMatch::Impossible;
                    }
                }
                if let Some(min) = &summary.min {
                    if ColumnZoneSummary::value_le(&pred.value, min) {
                        return SummaryMatch::Full;
                    }
                }
                SummaryMatch::Partial
            }
            PredicateOp::NotEq => {
                // Can only skip if all values are equal to the excluded value
                SummaryMatch::Partial
            }
            PredicateOp::IsNull => {
                if summary.null_count == 0 {
                    SummaryMatch::Impossible
                } else if summary.null_count == summary.row_count {
                    SummaryMatch::Full
                } else {
                    SummaryMatch::Partial
                }
            }
            PredicateOp::IsNotNull => {
                if summary.null_count == summary.row_count {
                    SummaryMatch::Impossible
                } else if summary.null_count == 0 {
                    SummaryMatch::Full
                } else {
                    SummaryMatch::Partial
                }
            }
            _ => SummaryMatch::Partial,
        }
    }

    /// Merge with another block summary (for consolidation)
    pub fn merge(&mut self, other: &BlockZoneSummary) {
        self.row_count += other.row_count;
        self.last_row_id = self.last_row_id.max(other.last_row_id);

        for (col_name, other_summary) in &other.columns {
            let summary = self.columns.entry(col_name.clone())
                .or_insert_with(|| ColumnZoneSummary::new(col_name));

            // Merge min/max
            if let Some(other_min) = &other_summary.min {
                let should_update = match &summary.min {
                    None => true,
                    Some(current_min) => ColumnZoneSummary::value_lt(other_min, current_min),
                };
                if should_update {
                    summary.min = Some(other_min.clone());
                }
            }
            if let Some(other_max) = &other_summary.max {
                let should_update = match &summary.max {
                    None => true,
                    Some(current_max) => ColumnZoneSummary::value_gt(other_max, current_max),
                };
                if should_update {
                    summary.max = Some(other_max.clone());
                }
            }

            // Merge counts
            summary.null_count += other_summary.null_count;
            summary.row_count += other_summary.row_count;

            // Merge HLL
            summary.hll.merge(&other_summary.hll);

            // Merge numeric stats
            if let (Some(s1), Some(s2)) = (summary.sum, other_summary.sum) {
                let merged_sum = s1 + s2;
                summary.sum = Some(merged_sum);
                let total_non_null = summary.row_count - summary.null_count;
                if total_non_null > 0 {
                    summary.avg = Some(merged_sum / total_non_null as f64);
                }
            }
        }
    }
}

/// Table-level zone summary manager
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableZoneSummaries {
    pub table_name: String,
    pub block_size: u64,
    pub blocks: HashMap<u64, BlockZoneSummary>,
    pub total_rows: u64,
}

impl TableZoneSummaries {
    pub fn new(table_name: &str, block_size: u64) -> Self {
        Self {
            table_name: table_name.to_string(),
            block_size,
            blocks: HashMap::new(),
            total_rows: 0,
        }
    }

    /// Get or create block summary for a row
    pub fn get_block_for_row(&mut self, row_id: u64) -> &mut BlockZoneSummary {
        let block_id = row_id / self.block_size;
        let first_row_id = block_id * self.block_size;

        self.blocks.entry(block_id)
            .or_insert_with(|| BlockZoneSummary::new(block_id, first_row_id))
    }

    /// Update on insert
    pub fn on_insert(&mut self, row_id: u64, values: &[(&str, &Value)]) {
        let block = self.get_block_for_row(row_id);
        block.update_on_insert(row_id, values);
        self.total_rows += 1;
    }

    /// Get blocks that might satisfy predicates
    pub fn get_candidate_blocks(&self, predicates: &[AnalyzedPredicate]) -> Vec<u64> {
        self.blocks.iter()
            .filter(|(_, block)| block.can_satisfy(predicates) != BlockDecision::Skip)
            .map(|(&id, _)| id)
            .collect()
    }

    /// Estimate selectivity across all blocks
    pub fn estimate_selectivity(&self, predicates: &[AnalyzedPredicate]) -> f64 {
        if self.total_rows == 0 {
            return 1.0;
        }

        let mut matching_rows = 0u64;
        for block in self.blocks.values() {
            match block.can_satisfy(predicates) {
                BlockDecision::Skip => {}
                BlockDecision::FullMatch => matching_rows += block.row_count,
                BlockDecision::Scan => {
                    // Estimate partial match using column summaries
                    let mut block_selectivity = 1.0f64;
                    for pred in predicates {
                        if let Some(summary) = block.columns.get(&pred.column_name) {
                            let pred_selectivity = match pred.op {
                                PredicateOp::Eq => {
                                    summary.estimate_equality_selectivity(&pred.value)
                                }
                                _ => {
                                    summary.estimate_range_selectivity(
                                        Some(&pred.value),
                                        None,
                                    )
                                }
                            };
                            block_selectivity *= pred_selectivity;
                        }
                    }
                    matching_rows += (block.row_count as f64 * block_selectivity) as u64;
                }
            }
        }

        matching_rows as f64 / self.total_rows as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::predicate_pushdown::PredicateOp;

    #[test]
    fn test_hyperloglog_basic() {
        let mut hll = HyperLogLog::new(12);

        for i in 0..10000i64 {
            hll.add(&Value::Int8(i));
        }

        let estimate = hll.estimate();
        // Should be within ~5% of actual
        assert!(estimate > 9500 && estimate < 10500, "Estimate: {}", estimate);
    }

    #[test]
    fn test_hyperloglog_duplicates() {
        let mut hll = HyperLogLog::new(12);

        // Add many duplicates
        for _ in 0..10000 {
            hll.add(&Value::Int8(42));
        }

        let estimate = hll.estimate();
        assert_eq!(estimate, 1);
    }

    #[test]
    fn test_hyperloglog_merge() {
        let mut hll1 = HyperLogLog::new(12);
        let mut hll2 = HyperLogLog::new(12);

        for i in 0..5000i64 {
            hll1.add(&Value::Int8(i));
        }
        for i in 5000..10000i64 {
            hll2.add(&Value::Int8(i));
        }

        hll1.merge(&hll2);
        let estimate = hll1.estimate();
        assert!(estimate > 9500 && estimate < 10500, "Estimate: {}", estimate);
    }

    #[test]
    fn test_column_zone_summary() {
        let mut summary = ColumnZoneSummary::new("test_col");

        for i in 0..100i64 {
            summary.update_incremental(&Value::Int8(i));
        }

        assert_eq!(summary.row_count, 100);
        assert_eq!(summary.null_count, 0);
        assert_eq!(summary.min, Some(Value::Int8(0)));
        assert_eq!(summary.max, Some(Value::Int8(99)));
        assert!(summary.sum.is_some());
        assert!((summary.avg.unwrap() - 49.5).abs() < 0.01);
    }

    #[test]
    fn test_block_zone_summary_skip() {
        let mut block = BlockZoneSummary::new(0, 0);

        for i in 100..200i64 {
            block.update_on_insert(i as u64, &[("id", &Value::Int8(i))]);
        }

        let pred = AnalyzedPredicate {
            column_name: "id".to_string(),
            column_index: 0,
            op: PredicateOp::Eq,
            value: Value::Int8(50), // Outside range
            value2: None,
            value_list: vec![],
            selectivity: 0.5,
            can_use_bloom: false,
            can_use_zone_map: true,
        };

        assert_eq!(block.can_satisfy(&[pred]), BlockDecision::Skip);
    }
}
