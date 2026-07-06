//! Transient COPY batch version markers (next-batch item #2).
//!
//! A `COPY … FROM STDIN` fast-batch (`insert_prepared_tuples_fast_batch`) with
//! time-travel enabled previously wrote a per-row `v:` (full value duplicate)
//! and `v_idx:` (reverse-ts index) record. On narrow rows that per-row version
//! traffic was ~2/3 of the COPY cost (measured 393 ms → 173 ms @100k when
//! elided). Instead, the batch writes ONE durable marker
//!
//! ```text
//! vmeta:{table}:{first_row_id:020}:{last_row_id:020} → {commit_ts: be u64}
//! ```
//!
//! over its contiguous row-id range, and this in-memory interval set answers
//! the only two questions the version records were needed for:
//!
//!   1. AS-OF read of a COPY'd-and-never-mutated row (no `v_idx:` at all):
//!      the row is visible iff `snapshot_ts >= marker_ts`, and its value is the
//!      current `data:` (no update has occurred). [`covering_ts`]
//!   2. First versioning UPDATE/DELETE of such a row must first materialize the
//!      insert version (`v:`/`v_idx:` at `marker_ts`) from the old `data:` value
//!      before overwriting — so later AS-OF reads between COPY and the update
//!      resolve correctly. The caller checks `v_idx:` absence, then this map.
//!
//! Correctness envelope: a row mutated through a version-SKIPPING fast path
//! (`update_tuple_fast`, which already writes no version today — the pre-existing
//! D4 gap) inherits the SAME AS-OF staleness a normal fast-updated row already
//! has; the marker never makes it worse. Never-mutated and
//! properly-versioned-updated rows are exactly correct.
//!
//! One marker per COPY statement (not per row), so the set stays tiny; markers
//! persist and are reloaded on open. Background materialization (converting
//! markers to per-row records to bound growth) is a documented follow-up.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) const VMETA_PREFIX: &str = "vmeta:";

/// One durable marker over a contiguous row-id range inserted at `ts`.
#[derive(Clone, Copy, Debug)]
struct Interval {
    first: u64,
    last: u64,
    ts: u64,
}

/// In-memory index of live COPY markers, keyed by table.
pub(crate) struct CopyMarkers {
    by_table: RwLock<HashMap<String, Vec<Interval>>>,
    /// Fast-out: `false` ⇒ no markers anywhere, hot paths skip the map read
    /// with a single relaxed atomic load.
    any: AtomicBool,
}

impl CopyMarkers {
    pub(crate) fn new() -> Self {
        Self {
            by_table: RwLock::new(HashMap::new()),
            any: AtomicBool::new(false),
        }
    }

    /// True when no markers exist — the hot-path fast-out.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        !self.any.load(Ordering::Relaxed)
    }

    /// Total number of live markers across all tables (inspection/testing).
    pub(crate) fn len(&self) -> usize {
        if self.is_empty() {
            return 0;
        }
        self.by_table.read().values().map(|v| v.len()).sum()
    }

    /// Encode the durable marker key for a range. Zero-padded to 20 digits so
    /// the lexical `vmeta:` scan order matches numeric row-id order.
    pub(crate) fn marker_key(table: &str, first: u64, last: u64) -> String {
        format!("{VMETA_PREFIX}{table}:{first:020}:{last:020}")
    }

    /// Parse a `vmeta:{table}:{first}:{last}` key + `{ts: be u64}` value back
    /// into `(table, first, last, ts)`. Returns `None` on any malformed record
    /// (skipped on load rather than aborting recovery — a torn marker just
    /// means those rows fall back to the "no versions → visible to all" path).
    fn parse_marker(key: &[u8], value: &[u8]) -> Option<(String, u64, u64, u64)> {
        let key = std::str::from_utf8(key).ok()?;
        let rest = key.strip_prefix(VMETA_PREFIX)?;
        // Split from the RIGHT: the two trailing `:first:last` are fixed-width
        // numeric; the table name (which may itself contain `:`) is the prefix.
        let (rest, last) = rest.rsplit_once(':')?;
        let (table, first) = rest.rsplit_once(':')?;
        let first: u64 = first.parse().ok()?;
        let last: u64 = last.parse().ok()?;
        if value.len() < 8 {
            return None;
        }
        let ts = u64::from_be_bytes(value.get(0..8)?.try_into().ok()?);
        Some((table.to_string(), first, last, ts))
    }

    /// Record a marker in the in-memory index (call after the durable write).
    pub(crate) fn insert(&self, table: &str, first: u64, last: u64, ts: u64) {
        let mut g = self.by_table.write();
        g.entry(table.to_string())
            .or_default()
            .push(Interval { first, last, ts });
        self.any.store(true, Ordering::Relaxed);
    }

    /// Load one marker record found during the open-time `vmeta:` scan.
    pub(crate) fn load_record(&self, key: &[u8], value: &[u8]) {
        if let Some((table, first, last, ts)) = Self::parse_marker(key, value) {
            self.insert(&table, first, last, ts);
        }
    }

    /// Whether `table` has at least one live marker. Used by TRUNCATE to skip
    /// the per-row materialization walk on unmarked tables.
    pub(crate) fn table_has_markers(&self, table: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        self.by_table.read().get(table).is_some_and(|v| !v.is_empty())
    }

    /// The COPY insert timestamp covering `row_id` of `table`, if any live
    /// marker's range includes it. `None` when no marker covers it (the caller
    /// then keeps the existing behavior). Overlaps cannot occur — COPY batches
    /// reserve disjoint contiguous row-id ranges — so the first match wins.
    pub(crate) fn covering_ts(&self, table: &str, row_id: u64) -> Option<u64> {
        if self.is_empty() {
            return None;
        }
        let g = self.by_table.read();
        let intervals = g.get(table)?;
        intervals
            .iter()
            .find(|iv| row_id >= iv.first && row_id <= iv.last)
            .map(|iv| iv.ts)
    }
}

impl Default for CopyMarkers {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrips_including_colon_in_table() {
        for (t, f, l, ts) in [("t", 1u64, 100u64, 42u64), ("weird:name", 5, 9, 7), ("x", 0, u64::MAX, 1)] {
            let k = CopyMarkers::marker_key(t, f, l);
            let v = ts.to_be_bytes();
            let (pt, pf, pl, pts) = CopyMarkers::parse_marker(k.as_bytes(), &v).unwrap();
            assert_eq!((pt.as_str(), pf, pl, pts), (t, f, l, ts));
        }
    }

    #[test]
    fn covering_ts_finds_and_misses() {
        let m = CopyMarkers::new();
        assert!(m.is_empty());
        assert_eq!(m.covering_ts("t", 5), None);
        m.insert("t", 10, 20, 100);
        m.insert("t", 30, 40, 200);
        assert!(!m.is_empty());
        assert_eq!(m.covering_ts("t", 9), None);
        assert_eq!(m.covering_ts("t", 10), Some(100));
        assert_eq!(m.covering_ts("t", 15), Some(100));
        assert_eq!(m.covering_ts("t", 20), Some(100));
        assert_eq!(m.covering_ts("t", 25), None);
        assert_eq!(m.covering_ts("t", 35), Some(200));
        assert_eq!(m.covering_ts("other", 15), None);
    }

    #[test]
    fn load_record_skips_garbage() {
        let m = CopyMarkers::new();
        m.load_record(b"vmeta:t:00000000000000000001:00000000000000000009", &50u64.to_be_bytes());
        assert_eq!(m.covering_ts("t", 5), Some(50));
        // Garbage records are ignored, not fatal.
        m.load_record(b"vmeta:garbage", b"x");
        m.load_record(b"not-a-marker", b"");
        assert_eq!(m.covering_ts("t", 5), Some(50));
    }

    #[test]
    fn key_ordering_is_numeric() {
        // Zero-padding keeps lexical scan order == numeric row-id order.
        let a = CopyMarkers::marker_key("t", 2, 9);
        let b = CopyMarkers::marker_key("t", 10, 20);
        assert!(a < b, "{a} should sort before {b}");
    }
}
