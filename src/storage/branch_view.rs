//! sprinter 6bc4a4040eb7 (phase B2): THE branch view — every read and write
//! of a table on a branch goes through it.
//!
//! # The model
//!
//! A branch `b` with ancestor chain `main → l1 → … → b` sees table `t` as
//! main's current rows with each level's overlay applied in chain order:
//!
//! * `bdata:{level}:{t}:{row}` — that level's own image of the row;
//! * `bdel:{level}:{t}:{row}`  — that level hides the row.
//!
//! For every row the NEAREST level that holds an entry for it decides; within
//! one level a tombstone beats an image (a pre-B2 delete wrote the tombstone
//! and left the image behind). A child's own image therefore beats an
//! ancestor's tombstone — the pre-B2 scan applied every tombstone of the chain
//! last, which hid a child's own row whenever its parent had deleted the same
//! row id.
//!
//! Phase B4 freezes a parent's pre-image into each live child at the parent's
//! write (marked `bfrz:`); the frozen copy is an ordinary `bdata` / `bdel` of
//! the child, so the read model here stays exactly "nearest level wins" and
//! nothing in this module reads history. [`BranchView::for_chain`] and
//! [`BranchView::own_entry`] are the two entry points that hook needs (a view
//! of any level `P`, and "does `C` already have its own entry for this row"),
//! and [`bump_write_generation`] is what it must call for every level it
//! writes.
//!
//! # Writes
//!
//! [`BranchView::put`] writes the branch's own image and clears its own
//! tombstone; [`BranchView::delete`] clears the own image and ALWAYS writes a
//! tombstone (it is what hides an inherited row); [`BranchView::insert`] is
//! `put` for a row id the caller just allocated (no level can hold an entry
//! for it, so there is no tombstone to clear). Each takes the caller's
//! transaction: with one the writes are STAGED in its write set (COMMIT
//! applies them to the branch only, ROLLBACK and ROLLBACK TO SAVEPOINT discard
//! them), without one they are written at once in one atomic batch followed by
//! the autocommit durability barrier. Reads take the same transaction and
//! merge its staged branch writes over the committed view, so a transaction
//! reads its own writes.
//!
//! Branch writes are never appended to the logical WAL: a branch is a local
//! sandbox and a standby exists to take over serving main (the commit batch's
//! `collect_logical_wal_ops` already ships only `data:` keys).
//!
//! # Constraints, and what they cost
//!
//! A branch's rows are in no ART index (the indexes are process-wide and
//! describe main). PRIMARY KEY / UNIQUE on a branch are enforced over the rows
//! the branch SEES by [`BranchView::rows_with_key`], from three sources:
//!
//! * **main** — one probe of main's index; a candidate counts only when no
//!   level of the chain decides the row and main still holds it with that
//!   key. While an open MAIN transaction has removed index entries of the
//!   table (a staged DELETE, a key-moving UPDATE, a TRUNCATE) main's index
//!   under-reports committed rows, so main's candidates then come from main's
//!   committed rows instead (one scan per statement, memoised in the
//!   statement's scope);
//! * **the committed overlay** of every chain level — decoded once and cached
//!   ACROSS statements, keyed by branch and table and validated by the write
//!   generation of every chain level plus the engine's schema generation
//!   (bound: about [`COMMITTED_OVERLAY_ROW_BUDGET`] cached rows in at most
//!   [`COMMITTED_OVERLAY_MAX_ENTRIES`] tables, least recently used evicted
//!   first; an overlay larger than the budget is not cached). A commit or a
//!   direct write applies its own rows to the cached copy instead of
//!   invalidating it, so a writer that is alone on the branch never reloads
//!   (a failed commit only invalidates; a physical standby, whose rows arrive
//!   below every generation bump, never caches);
//! * **the transaction's staged writes** — an index kept per transaction and
//!   per table as the view stages them ([`BranchStagedWrites`], proportional
//!   to the transaction's own branch writes), never a rescan of the write set
//!   (except once per table after a ROLLBACK TO SAVEPOINT rewound it).
//!
//! So a constrained INSERT costs O(1) lookups plus one index probe; N
//! statements cost O(N) once the overlay is cached.
//!
//! Two writers of the same key on the same branch are serialised by a claim
//! ([`BranchKeyClaim`]) taken BEFORE the visibility check and held until the
//! claiming transaction ends or rolls back past the savepoint that preceded
//! it. A committer bumps the level's write generation before it releases its
//! claims, so whoever claims the key next sees the bump and revalidates its
//! cached overlay before it checks. Claims cost one entry per distinct key a
//! transaction wrote. `INSERT … ON CONFLICT` arbitrates with the claims too
//! ([`BranchView::arbitrate`]): a key another open transaction claimed is a
//! conflict `DO NOTHING` skips, as main's eager index entry of an uncommitted
//! row is.
//!
//! # Visibility inside a transaction
//!
//! A branch transaction reads the branch's committed rows as of each
//! statement (READ COMMITTED, PostgreSQL's default) merged with its own
//! staged writes — not a snapshot of the transaction's start. The overlay
//! keyspace has no version index to read a snapshot from, and after B4 a
//! branch no longer depends on its parent's later writes.

use super::StorageEngine;
use crate::storage::art_index::ArtIndexError;
use crate::storage::{ArtIndexManager, EnforcedUniqueIndex, Transaction};
use crate::{Error, Result, Schema, Tuple, Value};
use dashmap::DashMap;
use rocksdb::{Direction, IteratorMode, ReadOptions, WriteBatch};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

// ---------------------------------------------------------------------------
// Key formats
// ---------------------------------------------------------------------------

/// `bdata:{level}:{table}:{row_id}` — level `level`'s own image of a row.
pub fn branch_row_key(level: u64, table: &str, row_id: u64) -> Vec<u8> {
    format!("bdata:{level}:{table}:{row_id}").into_bytes()
}

/// `bdel:{level}:{table}:{row_id}` — level `level` hides the row.
pub fn branch_tombstone_key(level: u64, table: &str, row_id: u64) -> Vec<u8> {
    format!("bdel:{level}:{table}:{row_id}").into_bytes()
}

fn row_prefix(level: u64, table: &str) -> String {
    format!("bdata:{level}:{table}:")
}

fn tombstone_prefix(level: u64, table: &str) -> String {
    format!("bdel:{level}:{table}:")
}

/// One overlay level's entry for one row.
#[derive(Debug, Clone)]
pub enum LevelEntry {
    /// The level's own image of the row.
    Row(Tuple),
    /// The level hides the row.
    Tombstone,
}

impl LevelEntry {
    fn into_visible(self) -> Option<Tuple> {
        match self {
            Self::Row(tuple) => Some(tuple),
            Self::Tombstone => None,
        }
    }
}

/// A PRIMARY KEY / UNIQUE rule a proposed row would violate on a branch, and
/// the visible row that already holds the key.
#[derive(Debug, Clone)]
pub struct BranchUniqueConflict {
    /// The rule (index name, user-facing label, columns, PK or UNIQUE).
    pub rule: EnforcedUniqueIndex,
    /// The visible row holding the key (its `row_id` is set).
    pub row: Tuple,
}

impl BranchUniqueConflict {
    /// The conflicting row's id.
    pub fn row_id(&self) -> Option<u64> {
        self.row.row_id
    }

    /// The violation text — byte-for-byte what main's ART check reports for
    /// the same rule (`ArtIndexError::DuplicateKey`), so the wire maps it to
    /// 23505 and an ORM parses the same constraint name.
    pub fn message(&self) -> String {
        duplicate_key_message(&self.rule)
    }

    /// The 23505 error.
    pub fn into_error(self) -> Error {
        Error::constraint_violation(self.message())
    }
}

fn duplicate_key_message(rule: &EnforcedUniqueIndex) -> String {
    let kind = if rule.is_primary_key { "PRIMARY KEY" } else { "UNIQUE" };
    ArtIndexError::DuplicateKey(format!(
        "Duplicate key value violates {kind} constraint \"{}\"",
        rule.label
    ))
    .to_string()
}

fn claimed_key_message(rule: &EnforcedUniqueIndex, branch: &str) -> String {
    format!(
        "{} (a concurrent transaction on branch \"{branch}\" holds the key)",
        duplicate_key_message(rule)
    )
}

// ---------------------------------------------------------------------------
// Key claims (concurrent writers of one key on one branch)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClaimKey {
    engine: u64,
    branch: u64,
    table: String,
    index: String,
    key: Vec<u8>,
}

/// Every live claim: key → owner (a transaction id, or a direct-write owner).
static CLAIMS: LazyLock<DashMap<ClaimKey, u64>> = LazyLock::new(DashMap::new);

/// Owners for writes made with no transaction. Counts DOWN from `u64::MAX`;
/// transaction ids count up from 1, so the two never meet.
static DIRECT_OWNER: AtomicU64 = AtomicU64::new(u64::MAX);

/// sprinter 6bc4a4040eb7 (phase B2): one PRIMARY KEY / UNIQUE key claimed on
/// one branch by one writer. A branch's rows are in no index, so this claim is
/// what a second writer of the same key on the same branch collides with
/// before either has committed (main's eager ART insert plays that part on
/// main). Released on drop: a transaction holds its claims until it ends
/// (`Transaction::hold_branch_claim`) or rolls back past the savepoint that
/// preceded them, a direct write until its batch is applied.
#[derive(Debug)]
pub struct BranchKeyClaim {
    key: ClaimKey,
    owner: u64,
}

impl Drop for BranchKeyClaim {
    fn drop(&mut self) {
        CLAIMS.remove_if(&self.key, |_, owner| *owner == self.owner);
    }
}

// ---------------------------------------------------------------------------
// Write generations
// ---------------------------------------------------------------------------

/// `(engine instance, branch level, table)`.
type GenerationKey = (u64, u64, String);

/// sprinter 6bc4a4040eb7 (phase B2): how many times writes to one table on one
/// branch level have become visible (committed, or written at once). A cached
/// committed overlay records the generation of every level of its chain and
/// is revalidated against them on every use. A writer bumps the generation
/// AFTER its rows are visible and BEFORE it releases its claims — so whoever
/// claims the key next sees the bump and reloads (or has the writer's rows
/// already applied to its cached copy) before it checks.
static GENERATIONS: LazyLock<DashMap<GenerationKey, u64>> = LazyLock::new(DashMap::new);

fn current_generation(engine: u64, level: u64, table: &str) -> u64 {
    GENERATIONS
        .get(&(engine, level, table.to_string()))
        .map_or(0, |generation| *generation)
}

/// Bump the write generation of `table` on `level` (and return the new
/// value) — for every writer that makes branch overlay writes visible outside
/// the view's own bookkeeping (MERGE into a branch; phase B4's freeze hook
/// must call it for every child level it writes).
pub fn bump_write_generation(engine: u64, level: u64, table: &str) -> u64 {
    let mut generation = GENERATIONS.entry((engine, level, table.to_string())).or_insert(0);
    *generation += 1;
    *generation
}

// ---------------------------------------------------------------------------
// A set of rows with lazily built key maps
// ---------------------------------------------------------------------------

/// Rows a level (or a transaction) decides: `Some` = visible image, `None` =
/// hidden; plus, per probed column set, the encoded key → visible row ids.
#[derive(Debug, Default, Clone)]
struct RowSet {
    rows: HashMap<u64, Option<Tuple>>,
    /// Sorted column positions → encoded key → visible rows with that key.
    keys: HashMap<Vec<usize>, HashMap<Vec<u8>, Vec<u64>>>,
}

impl RowSet {
    fn from_rows(rows: HashMap<u64, Option<Tuple>>) -> Self {
        Self {
            rows,
            keys: HashMap::new(),
        }
    }

    fn ensure_key_map(&mut self, positions: &[usize]) {
        if self.keys.contains_key(positions) {
            return;
        }
        let mut map: HashMap<Vec<u8>, Vec<u64>> = HashMap::new();
        for (row_id, state) in &self.rows {
            if let Some(tuple) = state {
                if let Some(key) = encode_positions(tuple, positions) {
                    map.entry(key).or_default().push(*row_id);
                }
            }
        }
        self.keys.insert(positions.to_vec(), map);
    }

    /// Visible rows whose `positions` encode to `encoded`.
    fn matching(&mut self, positions: &[usize], encoded: &[u8]) -> Vec<&Tuple> {
        self.ensure_key_map(positions);
        let ids: Vec<u64> = self
            .keys
            .get(positions)
            .and_then(|map| map.get(encoded))
            .cloned()
            .unwrap_or_default();
        ids.iter()
            .filter_map(|row_id| self.rows.get(row_id).and_then(Option::as_ref))
            .collect()
    }

    /// Does this set decide row `row_id` (an image or a tombstone)?
    fn decides(&self, row_id: u64) -> bool {
        self.rows.contains_key(&row_id)
    }

    /// Record a write: row `row_id` is now `image` (`None` = hidden).
    fn note(&mut self, row_id: u64, image: Option<&Tuple>) {
        let Self { rows, keys } = self;
        if let Some(Some(old)) = rows.get(&row_id) {
            for (positions, map) in keys.iter_mut() {
                if let Some(key) = encode_positions(old, positions) {
                    if let Some(ids) = map.get_mut(&key) {
                        ids.retain(|id| *id != row_id);
                        if ids.is_empty() {
                            map.remove(&key);
                        }
                    }
                }
            }
        }
        match image {
            Some(tuple) => {
                for (positions, map) in keys.iter_mut() {
                    if let Some(key) = encode_positions(tuple, positions) {
                        map.entry(key).or_default().push(row_id);
                    }
                }
                let mut stored = tuple.clone();
                stored.row_id = Some(row_id);
                rows.insert(row_id, Some(stored));
            }
            None => {
                rows.insert(row_id, None);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The committed-overlay cache (process-wide, across statements)
// ---------------------------------------------------------------------------

/// Upper bound on the overlay rows the committed-overlay cache keeps decoded
/// across statements (a cached overlay that grows through fast-forwarded
/// commits is re-accounted as it grows). An overlay larger than this is never
/// cached — it is reloaded by each statement that probes it.
pub const COMMITTED_OVERLAY_ROW_BUDGET: usize = 1_000_000;

/// Upper bound on the number of (branch, table) overlays cached at once.
pub const COMMITTED_OVERLAY_MAX_ENTRIES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CommittedKey {
    engine: u64,
    /// The view's own branch (its chain is a property of the branch).
    branch: u64,
    table: String,
}

/// One branch's committed overlay of one table: every row some level of its
/// chain decides, nearest level winning.
#[derive(Debug)]
struct CommittedOverlay {
    width: usize,
    schema_generation: u64,
    /// The write generation of each chain level (chain order) it reflects.
    generations: Vec<u64>,
    set: RowSet,
}

#[derive(Debug)]
struct CachedOverlay {
    overlay: Arc<parking_lot::Mutex<CommittedOverlay>>,
    rows: usize,
    last_used: u64,
}

#[derive(Debug, Default)]
struct CommittedCache {
    entries: HashMap<CommittedKey, CachedOverlay>,
    rows: usize,
    clock: u64,
}

impl CommittedCache {
    fn lookup(&mut self, key: &CommittedKey) -> Option<Arc<parking_lot::Mutex<CommittedOverlay>>> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(key).map(|entry| {
            entry.last_used = clock;
            Arc::clone(&entry.overlay)
        })
    }

    fn admit(&mut self, key: CommittedKey, overlay: Arc<parking_lot::Mutex<CommittedOverlay>>, rows: usize) {
        if let Some(previous) = self.entries.remove(&key) {
            self.rows = self.rows.saturating_sub(previous.rows);
        }
        if rows > COMMITTED_OVERLAY_ROW_BUDGET {
            return;
        }
        while !self.entries.is_empty()
            && (self.rows + rows > COMMITTED_OVERLAY_ROW_BUDGET || self.entries.len() >= COMMITTED_OVERLAY_MAX_ENTRIES)
        {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&victim) {
                self.rows = self.rows.saturating_sub(evicted.rows);
            }
        }
        self.clock += 1;
        self.rows += rows;
        self.entries.insert(
            key,
            CachedOverlay {
                overlay,
                rows,
                last_used: self.clock,
            },
        );
    }

    /// The cached `overlay` of `key` now holds `rows` rows (a fast-forward
    /// grew it): re-account it, and keep the cache within its row budget —
    /// an overlay grown past the budget is dropped, and least recently used
    /// others make room for a grown one.
    fn resize(&mut self, key: &CommittedKey, overlay: &Arc<parking_lot::Mutex<CommittedOverlay>>, rows: usize) {
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        if !Arc::ptr_eq(&entry.overlay, overlay) {
            return;
        }
        self.rows = self.rows.saturating_sub(entry.rows) + rows;
        entry.rows = rows;
        if rows > COMMITTED_OVERLAY_ROW_BUDGET {
            if let Some(removed) = self.entries.remove(key) {
                self.rows = self.rows.saturating_sub(removed.rows);
            }
            return;
        }
        while self.rows > COMMITTED_OVERLAY_ROW_BUDGET {
            let Some(victim) = self
                .entries
                .iter()
                .filter(|(other, _)| *other != key)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(other, _)| other.clone())
            else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&victim) {
                self.rows = self.rows.saturating_sub(evicted.rows);
            }
        }
    }
}

static COMMITTED: LazyLock<parking_lot::Mutex<CommittedCache>> =
    LazyLock::new(|| parking_lot::Mutex::new(CommittedCache::default()));

/// `writes` just became visible on `level` of `table` as write generation
/// `generation`: apply them to the cached overlay of the branch whose OWN
/// level that is, when that copy was current right before (otherwise it is
/// left to reload). Overlays of the level's descendants are not touched: the
/// bump makes them reload.
fn fast_forward<'t>(
    engine: u64,
    level: u64,
    table: &str,
    generation: u64,
    writes: impl IntoIterator<Item = (u64, Option<&'t Tuple>)>,
) {
    let key = CommittedKey {
        engine,
        branch: level,
        table: table.to_string(),
    };
    let cached = COMMITTED
        .lock()
        .entries
        .get(&key)
        .map(|entry| Arc::clone(&entry.overlay));
    let Some(cached) = cached else {
        return;
    };
    let mut overlay = cached.lock();
    let current_before = overlay
        .generations
        .last()
        .is_some_and(|own| own.saturating_add(1) == generation);
    if !current_before {
        return;
    }
    for (row_id, image) in writes {
        overlay.set.note(row_id, image);
    }
    if let Some(own) = overlay.generations.last_mut() {
        *own = generation;
    }
    let rows = overlay.set.rows.len();
    // The cache lock is never taken while an overlay is held.
    drop(overlay);
    COMMITTED.lock().resize(&key, &cached, rows);
}

// ---------------------------------------------------------------------------
// A transaction's staged branch writes
// ---------------------------------------------------------------------------

/// sprinter 6bc4a4040eb7 (phase B2): the branch rows a transaction has staged,
/// per (level, table), kept as the view stages them — so a constraint check
/// inside the transaction reads them in O(1) instead of rescanning the write
/// set. Held by the transaction (`Transaction::branch_staged`); published to
/// the committed-overlay cache by a successful COMMIT
/// ([`Self::publish`]), dropped by a ROLLBACK, rewound by ROLLBACK TO
/// SAVEPOINT ([`Self::rewound`]). Memory: one decoded row per staged branch
/// row, the same order as the write set's own copy.
#[derive(Debug, Default)]
pub struct BranchStagedWrites {
    tables: HashMap<GenerationKey, RowSet>,
    /// Every (level, table) this transaction ever staged a write for.
    touched: HashSet<GenerationKey>,
    /// Set by ROLLBACK TO SAVEPOINT: `tables` was cleared, and a table is
    /// rebuilt from the write set on its next use (`fresh` lists the rebuilt).
    stale: bool,
    fresh: HashSet<GenerationKey>,
}

impl BranchStagedWrites {
    /// ROLLBACK TO SAVEPOINT rewound the write set.
    pub(crate) fn rewound(&mut self) {
        self.tables.clear();
        self.fresh.clear();
        self.stale = true;
    }

    fn is_current(&self, key: &GenerationKey) -> bool {
        !self.stale || self.fresh.contains(key)
    }

    /// The transaction's commit batch is applied: bump the write generation of
    /// every (level, table) it wrote — BEFORE its claims are released — and
    /// apply its rows to the cached committed overlays that were current.
    pub(crate) fn publish(self) {
        for key in &self.touched {
            let (engine, level, table) = key;
            let generation = bump_write_generation(*engine, *level, table);
            if self.stale {
                continue;
            }
            if let Some(set) = self.tables.get(key) {
                fast_forward(
                    *engine,
                    *level,
                    table,
                    generation,
                    set.rows.iter().map(|(row_id, image)| (*row_id, image.as_ref())),
                );
            }
        }
    }

    /// The commit failed — possibly after its batch reached RocksDB (a failed
    /// fsync): bump the generations so every cached overlay of the tables is
    /// rebuilt from storage, but apply nothing to them.
    pub(crate) fn invalidate(mut self) {
        self.stale = true;
        self.publish();
    }
}

// ---------------------------------------------------------------------------
// The statement memo: main's committed rows while main's index is unreliable
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MainMemoKey {
    engine: u64,
    table: String,
    /// Sorted column positions.
    positions: Vec<usize>,
}

type MainMemo = HashMap<MainMemoKey, HashMap<Vec<u8>, Vec<Tuple>>>;

thread_local! {
    /// sprinter 6bc4a4040eb7 (phase B2): main's committed rows by key, built
    /// by the statement running on this thread from ONE scan when an open
    /// main transaction made main's index unreliable for the table (see
    /// [`BranchView::rows_with_key`]); `None` outside a branch statement.
    /// Statement execution stays on its thread (sprinter ccebc95977f1 runs it
    /// under `block_in_place`), which is what makes a thread-local the right
    /// home.
    static STATEMENT_MAIN_ROWS: RefCell<Option<MainMemo>> = const { RefCell::new(None) };
}

/// sprinter 6bc4a4040eb7 (phase B2): the statement-scoped memo of ONE
/// statement on a branch — installed by the statement's branch scope (lib.rs
/// `BranchStatementScope`), dropped with it. A nested statement scope (`ON
/// BRANCH` override) gets a fresh memo and restores the outer one.
///
/// Not `Send`: it must never be held across an `.await`.
pub struct OverlayCacheScope {
    // Outer `None` = nothing to restore; inner `None` = restore "no memo".
    #[allow(clippy::option_option)]
    previous: Option<Option<MainMemo>>,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl OverlayCacheScope {
    /// Install an empty memo for the statement starting on this thread.
    pub(crate) fn enter() -> Self {
        let previous = STATEMENT_MAIN_ROWS.with(|cell| cell.borrow_mut().replace(HashMap::new()));
        Self {
            previous: Some(previous),
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for OverlayCacheScope {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            STATEMENT_MAIN_ROWS.with(|cell| *cell.borrow_mut() = previous);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn encode_positions(tuple: &Tuple, positions: &[usize]) -> Option<Vec<u8>> {
    let mut values: Vec<&Value> = Vec::with_capacity(positions.len());
    for position in positions {
        match tuple.values.get(*position) {
            None | Some(Value::Null) => return None,
            Some(value) => values.push(value),
        }
    }
    Some(ArtIndexManager::encode_key_from_values(values))
}

/// The schema positions of `columns` (case-insensitive), or `None` when one
/// is not a column of the table.
fn column_positions(schema: &Schema, columns: &[String]) -> Option<Vec<usize>> {
    columns
        .iter()
        .map(|name| schema.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name)))
        .collect()
}

fn row_matches(tuple: &Tuple, positions: &[usize], key: &[Value]) -> bool {
    positions
        .iter()
        .zip(key.iter())
        .all(|(position, value)| tuple.values.get(*position) == Some(value))
}

/// `values` (parallel to `from`) reordered into `to`'s column order.
fn reorder_values(from: &[String], values: &[Value], to: &[String]) -> Option<Vec<Value>> {
    to.iter()
        .map(|want| {
            from.iter()
                .position(|have| have.eq_ignore_ascii_case(want))
                .and_then(|pos| values.get(pos).cloned())
        })
        .collect()
}

/// The values of `rule`'s columns in a full row (schema order), in the rule's
/// column order; `None` when one is NULL (NULLs never conflict) or missing.
fn rule_key(schema: &Schema, rule: &EnforcedUniqueIndex, row: &[Value]) -> Option<Vec<Value>> {
    let mut key = Vec::with_capacity(rule.columns.len());
    for column in &rule.columns {
        let position = schema
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(column))?;
        match row.get(position) {
            None | Some(Value::Null) => return None,
            Some(value) => key.push(value.clone()),
        }
    }
    Some(key)
}

fn serialize_tuple(tuple: &Tuple) -> Result<Vec<u8>> {
    bincode::serialize(tuple).map_err(|e| Error::storage(format!("Failed to serialize tuple: {e}")))
}

/// A staged (or cached) row, NULL-padded to the table's current width like
/// `merge_with_write_set_to_width` pads staged rows.
fn padded(tuple: &Tuple, width: usize) -> Tuple {
    let mut tuple = tuple.clone();
    if tuple.values.len() < width {
        tuple.values.resize(width, Value::Null);
    }
    tuple
}

/// Where main's candidates for a key come from.
enum MainCandidates {
    /// Row ids from main's index, to be verified against main's rows.
    Ids(Vec<u64>),
    /// Main's committed rows with the key (already verified).
    Rows(Vec<Tuple>),
}

// ---------------------------------------------------------------------------
// Arbitration (INSERT and ON CONFLICT)
// ---------------------------------------------------------------------------

/// What holds one rule's key on the branch.
#[derive(Debug)]
pub enum KeyState {
    /// A visible row holds it.
    Visible(Tuple),
    /// No visible row holds it, but another open transaction on the branch
    /// has claimed it (an uncommitted write of the same key).
    Claimed,
}

/// sprinter 6bc4a4040eb7 (phase B2): every PRIMARY KEY / UNIQUE rule of a
/// proposed row checked on the branch — visibility and claims together, so
/// `ON CONFLICT DO NOTHING` treats another transaction's uncommitted key as a
/// conflict (and skips) instead of raising 23505 from the insert that follows.
/// The claims taken while checking are held only if the caller goes on to
/// insert ([`Self::keep`]); dropping the arbitration releases them.
#[derive(Debug)]
pub struct BranchArbitration {
    rules: Vec<(EnforcedUniqueIndex, Option<KeyState>)>,
    claims: Vec<BranchKeyClaim>,
    branch: String,
}

impl BranchArbitration {
    /// The first conflict (PRIMARY KEY first) as main's 23505 message.
    pub fn conflict_message(&self) -> Option<String> {
        self.rules.iter().find_map(|(rule, state)| match state {
            Some(KeyState::Visible(_)) => Some(duplicate_key_message(rule)),
            Some(KeyState::Claimed) => Some(claimed_key_message(rule, &self.branch)),
            None => None,
        })
    }

    /// The conflict on the `ON CONFLICT (<cols>)` arbiter — on any rule when
    /// `target` is `None` (a targetless clause, MySQL's `ON DUPLICATE KEY`).
    pub fn conflict_on(&self, target: Option<&[String]>) -> Option<&KeyState> {
        self.rules
            .iter()
            .filter(|(rule, _)| target.map_or(true, |target| ArtIndexManager::same_column_set(&rule.columns, target)))
            .find_map(|(_, state)| state.as_ref())
    }

    /// The branch the arbitration ran on.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The row is going to be inserted: hand the claims taken while checking
    /// to `txn`, so no other writer can take the keys before the insert.
    pub fn keep(self, txn: Option<&Transaction>) {
        if let Some(txn) = txn {
            for claim in self.claims {
                txn.hold_branch_claim(claim);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

/// sprinter 6bc4a4040eb7 (phase B2): one branch's tables — see the module
/// docs. Built from the session's binding by [`StorageEngine::branch_view`];
/// cheap (an id, a name and the chain).
#[derive(Clone)]
pub struct BranchView<'a> {
    engine: &'a StorageEngine,
    branch_id: u64,
    name: String,
    /// Oldest ancestor first, ENDING WITH `branch_id`. Main is not a level.
    chain: Vec<u64>,
    /// Direct (no-transaction) writes skip the autocommit durability barrier;
    /// the caller runs it once after its batch ([`Self::sync`]).
    defer_durability: bool,
}

impl std::fmt::Debug for BranchView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchView")
            .field("branch_id", &self.branch_id)
            .field("name", &self.name)
            .field("chain", &self.chain)
            .finish_non_exhaustive()
    }
}

impl StorageEngine {
    /// sprinter 6bc4a4040eb7 (phase B2): the branch the running statement is
    /// bound to, as a [`BranchView`] — `None` on main. A binding whose branch
    /// was dropped or merged is refused (3D000) rather than read as main.
    ///
    /// PERF: on main this is the lock-free `is_branch_active` gate — one
    /// thread-local read — so the per-row funnels that ask it (point reads,
    /// FK probes, the batch-insert funnel) pay nothing measurable on main.
    #[inline]
    pub fn branch_view(&self) -> Result<Option<BranchView<'_>>> {
        if !self.is_branch_active() {
            return Ok(None);
        }
        Ok(self
            .check_active_branch()?
            .map(|binding| BranchView::from_binding(self, &binding)))
    }

    /// sprinter 6bc4a4040eb7 (phase B2): refuse (0A000) a write path that can
    /// only write MAIN's keyspace — `path` names it — when the running
    /// statement is on a branch. Every SQL write path routes through the
    /// branch view on a branch; this is the backstop that keeps one that does
    /// not (a fast path a future change forgets to gate, lock-free ingestion,
    /// direct bulk load, a main-only purge) from writing main: it fails
    /// closed, before anything is written. One thread-local check on main.
    pub(crate) fn refuse_main_only_write_on_branch(&self, path: &str) -> Result<()> {
        if !self.is_branch_active() {
            return Ok(());
        }
        let name = self
            .active_branch()
            .map_or_else(|| "?".to_string(), |binding| binding.name.clone());
        Err(Error::branch_write_unsupported(path, &name))
    }

    /// sprinter 6bc4a4040eb7 (phase B2 fix pass): refuse (0A000) `statement`,
    /// which cannot be scoped to a branch yet, when the running statement is
    /// on one — before anything is written.
    pub(crate) fn refuse_statement_on_branch(&self, statement: &str) -> Result<()> {
        if !self.is_branch_active() {
            return Ok(());
        }
        let name = self
            .active_branch()
            .map_or_else(|| "?".to_string(), |binding| binding.name.clone());
        Err(Error::branch_statement_unsupported(statement, &name))
    }
}

impl<'a> BranchView<'a> {
    /// The view of the branch `binding` names.
    pub(crate) fn from_binding(engine: &'a StorageEngine, binding: &crate::session::BranchBinding) -> Self {
        let mut chain = binding.chain.clone();
        if chain.last() != Some(&binding.branch_id) {
            chain.push(binding.branch_id);
        }
        Self {
            engine,
            branch_id: binding.branch_id,
            name: binding.name.clone(),
            chain,
            defer_durability: false,
        }
    }

    /// The view of the level ending `chain` (oldest first) — for engine work
    /// that reads a branch other than the session's (phase B4's freeze hook
    /// reads a parent level's pre-image through this).
    pub fn for_chain(engine: &'a StorageEngine, name: impl Into<String>, chain: Vec<u64>) -> Result<Self> {
        let branch_id = *chain
            .last()
            .ok_or_else(|| Error::internal("a branch view needs a non-empty chain"))?;
        Ok(Self {
            engine,
            branch_id,
            name: name.into(),
            chain,
            defer_durability: false,
        })
    }

    /// This view with the durability barrier of its direct writes deferred
    /// to one [`Self::sync`] by the caller (a bulk load).
    #[must_use]
    pub fn with_deferred_durability(mut self) -> Self {
        self.defer_durability = true;
        self
    }

    /// The autocommit durability barrier for the direct writes made so far
    /// (a no-op unless synchronous commit is required).
    pub fn sync(&self) -> Result<()> {
        self.engine.durable_autocommit_barrier()
    }

    /// The branch's id — the level its own writes go to.
    pub fn branch_id(&self) -> u64 {
        self.branch_id
    }

    /// The branch's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The overlay chain, oldest first, ending with [`Self::branch_id`].
    pub fn chain(&self) -> &[u64] {
        &self.chain
    }

    fn ancestors(&self) -> &[u64] {
        self.chain.split_last().map_or(&[][..], |(_, ancestors)| ancestors)
    }

    fn engine_id(&self) -> u64 {
        self.engine.instance_id()
    }

    // ---- low-level reads ----

    fn for_each_with_prefix(&self, prefix: &[u8], mut each: impl FnMut(&[u8], &[u8]) -> Result<()>) -> Result<()> {
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self
            .engine
            .db
            .iterator_opt(IteratorMode::From(prefix, Direction::Forward), read_opts);
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {e}")))?;
            if !key.starts_with(prefix) {
                break;
            }
            each(&key, &value)?;
        }
        Ok(())
    }

    fn decode_committed(&self, key: &[u8], raw: &[u8], row_id: u64, level: u64, width: usize) -> Result<Tuple> {
        let value = self.engine.decrypt_value(key, raw)?;
        let mut tuple: Tuple =
            bincode::deserialize(&value).map_err(|e| Error::storage(format!("Failed to deserialize tuple: {e}")))?;
        tuple.row_id = Some(row_id);
        tuple.branch_id = Some(level);
        // W3.5: a row forked before an ADD/DROP COLUMN keeps its arity; shape
        // it to the current width exactly as the pre-B2 overlay scan did.
        if self.engine.null_pad_snapshot_schema() {
            StorageEngine::shape_base_tuple_to_width(&mut tuple, width);
        }
        Ok(tuple)
    }

    /// A row value this view's transaction STAGED: plaintext (the commit
    /// batch seals it), NULL-padded like `merge_with_write_set_to_width`.
    fn decode_staged(&self, value: &[u8], row_id: u64, width: usize) -> Result<Tuple> {
        let mut tuple: Tuple =
            bincode::deserialize(value).map_err(|e| Error::storage(format!("Failed to deserialize tuple: {e}")))?;
        tuple.row_id = Some(row_id);
        tuple.branch_id = Some(self.branch_id);
        if tuple.values.len() < width {
            tuple.values.resize(width, Value::Null);
        }
        Ok(tuple)
    }

    /// Main's current row `row_id` of `table`, decoded exactly as main's scan
    /// decodes it (storage references resolved).
    fn main_row(&self, table: &str, row_id: u64, schema: &Schema) -> Result<Option<Tuple>> {
        let key = StorageEngine::build_data_key(table, row_id);
        let raw = self
            .engine
            .db
            .get(&key)
            .map_err(|e| Error::storage(format!("Get failed: {e}")))?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let stored = self.engine.decode_stored_ref(&key, &raw)?;
        let mut tuple: Tuple =
            bincode::deserialize(&stored).map_err(|e| Error::storage(format!("Failed to deserialize tuple: {e}")))?;
        if self.engine.null_pad_snapshot_schema() {
            StorageEngine::shape_base_tuple_to_width(&mut tuple, schema.columns.len());
        }
        tuple.row_id = Some(row_id);
        tuple.branch_id = None;
        self.engine
            .resolve_column_storage_refs(table, schema, row_id, &mut tuple)?;
        Ok(Some(tuple))
    }

    fn key_present(&self, key: &[u8]) -> Result<bool> {
        Ok(self
            .engine
            .db
            .get_pinned(key)
            .map_err(|e| Error::storage(format!("Get failed: {e}")))?
            .is_some())
    }

    /// Level `level`'s committed images and tombstones for `table`.
    fn committed_level(&self, level: u64, table: &str, width: usize) -> Result<(HashMap<u64, Tuple>, HashSet<u64>)> {
        let mut rows: HashMap<u64, Tuple> = HashMap::new();
        let prefix = row_prefix(level, table);
        let prefix_len = prefix.len();
        self.for_each_with_prefix(prefix.as_bytes(), |key, raw| {
            if let Some(row_id) = StorageEngine::parse_row_id_after_prefix(key, prefix_len) {
                rows.insert(row_id, self.decode_committed(key, raw, row_id, level, width)?);
            }
            Ok(())
        })?;
        let mut tombstones: HashSet<u64> = HashSet::new();
        let prefix = tombstone_prefix(level, table);
        let prefix_len = prefix.len();
        self.for_each_with_prefix(prefix.as_bytes(), |key, _| {
            if let Some(row_id) = StorageEngine::parse_row_id_after_prefix(key, prefix_len) {
                tombstones.insert(row_id);
            }
            Ok(())
        })?;
        Ok((rows, tombstones))
    }

    /// Resolve one level: an image unless the same level also tombstones it.
    fn resolve_level(rows: HashMap<u64, Tuple>, tombstones: HashSet<u64>) -> HashMap<u64, LevelEntry> {
        let mut out: HashMap<u64, LevelEntry> = HashMap::with_capacity(rows.len() + tombstones.len());
        for (row_id, tuple) in rows {
            if !tombstones.contains(&row_id) {
                out.insert(row_id, LevelEntry::Row(tuple));
            }
        }
        for row_id in tombstones {
            out.insert(row_id, LevelEntry::Tombstone);
        }
        out
    }

    /// Every row some COMMITTED level of the chain decides, nearest level
    /// winning: `Some` = visible image, `None` = hidden. Read from storage.
    fn load_committed_rows(&self, table: &str, width: usize) -> Result<HashMap<u64, Option<Tuple>>> {
        let mut out: HashMap<u64, Option<Tuple>> = HashMap::new();
        for &level in &self.chain {
            let (rows, tombstones) = self.committed_level(level, table, width)?;
            for (row_id, entry) in Self::resolve_level(rows, tombstones) {
                out.insert(row_id, entry.into_visible());
            }
        }
        Ok(out)
    }

    /// The branch's committed overlay of `table` — the cached copy when it is
    /// still current (every chain level's write generation and the schema
    /// generation unchanged), a fresh load otherwise.
    fn committed_overlay(&self, table: &str, width: usize) -> Result<Arc<parking_lot::Mutex<CommittedOverlay>>> {
        let engine = self.engine_id();
        let key = CommittedKey {
            engine,
            branch: self.branch_id,
            table: table.to_string(),
        };
        // Read BEFORE loading: a write that becomes visible while the overlay
        // loads leaves the copy behind its recorded generations, so its next
        // use reloads it (a reload that was not needed costs time, never
        // correctness).
        let generations: Vec<u64> = self
            .chain
            .iter()
            .map(|level| current_generation(engine, *level, table))
            .collect();
        let schema_generation = self.engine.schema_generation();
        // A physical standby's rows arrive through the WAL applier, below
        // every write-generation bump, so nothing could tell a cached copy it
        // is stale: never cache there (reads only; a standby takes no writes).
        if self.engine.is_physical_standby() {
            let rows = self.load_committed_rows(table, width)?;
            return Ok(Arc::new(parking_lot::Mutex::new(CommittedOverlay {
                width,
                schema_generation,
                generations,
                set: RowSet::from_rows(rows),
            })));
        }
        let cached = COMMITTED.lock().lookup(&key);
        if let Some(cached) = cached {
            let current = {
                let overlay = cached.lock();
                overlay.width == width
                    && overlay.schema_generation == schema_generation
                    && overlay.generations == generations
            };
            if current {
                return Ok(cached);
            }
        }
        let rows = self.load_committed_rows(table, width)?;
        let count = rows.len();
        let overlay = Arc::new(parking_lot::Mutex::new(CommittedOverlay {
            width,
            schema_generation,
            generations,
            set: RowSet::from_rows(rows),
        }));
        COMMITTED.lock().admit(key, Arc::clone(&overlay), count);
        Ok(overlay)
    }

    /// The transaction's staged rows of `table` at this branch's level,
    /// rebuilt from the write set (they are always staged as an image or as a
    /// tombstone, never half of a pair).
    fn staged_from_write_set(&self, txn: &Transaction, table: &str) -> Result<RowSet> {
        let mut rows: HashMap<u64, Option<Tuple>> = HashMap::new();
        let prefix = row_prefix(self.branch_id, table);
        for (key, staged) in txn.staged_with_prefix(prefix.as_bytes()) {
            if let (Some(row_id), Some(value)) = (StorageEngine::parse_row_id_after_prefix(&key, prefix.len()), staged)
            {
                rows.insert(row_id, Some(self.decode_staged(&value, row_id, 0)?));
            }
        }
        let prefix = tombstone_prefix(self.branch_id, table);
        for (key, staged) in txn.staged_with_prefix(prefix.as_bytes()) {
            if let (Some(row_id), Some(_)) = (StorageEngine::parse_row_id_after_prefix(&key, prefix.len()), staged) {
                rows.insert(row_id, None);
            }
        }
        Ok(RowSet::from_rows(rows))
    }

    /// Run `with` on `txn`'s staged rows of `table` (at this branch's level).
    fn with_staged<R>(&self, txn: &Transaction, table: &str, with: impl FnOnce(&mut RowSet) -> R) -> Result<R> {
        let key: GenerationKey = (self.engine_id(), self.branch_id, table.to_string());
        let current = txn.branch_staged().is_current(&key);
        if !current {
            // ROLLBACK TO SAVEPOINT rewound the write set: rebuild this
            // table's staged rows from it, once.
            let rebuilt = self.staged_from_write_set(txn, table)?;
            let mut staged = txn.branch_staged();
            staged.tables.insert(key.clone(), rebuilt);
            staged.fresh.insert(key.clone());
        }
        let mut staged = txn.branch_staged();
        let set = staged.tables.entry(key).or_default();
        Ok(with(set))
    }

    /// Committed entry of `level` for one row (tombstone first: it wins
    /// within a level).
    fn committed_entry(&self, level: u64, table: &str, row_id: u64, width: usize) -> Result<Option<LevelEntry>> {
        if self.key_present(&branch_tombstone_key(level, table, row_id))? {
            return Ok(Some(LevelEntry::Tombstone));
        }
        let key = branch_row_key(level, table, row_id);
        let raw = self
            .engine
            .db
            .get(&key)
            .map_err(|e| Error::storage(format!("Get failed: {e}")))?;
        match raw {
            Some(raw) => Ok(Some(LevelEntry::Row(
                self.decode_committed(&key, &raw, row_id, level, width)?,
            ))),
            None => Ok(None),
        }
    }

    /// The branch's OWN entry for row `row_id` of `table`, including what
    /// `txn` staged — `None` when this level decides nothing for the row (it
    /// is then its parent's, or main's). Phase B4's freeze hook asks this
    /// ("does the child already have its own version?").
    pub fn own_entry(
        &self,
        txn: Option<&Transaction>,
        table: &str,
        row_id: u64,
        width: usize,
    ) -> Result<Option<LevelEntry>> {
        let tombstone_key = branch_tombstone_key(self.branch_id, table, row_id);
        let row_key = branch_row_key(self.branch_id, table, row_id);
        let staged_tombstone = txn.and_then(|txn| txn.staged(&tombstone_key));
        let staged_row = txn.and_then(|txn| txn.staged(&row_key));
        let tombstoned = match staged_tombstone {
            Some(staged) => staged.is_some(),
            None => self.key_present(&tombstone_key)?,
        };
        if tombstoned {
            return Ok(Some(LevelEntry::Tombstone));
        }
        match staged_row {
            Some(Some(value)) => Ok(Some(LevelEntry::Row(self.decode_staged(&value, row_id, width)?))),
            Some(None) => Ok(None),
            None => {
                let raw = self
                    .engine
                    .db
                    .get(&row_key)
                    .map_err(|e| Error::storage(format!("Get failed: {e}")))?;
                match raw {
                    Some(raw) => Ok(Some(LevelEntry::Row(self.decode_committed(
                        &row_key,
                        &raw,
                        row_id,
                        self.branch_id,
                        width,
                    )?))),
                    None => Ok(None),
                }
            }
        }
    }

    // ---- the read API ----

    /// Row `row_id` of `table` as the branch sees it (nearest level wins,
    /// then main), with `txn`'s staged writes.
    pub fn get(&self, txn: Option<&Transaction>, table: &str, row_id: u64, schema: &Schema) -> Result<Option<Tuple>> {
        let width = schema.columns.len();
        if let Some(entry) = self.own_entry(txn, table, row_id, width)? {
            return Ok(entry.into_visible());
        }
        for &level in self.ancestors().iter().rev() {
            if let Some(entry) = self.committed_entry(level, table, row_id, width)? {
                return Ok(entry.into_visible());
            }
        }
        self.main_row(table, row_id, schema)
    }

    /// Every row of `table` the branch sees, ordered by row id, with `txn`'s
    /// staged writes merged in.
    pub fn scan(&self, txn: Option<&Transaction>, table: &str, schema: &Schema) -> Result<Vec<Tuple>> {
        let width = schema.columns.len();
        let mut rows: HashMap<u64, Tuple> = HashMap::new();
        for mut tuple in self.engine.scan_table_with_schema(table, schema)? {
            if let Some(row_id) = tuple.row_id {
                tuple.branch_id = None;
                rows.insert(row_id, tuple);
            }
        }
        let committed = self.committed_overlay(table, width)?;
        {
            let overlay = committed.lock();
            for (row_id, state) in &overlay.set.rows {
                match state {
                    Some(tuple) => {
                        rows.insert(*row_id, padded(tuple, width));
                    }
                    None => {
                        rows.remove(row_id);
                    }
                }
            }
        }
        if let Some(txn) = txn {
            let staged: Vec<(u64, Option<Tuple>)> = self.with_staged(txn, table, |set| {
                set.rows
                    .iter()
                    .map(|(row_id, state)| (*row_id, state.as_ref().map(|tuple| padded(tuple, width))))
                    .collect()
            })?;
            for (row_id, state) in staged {
                match state {
                    Some(tuple) => {
                        rows.insert(row_id, tuple);
                    }
                    None => {
                        rows.remove(&row_id);
                    }
                }
            }
        }
        let mut out: Vec<Tuple> = rows.into_values().collect();
        out.sort_by_key(|tuple| tuple.row_id.unwrap_or(0));
        Ok(out)
    }

    /// Main's committed rows of `table` whose `positions` hold `key` — from
    /// ONE scan per statement (memoised in the statement scope), or a scan
    /// with no statement scope installed.
    fn main_rows_with_key(
        &self,
        table: &str,
        schema: &Schema,
        positions: &[usize],
        sorted_positions: &[usize],
        encoded: &[u8],
        key: &[Value],
    ) -> Result<Vec<Tuple>> {
        let memo_key = MainMemoKey {
            engine: self.engine_id(),
            table: table.to_string(),
            positions: sorted_positions.to_vec(),
        };
        // Outer `None`: no statement scope; inner `None`: not built yet.
        #[allow(clippy::option_option)]
        let memoised: Option<Option<Vec<Tuple>>> = STATEMENT_MAIN_ROWS.with(|cell| {
            cell.borrow().as_ref().map(|memo| {
                memo.get(&memo_key)
                    .map(|by_key| by_key.get(encoded).cloned().unwrap_or_default())
            })
        });
        match memoised {
            Some(Some(rows)) => Ok(rows),
            Some(None) => {
                let mut by_key: HashMap<Vec<u8>, Vec<Tuple>> = HashMap::new();
                for tuple in self.engine.scan_table_with_schema(table, schema)? {
                    if let Some(encoded_key) = encode_positions(&tuple, sorted_positions) {
                        by_key.entry(encoded_key).or_default().push(tuple);
                    }
                }
                let rows = by_key.get(encoded).cloned().unwrap_or_default();
                STATEMENT_MAIN_ROWS.with(|cell| {
                    if let Some(memo) = cell.borrow_mut().as_mut() {
                        memo.insert(memo_key, by_key);
                    }
                });
                Ok(rows)
            }
            None => Ok(self
                .engine
                .scan_table_with_schema(table, schema)?
                .into_iter()
                .filter(|tuple| row_matches(tuple, positions, key))
                .collect()),
        }
    }

    /// Main's candidates for `key` on `columns`: main's index when it can be
    /// trusted to hold every committed row, main's committed rows otherwise.
    ///
    /// sprinter 6bc4a4040eb7 (B2 fix pass): main's ART is maintained EAGERLY —
    /// an open main transaction's DELETE, key-moving UPDATE or TRUNCATE takes
    /// the entry out before the row leaves main, so a probe miss would read as
    /// "main has no such row" and let a branch insert a duplicate of a row
    /// that is still committed (or fail a FK whose parent still exists). The
    /// uncommitted-write census says when that can be the case; it is asked
    /// again after the probe because it is raised just after the mutation.
    #[allow(clippy::too_many_arguments)]
    fn main_candidates(
        &self,
        table: &str,
        schema: &Schema,
        columns: &[String],
        key: &[Value],
        positions: &[usize],
        sorted_positions: &[usize],
        encoded: &[u8],
    ) -> Result<MainCandidates> {
        let art = self.engine.art_indexes();
        if !self.engine.has_uncommitted_index_removals_for_table(table) {
            if let Some((index_name, index_columns)) = art.index_covering_columns(table, columns) {
                let ordered = reorder_values(columns, key, &index_columns)
                    .ok_or_else(|| Error::internal("index columns do not match the probed columns"))?;
                let ids = art.index_get_all(&index_name, &ArtIndexManager::encode_key(&ordered));
                if !self.engine.has_uncommitted_index_removals_for_table(table) {
                    return Ok(MainCandidates::Ids(ids));
                }
            }
        }
        Ok(MainCandidates::Rows(self.main_rows_with_key(
            table,
            schema,
            positions,
            sorted_positions,
            encoded,
            key,
        )?))
    }

    /// The rows of `table` the branch sees whose `columns` equal `key`
    /// (parallel slices; a NULL in `key` matches nothing), with `txn`'s
    /// staged writes.
    ///
    /// Candidates: main's (see [`Self::main_candidates`]), each kept only when
    /// no level of the chain — nor the transaction — decides the row and main
    /// still holds it with that key; plus every visible row of the cached
    /// committed overlay and of the transaction's staged rows with the key.
    /// Cost: hash lookups and one index probe (see the module docs).
    pub fn rows_with_key(
        &self,
        txn: Option<&Transaction>,
        table: &str,
        schema: &Schema,
        columns: &[String],
        key: &[Value],
    ) -> Result<Vec<Tuple>> {
        let Some(positions) = column_positions(schema, columns) else {
            return Ok(Vec::new());
        };
        if positions.len() != key.len() || key.iter().any(|value| matches!(value, Value::Null)) {
            return Ok(Vec::new());
        }
        let width = schema.columns.len();

        // Canonical (sorted-position) form for the key maps.
        let mut canonical: Vec<(usize, &Value)> = positions.iter().copied().zip(key.iter()).collect();
        canonical.sort_by_key(|(position, _)| *position);
        let sorted_positions: Vec<usize> = canonical.iter().map(|(position, _)| *position).collect();
        let encoded = ArtIndexManager::encode_key_from_values(canonical.iter().map(|(_, value)| *value));

        let main = self.main_candidates(table, schema, columns, key, &positions, &sorted_positions, &encoded)?;
        let committed = self.committed_overlay(table, width)?;

        // Lock order: the transaction's staged rows, then the committed copy.
        let collect = |staged: Option<&mut RowSet>| -> (Vec<Tuple>, Vec<u64>, Vec<Tuple>) {
            let mut overlay = committed.lock();
            let mut out: Vec<Tuple> = Vec::new();
            let staged: Option<&RowSet> = match staged {
                Some(staged) => {
                    out.extend(
                        staged
                            .matching(&sorted_positions, &encoded)
                            .into_iter()
                            .map(|tuple| padded(tuple, width)),
                    );
                    Some(&*staged)
                }
                None => None,
            };
            let staged_decides = |row_id: u64| staged.is_some_and(|staged| staged.decides(row_id));
            let committed_matches: Vec<Tuple> = overlay
                .set
                .matching(&sorted_positions, &encoded)
                .into_iter()
                .filter(|tuple| tuple.row_id.is_some_and(|row_id| !staged_decides(row_id)))
                .map(|tuple| padded(tuple, width))
                .collect();
            out.extend(committed_matches);
            let decided = |row_id: u64| staged_decides(row_id) || overlay.set.decides(row_id);
            match main {
                MainCandidates::Ids(ids) => (out, ids.into_iter().filter(|id| !decided(*id)).collect(), Vec::new()),
                MainCandidates::Rows(rows) => {
                    let rows = rows
                        .into_iter()
                        .filter(|tuple| tuple.row_id.is_some_and(|row_id| !decided(row_id)))
                        .collect();
                    (out, Vec::new(), rows)
                }
            }
        };
        let (mut out, undecided_ids, undecided_rows) = match txn {
            Some(txn) => self.with_staged(txn, table, |staged| collect(Some(staged)))?,
            None => collect(None),
        };

        let mut seen: HashSet<u64> = out.iter().filter_map(|tuple| tuple.row_id).collect();
        for row_id in undecided_ids {
            if !seen.insert(row_id) {
                continue;
            }
            // Main's index can hold a key a still-open MAIN transaction claimed
            // but has not committed, or a stale entry: count the row only when
            // main holds it, with this key.
            if let Some(tuple) = self.main_row(table, row_id, schema)? {
                if row_matches(&tuple, &positions, key) {
                    out.push(tuple);
                }
            }
        }
        for tuple in undecided_rows {
            if tuple.row_id.is_some_and(|row_id| seen.insert(row_id)) {
                out.push(tuple);
            }
        }
        out.sort_by_key(|tuple| tuple.row_id.unwrap_or(0));
        Ok(out)
    }

    /// The first PRIMARY KEY / UNIQUE rule of `table` (PRIMARY KEY first) that
    /// the full row `values` (schema order) would violate on this branch — a
    /// visible row other than `exclude` holds the same non-NULL key. With
    /// `arbiter` (an `ON CONFLICT (<cols>)` target) only the rule over exactly
    /// that column set is considered.
    pub fn unique_conflict(
        &self,
        txn: Option<&Transaction>,
        table: &str,
        schema: &Schema,
        values: &[Value],
        exclude: Option<u64>,
        arbiter: Option<&[String]>,
    ) -> Result<Option<BranchUniqueConflict>> {
        for rule in self.engine.art_indexes().enforced_unique_indexes(table) {
            if let Some(arbiter) = arbiter {
                if !ArtIndexManager::same_column_set(&rule.columns, arbiter) {
                    continue;
                }
            }
            let Some(key) = rule_key(schema, &rule, values) else {
                continue;
            };
            let holders = self.rows_with_key(txn, table, schema, &rule.columns, &key)?;
            if let Some(row) = holders
                .into_iter()
                .find(|row| exclude.is_none() || row.row_id != exclude)
            {
                return Ok(Some(BranchUniqueConflict { rule, row }));
            }
        }
        Ok(None)
    }

    /// Check every PRIMARY KEY / UNIQUE rule of the proposed full row `values`
    /// on this branch — claiming each non-NULL key for `txn` first, then
    /// asking who holds it. A key a visible row holds is [`KeyState::Visible`];
    /// one only another open transaction has claimed is [`KeyState::Claimed`].
    pub fn arbitrate(
        &self,
        txn: Option<&Transaction>,
        table: &str,
        schema: &Schema,
        values: &[Value],
    ) -> Result<BranchArbitration> {
        let owner = txn.map_or_else(
            || DIRECT_OWNER.fetch_sub(1, Ordering::Relaxed),
            Transaction::transaction_id,
        );
        let mut rules = Vec::new();
        let mut claims = Vec::new();
        for rule in self.engine.art_indexes().enforced_unique_indexes(table) {
            let Some(key) = rule_key(schema, &rule, values) else {
                rules.push((rule, None));
                continue;
            };
            let claimed_by_another = match self.claim(owner, table, &rule, &key) {
                Ok(Some(claim)) => {
                    claims.push(claim);
                    false
                }
                Ok(None) => false,
                Err(()) => true,
            };
            let holders = self.rows_with_key(txn, table, schema, &rule.columns, &key)?;
            let state = match holders.into_iter().next() {
                Some(row) => Some(KeyState::Visible(row)),
                None if claimed_by_another => Some(KeyState::Claimed),
                None => None,
            };
            rules.push((rule, state));
        }
        Ok(BranchArbitration {
            rules,
            claims,
            branch: self.name.clone(),
        })
    }

    /// Is the key `values` (parallel to `columns`) of the PRIMARY KEY /
    /// UNIQUE rule over that column set held on this branch by a row other
    /// than `exclude`? The UPDATE-shaped question
    /// (`ArtIndexManager::unique_key_taken_by_other_row` on main). The key is
    /// CLAIMED for `txn` first (see [`BranchKeyClaim`]); a claim held by
    /// another live writer answers "taken". No rule over the set answers "not
    /// taken", exactly as main's probe does.
    ///
    /// Needs the writing transaction: without one there is nothing to hold
    /// the claim until the write lands, so the probe could not protect it —
    /// refused (fail closed) rather than answered.
    pub fn key_taken_by_other_row(
        &self,
        txn: Option<&Transaction>,
        table: &str,
        schema: &Schema,
        columns: &[String],
        values: &[Value],
        exclude: Option<u64>,
    ) -> Result<bool> {
        let Some(txn) = txn else {
            return Err(Error::branch_write_unsupported(
                "a uniqueness check outside a transaction",
                &self.name,
            ));
        };
        if values.iter().any(|value| matches!(value, Value::Null)) {
            return Ok(false);
        }
        let Some(rule) = self
            .engine
            .art_indexes()
            .enforced_unique_indexes(table)
            .into_iter()
            .find(|rule| ArtIndexManager::same_column_set(&rule.columns, columns))
        else {
            return Ok(false);
        };
        let Some(key) = reorder_values(columns, values, &rule.columns) else {
            return Ok(false);
        };
        match self.claim(txn.transaction_id(), table, &rule, &key) {
            Ok(Some(claim)) => txn.hold_branch_claim(claim),
            Ok(None) => {}
            Err(()) => return Ok(true),
        }
        let holders = self.rows_with_key(Some(txn), table, schema, &rule.columns, &key)?;
        Ok(holders.iter().any(|row| exclude.is_none() || row.row_id != exclude))
    }

    /// Claim `key` of `rule` for `owner`. `Ok(None)` = `owner` already holds
    /// it; `Err(())` = another live writer does.
    fn claim(
        &self,
        owner: u64,
        table: &str,
        rule: &EnforcedUniqueIndex,
        key: &[Value],
    ) -> std::result::Result<Option<BranchKeyClaim>, ()> {
        let claim_key = ClaimKey {
            engine: self.engine_id(),
            branch: self.branch_id,
            table: table.to_string(),
            index: rule.index_name.clone(),
            key: ArtIndexManager::encode_key(key),
        };
        match CLAIMS.entry(claim_key.clone()) {
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                vacant.insert(owner);
                Ok(Some(BranchKeyClaim { key: claim_key, owner }))
            }
            dashmap::mapref::entry::Entry::Occupied(occupied) if *occupied.get() == owner => Ok(None),
            dashmap::mapref::entry::Entry::Occupied(_) => Err(()),
        }
    }

    /// Claim every non-NULL PRIMARY KEY / UNIQUE key of the full row `values`
    /// for `owner`. A key another live writer holds is a 23505.
    fn claim_row_keys(
        &self,
        owner: u64,
        table: &str,
        schema: &Schema,
        values: &[Value],
    ) -> Result<Vec<BranchKeyClaim>> {
        let mut claims = Vec::new();
        for rule in self.engine.art_indexes().enforced_unique_indexes(table) {
            let Some(key) = rule_key(schema, &rule, values) else {
                continue;
            };
            match self.claim(owner, table, &rule, &key) {
                Ok(Some(claim)) => claims.push(claim),
                Ok(None) => {}
                Err(()) => return Err(Error::constraint_violation(claimed_key_message(&rule, &self.name))),
            }
        }
        Ok(claims)
    }

    // ---- the write API ----

    /// A direct (no-transaction) batch is applied: bump the level's write
    /// generation, apply the rows to this view's cached overlay, and run the
    /// autocommit durability barrier unless the caller deferred it.
    fn after_direct_write<'t>(
        &self,
        table: &str,
        writes: impl IntoIterator<Item = (u64, Option<&'t Tuple>)>,
    ) -> Result<()> {
        let engine = self.engine_id();
        let generation = bump_write_generation(engine, self.branch_id, table);
        fast_forward(engine, self.branch_id, table, generation, writes);
        if !self.defer_durability {
            self.engine.durable_autocommit_barrier()?;
        }
        Ok(())
    }

    fn write(
        &self,
        txn: Option<&Transaction>,
        table: &str,
        row_id: u64,
        image: Option<&Tuple>,
        clear_tombstone: bool,
    ) -> Result<()> {
        let row_key = branch_row_key(self.branch_id, table, row_id);
        let tombstone_key = branch_tombstone_key(self.branch_id, table, row_id);
        match txn {
            Some(txn) => {
                // The staged rows must be current before this write is noted.
                self.with_staged(txn, table, |_| ())?;
                match image {
                    Some(tuple) => {
                        txn.put(row_key, serialize_tuple(tuple)?)?;
                        if clear_tombstone {
                            txn.delete(tombstone_key)?;
                        }
                    }
                    None => {
                        txn.delete(row_key)?;
                        txn.put(tombstone_key, Vec::new())?;
                    }
                }
                let key: GenerationKey = (self.engine_id(), self.branch_id, table.to_string());
                let mut staged = txn.branch_staged();
                staged.touched.insert(key.clone());
                staged.tables.entry(key).or_default().note(row_id, image);
                Ok(())
            }
            None => {
                let mut batch = WriteBatch::default();
                match image {
                    Some(tuple) => {
                        let value = serialize_tuple(tuple)?;
                        let sealed = crate::storage::tde::seal(self.engine.key_manager_ref(), &value)?;
                        batch.put(&row_key, sealed.as_ref());
                        if clear_tombstone {
                            batch.delete(&tombstone_key);
                        }
                    }
                    None => {
                        batch.delete(&row_key);
                        batch.put(&tombstone_key, b"");
                    }
                }
                self.engine.write_batch_through_gate(batch)?;
                self.after_direct_write(table, std::iter::once((row_id, image)))
            }
        }
    }

    /// Write `tuple` as the branch's own image of a row id the caller JUST
    /// allocated (`next_row_id_volatile`): no level can hold an entry for it,
    /// so there is no tombstone to clear.
    pub fn insert(&self, txn: Option<&Transaction>, table: &str, row_id: u64, tuple: &Tuple) -> Result<()> {
        self.write(txn, table, row_id, Some(tuple), false)
    }

    /// Write `tuple` as the branch's own image of row `row_id` and clear the
    /// branch's own tombstone for it.
    pub fn put(&self, txn: Option<&Transaction>, table: &str, row_id: u64, tuple: &Tuple) -> Result<()> {
        self.write(txn, table, row_id, Some(tuple), true)
    }

    /// Hide row `row_id` on the branch: clear its own image and write its own
    /// tombstone — always a tombstone, which is what hides an inherited row.
    pub fn delete(&self, txn: Option<&Transaction>, table: &str, row_id: u64) -> Result<()> {
        self.write(txn, table, row_id, None, true)
    }

    /// INSERT a NEW row `row_id` (just allocated) after enforcing every
    /// PRIMARY KEY / UNIQUE rule of `table` over the branch's view: the row's
    /// keys are CLAIMED first (held by `txn` until it ends, or for the
    /// duration of this call), then checked, then the row is written. A
    /// violation is 23505 with main's message, and nothing is written.
    pub fn insert_checked(
        &self,
        txn: Option<&Transaction>,
        table: &str,
        schema: &Schema,
        row_id: u64,
        tuple: &Tuple,
    ) -> Result<()> {
        let owner = txn.map_or_else(
            || DIRECT_OWNER.fetch_sub(1, Ordering::Relaxed),
            Transaction::transaction_id,
        );
        let claims = self.claim_row_keys(owner, table, schema, &tuple.values)?;
        if let Some(conflict) = self.unique_conflict(txn, table, schema, &tuple.values, None, None)? {
            return Err(conflict.into_error());
        }
        self.insert(txn, table, row_id, tuple)?;
        if let Some(txn) = txn {
            for claim in claims {
                txn.hold_branch_claim(claim);
            }
        }
        Ok(())
    }

    /// TRUNCATE `table` on this branch only: tombstone every row the branch
    /// sees and drop the branch's own images of the table. Main and every
    /// other branch are untouched; with `txn` it is staged (ROLLBACK restores
    /// the rows). Returns the number of rows the branch saw.
    pub fn truncate(&self, txn: Option<&Transaction>, table: &str, schema: &Schema) -> Result<u64> {
        let visible = self.scan(txn, table, schema)?;
        let width = schema.columns.len();
        let mut targets: Vec<u64> = visible.iter().filter_map(|tuple| tuple.row_id).collect();
        let mut seen: HashSet<u64> = targets.iter().copied().collect();
        // An own image hidden by an own tombstone (a pre-B2 delete left both)
        // is dropped too, so nothing of the table survives on this level.
        let (own_rows, _) = self.committed_level(self.branch_id, table, width)?;
        for row_id in own_rows.keys() {
            if seen.insert(*row_id) {
                targets.push(*row_id);
            }
        }
        match txn {
            Some(txn) => {
                for row_id in &targets {
                    self.write(Some(txn), table, *row_id, None, true)?;
                }
            }
            None => {
                let mut batch = WriteBatch::default();
                for row_id in &targets {
                    batch.delete(branch_row_key(self.branch_id, table, *row_id));
                    batch.put(branch_tombstone_key(self.branch_id, table, *row_id), b"");
                }
                self.engine.write_batch_through_gate(batch)?;
                self.after_direct_write(table, targets.iter().map(|row_id| (*row_id, None)))?;
            }
        }
        Ok(visible.len() as u64)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::Config;

    fn engine() -> StorageEngine {
        StorageEngine::open_in_memory(&Config::in_memory()).unwrap()
    }

    fn row(id: i32, v: &str) -> Tuple {
        Tuple::new(vec![Value::Int4(id), Value::String(v.to_string())])
    }

    fn schema() -> Schema {
        Schema::new(vec![
            crate::Column::new("id", crate::DataType::Int4),
            crate::Column::new("v", crate::DataType::Text),
        ])
    }

    fn put_main(engine: &StorageEngine, row_id: u64, tuple: &Tuple) {
        engine
            .put(
                &StorageEngine::build_data_key("t", row_id),
                &bincode::serialize(tuple).unwrap(),
            )
            .unwrap();
    }

    fn ids(rows: &[Tuple]) -> Vec<u64> {
        rows.iter().map(|t| t.row_id.unwrap()).collect()
    }

    /// The nearest level decides: a child's own image beats its parent's
    /// tombstone of the same row, and a child's tombstone hides the parent's
    /// image (the pre-B2 scan applied every tombstone of the chain last).
    #[test]
    fn nearest_level_wins() {
        let engine = engine();
        let schema = schema();
        put_main(&engine, 1, &row(1, "main1"));
        put_main(&engine, 2, &row(2, "main2"));
        let parent = BranchView::for_chain(&engine, "p", vec![7]).unwrap();
        let child = BranchView::for_chain(&engine, "c", vec![7, 9]).unwrap();
        parent.delete(None, "t", 1).unwrap();
        parent.put(None, "t", 2, &row(2, "p2")).unwrap();
        child.put(None, "t", 1, &row(1, "c1")).unwrap();
        child.delete(None, "t", 2).unwrap();

        assert_eq!(ids(&parent.scan(None, "t", &schema).unwrap()), vec![2]);
        let seen = child.scan(None, "t", &schema).unwrap();
        assert_eq!(
            ids(&seen),
            vec![1],
            "the child's own row 1 survives the parent's tombstone"
        );
        assert_eq!(seen[0].values[1], Value::String("c1".into()));
        assert!(child.get(None, "t", 2, &schema).unwrap().is_none());
        assert_eq!(
            child.get(None, "t", 1, &schema).unwrap().unwrap().values[1],
            Value::String("c1".into())
        );
    }

    /// A tombstone and an image at the SAME level: the tombstone wins (the
    /// pre-B2 delete left both behind).
    #[test]
    fn a_tombstone_beats_an_image_at_the_same_level() {
        let engine = engine();
        let schema = schema();
        let view = BranchView::for_chain(&engine, "b", vec![3]).unwrap();
        view.insert(None, "t", 5, &row(5, "x")).unwrap();
        engine.put(&branch_tombstone_key(3, "t", 5), &[]).unwrap();
        bump_write_generation(engine.instance_id(), 3, "t");
        assert!(view.scan(None, "t", &schema).unwrap().is_empty());
        assert!(view.get(None, "t", 5, &schema).unwrap().is_none());
    }

    /// Level ids are matched exactly: level 1's keys never read as level 12's.
    #[test]
    fn level_prefixes_do_not_collide() {
        let engine = engine();
        let schema = schema();
        let one = BranchView::for_chain(&engine, "one", vec![1]).unwrap();
        let twelve = BranchView::for_chain(&engine, "twelve", vec![12]).unwrap();
        twelve.insert(None, "t", 4, &row(4, "twelve")).unwrap();
        assert!(one.scan(None, "t", &schema).unwrap().is_empty());
        assert_eq!(ids(&twelve.scan(None, "t", &schema).unwrap()), vec![4]);
    }

    /// `rows_with_key` with no index over the columns falls back to scanning
    /// main, and filters every row the overlay decides.
    #[test]
    fn rows_with_key_hides_rows_the_branch_changed() {
        let engine = engine();
        let schema = schema();
        put_main(&engine, 1, &row(1, "k"));
        put_main(&engine, 2, &row(2, "k"));
        let view = BranchView::for_chain(&engine, "b", vec![5]).unwrap();
        view.delete(None, "t", 1).unwrap();
        view.put(None, "t", 2, &row(2, "other")).unwrap();
        view.insert(None, "t", 3, &row(3, "k")).unwrap();
        let holders = view
            .rows_with_key(None, "t", &schema, &["v".to_string()], &[Value::String("k".into())])
            .unwrap();
        assert_eq!(ids(&holders), vec![3]);
        let none = view
            .rows_with_key(None, "t", &schema, &["v".to_string()], &[Value::Null])
            .unwrap();
        assert!(none.is_empty(), "NULL matches nothing");
    }

    /// The committed overlay is cached across calls and kept current by the
    /// view's own writes; a write another writer made visible (and announced
    /// by bumping the generation) forces a reload.
    #[test]
    fn the_committed_cache_is_write_through_and_reloads_after_another_writer() {
        let engine = engine();
        let schema = schema();
        let view = BranchView::for_chain(&engine, "b", vec![8]).unwrap();
        let columns = ["v".to_string()];
        let key = [Value::String("k".into())];
        assert!(view
            .rows_with_key(None, "t", &schema, &columns, &key)
            .unwrap()
            .is_empty());
        // Our own write: applied to the cached overlay.
        view.insert(None, "t", 5, &row(5, "k")).unwrap();
        assert_eq!(
            ids(&view.rows_with_key(None, "t", &schema, &columns, &key).unwrap()),
            vec![5]
        );
        // Another writer's: raw, then announced.
        engine
            .put(&branch_row_key(8, "t", 9), &bincode::serialize(&row(9, "k")).unwrap())
            .unwrap();
        bump_write_generation(engine.instance_id(), 8, "t");
        assert_eq!(
            ids(&view.rows_with_key(None, "t", &schema, &columns, &key).unwrap()),
            vec![5, 9]
        );
    }

    /// An ancestor level's write invalidates a descendant's cached overlay.
    #[test]
    fn an_ancestor_write_invalidates_the_childs_cached_overlay() {
        let engine = engine();
        let schema = schema();
        let parent = BranchView::for_chain(&engine, "p", vec![20]).unwrap();
        let child = BranchView::for_chain(&engine, "c", vec![20, 21]).unwrap();
        let columns = ["v".to_string()];
        let key = [Value::String("k".into())];
        assert!(child
            .rows_with_key(None, "t", &schema, &columns, &key)
            .unwrap()
            .is_empty());
        parent.insert(None, "t", 30, &row(30, "k")).unwrap();
        assert_eq!(
            ids(&child.rows_with_key(None, "t", &schema, &columns, &key).unwrap()),
            vec![30]
        );
    }

    /// A claim held by one owner refuses another until it is dropped.
    #[test]
    fn claims_serialise_writers_of_one_key() {
        let engine = engine();
        let view = BranchView::for_chain(&engine, "b", vec![6]).unwrap();
        let rule = EnforcedUniqueIndex {
            index_name: "t_pkey".into(),
            label: "t_pkey".into(),
            columns: vec!["id".into()],
            is_primary_key: true,
        };
        let key = [Value::Int4(1)];
        let first = view.claim(10, "t", &rule, &key).unwrap().unwrap();
        assert!(
            view.claim(10, "t", &rule, &key).unwrap().is_none(),
            "re-entrant for the owner"
        );
        assert!(view.claim(11, "t", &rule, &key).is_err(), "another owner is refused");
        drop(first);
        assert!(view.claim(11, "t", &rule, &key).unwrap().is_some());
    }

    /// The cache never holds more than its row budget.
    #[test]
    fn the_committed_cache_respects_its_row_budget() {
        let mut cache = CommittedCache::default();
        let overlay = |rows: usize| {
            let set = RowSet::from_rows((0..rows as u64).map(|id| (id, None)).collect());
            Arc::new(parking_lot::Mutex::new(CommittedOverlay {
                width: 1,
                schema_generation: 0,
                generations: vec![0],
                set,
            }))
        };
        let key = |n: u64| CommittedKey {
            engine: 1,
            branch: n,
            table: "t".into(),
        };
        let half = COMMITTED_OVERLAY_ROW_BUDGET / 2 + 1;
        cache.admit(key(1), overlay(10), half);
        cache.admit(key(2), overlay(10), half);
        assert!(cache.rows <= COMMITTED_OVERLAY_ROW_BUDGET);
        assert!(cache.entries.contains_key(&key(2)), "the newest entry is kept");
        assert!(
            !cache.entries.contains_key(&key(1)),
            "the least recently used entry was evicted"
        );
        cache.admit(key(3), overlay(10), COMMITTED_OVERLAY_ROW_BUDGET + 1);
        assert!(
            !cache.entries.contains_key(&key(3)),
            "an overlay over the budget is not cached"
        );

        // An overlay that GROWS (fast-forwarded commits) is re-accounted: past
        // the budget it is dropped, and it never pushes the cache over it.
        let grown = overlay(10);
        cache.admit(key(4), Arc::clone(&grown), 10);
        cache.resize(&key(4), &grown, half);
        assert!(cache.rows <= COMMITTED_OVERLAY_ROW_BUDGET, "{} rows cached", cache.rows);
        assert!(cache.entries.contains_key(&key(4)), "the grown entry is kept");
        assert!(!cache.entries.contains_key(&key(2)), "an older entry made room for it");
        cache.resize(&key(4), &grown, COMMITTED_OVERLAY_ROW_BUDGET + 1);
        assert!(
            !cache.entries.contains_key(&key(4)),
            "an overlay grown past the budget is dropped"
        );
        assert_eq!(cache.rows, 0);
    }
}
