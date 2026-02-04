//! Query Result Cache
//!
//! A simple in-memory cache for query results with TTL-based expiration
//! and table-based invalidation.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::{Result, Error, Tuple};

/// Cached query result
#[derive(Debug, Clone)]
pub struct CachedResult {
    /// Cached tuples
    pub tuples: Vec<Tuple>,
    /// When this result was cached
    pub cached_at: Instant,
    /// Time-to-live for this result
    pub ttl: Duration,
    /// Tables referenced by the query
    pub tables: Vec<String>,
}

impl CachedResult {
    /// Create a new cached result
    pub fn new(tuples: Vec<Tuple>, ttl: Duration, tables: Vec<String>) -> Self {
        Self {
            tuples,
            cached_at: Instant::now(),
            ttl,
            tables,
        }
    }

    /// Check if this cached result has expired
    pub fn is_expired(&self) -> bool {
        self.cached_at.elapsed() > self.ttl
    }

    /// Get the age of this cached result
    pub fn age(&self) -> Duration {
        self.cached_at.elapsed()
    }
}

/// Cache key for lookup operations
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// Hash of the SQL query
    pub query_hash: u64,
    /// Branch name (for branching support)
    pub branch: Option<String>,
}

impl CacheKey {
    /// Create a new cache key from a SQL query
    pub fn new(sql: &str, branch: Option<String>) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        sql.hash(&mut hasher);
        Self {
            query_hash: hasher.finish(),
            branch,
        }
    }
}

/// Query result cache
pub struct QueryCache {
    /// Cache storage (key -> cached result)
    cache: Arc<RwLock<HashMap<CacheKey, CachedResult>>>,
    /// Maximum number of entries
    max_entries: usize,
    /// Default TTL for cache entries
    default_ttl: Duration,
    /// Cache statistics
    stats: Arc<RwLock<CacheStats>>,
}

/// Cache statistics
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    /// Number of cache hits
    pub hits: u64,
    /// Number of cache misses
    pub misses: u64,
    /// Number of invalidations
    pub invalidations: u64,
    /// Number of evictions
    pub evictions: u64,
}

impl QueryCache {
    /// Create a new query cache with default settings
    pub fn new() -> Self {
        Self::with_config(1000, Duration::from_secs(60))
    }

    /// Create a new query cache with custom settings
    pub fn with_config(max_entries: usize, default_ttl: Duration) -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            max_entries,
            default_ttl,
            stats: Arc::new(RwLock::new(CacheStats::default())),
        }
    }

    /// Get a cached result if it exists and is not expired
    pub fn get(&self, key: &CacheKey) -> Result<Option<Vec<Tuple>>> {
        use crate::error::LockResultExt;

        let cache = self.cache.read()
            .map_lock_err("Failed to acquire read lock on query cache")?;

        if let Some(cached) = cache.get(key) {
            if cached.is_expired() {
                // Entry exists but is expired - return None
                // The entry will be cleaned up lazily
                drop(cache);
                let mut stats = self.stats.write()
                    .map_lock_err("Failed to acquire write lock on cache stats")?;
                stats.misses += 1;
                return Ok(None);
            }

            // Valid cache hit
            drop(cache);
            let mut stats = self.stats.write()
                .map_lock_err("Failed to acquire write lock on cache stats")?;
            stats.hits += 1;

            // Re-acquire read lock to return the result
            let cache = self.cache.read()
                .map_lock_err("Failed to acquire read lock on query cache")?;
            return Ok(cache.get(key).map(|c| c.tuples.clone()));
        }

        // Cache miss
        let mut stats = self.stats.write()
            .map_lock_err("Failed to acquire write lock on cache stats")?;
        stats.misses += 1;

        Ok(None)
    }

    /// Store a result in the cache
    pub fn put(&self, key: CacheKey, tuples: Vec<Tuple>, tables: Vec<String>) -> Result<()> {
        self.put_with_ttl(key, tuples, tables, self.default_ttl)
    }

    /// Store a result in the cache with custom TTL
    pub fn put_with_ttl(
        &self,
        key: CacheKey,
        tuples: Vec<Tuple>,
        tables: Vec<String>,
        ttl: Duration,
    ) -> Result<()> {
        use crate::error::LockResultExt;

        let mut cache = self.cache.write()
            .map_lock_err("Failed to acquire write lock on query cache")?;

        // Check capacity and evict if needed
        if cache.len() >= self.max_entries && !cache.contains_key(&key) {
            // Evict expired entries first
            let expired_keys: Vec<CacheKey> = cache
                .iter()
                .filter(|(_, v)| v.is_expired())
                .map(|(k, _)| k.clone())
                .collect();

            for k in expired_keys {
                cache.remove(&k);
            }

            // If still at capacity, evict oldest entry
            if cache.len() >= self.max_entries {
                // Find oldest entry by cached_at time
                let oldest_key = cache
                    .iter()
                    .min_by_key(|(_, v)| v.cached_at)
                    .map(|(k, _)| k.clone());

                if let Some(key) = oldest_key {
                    cache.remove(&key);

                    let mut stats = self.stats.write()
                        .map_lock_err("Failed to acquire write lock on cache stats")?;
                    stats.evictions += 1;
                }
            }
        }

        // Store the result
        cache.insert(key, CachedResult::new(tuples, ttl, tables));
        Ok(())
    }

    /// Invalidate cache entries that reference a specific table
    pub fn invalidate_table(&self, table_name: &str) -> Result<u64> {
        use crate::error::LockResultExt;

        let mut cache = self.cache.write()
            .map_lock_err("Failed to acquire write lock on query cache")?;

        let keys_to_remove: Vec<CacheKey> = cache
            .iter()
            .filter(|(_, v)| v.tables.iter().any(|t| t == table_name))
            .map(|(k, _)| k.clone())
            .collect();

        let count = keys_to_remove.len() as u64;
        for key in keys_to_remove {
            cache.remove(&key);
        }

        if count > 0 {
            let mut stats = self.stats.write()
                .map_lock_err("Failed to acquire write lock on cache stats")?;
            stats.invalidations += count;
        }

        Ok(count)
    }

    /// Invalidate all cache entries
    pub fn invalidate_all(&self) -> Result<u64> {
        use crate::error::LockResultExt;

        let mut cache = self.cache.write()
            .map_lock_err("Failed to acquire write lock on query cache")?;

        let count = cache.len() as u64;
        cache.clear();

        if count > 0 {
            let mut stats = self.stats.write()
                .map_lock_err("Failed to acquire write lock on cache stats")?;
            stats.invalidations += count;
        }

        Ok(count)
    }

    /// Get cache statistics
    pub fn stats(&self) -> Result<CacheStats> {
        use crate::error::LockResultExt;

        let stats = self.stats.read()
            .map_lock_err("Failed to acquire read lock on cache stats")?;
        Ok(stats.clone())
    }

    /// Get the number of entries in the cache
    pub fn len(&self) -> Result<usize> {
        use crate::error::LockResultExt;

        let cache = self.cache.read()
            .map_lock_err("Failed to acquire read lock on query cache")?;
        Ok(cache.len())
    }

    /// Check if the cache is empty
    pub fn is_empty(&self) -> Result<bool> {
        self.len().map(|len| len == 0)
    }

    /// Get the cache hit rate
    pub fn hit_rate(&self) -> Result<f64> {
        let stats = self.stats()?;
        let total = stats.hits + stats.misses;
        if total == 0 {
            return Ok(0.0);
        }
        Ok(stats.hits as f64 / total as f64)
    }
}

impl Default for QueryCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_hit_miss() {
        let cache = QueryCache::new();

        let key = CacheKey::new("SELECT * FROM users", None);

        // Cache miss
        let result = cache.get(&key).unwrap();
        assert!(result.is_none());

        // Store result
        let tuples = vec![Tuple::new(vec![crate::Value::Int4(1)])];
        cache.put(key.clone(), tuples.clone(), vec!["users".to_string()]).unwrap();

        // Cache hit
        let result = cache.get(&key).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 1);
    }

    #[test]
    fn test_cache_expiration() {
        let cache = QueryCache::with_config(100, Duration::from_millis(10));

        let key = CacheKey::new("SELECT * FROM users", None);
        let tuples = vec![Tuple::new(vec![crate::Value::Int4(1)])];
        cache.put(key.clone(), tuples, vec!["users".to_string()]).unwrap();

        // Should hit immediately
        assert!(cache.get(&key).unwrap().is_some());

        // Wait for expiration
        std::thread::sleep(Duration::from_millis(20));

        // Should miss after expiration
        assert!(cache.get(&key).unwrap().is_none());
    }

    #[test]
    fn test_cache_invalidation() {
        let cache = QueryCache::new();

        let key1 = CacheKey::new("SELECT * FROM users", None);
        let key2 = CacheKey::new("SELECT * FROM orders", None);

        cache.put(key1.clone(), vec![], vec!["users".to_string()]).unwrap();
        cache.put(key2.clone(), vec![], vec!["orders".to_string()]).unwrap();

        // Invalidate users table
        let count = cache.invalidate_table("users").unwrap();
        assert_eq!(count, 1);

        // Users query should miss
        assert!(cache.get(&key1).unwrap().is_none());
        // Orders query should still hit
        assert!(cache.get(&key2).unwrap().is_some());
    }
}
