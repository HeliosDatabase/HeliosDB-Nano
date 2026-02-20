//! Row-Level Result Cache
//!
//! High-performance LRU cache for frequently accessed rows with:
//! - Configurable TTL (time-to-live) per entry
//! - Table-level invalidation for write operations
//! - Memory-bounded with configurable max entries
//! - Cache hit/miss statistics
//!
//! # Performance Impact
//! - Expected 10-100x speedup for repeated single-row lookups
//! - Reduces RocksDB read amplification
//! - Automatic invalidation on INSERT/UPDATE/DELETE

use crate::Tuple;
use lru::LruCache;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

/// Cache key for row lookups
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RowCacheKey {
    /// Table name
    pub table: String,
    /// Row ID
    pub row_id: u64,
}

impl RowCacheKey {
    /// Create a new cache key
    pub fn new(table: impl Into<String>, row_id: u64) -> Self {
        Self {
            table: table.into(),
            row_id,
        }
    }
}

/// Cached row entry with metadata
#[derive(Debug, Clone)]
struct CachedRow {
    /// The cached tuple
    tuple: Tuple,
    /// When this entry was cached
    cached_at: Instant,
    /// Time-to-live for this entry
    ttl: Duration,
    /// Number of times this entry has been accessed
    access_count: u64,
}

impl CachedRow {
    /// Create a new cached row
    fn new(tuple: Tuple, ttl: Duration) -> Self {
        Self {
            tuple,
            cached_at: Instant::now(),
            ttl,
            access_count: 1,
        }
    }

    /// Check if this entry has expired
    fn is_expired(&self) -> bool {
        self.cached_at.elapsed() > self.ttl
    }

    /// Record an access and return the tuple
    fn access(&mut self) -> Tuple {
        self.access_count += 1;
        self.tuple.clone()
    }
}

/// Row cache statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RowCacheStats {
    /// Total cache lookups
    pub lookups: u64,
    /// Cache hits (found and not expired)
    pub hits: u64,
    /// Cache misses (not found)
    pub misses: u64,
    /// Expired entries encountered
    pub expirations: u64,
    /// Entries evicted due to capacity
    pub evictions: u64,
    /// Total entries inserted
    pub inserts: u64,
    /// Total invalidations
    pub invalidations: u64,
    /// Current entry count
    pub current_entries: u64,
    /// Peak entry count
    pub peak_entries: u64,
}

impl RowCacheStats {
    /// Calculate hit rate (0.0 to 1.0)
    pub fn hit_rate(&self) -> f64 {
        if self.lookups == 0 {
            0.0
        } else {
            self.hits as f64 / self.lookups as f64
        }
    }

    /// Calculate miss rate (0.0 to 1.0)
    pub fn miss_rate(&self) -> f64 {
        1.0 - self.hit_rate()
    }
}

/// Row cache configuration
#[derive(Debug, Clone)]
pub struct RowCacheConfig {
    /// Maximum number of entries in the cache
    pub max_entries: usize,
    /// Default TTL for cached entries
    pub default_ttl: Duration,
    /// Minimum TTL (for frequently updated tables)
    pub min_ttl: Duration,
    /// Maximum TTL (for stable tables)
    pub max_ttl: Duration,
    /// Whether to enable the cache
    pub enabled: bool,
}

impl Default for RowCacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            default_ttl: Duration::from_secs(60),
            min_ttl: Duration::from_secs(5),
            max_ttl: Duration::from_secs(300),
            enabled: true,
        }
    }
}

/// High-performance row cache with LRU eviction and TTL
pub struct RowCache {
    /// LRU cache storage
    cache: RwLock<LruCache<RowCacheKey, CachedRow>>,
    /// Tables with active invalidation (recently written)
    hot_tables: RwLock<HashSet<String>>,
    /// Last time hot_tables was reset (auto-resets every 60s)
    hot_tables_last_reset: RwLock<Instant>,
    /// Configuration
    config: RowCacheConfig,
    /// Statistics
    stats: RwLock<RowCacheStats>,
}

impl RowCache {
    /// Create a new row cache with default configuration
    pub fn new() -> Self {
        Self::with_config(RowCacheConfig::default())
    }

    /// Create a row cache with custom configuration
    pub fn with_config(config: RowCacheConfig) -> Self {
        // SAFETY: 1 is always non-zero
        let cache_size = NonZeroUsize::new(config.max_entries.max(1))
            .unwrap_or(NonZeroUsize::MIN);

        Self {
            cache: RwLock::new(LruCache::new(cache_size)),
            hot_tables: RwLock::new(HashSet::new()),
            hot_tables_last_reset: RwLock::new(Instant::now()),
            config,
            stats: RwLock::new(RowCacheStats::default()),
        }
    }

    /// Create a row cache with specified capacity
    pub fn with_capacity(max_entries: usize) -> Self {
        Self::with_config(RowCacheConfig {
            max_entries,
            ..Default::default()
        })
    }

    /// Get a cached row by key
    ///
    /// Returns `Some(Tuple)` if found and not expired, `None` otherwise.
    pub fn get(&self, table: &str, row_id: u64) -> Option<Tuple> {
        if !self.config.enabled {
            return None;
        }

        let key = RowCacheKey::new(table, row_id);

        // Fast path: read lock for lookup
        {
            let mut stats = self.stats.write();
            stats.lookups += 1;
        }

        let mut cache = self.cache.write();

        if let Some(entry) = cache.get_mut(&key) {
            if entry.is_expired() {
                // Entry expired, remove it
                cache.pop(&key);
                let mut stats = self.stats.write();
                stats.expirations += 1;
                stats.current_entries = cache.len() as u64;
                return None;
            }

            // Cache hit
            let tuple = entry.access();
            let mut stats = self.stats.write();
            stats.hits += 1;
            return Some(tuple);
        }

        // Cache miss
        let mut stats = self.stats.write();
        stats.misses += 1;
        None
    }

    /// Insert a row into the cache
    pub fn put(&self, table: &str, row_id: u64, tuple: Tuple) {
        if !self.config.enabled {
            return;
        }

        let key = RowCacheKey::new(table, row_id);

        // Determine TTL based on table hotness
        let ttl = self.get_ttl_for_table(table);

        let mut cache = self.cache.write();

        // Check if we're at capacity (LRU will handle eviction)
        let was_full = cache.len() >= self.config.max_entries;

        cache.put(key, CachedRow::new(tuple, ttl));

        let mut stats = self.stats.write();
        stats.inserts += 1;
        stats.current_entries = cache.len() as u64;
        if stats.current_entries > stats.peak_entries {
            stats.peak_entries = stats.current_entries;
        }
        if was_full {
            stats.evictions += 1;
        }
    }

    /// Invalidate a specific row
    pub fn invalidate(&self, table: &str, row_id: u64) {
        if !self.config.enabled {
            return;
        }

        let key = RowCacheKey::new(table, row_id);

        let mut cache = self.cache.write();
        if cache.pop(&key).is_some() {
            let mut stats = self.stats.write();
            stats.invalidations += 1;
            stats.current_entries = cache.len() as u64;
        }

        // Mark table as hot
        self.mark_table_hot(table);
    }

    /// Invalidate all cached rows for a table
    pub fn invalidate_table(&self, table: &str) {
        if !self.config.enabled {
            return;
        }

        let mut cache = self.cache.write();
        let mut stats = self.stats.write();

        // Collect keys to remove (can't modify while iterating)
        let keys_to_remove: Vec<RowCacheKey> = cache
            .iter()
            .filter(|(k, _)| k.table == table)
            .map(|(k, _)| k.clone())
            .collect();

        let removed_count = keys_to_remove.len();
        for key in keys_to_remove {
            cache.pop(&key);
        }

        stats.invalidations += removed_count as u64;
        stats.current_entries = cache.len() as u64;

        // Mark table as hot
        drop(cache);
        drop(stats);
        self.mark_table_hot(table);
    }

    /// Clear all cached entries
    pub fn clear(&self) {
        let mut cache = self.cache.write();
        let count = cache.len();
        cache.clear();

        let mut stats = self.stats.write();
        stats.invalidations += count as u64;
        stats.current_entries = 0;
    }

    /// Get cache statistics
    pub fn stats(&self) -> RowCacheStats {
        self.stats.read().clone()
    }

    /// Reset statistics
    pub fn reset_stats(&self) {
        let current_entries = self.cache.read().len() as u64;
        let mut stats = self.stats.write();
        *stats = RowCacheStats {
            current_entries,
            peak_entries: current_entries,
            ..Default::default()
        };
    }

    /// Check if cache is enabled
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Enable or disable the cache
    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
        if !enabled {
            self.clear();
        }
    }

    /// Get current entry count
    pub fn len(&self) -> usize {
        self.cache.read().len()
    }

    /// Check if cache is empty
    pub fn is_empty(&self) -> bool {
        self.cache.read().is_empty()
    }

    /// Mark a table as "hot" (recently written to).
    /// Auto-resets the hot set every 60 seconds to prevent unbounded growth.
    fn mark_table_hot(&self, table: &str) {
        let should_reset = self.hot_tables_last_reset.read().elapsed() > Duration::from_secs(60);
        if should_reset {
            let mut hot_tables = self.hot_tables.write();
            hot_tables.clear();
            hot_tables.insert(table.to_string());
            *self.hot_tables_last_reset.write() = Instant::now();
        } else {
            let mut hot_tables = self.hot_tables.write();
            hot_tables.insert(table.to_string());
        }
    }

    /// Get TTL for a table based on its hotness
    fn get_ttl_for_table(&self, table: &str) -> Duration {
        let hot_tables = self.hot_tables.read();
        if hot_tables.contains(table) {
            // Hot table - use shorter TTL
            self.config.min_ttl
        } else {
            // Cold table - use default TTL
            self.config.default_ttl
        }
    }

    /// Clear hot table markers (call periodically)
    pub fn reset_hot_tables(&self) {
        let mut hot_tables = self.hot_tables.write();
        hot_tables.clear();
    }
}

impl Default for RowCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Value;

    fn make_tuple(id: i32, name: &str) -> Tuple {
        Tuple::new(vec![
            Value::Int4(id),
            Value::String(name.to_string()),
        ])
    }

    #[test]
    fn test_basic_cache_operations() {
        let cache = RowCache::new();

        // Insert a row
        cache.put("users", 1, make_tuple(1, "Alice"));

        // Get the row back
        let result = cache.get("users", 1);
        assert!(result.is_some());

        let tuple = result.unwrap();
        assert_eq!(tuple.values.len(), 2);

        // Miss for non-existent row
        assert!(cache.get("users", 999).is_none());

        // Stats check
        let stats = cache.stats();
        assert_eq!(stats.inserts, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
    }

    #[test]
    fn test_cache_invalidation() {
        let cache = RowCache::new();

        cache.put("users", 1, make_tuple(1, "Alice"));
        cache.put("users", 2, make_tuple(2, "Bob"));
        cache.put("orders", 1, make_tuple(100, "Order1"));

        // Single row invalidation
        cache.invalidate("users", 1);
        assert!(cache.get("users", 1).is_none());
        assert!(cache.get("users", 2).is_some());

        // Table invalidation
        cache.invalidate_table("users");
        assert!(cache.get("users", 2).is_none());
        assert!(cache.get("orders", 1).is_some());
    }

    #[test]
    fn test_cache_ttl() {
        let config = RowCacheConfig {
            default_ttl: Duration::from_millis(50),
            ..Default::default()
        };
        let cache = RowCache::with_config(config);

        cache.put("test", 1, make_tuple(1, "Test"));
        assert!(cache.get("test", 1).is_some());

        // Wait for TTL to expire
        std::thread::sleep(Duration::from_millis(100));

        // Should be expired now
        assert!(cache.get("test", 1).is_none());

        let stats = cache.stats();
        assert_eq!(stats.expirations, 1);
    }

    #[test]
    fn test_cache_capacity() {
        let cache = RowCache::with_capacity(3);

        cache.put("t", 1, make_tuple(1, "One"));
        cache.put("t", 2, make_tuple(2, "Two"));
        cache.put("t", 3, make_tuple(3, "Three"));
        cache.put("t", 4, make_tuple(4, "Four")); // Should evict row 1

        assert_eq!(cache.len(), 3);

        let stats = cache.stats();
        assert!(stats.evictions >= 1);
    }

    #[test]
    fn test_hit_rate() {
        let cache = RowCache::new();

        cache.put("t", 1, make_tuple(1, "One"));

        // 3 hits
        cache.get("t", 1);
        cache.get("t", 1);
        cache.get("t", 1);

        // 1 miss
        cache.get("t", 999);

        let stats = cache.stats();
        assert_eq!(stats.hits, 3);
        assert_eq!(stats.misses, 1);
        assert!((stats.hit_rate() - 0.75).abs() < 0.01);
    }
}
