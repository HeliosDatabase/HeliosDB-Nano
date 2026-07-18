//! COPY fast-path phase-timing census (W3.4 ART-maintenance attribution).
//!
//! Splits the wall time of the COPY bulk-insert funnel
//! (`copy_bulk_insert` → `insert_prepared_tuples_fast_batch`) across its
//! phases so the ART-index-maintenance share can be measured against total
//! COPY wall time. It exists to answer ONE question before any ART-batching
//! work: what fraction of a COPY is `on_insert_tuple` (the per-row secondary-
//! index walk)? (`W3_4_DESIGN.md` §1. STOP rule: below 8% ⇒ record and stop.)
//!
//! ## Phases
//!
//! | phase              | region                                              |
//! |--------------------|-----------------------------------------------------|
//! | `decode`           | wire text/CSV decode (`handler.rs handle_copy`)     |
//! | `type_convert`     | `String` → typed `Value` (`materialize_copy_tuple`) |
//! | `check_constraint` | CHECK evaluation (`validate_check_constraints`)     |
//! | `prepare`          | PK/SERIAL auto-fill (`prepare_fast_insert_batch`)   |
//! | `validate_batch`   | NOT NULL + duplicate-PK (`validate_fast_insert_batch`) |
//! | `fk_constraint`    | FK probes (`validate_copy_batch_fks`)               |
//! | `batch_build`      | serialize + `data:`/`v:`/`vmeta:`/columnar WriteBatch build |
//! | `commit`           | `db.write(batch)` durable RocksDB write             |
//! | `art_maintain`     | `on_insert_tuple` + HNSW `on_row_insert` per row    |
//! | `total`            | whole `copy_bulk_insert` insert work (denominator)  |
//!
//! `total` wraps the engine-side funnel and is the COPY-wall-time denominator;
//! `decode` is disjoint from it (it runs on the wire before the funnel), so the
//! full server-side COPY wall time is `total + decode`. The itemized phases
//! (`type_convert`..`art_maintain`) sum to `total` minus a small un-attributed
//! remainder (fall-back checks, spec resolve, SMFI guard, durability barrier).
//! `art_maintain` share = `art_maintain.total_nanos / (total + decode).total_nanos`.
//!
//! ## Zero cost when disabled — runtime-only gate, no cargo feature
//!
//! Every phase boundary is ONE relaxed load of a process-global `AtomicBool`
//! (`[performance] copy_phase_stats`, default `false`), mirroring the
//! `global_txn_active` fast-out (`lib.rs:540`) and the `write_volume` census
//! (W3.2). When disabled, [`time`] takes no clock reading and returns an inert
//! guard — one predicable not-taken branch per phase boundary, no
//! `Instant::now`, no store, no allocation. There is at most ONE guard per phase
//! per COPY batch (the guards wrap whole loops / calls, never a per-row body),
//! so a 100k-row COPY pays ten relaxed loads total when off — the reason a cargo
//! feature (as `lock_census` needs for its try-lock sampling) is over-
//! engineering here. ENABLED: one `Instant::now` at each boundary and, on drop,
//! three `Relaxed` `fetch_add`s (nanos, calls, rows) — a diagnostic aggregate,
//! not a serialization point.
//!
//! The census is process-global: `batch_build`/`commit`/`art_maintain` live in
//! `insert_prepared_tuples_fast_batch`, shared with multi-row `INSERT ... VALUES`
//! (`try_fast_insert_many_params`). Read the view after a COPY-only workload —
//! exactly as `write_volume` (W3.2) is driven per statement class.
//!
//! Surfaced via the `heliosdb_copy_phase_stats` system view
//! (`sql/phase3/system_views.rs`): one row per phase with cumulative nanos, a
//! call count, and a processed-row count.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// A phase of the COPY fast-path funnel. `Total` wraps the whole engine-side
/// insert work and is the wall-time denominator; the others are its itemized
/// sub-phases (`Decode` excepted — it runs on the wire, disjoint from `Total`).
#[derive(Clone, Copy)]
pub(crate) enum Phase {
    Decode,
    TypeConvert,
    CheckConstraint,
    Prepare,
    ValidateBatch,
    FkConstraint,
    BatchBuild,
    Commit,
    ArtMaintain,
    Total,
}

const PHASE_COUNT: usize = 10;

/// Stable per-phase display names, in [`phase_index`] order.
const PHASE_NAMES: [&str; PHASE_COUNT] = [
    "decode",
    "type_convert",
    "check_constraint",
    "prepare",
    "validate_batch",
    "fk_constraint",
    "batch_build",
    "commit",
    "art_maintain",
    "total",
];

struct PhaseCounters {
    /// Cumulative wall nanos attributed to this phase.
    nanos: AtomicU64,
    /// Number of times this phase's timer fired (COPY batches through it).
    calls: AtomicU64,
    /// Rows processed by this phase (0 for `decode` — see module docs; use
    /// `total.rows` as the COPY row count N for per-row math).
    rows: AtomicU64,
}

impl PhaseCounters {
    const fn new() -> Self {
        Self {
            nanos: AtomicU64::new(0),
            calls: AtomicU64::new(0),
            rows: AtomicU64::new(0),
        }
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);

// Explicit array literal (no inline-const) to compile on the repo's pinned
// toolchain regardless of `[const { .. }; N]` availability — matching the
// `write_volume` / `lock_census` statics.
static COUNTERS: [PhaseCounters; PHASE_COUNT] = [
    PhaseCounters::new(),
    PhaseCounters::new(),
    PhaseCounters::new(),
    PhaseCounters::new(),
    PhaseCounters::new(),
    PhaseCounters::new(),
    PhaseCounters::new(),
    PhaseCounters::new(),
    PhaseCounters::new(),
    PhaseCounters::new(),
];

fn phase_index(phase: Phase) -> usize {
    match phase {
        Phase::Decode => 0,
        Phase::TypeConvert => 1,
        Phase::CheckConstraint => 2,
        Phase::Prepare => 3,
        Phase::ValidateBatch => 4,
        Phase::FkConstraint => 5,
        Phase::BatchBuild => 6,
        Phase::Commit => 7,
        Phase::ArtMaintain => 8,
        Phase::Total => 9,
    }
}

/// Apply the runtime toggle. Process-global (last config wins — a diagnostic
/// aggregate, like a metrics registry), called from `EmbeddedDatabase`
/// construction.
pub(crate) fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// The single relaxed-load fast-out.
#[inline]
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Accumulate one phase observation. Unconditional (the caller has already
/// gated on [`enabled`] via [`time`]); kept separate so the in-module test can
/// record a known delta without touching the process-global flag.
#[inline]
fn record(phase: Phase, nanos: u64, rows: u64) {
    if let Some(counters) = COUNTERS.get(phase_index(phase)) {
        counters.nanos.fetch_add(nanos, Ordering::Relaxed);
        counters.calls.fetch_add(1, Ordering::Relaxed);
        counters.rows.fetch_add(rows, Ordering::Relaxed);
    }
}

/// RAII phase timer. Records `start.elapsed()` into the phase's counters on
/// drop. When the census is disabled `start` is `None` and the guard is inert
/// (no clock reading, no store), preserving the zero-cost-disabled contract at
/// phase granularity.
pub(crate) struct PhaseTimer {
    phase: Phase,
    rows: u64,
    start: Option<Instant>,
}

impl Drop for PhaseTimer {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            record(self.phase, start.elapsed().as_nanos() as u64, self.rows);
        }
    }
}

/// Time `phase` over `rows` rows for the duration of the returned guard. One
/// relaxed load when disabled (returns an inert guard); one `Instant::now` when
/// enabled.
#[inline]
pub(crate) fn time(phase: Phase, rows: u64) -> PhaseTimer {
    if !enabled() {
        return PhaseTimer {
            phase,
            rows: 0,
            start: None,
        };
    }
    PhaseTimer {
        phase,
        rows,
        start: Some(Instant::now()),
    }
}

/// One census row for the system view.
pub(crate) struct PhaseStat {
    pub phase: &'static str,
    pub total_nanos: u64,
    pub calls: u64,
    pub rows: u64,
}

/// Snapshot all ten per-phase counters (always ten rows; zeros when the census
/// has never been enabled).
pub(crate) fn snapshot() -> Vec<PhaseStat> {
    COUNTERS
        .iter()
        .zip(PHASE_NAMES)
        .map(|(counters, phase)| PhaseStat {
            phase,
            total_nanos: counters.nanos.load(Ordering::Relaxed),
            calls: counters.calls.load(Ordering::Relaxed),
            rows: counters.rows.load(Ordering::Relaxed),
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    // Deliberately flag-INDEPENDENT: `record` does not read `ENABLED` (the
    // caller gates via `time`), so this test never touches the process-global
    // flag. Every `EmbeddedDatabase::with_config` in the threaded suite calls
    // `set_enabled(config…copy_phase_stats)` = false and no test sets it true,
    // so `ENABLED` stays false suite-wide: the census `time` guards are inert
    // and the counters are mutated ONLY by this test's direct `record` calls —
    // exact-delta assertions are race-free.
    use super::{record, snapshot, Phase};

    #[test]
    fn copy_phase_stats_accumulate_per_phase() {
        // Snapshot shape is stable: ten named rows, always.
        let names: Vec<&str> = snapshot().iter().map(|s| s.phase).collect();
        assert_eq!(
            names,
            vec![
                "decode",
                "type_convert",
                "check_constraint",
                "prepare",
                "validate_batch",
                "fk_constraint",
                "batch_build",
                "commit",
                "art_maintain",
                "total",
            ]
        );

        // `art_maintain` is index 8. Record a known delta directly (bypassing
        // the caller-side `enabled()` gate) and assert nanos/calls/rows land on
        // that phase and no other.
        let before = snapshot();
        record(Phase::ArtMaintain, 500, 100);
        let after = snapshot();

        assert_eq!(
            after[8].total_nanos,
            before[8].total_nanos + 500,
            "nanos on art_maintain"
        );
        assert_eq!(after[8].calls, before[8].calls + 1, "calls on art_maintain");
        assert_eq!(after[8].rows, before[8].rows + 100, "rows on art_maintain");

        // Phases are independent: recording `art_maintain` must not move `total`.
        assert_eq!(
            after[9].total_nanos, before[9].total_nanos,
            "total untouched by art_maintain write"
        );
    }
}
