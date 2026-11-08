//! Spec R5.6: layout epochs. A statement reads its catalog and its rows under one
//! view, on a primary and on a physical standby.
//!
//! # Why
//!
//! A reader resolves a table's layout (schema, missing values, index list) when it
//! plans, and reads the table's rows later, from the live store, with no snapshot
//! shared between the two. A schema statement that lands in between leaves it
//! holding one moment's catalog over another moment's rows: `DROP COLUMN` shows a
//! value under its neighbour's name, a plan cached before an `ALTER` reads the
//! rows the `ALTER` rewrote. One write group per statement (R5.3, R5.4) makes the
//! store move from one state to the next at once; it does not make a reader's two
//! reads land on the same side of that move. This module does, without a reader
//! gate and without any reader waiting for a writer.
//!
//! # The epoch
//!
//! One per table NAME (never striped, decision 2(i)), made on first use and kept for
//! the life of the engine: an epoch that went back to zero would let a reader
//! validate across a `DROP` and a `CREATE` of the name. Session-private
//! (`pg_temp_*`) tables have none (decision 1); the writer lock still covers them.
//! The low 16 bits count the changes of the table's layout in flight (the seqlock's
//! "odd", generalised so that two changes in flight at once are both seen), the rest
//! count the changes done. A reader's value is usable only when nothing is in
//! flight, and stays valid only while the epoch does not move.
//!
//! # Writers
//!
//! * `WriteGate::write` (and its single-key `put` / `delete`): a batch carrying any
//!   key of [`LAYOUT_PREFIXES`] marks every table it names in flight before RocksDB
//!   sees it, then, once it is in the store, has the owning engine drop what readers
//!   cache of those tables' layouts ([`LayoutHook::publish`]: the schema
//!   generation, the schema cache, the missing values, the row cache), then ends the
//!   change. So a reader that loads an epoch after the change finds no cache from
//!   before it. Any writer, inside a statement batch or not (decision 2(a)).
//! * The physical applier (`replication::physical_applier`), on a standby: the same
//!   around the write and the publication of a run whose batches carry such keys;
//!   such a batch is a run of its own.
//!
//! # Readers
//!
//! A client statement opens a read set ([`StatementScope`]; a nested execution joins
//! the one open on its thread). Every resolution of a table's layout records the
//! table's epoch BEFORE it looks at a cache ([`note_table`], from
//! `Catalog::get_table_schema` / `main_table_schema` and the branch resolver), and a
//! plan built while a [`StampCollector`] is open carries the epochs its planning
//! recorded ([`LayoutStamp`]): a cached or pinned plan is used only while its stamp
//! is current, and records its stamp into the read set of the statement using it
//! (decision 2(b), (c)).
//!
//! # Validation
//!
//! The read set is valid when every recorded epoch had nothing in flight and has not
//! moved. It is checked at the statement's first durable effect (the write funnel
//! refuses a write of a statement whose read set is no longer valid,
//! [`at_durable_effect`]) and, for a statement with none, before its rows are handed
//! to the caller (decision 2(d)). Past its first durable effect a statement never
//! retries. A statement's own layout changes are recorded into its read set at once
//! (decision 2(e)). A mismatch is [`Error`] text carrying [`LAYOUT_CHANGED_MARKER`],
//! which already is SQLSTATE 40001 on every wire; the statement entries retry it
//! (`EmbeddedDatabase::run_layout_statement`), with bounded backoff, off the async
//! workers, up to [`RETRY_BUDGET`] (decision 2(f), (g)).
//!
//! # Not to be confused with
//!
//! `storage::row_locks`'s change epochs (spec R5.4): 1024 shared slots counting ROW
//! changes, bumped by every writer after its write is visible, read by a writer of a
//! row to decide whether a row it read can have moved. Layout epochs count CATALOG
//! changes, per table, and every statement reads them. The two never share a counter.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;

use crate::{Error, Result};

/// The changes of one table's layout in flight: the low bits of its epoch.
const IN_FLIGHT_MASK: u64 = 0xFFFF;

/// A table whose actual stored layout/rows could not be reconciled. This uses
/// the existing epoch load on plan/read validation, and lasts until reopen.
const UNAVAILABLE: u64 = 1 << 63;

/// One change done: what a change adds to its table's epoch, in total.
const ONE_CHANGE: u64 = IN_FLIGHT_MASK + 1;

/// How long a statement keeps retrying a layout change in total before it is
/// refused (decision 2(g)).
pub const RETRY_BUDGET: Duration = Duration::from_millis(100);

/// The first pause between two attempts; it doubles, inside [`RETRY_BUDGET`].
pub const FIRST_PAUSE: Duration = Duration::from_millis(1);

/// What a statement refused for a concurrent schema change says. Its text holds
/// "serialization failure", which every wire already answers 40001.
pub const LAYOUT_CHANGED_MARKER: &str = "could not serialize access due to a concurrent schema change";

/// The key families whose writes change what a reader of a table resolves or decodes
/// (decision 2(a)): the table's definition (main's and a branch level's), its missing
/// values (main's and a frozen branch record's), its shape and layout records (B4,
/// R5.5), its materialized view definition, and its index definitions. Everything a
/// statement of spec R5.5 writes for `DROP COLUMN` and a multi-clause `ALTER` reaches
/// one of them: the schema record always does.
pub const LAYOUT_PREFIXES: &[&[u8]] = &[
    b"meta:table:",
    b"meta:attmissing:",
    b"meta:table_shape:",
    b"meta:table_layout:",
    b"meta:index:",
    b"meta:mv:",
    b"bmeta:",
];

/// One table name's epoch.
#[derive(Debug)]
pub struct EpochSlot {
    value: AtomicU64,
    table: Box<str>,
}

impl EpochSlot {
    fn new(table: &str) -> Self {
        Self {
            value: AtomicU64::new(0),
            table: table.into(),
        }
    }

    /// The epoch now.
    #[inline]
    fn load(&self) -> u64 {
        self.value.load(Ordering::SeqCst)
    }

    fn advance(&self, delta: u64) {
        // Exhaustion must never wrap into an old valid epoch or clear a fence.
        let _ = self.value.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
            Some(
                if value & UNAVAILABLE != 0 || (delta == 1 && value & IN_FLIGHT_MASK == IN_FLIGHT_MASK) {
                    UNAVAILABLE
                } else {
                    value
                        .checked_add(delta)
                        .filter(|next| next & UNAVAILABLE == 0)
                        .unwrap_or(UNAVAILABLE)
                },
            )
        });
    }

    /// The table this slot counts.
    pub fn table(&self) -> &str {
        &self.table
    }
}

/// Whether `value` was read with no change of its table in flight.
#[inline]
fn settled(value: u64) -> bool {
    value & (IN_FLIGHT_MASK | UNAVAILABLE) == 0
}

/// The layout epochs of one engine.
#[derive(Debug, Default)]
pub struct LayoutEpochs {
    slots: DashMap<String, Arc<EpochSlot>>,
}

impl LayoutEpochs {
    pub fn new() -> Self {
        Self::default()
    }

    /// `table`'s slot, made on first use. `None` for a session-private table, which
    /// has no epoch.
    fn slot(&self, table: &str) -> Option<Arc<EpochSlot>> {
        if crate::sql::temp_tables::is_temp_key(table) {
            return None;
        }
        if let Some(slot) = self.slots.get(table) {
            return Some(Arc::clone(slot.value()));
        }
        Some(Arc::clone(
            self.slots
                .entry(table.to_string())
                .or_insert_with(|| Arc::new(EpochSlot::new(table)))
                .value(),
        ))
    }

    /// `table`'s epoch now: 0 for a name nothing ever changed or read. For tests and
    /// diagnostics; a reader records through [`note_table`].
    pub fn current(&self, table: &str) -> u64 {
        self.slots.get(table).map_or(0, |slot| slot.value().load())
    }

    /// Whether a change of `table`'s layout is in flight.
    pub fn in_flight(&self, table: &str) -> bool {
        !settled(self.current(table))
    }

    /// Refuse every stamped read of this table until a fresh engine rebuilds it.
    pub(crate) fn quarantine(&self, table: &str) {
        if let Some(slot) = self.slot(table) {
            slot.value.fetch_or(UNAVAILABLE, Ordering::SeqCst);
        }
    }

    pub(crate) fn check_available(&self, table: &str) -> Result<()> {
        if self.current(table) & UNAVAILABLE != 0 {
            return Err(Error::storage(format!(
                "table '{table}' is unavailable after materialized view reconciliation failed; reopen to rebuild"
            )));
        }
        Ok(())
    }

    /// Tests only: hold a change of `tables` in flight, as a schema statement's write
    /// and publication do, until the returned change drops on this thread.
    #[doc(hidden)]
    pub fn debug_begin_change(&self, tables: &[&str]) -> LayoutChange {
        self.begin_change(tables.iter().copied())
    }

    /// Mark each of `tables` as changing, until the returned change ends (its drop).
    /// A name listed twice counts once.
    pub(crate) fn begin_change<'t>(&self, tables: impl IntoIterator<Item = &'t str>) -> LayoutChange {
        let mut slots: Vec<Arc<EpochSlot>> = Vec::new();
        for table in tables {
            let Some(slot) = self.slot(table) else {
                continue;
            };
            if slots.iter().any(|known| Arc::ptr_eq(known, &slot)) {
                continue;
            }
            slot.advance(1);
            slots.push(slot);
        }
        if !slots.is_empty() {
            // This thread's own change: what it resolves of these tables meanwhile (its
            // publication's own lookups) is not recorded, and the end records the result.
            let _ = STATE.try_with(|state| state.borrow_mut().own.extend(slots.iter().cloned()));
        }
        LayoutChange {
            slots,
            _thread: std::marker::PhantomData,
        }
    }
}

/// A change of some tables' layouts in flight ([`LayoutEpochs::begin_change`]). Its
/// drop ends it: each table's epoch counts one more change and one less in flight,
/// and the change is recorded into this thread's read set (decision 2(e)), so the
/// statement that made it does not refuse itself for it.
#[must_use = "a layout change ends when this drops"]
pub struct LayoutChange {
    slots: Vec<Arc<EpochSlot>>,
    // Drop updates the creator thread's read set and own-change list.
    _thread: std::marker::PhantomData<*const ()>,
}

impl LayoutChange {
    /// No table: nothing to end.
    pub(crate) fn none() -> Self {
        Self {
            slots: Vec::new(),
            _thread: std::marker::PhantomData,
        }
    }

    /// Whether this change names no table.
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The tables this change marks.
    pub(crate) fn tables(&self) -> Vec<String> {
        self.slots.iter().map(|slot| slot.table().to_string()).collect()
    }
}

impl Drop for LayoutChange {
    fn drop(&mut self) {
        for slot in &self.slots {
            slot.advance(ONE_CHANGE - 1);
        }
        // `try_with`: a change can end while the thread's locals are being torn down.
        if !self.slots.is_empty() {
            let _ = STATE.try_with(|state| {
                let mut state = state.borrow_mut();
                for slot in &self.slots {
                    if let Some(at) = state.own.iter().position(|own| Arc::ptr_eq(own, slot)) {
                        state.own.swap_remove(at);
                    }
                    state.rerecord(slot);
                }
            });
        }
    }
}

thread_local! {
    /// Publication scopes open on this thread ([`PublicationScope`]).
    static DEFERRING: Cell<u32> = const { Cell::new(0) };
    /// Changes the write funnel made inside them, ended when the outermost closes.
    static DEFERRED: RefCell<Vec<LayoutChange>> = const { RefCell::new(Vec::new()) };
}

/// A schema statement's write and its publication (what it updates in memory after its
/// write: the schema it caches, its index registrations, the on-commit steps of its
/// `DdlBatch`) as one change of its tables' layouts: a change the write funnel makes
/// inside the scope ends when the outermost scope on the thread closes, so no reader
/// trusts an epoch while the publication is still under way. The statement's
/// synchronous-replication wait stays outside it.
#[must_use = "the changes made inside end when the scope drops"]
pub struct PublicationScope {
    /// Thread-bound: the deferred changes are this thread's.
    _thread: std::marker::PhantomData<*const ()>,
}

impl PublicationScope {
    pub fn enter() -> Self {
        DEFERRING.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self {
            _thread: std::marker::PhantomData,
        }
    }
}

impl Drop for PublicationScope {
    fn drop(&mut self) {
        let outermost = DEFERRING.with(|depth| {
            let left = depth.get().saturating_sub(1);
            depth.set(left);
            left == 0
        });
        if outermost {
            let pending = DEFERRED.with(|deferred| std::mem::take(&mut *deferred.borrow_mut()));
            drop(pending);
        }
    }
}

/// End `change` now, or when the publication scope open on this thread closes.
pub(crate) fn end_or_defer(change: LayoutChange) {
    if change.is_empty() {
        return;
    }
    if DEFERRING.with(Cell::get) > 0 {
        DEFERRED.with(|deferred| deferred.borrow_mut().push(change));
    } else {
        drop(change);
    }
}

/// What the write funnel asks the engine that owns it about a batch carrying layout
/// keys. Installed once, at open, on a store that takes writes.
pub trait LayoutHook: Send + Sync {
    /// The table `meta:index:{name}` belongs to: from `value` for a put, from the
    /// store (still holding the definition) for a delete.
    fn index_table(&self, name: &str, value: Option<&[u8]>) -> Option<String>;

    /// After the batch is in the store, while its tables are in flight: drop what
    /// readers cache of each table's layout, so a reader that loads an epoch after
    /// the change cannot find a cache from before it.
    fn publish(&self, tables: &[String]);
}

/// The table a layout key names, when the key alone says (`meta:index:` does not:
/// [`LayoutKey::Index`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LayoutKey<'k> {
    /// One table, or two readings of an ambiguous key (`meta:attmissing:{t}:{c}` with
    /// a ':' in either name): both are marked.
    Tables(&'k str, Option<&'k str>),
    /// `meta:index:{name}`.
    Index(&'k str),
}

/// Whether `key` is a layout key, and what it names. One or two byte comparisons for
/// a key that is not (`data:`, `v:`, the logical WAL, ...).
pub(crate) fn layout_key(key: &[u8]) -> Option<LayoutKey<'_>> {
    match key.first() {
        Some(b'm') | Some(b'b') => {}
        _ => return None,
    }
    let text = std::str::from_utf8(key).ok()?;
    if let Some(table) = text.strip_prefix("meta:table:") {
        return (!table.is_empty()).then_some(LayoutKey::Tables(table, None));
    }
    if let Some(rest) = text.strip_prefix("meta:attmissing:") {
        let first = rest.split_once(':').map(|(table, _)| table)?;
        let last = rest.rsplit_once(':').map(|(table, _)| table)?;
        if first.is_empty() {
            return None;
        }
        return Some(LayoutKey::Tables(
            first,
            (last != first && !last.is_empty()).then_some(last),
        ));
    }
    for prefix in ["meta:table_shape:", "meta:table_layout:", "meta:mv:"] {
        if let Some(table) = text.strip_prefix(prefix) {
            return (!table.is_empty()).then_some(LayoutKey::Tables(table, None));
        }
    }
    if let Some(name) = text.strip_prefix("meta:index:") {
        return (!name.is_empty()).then_some(LayoutKey::Index(name));
    }
    if let Some(rest) = text.strip_prefix("bmeta:") {
        // `bmeta:{level}:table:{t}` and `bmeta:{level}:missing:{t}`; the level's other
        // records (`fork`) change no table's layout.
        let (level, rest) = rest.split_once(':')?;
        if level.is_empty() || !level.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let table = rest.strip_prefix("table:").or_else(|| rest.strip_prefix("missing:"))?;
        return (!table.is_empty()).then_some(LayoutKey::Tables(table, None));
    }
    None
}

/// One default-family operation, borrowed from a RocksDB batch without copying
/// its row values. The gate shares this pass between layout and delta metadata.
#[derive(Clone, Copy)]
pub(crate) enum BatchKey<'a> {
    Put(&'a [u8], &'a [u8]),
    Delete(&'a [u8]),
    Merge(&'a [u8]),
    Range(&'a [u8], &'a [u8]),
}

/// Walk raw batch keys and skip values in place. `false` means the format is
/// unknown; callers must not assume they saw every mutation in that case.
pub(crate) fn walk_batch_keys<'a>(raw: &'a [u8], mut visit: impl FnMut(BatchKey<'a>)) -> bool {
    let Some(mut input) = raw.get(12..) else {
        return false;
    };
    while let Some((&tag, rest)) = input.split_first() {
        input = rest;
        let (has_family, has_value, two_keys) = match tag {
            0 | 7 => (false, false, false),
            1 | 2 => (false, true, false),
            4 | 8 => (true, false, false),
            5 | 6 => (true, true, false),
            15 => (false, false, true),
            14 => (true, false, true),
            3 => {
                if take_slice(&mut input).is_none() {
                    return false;
                }
                continue;
            }
            _ => return false,
        };
        let family = if has_family {
            match take_varint32(&mut input) {
                Some(family) => family,
                None => return false,
            }
        } else {
            0
        };
        let Some(key) = take_slice(&mut input) else {
            return false;
        };
        let value = if has_value || two_keys {
            match take_slice(&mut input) {
                Some(value) => value,
                None => return false,
            }
        } else {
            &[]
        };
        if family != 0 {
            continue;
        }
        visit(if two_keys {
            BatchKey::Range(key, value)
        } else if !has_value {
            BatchKey::Delete(key)
        } else if tag == 1 || tag == 5 {
            BatchKey::Put(key, value)
        } else {
            BatchKey::Merge(key)
        });
    }
    true
}

/// Every layout key and put value, without copying row payloads.
#[cfg(test)]
pub(crate) fn walk_layout_keys<'a>(raw: &'a [u8], mut visit: impl FnMut(LayoutKey<'a>, Option<&'a [u8]>)) -> bool {
    walk_batch_keys(raw, |operation| {
        let (key, value) = match operation {
            BatchKey::Put(key, value) => (key, Some(value)),
            BatchKey::Delete(key) | BatchKey::Merge(key) | BatchKey::Range(key, _) => (key, None),
        };
        if let Some(found) = layout_key(key) {
            visit(found, value);
        }
    })
}

fn take_varint32(input: &mut &[u8]) -> Option<u32> {
    let mut value: u32 = 0;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = input.split_first()?;
        *input = rest;
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn take_slice<'a>(input: &mut &'a [u8]) -> Option<&'a [u8]> {
    let length = take_varint32(input)? as usize;
    if length > input.len() {
        return None;
    }
    let (value, rest) = input.split_at(length);
    *input = rest;
    Some(value)
}

/// The epochs a plan was built under: what its planning recorded. A cached or pinned
/// plan is used only while [`LayoutStamp::is_current`], and records its stamp into
/// the read set of the statement that uses it ([`LayoutStamp::record`]).
#[derive(Debug, Clone, Default)]
pub struct LayoutStamp {
    entries: Arc<[(Arc<EpochSlot>, u64)]>,
}

impl LayoutStamp {
    /// A stamp naming no table: a plan that resolved no table's layout.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether every epoch the plan was built under had nothing in flight and has not
    /// moved since: one atomic load per table.
    pub fn is_current(&self) -> bool {
        self.entries
            .iter()
            .all(|(slot, value)| settled(*value) && slot.load() == *value)
    }

    /// Record the plan's epochs into this thread's read set and open collectors, as
    /// if its planning had just run.
    pub fn record(&self) {
        if self.entries.is_empty() || ACTIVE.with(Cell::get) == 0 {
            return;
        }
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            for (slot, value) in self.entries.iter() {
                state.record(slot, *value);
            }
        });
    }

    /// The tables of the stamp, for diagnostics and tests.
    pub fn tables(&self) -> Vec<String> {
        self.entries.iter().map(|(slot, _)| slot.table().to_string()).collect()
    }
}

/// One set of recorded epochs, each table once, with the first value seen.
#[derive(Debug, Default)]
struct Recorded {
    entries: Vec<(Arc<EpochSlot>, u64)>,
}

impl Recorded {
    /// Record `value` for `slot`. A second, different value for one table means its
    /// layout moved while the statement was resolving it: reported.
    fn record(&mut self, slot: &Arc<EpochSlot>, value: u64) -> bool {
        match self.entries.iter().find(|(known, _)| Arc::ptr_eq(known, slot)) {
            Some((_, first)) => *first == value,
            None => {
                self.entries.push((Arc::clone(slot), value));
                true
            }
        }
    }

    fn rerecord(&mut self, slot: &Arc<EpochSlot>) {
        let value = slot.load();
        match self.entries.iter_mut().find(|(known, _)| Arc::ptr_eq(known, slot)) {
            Some(entry) => entry.1 = value,
            None => self.entries.push((Arc::clone(slot), value)),
        }
    }

    /// The first table whose recorded epoch is no longer valid. A table this thread is
    /// changing (`own`) is skipped: the end of that change records its result.
    fn first_invalid(&self, own: &[Arc<EpochSlot>]) -> Option<&str> {
        self.entries
            .iter()
            .filter(|(slot, _)| !own.iter().any(|mine| Arc::ptr_eq(mine, slot)))
            .find(|(slot, value)| !settled(*value) || slot.load() != *value)
            .map(|(slot, _)| slot.table())
    }
}

/// This thread's statement read set and plan collectors.
#[derive(Debug, Default)]
struct State {
    /// Statement scopes open on this thread (the outermost owns the read set).
    depth: u32,
    statement: Recorded,
    /// A table whose epoch was seen with two values inside the statement.
    moved: Option<String>,
    /// Why a layout change refuses this statement at once instead of retrying it.
    no_retry: Option<&'static str>,
    /// A write of the statement reached the store: it never retries.
    durable: bool,
    /// Open plan collectors, innermost last.
    collectors: Vec<Recorded>,
    /// Slots of the changes this thread has in flight (one entry per change).
    own: Vec<Arc<EpochSlot>>,
}

impl State {
    fn record(&mut self, slot: &Arc<EpochSlot>, value: u64) {
        if self.own.iter().any(|own| Arc::ptr_eq(own, slot)) {
            return;
        }
        if self.depth > 0 && !self.statement.record(slot, value) && self.moved.is_none() {
            self.moved = Some(slot.table().to_string());
        }
        for collector in &mut self.collectors {
            collector.record(slot, value);
        }
    }

    fn rerecord(&mut self, slot: &Arc<EpochSlot>) {
        if self.own.iter().any(|own| Arc::ptr_eq(own, slot)) {
            // Another change of the table by this thread is still in flight: its end
            // records the result.
            return;
        }
        if self.depth > 0 {
            self.statement.rerecord(slot);
        }
        for collector in &mut self.collectors {
            collector.rerecord(slot);
        }
    }

    fn reset_statement(&mut self) {
        self.statement.entries.clear();
        self.moved = None;
        self.no_retry = None;
        self.durable = false;
    }

    fn validate(&self) -> Result<()> {
        if let Some(table) = &self.moved {
            return Err(layout_changed(table));
        }
        match self.statement.first_invalid(&self.own) {
            Some(table) => Err(layout_changed(table)),
            None => Ok(()),
        }
    }
}

thread_local! {
    /// Statement scopes plus collectors open on this thread: the one load the hot path
    /// pays when nothing is open.
    static ACTIVE: Cell<u32> = const { Cell::new(0) };
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn activate() {
    ACTIVE.with(|active| active.set(active.get().saturating_add(1)));
}

fn deactivate() {
    ACTIVE.with(|active| active.set(active.get().saturating_sub(1)));
}

/// Record `table`'s epoch in `epochs` into this thread's read set and open
/// collectors. Called BEFORE any cache of the table's layout is looked at (decision
/// 2(b)). One thread-local load when nothing is open.
#[inline]
pub(crate) fn note_table(epochs: &LayoutEpochs, table: &str) {
    if ACTIVE.with(Cell::get) == 0 {
        return;
    }
    note_table_slow(epochs, table);
}

/// A main-table dependency sampled before branch resolution, but recorded only
/// if resolution actually inherits main. A branch's own/frozen definition is
/// protected by its statement drain and does not depend on main's later DDL.
pub(crate) struct DeferredTableRead {
    observed: Option<(Arc<EpochSlot>, u64)>,
}

impl DeferredTableRead {
    pub(crate) fn record(self) {
        if let Some((slot, value)) = self.observed {
            STATE.with(|state| state.borrow_mut().record(&slot, value));
        }
    }
}

#[inline]
pub(crate) fn defer_table(epochs: &LayoutEpochs, table: &str) -> DeferredTableRead {
    if ACTIVE.with(Cell::get) == 0 {
        return DeferredTableRead { observed: None };
    }
    defer_table_slow(epochs, table)
}

#[cold]
fn defer_table_slow(epochs: &LayoutEpochs, table: &str) -> DeferredTableRead {
    let observed = epochs.slot(table).map(|slot| {
        let value = slot.load();
        (slot, value)
    });
    DeferredTableRead { observed }
}

#[cold]
fn note_table_slow(epochs: &LayoutEpochs, table: &str) {
    let Some(slot) = epochs.slot(table) else {
        return;
    };
    let value = slot.load();
    STATE.with(|state| state.borrow_mut().record(&slot, value));
}

/// Statements run again for a layout change, and refused for one, since the process
/// started: diagnostics, and what the tests read.
static RETRIES: AtomicU64 = AtomicU64::new(0);
static REFUSALS: AtomicU64 = AtomicU64::new(0);

/// How many times a statement was run again because a layout moved under it.
pub fn retries() -> u64 {
    RETRIES.load(Ordering::Relaxed)
}

/// How many statements were refused (40001) because a layout moved under them.
pub fn refusals() -> u64 {
    REFUSALS.load(Ordering::Relaxed)
}

/// Count one more attempt of a statement (`EmbeddedDatabase::run_layout_statement`).
pub fn count_retry() {
    RETRIES.fetch_add(1, Ordering::Relaxed);
}

/// Count one statement refused for a layout change.
pub fn count_refusal() {
    REFUSALS.fetch_add(1, Ordering::Relaxed);
}

/// The error of a statement whose layout moved under it: SQLSTATE 40001 on every wire.
pub fn layout_changed(table: &str) -> Error {
    Error::Transaction(format!(
        "{LAYOUT_CHANGED_MARKER} (serialization failure): table \"{table}\" was altered while the statement ran; \
         retry the statement"
    ))
}

/// Whether `error` is a statement's refusal for a layout that moved under it, as
/// raised or wrapped on its way out (a commit's "Commit failed: …"). Error paths only:
/// a wrapped error is rendered to be read.
pub fn is_layout_changed(error: &Error) -> bool {
    // A nested graph-publication cause can name a layout refusal after the
    // owning rows applied. Preserve that system error instead of treating it
    // as a fresh statement refusal and replacing it with retryable 40001.
    if error.is_applied_publication_failure() {
        return false;
    }
    match error {
        Error::Transaction(message) if message.starts_with(LAYOUT_CHANGED_MARKER) => true,
        other => other.to_string().contains(LAYOUT_CHANGED_MARKER),
    }
}

/// Note that the running statement did something no retry may repeat (a sequence
/// advanced, an advisory lock taken, a procedure body run): a layout change refuses
/// it at once (decision 2(f)).
pub fn note_side_effect(what: &'static str) {
    if ACTIVE.with(Cell::get) == 0 {
        return;
    }
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.depth > 0 && state.no_retry.is_none() {
            state.no_retry = Some(what);
        }
    });
}

/// Whether an execution helper may repeat work inside the running statement.
/// In particular, a prepared-plan fallback must not bypass the outer retry
/// driver's side-effect/durability rule. With no tracked statement it cannot
/// establish that repeating the work is safe.
pub fn statement_may_retry() -> bool {
    if ACTIVE.with(Cell::get) == 0 {
        return false;
    }
    STATE.with(|state| {
        let state = state.borrow();
        state.depth > 0 && !state.durable && state.no_retry.is_none()
    })
}

/// Whether the running statement's read set is still valid (`true` with none open).
/// Asked before a statement's rows are kept for others (the result cache): a result
/// computed across a layout change is never served again.
pub fn statement_still_valid() -> bool {
    if ACTIVE.with(Cell::get) == 0 {
        return true;
    }
    STATE.with(|state| {
        let state = state.borrow();
        state.depth == 0 || state.validate().is_ok()
    })
}

/// The running statement's read set, validated where an explicit transaction's statement
/// ends (the HDB-008 boundary, `EmbeddedDatabase::run_statement_in_transaction`): a
/// statement there has no durable effect of its own, and a layout that moved under it
/// must fail it INSIDE the boundary, so the transaction is aborted and rows it staged
/// under the old layout are never committed. `Ok` with no statement open, or one past a
/// durable write (validated there).
pub fn validate_statement() -> Result<()> {
    if ACTIVE.with(Cell::get) == 0 {
        return Ok(());
    }
    STATE.with(|state| {
        let state = state.borrow();
        if state.depth == 0 || state.durable {
            return Ok(());
        }
        state.validate()
    })
}

thread_local! {
    /// [`outside_statement`] scopes open on this thread.
    static OUTSIDE: Cell<u32> = const { Cell::new(0) };
}

/// Run `write`, a write the running statement makes on behalf of no statement (a
/// sequence's durable high-water, which no rollback or retry hands back), outside the
/// statement's read set: the write funnel neither validates the read set for it nor
/// counts it as the statement's first durable effect. Without this, a `SELECT` whose
/// first `nextval` refills a block would count as durable from that write on, and
/// skip the validation of its rows at its end.
pub fn outside_statement<T>(write: impl FnOnce() -> T) -> T {
    struct Leave;
    impl Drop for Leave {
        fn drop(&mut self) {
            OUTSIDE.with(|outside| outside.set(outside.get().saturating_sub(1)));
        }
    }
    OUTSIDE.with(|outside| outside.set(outside.get().saturating_add(1)));
    let _leave = Leave;
    write()
}

/// The write funnel, before it applies a write: the statement's last validation sits
/// immediately before its first durable effect (decision 2(d)). `Err` refuses the
/// write; nothing reached the store. With no statement open, a read set still valid,
/// or inside [`outside_statement`], `Ok`.
pub(crate) fn at_durable_effect() -> Result<()> {
    if ACTIVE.with(Cell::get) == 0 || OUTSIDE.with(Cell::get) > 0 {
        return Ok(());
    }
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.depth == 0 {
            return Ok(());
        }
        match state.validate() {
            Ok(()) => {
                state.durable = true;
                Ok(())
            }
            Err(error) => {
                if state.durable && state.no_retry.is_none() {
                    state.no_retry = Some("an earlier write of the statement is already durable");
                }
                Err(error)
            }
        }
    })
}

/// A client statement's read set (decision 2(f): one statement is the unit of retry).
/// The outermost scope on a thread owns it; an execution nested inside a statement
/// joins the read set of the statement it runs in.
#[must_use = "the read set closes when the scope drops"]
pub struct StatementScope {
    outermost: bool,
    // Enter, validation and Drop must all use the same thread-local state.
    _thread: std::marker::PhantomData<*const ()>,
}

impl StatementScope {
    /// Open (or join) this thread's statement read set.
    pub fn enter() -> Self {
        let outermost = STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.depth += 1;
            if state.depth == 1 {
                state.reset_statement();
                true
            } else {
                false
            }
        });
        activate();
        Self {
            outermost,
            _thread: std::marker::PhantomData,
        }
    }

    /// Whether this scope owns the read set (and so decides on retries).
    pub fn is_outermost(&self) -> bool {
        self.outermost
    }

    /// Forget what the previous attempt recorded, for a new one.
    pub fn reset(&self) {
        if self.outermost {
            STATE.with(|state| state.borrow_mut().reset_statement());
        }
    }

    /// The last validation of a statement that returned: done at its first durable
    /// effect when it had one, here otherwise.
    pub fn finish(&self) -> Result<()> {
        STATE.with(|state| {
            let state = state.borrow();
            if state.durable {
                return Ok(());
            }
            state.validate()
        })
    }

    /// Whether the failed attempt may run again: no write of it reached the store and
    /// it did nothing a retry would repeat.
    pub fn retriable(&self) -> bool {
        STATE.with(|state| {
            let state = state.borrow();
            !state.durable && state.no_retry.is_none()
        })
    }

    /// Why the failed attempt may not run again, when it may not.
    pub fn no_retry_reason(&self) -> Option<&'static str> {
        STATE.with(|state| {
            let state = state.borrow();
            state
                .no_retry
                .or_else(|| state.durable.then_some("part of the statement is already durable"))
        })
    }
}

impl Drop for StatementScope {
    fn drop(&mut self) {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.depth = state.depth.saturating_sub(1);
            if self.outermost {
                state.reset_statement();
            }
        });
        deactivate();
    }
}

/// The epochs a planning records, for the plan's [`LayoutStamp`]. Open while the
/// planner runs; [`StampCollector::finish`] hands the stamp back.
#[must_use = "the stamp is collected until the collector finishes or drops"]
pub struct StampCollector {
    open: bool,
    // Finish/Drop pops the creator thread's collector stack.
    _thread: std::marker::PhantomData<*const ()>,
}

impl StampCollector {
    /// Start collecting on this thread (collectors nest: an inner planning's epochs
    /// are its outer planning's too).
    pub fn start() -> Self {
        STATE.with(|state| state.borrow_mut().collectors.push(Recorded::default()));
        activate();
        Self {
            open: true,
            _thread: std::marker::PhantomData,
        }
    }

    /// The stamp of what was planned since [`Self::start`].
    pub fn finish(mut self) -> LayoutStamp {
        self.open = false;
        let recorded = STATE
            .with(|state| state.borrow_mut().collectors.pop())
            .unwrap_or_default();
        deactivate();
        // What an inner planning recorded, the outer one used too.
        if !recorded.entries.is_empty() {
            STATE.with(|state| {
                let mut state = state.borrow_mut();
                if let Some(outer) = state.collectors.last_mut() {
                    for (slot, value) in &recorded.entries {
                        outer.record(slot, *value);
                    }
                }
            });
        }
        LayoutStamp {
            entries: recorded.entries.into(),
        }
    }
}

impl Drop for StampCollector {
    fn drop(&mut self) {
        if self.open {
            STATE.with(|state| state.borrow_mut().collectors.pop());
            deactivate();
        }
    }
}

/// Run `plan` with a collector open: the plan and the epochs its planning recorded.
pub fn collect_stamp<T>(plan: impl FnOnce() -> T) -> (T, LayoutStamp) {
    let collector = StampCollector::start();
    let planned = plan();
    (planned, collector.finish())
}

/// The pause before the next attempt, or `None` when the budget is spent: `pause`
/// doubled, inside what is left of [`RETRY_BUDGET`] after `spent`.
pub fn next_pause(spent: Duration, pause: Duration) -> Option<Duration> {
    let left = RETRY_BUDGET.checked_sub(spent)?;
    if left.is_zero() {
        return None;
    }
    Some(pause.min(left))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn an_applied_publication_cause_is_not_a_retryable_layout_refusal() {
        let original = layout_changed("published_rows");
        assert!(is_layout_changed(&original));
        let applied = Error::applied_publication_failure(original);
        let message = applied.to_string();
        assert!(message.contains(LAYOUT_CHANGED_MARKER), "retain original derived cause");
        for error in [
            applied,
            Error::transaction(format!("Commit failed: {message}")),
            Error::query_execution(message.clone()),
            Error::Generic(message.clone()),
        ] {
            assert!(error.is_applied_publication_failure(), "{error}");
            assert!(
                !is_layout_changed(&error),
                "applied error cannot enter layout retry/replacement: {error}"
            );
        }
        assert!(is_layout_changed(&Error::transaction(format!(
            "Commit failed: {}",
            layout_changed("t")
        ))));
    }

    #[test]
    fn a_change_is_in_flight_until_it_ends_and_moves_the_epoch_once() {
        let epochs = LayoutEpochs::new();
        assert_eq!(epochs.current("t"), 0);
        let change = epochs.begin_change(["t", "t"]);
        assert!(epochs.in_flight("t"), "a change in flight is seen");
        drop(change);
        assert!(!epochs.in_flight("t"));
        assert_eq!(epochs.current("t"), ONE_CHANGE, "one change, counted once");
    }

    #[test]
    fn two_changes_of_one_table_in_flight_at_once_are_both_seen() {
        let epochs = LayoutEpochs::new();
        let first = epochs.begin_change(["t"]);
        let second = epochs.begin_change(["t"]);
        drop(first);
        assert!(epochs.in_flight("t"), "the second is still in flight");
        drop(second);
        assert!(!epochs.in_flight("t"));
        assert_eq!(epochs.current("t"), 2 * ONE_CHANGE);
    }

    #[test]
    fn quarantine_refuses_old_and_new_stamps_and_survives_change_publication() {
        let epochs = LayoutEpochs::new();
        let (_, old) = collect_stamp(|| note_table(&epochs, "t"));
        assert!(old.is_current());
        let change = epochs.begin_change(["t"]);
        epochs.quarantine("t");
        drop(change);
        assert!(!old.is_current());
        assert!(epochs.check_available("t").is_err());
        let (_, fresh) = collect_stamp(|| note_table(&epochs, "t"));
        assert!(!fresh.is_current(), "planning afresh cannot clear quarantine");
        drop(epochs.begin_change(["t"]));
        assert!(!fresh.is_current());
        assert!(epochs.in_flight("t"));
        assert!(epochs.check_available("unrelated").is_ok());
    }

    #[test]
    fn epoch_exhaustion_and_inflight_exhaustion_fail_closed_without_wrapping() {
        for value in [UNAVAILABLE - ONE_CHANGE, IN_FLIGHT_MASK] {
            let epochs = LayoutEpochs::new();
            let slot = epochs.slot("t").unwrap();
            slot.value.store(value, Ordering::SeqCst);
            drop(epochs.begin_change(["t"]));
            assert_eq!(epochs.current("t"), UNAVAILABLE);
            assert!(epochs.check_available("t").is_err());
            let (_, stamp) = collect_stamp(|| note_table(&epochs, "t"));
            assert!(!stamp.is_current());
            drop(epochs.begin_change(["t"]));
            assert_eq!(epochs.current("t"), UNAVAILABLE);
        }
    }

    #[test]
    fn a_temp_table_has_no_epoch() {
        let epochs = LayoutEpochs::new();
        let change = epochs.begin_change(["pg_temp_7.scratch"]);
        assert!(change.is_empty());
        drop(change);
        assert_eq!(epochs.current("pg_temp_7.scratch"), 0);
    }

    /// A change made by another writer: begun and ended on a thread of its own.
    fn change_elsewhere(epochs: &Arc<LayoutEpochs>, table: &'static str) {
        let epochs = Arc::clone(epochs);
        std::thread::spawn(move || drop(epochs.begin_change([table])))
            .join()
            .unwrap();
    }

    #[test]
    fn a_read_set_is_valid_until_a_recorded_table_moves() {
        let epochs = Arc::new(LayoutEpochs::new());
        let scope = StatementScope::enter();
        note_table(&epochs, "t");
        note_table(&epochs, "u");
        assert!(scope.finish().is_ok());
        change_elsewhere(&epochs, "u");
        let error = scope.finish().unwrap_err();
        assert!(is_layout_changed(&error), "{error}");
        assert!(error.to_string().contains("\"u\""), "{error}");
        assert!(scope.retriable());
    }

    #[test]
    fn a_table_read_while_another_writers_change_is_in_flight_fails_validation() {
        let epochs = Arc::new(LayoutEpochs::new());
        let scope = StatementScope::enter();
        let (begun_tx, begun_rx) = std::sync::mpsc::channel();
        let (end_tx, end_rx) = std::sync::mpsc::channel::<()>();
        let writer = {
            let epochs = Arc::clone(&epochs);
            std::thread::spawn(move || {
                let change = epochs.begin_change(["t"]);
                begun_tx.send(()).unwrap();
                end_rx.recv().unwrap();
                drop(change);
            })
        };
        begun_rx.recv().unwrap();
        note_table(&epochs, "t");
        end_tx.send(()).unwrap();
        writer.join().unwrap();
        assert!(scope.finish().is_err(), "read in flight");
    }

    #[test]
    fn a_statements_own_change_is_rerecorded() {
        let epochs = LayoutEpochs::new();
        let scope = StatementScope::enter();
        note_table(&epochs, "t");
        let change = epochs.begin_change(["t"]);
        note_table(&epochs, "t");
        drop(change);
        note_table(&epochs, "t");
        assert!(scope.finish().is_ok(), "decision 2(e)");
    }

    #[test]
    fn a_change_inside_a_publication_scope_ends_with_the_scope() {
        let epochs = LayoutEpochs::new();
        let publication = PublicationScope::enter();
        end_or_defer(epochs.begin_change(["t"]));
        assert!(epochs.in_flight("t"), "deferred to the end of the publication");
        drop(publication);
        assert!(!epochs.in_flight("t"));
        assert_eq!(epochs.current("t"), ONE_CHANGE);
    }

    #[test]
    fn a_nested_scope_joins_and_only_the_outermost_resets() {
        let epochs = Arc::new(LayoutEpochs::new());
        let outer = StatementScope::enter();
        assert!(outer.is_outermost());
        note_table(&epochs, "t");
        {
            let inner = StatementScope::enter();
            assert!(!inner.is_outermost());
            inner.reset();
        }
        change_elsewhere(&epochs, "t");
        assert!(outer.finish().is_err(), "the inner scope kept the outer read set");
    }

    #[test]
    fn a_durable_write_is_the_last_validation_and_ends_retries() {
        let epochs = Arc::new(LayoutEpochs::new());
        let scope = StatementScope::enter();
        note_table(&epochs, "t");
        assert!(at_durable_effect().is_ok());
        change_elsewhere(&epochs, "t");
        assert!(scope.finish().is_ok(), "validated at the write");
        assert!(at_durable_effect().is_err(), "a later write still sees the move");
        assert!(!scope.retriable());
    }

    #[test]
    fn a_statements_second_write_inside_its_own_publication_is_not_refused() {
        let epochs = LayoutEpochs::new();
        let scope = StatementScope::enter();
        note_table(&epochs, "t");
        let publication = PublicationScope::enter();
        assert!(at_durable_effect().is_ok());
        end_or_defer(epochs.begin_change(["t"]));
        assert!(at_durable_effect().is_ok(), "its own change in flight is not a move");
        drop(publication);
        assert!(at_durable_effect().is_ok());
        assert!(scope.finish().is_ok());
    }

    #[test]
    fn a_write_outside_the_statement_is_not_its_durable_effect() {
        let epochs = Arc::new(LayoutEpochs::new());
        let scope = StatementScope::enter();
        note_table(&epochs, "t");
        change_elsewhere(&epochs, "t");
        assert!(outside_statement(at_durable_effect).is_ok(), "a sequence's high-water");
        assert!(scope.retriable(), "not durable");
        assert!(scope.finish().is_err(), "still validated where the statement ends");
    }

    #[test]
    fn a_side_effect_ends_retries() {
        let scope = StatementScope::enter();
        assert!(scope.retriable());
        note_side_effect("nextval");
        assert!(!scope.retriable());
        assert_eq!(scope.no_retry_reason(), Some("nextval"));
    }

    #[test]
    fn a_stamp_is_what_planning_recorded_and_is_current_until_a_table_moves() {
        let epochs = Arc::new(LayoutEpochs::new());
        let ((), stamp) = collect_stamp(|| {
            note_table(&epochs, "t");
            let ((), inner) = collect_stamp(|| note_table(&epochs, "u"));
            assert_eq!(inner.tables(), vec!["u".to_string()]);
        });
        assert_eq!(stamp.tables(), vec!["t".to_string(), "u".to_string()]);
        assert!(stamp.is_current());
        change_elsewhere(&epochs, "u");
        assert!(!stamp.is_current());
        let scope = StatementScope::enter();
        stamp.record();
        assert!(
            scope.finish().is_err(),
            "a stale stamp fails the statement that used it"
        );
    }

    #[test]
    fn layout_keys_name_their_tables() {
        assert_eq!(layout_key(b"meta:table:t"), Some(LayoutKey::Tables("t", None)));
        assert_eq!(layout_key(b"meta:attmissing:t:c"), Some(LayoutKey::Tables("t", None)));
        assert_eq!(
            layout_key(b"meta:attmissing:a:b:c"),
            Some(LayoutKey::Tables("a", Some("a:b")))
        );
        assert_eq!(layout_key(b"meta:table_shape:t"), Some(LayoutKey::Tables("t", None)));
        assert_eq!(layout_key(b"meta:table_layout:t"), Some(LayoutKey::Tables("t", None)));
        assert_eq!(layout_key(b"meta:index:i"), Some(LayoutKey::Index("i")));
        assert_eq!(layout_key(b"bmeta:3:table:t"), Some(LayoutKey::Tables("t", None)));
        assert_eq!(layout_key(b"bmeta:3:missing:t"), Some(LayoutKey::Tables("t", None)));
        assert_eq!(layout_key(b"bmeta:3:fork"), None);
        assert_eq!(layout_key(b"meta:table_incarnation:t"), None);
        assert_eq!(layout_key(b"data:t:1"), None);
        assert_eq!(layout_key(b"meta:history_purge:t"), None);
    }

    #[test]
    fn the_walker_finds_layout_keys_in_a_write_batch() {
        let mut batch = rocksdb::WriteBatch::default();
        batch.put(b"data:t:1", b"row");
        batch.put(b"meta:table:t", b"schema");
        batch.delete(b"meta:attmissing:t:c");
        batch.put(b"meta:index:i", b"definition");
        let mut seen = Vec::new();
        assert!(walk_layout_keys(batch.data(), |key, value| seen.push((key, value.map(<[u8]>::to_vec)))));
        assert_eq!(
            seen,
            vec![
                (LayoutKey::Tables("t", None), Some(b"schema".to_vec())),
                (LayoutKey::Tables("t", None), None),
                (LayoutKey::Index("i"), Some(b"definition".to_vec())),
            ]
        );
    }

    #[test]
    fn the_pause_stays_inside_the_budget() {
        assert_eq!(next_pause(Duration::ZERO, FIRST_PAUSE), Some(FIRST_PAUSE));
        assert_eq!(
            next_pause(Duration::from_millis(99), Duration::from_millis(8)),
            Some(Duration::from_millis(1))
        );
        assert_eq!(next_pause(RETRY_BUDGET, FIRST_PAUSE), None);
    }
}
