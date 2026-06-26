//! Process-scoped runtime for `CREATE SEQUENCE` / `nextval` / `currval` /
//! `setval`, backed by durable catalog records.
//!
//! # Durability model (v3.60.0)
//!
//! Each sequence has two durable records in the catalog (see
//! [`crate::storage::catalog`]):
//! * `meta:sequence:<name>` — the [`PersistedSequence`] config (start,
//!   increment, min, max, cache, cycle, owner). Written on CREATE/ALTER/DROP
//!   by the executor (which holds storage); rides the normal post-statement
//!   durability barrier.
//! * `meta:seqstate:<name>` — the [`PersistedSeqState`] high-water mark. This
//!   is the tiny, hot record that is fsynced *explicitly* (via
//!   [`StorageEngine::flush_sequence_state`]) before any value in a reserved
//!   block is served.
//!
//! At runtime each sequence has a lock-free [`SeqRuntime`] (per-sequence
//! `Arc`, so different sequences never contend). `nextval` serves values with
//! an atomic CAS inside a durable window `[next ..= block_end]`; only when the
//! window is exhausted does it take the per-sequence refill mutex, reserve a
//! fresh block, fsync the new high-water mark, and *then* publish the window.
//!
//! ## THE NO-DUPLICATE INVARIANT
//!
//! A value `V` is returned by `nextval` ONLY AFTER a durable high-water mark
//! `H >= V` (`PersistedSeqState.last_reserved`) has been fsynced to disk. The
//! refill path persists+fsyncs the new block's `last_reserved` BEFORE
//! publishing `block_end`/`next` to the lock-free atomics (a `Release` store
//! gated behind the in-critical-section fsync, read with `Acquire`), so no
//! value within a block is observable until its backing fsync has completed.
//! On crash+restart the runtime resumes strictly PAST `last_reserved`, so
//! every value the prior process could have served is `<= last_reserved` and
//! is never produced again. The unused tail of an in-flight cache block
//! becomes a gap — which is CORRECT and identical to PostgreSQL/Oracle
//! cached-sequence semantics.
//!
//! Corollaries: `setval` fsyncs before returning; `currval` never advances
//! durable state; a CYCLE wrap point is itself fsynced as the new
//! `last_reserved` before the wrapped value is served. The invariant holds
//! under concurrency (publication is a `Release` gated behind the fsync) and
//! across processes (the refill critical section re-reads durable state and
//! takes the max, so two processes over one data dir never reserve
//! overlapping ranges).
//!
//! ## Divergences from PostgreSQL (documented, intentional)
//! * `currval` returns 0 (not an error) when the sequence has never been
//!   advanced in this process: the evaluator is session-less, so PG's
//!   per-session "currval is undefined before nextval" cannot be faithfully
//!   emulated. a2h/ORMs rely on `nextval` for monotonicity, not `currval`.
//! * An unknown `nextval('x')` auto-vivifies a default (bigint, from 1)
//!   sequence and now persists it so it becomes discoverable, preserving the
//!   leniency SERIAL internals and aggressive migrations rely on.
//! * `CREATE SEQUENCE` without IF NOT EXISTS overwrites/resets (Prisma
//!   re-runs migrations); IF NOT EXISTS is honoured as a no-op.
//!
//! ## Memory-only / storage-absent fallback
//! When no persistence handle is installed (pure in-memory evaluation) or the
//! database is `:memory:`, sequences run volatile: state lives only in the
//! in-process atomics and no fsync is attempted. `nextval` still works and
//! never panics.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;

use crate::storage::StorageEngine;
use crate::storage::{PersistedSeqState, PersistedSequence};
use crate::{Error, Result};

/// Live runtime state for one sequence. Config fields are an immutable
/// snapshot of the persisted definition; the hot fields are atomics so the
/// common `nextval` path is lock-free.
struct SeqRuntime {
    increment: i64,
    min_value: i64,
    max_value: i64,
    /// Reservation block size (>= 1). A perf hint only — never lets a served
    /// value exceed the bound (see `clamp_block_end`).
    cache: i64,
    cycle: bool,
    /// The start value the *first ever* `nextval` returns when `is_called`
    /// is false (CREATE / `setval(., false)` / RESTART).
    start: i64,
    /// The exact value the first `nextval` serves while `is_called` is still
    /// false. For a fresh CREATE this equals `start`; for `RESTART WITH n` and
    /// `setval(., false)` it is `n` (which may differ from `start`). The refill
    /// path serves this (after fsyncing it durable) instead of `start` so the
    /// not-yet-called target is honoured even when it is below `start`.
    not_called_target: i64,

    /// Next value to hand out (lock-free fast-path CAS target).
    next: AtomicI64,
    /// Inclusive last value reservable from the current durable block. When
    /// `next` steps past this, a refill is required.
    block_end: AtomicI64,
    /// Whether any value has been served (drives `currval` and the
    /// is_called/start distinction).
    is_called: AtomicBool,
    /// The exact value most recently RETURNED by `nextval` in THIS process
    /// session, or `i64::MIN` (sentinel) if none has been served yet this
    /// session. Unlike `next - increment` this is exact even right after a
    /// refill, and unlike the durable high-water (`last_reserved`, the block
    /// END) it is the value actually handed out — so `pg_sequences.last_value`
    /// can report it faithfully (PostgreSQL reports the last value obtained, not
    /// the reserved block end). It is NOT durable: after a reopen with no
    /// nextval this session it stays at the sentinel and introspection falls
    /// back to the conservative durable high-water.
    last_served: AtomicI64,
    /// Serializes ONLY the durable refill, never the lock-free serve.
    refill: Mutex<()>,
    /// True when this runtime has no durable backing (memory-only / no
    /// persistence handle): refill extends the window in-memory and never
    /// fsyncs.
    volatile: bool,
}

/// `name -> Arc<SeqRuntime>`. The map is read-locked only briefly to fetch
/// (or lazily build) the `Arc`; all hot work runs on the `Arc`'s atomics, so
/// distinct sequences never contend. An inner `HashMap` under one
/// `parking_lot::Mutex` is used rather than a new `DashMap` dependency.
static STORE: OnceLock<Mutex<HashMap<String, Arc<SeqRuntime>>>> = OnceLock::new();

/// Process-global persistence handle (D1). Installed ONCE at DB-open via
/// [`install_persistence`]; `nextval`/`setval`/introspection upgrade the
/// `Weak` only when they actually run, so NOTHING is added to the
/// per-statement hot path.
static PERSIST: OnceLock<Mutex<Option<Weak<StorageEngine>>>> = OnceLock::new();

fn store() -> &'static Mutex<HashMap<String, Arc<SeqRuntime>>> {
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Install the durable persistence handle. Called once per `EmbeddedDatabase`
/// open, right after `rebuild_all_indexes`. Takes `&Arc` and downgrades, so
/// the caller's subsequent move of the `Arc` into `Self` is unaffected, and
/// the `Weak` never keeps the engine alive.
pub fn install_persistence(engine: &Arc<StorageEngine>) {
    *PERSIST.get_or_init(|| Mutex::new(None)).lock() = Some(Arc::downgrade(engine));
}

/// Upgrade the persistence handle, if installed and still live. `None` means
/// pure in-memory evaluation (no DB) — callers fall back to volatile mode.
pub fn persist_handle() -> Option<Arc<StorageEngine>> {
    PERSIST
        .get()
        .and_then(|m| m.lock().clone())
        .and_then(|w| w.upgrade())
}

/// Evict a sequence's cached runtime so the next `nextval` rebuilds it from
/// the (freshly written) durable definition + state. Called by the executor
/// after CREATE / ALTER (incl. RESTART) / DROP, which is what makes RESTART
/// discard any in-flight cached block.
pub fn invalidate_cache(name: &str) {
    if let Some(m) = STORE.get() {
        m.lock().remove(name);
    }
}

/// Eagerly load every persisted sequence into the runtime map. Optional —
/// `nextval` lazy-loads on demand, so this is NOT required for correctness and
/// is NOT wired into the startup hot path. Provided for callers (tests /
/// future warm-start) that want the map prepopulated.
pub fn warm_load(engine: &StorageEngine) -> Result<()> {
    let defs = engine.catalog().list_sequences()?;
    let mut guard = store().lock();
    for def in defs {
        let st = engine.catalog().get_sequence_state(&def.name)?;
        let rt = SeqRuntime::from_persisted(&def, st, false);
        guard.insert(def.name.clone(), Arc::new(rt));
    }
    Ok(())
}

impl SeqRuntime {
    /// Seed state for a sequence that has a definition but no persisted state
    /// yet (just created): nothing served, resume point == start.
    fn seed_state(def: &PersistedSequence) -> PersistedSeqState {
        PersistedSeqState {
            last_reserved: def.start_value,
            is_called: false,
        }
    }

    /// Build a runtime from a durable definition + (optional) durable state.
    ///
    /// Restart-init choice: `block_end = last_reserved` forces the FIRST
    /// post-restart `nextval` into the refill path, which reserves a FRESH
    /// durable block strictly past `last_reserved`. This is the simplest
    /// correct resume (gaps allowed) and upholds the no-duplicate invariant.
    fn from_persisted(def: &PersistedSequence, state: Option<PersistedSeqState>, volatile: bool) -> Self {
        let st = state.unwrap_or_else(|| Self::seed_state(def));
        let increment = def.increment_by;

        // The runtime is ALWAYS built with an EMPTY initial window, so the very
        // first `nextval` (for either is_called state) is forced into the
        // durable refill path — which fsyncs the block high-water BEFORE serving
        // any value. This is what upholds the NO-DUPLICATE invariant even for
        // the FIRST value of a freshly-created (or RESTARTed) sequence with any
        // cache size: without it, a CACHE=1 seed window `[start ..= start]`
        // would let the fast path serve `start` with no fsync, and a reopen
        // would serve `start` again (a duplicate).
        //
        // We park `next`/`block_end` so that `in_block(next, block_end)` is
        // false:
        //   * is_called: `block_end = last_reserved`, `next` parked just OUTSIDE
        //     (past the high-water) — the refill reserves a fresh block strictly
        //     past `last_reserved`, so we resume without ever re-serving a
        //     reserved value.
        //   * not called: `block_end` parked just BEFORE the not-called target
        //     (`last_reserved`), so the window is empty; the refill's
        //     `!base_called` branch serves `last_reserved` itself (== `start`
        //     for a fresh CREATE; == `n` for RESTART WITH n / setval(., false))
        //     after fsyncing it durable.
        let (next, block_end) = if st.is_called {
            let parked = if increment > 0 {
                st.last_reserved.saturating_add(1)
            } else {
                st.last_reserved.saturating_sub(1)
            };
            (parked, st.last_reserved)
        } else {
            // Empty window: block_end one step "behind" the target so the target
            // is out-of-block and the first call refills.
            let empty_end = if increment > 0 {
                st.last_reserved.saturating_sub(1)
            } else {
                st.last_reserved.saturating_add(1)
            };
            (st.last_reserved, empty_end)
        };

        SeqRuntime {
            increment,
            min_value: def.min_value,
            max_value: def.max_value,
            cache: def.cache.max(1),
            cycle: def.cycle,
            start: def.start_value,
            // For the not-called case the refill serves the durable not-called
            // target, which we stash here so RESTART WITH n / setval(., false)
            // (where the target differs from `start`) serve `n`, not `start`.
            not_called_target: st.last_reserved,
            next: AtomicI64::new(next),
            block_end: AtomicI64::new(block_end),
            is_called: AtomicBool::new(st.is_called),
            // No value served in THIS session yet (even if durably is_called):
            // the sentinel makes `peek_last_served` decline so introspection
            // falls back to the durable high-water until this process serves one.
            last_served: AtomicI64::new(i64::MIN),
            refill: Mutex::new(()),
            volatile,
        }
    }

    /// Is `cur` still inside the currently reserved durable window?
    #[inline]
    fn in_block(&self, cur: i64, end: i64) -> bool {
        if self.increment > 0 {
            cur <= end
        } else {
            cur >= end
        }
    }
}

/// One arithmetic step with full bound + overflow handling.
///
/// Ascending (`incr > 0`): `cur + incr`; on `checked_add` overflow or
/// `n > max` -> if `cycle` returns `min`, else `Err` with the PG message.
/// Descending mirrors (`n < min` -> cycle to `max` or `Err`).
fn checked_step(name: &str, cur: i64, incr: i64, min: i64, max: i64, cycle: bool) -> Result<i64> {
    match cur.checked_add(incr) {
        Some(n) if incr > 0 && n <= max => Ok(n),
        Some(n) if incr < 0 && n >= min => Ok(n),
        // incr == 0 is impossible for a valid sequence, but treat as no-move
        // rather than panicking.
        Some(n) if incr == 0 => Ok(n),
        _ => {
            if cycle {
                Ok(if incr > 0 { min } else { max })
            } else if incr > 0 {
                Err(Error::query_execution(format!(
                    "nextval: reached maximum value of sequence \"{}\" ({})",
                    name, max
                )))
            } else {
                Err(Error::query_execution(format!(
                    "nextval: reached minimum value of sequence \"{}\" ({})",
                    name, min
                )))
            }
        }
    }
}

/// Inclusive last value reservable for a block starting at `first`, covering
/// up to `cache` values, clamped into `[min, max]` via an `i128` intermediate
/// so CACHE can never push the durable high-water (or any served value) past
/// the bound.
fn clamp_block_end(first: i64, incr: i64, cache: i64, min: i64, max: i64) -> i64 {
    let span = (incr as i128) * ((cache.max(1) - 1) as i128);
    let raw = first as i128 + span;
    let clamped = raw.clamp(min as i128, max as i128);
    clamped as i64
}

/// Fetch (or lazily build) the runtime `Arc` for `name`.
///
/// Lazy-load order:
/// 1. present in the map -> clone the `Arc`.
/// 2. storage handle + persisted def -> build from def + durable state.
/// 3. storage handle + NO def -> D7 auto-vivify: persist a default durable
///    def + seed state, then build (so it is discoverable).
/// 4. no storage handle -> volatile default runtime (no regression for pure
///    in-memory evaluation).
fn runtime_for(name: &str) -> Result<Arc<SeqRuntime>> {
    {
        let guard = store().lock();
        if let Some(rt) = guard.get(name) {
            return Ok(Arc::clone(rt));
        }
    }

    // Known-definition path: build outside the map lock to avoid holding it
    // across catalog I/O, then insert-or-take-existing (another thread may have
    // raced us). For the AUTO-VIVIFY path we must serialize the durable seed
    // with claiming the map entry (see below), so it is handled separately.
    let rt = match persist_handle() {
        Some(engine) => match engine.catalog().get_sequence(name)? {
            Some(def) => {
                let st = engine.catalog().get_sequence_state(name)?;
                Arc::new(SeqRuntime::from_persisted(&def, st, false))
            }
            None => {
                // D7: lenient auto-vivify of an UNKNOWN sequence, now durable +
                // discoverable. This MUST be serialized so two concurrent
                // auto-vivifiers cannot have one write the seed {start,false}
                // AFTER the other has already refilled+fsynced {N,true} — that
                // would clobber a durable high-water back to the start and
                // re-serve already-handed-out values across a crash.
                //
                // We serialize on the STORE map lock: only the thread that wins
                // the map entry performs the durable seed, and it does so while
                // holding the lock and ONLY when no state record already exists
                // (so a surviving fsynced high-water is never lowered). Losing
                // threads adopt the winner's `Arc` and never touch durable
                // state. Holding the lock across this catalog I/O is acceptable:
                // auto-vivify of an unknown name is rare and off the hot path.
                let def = PersistedSequence::default_named(name);
                let mut guard = store().lock();
                if let Some(existing) = guard.get(name) {
                    return Ok(Arc::clone(existing));
                }
                // We are the map winner. Persist the definition (idempotent —
                // same content) and seed state ONLY IF ABSENT, never lowering an
                // existing fsynced high-water (no-duplicate invariant).
                engine.catalog().save_sequence(&def)?;
                let st = match engine.catalog().get_sequence_state(name)? {
                    Some(existing) => existing,
                    None => {
                        let seed = SeqRuntime::seed_state(&def);
                        engine.catalog().save_sequence_state(name, &seed)?;
                        seed
                    }
                };
                let rt = Arc::new(SeqRuntime::from_persisted(&def, Some(st), false));
                guard.insert(name.to_string(), Arc::clone(&rt));
                return Ok(rt);
            }
        },
        None => {
            // Storage-absent: volatile default.
            let def = PersistedSequence::default_named(name);
            Arc::new(SeqRuntime::from_persisted(&def, None, true))
        }
    };

    let mut guard = store().lock();
    let entry = guard.entry(name.to_string()).or_insert_with(|| Arc::clone(&rt));
    Ok(Arc::clone(entry))
}

/// Persist + fsync the high-water mark for `name`, upholding the no-duplicate
/// invariant. Volatile runtimes (memory-only / no handle) skip persistence.
fn persist_high_water(rt: &SeqRuntime, name: &str, last_reserved: i64) -> Result<()> {
    if rt.volatile {
        return Ok(());
    }
    let engine =
        persist_handle().ok_or_else(|| Error::query_execution("nextval requires storage context"))?;
    engine.flush_sequence_state(
        name,
        PersistedSeqState {
            last_reserved,
            is_called: true,
        },
    )
}

// ============================================================
// Richer, fallible entry points (used by the executor / later evaluator
// wiring). The legacy free functions below delegate to these.
// ============================================================

/// `nextval(name)` honouring min/max/increment/cycle/overflow and durable
/// cached-block reservation. Returns `Err` on NO-CYCLE overflow / out-of-range
/// (with the PostgreSQL message) and consumes/advances no value in that case.
pub fn try_nextval(name: &str) -> Result<i64> {
    let rt = runtime_for(name)?;

    // ---- FAST PATH: lock-free CAS within the reserved durable window ----
    //
    // Every `cur` in `[next ..= block_end]` is guaranteed in-range (the refill
    // clamps `block_end` into `[min, max]`), so serving `cur` is always valid.
    // The ONLY job here is to advance the pointer past `cur`; the bound / cycle
    // / overflow decision belongs solely to the refill path (which re-derives
    // the next value from the durable high-water). So we advance with a plain
    // `checked_add`, and if that would overflow we publish a sentinel that is
    // definitively OUTSIDE the block, which simply forces the next call to
    // refill. We must never let a step-ahead failure abort a call that is
    // returning an already-reserved `cur`.
    loop {
        let cur = rt.next.load(Ordering::Acquire);
        let end = rt.block_end.load(Ordering::Acquire);
        if !rt.in_block(cur, end) {
            break; // window exhausted -> refill
        }
        let stepped = match cur.checked_add(rt.increment) {
            Some(n) => n,
            // Overflow: park the pointer just past the block so the next call
            // refills. `end` itself is in-block, so `end (+/-) 1` is out.
            None => {
                if rt.increment > 0 {
                    end.saturating_add(1)
                } else {
                    end.saturating_sub(1)
                }
            }
        };
        if rt
            .next
            .compare_exchange(cur, stepped, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            rt.is_called.store(true, Ordering::Relaxed);
            // Record the exact value handed out so `pg_sequences.last_value`
            // reports the served value (not the reserved block end). A racing
            // store from another thread serving a higher value is fine — under
            // concurrency last_value is inherently "the most recent server",
            // matching PG's non-transactional sequence semantics.
            rt.last_served.store(cur, Ordering::Relaxed);
            // `cur` is within `[min, max]` (durable, clamped block), invariant holds.
            return Ok(cur);
        }
        // Lost the race; retry.
    }

    // ---- REFILL: serialized per-sequence; ~1/cache of calls ----
    let _g = rt.refill.lock();

    // Re-check: another thread may have refilled while we waited.
    {
        let cur = rt.next.load(Ordering::Acquire);
        let end = rt.block_end.load(Ordering::Acquire);
        if rt.in_block(cur, end) {
            drop(_g);
            return try_nextval(name);
        }
    }

    // Cross-process safety (D8): re-read the durable high-water INSIDE the
    // lock and take the max with our in-memory view, so two processes over one
    // data dir never reserve overlapping ranges.
    let in_mem_high = rt.block_end.load(Ordering::Acquire);
    let in_mem_called = rt.is_called.load(Ordering::Acquire);
    // Re-read the durable record (if any). `durable` is None when this runtime
    // is volatile OR no durable state record exists yet (a just-created sequence
    // whose seed state was not separately written, or the storage-absent path).
    let durable: Option<PersistedSeqState> = if rt.volatile {
        None
    } else {
        match persist_handle() {
            Some(engine) => engine.catalog().get_sequence_state(name)?,
            None => None,
        }
    };
    let durable_called = durable.map(|d| d.is_called).unwrap_or(false);
    let base_called = durable_called || in_mem_called;

    // First value of the new block.
    //
    // Cross-instance/cross-process note: within ONE process all refills for a
    // sequence serialize through this `rt.refill` mutex (the process-global
    // STORE hands every opener of the same dir the SAME `Arc<SeqRuntime>`), so
    // the in-memory `is_called` flag is authoritative and two in-process
    // refillers can NEVER both take the `!base_called` arm — the first sets
    // `is_called=true` while holding the lock, so the second re-reads it as
    // called and steps. The residual unprotected case is two distinct OS
    // processes over one data dir (separate RocksDB handles, no shared CAS);
    // there the durable record is the only rendezvous, so we re-read it here and
    // reconcile via max() in BOTH arms — including the not-called arm, which the
    // previous code skipped (a TOCTOU that let two processes both serve the very
    // first value). True cross-OS-process atomicity for the first value is a
    // documented architectural limitation (see the module + test-file headers).
    let first = if !base_called {
        // Not-yet-called: serve the not-called TARGET itself (this is the FIRST
        // value the sequence ever yields). For a fresh CREATE this is `start`;
        // for RESTART WITH n / setval(., false) it is `n`. Prefer a durable
        // not-called record (authoritative — e.g. a RESTART that persisted a new
        // target), else the runtime's `not_called_target` (set from the durable
        // state at build time). NEVER use `block_end`, which is the empty-window
        // sentinel (target ∓ 1) for a not-yet-called runtime.
        match durable {
            Some(d) if !d.is_called => d.last_reserved,
            _ => rt.not_called_target,
        }
    } else {
        // Already called somewhere: step strictly past the highest value any
        // party (this process's in-mem block, or a durable record written by a
        // peer process / a prior setval) could have reserved.
        let durable_high = durable.map(|d| d.last_reserved).unwrap_or(in_mem_high);
        let high = std::cmp::max(durable_high, in_mem_high);
        checked_step(name, high, rt.increment, rt.min_value, rt.max_value, rt.cycle)?
    };

    // Reserve up to `cache` values, clamped to the bound.
    let last = clamp_block_end(first, rt.increment, rt.cache, rt.min_value, rt.max_value);

    // *** DURABILITY BARRIER — persist+fsync the high-water BEFORE publishing
    //     the window. After this returns, `first` is durably reserved. ***
    persist_high_water(&rt, name, last)?;

    // `first` is consumed by THIS call; the next value to hand out is the plain
    // step past `first`. We must NOT apply bound/cycle logic here (that belongs
    // to the next refill). If `last == first` (e.g. cache == 1) the stepped
    // value lands outside the one-element block and the next call refills —
    // exactly what we want. On overflow, park just past the block.
    let next_after = match first.checked_add(rt.increment) {
        Some(n) => n,
        None => {
            if rt.increment > 0 {
                last.saturating_add(1)
            } else {
                last.saturating_sub(1)
            }
        }
    };

    // *** PUBLICATION ORDER IS LOAD-BEARING (no-duplicate under concurrency) ***
    //
    // Store `next` (advanced past `first`) FIRST, then widen `block_end`. The
    // window `[next ..= block_end]` only becomes serveable to a lock-free
    // fast-path thread once `block_end` has been widened — and by then `next`
    // already points at `next_after` (strictly past `first`). So no fast-path
    // thread can ever observe `next == first` together with the widened
    // `block_end` and re-hand-out `first` (which this refilling thread is about
    // to return).
    //
    // The reverse order (widen block_end first) has a fatal window: between the
    // two Release stores, `block_end` is the NEW (wider) end while `next` still
    // holds its OLD value — and that OLD value equals THIS block's `first`
    // (for an exhausted ascending block the last fast-path CAS set next = b+inc
    // == checked_step(b) == first; for a fresh/not-called runtime old next ==
    // the target == first). A concurrent fast-path thread reading OLD `next`
    // (== first) and NEW `block_end` would pass `in_block`, CAS-advance, and
    // return `first` — a duplicate of the value this refiller also returns.
    // Release/Acquire on two independent atomics does NOT couple the two loads,
    // so memory ordering alone cannot save the reverse order; only the store
    // ORDER does.
    //
    // While `next` is published but `block_end` is not yet widened, a fast-path
    // thread sees `next == next_after` (out of the OLD block) and harmlessly
    // takes the refill path, where it blocks on `rt.refill` until we release it
    // and then re-checks the now-widened window. No value is lost or duplicated.
    rt.next.store(next_after, Ordering::Release);
    rt.is_called.store(true, Ordering::Relaxed);
    // Record the served value for faithful `pg_sequences.last_value` (the value
    // handed out, not the block end `last`).
    rt.last_served.store(first, Ordering::Relaxed);
    rt.block_end.store(last, Ordering::Release);
    drop(_g);

    Ok(first)
}

/// `currval(name)` — last value served by this process, or 0 if never served
/// (documented divergence from PG, which raises). Never advances durable state
/// and — unlike the old behavior — NEVER auto-vivifies: a `currval` on a name
/// with no live runtime returns 0 WITHOUT creating or persisting a sequence (a
/// read-only function must not conjure a durable catalog object). Only `nextval`
/// auto-vivifies (D7).
pub fn try_currval(name: &str) -> Result<i64> {
    // Peek the map without building/persisting anything.
    let rt = match STORE.get().and_then(|m| m.lock().get(name).cloned()) {
        Some(rt) => rt,
        None => return Ok(0),
    };
    if !rt.is_called.load(Ordering::Acquire) {
        return Ok(0);
    }
    // Prefer the exact value served THIS session (set on every nextval return):
    // it is correct even right after a refill and at the overflow-sentinel edge,
    // where `next - increment` would be off. Only when nothing has been served
    // this session (durably is_called but no nextval since reopen) do we fall
    // back to `next - increment`, the best in-memory estimate.
    let v = rt.last_served.load(Ordering::Relaxed);
    if v != i64::MIN {
        return Ok(v);
    }
    let next = rt.next.load(Ordering::Acquire);
    let last = next.checked_sub(rt.increment).unwrap_or(next);
    Ok(last)
}

/// The exact value most recently RETURNED by `nextval` for `name` in THIS
/// process session, if any. Returns `None` when no runtime is cached for the
/// name OR no value has been served this session (e.g. right after a reopen
/// before the first `nextval`). NEVER auto-vivifies and does NO I/O — it only
/// peeks the in-memory runtime map — so it is safe to call from read-only
/// introspection (`pg_sequences.last_value`).
///
/// Introspection uses this to report PostgreSQL-faithful `last_value` (the
/// value actually handed out) and falls back to the durable high-water (the
/// reserved block END) only when this returns `None`. That fallback can
/// overstate by up to `cache - 1` for a sequence advanced in a PRIOR session,
/// which is the documented cached-sequence gap behavior; within the live
/// session the reported value is exact.
pub fn peek_last_served(name: &str) -> Option<i64> {
    let guard = STORE.get()?.lock();
    let rt = guard.get(name)?;
    let v = rt.last_served.load(Ordering::Relaxed);
    if v == i64::MIN {
        None
    } else {
        Some(v)
    }
}

/// `setval(name, value, is_called)` — durable. Validates `value` is within
/// `[min, max]` (else `Err`), updates the runtime, forces the window to refill
/// on the next `nextval`, and fsyncs the new high-water so it survives restart
/// (pg_dump emits setval at restore).
pub fn try_setval(name: &str, value: i64, is_called: bool) -> Result<i64> {
    let rt = runtime_for(name)?;
    if value < rt.min_value || value > rt.max_value {
        return Err(Error::query_execution(format!(
            "setval: value {} is out of bounds for sequence \"{}\" (min {}, max {})",
            value, name, rt.min_value, rt.max_value
        )));
    }

    let _g = rt.refill.lock();

    // Durably persist FIRST (skipped for volatile runtimes), so the new
    // high-water is on disk before any in-memory window change is observable.
    if !rt.volatile {
        let engine =
            persist_handle().ok_or_else(|| Error::query_execution("setval requires storage context"))?;
        engine.flush_sequence_state(
            name,
            PersistedSeqState {
                last_reserved: value,
                is_called,
            },
        )?;
        // Drop the cached runtime so the next nextval/currval rebuilds it from
        // the freshly-fsynced durable state via `from_persisted` (which parks an
        // EMPTY initial window). This guarantees the next `nextval` re-enters
        // the durable refill barrier rather than serving `value` from a stale
        // in-memory window — preserving the no-duplicate invariant across a
        // crash right after `setval` (pg_dump restore emits setval).
        drop(_g);
        invalidate_cache(name);
        return Ok(value);
    }

    // Volatile (memory-only / no persistence handle): mutate the in-memory
    // window directly. No durability requirement (no WAL, no crash recovery),
    // so it is fine to serve from the in-memory window without a refill.
    if is_called {
        // `value` already produced; collapse the window so the next nextval
        // steps past `value` via the in-memory refill.
        let nxt = checked_step(name, value, rt.increment, rt.min_value, rt.max_value, true).unwrap_or(value);
        rt.block_end.store(value, Ordering::Release);
        rt.next.store(nxt, Ordering::Release);
    } else {
        // `value` not yet produced -> the next nextval returns exactly `value`.
        // A single-element window `[value ..= value]` serves it from the fast
        // path; the call after that refills and steps past.
        rt.block_end.store(value, Ordering::Release);
        rt.next.store(value, Ordering::Release);
    }
    rt.is_called.store(is_called, Ordering::Release);
    drop(_g);
    Ok(value)
}

/// Build the runtime for a freshly-created/altered sequence directly from its
/// definition + (optional) state, replacing any cached runtime. The executor
/// calls this after persisting the definition so the next `nextval` reflects
/// it without a catalog round-trip. (Equivalent to `invalidate_cache` followed
/// by a lazy load, but avoids the extra read.)
pub fn install_runtime(def: &PersistedSequence, state: Option<PersistedSeqState>) {
    let rt = Arc::new(SeqRuntime::from_persisted(def, state, persist_handle().is_none()));
    store().lock().insert(def.name.clone(), rt);
}

// ============================================================
// Legacy free-function API — preserved EXACTLY so the session-less,
// storage-less evaluator (evaluator.rs) keeps compiling unchanged. These
// delegate to the durable, fallible entry points above.
// ============================================================

/// Register a new sequence, honouring `START WITH` / `INCREMENT BY`.
///
/// Storage-less convenience path (kept for the evaluator and unit tests). When
/// a persistence handle is installed it ALSO persists a durable definition so
/// the sequence is discoverable; otherwise it builds a volatile runtime.
/// `start` defaults to 1 and `increment` to 1 when omitted, reproducing the
/// historical `nextval` -> 1, 2, 3 … behaviour.
pub fn create_sequence(name: &str, if_not_exists: bool, start: Option<i64>, increment: Option<i64>) {
    let increment = match increment.unwrap_or(1) {
        0 => 1,
        n => n,
    };
    let start = start.unwrap_or(1);

    if if_not_exists {
        // Honour IF NOT EXISTS: if a runtime or durable def already exists,
        // leave it untouched.
        if store().lock().contains_key(name) {
            return;
        }
        if let Some(engine) = persist_handle() {
            if engine.catalog().sequence_exists(name).unwrap_or(false) {
                return;
            }
        }
    }

    // Build a bigint definition with ascending defaults, overlaying the two
    // historical knobs. Bounds follow the type defaults unless start sits
    // below the default min (then widen min to start so start is in-range).
    let (mut min_value, max_value) = if increment > 0 {
        (1, PersistedSequence::BIGINT_MAX)
    } else {
        (PersistedSequence::BIGINT_MIN, -1)
    };
    if increment > 0 && start < min_value {
        min_value = start;
    }
    let def = PersistedSequence {
        name: name.to_string(),
        data_type: "bigint".to_string(),
        start_value: start,
        increment_by: increment,
        min_value,
        max_value,
        cache: 1,
        cycle: false,
        owned_by_table: None,
        owned_by_column: None,
    };

    if let Some(engine) = persist_handle() {
        // Best-effort durable persist; ignore errors here to preserve the
        // infallible legacy signature (the executor path surfaces errors).
        let _ = engine.catalog().save_sequence(&def);
        let _ = engine
            .catalog()
            .save_sequence_state(name, &SeqRuntime::seed_state(&def));
    }
    install_runtime(&def, None);
}

/// `nextval(name)` — infallible legacy wrapper. On a NO-CYCLE overflow /
/// out-of-range it returns the boundary value (best-effort) instead of
/// erroring, since the historical evaluator call site expects an `i64`. The
/// fallible [`try_nextval`] carries the real error for the executor and the
/// later evaluator wiring (SEQ-5).
pub fn nextval(name: &str) -> i64 {
    match try_nextval(name) {
        Ok(v) => v,
        Err(_) => {
            // Best-effort sentinel: the bound the sequence was trying to pass.
            // Reading the runtime is cheap and avoids a panic.
            match runtime_for(name) {
                Ok(rt) => {
                    if rt.increment >= 0 {
                        rt.max_value
                    } else {
                        rt.min_value
                    }
                }
                Err(_) => 0,
            }
        }
    }
}

/// `currval(name)` — last value produced by `nextval` for this sequence in
/// this process, or 0 if unknown / never called (documented divergence).
pub fn currval(name: &str) -> i64 {
    try_currval(name).unwrap_or(0)
}

/// `setval(name, value[, is_called])` — infallible legacy wrapper. Always
/// returns `value` (like PG). Out-of-range / storage errors are swallowed to
/// preserve the historical signature; [`try_setval`] surfaces them.
pub fn setval(name: &str, value: i64, is_called: bool) -> i64 {
    let _ = try_setval(name, value, is_called);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests run WITHOUT a persistence handle installed, so the runtime
    // is volatile (in-memory). That exercises the full arithmetic / cache /
    // cycle / overflow logic without touching disk. Durable reopen behaviour
    // is covered by the integration suite (SEQ-1's reopen test).

    #[test]
    fn default_sequence_starts_at_one() {
        create_sequence("seq_default", false, None, None);
        assert_eq!(nextval("seq_default"), 1);
        assert_eq!(nextval("seq_default"), 2);
        assert_eq!(currval("seq_default"), 2);
    }

    #[test]
    fn honors_start_and_increment() {
        create_sequence("seq_si", false, Some(100), Some(10));
        assert_eq!(nextval("seq_si"), 100);
        assert_eq!(nextval("seq_si"), 110);
        assert_eq!(nextval("seq_si"), 120);
    }

    #[test]
    fn setval_preserves_increment() {
        create_sequence("seq_sv", false, Some(1), Some(5));
        assert_eq!(nextval("seq_sv"), 1);
        // Two-arg form == is_called=true: next nextval is value + increment.
        setval("seq_sv", 50, true);
        assert_eq!(nextval("seq_sv"), 55);
    }

    #[test]
    fn setval_is_called_false_makes_next_nextval_equal_value() {
        create_sequence("seq_sv_false", false, Some(1), Some(5));
        assert_eq!(nextval("seq_sv_false"), 1);
        // is_called=false: the next nextval returns exactly `value`.
        setval("seq_sv_false", 300, false);
        assert_eq!(nextval("seq_sv_false"), 300);
        // and then resumes stepping by the increment.
        assert_eq!(nextval("seq_sv_false"), 305);
    }

    #[test]
    fn unknown_sequence_auto_vivifies_at_one() {
        // Preserves the lenient pre-existing behaviour SERIAL internals rely on.
        assert_eq!(nextval("seq_never_created_xyz"), 1);
    }

    #[test]
    fn cache_serves_contiguous_values_in_one_window() {
        // CACHE > 1: the volatile window serves several values before refill.
        let def = PersistedSequence {
            name: "seq_cache".into(),
            data_type: "bigint".into(),
            start_value: 1,
            increment_by: 1,
            min_value: 1,
            max_value: PersistedSequence::BIGINT_MAX,
            cache: 8,
            cycle: false,
            owned_by_table: None,
            owned_by_column: None,
        };
        install_runtime(&def, None);
        for expected in 1..=20 {
            assert_eq!(try_nextval("seq_cache").unwrap(), expected);
        }
    }

    #[test]
    fn ascending_maxvalue_no_cycle_errors() {
        let def = PersistedSequence {
            name: "seq_maxnc".into(),
            data_type: "bigint".into(),
            start_value: 1,
            increment_by: 1,
            min_value: 1,
            max_value: 3,
            cache: 1,
            cycle: false,
            owned_by_table: None,
            owned_by_column: None,
        };
        install_runtime(&def, None);
        assert_eq!(try_nextval("seq_maxnc").unwrap(), 1);
        assert_eq!(try_nextval("seq_maxnc").unwrap(), 2);
        assert_eq!(try_nextval("seq_maxnc").unwrap(), 3);
        let err = try_nextval("seq_maxnc").unwrap_err();
        assert!(err.to_string().contains("reached maximum value"), "{}", err);
    }

    #[test]
    fn ascending_maxvalue_cycle_wraps_to_min() {
        let def = PersistedSequence {
            name: "seq_maxcy".into(),
            data_type: "bigint".into(),
            start_value: 1,
            increment_by: 1,
            min_value: 1,
            max_value: 3,
            cache: 1,
            cycle: true,
            owned_by_table: None,
            owned_by_column: None,
        };
        install_runtime(&def, None);
        assert_eq!(try_nextval("seq_maxcy").unwrap(), 1);
        assert_eq!(try_nextval("seq_maxcy").unwrap(), 2);
        assert_eq!(try_nextval("seq_maxcy").unwrap(), 3);
        // Wraps back to min.
        assert_eq!(try_nextval("seq_maxcy").unwrap(), 1);
    }

    #[test]
    fn descending_minvalue_no_cycle_errors() {
        let def = PersistedSequence {
            name: "seq_desc".into(),
            data_type: "bigint".into(),
            start_value: -1,
            increment_by: -1,
            min_value: -3,
            max_value: -1,
            cache: 1,
            cycle: false,
            owned_by_table: None,
            owned_by_column: None,
        };
        install_runtime(&def, None);
        assert_eq!(try_nextval("seq_desc").unwrap(), -1);
        assert_eq!(try_nextval("seq_desc").unwrap(), -2);
        assert_eq!(try_nextval("seq_desc").unwrap(), -3);
        let err = try_nextval("seq_desc").unwrap_err();
        assert!(err.to_string().contains("reached minimum value"), "{}", err);
    }

    #[test]
    fn overflow_near_i64_max_errors_not_panics() {
        let def = PersistedSequence {
            name: "seq_ovf".into(),
            data_type: "bigint".into(),
            start_value: i64::MAX - 1,
            increment_by: 10,
            min_value: 1,
            max_value: PersistedSequence::BIGINT_MAX,
            cache: 1,
            cycle: false,
            owned_by_table: None,
            owned_by_column: None,
        };
        install_runtime(&def, None);
        // First value is the start (is_called=false), which is i64::MAX - 1.
        assert_eq!(try_nextval("seq_ovf").unwrap(), i64::MAX - 1);
        // Next would overflow / exceed max -> Err, no panic.
        let err = try_nextval("seq_ovf").unwrap_err();
        assert!(err.to_string().contains("reached maximum value"), "{}", err);
    }

    #[test]
    fn setval_out_of_bounds_errors() {
        let def = PersistedSequence {
            name: "seq_svb".into(),
            data_type: "bigint".into(),
            start_value: 1,
            increment_by: 1,
            min_value: 1,
            max_value: 100,
            cache: 1,
            cycle: false,
            owned_by_table: None,
            owned_by_column: None,
        };
        install_runtime(&def, None);
        assert!(try_setval("seq_svb", 999, true).is_err());
        assert_eq!(try_setval("seq_svb", 50, true).unwrap(), 50);
    }

    #[test]
    fn cache_never_exceeds_max_bound() {
        // CACHE wide enough to overshoot the bound; the last block must clamp.
        let def = PersistedSequence {
            name: "seq_clamp".into(),
            data_type: "bigint".into(),
            start_value: 1,
            increment_by: 1,
            min_value: 1,
            max_value: 5,
            cache: 100,
            cycle: false,
            owned_by_table: None,
            owned_by_column: None,
        };
        install_runtime(&def, None);
        for expected in 1..=5 {
            assert_eq!(try_nextval("seq_clamp").unwrap(), expected);
        }
        // Sixth must error (no value > 5 ever served), not return a clamped dup.
        assert!(try_nextval("seq_clamp").unwrap_err().to_string().contains("reached maximum value"));
    }

    #[test]
    fn clamp_block_end_stays_in_range() {
        // Pure-function check on the i128 clamp.
        assert_eq!(clamp_block_end(1, 1, 100, 1, 5), 5);
        assert_eq!(clamp_block_end(1, 1, 8, 1, i64::MAX), 8);
        assert_eq!(clamp_block_end(-1, -1, 100, -5, -1), -5);
        // No overflow near i64::MAX.
        assert_eq!(clamp_block_end(i64::MAX - 1, 10, 1000, 1, i64::MAX), i64::MAX);
    }
}
