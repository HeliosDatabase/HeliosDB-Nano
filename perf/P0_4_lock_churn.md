# P0#4 — Reduce per-statement lock churn

**Branch:** `perf/p0-p1-p2`  ·  32-core EPYC, in-memory, 2000-row hot set fully cached

## Done: row-cache read path is now a shared lock

`RowCache::get` took an **exclusive** `cache.write()` (because `LruCache::get_mut`
mutates recency) plus 2–3 `stats.write()` locks **per lookup** — so concurrent
point lookups serialized on the cache. Changed to a **shared** `cache.read()` +
`peek` (no recency mutation) and **lock-free atomic** hot counters
(`hot_lookups/hits/misses/expirations`). Trade-off: read accesses no longer
promote LRU recency (a read-hot row may evict slightly sooner); TTL unchanged.
`HELIOS_ROWCACHE_LEGACY=1` restores the old path.

### Concurrent hot-lookup throughput (A/B in one binary)

| threads | LEGACY (write-lock) | NEW (read-lock+peek) | NEW/LEGACY |
|---:|---:|---:|---:|
| 1 | 844 k/s | 843 k/s | 1.00× (no contention) |
| 4 | 951 k/s | **1.50 M/s** | **1.58×** |
| 16 | 423 k/s | 480 k/s | 1.13× |

The read-lock wins at every contended thread count, best at moderate concurrency.

## The bigger finding: the per-query Mutexes dominate at high concurrency

Both paths **collapse below the single-thread rate at 16 threads** (480 k vs
843 k). The row cache is no longer the limiter — the `query()` entry path takes
**four exclusive `Mutex` locks on every statement**: `current_transaction`,
`result_cache`, `plan_cache`, and `parse_cache`. At 16 threads these convoy and
cap *all* query throughput regardless of the row cache. (Single-thread is
unaffected — and already faster than SQLite — which is why the OLTP harness, being
single-threaded, doesn't surface this.)

## Scoped follow-up (the larger P0#4 win)

Make the query-path caches concurrent:
- `parse_cache` / `plan_cache` / `result_cache`: sharded locks or a lock-free
  cache (e.g. a `RwLock`-per-shard or a `dashmap`/`moka`-style structure); reads
  are the common case.
- `current_transaction`: per-session state / lock-free "is a txn active?" check
  instead of a global `Mutex` taken on every statement.

These live in the `lib.rs` `query()`/`execute()` entry path — the same region the
concurrent insert-path session is editing — so they are sequenced after that work
to avoid a merge collision. Expectation: near-linear point-lookup scaling to the
core count (vs today's collapse at 16 threads).

## Validation

`row_cache` unit tests pass (5/5) including `test_cache_ttl` (expirations are now
counted atomically without popping under the shared lock). Single-thread latency
unchanged.
