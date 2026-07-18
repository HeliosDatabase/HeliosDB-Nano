//! Read-hot-path lock-contention census (`lock-census` feature — W3.1).
//!
//! Attributes the c>=32 wire-throughput plateau to the serialization points on
//! the per-query read path BEFORE the lock-free hot-shape slot is designed.
//! Every instrumented acquisition — the sharded plan/parse/result caches
//! (`std::sync::Mutex` per shard, `sharded_lru.rs`), the ART index registry
//! (`std::sync::RwLock`, `storage/art_manager.rs`), and the prepared-statement
//! fast-select registry (`parking_lot::RwLock`, `lib.rs`) — try-locks first: a
//! `WouldBlock` proves a holder is inside the critical section, so it bumps a
//! relaxed contention counter and accumulates the blocked-wait nanos, then
//! blocks normally. try-lock success and poison recovery bump only the
//! acquisition counter.
//!
//! Zero cost when the `lock-census` feature is off: the helpers below compile
//! to a plain `.lock()` / `.read()` with the [`Site`] argument dropped, so
//! release builds carry no instrumentation at all. Within an instrumented
//! build a single relaxed `AtomicBool` (`[performance] lock_census`) gates all
//! sampling — one `Relaxed` load fast-out, mirroring the `global_txn_active`
//! precedent (`lib.rs:540`). Counters and the enable flag are process-global
//! (a diagnostic aggregate, like a metrics registry); the runtime toggle is
//! last-config-wins across `EmbeddedDatabase` instances in one process.
//!
//! Surfaced via the `heliosdb_lock_census` system view
//! (`sql/phase3/system_views.rs`): one row per site with acquisitions,
//! contended count, and cumulative contended-wait nanos.

/// A named lock site on the read hot path. The ordinal is stable — the
/// `heliosdb_lock_census` view reads counters by [`site_index`]. `Unlabeled`
/// is carried by the write-path DML spec caches so a census build never
/// samples them.
#[derive(Clone, Copy)]
pub(crate) enum Site {
    Unlabeled,
    PlanCache,
    ParseCache,
    ResultCache,
    ArtIndexRegistry,
    ArtPkRegistry,
    StatementRegistry,
}

/// One census row for the system view. When the `lock-census` feature is off,
/// `snapshot()` returns an empty vector and never builds one, hence the allow.
#[allow(dead_code)]
pub(crate) struct SiteStat {
    pub name: &'static str,
    pub acquisitions: u64,
    pub contended: u64,
    pub contended_wait_nanos: u64,
}

// ======================= feature-on internals =======================

#[cfg(feature = "lock-census")]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(feature = "lock-census")]
use std::time::{Duration, Instant};

/// Number of surfaced sites (every [`Site`] except `Unlabeled`).
#[cfg(feature = "lock-census")]
const SITE_COUNT: usize = 6;

/// Stable per-site display names, in [`site_index`] order.
#[cfg(feature = "lock-census")]
const SITE_NAMES: [&str; SITE_COUNT] = [
    "plan_cache_shard",
    "parse_cache_shard",
    "result_cache_shard",
    "art_index_registry",
    "art_pk_registry",
    "statement_registry",
];

#[cfg(feature = "lock-census")]
struct SiteCounters {
    acquisitions: AtomicU64,
    contended: AtomicU64,
    contended_wait_nanos: AtomicU64,
}

#[cfg(feature = "lock-census")]
impl SiteCounters {
    const fn new() -> Self {
        Self {
            acquisitions: AtomicU64::new(0),
            contended: AtomicU64::new(0),
            contended_wait_nanos: AtomicU64::new(0),
        }
    }
}

#[cfg(feature = "lock-census")]
static ENABLED: AtomicBool = AtomicBool::new(false);

// Explicit array literal (no inline-const) so this compiles on the repo's
// pinned toolchain regardless of `[const { .. }; N]` availability.
#[cfg(feature = "lock-census")]
static SITES: [SiteCounters; SITE_COUNT] = [
    SiteCounters::new(),
    SiteCounters::new(),
    SiteCounters::new(),
    SiteCounters::new(),
    SiteCounters::new(),
    SiteCounters::new(),
];

#[cfg(feature = "lock-census")]
fn site_index(site: Site) -> Option<usize> {
    match site {
        Site::Unlabeled => None,
        Site::PlanCache => Some(0),
        Site::ParseCache => Some(1),
        Site::ResultCache => Some(2),
        Site::ArtIndexRegistry => Some(3),
        Site::ArtPkRegistry => Some(4),
        Site::StatementRegistry => Some(5),
    }
}

#[cfg(feature = "lock-census")]
#[inline]
fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[cfg(feature = "lock-census")]
#[inline]
fn record_acquire(site: Site) {
    if let Some(c) = site_index(site).and_then(|i| SITES.get(i)) {
        c.acquisitions.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(feature = "lock-census")]
#[inline]
fn record_contended(site: Site, waited: Duration) {
    if let Some(c) = site_index(site).and_then(|i| SITES.get(i)) {
        c.acquisitions.fetch_add(1, Ordering::Relaxed);
        c.contended.fetch_add(1, Ordering::Relaxed);
        c.contended_wait_nanos
            .fetch_add(waited.as_nanos() as u64, Ordering::Relaxed);
    }
}

// ======================= public API (two flavours) =======================

#[cfg(feature = "lock-census")]
pub(crate) fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

#[cfg(not(feature = "lock-census"))]
#[inline]
pub(crate) fn set_enabled(_on: bool) {}

#[cfg(feature = "lock-census")]
#[inline]
pub(crate) fn mutex_lock<T>(site: Site, m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    if !is_enabled() {
        return m.lock().unwrap_or_else(|e| e.into_inner());
    }
    match m.try_lock() {
        Ok(guard) => {
            record_acquire(site);
            guard
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            let start = Instant::now();
            let guard = m.lock().unwrap_or_else(|e| e.into_inner());
            record_contended(site, start.elapsed());
            guard
        }
        Err(std::sync::TryLockError::Poisoned(p)) => {
            record_acquire(site);
            p.into_inner()
        }
    }
}

#[cfg(not(feature = "lock-census"))]
#[inline]
pub(crate) fn mutex_lock<T>(_site: Site, m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(feature = "lock-census")]
#[inline]
pub(crate) fn rwlock_read<T>(site: Site, l: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    if !is_enabled() {
        return l.read().unwrap_or_else(|e| e.into_inner());
    }
    match l.try_read() {
        Ok(guard) => {
            record_acquire(site);
            guard
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            let start = Instant::now();
            let guard = l.read().unwrap_or_else(|e| e.into_inner());
            record_contended(site, start.elapsed());
            guard
        }
        Err(std::sync::TryLockError::Poisoned(p)) => {
            record_acquire(site);
            p.into_inner()
        }
    }
}

#[cfg(not(feature = "lock-census"))]
#[inline]
pub(crate) fn rwlock_read<T>(_site: Site, l: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

// `parking_lot::RwLock` read (the prepared-statement fast-select registry,
// `lib.rs`): `try_read` returns `None` when a writer holds/waits, and
// parking_lot locks never poison. Like the ART registry reads, a read-only
// benchmark has no writer so `contended` stays near zero by construction — see
// the §2.1 reader cache-line blind spot in `W3_1_DESIGN.md`; the `acquisitions`
// count still proves the lock is on the prepared hot path.
#[cfg(feature = "lock-census")]
#[inline]
pub(crate) fn pl_rwlock_read<T>(site: Site, l: &parking_lot::RwLock<T>) -> parking_lot::RwLockReadGuard<'_, T> {
    if !is_enabled() {
        return l.read();
    }
    match l.try_read() {
        Some(guard) => {
            record_acquire(site);
            guard
        }
        None => {
            let start = Instant::now();
            let guard = l.read();
            record_contended(site, start.elapsed());
            guard
        }
    }
}

#[cfg(not(feature = "lock-census"))]
#[inline]
pub(crate) fn pl_rwlock_read<T>(_site: Site, l: &parking_lot::RwLock<T>) -> parking_lot::RwLockReadGuard<'_, T> {
    l.read()
}

#[cfg(feature = "lock-census")]
pub(crate) fn snapshot() -> Vec<SiteStat> {
    SITES
        .iter()
        .zip(SITE_NAMES)
        .map(|(counters, name)| SiteStat {
            name,
            acquisitions: counters.acquisitions.load(Ordering::Relaxed),
            contended: counters.contended.load(Ordering::Relaxed),
            contended_wait_nanos: counters.contended_wait_nanos.load(Ordering::Relaxed),
        })
        .collect()
}

#[cfg(not(feature = "lock-census"))]
#[inline]
pub(crate) fn snapshot() -> Vec<SiteStat> {
    Vec::new()
}

// Exercises the sampling path; only meaningful (and only compiled) under the
// `lock-census` feature, which the perf gate builds with. Single test fn so the
// process-global counters/enable flag are touched from one place — no
// intra-suite race. Assertions are delta/`>=` based to tolerate the harness.
#[cfg(all(test, feature = "lock-census"))]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::{mutex_lock, set_enabled, snapshot, Site};
    use std::sync::Mutex;

    #[test]
    fn census_records_and_snapshots() {
        // Snapshot shape is stable regardless of enable state.
        let names: Vec<&str> = snapshot().iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "plan_cache_shard",
                "parse_cache_shard",
                "result_cache_shard",
                "art_index_registry",
                "art_pk_registry",
                "statement_registry",
            ]
        );

        let m = Mutex::new(0u32);

        // Disabled: an acquisition must not be counted.
        set_enabled(false);
        let before_off = snapshot()[0].acquisitions;
        {
            let _g = mutex_lock(Site::PlanCache, &m);
        }
        assert_eq!(
            snapshot()[0].acquisitions,
            before_off,
            "disabled census must not record"
        );

        // Enabled: an uncontended acquisition bumps acquisitions, not contended.
        set_enabled(true);
        let before = snapshot()[0].acquisitions;
        let contended_before = snapshot()[0].contended;
        {
            let _g = mutex_lock(Site::PlanCache, &m);
        }
        let after = snapshot();
        assert!(after[0].acquisitions > before, "enabled census must record acquisition");
        assert_eq!(
            after[0].contended, contended_before,
            "uncontended lock must not count as contended"
        );

        // Unlabeled sites are never recorded (write-path spec caches).
        let unlabeled_acq = snapshot()[0].acquisitions;
        {
            let _g = mutex_lock(Site::Unlabeled, &m);
        }
        assert_eq!(snapshot()[0].acquisitions, unlabeled_acq, "Unlabeled must not record");

        set_enabled(false);
    }
}
