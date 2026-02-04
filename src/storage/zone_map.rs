//! Zone Map Implementation for Storage-Level Range Filtering
//!
//! Zone maps (also called min/max indexes or data skipping indexes) track
//! minimum and maximum values for each data block. This allows the storage
//! engine to skip entire blocks that cannot possibly match range predicates.
//!
//! Key features:
//! - Per-column min/max tracking
//! - Support for all comparable data types
//! - Block-level granularity
//! - Null tracking for IS NULL predicates
//! - Efficient range predicate evaluation

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;

use crate::Value;

/// Represents a range of values [min, max]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueRange {
    /// Minimum value in the range
    pub min: Option<Value>,
    /// Maximum value in the range
    pub max: Option<Value>,
    /// Whether any null values exist
    pub has_nulls: bool,
    /// Count of values in this range
    pub count: u64,
    /// Count of null values
    pub null_count: u64,
}

impl ValueRange {
    /// Create an empty range
    pub fn empty() -> Self {
        Self {
            min: None,
            max: None,
            has_nulls: false,
            count: 0,
            null_count: 0,
        }
    }

    /// Create a range from a single value
    pub fn from_value(value: Value) -> Self {
        if matches!(value, Value::Null) {
            Self {
                min: None,
                max: None,
                has_nulls: true,
                count: 1,
                null_count: 1,
            }
        } else {
            Self {
                min: Some(value.clone()),
                max: Some(value),
                has_nulls: false,
                count: 1,
                null_count: 0,
            }
        }
    }

    /// Update the range with a new value
    pub fn update(&mut self, value: &Value) {
        self.count += 1;

        if matches!(value, Value::Null) {
            self.has_nulls = true;
            self.null_count += 1;
            return;
        }

        // Update min
        match &self.min {
            None => self.min = Some(value.clone()),
            Some(current_min) => {
                if Self::compare_values(value, current_min) == Some(Ordering::Less) {
                    self.min = Some(value.clone());
                }
            }
        }

        // Update max
        match &self.max {
            None => self.max = Some(value.clone()),
            Some(current_max) => {
                if Self::compare_values(value, current_max) == Some(Ordering::Greater) {
                    self.max = Some(value.clone());
                }
            }
        }
    }

    /// Merge another range into this one
    pub fn merge(&mut self, other: &ValueRange) {
        self.count += other.count;
        self.null_count += other.null_count;
        self.has_nulls = self.has_nulls || other.has_nulls;

        // Merge min
        if let Some(other_min) = &other.min {
            match &self.min {
                None => self.min = Some(other_min.clone()),
                Some(current_min) => {
                    if Self::compare_values(other_min, current_min) == Some(Ordering::Less) {
                        self.min = Some(other_min.clone());
                    }
                }
            }
        }

        // Merge max
        if let Some(other_max) = &other.max {
            match &self.max {
                None => self.max = Some(other_max.clone()),
                Some(current_max) => {
                    if Self::compare_values(other_max, current_max) == Some(Ordering::Greater) {
                        self.max = Some(other_max.clone());
                    }
                }
            }
        }
    }

    /// Check if a value might be in this range (for equality)
    pub fn might_contain(&self, value: &Value) -> bool {
        if matches!(value, Value::Null) {
            return self.has_nulls;
        }

        let min = match &self.min {
            Some(m) => m,
            None => return false, // Empty range
        };

        let max = match &self.max {
            Some(m) => m,
            None => return false,
        };

        // Value must be >= min and <= max
        let ge_min = Self::compare_values(value, min)
            .map(|o| o != Ordering::Less)
            .unwrap_or(false);
        let le_max = Self::compare_values(value, max)
            .map(|o| o != Ordering::Greater)
            .unwrap_or(false);

        ge_min && le_max
    }

    /// Check if this range might contain values less than the given value
    pub fn might_contain_less_than(&self, value: &Value) -> bool {
        match &self.min {
            Some(min) => {
                Self::compare_values(min, value)
                    .map(|o| o == Ordering::Less)
                    .unwrap_or(false)
            }
            None => false,
        }
    }

    /// Check if this range might contain values less than or equal to the given value
    pub fn might_contain_less_or_equal(&self, value: &Value) -> bool {
        match &self.min {
            Some(min) => {
                Self::compare_values(min, value)
                    .map(|o| o != Ordering::Greater)
                    .unwrap_or(false)
            }
            None => false,
        }
    }

    /// Check if this range might contain values greater than the given value
    pub fn might_contain_greater_than(&self, value: &Value) -> bool {
        match &self.max {
            Some(max) => {
                Self::compare_values(max, value)
                    .map(|o| o == Ordering::Greater)
                    .unwrap_or(false)
            }
            None => false,
        }
    }

    /// Check if this range might contain values greater than or equal to the given value
    pub fn might_contain_greater_or_equal(&self, value: &Value) -> bool {
        match &self.max {
            Some(max) => {
                Self::compare_values(max, value)
                    .map(|o| o != Ordering::Less)
                    .unwrap_or(false)
            }
            None => false,
        }
    }

    /// Check if this range might overlap with a given range [low, high]
    pub fn might_overlap(&self, low: &Value, high: &Value) -> bool {
        // Range overlaps if: self.min <= high AND self.max >= low
        let min_le_high = match &self.min {
            Some(min) => Self::compare_values(min, high)
                .map(|o| o != Ordering::Greater)
                .unwrap_or(false),
            None => false,
        };

        let max_ge_low = match &self.max {
            Some(max) => Self::compare_values(max, low)
                .map(|o| o != Ordering::Less)
                .unwrap_or(false),
            None => false,
        };

        min_le_high && max_ge_low
    }

    /// Compare two values
    fn compare_values(a: &Value, b: &Value) -> Option<Ordering> {
        match (a, b) {
            (Value::Null, Value::Null) => Some(Ordering::Equal),
            (Value::Null, _) => Some(Ordering::Less), // NULL sorts first
            (_, Value::Null) => Some(Ordering::Greater),

            (Value::Boolean(a), Value::Boolean(b)) => Some(a.cmp(b)),
            (Value::Int2(a), Value::Int2(b)) => Some(a.cmp(b)),
            (Value::Int4(a), Value::Int4(b)) => Some(a.cmp(b)),
            (Value::Int8(a), Value::Int8(b)) => Some(a.cmp(b)),

            // Cross-integer comparisons
            (Value::Int2(a), Value::Int4(b)) => Some((*a as i32).cmp(b)),
            (Value::Int4(a), Value::Int2(b)) => Some(a.cmp(&(*b as i32))),
            (Value::Int2(a), Value::Int8(b)) => Some((*a as i64).cmp(b)),
            (Value::Int8(a), Value::Int2(b)) => Some(a.cmp(&(*b as i64))),
            (Value::Int4(a), Value::Int8(b)) => Some((*a as i64).cmp(b)),
            (Value::Int8(a), Value::Int4(b)) => Some(a.cmp(&(*b as i64))),

            (Value::Float4(a), Value::Float4(b)) => a.partial_cmp(b),
            (Value::Float8(a), Value::Float8(b)) => a.partial_cmp(b),
            (Value::Float4(a), Value::Float8(b)) => (*a as f64).partial_cmp(b),
            (Value::Float8(a), Value::Float4(b)) => a.partial_cmp(&(*b as f64)),

            (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
            (Value::Bytes(a), Value::Bytes(b)) => Some(a.cmp(b)),
            (Value::Uuid(a), Value::Uuid(b)) => Some(a.cmp(b)),
            (Value::Timestamp(a), Value::Timestamp(b)) => Some(a.cmp(b)),
            (Value::Numeric(a), Value::Numeric(b)) => {
                // Parse and compare numerics
                Self::compare_numeric_strings(a, b)
            }
            (Value::Json(a), Value::Json(b)) => Some(a.cmp(b)),

            // Incompatible types - can't compare
            _ => None,
        }
    }

    /// Compare numeric strings
    fn compare_numeric_strings(a: &str, b: &str) -> Option<Ordering> {
        // Try parsing as floats for comparison
        let a_val: f64 = a.parse().ok()?;
        let b_val: f64 = b.parse().ok()?;
        a_val.partial_cmp(&b_val)
    }
}

/// Column zone map - tracks value ranges for a single column
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnZoneMap {
    /// Column name
    pub column_name: String,
    /// Overall value range
    pub range: ValueRange,
}

impl ColumnZoneMap {
    /// Create a new column zone map
    pub fn new(column_name: String) -> Self {
        Self {
            column_name,
            range: ValueRange::empty(),
        }
    }

    /// Update with a value
    pub fn update(&mut self, value: &Value) {
        self.range.update(value);
    }

    /// Merge with another zone map
    pub fn merge(&mut self, other: &ColumnZoneMap) {
        self.range.merge(&other.range);
    }
}

/// Block zone map - tracks ranges for all columns in a block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockZoneMap {
    /// Block identifier
    pub block_id: u64,
    /// Row count in this block
    pub row_count: u64,
    /// First row ID in block
    pub first_row_id: u64,
    /// Last row ID in block
    pub last_row_id: u64,
    /// Per-column zone maps
    pub columns: HashMap<String, ColumnZoneMap>,
}

impl BlockZoneMap {
    /// Create a new block zone map
    pub fn new(block_id: u64, first_row_id: u64) -> Self {
        Self {
            block_id,
            row_count: 0,
            first_row_id,
            last_row_id: first_row_id,
            columns: HashMap::new(),
        }
    }

    /// Add a row to the zone map
    pub fn add_row(&mut self, row_id: u64, values: &[(String, Value)]) {
        self.row_count += 1;
        self.last_row_id = row_id;

        for (col_name, value) in values {
            self.columns
                .entry(col_name.clone())
                .or_insert_with(|| ColumnZoneMap::new(col_name.clone()))
                .update(value);
        }
    }

    /// Check if this block might contain a value in a column (equality)
    pub fn might_contain(&self, column_name: &str, value: &Value) -> bool {
        match self.columns.get(column_name) {
            Some(czm) => czm.range.might_contain(value),
            None => true, // Unknown column - might contain
        }
    }

    /// Check if this block might match a range predicate
    pub fn might_match_range(
        &self,
        column_name: &str,
        op: RangeOp,
        value: &Value,
    ) -> bool {
        match self.columns.get(column_name) {
            Some(czm) => match op {
                RangeOp::Eq => czm.range.might_contain(value),
                RangeOp::NotEq => true, // Can't skip for not-equal
                RangeOp::Lt => czm.range.might_contain_less_than(value),
                RangeOp::LtEq => czm.range.might_contain_less_or_equal(value),
                RangeOp::Gt => czm.range.might_contain_greater_than(value),
                RangeOp::GtEq => czm.range.might_contain_greater_or_equal(value),
            },
            None => true,
        }
    }

    /// Check if this block might match a BETWEEN predicate
    pub fn might_match_between(
        &self,
        column_name: &str,
        low: &Value,
        high: &Value,
    ) -> bool {
        match self.columns.get(column_name) {
            Some(czm) => czm.range.might_overlap(low, high),
            None => true,
        }
    }

    /// Check if this block might contain NULL values in a column
    pub fn might_contain_null(&self, column_name: &str) -> bool {
        match self.columns.get(column_name) {
            Some(czm) => czm.range.has_nulls,
            None => true,
        }
    }

    /// Check if this block might contain non-NULL values in a column
    pub fn might_contain_not_null(&self, column_name: &str) -> bool {
        match self.columns.get(column_name) {
            Some(czm) => czm.range.count > czm.range.null_count,
            None => true,
        }
    }
}

/// Range operation type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

/// Zone map statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZoneMapStats {
    /// Total blocks tracked
    pub total_blocks: u64,
    /// Blocks skipped due to zone map filtering
    pub blocks_skipped: u64,
    /// Blocks scanned
    pub blocks_scanned: u64,
    /// Total predicate evaluations
    pub predicate_evaluations: u64,
}

impl ZoneMapStats {
    /// Calculate skip ratio
    pub fn skip_ratio(&self) -> f64 {
        let total = self.blocks_skipped + self.blocks_scanned;
        if total == 0 {
            0.0
        } else {
            self.blocks_skipped as f64 / total as f64
        }
    }
}

/// Table zone map manager
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableZoneMap {
    /// Table name
    pub table_name: String,
    /// Block zone maps
    pub blocks: Vec<BlockZoneMap>,
    /// Statistics
    pub stats: ZoneMapStats,
    /// Block size (rows per block)
    pub block_size: usize,
}

impl TableZoneMap {
    /// Create a new table zone map
    pub fn new(table_name: String, block_size: usize) -> Self {
        Self {
            table_name,
            blocks: Vec::new(),
            stats: ZoneMapStats::default(),
            block_size: block_size.max(1),
        }
    }

    /// Create with default block size (1000 rows)
    pub fn with_defaults(table_name: String) -> Self {
        Self::new(table_name, 1000)
    }

    /// Add a row to the appropriate block
    pub fn add_row(&mut self, row_id: u64, values: &[(String, Value)]) {
        let block_idx = (row_id as usize) / self.block_size;

        // Ensure we have enough blocks
        while self.blocks.len() <= block_idx {
            let block_id = self.blocks.len() as u64;
            let first_row_id = block_id * self.block_size as u64;
            self.blocks.push(BlockZoneMap::new(block_id, first_row_id));
            self.stats.total_blocks += 1;
        }

        self.blocks[block_idx].add_row(row_id, values);
    }

    /// Get blocks that might match an equality predicate
    pub fn get_matching_blocks_eq(&mut self, column_name: &str, value: &Value) -> Vec<u64> {
        let mut matching = Vec::new();

        for block in &self.blocks {
            self.stats.predicate_evaluations += 1;
            if block.might_contain(column_name, value) {
                matching.push(block.block_id);
                self.stats.blocks_scanned += 1;
            } else {
                self.stats.blocks_skipped += 1;
            }
        }

        matching
    }

    /// Get blocks that might match a range predicate
    pub fn get_matching_blocks_range(
        &mut self,
        column_name: &str,
        op: RangeOp,
        value: &Value,
    ) -> Vec<u64> {
        let mut matching = Vec::new();

        for block in &self.blocks {
            self.stats.predicate_evaluations += 1;
            if block.might_match_range(column_name, op, value) {
                matching.push(block.block_id);
                self.stats.blocks_scanned += 1;
            } else {
                self.stats.blocks_skipped += 1;
            }
        }

        matching
    }

    /// Build zone maps from existing tuples
    pub fn build_from_tuples(&mut self, tuples: &[crate::Tuple], schema: &crate::Schema) {
        for (row_id, tuple) in tuples.iter().enumerate() {
            let values: Vec<(String, Value)> = schema.columns.iter()
                .zip(tuple.values.iter())
                .map(|(col, val)| (col.name.clone(), val.clone()))
                .collect();
            self.add_row(row_id as u64, &values);
        }
    }

    /// Get blocks that might match a BETWEEN predicate
    pub fn get_matching_blocks_between(
        &mut self,
        column_name: &str,
        low: &Value,
        high: &Value,
    ) -> Vec<u64> {
        let mut matching = Vec::new();

        for block in &self.blocks {
            self.stats.predicate_evaluations += 1;
            if block.might_match_between(column_name, low, high) {
                matching.push(block.block_id);
                self.stats.blocks_scanned += 1;
            } else {
                self.stats.blocks_skipped += 1;
            }
        }

        matching
    }

    /// Get blocks that might contain NULL values
    pub fn get_blocks_with_nulls(&mut self, column_name: &str) -> Vec<u64> {
        let mut matching = Vec::new();

        for block in &self.blocks {
            if block.might_contain_null(column_name) {
                matching.push(block.block_id);
            }
        }

        matching
    }

    /// Get blocks that might contain non-NULL values
    pub fn get_blocks_with_not_nulls(&mut self, column_name: &str) -> Vec<u64> {
        let mut matching = Vec::new();

        for block in &self.blocks {
            if block.might_contain_not_null(column_name) {
                matching.push(block.block_id);
            }
        }

        matching
    }

    /// Get row ID range for a block
    pub fn get_block_row_range(&self, block_id: u64) -> Option<(u64, u64)> {
        self.blocks
            .get(block_id as usize)
            .map(|b| (b.first_row_id, b.last_row_id))
    }

    /// Get statistics
    pub fn stats(&self) -> &ZoneMapStats {
        &self.stats
    }

    /// Reset statistics
    pub fn reset_stats(&mut self) {
        self.stats = ZoneMapStats {
            total_blocks: self.blocks.len() as u64,
            ..Default::default()
        };
    }

    /// Get memory usage estimate
    pub fn memory_usage(&self) -> usize {
        let mut total = std::mem::size_of::<Self>();
        for block in &self.blocks {
            total += std::mem::size_of::<BlockZoneMap>();
            for (name, czm) in &block.columns {
                total += name.len() + std::mem::size_of::<ColumnZoneMap>();
                // Estimate Value sizes
                if let Some(min) = &czm.range.min {
                    total += Self::estimate_value_size(min);
                }
                if let Some(max) = &czm.range.max {
                    total += Self::estimate_value_size(max);
                }
            }
        }
        total
    }

    fn estimate_value_size(value: &Value) -> usize {
        match value {
            Value::Null => 1,
            Value::Boolean(_) => 1,
            Value::Int2(_) => 2,
            Value::Int4(_) => 4,
            Value::Int8(_) => 8,
            Value::Float4(_) => 4,
            Value::Float8(_) => 8,
            Value::String(s) => s.len(),
            Value::Bytes(b) => b.len(),
            Value::Uuid(_) => 16,
            Value::Timestamp(_) => 12,
            Value::Date(_) => 4,
            Value::Time(_) => 8,
            Value::Numeric(s) => s.len(),
            Value::Json(j) => j.len(),
            Value::Array(arr) => arr.iter().map(Self::estimate_value_size).sum(),
            Value::Vector(v) => v.len() * 4,
            // Storage references
            Value::DictRef { .. } => 4,
            Value::CasRef { .. } => 32,
            Value::ColumnarRef => 1,
            Value::Interval(_) => 16, // Interval contains months, days, microseconds
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_range_basic() {
        let mut range = ValueRange::empty();

        range.update(&Value::Int8(10));
        range.update(&Value::Int8(5));
        range.update(&Value::Int8(15));

        assert_eq!(range.min, Some(Value::Int8(5)));
        assert_eq!(range.max, Some(Value::Int8(15)));
        assert_eq!(range.count, 3);
    }

    #[test]
    fn test_value_range_nulls() {
        let mut range = ValueRange::empty();

        range.update(&Value::Int8(10));
        range.update(&Value::Null);
        range.update(&Value::Int8(20));

        assert!(range.has_nulls);
        assert_eq!(range.null_count, 1);
        assert_eq!(range.min, Some(Value::Int8(10)));
        assert_eq!(range.max, Some(Value::Int8(20)));
    }

    #[test]
    fn test_value_range_might_contain() {
        let mut range = ValueRange::empty();
        range.update(&Value::Int8(10));
        range.update(&Value::Int8(20));

        assert!(range.might_contain(&Value::Int8(15)));
        assert!(range.might_contain(&Value::Int8(10)));
        assert!(range.might_contain(&Value::Int8(20)));
        assert!(!range.might_contain(&Value::Int8(5)));
        assert!(!range.might_contain(&Value::Int8(25)));
    }

    #[test]
    fn test_block_zone_map() {
        let mut bzm = BlockZoneMap::new(0, 0);

        bzm.add_row(0, &[
            ("id".to_string(), Value::Int8(1)),
            ("name".to_string(), Value::String("Alice".to_string())),
        ]);
        bzm.add_row(1, &[
            ("id".to_string(), Value::Int8(5)),
            ("name".to_string(), Value::String("Bob".to_string())),
        ]);

        assert!(bzm.might_contain("id", &Value::Int8(3)));
        assert!(!bzm.might_contain("id", &Value::Int8(10)));

        assert!(bzm.might_match_range("id", RangeOp::Gt, &Value::Int8(0)));
        assert!(!bzm.might_match_range("id", RangeOp::Gt, &Value::Int8(10)));
    }

    #[test]
    fn test_table_zone_map() {
        let mut tzm = TableZoneMap::new("users".to_string(), 10);

        for i in 0..25 {
            tzm.add_row(i, &[
                ("id".to_string(), Value::Int8(i as i64)),
                ("status".to_string(), Value::String(if i % 2 == 0 { "active" } else { "inactive" }.to_string())),
            ]);
        }

        // Should have 3 blocks (0-9, 10-19, 20-24)
        assert_eq!(tzm.blocks.len(), 3);

        // Test equality filtering
        let matching = tzm.get_matching_blocks_eq("id", &Value::Int8(5));
        assert!(matching.contains(&0)); // Block 0 has IDs 0-9

        // Test range filtering
        let matching = tzm.get_matching_blocks_range("id", RangeOp::Gt, &Value::Int8(15));
        assert!(matching.contains(&1)); // Block 1 has IDs 10-19
        assert!(matching.contains(&2)); // Block 2 has IDs 20-24
    }

    #[test]
    fn test_zone_map_between() {
        let mut tzm = TableZoneMap::new("test".to_string(), 10);

        for i in 0..30 {
            tzm.add_row(i, &[("val".to_string(), Value::Int8(i as i64))]);
        }

        let matching = tzm.get_matching_blocks_between(
            "val",
            &Value::Int8(5),
            &Value::Int8(15),
        );

        // Should match blocks 0 and 1 (values 0-9 and 10-19)
        assert!(matching.contains(&0));
        assert!(matching.contains(&1));
        assert!(!matching.contains(&2)); // Block 2 has values 20-29
    }

    #[test]
    fn test_zone_map_strings() {
        let mut tzm = TableZoneMap::new("test".to_string(), 5);

        tzm.add_row(0, &[("name".to_string(), Value::String("Alice".to_string()))]);
        tzm.add_row(1, &[("name".to_string(), Value::String("Bob".to_string()))]);
        tzm.add_row(2, &[("name".to_string(), Value::String("Charlie".to_string()))]);

        let matching = tzm.get_matching_blocks_eq("name", &Value::String("Bob".to_string()));
        assert!(!matching.is_empty());

        let matching = tzm.get_matching_blocks_eq("name", &Value::String("Zoe".to_string()));
        assert!(matching.is_empty()); // "Zoe" > "Charlie" (max)
    }
}
