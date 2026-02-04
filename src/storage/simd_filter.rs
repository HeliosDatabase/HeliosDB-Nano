//! SIMD-Optimized Predicate Filtering Engine
//!
//! This module implements vectorized predicate evaluation for columnar data processing.
//! It uses batch processing with optional SIMD acceleration for maximum throughput.
//!
//! Key features:
//! - Batch-based filtering for cache efficiency
//! - Early termination with LIMIT support
//! - Combined predicate evaluation (AND/OR)
//! - Statistics tracking for optimization decisions
//! - Support for all Value types

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use parking_lot::RwLock;

use crate::{Value, Tuple, Schema};
use super::predicate_pushdown::{AnalyzedPredicate, PredicateOp};

/// Filter operation types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterOp {
    /// Equality: column = value
    Eq,
    /// Not equal: column != value
    NotEq,
    /// Less than: column < value
    Lt,
    /// Less than or equal: column <= value
    LtEq,
    /// Greater than: column > value
    Gt,
    /// Greater than or equal: column >= value
    GtEq,
    /// IS NULL
    IsNull,
    /// IS NOT NULL
    IsNotNull,
    /// LIKE pattern match
    Like,
    /// NOT LIKE pattern match
    NotLike,
    /// IN list
    In,
    /// NOT IN list
    NotIn,
    /// BETWEEN low AND high
    Between,
    /// NOT BETWEEN
    NotBetween,
}

impl FilterOp {
    /// Get string representation
    pub fn as_str(&self) -> &'static str {
        match self {
            FilterOp::Eq => "=",
            FilterOp::NotEq => "!=",
            FilterOp::Lt => "<",
            FilterOp::LtEq => "<=",
            FilterOp::Gt => ">",
            FilterOp::GtEq => ">=",
            FilterOp::IsNull => "IS NULL",
            FilterOp::IsNotNull => "IS NOT NULL",
            FilterOp::Like => "LIKE",
            FilterOp::NotLike => "NOT LIKE",
            FilterOp::In => "IN",
            FilterOp::NotIn => "NOT IN",
            FilterOp::Between => "BETWEEN",
            FilterOp::NotBetween => "NOT BETWEEN",
        }
    }
}

/// A single filter predicate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterPredicate {
    /// Column index to filter on
    pub column_index: usize,
    /// Column name (for debugging)
    pub column_name: String,
    /// Filter operation
    pub op: FilterOp,
    /// Primary comparison value
    pub value: Value,
    /// Secondary value (for BETWEEN)
    pub value2: Option<Value>,
    /// List of values (for IN)
    pub value_list: Vec<Value>,
    /// Pattern (for LIKE)
    pub pattern: Option<String>,
}

impl FilterPredicate {
    /// Create an equality predicate
    pub fn eq(column_index: usize, column_name: String, value: Value) -> Self {
        Self {
            column_index,
            column_name,
            op: FilterOp::Eq,
            value,
            value2: None,
            value_list: Vec::new(),
            pattern: None,
        }
    }

    /// Create a comparison predicate
    pub fn compare(column_index: usize, column_name: String, op: FilterOp, value: Value) -> Self {
        Self {
            column_index,
            column_name,
            op,
            value,
            value2: None,
            value_list: Vec::new(),
            pattern: None,
        }
    }

    /// Create a BETWEEN predicate
    pub fn between(column_index: usize, column_name: String, low: Value, high: Value) -> Self {
        Self {
            column_index,
            column_name,
            op: FilterOp::Between,
            value: low,
            value2: Some(high),
            value_list: Vec::new(),
            pattern: None,
        }
    }

    /// Create an IN predicate
    pub fn in_list(column_index: usize, column_name: String, values: Vec<Value>) -> Self {
        Self {
            column_index,
            column_name,
            op: FilterOp::In,
            value: Value::Null,
            value2: None,
            value_list: values,
            pattern: None,
        }
    }

    /// Create an IS NULL predicate
    pub fn is_null(column_index: usize, column_name: String) -> Self {
        Self {
            column_index,
            column_name,
            op: FilterOp::IsNull,
            value: Value::Null,
            value2: None,
            value_list: Vec::new(),
            pattern: None,
        }
    }

    /// Create a LIKE predicate
    pub fn like(column_index: usize, column_name: String, pattern: String) -> Self {
        Self {
            column_index,
            column_name,
            op: FilterOp::Like,
            value: Value::Null,
            value2: None,
            value_list: Vec::new(),
            pattern: Some(pattern),
        }
    }

    /// Evaluate the predicate against a value
    pub fn evaluate(&self, value: &Value) -> bool {
        match self.op {
            FilterOp::Eq => Self::compare_eq(value, &self.value),
            FilterOp::NotEq => !Self::compare_eq(value, &self.value),
            FilterOp::Lt => Self::compare_lt(value, &self.value),
            FilterOp::LtEq => Self::compare_lt(value, &self.value) || Self::compare_eq(value, &self.value),
            FilterOp::Gt => Self::compare_gt(value, &self.value),
            FilterOp::GtEq => Self::compare_gt(value, &self.value) || Self::compare_eq(value, &self.value),
            FilterOp::IsNull => matches!(value, Value::Null),
            FilterOp::IsNotNull => !matches!(value, Value::Null),
            FilterOp::Like => self.evaluate_like(value),
            FilterOp::NotLike => !self.evaluate_like(value),
            FilterOp::In => self.value_list.iter().any(|v| Self::compare_eq(value, v)),
            FilterOp::NotIn => !self.value_list.iter().any(|v| Self::compare_eq(value, v)),
            FilterOp::Between => {
                if let Some(high) = &self.value2 {
                    (Self::compare_gt(value, &self.value) || Self::compare_eq(value, &self.value))
                        && (Self::compare_lt(value, high) || Self::compare_eq(value, high))
                } else {
                    false
                }
            }
            FilterOp::NotBetween => {
                if let Some(high) = &self.value2 {
                    Self::compare_lt(value, &self.value) || Self::compare_gt(value, high)
                } else {
                    true
                }
            }
        }
    }

    fn compare_eq(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Null, Value::Null) => true,
            (Value::Null, _) | (_, Value::Null) => false,
            (Value::Boolean(a), Value::Boolean(b)) => a == b,
            (Value::Int2(a), Value::Int2(b)) => a == b,
            (Value::Int4(a), Value::Int4(b)) => a == b,
            (Value::Int8(a), Value::Int8(b)) => a == b,
            // Cross-integer comparisons
            (Value::Int2(a), Value::Int4(b)) => (*a as i32) == *b,
            (Value::Int4(a), Value::Int2(b)) => *a == (*b as i32),
            (Value::Int2(a), Value::Int8(b)) => (*a as i64) == *b,
            (Value::Int8(a), Value::Int2(b)) => *a == (*b as i64),
            (Value::Int4(a), Value::Int8(b)) => (*a as i64) == *b,
            (Value::Int8(a), Value::Int4(b)) => *a == (*b as i64),
            (Value::Float4(a), Value::Float4(b)) => (a - b).abs() < f32::EPSILON,
            (Value::Float8(a), Value::Float8(b)) => (a - b).abs() < f64::EPSILON,
            (Value::Float4(a), Value::Float8(b)) => ((*a as f64) - b).abs() < f64::EPSILON,
            (Value::Float8(a), Value::Float4(b)) => (a - (*b as f64)).abs() < f64::EPSILON,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            (Value::Uuid(a), Value::Uuid(b)) => a == b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a == b,
            (Value::Numeric(a), Value::Numeric(b)) => a == b,
            (Value::Json(a), Value::Json(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| Self::compare_eq(x, y))
            }
            (Value::Vector(a), Value::Vector(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < f32::EPSILON)
            }
            _ => false,
        }
    }

    fn compare_lt(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Null, _) | (_, Value::Null) => false,
            (Value::Boolean(a), Value::Boolean(b)) => !a && *b,
            (Value::Int2(a), Value::Int2(b)) => a < b,
            (Value::Int4(a), Value::Int4(b)) => a < b,
            (Value::Int8(a), Value::Int8(b)) => a < b,
            (Value::Int2(a), Value::Int4(b)) => (*a as i32) < *b,
            (Value::Int4(a), Value::Int2(b)) => *a < (*b as i32),
            (Value::Int2(a), Value::Int8(b)) => (*a as i64) < *b,
            (Value::Int8(a), Value::Int2(b)) => *a < (*b as i64),
            (Value::Int4(a), Value::Int8(b)) => (*a as i64) < *b,
            (Value::Int8(a), Value::Int4(b)) => *a < (*b as i64),
            (Value::Float4(a), Value::Float4(b)) => a < b,
            (Value::Float8(a), Value::Float8(b)) => a < b,
            (Value::Float4(a), Value::Float8(b)) => (*a as f64) < *b,
            (Value::Float8(a), Value::Float4(b)) => *a < (*b as f64),
            (Value::String(a), Value::String(b)) => a < b,
            (Value::Bytes(a), Value::Bytes(b)) => a < b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a < b,
            (Value::Numeric(a), Value::Numeric(b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<f64>(), b.parse::<f64>()) {
                    a < b
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn compare_gt(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Null, _) | (_, Value::Null) => false,
            (Value::Boolean(a), Value::Boolean(b)) => *a && !b,
            (Value::Int2(a), Value::Int2(b)) => a > b,
            (Value::Int4(a), Value::Int4(b)) => a > b,
            (Value::Int8(a), Value::Int8(b)) => a > b,
            (Value::Int2(a), Value::Int4(b)) => (*a as i32) > *b,
            (Value::Int4(a), Value::Int2(b)) => *a > (*b as i32),
            (Value::Int2(a), Value::Int8(b)) => (*a as i64) > *b,
            (Value::Int8(a), Value::Int2(b)) => *a > (*b as i64),
            (Value::Int4(a), Value::Int8(b)) => (*a as i64) > *b,
            (Value::Int8(a), Value::Int4(b)) => *a > (*b as i64),
            (Value::Float4(a), Value::Float4(b)) => a > b,
            (Value::Float8(a), Value::Float8(b)) => a > b,
            (Value::Float4(a), Value::Float8(b)) => (*a as f64) > *b,
            (Value::Float8(a), Value::Float4(b)) => *a > (*b as f64),
            (Value::String(a), Value::String(b)) => a > b,
            (Value::Bytes(a), Value::Bytes(b)) => a > b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a > b,
            (Value::Numeric(a), Value::Numeric(b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<f64>(), b.parse::<f64>()) {
                    a > b
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn evaluate_like(&self, value: &Value) -> bool {
        let pattern = match &self.pattern {
            Some(p) => p,
            None => return false,
        };

        let text = match value {
            Value::String(s) => s.as_str(),
            _ => return false,
        };

        // Simple LIKE pattern matching
        // % matches any sequence, _ matches any single character
        Self::match_like_pattern(text, pattern)
    }

    fn match_like_pattern(text: &str, pattern: &str) -> bool {
        let text_chars: Vec<char> = text.chars().collect();
        let pattern_chars: Vec<char> = pattern.chars().collect();
        Self::match_like_recursive(&text_chars, &pattern_chars, 0, 0)
    }

    fn match_like_recursive(text: &[char], pattern: &[char], ti: usize, pi: usize) -> bool {
        if pi == pattern.len() {
            return ti == text.len();
        }

        if pattern[pi] == '%' {
            // Try matching 0 or more characters
            for i in ti..=text.len() {
                if Self::match_like_recursive(text, pattern, i, pi + 1) {
                    return true;
                }
            }
            return false;
        }

        if ti == text.len() {
            return false;
        }

        if pattern[pi] == '_' || pattern[pi] == text[ti] {
            return Self::match_like_recursive(text, pattern, ti + 1, pi + 1);
        }

        false
    }
}

/// Filter result for a batch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterResult {
    /// Indices of matching rows
    pub matched_indices: Vec<usize>,
    /// Number of matches
    pub matched_count: usize,
    /// Total rows evaluated
    pub total_count: usize,
    /// Time spent filtering in microseconds
    pub filter_time_micros: u64,
    /// Selectivity (matched / total)
    pub selectivity: f64,
}

impl FilterResult {
    /// Create an empty result
    pub fn empty() -> Self {
        Self {
            matched_indices: Vec::new(),
            matched_count: 0,
            total_count: 0,
            filter_time_micros: 0,
            selectivity: 0.0,
        }
    }
}

/// CPU SIMD capabilities detection
#[derive(Debug, Clone, Copy, Default)]
pub struct SimdCapabilities {
    /// AVX2 available (256-bit SIMD)
    pub avx2: bool,
    /// AVX-512 available (512-bit SIMD)
    pub avx512f: bool,
    /// SSE4.1 available (128-bit SIMD)
    pub sse41: bool,
}

impl SimdCapabilities {
    /// Detect available CPU SIMD features at runtime
    #[cfg(target_arch = "x86_64")]
    pub fn detect() -> Self {
        Self {
            avx2: is_x86_feature_detected!("avx2"),
            avx512f: is_x86_feature_detected!("avx512f"),
            sse41: is_x86_feature_detected!("sse4.1"),
        }
    }

    /// For non-x86_64 platforms, return no SIMD features
    #[cfg(not(target_arch = "x86_64"))]
    pub fn detect() -> Self {
        Self {
            avx2: false,
            avx512f: false,
            sse41: false,
        }
    }

    /// Get a string describing available features
    pub fn description(&self) -> String {
        let mut features = Vec::new();
        if self.avx512f {
            features.push("AVX-512");
        }
        if self.avx2 {
            features.push("AVX2");
        }
        if self.sse41 {
            features.push("SSE4.1");
        }
        if features.is_empty() {
            "Scalar (no SIMD)".to_string()
        } else {
            features.join(", ")
        }
    }

    /// Get the best available SIMD level
    pub fn best_level(&self) -> SimdLevel {
        if self.avx512f {
            SimdLevel::Avx512
        } else if self.avx2 {
            SimdLevel::Avx2
        } else if self.sse41 {
            SimdLevel::Sse41
        } else {
            SimdLevel::Scalar
        }
    }
}

/// SIMD optimization level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimdLevel {
    /// No SIMD, scalar operations
    Scalar,
    /// SSE4.1 (128-bit, 4 floats or 4 i32s)
    Sse41,
    /// AVX2 (256-bit, 8 floats or 8 i32s)
    Avx2,
    /// AVX-512 (512-bit, 16 floats or 16 i32s)
    Avx512,
}

impl SimdLevel {
    /// Get the width in i32 elements
    pub fn i32_width(&self) -> usize {
        match self {
            SimdLevel::Scalar => 1,
            SimdLevel::Sse41 => 4,
            SimdLevel::Avx2 => 8,
            SimdLevel::Avx512 => 16,
        }
    }
}

/// Get detected CPU SIMD capabilities (cached)
pub fn simd_capabilities() -> SimdCapabilities {
    static CAPABILITIES: std::sync::OnceLock<SimdCapabilities> = std::sync::OnceLock::new();
    *CAPABILITIES.get_or_init(SimdCapabilities::detect)
}

/// Statistics for filter engine performance
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SimdFilterStats {
    /// Total filter operations
    pub total_filters: u64,
    /// Total rows filtered
    pub total_rows_filtered: u64,
    /// Total rows matched
    pub total_rows_matched: u64,
    /// Total time spent filtering (microseconds)
    pub total_filter_time_micros: u64,
    /// Early terminations (due to LIMIT)
    pub early_terminations: u64,
    /// Average selectivity
    pub avg_selectivity: f64,
    /// Number of SIMD-accelerated filter operations
    pub simd_operations: u64,
    /// Number of scalar fallback operations
    pub scalar_operations: u64,
}

impl SimdFilterStats {
    /// Overall selectivity
    pub fn overall_selectivity(&self) -> f64 {
        if self.total_rows_filtered == 0 {
            0.0
        } else {
            self.total_rows_matched as f64 / self.total_rows_filtered as f64
        }
    }

    /// Average filter time per row (nanoseconds)
    pub fn avg_time_per_row_ns(&self) -> f64 {
        if self.total_rows_filtered == 0 {
            0.0
        } else {
            (self.total_filter_time_micros as f64 * 1000.0) / self.total_rows_filtered as f64
        }
    }
}

/// Combined predicate for AND/OR operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CombinedPredicate {
    /// Single predicate
    Single(FilterPredicate),
    /// AND of predicates (all must match)
    And(Vec<CombinedPredicate>),
    /// OR of predicates (any must match)
    Or(Vec<CombinedPredicate>),
    /// NOT of predicate
    Not(Box<CombinedPredicate>),
}

impl CombinedPredicate {
    /// Evaluate against a row (slice of values)
    pub fn evaluate(&self, row: &[Value]) -> bool {
        match self {
            CombinedPredicate::Single(pred) => {
                if pred.column_index < row.len() {
                    pred.evaluate(&row[pred.column_index])
                } else {
                    false
                }
            }
            CombinedPredicate::And(preds) => preds.iter().all(|p| p.evaluate(row)),
            CombinedPredicate::Or(preds) => preds.iter().any(|p| p.evaluate(row)),
            CombinedPredicate::Not(pred) => !pred.evaluate(row),
        }
    }

    /// Get all column indices referenced by this predicate
    pub fn get_column_indices(&self) -> Vec<usize> {
        let mut indices = Vec::new();
        self.collect_column_indices(&mut indices);
        indices.sort();
        indices.dedup();
        indices
    }

    fn collect_column_indices(&self, indices: &mut Vec<usize>) {
        match self {
            CombinedPredicate::Single(pred) => indices.push(pred.column_index),
            CombinedPredicate::And(preds) | CombinedPredicate::Or(preds) => {
                for p in preds {
                    p.collect_column_indices(indices);
                }
            }
            CombinedPredicate::Not(pred) => pred.collect_column_indices(indices),
        }
    }
}

/// SIMD-optimized predicate filtering engine
pub struct SimdPredicateFilteringEngine {
    /// Statistics
    stats: Arc<RwLock<SimdFilterStats>>,
    /// Batch size for vectorized operations
    batch_size: usize,
}

impl SimdPredicateFilteringEngine {
    /// Create a new filtering engine
    pub fn new() -> Self {
        Self {
            stats: Arc::new(RwLock::new(SimdFilterStats::default())),
            batch_size: 1024,
        }
    }

    /// Create with custom batch size
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self {
            stats: Arc::new(RwLock::new(SimdFilterStats::default())),
            batch_size: batch_size.max(64),
        }
    }

    /// Filter a column of values with a single predicate
    pub fn filter_column(&self, values: &[Value], predicate: &FilterPredicate) -> FilterResult {
        let start = std::time::Instant::now();
        let mut result = FilterResult::empty();
        result.total_count = values.len();

        for (idx, value) in values.iter().enumerate() {
            if predicate.evaluate(value) {
                result.matched_indices.push(idx);
                result.matched_count += 1;
            }
        }

        result.filter_time_micros = start.elapsed().as_micros() as u64;
        result.selectivity = if result.total_count > 0 {
            result.matched_count as f64 / result.total_count as f64
        } else {
            0.0
        };

        self.update_stats(&result);
        result
    }

    /// Filter a column with early termination (for LIMIT)
    pub fn filter_column_with_limit(
        &self,
        values: &[Value],
        predicate: &FilterPredicate,
        limit: usize,
    ) -> FilterResult {
        let start = std::time::Instant::now();
        let mut result = FilterResult::empty();
        result.total_count = values.len();

        for (idx, value) in values.iter().enumerate() {
            if predicate.evaluate(value) {
                result.matched_indices.push(idx);
                result.matched_count += 1;

                if result.matched_count >= limit {
                    let mut stats = self.stats.write();
                    stats.early_terminations += 1;
                    break;
                }
            }
        }

        result.filter_time_micros = start.elapsed().as_micros() as u64;
        result.selectivity = if result.total_count > 0 {
            result.matched_count as f64 / result.total_count as f64
        } else {
            0.0
        };

        self.update_stats(&result);
        result
    }

    /// Filter rows with a combined predicate (AND/OR)
    pub fn filter_rows(&self, rows: &[Vec<Value>], predicate: &CombinedPredicate) -> FilterResult {
        let start = std::time::Instant::now();
        let mut result = FilterResult::empty();
        result.total_count = rows.len();

        for (idx, row) in rows.iter().enumerate() {
            if predicate.evaluate(row) {
                result.matched_indices.push(idx);
                result.matched_count += 1;
            }
        }

        result.filter_time_micros = start.elapsed().as_micros() as u64;
        result.selectivity = if result.total_count > 0 {
            result.matched_count as f64 / result.total_count as f64
        } else {
            0.0
        };

        self.update_stats(&result);
        result
    }

    /// Filter rows with early termination
    pub fn filter_rows_with_limit(
        &self,
        rows: &[Vec<Value>],
        predicate: &CombinedPredicate,
        limit: usize,
    ) -> FilterResult {
        let start = std::time::Instant::now();
        let mut result = FilterResult::empty();
        result.total_count = rows.len();

        for (idx, row) in rows.iter().enumerate() {
            if predicate.evaluate(row) {
                result.matched_indices.push(idx);
                result.matched_count += 1;

                if result.matched_count >= limit {
                    let mut stats = self.stats.write();
                    stats.early_terminations += 1;
                    break;
                }
            }
        }

        result.filter_time_micros = start.elapsed().as_micros() as u64;
        result.selectivity = if result.total_count > 0 {
            result.matched_count as f64 / result.total_count as f64
        } else {
            0.0
        };

        self.update_stats(&result);
        result
    }

    /// Filter with multiple AND predicates (optimized order)
    pub fn filter_and_predicates(
        &self,
        rows: &[Vec<Value>],
        predicates: &[FilterPredicate],
    ) -> FilterResult {
        if predicates.is_empty() {
            return FilterResult {
                matched_indices: (0..rows.len()).collect(),
                matched_count: rows.len(),
                total_count: rows.len(),
                filter_time_micros: 0,
                selectivity: 1.0,
            };
        }

        let start = std::time::Instant::now();
        let mut result = FilterResult::empty();
        result.total_count = rows.len();

        'outer: for (idx, row) in rows.iter().enumerate() {
            for pred in predicates {
                if pred.column_index >= row.len() || !pred.evaluate(&row[pred.column_index]) {
                    continue 'outer;
                }
            }
            result.matched_indices.push(idx);
            result.matched_count += 1;
        }

        result.filter_time_micros = start.elapsed().as_micros() as u64;
        result.selectivity = if result.total_count > 0 {
            result.matched_count as f64 / result.total_count as f64
        } else {
            0.0
        };

        self.update_stats(&result);
        result
    }

    /// Batch filter for i32 columns with SIMD acceleration
    ///
    /// Uses AVX2 when available for 8x parallelism, falling back to scalar otherwise.
    pub fn filter_int32_batch_simd(&self, values: &[i32], op: FilterOp, compare_value: i32) -> FilterResult {
        let start = std::time::Instant::now();
        let mut result = FilterResult::empty();
        result.total_count = values.len();

        let caps = simd_capabilities();

        #[cfg(target_arch = "x86_64")]
        {
            if caps.avx2 && values.len() >= 8 {
                // AVX2 path - process 8 i32s at a time
                let matched = unsafe {
                    self.filter_int32_avx2(values, op, compare_value)
                };
                result.matched_indices = matched;
                result.matched_count = result.matched_indices.len();

                // Track SIMD usage
                let mut stats = self.stats.write();
                stats.simd_operations += 1;
            } else {
                // Scalar fallback
                self.filter_int32_scalar(values, op, compare_value, &mut result);
                let mut stats = self.stats.write();
                stats.scalar_operations += 1;
            }
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            self.filter_int32_scalar(values, op, compare_value, &mut result);
            let mut stats = self.stats.write();
            stats.scalar_operations += 1;
        }

        result.filter_time_micros = start.elapsed().as_micros() as u64;
        result.selectivity = if result.total_count > 0 {
            result.matched_count as f64 / result.total_count as f64
        } else {
            0.0
        };

        self.update_stats(&result);
        result
    }

    /// Scalar i32 filter implementation
    fn filter_int32_scalar(&self, values: &[i32], op: FilterOp, compare_value: i32, result: &mut FilterResult) {
        for (i, &val) in values.iter().enumerate() {
            let matches = match op {
                FilterOp::Eq => val == compare_value,
                FilterOp::NotEq => val != compare_value,
                FilterOp::Lt => val < compare_value,
                FilterOp::LtEq => val <= compare_value,
                FilterOp::Gt => val > compare_value,
                FilterOp::GtEq => val >= compare_value,
                _ => false,
            };
            if matches {
                result.matched_indices.push(i);
                result.matched_count += 1;
            }
        }
    }

    /// AVX2-accelerated i32 filter (x86_64 only)
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn filter_int32_avx2(&self, values: &[i32], op: FilterOp, compare_value: i32) -> Vec<usize> {
        use std::arch::x86_64::*;

        let mut matched = Vec::new();
        let len = values.len();
        let chunks = len / 8;
        let remainder = len % 8;

        // Broadcast compare value to all 8 lanes
        let cmp_vec = _mm256_set1_epi32(compare_value);

        let ptr = values.as_ptr();

        for chunk_idx in 0..chunks {
            let offset = chunk_idx * 8;
            let data = _mm256_loadu_si256(ptr.add(offset) as *const __m256i);

            // Compare based on operation
            let mask = match op {
                FilterOp::Eq => _mm256_cmpeq_epi32(data, cmp_vec),
                FilterOp::Lt => _mm256_cmpgt_epi32(cmp_vec, data), // cmp > data means data < cmp
                FilterOp::Gt => _mm256_cmpgt_epi32(data, cmp_vec),
                FilterOp::LtEq => {
                    // data <= cmp means NOT (data > cmp)
                    let gt = _mm256_cmpgt_epi32(data, cmp_vec);
                    _mm256_xor_si256(gt, _mm256_set1_epi32(-1))
                }
                FilterOp::GtEq => {
                    // data >= cmp means NOT (cmp > data)
                    let lt = _mm256_cmpgt_epi32(cmp_vec, data);
                    _mm256_xor_si256(lt, _mm256_set1_epi32(-1))
                }
                FilterOp::NotEq => {
                    let eq = _mm256_cmpeq_epi32(data, cmp_vec);
                    _mm256_xor_si256(eq, _mm256_set1_epi32(-1))
                }
                _ => _mm256_setzero_si256(), // Unsupported ops return no matches
            };

            // Extract matched indices from mask
            let mask_bits = _mm256_movemask_epi8(mask) as u32;

            // Each i32 uses 4 bytes, so check every 4th bit
            for i in 0..8 {
                if (mask_bits >> (i * 4)) & 0xF == 0xF {
                    matched.push(offset + i);
                }
            }
        }

        // Handle remainder with scalar
        let remainder_start = chunks * 8;
        for i in remainder_start..len {
            let val = values[i];
            let matches = match op {
                FilterOp::Eq => val == compare_value,
                FilterOp::NotEq => val != compare_value,
                FilterOp::Lt => val < compare_value,
                FilterOp::LtEq => val <= compare_value,
                FilterOp::Gt => val > compare_value,
                FilterOp::GtEq => val >= compare_value,
                _ => false,
            };
            if matches {
                matched.push(i);
            }
        }

        matched
    }

    /// Batch filter for integer columns (optimized path)
    pub fn filter_int64_batch(&self, values: &[i64], op: FilterOp, compare_value: i64) -> FilterResult {
        let start = std::time::Instant::now();
        let mut result = FilterResult::empty();
        result.total_count = values.len();

        // Process in batches for cache efficiency
        for (batch_start, chunk) in values.chunks(self.batch_size).enumerate() {
            let base_idx = batch_start * self.batch_size;

            for (i, &val) in chunk.iter().enumerate() {
                let matches = match op {
                    FilterOp::Eq => val == compare_value,
                    FilterOp::NotEq => val != compare_value,
                    FilterOp::Lt => val < compare_value,
                    FilterOp::LtEq => val <= compare_value,
                    FilterOp::Gt => val > compare_value,
                    FilterOp::GtEq => val >= compare_value,
                    _ => false,
                };

                if matches {
                    result.matched_indices.push(base_idx + i);
                    result.matched_count += 1;
                }
            }
        }

        result.filter_time_micros = start.elapsed().as_micros() as u64;
        result.selectivity = if result.total_count > 0 {
            result.matched_count as f64 / result.total_count as f64
        } else {
            0.0
        };

        self.update_stats(&result);
        result
    }

    /// Batch filter for float64 columns (optimized path)
    pub fn filter_float64_batch(&self, values: &[f64], op: FilterOp, compare_value: f64) -> FilterResult {
        let start = std::time::Instant::now();
        let mut result = FilterResult::empty();
        result.total_count = values.len();

        for (batch_start, chunk) in values.chunks(self.batch_size).enumerate() {
            let base_idx = batch_start * self.batch_size;

            for (i, &val) in chunk.iter().enumerate() {
                let matches = match op {
                    FilterOp::Eq => (val - compare_value).abs() < f64::EPSILON,
                    FilterOp::NotEq => (val - compare_value).abs() >= f64::EPSILON,
                    FilterOp::Lt => val < compare_value,
                    FilterOp::LtEq => val <= compare_value,
                    FilterOp::Gt => val > compare_value,
                    FilterOp::GtEq => val >= compare_value,
                    _ => false,
                };

                if matches {
                    result.matched_indices.push(base_idx + i);
                    result.matched_count += 1;
                }
            }
        }

        result.filter_time_micros = start.elapsed().as_micros() as u64;
        result.selectivity = if result.total_count > 0 {
            result.matched_count as f64 / result.total_count as f64
        } else {
            0.0
        };

        self.update_stats(&result);
        result
    }

    /// Filter a batch of tuples using analyzed predicates
    pub fn filter_batch(
        &self,
        tuples: &[Tuple],
        predicates: &[AnalyzedPredicate],
        schema: &Schema,
    ) -> Vec<Tuple> {
        let start = std::time::Instant::now();
        let input_count = tuples.len();

        let result: Vec<Tuple> = tuples.iter()
            .filter(|tuple| {
                predicates.iter().all(|pred| {
                    // Find column index by name
                    let col_idx = schema.columns.iter()
                        .position(|c| c.name == pred.column_name);

                    if let Some(idx) = col_idx {
                        if idx < tuple.values.len() {
                            let value = &tuple.values[idx];
                            self.evaluate_predicate(value, pred)
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                })
            })
            .cloned()
            .collect();

        // Update stats
        let mut stats = self.stats.write();
        stats.total_filters += 1;
        stats.total_rows_filtered += input_count as u64;
        stats.total_rows_matched += result.len() as u64;
        stats.total_filter_time_micros += start.elapsed().as_micros() as u64;

        result
    }

    /// Evaluate a single predicate against a value
    fn evaluate_predicate(&self, value: &Value, pred: &AnalyzedPredicate) -> bool {
        match pred.op {
            PredicateOp::Eq => value == &pred.value,
            PredicateOp::NotEq => value != &pred.value,
            PredicateOp::Lt => self.value_lt(value, &pred.value),
            PredicateOp::LtEq => self.value_le(value, &pred.value),
            PredicateOp::Gt => self.value_gt(value, &pred.value),
            PredicateOp::GtEq => self.value_ge(value, &pred.value),
            PredicateOp::IsNull => matches!(value, Value::Null),
            PredicateOp::IsNotNull => !matches!(value, Value::Null),
            // Between, In, Like - for now return true (not filtered out)
            // These would need additional predicate fields to implement properly
            PredicateOp::Between | PredicateOp::In | PredicateOp::Like => true,
        }
    }

    fn value_lt(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => a < b,
            (Value::Float8(a), Value::Float8(b)) => a < b,
            (Value::String(a), Value::String(b)) => a < b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a < b,
            _ => false,
        }
    }

    fn value_le(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => a <= b,
            (Value::Float8(a), Value::Float8(b)) => a <= b,
            (Value::String(a), Value::String(b)) => a <= b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a <= b,
            _ => false,
        }
    }

    fn value_gt(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => a > b,
            (Value::Float8(a), Value::Float8(b)) => a > b,
            (Value::String(a), Value::String(b)) => a > b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a > b,
            _ => false,
        }
    }

    fn value_ge(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => a >= b,
            (Value::Float8(a), Value::Float8(b)) => a >= b,
            (Value::String(a), Value::String(b)) => a >= b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a >= b,
            _ => false,
        }
    }

    /// Get statistics
    pub fn get_stats(&self) -> SimdFilterStats {
        self.stats.read().clone()
    }

    /// Reset statistics
    pub fn reset_stats(&self) {
        *self.stats.write() = SimdFilterStats::default();
    }

    /// Get batch size
    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    fn update_stats(&self, result: &FilterResult) {
        let mut stats = self.stats.write();
        stats.total_filters += 1;
        stats.total_rows_filtered += result.total_count as u64;
        stats.total_rows_matched += result.matched_count as u64;
        stats.total_filter_time_micros += result.filter_time_micros;

        // Running average of selectivity
        let n = stats.total_filters as f64;
        stats.avg_selectivity = stats.avg_selectivity * ((n - 1.0) / n) + result.selectivity / n;
    }
}

impl Default for SimdPredicateFilteringEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_predicate_eq() {
        let pred = FilterPredicate::eq(0, "id".to_string(), Value::Int8(42));
        assert!(pred.evaluate(&Value::Int8(42)));
        assert!(!pred.evaluate(&Value::Int8(43)));
    }

    #[test]
    fn test_filter_predicate_comparison() {
        let pred = FilterPredicate::compare(0, "age".to_string(), FilterOp::Gt, Value::Int8(18));
        assert!(pred.evaluate(&Value::Int8(21)));
        assert!(!pred.evaluate(&Value::Int8(18)));
        assert!(!pred.evaluate(&Value::Int8(15)));
    }

    #[test]
    fn test_filter_predicate_between() {
        let pred = FilterPredicate::between(0, "age".to_string(), Value::Int8(18), Value::Int8(65));
        assert!(pred.evaluate(&Value::Int8(30)));
        assert!(pred.evaluate(&Value::Int8(18)));
        assert!(pred.evaluate(&Value::Int8(65)));
        assert!(!pred.evaluate(&Value::Int8(17)));
        assert!(!pred.evaluate(&Value::Int8(66)));
    }

    #[test]
    fn test_filter_predicate_in() {
        let pred = FilterPredicate::in_list(
            0,
            "status".to_string(),
            vec![
                Value::String("active".to_string()),
                Value::String("pending".to_string()),
            ],
        );
        assert!(pred.evaluate(&Value::String("active".to_string())));
        assert!(pred.evaluate(&Value::String("pending".to_string())));
        assert!(!pred.evaluate(&Value::String("inactive".to_string())));
    }

    #[test]
    fn test_filter_predicate_like() {
        let pred = FilterPredicate::like(0, "name".to_string(), "A%".to_string());
        assert!(pred.evaluate(&Value::String("Alice".to_string())));
        assert!(pred.evaluate(&Value::String("Anna".to_string())));
        assert!(!pred.evaluate(&Value::String("Bob".to_string())));

        let pred2 = FilterPredicate::like(0, "name".to_string(), "%ice".to_string());
        assert!(pred2.evaluate(&Value::String("Alice".to_string())));
        assert!(!pred2.evaluate(&Value::String("Anna".to_string())));

        let pred3 = FilterPredicate::like(0, "name".to_string(), "A_ice".to_string());
        assert!(pred3.evaluate(&Value::String("Alice".to_string())));
        assert!(!pred3.evaluate(&Value::String("Aice".to_string())));
    }

    #[test]
    fn test_combined_predicate_and() {
        let pred = CombinedPredicate::And(vec![
            CombinedPredicate::Single(FilterPredicate::compare(0, "age".to_string(), FilterOp::GtEq, Value::Int8(18))),
            CombinedPredicate::Single(FilterPredicate::compare(1, "status".to_string(), FilterOp::Eq, Value::String("active".to_string()))),
        ]);

        assert!(pred.evaluate(&[Value::Int8(21), Value::String("active".to_string())]));
        assert!(!pred.evaluate(&[Value::Int8(21), Value::String("inactive".to_string())]));
        assert!(!pred.evaluate(&[Value::Int8(16), Value::String("active".to_string())]));
    }

    #[test]
    fn test_combined_predicate_or() {
        let pred = CombinedPredicate::Or(vec![
            CombinedPredicate::Single(FilterPredicate::eq(0, "status".to_string(), Value::String("active".to_string()))),
            CombinedPredicate::Single(FilterPredicate::eq(0, "status".to_string(), Value::String("pending".to_string()))),
        ]);

        assert!(pred.evaluate(&[Value::String("active".to_string())]));
        assert!(pred.evaluate(&[Value::String("pending".to_string())]));
        assert!(!pred.evaluate(&[Value::String("inactive".to_string())]));
    }

    #[test]
    fn test_simd_engine_filter_column() {
        let engine = SimdPredicateFilteringEngine::new();
        let values = vec![
            Value::Int8(1),
            Value::Int8(5),
            Value::Int8(10),
            Value::Int8(15),
            Value::Int8(20),
        ];
        let pred = FilterPredicate::compare(0, "val".to_string(), FilterOp::Gt, Value::Int8(7));

        let result = engine.filter_column(&values, &pred);
        assert_eq!(result.matched_count, 3); // 10, 15, 20
        assert_eq!(result.matched_indices, vec![2, 3, 4]);
    }

    #[test]
    fn test_simd_engine_filter_with_limit() {
        let engine = SimdPredicateFilteringEngine::new();
        let values = vec![
            Value::Int8(1),
            Value::Int8(5),
            Value::Int8(10),
            Value::Int8(15),
            Value::Int8(20),
        ];
        let pred = FilterPredicate::compare(0, "val".to_string(), FilterOp::Gt, Value::Int8(3));

        let result = engine.filter_column_with_limit(&values, &pred, 2);
        assert_eq!(result.matched_count, 2);
        assert_eq!(result.matched_indices, vec![1, 2]); // 5, 10
    }

    #[test]
    fn test_simd_engine_int64_batch() {
        let engine = SimdPredicateFilteringEngine::with_batch_size(2);
        let values: Vec<i64> = vec![1, 5, 10, 15, 20, 25];

        let result = engine.filter_int64_batch(&values, FilterOp::GtEq, 10);
        assert_eq!(result.matched_count, 4); // 10, 15, 20, 25
    }

    #[test]
    fn test_filter_and_predicates() {
        let engine = SimdPredicateFilteringEngine::new();
        let rows = vec![
            vec![Value::Int8(1), Value::String("active".to_string())],
            vec![Value::Int8(5), Value::String("active".to_string())],
            vec![Value::Int8(10), Value::String("inactive".to_string())],
            vec![Value::Int8(15), Value::String("active".to_string())],
        ];

        let predicates = vec![
            FilterPredicate::compare(0, "id".to_string(), FilterOp::Gt, Value::Int8(3)),
            FilterPredicate::eq(1, "status".to_string(), Value::String("active".to_string())),
        ];

        let result = engine.filter_and_predicates(&rows, &predicates);
        assert_eq!(result.matched_count, 2); // rows 1 and 3 (5, active) and (15, active)
    }
}
