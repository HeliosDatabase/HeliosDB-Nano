//! N-way sharded LRU cache (R2.1 — query-entry mutex convoy removal).
//!
//! Every `query()` / `execute()` statement used to take several global
//! `std::sync::Mutex<lru::LruCache>` locks (plan / parse / result /
//! fast-DML-spec caches). Under 16 threads those single mutexes convoy:
//! aggregate throughput measured *below* single-thread
//! (`perf/P0_4_lock_churn.md`). This helper keeps the exact LRU semantics
//! *per shard* while letting statements with different cache keys proceed
//! in parallel: a key is hashed to one of N shards (N = 16, power of two)
//! and only that shard's mutex is taken. Total capacity is divided across
//! shards.
//!
//! Semantics preserved relative to the previous global `Mutex<LruCache>`:
//! - `get` updates per-shard LRU recency exactly like `LruCache::get`.
//! - `put` evicts least-recently-used entries *within the key's shard*
//!   (global LRU order becomes per-shard LRU order — the only intended
//!   behavioral difference, capacity-neutral in aggregate).
//! - `clear` iterates every shard, so DDL clear-all / DML result-cache
//!   invalidation still wipe everything.
//! - Mutex poisoning is recovered via `into_inner()` instead of silently
//!   skipping the operation; for invalidation paths this is strictly
//!   safer (a poisoned cache can never serve stale entries by dodging a
//!   `clear`), and the LRU structure cannot be left logically broken by
//!   a panic in our code (no user code runs under the lock).

use std::borrow::Borrow;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Number of shards. Power of two so shard selection is a mask, sized to
/// cover the 16-thread collapse documented in the R2.1 audit while keeping
/// per-shard capacity meaningful for the 128-entry spec caches (128/16 = 8).
const SHARD_COUNT: usize = 16;

/// A concurrent LRU cache split into [`SHARD_COUNT`] independent
/// `Mutex<LruCache>` shards. A given key always maps to the same shard.
pub(crate) struct ShardedLruCache<K: Hash + Eq, V> {
    shards: Box<[Mutex<lru::LruCache<K, V>>]>,
    hasher: RandomState,
    /// Bumped on every `clear()`. Lets long-lived holders of cached
    /// entries (e.g. prepared statements pinning an `Arc<LogicalPlan>`,
    /// R5.W2) cheaply detect that the cache was invalidated (DDL) and
    /// refresh, without re-keying into the cache per use.
    epoch: AtomicU64,
    /// Lock-census site label for shard-mutex contention accounting (W3.1).
    /// `Unlabeled` unless set via [`with_site`]; carried by value into the
    /// zero-cost `lock_census::mutex_lock` shim, so the field is read (never
    /// dead) even when the `lock-census` feature compiles the sampling out.
    site: crate::lock_census::Site,
}

impl<K: Hash + Eq, V: Clone> ShardedLruCache<K, V> {
    /// Create a cache with `total_capacity` entries divided evenly across
    /// the shards (minimum one entry per shard).
    pub(crate) fn new(total_capacity: NonZeroUsize) -> Self {
        let per_shard = NonZeroUsize::new((total_capacity.get() / SHARD_COUNT).max(1)).unwrap_or(NonZeroUsize::MIN);
        let shards: Vec<Mutex<lru::LruCache<K, V>>> = (0..SHARD_COUNT)
            .map(|_| Mutex::new(lru::LruCache::new(per_shard)))
            .collect();
        Self {
            shards: shards.into_boxed_slice(),
            hasher: RandomState::new(),
            epoch: AtomicU64::new(0),
            site: crate::lock_census::Site::Unlabeled,
        }
    }

    /// Label this cache's shard mutexes for the lock-census (W3.1). Chained at
    /// construction on the read-hot caches (plan / parse / result); the DML
    /// spec caches keep the default `Unlabeled` and are never sampled.
    #[inline]
    pub(crate) fn with_site(mut self, site: crate::lock_census::Site) -> Self {
        self.site = site;
        self
    }

    #[allow(clippy::expect_used)] // Safety: index is masked to SHARD_COUNT-1; `shards` holds SHARD_COUNT entries.
    fn shard_for<Q>(&self, key: &Q) -> &Mutex<lru::LruCache<K, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let idx = (self.hasher.hash_one(key) as usize) & (SHARD_COUNT - 1);
        self.shards.get(idx).expect("masked shard index in range")
    }

    /// Take a shard's lock through the lock-census shim. When the `lock-census`
    /// feature is off this is exactly the previous `shard.lock()` +
    /// poison-recovery, inlined at zero cost; when on and enabled at runtime it
    /// try-locks first and records contention against `self.site`.
    #[inline]
    fn lock_shard<'a>(&self, shard: &'a Mutex<lru::LruCache<K, V>>) -> std::sync::MutexGuard<'a, lru::LruCache<K, V>> {
        crate::lock_census::mutex_lock(self.site, shard)
    }

    /// Clone-on-hit lookup. Updates the key's recency within its shard.
    pub(crate) fn get<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.lock_shard(self.shard_for(key)).get(key).cloned()
    }

    /// Insert (or refresh) an entry, evicting the LRU entry of the key's
    /// shard if that shard is full.
    pub(crate) fn put(&self, key: K, value: V) {
        self.lock_shard(self.shard_for(&key)).put(key, value);
    }

    /// Recency-neutral membership probe (matches `LruCache::contains`).
    pub(crate) fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.lock_shard(self.shard_for(key)).contains(key)
    }

    /// Clear every shard. Shards are cleared one at a time; concurrent
    /// readers of an already-cleared shard simply miss (same guarantee the
    /// global mutex gave: an invalidated entry is never served afterwards).
    pub(crate) fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::Release);
        for shard in self.shards.iter() {
            self.lock_shard(shard).clear();
        }
    }

    /// Invalidation epoch: incremented by every `clear()`. An entry
    /// captured together with `epoch()` is safe to reuse for as long as
    /// the epoch is unchanged.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// True when every shard is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.shards.iter().all(|shard| self.lock_shard(shard).is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(capacity: usize) -> ShardedLruCache<String, u64> {
        ShardedLruCache::new(NonZeroUsize::new(capacity).unwrap())
    }

    #[test]
    fn get_put_contains_roundtrip() {
        let c = cache(128);
        assert!(c.is_empty());
        assert_eq!(c.get("k1"), None);
        c.put("k1".to_string(), 7);
        assert_eq!(c.get("k1"), Some(7));
        assert!(c.contains("k1"));
        assert!(!c.contains("k2"));
        assert!(!c.is_empty());
    }

    #[test]
    fn clear_wipes_every_shard() {
        let c = cache(256);
        for i in 0..200u64 {
            c.put(format!("key-{i}"), i);
        }
        assert!(!c.is_empty());
        c.clear();
        assert!(c.is_empty());
        for i in 0..200u64 {
            assert_eq!(c.get(&format!("key-{i}")), None);
        }
    }

    #[test]
    fn per_shard_lru_evicts_within_shard_only() {
        // Total capacity 16 → 1 entry per shard. Inserting two keys that
        // land in the same shard must evict the older one; keys in other
        // shards must survive.
        let c = cache(16);
        for i in 0..1000u64 {
            c.put(format!("key-{i}"), i);
        }
        // At most one entry per shard survives.
        let live: usize = (0..1000u64).filter(|i| c.contains(&format!("key-{i}"))).count();
        assert!(live <= 16, "live={live} exceeds shard capacity");
        assert!(live >= 1);
    }

    #[test]
    fn recency_update_on_get() {
        // Force a single-entry-per-shard cache and verify `get` refreshes
        // recency: with capacity 32 (2 per shard), repeatedly getting one
        // key keeps it alive while sibling keys in its shard churn.
        let c = cache(32);
        c.put("hot".to_string(), 1);
        for i in 0..500u64 {
            assert_eq!(c.get("hot"), Some(1), "hot key evicted at i={i}");
            c.put(format!("cold-{i}"), i);
            assert_eq!(c.get("hot"), Some(1), "hot key evicted after cold-{i}");
        }
    }

    #[test]
    fn concurrent_smoke() {
        let c = std::sync::Arc::new(cache(256));
        std::thread::scope(|s| {
            for t in 0..8usize {
                let c = std::sync::Arc::clone(&c);
                s.spawn(move || {
                    for i in 0..10_000u64 {
                        let key = format!("k-{}", (i + t as u64 * 17) % 64);
                        c.put(key.clone(), i);
                        let _ = c.get(&key);
                        if i % 1000 == 0 {
                            c.clear();
                        }
                    }
                });
            }
        });
    }
}
