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
use std::sync::Arc;

use lru::LruCache;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// The cached tuple, shared via `Arc` so a cache hit can hand out a cheap
    /// refcount bump (`get_arc`) instead of deep-copying the whole
    /// `Vec<Value>` — the win for index-nested-loop joins, where the right row
    /// is only borrowed to build the combined output tuple.
    tuple: Arc<Tuple>,
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
        Self::from_arc(Arc::new(tuple), ttl)
    }

    /// Create a cached row from an already-shared tuple (no deep copy).
    fn from_arc(tuple: Arc<Tuple>, ttl: Duration) -> Self {
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

    /// Record an access and return a shared handle to the tuple.
    fn access(&mut self) -> Arc<Tuple> {
        self.access_count += 1;
        Arc::clone(&self.tuple)
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

/// Number of independent LRU shards (R-D3). Power of two so the shard index is
/// a mask. Sharding removes the single global cache lock that every committer
/// took per written row at commit time (the row-cache invalidation fence),
/// which convoyed concurrent writers to different rows.
const ROW_CACHE_SHARDS: usize = 16;

/// One independently-locked LRU partition of the row cache.
struct CacheShard {
    cache: RwLock<LruCache<RowCacheKey, CachedRow>>,
}

/// High-performance row cache with LRU eviction and TTL.
///
/// R-D3: storage is sharded across [`ROW_CACHE_SHARDS`] independently-locked
/// LRU partitions keyed by a hash of `(table, row_id)`, and the write-path
/// counters (inserts/evictions/invalidations/peak) are lock-free atomics — so
/// neither a point-read nor a commit-time invalidation of a written row takes a
/// process-global lock. Read-path counters were already atomics (P0#4).
pub struct RowCache {
    /// LRU storage, partitioned by key hash.
    shards: Box<[CacheShard]>,
    /// `shards.len() - 1`, used to mask a key hash to a shard index.
    shard_mask: usize,
    /// Tables with active invalidation (recently written)
    hot_tables: RwLock<HashSet<String>>,
    /// Last time hot_tables was reset (auto-resets every 60s)
    hot_tables_last_reset: RwLock<Instant>,
    /// Configuration
    config: RowCacheConfig,
    /// Write-path counters — atomics (R-D3) so `put`/`invalidate` take no global
    /// stats lock. Merged into `stats()` on demand alongside the read counters.
    stat_inserts: AtomicU64,
    stat_evictions: AtomicU64,
    stat_invalidations: AtomicU64,
    stat_peak_entries: AtomicU64,
    /// Hot per-lookup counters — atomics so the read path needs no stats lock
    /// (P0#4). Merged into `stats()` on demand.
    hot_lookups: AtomicU64,
    hot_hits: AtomicU64,
    hot_misses: AtomicU64,
    hot_expirations: AtomicU64,
}

impl RowCache {
    /// Create a new row cache with default configuration
    pub fn new() -> Self {
        Self::with_config(RowCacheConfig::default())
    }

    /// Create a row cache with custom configuration
    pub fn with_config(config: RowCacheConfig) -> Self {
        // Shard only when the capacity is large enough that each shard still
        // holds a meaningful slice; a tiny cache stays a single exact LRU (so
        // small configured capacities keep precise total-capacity eviction).
        // Both branches are powers of two, so the shard index is a mask.
        let num_shards = if config.max_entries >= ROW_CACHE_SHARDS {
            ROW_CACHE_SHARDS
        } else {
            1
        };
        let per_shard = (config.max_entries / num_shards).max(1);
        let per_shard = NonZeroUsize::new(per_shard).unwrap_or(NonZeroUsize::MIN);
        let shards = (0..num_shards)
            .map(|_| CacheShard {
                cache: RwLock::new(LruCache::new(per_shard)),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            shards,
            shard_mask: num_shards - 1,
            hot_tables: RwLock::new(HashSet::new()),
            hot_tables_last_reset: RwLock::new(Instant::now()),
            config,
            stat_inserts: AtomicU64::new(0),
            stat_evictions: AtomicU64::new(0),
            stat_invalidations: AtomicU64::new(0),
            stat_peak_entries: AtomicU64::new(0),
            hot_lookups: AtomicU64::new(0),
            hot_hits: AtomicU64::new(0),
            hot_misses: AtomicU64::new(0),
            hot_expirations: AtomicU64::new(0),
        }
    }

    /// Route a key to its shard by an FNV-1a hash of `(table, row_id)`.
    fn shard_for(&self, key: &RowCacheKey) -> &RwLock<LruCache<RowCacheKey, CachedRow>> {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for b in key.table.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= key.row_id;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        &self.shards[(h as usize) & self.shard_mask].cache
    }

    /// Current live entry count across all shards.
    fn total_len(&self) -> u64 {
        self.shards.iter().map(|s| s.cache.read().len() as u64).sum()
    }

    /// Bump the peak-entries high-water atomically.
    fn record_peak(&self) {
        let cur = self.total_len();
        let mut peak = self.stat_peak_entries.load(Ordering::Relaxed);
        while cur > peak {
            match self
                .stat_peak_entries
                .compare_exchange_weak(peak, cur, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(observed) => peak = observed,
            }
        }
    }

    /// Create a row cache with specified capacity
    pub fn with_capacity(max_entries: usize) -> Self {
        Self::with_config(RowCacheConfig {
            max_entries,
            ..Default::default()
        })
    }

    /// Get a cached row by key as an owned `Tuple` (deep-copies the values on
    /// hit). Most callers want this; the index-nested-loop join uses
    /// [`Self::get_arc`] to avoid the copy when it only borrows the row.
    ///
    /// Returns `Some(Tuple)` if found and not expired, `None` otherwise.
    pub fn get(&self, table: &str, row_id: u64) -> Option<Tuple> {
        self.get_arc(table, row_id).map(|t| (*t).clone())
    }

    /// Get a shared `Arc` handle to a cached row by key (cheap refcount bump on
    /// hit, no `Vec<Value>` copy).
    ///
    /// Returns `Some(Arc<Tuple>)` if found and not expired, `None` otherwise.
    pub fn get_arc(&self, table: &str, row_id: u64) -> Option<Arc<Tuple>> {
        if !self.config.enabled {
            return None;
        }

        let key = RowCacheKey::new(table, row_id);

        // P0#4: concurrent-read path. Use a SHARED read lock + `peek` (no LRU
        // recency mutation) so simultaneous point lookups don't serialize on an
        // exclusive cache lock, and bump lock-free atomic counters instead of
        // taking the stats lock. Trade-off: reads no longer promote LRU recency,
        // so a read-hot row may be evicted slightly sooner; TTL is unchanged.
        // Expired entries are left in place (reaped on the next put/eviction).
        self.hot_lookups.fetch_add(1, Ordering::Relaxed);

        // HELIOS_ROWCACHE_LEGACY=1 restores the exclusive-write-lock + LRU-recency
        // read path for A/B comparison (and as a fallback if strict LRU recency is
        // required). Read once.
        static LEGACY: once_cell::sync::Lazy<bool> =
            once_cell::sync::Lazy::new(|| std::env::var("HELIOS_ROWCACHE_LEGACY").is_ok());
        let shard = self.shard_for(&key);
        if *LEGACY {
            let mut cache = shard.write();
            if let Some(entry) = cache.get_mut(&key) {
                if entry.is_expired() {
                    cache.pop(&key);
                    self.hot_expirations.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                let tuple = entry.access();
                self.hot_hits.fetch_add(1, Ordering::Relaxed);
                return Some(tuple);
            }
            self.hot_misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        {
            let cache = shard.read();
            match cache.peek(&key) {
                Some(entry) if !entry.is_expired() => {
                    let tuple = entry.tuple.clone();
                    self.hot_hits.fetch_add(1, Ordering::Relaxed);
                    return Some(tuple);
                }
                Some(_) => {} // expired — fall through to evict under the write lock
                None => {
                    self.hot_misses.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            }
        }
        // Expired entry: take the exclusive lock, re-check, and pop exactly once
        // (so `expirations` is counted once — not once per read — and the slot is
        // reclaimed rather than occupying capacity until the next put). The hot,
        // non-expired path above stays fully shared + lock-free.
        let mut cache = shard.write();
        if let Some(entry) = cache.peek(&key) {
            if entry.is_expired() {
                cache.pop(&key);
                self.hot_expirations.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            // Refreshed by a concurrent writer between the read and write lock.
            let tuple = entry.tuple.clone();
            self.hot_hits.fetch_add(1, Ordering::Relaxed);
            return Some(tuple);
        }
        self.hot_misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Insert a row into the cache.
    pub fn put(&self, table: &str, row_id: u64, tuple: Tuple) {
        self.put_arc(table, row_id, Arc::new(tuple));
    }

    /// Insert an already-shared row into the cache without a deep copy (used by
    /// the `Arc`-returning point/INLJ fetch path).
    pub fn put_arc(&self, table: &str, row_id: u64, tuple: Arc<Tuple>) {
        if !self.config.enabled {
            return;
        }

        let key = RowCacheKey::new(table, row_id);

        // Determine TTL based on table hotness
        let ttl = self.get_ttl_for_table(table);

        let per_shard_cap = (self.config.max_entries / ROW_CACHE_SHARDS).max(1);
        let shard = self.shard_for(&key);
        let was_full = {
            let mut cache = shard.write();
            let full = cache.len() >= per_shard_cap;
            cache.put(key, CachedRow::from_arc(tuple, ttl));
            full
        };

        self.stat_inserts.fetch_add(1, Ordering::Relaxed);
        if was_full {
            self.stat_evictions.fetch_add(1, Ordering::Relaxed);
        }
        self.record_peak();
    }

    /// Invalidate a specific row
    pub fn invalidate(&self, table: &str, row_id: u64) {
        if !self.config.enabled {
            return;
        }

        let key = RowCacheKey::new(table, row_id);

        let removed = self.shard_for(&key).write().pop(&key).is_some();
        if removed {
            self.stat_invalidations.fetch_add(1, Ordering::Relaxed);
        }

        // Mark the table hot only when this invalidation actually removed a
        // cached row. UPDATE/DELETE fast paths often invalidate rows that were
        // never cached; paying hot-table bookkeeping on every miss is pure
        // write-path overhead and does not protect correctness.
        if removed {
            self.mark_table_hot(table);
        }
    }

    /// Invalidate all cached rows for a table
    pub fn invalidate_table(&self, table: &str) {
        if !self.config.enabled {
            return;
        }

        // A table's rows may live in any shard — sweep them all.
        let mut removed_count = 0u64;
        for shard in self.shards.iter() {
            let mut cache = shard.cache.write();
            let keys_to_remove: Vec<RowCacheKey> = cache
                .iter()
                .filter(|(k, _)| k.table == table)
                .map(|(k, _)| k.clone())
                .collect();
            removed_count += keys_to_remove.len() as u64;
            for key in keys_to_remove {
                cache.pop(&key);
            }
        }

        self.stat_invalidations.fetch_add(removed_count, Ordering::Relaxed);
        self.mark_table_hot(table);
    }

    /// Clear all cached entries
    pub fn clear(&self) {
        let mut count = 0u64;
        for shard in self.shards.iter() {
            let mut cache = shard.cache.write();
            count += cache.len() as u64;
            cache.clear();
        }
        self.stat_invalidations.fetch_add(count, Ordering::Relaxed);
    }

    /// Get cache statistics
    pub fn stats(&self) -> RowCacheStats {
        RowCacheStats {
            // Read-path counters (P0#4).
            lookups: self.hot_lookups.load(Ordering::Relaxed),
            hits: self.hot_hits.load(Ordering::Relaxed),
            misses: self.hot_misses.load(Ordering::Relaxed),
            expirations: self.hot_expirations.load(Ordering::Relaxed),
            // Write-path counters (R-D3).
            evictions: self.stat_evictions.load(Ordering::Relaxed),
            inserts: self.stat_inserts.load(Ordering::Relaxed),
            invalidations: self.stat_invalidations.load(Ordering::Relaxed),
            current_entries: self.total_len(),
            peak_entries: self.stat_peak_entries.load(Ordering::Relaxed),
        }
    }

    /// Reset statistics
    pub fn reset_stats(&self) {
        let current_entries = self.total_len();
        self.stat_inserts.store(0, Ordering::Relaxed);
        self.stat_evictions.store(0, Ordering::Relaxed);
        self.stat_invalidations.store(0, Ordering::Relaxed);
        self.stat_peak_entries.store(current_entries, Ordering::Relaxed);
        self.hot_lookups.store(0, Ordering::Relaxed);
        self.hot_hits.store(0, Ordering::Relaxed);
        self.hot_misses.store(0, Ordering::Relaxed);
        self.hot_expirations.store(0, Ordering::Relaxed);
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
        self.total_len() as usize
    }

    /// Check if cache is empty
    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| s.cache.read().is_empty())
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
        Tuple::new(vec![Value::Int4(id), Value::String(name.to_string())])
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
