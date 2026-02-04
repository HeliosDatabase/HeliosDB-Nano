//! Bloom Filter Implementation for Storage-Level Filtering
//!
//! This module implements a probabilistic data structure for fast negative lookups.
//! Bloom filters are used to quickly determine if a value is definitely NOT in a set,
//! avoiding unnecessary disk reads during table scans.
//!
//! Key features:
//! - Configurable false positive rate
//! - Multiple hash functions using Kirsch-Mitzenmacher optimization
//! - Efficient bit vector storage
//! - Serialization support for persistence

use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::Value;

/// Bloom filter configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BloomFilterConfig {
    /// Expected number of elements
    pub expected_elements: usize,
    /// Desired false positive rate (0.0 - 1.0)
    pub false_positive_rate: f64,
}

impl Default for BloomFilterConfig {
    fn default() -> Self {
        Self {
            expected_elements: 10000,
            false_positive_rate: 0.01, // 1% false positive rate
        }
    }
}

impl BloomFilterConfig {
    /// Create a new bloom filter config
    pub fn new(expected_elements: usize, false_positive_rate: f64) -> Self {
        Self {
            expected_elements,
            false_positive_rate: false_positive_rate.clamp(0.0001, 0.5),
        }
    }

    /// Calculate optimal bit array size
    /// m = -n * ln(p) / (ln(2)^2)
    pub fn optimal_bits(&self) -> usize {
        let n = self.expected_elements as f64;
        let p = self.false_positive_rate;
        let ln2_squared = std::f64::consts::LN_2 * std::f64::consts::LN_2;
        let m = -(n * p.ln()) / ln2_squared;
        (m.ceil() as usize).max(64)
    }

    /// Calculate optimal number of hash functions
    /// k = (m/n) * ln(2)
    pub fn optimal_hash_count(&self) -> usize {
        let m = self.optimal_bits() as f64;
        let n = self.expected_elements as f64;
        let k = (m / n) * std::f64::consts::LN_2;
        (k.ceil() as usize).clamp(1, 16)
    }
}

/// Statistics for bloom filter performance
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BloomFilterStats {
    /// Total lookups performed
    pub lookups: u64,
    /// True positives (element found and actually exists)
    pub true_positives: u64,
    /// True negatives (element not found and doesn't exist)
    pub true_negatives: u64,
    /// False positives (element found but doesn't exist)
    pub false_positives: u64,
    /// Elements inserted
    pub insertions: u64,
    /// Current estimated fill ratio
    pub fill_ratio: f64,
}

impl BloomFilterStats {
    /// Calculate actual false positive rate
    pub fn actual_fpr(&self) -> f64 {
        let total_negatives = self.true_negatives + self.false_positives;
        if total_negatives == 0 {
            0.0
        } else {
            self.false_positives as f64 / total_negatives as f64
        }
    }

    /// Calculate hit rate (useful for cache-like behavior)
    pub fn hit_rate(&self) -> f64 {
        if self.lookups == 0 {
            0.0
        } else {
            self.true_positives as f64 / self.lookups as f64
        }
    }
}

/// Bloom filter for fast set membership testing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BloomFilter {
    /// Bit vector (stored as bytes for efficiency)
    bits: Vec<u64>,
    /// Number of bits in the filter
    num_bits: usize,
    /// Number of hash functions
    num_hashes: usize,
    /// Configuration used to create this filter
    config: BloomFilterConfig,
    /// Statistics
    stats: BloomFilterStats,
}

impl BloomFilter {
    /// Create a new bloom filter with the given configuration
    pub fn new(config: BloomFilterConfig) -> Self {
        let num_bits = config.optimal_bits();
        let num_hashes = config.optimal_hash_count();
        let num_words = (num_bits + 63) / 64;

        Self {
            bits: vec![0u64; num_words],
            num_bits,
            num_hashes,
            config,
            stats: BloomFilterStats::default(),
        }
    }

    /// Create with default configuration
    pub fn with_defaults() -> Self {
        Self::new(BloomFilterConfig::default())
    }

    /// Create for a specific expected element count
    pub fn for_elements(expected: usize) -> Self {
        Self::new(BloomFilterConfig::new(expected, 0.01))
    }

    /// Insert a value into the bloom filter
    pub fn insert<T: Hash>(&mut self, value: &T) {
        let (h1, h2) = self.hash_pair(value);

        for i in 0..self.num_hashes {
            let bit_index = self.get_bit_index(h1, h2, i);
            self.set_bit(bit_index);
        }

        self.stats.insertions += 1;
        self.update_fill_ratio();
    }

    /// Insert a Value type
    pub fn insert_value(&mut self, value: &Value) {
        match value {
            Value::Null => self.insert(&0u8),
            Value::Boolean(b) => self.insert(b),
            Value::Int2(i) => self.insert(i),
            Value::Int4(i) => self.insert(i),
            Value::Int8(i) => self.insert(i),
            Value::Float4(f) => self.insert(&f.to_bits()),
            Value::Float8(f) => self.insert(&f.to_bits()),
            Value::Numeric(s) => self.insert(s),
            Value::String(s) => self.insert(s),
            Value::Bytes(b) => self.insert(b),
            Value::Uuid(u) => self.insert(&u.to_string()),
            Value::Timestamp(t) => self.insert(&t.timestamp_nanos_opt().unwrap_or(0)),
            Value::Date(d) => self.insert(&d.to_string()),
            Value::Time(t) => self.insert(&t.to_string()),
            Value::Json(j) => self.insert(j),
            Value::Array(arr) => {
                // Hash array elements together
                let mut hasher = DefaultHasher::new();
                for v in arr {
                    self.hash_value(v, &mut hasher);
                }
                let h = hasher.finish();
                self.insert(&h);
            }
            Value::Vector(v) => {
                // Hash vector elements
                let mut hasher = DefaultHasher::new();
                for f in v {
                    f.to_bits().hash(&mut hasher);
                }
                let h = hasher.finish();
                self.insert(&h);
            }
            // Storage references (hash the reference, not the underlying value)
            Value::DictRef { dict_id } => self.insert(dict_id),
            Value::CasRef { hash } => self.insert(hash),
            Value::ColumnarRef => self.insert(&0u8),
            Value::Interval(iv) => self.insert(iv), // Hash interval microseconds
        }
    }

    /// Check if a value might be in the set
    /// Returns false if definitely not in set, true if possibly in set
    pub fn might_contain<T: Hash>(&mut self, value: &T) -> bool {
        self.stats.lookups += 1;
        let (h1, h2) = self.hash_pair(value);

        for i in 0..self.num_hashes {
            let bit_index = self.get_bit_index(h1, h2, i);
            if !self.get_bit(bit_index) {
                self.stats.true_negatives += 1;
                return false;
            }
        }

        // All bits set - might contain (could be false positive)
        true
    }

    /// Check if a Value might be in the set
    pub fn might_contain_value(&mut self, value: &Value) -> bool {
        match value {
            Value::Null => self.might_contain(&0u8),
            Value::Boolean(b) => self.might_contain(b),
            Value::Int2(i) => self.might_contain(i),
            Value::Int4(i) => self.might_contain(i),
            Value::Int8(i) => self.might_contain(i),
            Value::Float4(f) => self.might_contain(&f.to_bits()),
            Value::Float8(f) => self.might_contain(&f.to_bits()),
            Value::Numeric(s) => self.might_contain(s),
            Value::String(s) => self.might_contain(s),
            Value::Bytes(b) => self.might_contain(b),
            Value::Uuid(u) => self.might_contain(&u.to_string()),
            Value::Timestamp(t) => self.might_contain(&t.timestamp_nanos_opt().unwrap_or(0)),
            Value::Date(d) => self.might_contain(&d.to_string()),
            Value::Time(t) => self.might_contain(&t.to_string()),
            Value::Json(j) => self.might_contain(j),
            Value::Array(arr) => {
                let mut hasher = DefaultHasher::new();
                for v in arr {
                    self.hash_value(v, &mut hasher);
                }
                let h = hasher.finish();
                self.might_contain(&h)
            }
            Value::Vector(v) => {
                let mut hasher = DefaultHasher::new();
                for f in v {
                    f.to_bits().hash(&mut hasher);
                }
                let h = hasher.finish();
                self.might_contain(&h)
            }
            // Storage references
            Value::DictRef { dict_id } => self.might_contain(dict_id),
            Value::CasRef { hash } => self.might_contain(hash),
            Value::ColumnarRef => self.might_contain(&0u8),
            Value::Interval(iv) => self.might_contain(iv), // Check interval microseconds
        }
    }

    /// Immutable check if a value might be in the set (no stats update)
    pub fn check<T: Hash>(&self, value: &T) -> bool {
        let (h1, h2) = self.hash_pair(value);

        for i in 0..self.num_hashes {
            let bit_index = self.get_bit_index(h1, h2, i);
            if !self.get_bit(bit_index) {
                return false;
            }
        }
        true
    }

    /// Immutable check if a Value might be in the set (no stats update)
    pub fn check_value(&self, value: &Value) -> bool {
        match value {
            Value::Null => self.check(&0u8),
            Value::Boolean(b) => self.check(b),
            Value::Int2(i) => self.check(i),
            Value::Int4(i) => self.check(i),
            Value::Int8(i) => self.check(i),
            Value::Float4(f) => self.check(&f.to_bits()),
            Value::Float8(f) => self.check(&f.to_bits()),
            Value::Numeric(s) => self.check(s),
            Value::String(s) => self.check(s),
            Value::Bytes(b) => self.check(b),
            Value::Uuid(u) => self.check(&u.to_string()),
            Value::Timestamp(t) => self.check(&t.timestamp_nanos_opt().unwrap_or(0)),
            Value::Date(d) => self.check(&d.to_string()),
            Value::Time(t) => self.check(&t.to_string()),
            Value::Json(j) => self.check(j),
            Value::Array(arr) => {
                let mut hasher = DefaultHasher::new();
                for v in arr {
                    self.hash_value(v, &mut hasher);
                }
                let h = hasher.finish();
                self.check(&h)
            }
            Value::Vector(v) => {
                let mut hasher = DefaultHasher::new();
                for f in v {
                    f.to_bits().hash(&mut hasher);
                }
                let h = hasher.finish();
                self.check(&h)
            }
            // Storage references
            Value::DictRef { dict_id } => self.check(dict_id),
            Value::CasRef { hash } => self.check(hash),
            Value::ColumnarRef => self.check(&0u8),
            Value::Interval(iv) => self.check(iv), // Check interval microseconds
        }
    }

    /// Record a true positive (for statistics tracking)
    pub fn record_true_positive(&mut self) {
        self.stats.true_positives += 1;
    }

    /// Record a false positive (for statistics tracking)
    pub fn record_false_positive(&mut self) {
        self.stats.false_positives += 1;
    }

    /// Get statistics
    pub fn stats(&self) -> &BloomFilterStats {
        &self.stats
    }

    /// Get configuration
    pub fn config(&self) -> &BloomFilterConfig {
        &self.config
    }

    /// Get number of bits
    pub fn num_bits(&self) -> usize {
        self.num_bits
    }

    /// Get number of hash functions
    pub fn num_hashes(&self) -> usize {
        self.num_hashes
    }

    /// Get memory usage in bytes
    pub fn memory_usage(&self) -> usize {
        self.bits.len() * 8
    }

    /// Clear the bloom filter
    pub fn clear(&mut self) {
        self.bits.fill(0);
        self.stats = BloomFilterStats::default();
    }

    /// Apply delta bits - OR the bits_mask to the word at word_idx
    pub fn apply_delta_bits(&mut self, word_idx: usize, bits_mask: u64) {
        if word_idx < self.bits.len() {
            self.bits[word_idx] |= bits_mask;
        }
    }

    /// Get the number of words in the bit vector
    pub fn bits_len(&self) -> usize {
        self.bits.len()
    }

    /// Increment items added count
    pub fn increment_items_added(&mut self, count: usize) {
        self.stats.insertions += count as u64;
    }

    /// Merge another bloom filter (union)
    pub fn merge(&mut self, other: &BloomFilter) -> Result<(), &'static str> {
        if self.num_bits != other.num_bits || self.num_hashes != other.num_hashes {
            return Err("Cannot merge bloom filters with different configurations");
        }

        for (i, word) in other.bits.iter().enumerate() {
            self.bits[i] |= word;
        }

        self.stats.insertions += other.stats.insertions;
        self.update_fill_ratio();
        Ok(())
    }

    // Internal helper methods

    /// Hash a value to get two hash values for Kirsch-Mitzenmacher optimization
    fn hash_pair<T: Hash>(&self, value: &T) -> (u64, u64) {
        let mut hasher1 = DefaultHasher::new();
        value.hash(&mut hasher1);
        let h1 = hasher1.finish();

        let mut hasher2 = DefaultHasher::new();
        h1.hash(&mut hasher2);
        value.hash(&mut hasher2);
        let h2 = hasher2.finish();

        (h1, h2)
    }

    /// Get bit index using Kirsch-Mitzenmacher formula: h(i) = h1 + i*h2
    fn get_bit_index(&self, h1: u64, h2: u64, i: usize) -> usize {
        let combined = h1.wrapping_add((i as u64).wrapping_mul(h2));
        (combined as usize) % self.num_bits
    }

    /// Set a bit in the filter
    fn set_bit(&mut self, index: usize) {
        let word_index = index / 64;
        let bit_index = index % 64;
        self.bits[word_index] |= 1u64 << bit_index;
    }

    /// Get a bit from the filter
    fn get_bit(&self, index: usize) -> bool {
        let word_index = index / 64;
        let bit_index = index % 64;
        (self.bits[word_index] >> bit_index) & 1 == 1
    }

    /// Update fill ratio statistic
    fn update_fill_ratio(&mut self) {
        let set_bits: usize = self.bits.iter().map(|w| w.count_ones() as usize).sum();
        self.stats.fill_ratio = set_bits as f64 / self.num_bits as f64;
    }

    /// Hash a Value into a hasher
    fn hash_value(&self, value: &Value, hasher: &mut DefaultHasher) {
        match value {
            Value::Null => 0u8.hash(hasher),
            Value::Boolean(b) => b.hash(hasher),
            Value::Int2(i) => i.hash(hasher),
            Value::Int4(i) => i.hash(hasher),
            Value::Int8(i) => i.hash(hasher),
            Value::Float4(f) => f.to_bits().hash(hasher),
            Value::Float8(f) => f.to_bits().hash(hasher),
            Value::Numeric(s) => s.hash(hasher),
            Value::String(s) => s.hash(hasher),
            Value::Bytes(b) => b.hash(hasher),
            Value::Uuid(u) => u.to_string().hash(hasher),
            Value::Timestamp(t) => t.timestamp_nanos_opt().unwrap_or(0).hash(hasher),
            Value::Date(d) => d.to_string().hash(hasher),
            Value::Time(t) => t.to_string().hash(hasher),
            Value::Json(j) => j.hash(hasher),
            Value::Array(arr) => {
                for v in arr {
                    self.hash_value(v, hasher);
                }
            }
            Value::Vector(v) => {
                for f in v {
                    f.to_bits().hash(hasher);
                }
            }
            // Storage references
            Value::DictRef { dict_id } => dict_id.hash(hasher),
            Value::CasRef { hash } => hash.hash(hasher),
            Value::ColumnarRef => 0u8.hash(hasher),
            Value::Interval(iv) => iv.hash(hasher), // Hash interval microseconds
        }
    }
}

/// Column bloom filter - tracks distinct values in a column
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnBloomFilter {
    /// Column name
    pub column_name: String,
    /// The bloom filter
    pub filter: BloomFilter,
    /// Number of rows tracked
    pub row_count: u64,
}

impl ColumnBloomFilter {
    /// Create a new column bloom filter
    pub fn new(column_name: String, expected_distinct: usize) -> Self {
        Self {
            column_name,
            filter: BloomFilter::for_elements(expected_distinct),
            row_count: 0,
        }
    }

    /// Add a value from a row
    pub fn add(&mut self, value: &Value) {
        self.filter.insert_value(value);
        self.row_count += 1;
    }

    /// Check if a value might exist in the column
    pub fn might_contain(&mut self, value: &Value) -> bool {
        self.filter.might_contain_value(value)
    }

    /// Immutable check if a value might exist in the column (no stats update)
    pub fn might_contain_check(&self, value: &Value) -> bool {
        self.filter.check_value(value)
    }
}

/// Block-level bloom filter for row existence checks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockBloomFilter {
    /// Block/page identifier
    pub block_id: u64,
    /// Row ID bloom filter
    pub row_filter: BloomFilter,
    /// Column-specific bloom filters
    pub column_filters: Vec<ColumnBloomFilter>,
}

impl BlockBloomFilter {
    /// Create a new block bloom filter
    pub fn new(block_id: u64, expected_rows: usize) -> Self {
        Self {
            block_id,
            row_filter: BloomFilter::for_elements(expected_rows),
            column_filters: Vec::new(),
        }
    }

    /// Add a column filter
    pub fn add_column_filter(&mut self, column_name: String, expected_distinct: usize) {
        self.column_filters.push(ColumnBloomFilter::new(column_name, expected_distinct));
    }

    /// Add a row ID
    pub fn add_row(&mut self, row_id: u64) {
        self.row_filter.insert(&row_id);
    }

    /// Check if a row might exist
    pub fn might_contain_row(&mut self, row_id: u64) -> bool {
        self.row_filter.might_contain(&row_id)
    }

    /// Check if a value might exist in a column
    pub fn might_contain_in_column(&mut self, column_name: &str, value: &Value) -> bool {
        for cf in &mut self.column_filters {
            if cf.column_name == column_name {
                return cf.might_contain(value);
            }
        }
        // If no filter for this column, assume it might contain
        true
    }
}

/// Table-level bloom filter manager
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableBloomFilters {
    /// Table name
    pub table_name: String,
    /// Per-column bloom filters for equality predicates
    pub column_filters: Vec<ColumnBloomFilter>,
    /// Block-level filters
    pub block_filters: Vec<BlockBloomFilter>,
    /// Global row ID filter
    pub row_id_filter: BloomFilter,
}

impl TableBloomFilters {
    /// Create new table bloom filters
    pub fn new(table_name: String, expected_rows: usize) -> Self {
        Self {
            table_name,
            column_filters: Vec::new(),
            block_filters: Vec::new(),
            row_id_filter: BloomFilter::for_elements(expected_rows),
        }
    }

    /// Add a column to track
    pub fn add_column(&mut self, column_name: String, expected_distinct: usize) {
        self.column_filters.push(ColumnBloomFilter::new(column_name, expected_distinct));
    }

    /// Index a row
    pub fn index_row(&mut self, row_id: u64, values: &[(String, Value)]) {
        self.row_id_filter.insert(&row_id);

        for (col_name, value) in values {
            for cf in &mut self.column_filters {
                if &cf.column_name == col_name {
                    cf.add(value);
                    break;
                }
            }
        }
    }

    /// Check if a value might exist in a column (for equality predicates)
    pub fn might_contain_value(&mut self, column_name: &str, value: &Value) -> bool {
        for cf in &mut self.column_filters {
            if cf.column_name == column_name {
                return cf.might_contain(value);
            }
        }
        // No filter for this column - might contain
        true
    }

    /// Get memory usage
    pub fn memory_usage(&self) -> usize {
        let mut total = self.row_id_filter.memory_usage();
        for cf in &self.column_filters {
            total += cf.filter.memory_usage();
        }
        for bf in &self.block_filters {
            total += bf.row_filter.memory_usage();
            for cf in &bf.column_filters {
                total += cf.filter.memory_usage();
            }
        }
        total
    }

    /// Build bloom filters from existing tuples
    pub fn build_from_tuples(&mut self, tuples: &[crate::Tuple], schema: &crate::Schema) {
        // Add columns from schema if not already present
        for col in &schema.columns {
            if !self.column_filters.iter().any(|cf| cf.column_name == col.name) {
                self.add_column(col.name.clone(), tuples.len().max(1000));
            }
        }

        // Index each tuple
        for (row_id, tuple) in tuples.iter().enumerate() {
            let values: Vec<(String, Value)> = schema.columns.iter()
                .zip(tuple.values.iter())
                .map(|(col, val)| (col.name.clone(), val.clone()))
                .collect();
            self.index_row(row_id as u64, &values);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bloom_filter_basic() {
        let mut bf = BloomFilter::for_elements(1000);

        bf.insert(&"hello");
        bf.insert(&"world");
        bf.insert(&42i64);

        assert!(bf.might_contain(&"hello"));
        assert!(bf.might_contain(&"world"));
        assert!(bf.might_contain(&42i64));

        // These should likely not be found (with very high probability)
        // Note: bloom filters can have false positives, so we just check the logic works
    }

    #[test]
    fn test_bloom_filter_value_types() {
        let mut bf = BloomFilter::for_elements(100);

        bf.insert_value(&Value::Int8(42));
        bf.insert_value(&Value::String("test".to_string()));
        bf.insert_value(&Value::Boolean(true));

        assert!(bf.might_contain_value(&Value::Int8(42)));
        assert!(bf.might_contain_value(&Value::String("test".to_string())));
        assert!(bf.might_contain_value(&Value::Boolean(true)));
    }

    #[test]
    fn test_bloom_filter_config() {
        let config = BloomFilterConfig::new(10000, 0.01);
        let bits = config.optimal_bits();
        let hashes = config.optimal_hash_count();

        // For 10000 elements at 1% FPR, we expect roughly 96000 bits and 7 hash functions
        assert!(bits > 50000);
        assert!(hashes >= 5 && hashes <= 10);
    }

    #[test]
    fn test_bloom_filter_merge() {
        let mut bf1 = BloomFilter::for_elements(100);
        let mut bf2 = BloomFilter::new(bf1.config().clone());

        bf1.insert(&"a");
        bf1.insert(&"b");
        bf2.insert(&"c");
        bf2.insert(&"d");

        bf1.merge(&bf2).unwrap();

        assert!(bf1.might_contain(&"a"));
        assert!(bf1.might_contain(&"b"));
        assert!(bf1.might_contain(&"c"));
        assert!(bf1.might_contain(&"d"));
    }

    #[test]
    fn test_column_bloom_filter() {
        let mut cbf = ColumnBloomFilter::new("status".to_string(), 10);

        cbf.add(&Value::String("active".to_string()));
        cbf.add(&Value::String("inactive".to_string()));

        assert!(cbf.might_contain(&Value::String("active".to_string())));
        assert!(cbf.might_contain(&Value::String("inactive".to_string())));
    }

    #[test]
    fn test_table_bloom_filters() {
        let mut tbf = TableBloomFilters::new("users".to_string(), 1000);
        tbf.add_column("status".to_string(), 5);
        tbf.add_column("name".to_string(), 500);

        tbf.index_row(1, &[
            ("status".to_string(), Value::String("active".to_string())),
            ("name".to_string(), Value::String("Alice".to_string())),
        ]);

        assert!(tbf.might_contain_value("status", &Value::String("active".to_string())));
        assert!(tbf.might_contain_value("name", &Value::String("Alice".to_string())));
    }
}
