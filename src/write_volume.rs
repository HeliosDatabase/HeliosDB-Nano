//! Per-statement-class write-volume census (W3.2 quantification).
//!
//! Attributes durable write bytes to three key families — the current row
//! (`data:`), the time-travel version chain (`v:`/`v_idx:` and the COPY
//! `vmeta:` marker), and secondary-index maintenance (ART entry keys) — split
//! by statement class (single INSERT, multi-row INSERT, COPY, UPDATE, DELETE).
//! It exists to answer ONE question before any version-format work: what share
//! of INSERT byte volume is the full `v:` duplicate of every row? (The `v:` value
//! is the row's logical value — equal to `data:` for the default row-store, larger
//! under side-storage; `W3_2_DESIGN.md` §1.1. STOP rule: below 15% ⇒ deprioritize.)
//!
//! ## Zero cost when disabled — runtime-only gate, no cargo feature
//!
//! Every recording site is guarded by ONE relaxed load of a process-global
//! `AtomicBool` (`[performance] write_volume_stats`, default `false`), mirroring
//! the `global_txn_active` fast-out (`lib.rs:540`) and the copy-marker `any`
//! fast-out (`storage/copy_marker.rs:53`). Unlike `lock_census` (W3.1) this
//! needs NO `#[cfg(feature)]`: the per-row cost when disabled is only relaxed
//! atomic *loads* — a load of an almost-always-false, cache-resident,
//! uncontended global — never a store, fence, lock, thread-local access, or
//! allocation. Atoms per row, DISABLED (the RAII `stmt_scope` guard loads
//! `enabled()` once on construction, then one gate per write funnel the row
//! crosses; a funnel gates ALL its `add`/`add_row` calls behind a single
//! `enabled()`):
//!
//!   * autocommit single INSERT  = 4 loads  (scope guard `stmt_scope` + data
//!                                            funnel `insert_tuple_fast` + version
//!                                            funnel `append_version_snapshot_to_batch`
//!                                            + index funnel `on_insert_tuple`);
//!                                            3 loads when time-travel is OFF
//!                                            (`fast`/`fast_ingest` profiles) —
//!                                            the version funnel never runs
//!   * autocommit UPDATE / DELETE = 2 loads  (scope guard + data funnel only —
//!                                            the fast paths are version-skipping)
//!   * COPY / multi-row INSERT    = 1 scope load (statement) + 1 load hoisted
//!                                            before the batch loop + 1 index-funnel
//!                                            load; the per-row cost inside the loop
//!                                            is zero (the gate is hoisted out)
//!
//! Each is a predictable not-taken branch over a single `mov`; there is no
//! measurable steady-state cost, which is the claim this comment proves and the
//! reason a cargo feature would be over-engineering here. Atoms per row,
//! ENABLED: one `fetch_add` per (category) recorded plus one `add_row`, all
//! `Relaxed` — a diagnostic aggregate, not a serialization point.
//!
//! Statement class rides a thread-local `Cell` set by a RAII [`stmt_scope`]
//! guard at the DML dispatch boundary. Storage writes run synchronously on the
//! dispatching thread (no `.await` between the scope and the RocksDB write), so
//! the thread-local is exact for autocommit statements. Buffered explicit-
//! transaction writes land at COMMIT time, decoupled from the staging
//! statement, and are attributed to [`StmtClass::Other`] — documented in
//! `W3_2_DESIGN.md` §"instrumentation coverage".
//!
//! ## Buffered autocommit-implicit commits (the multi-row `VALUES` funnel)
//!
//! A literal multi-row `INSERT … VALUES (…),(…),…` and any single-row INSERT
//! the fast literal path rejects (constrained tables, `ON CONFLICT`,
//! `RETURNING`) fall to the generic plan arm, which STAGES rows into an implicit
//! transaction whose durable bytes are written by `txn.commit()` in
//! `execute_with_implicit_transaction` — *after* the staging arm returns, so a
//! bare `stmt_scope` in the arm would drop before the write. These are still
//! autocommit-implicit (never explicit `BEGIN … COMMIT`), so they DO carry their
//! true class: the wrapper takes a [`checkpoint`] spanning the commit and the
//! arm upgrades the class via [`set_current_class_if_other`]
//! (`insert_multi`/`insert_single`). The `if_other` guard preserves an enclosing
//! `Copy` scope so a COPY that routes through the generic fallback (an
//! ineligible/columnar/trigger table, `handler.rs`) still attributes to `copy`
//! rather than being reclassified as `insert_multi`. Explicit `BEGIN … COMMIT`
//! never reaches that wrapper (its COMMIT runs through `handle_transaction_control`),
//! so it stays `Other` — `W3_2_DESIGN.md` §2.3 note 1 holds.
//!
//! ## Known label loss: parameterized / prepared re-execution (W3 follow-up)
//!
//! Prepared-statement re-execution — SQL `EXECUTE`, and the PG-wire extended
//! protocol's pinned-plan Execute — dispatches DML straight through
//! `execute_plan_with_params` → `execute_plan_with_params_inner` (`lib.rs`),
//! BYPASSING the scoped fast paths (`try_autocommit_fast_insert_params`,
//! `try_autocommit_fast_update_delete_params_cached`) that a simple-query
//! statement would take. That funnel sets NO `stmt_scope`, and its
//! autocommit (no active txn) main-branch INSERT lands in
//! `insert_tuple_versioned_with_schema` — the non-fast versioned path that §2.3
//! note 2 leaves UNINSTRUMENTED (recording its version without its `data:` write
//! would skew the ratio), so those rows appear in NO bucket at all; the buffered
//! / session-transaction sub-path instead records at COMMIT under the ambient
//! class (`Other`). Either way the per-class INSERT/UPDATE/DELETE buckets stop
//! growing once a workload shifts from cold simple-query execution (first
//! pgbench sweep, scoped) onto pinned prepared plans (steady state) — the
//! "counters freeze after the first sweep / label loss on cache rebuild" report.
//! The process-global atomics never literally freeze; the CLASS is lost (or the
//! whole write is omitted), not the count. Closing it means giving
//! `execute_plan_with_params_inner`'s autocommit DML its own class scope AND
//! instrumenting the versioned path's `data:`+version writes together, gated to
//! `session_txn.is_none()` so explicit session-transaction COMMITs keep `Other`
//! (note 1). That touches the hot OLTP path and must be validated by a live
//! `heliosdb_write_volume` re-run under pgbench, so it is deferred to the
//! coordinator gate rather than changed blind.
//!
//! Surfaced via the `heliosdb_write_volume` system view
//! (`sql/phase3/system_views.rs`): one row per statement class with the three
//! byte counters and a row-event count.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The statement class a write is attributed to. `Other` is the default and
/// catches buffered-commit and slow-path writes not wrapped in a [`stmt_scope`].
#[derive(Clone, Copy)]
pub(crate) enum StmtClass {
    InsertSingle,
    InsertMulti,
    Copy,
    Update,
    Delete,
    Other,
}

/// Which key family the bytes belong to.
#[derive(Clone, Copy)]
pub(crate) enum Category {
    /// `data:` — the current row value (key + value bytes).
    Data,
    /// `v:` / `v_idx:` version chain and the COPY `vmeta:` marker.
    Version,
    /// Secondary-index (ART) entry key bytes.
    IndexKey,
}

const CLASS_COUNT: usize = 6;

/// Stable per-class display names, in [`class_index`] order.
const CLASS_NAMES: [&str; CLASS_COUNT] = ["insert_single", "insert_multi", "copy", "update", "delete", "other"];

struct ClassCounters {
    /// `data:` byte total for this class.
    data_bytes: AtomicU64,
    /// `v:`/`v_idx:`/`vmeta:` version-chain byte total for this class.
    version_bytes: AtomicU64,
    /// Secondary-index (ART) entry-key byte total for this class.
    index_key_bytes: AtomicU64,
    /// One event per written row (or per tombstone), for per-row averages.
    rows: AtomicU64,
}

impl ClassCounters {
    const fn new() -> Self {
        Self {
            data_bytes: AtomicU64::new(0),
            version_bytes: AtomicU64::new(0),
            index_key_bytes: AtomicU64::new(0),
            rows: AtomicU64::new(0),
        }
    }

    fn counter(&self, cat: Category) -> &AtomicU64 {
        match cat {
            Category::Data => &self.data_bytes,
            Category::Version => &self.version_bytes,
            Category::IndexKey => &self.index_key_bytes,
        }
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);

// Explicit array literal (no inline-const) to compile on the repo's pinned
// toolchain regardless of `[const { .. }; N]` availability — matching the
// `lock_census` static (`lock_census.rs:98`).
static COUNTERS: [ClassCounters; CLASS_COUNT] = [
    ClassCounters::new(),
    ClassCounters::new(),
    ClassCounters::new(),
    ClassCounters::new(),
    ClassCounters::new(),
    ClassCounters::new(),
];

thread_local! {
    static CURRENT: Cell<StmtClass> = const { Cell::new(StmtClass::Other) };
}

fn class_index(class: StmtClass) -> usize {
    match class {
        StmtClass::InsertSingle => 0,
        StmtClass::InsertMulti => 1,
        StmtClass::Copy => 2,
        StmtClass::Update => 3,
        StmtClass::Delete => 4,
        StmtClass::Other => 5,
    }
}

/// Apply the runtime toggle. Process-global (last config wins — a diagnostic
/// aggregate, like a metrics registry), called from `EmbeddedDatabase`
/// construction.
pub(crate) fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// The single relaxed-load fast-out. Write funnels call this ONCE and gate all
/// their `add`/`add_row` calls on the result.
#[inline]
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Add `bytes` to the current statement class's `cat` counter. Callers MUST
/// have checked [`enabled`] first (so a disabled build pays only that one load).
#[inline]
pub(crate) fn add(cat: Category, bytes: u64) {
    let ci = CURRENT.with(|c| class_index(c.get()));
    if let Some(counters) = COUNTERS.get(ci) {
        counters.counter(cat).fetch_add(bytes, Ordering::Relaxed);
    }
}

/// Count one written row (or tombstone) against the current class. Callers MUST
/// have checked [`enabled`] first.
#[inline]
pub(crate) fn add_row() {
    let ci = CURRENT.with(|c| class_index(c.get()));
    if let Some(counters) = COUNTERS.get(ci) {
        counters.rows.fetch_add(1, Ordering::Relaxed);
    }
}

/// RAII statement-class scope. Restores the previous class on drop so nested
/// DML (e.g. a trigger's INSERT inside an UPDATE) attributes correctly. When
/// the census is disabled the guard is inert (one relaxed load, no thread-local
/// write), preserving the zero-cost-disabled contract at statement granularity.
pub(crate) struct ClassGuard {
    prev: StmtClass,
    active: bool,
}

impl Drop for ClassGuard {
    fn drop(&mut self) {
        if self.active {
            let prev = self.prev;
            CURRENT.with(|c| c.set(prev));
        }
    }
}

/// Enter a statement-class scope for the duration of the returned guard.
pub(crate) fn stmt_scope(class: StmtClass) -> ClassGuard {
    if !enabled() {
        return ClassGuard {
            prev: StmtClass::Other,
            active: false,
        };
    }
    let prev = CURRENT.with(|c| {
        let p = c.get();
        c.set(class);
        p
    });
    ClassGuard { prev, active: true }
}

/// Capture the current class and restore it when the returned guard drops,
/// WITHOUT changing it. Used where the durable write of an autocommit statement
/// lands at a `txn.commit()` decoupled from the plan arm that stages it (the
/// literal multi-row `INSERT … VALUES` path funnels through
/// `execute_with_implicit_transaction`, whose commit runs after the staging arm
/// returns — `lib.rs`). The wrapper takes this checkpoint so the arm may upgrade
/// the ambient class via [`set_current_class_if_other`] for the commit, and the
/// original class is restored afterwards — bounding that upgrade to the one
/// statement so a later explicit-transaction COMMIT is not misattributed
/// (`W3_2_DESIGN.md` §2.3 note 1). Inert (one relaxed load) when disabled.
pub(crate) fn checkpoint() -> ClassGuard {
    if !enabled() {
        return ClassGuard {
            prev: StmtClass::Other,
            active: false,
        };
    }
    let prev = CURRENT.with(|c| c.get());
    ClassGuard { prev, active: true }
}

/// Upgrade the current statement class to `class` ONLY when it is still the
/// `Other` default, leaving any already-set enclosing scope untouched. Used by
/// the generic (buffered) INSERT plan arm to attribute the durable bytes it
/// stages — which are written by the enclosing autocommit COMMIT, not the arm —
/// to `insert_single`/`insert_multi`, while preserving an enclosing `Copy`
/// scope when the arm runs as a COPY generic-path fallback chunk. Does NOT
/// capture a restore point (the enclosing [`checkpoint`] owns that); a bare
/// non-restoring set would leak the class past the statement. Inert (no
/// thread-local write) when disabled.
#[inline]
pub(crate) fn set_current_class_if_other(class: StmtClass) {
    if !enabled() {
        return;
    }
    CURRENT.with(|c| {
        if matches!(c.get(), StmtClass::Other) {
            c.set(class);
        }
    });
}

/// One census row for the system view.
pub(crate) struct ClassStat {
    pub class: &'static str,
    pub data_bytes: u64,
    pub version_bytes: u64,
    pub index_key_bytes: u64,
    pub rows: u64,
}

/// Snapshot all six per-class counters (always six rows; zeros when the census
/// has never been enabled).
pub(crate) fn snapshot() -> Vec<ClassStat> {
    COUNTERS
        .iter()
        .zip(CLASS_NAMES)
        .map(|(counters, class)| ClassStat {
            class,
            data_bytes: counters.data_bytes.load(Ordering::Relaxed),
            version_bytes: counters.version_bytes.load(Ordering::Relaxed),
            index_key_bytes: counters.index_key_bytes.load(Ordering::Relaxed),
            rows: counters.rows.load(Ordering::Relaxed),
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    // Deliberately flag-INDEPENDENT: `add`/`add_row` do not read `ENABLED` (the
    // caller gates on `enabled()`), so this test never touches the process-
    // global flag. Every `EmbeddedDatabase::with_config` in the threaded suite
    // calls `set_enabled(config…write_volume_stats)` = false, and no test sets
    // it true, so `ENABLED` stays false suite-wide: the census funnels never
    // record, `stmt_scope` is inert (leaving `CURRENT` at its `Other` default on
    // every thread), and the `other`-class counters are mutated ONLY by this
    // test's direct calls — exact-delta assertions are race-free.
    use super::{add, add_row, snapshot, Category};

    #[test]
    fn write_volume_counts_by_category_on_the_current_class() {
        // Snapshot shape is stable: six named rows, always.
        let names: Vec<&str> = snapshot().iter().map(|s| s.class).collect();
        assert_eq!(names, vec!["insert_single", "insert_multi", "copy", "update", "delete", "other"]);

        // `other` is index 5 and the default thread class. Record a known delta
        // directly (bypassing the caller-side `enabled()` gate) and assert it
        // lands in the right category counters.
        let before = snapshot();
        add_row();
        add(Category::Data, 40);
        add(Category::Version, 48);
        add(Category::IndexKey, 7);
        let after = snapshot();

        assert_eq!(after[5].data_bytes, before[5].data_bytes + 40, "data bytes on `other`");
        assert_eq!(after[5].version_bytes, before[5].version_bytes + 48, "version bytes on `other`");
        assert_eq!(after[5].index_key_bytes, before[5].index_key_bytes + 7, "index-key bytes on `other`");
        assert_eq!(after[5].rows, before[5].rows + 1, "row event on `other`");

        // Categories are independent: writing Data must not move Version.
        let base_ver = snapshot()[5].version_bytes;
        add(Category::Data, 11);
        assert_eq!(snapshot()[5].version_bytes, base_ver, "Data write leaves Version untouched");
    }
}
