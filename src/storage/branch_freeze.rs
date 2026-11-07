//! sprinter 6bc4a4040eb7 (phase B4): a branch is a fork. Copy-on-write at the
//! parent's write.
//!
//! Plan `docs/plans/BRANCH_ISOLATION.md` D5, spec
//! `docs/plans/specs/B4_branch_fork_point.md`. Branch reads stay what B2 defined:
//! main's current rows with each level's overlay applied, nearest level winning.
//! What makes that a fork is the WRITE side: before a level `P` (main, or a
//! branch) changes a row a live child `C` of `P` sees, what `C` sees goes into
//! `C`'s own overlay, marked as a copy (`bfrz:`) so MERGE never takes it for a
//! change.
//!
//! # Where it runs
//!
//! Every write of a store passes through its `WriteGate`, and every component
//! that holds the raw `Arc<DB>` shares one gate. With no live child the gate
//! pays one atomic load ([`super::write_gate`]). With one, a batch goes through
//! `WriteGate::write_frozen`, on the writer's own thread:
//!
//! 1. [`BranchFreeze::row_touches`] walks the batch's keys. `data:{t}:{r}` is a
//!    row of main, `bdata:{P}:{t}:{r}` and `bdel:{P}:{t}:{r}` a row of `P`; a
//!    level no live branch depends on is ignored. Other keys (indexes, versions,
//!    counters, sequences, catalog) cost a prefix comparison. No row key: the
//!    batch is applied with nothing taken.
//! 2. The registry is taken shared, for the rest of the write.
//! 3. [`BranchFreeze::stage`] takes the batch's locks, and for each row and each
//!    live child decides whether the child still reads the row from `P`, reads
//!    `P`'s pre-image, and adds the child's keys to the batch.
//! 4. The batch is applied, the children's write generations move.
//!
//! So the frozen copy and `P`'s change are one write group in the ordinary case,
//! and a standby receives them together; its applier treats them like any other
//! bytes (`KeyClass::BranchData`, `BranchDel`, `BranchFrozen`), and a standby
//! never runs this: nothing installs a hook on a store that does not accept
//! writes. A promoted standby opens as a primary, and the registry is rebuilt
//! from the branch registry at that open.
//!
//! # What a row's copy is
//!
//! `P`'s view of the row read before the batch is applied, decoded for the key it
//! came from and sealed for the key it goes to, never copied as stored bytes. A
//! reference into side storage (a dictionary code, a content hash, a columnar
//! slot) is resolved to its value: an overlay holds logical rows. `P` sees nothing
//! there: a tombstone, also marked. A row the child already holds an entry for, on
//! its own level or on a merged level between it and `P`, is the child's, and is
//! left alone; so is a table the child, or such a level, defines isolated or
//! dropped: it never reads `P`'s rows.
//!
//! A statement that writes one row twice in one batch gets one copy: the rows of a
//! batch are deduplicated first. A transaction on `P` staged before a fork and
//! committed after it reads its pre-image at commit, through here: that is the
//! state at the fork, or the child already holds a copy from an earlier commit.
//! A rollback writes nothing, so nothing happens.
//!
//! A copy does not change what any level sees, so the keys the hook writes do not
//! go through the hook: a child of the child reads the same row through it either
//! way.
//!
//! # Serialization: the row stripes and the table locks
//!
//! Between "the child has no entry" and the write, a concurrent write of the
//! child's own row would be overwritten by the copy. And two writers of a parent's
//! row must not both read a pre-image. So a row's check and its write happen under
//! one of [`ROW_STRIPES`] mutexes, picked by table and row, whatever the level:
//! `P`'s writer and `C`'s writer of a row meet there, and two writers of one row
//! on two levels of a chain are ordered. Stripes are taken in ascending order.
//!
//! A batch of more than [`BIG_BATCH_ROWS`] rows would want most of the stripes, and
//! stall the writers of every table. So it takes none: it excludes the writers of its
//! own tables instead, with a per-table lock small batches take shared and it takes
//! exclusively. [`TABLE_LOCKS`] locks, picked by a hash of the table's name, so two
//! tables share one by chance alone. No level matters to them, as none does to a
//! stripe. A store with no live child reaches none of this.
//!
//! # Lock order
//!
//! Outermost first, taken in this order and only this order:
//!
//! 1. a schema statement's place in the fork fence
//!    ([`BranchFreeze::enter_schema_statement`], a count, not held as a lock). Entered
//!    before anything below, and never waited for by a thread whose statement already
//!    holds a writer lock (it is refused at once instead): a fork waiting for the
//!    statements in the fence never waits, through one of them, for such a lock;
//! 2. statement-long: a branch's drain section (`BranchDrains`), then the writer locks
//!    of spec R5.1 (`storage::table_locks`, per level and table, sorted within a
//!    level). A parent branch's schema statement takes a child's after its own
//!    (`StorageEngine::freeze_table_into_children`), and a merge the target's, then
//!    the source's, then the target's other children's: parent before child, always,
//!    and a branch's statement takes no lock of another level;
//! 3. a definition stripe ([`BranchFreeze::lock_definition`]), for a frozen record;
//! 4. the registry, shared, for the rest of the write;
//! 5. the hook's table locks of the batch, in ascending index, shared or exclusive;
//! 6. the row stripes of the batch, in ascending index, a small batch only;
//! 7. a dictionary's read lock for a reference, and RocksDB.
//!
//! Nothing that waits for a table lock or a stripe holds anything the holder needs:
//! the holder of 5 and 6 takes nothing further but 7, and no write nests inside
//! another. Every writer lock of 2 is taken before 3: no thread waits for one while
//! it holds a stripe, the registry or a hook lock. A new branch takes the registry
//! exclusively for an instant, which is exactly the drain the creation needs (see
//! `StorageEngine::create_branch_at_snapshot`): it holds no lock of 2 to 6 while it
//! waits, only the fence, which no writer of rows asks for.
//!
//! R5.1's detector looks at the writer's own keys, before the hook adds the
//! children's copies: those are the parent statement's, under its writer lock on the
//! parent's table, and ordered against the child's own writers by 5 and 6. A
//! statement of the child cannot hold the parent's lock. Copies written apart from
//! the writer's batch (chunks, a side write's) go through no detector.
//!
//! # A big batch
//!
//! One write group may not grow without bound: `DROP TABLE` of a large table is one
//! batch of a delete per row, and a transaction can stage many rows. A batch of more
//! than [`FREEZE_CHUNK_ROWS`] rows (or that is already large, or whose copies are)
//! is frozen BEFORE it is applied, in writes of their own of at most
//! [`FREEZE_CHUNK_ROWS`] rows and [`FREEZE_CHUNK_BYTES`] bytes. Its table locks are
//! taken once and held through all of those writes and the batch's own: one let go
//! early would let a parent's writer of one of its rows decide "the child has no
//! entry" and land its copy after the batch, over the child's own write. So the writers
//! of those tables wait for it, and writers of other tables do not. Readers stay
//! consistent at every boundary because a copy equals what the child already sees. A crash between chunks
//! leaves copies, which nothing mistakes for a change, and the parent's batch
//! unapplied. So memory is the batch plus one chunk, whatever the table's size, and
//! the hook never grows a batch that is near the write-group limit: a merge's or a
//! materialization's last batch keeps what it was validated against.
//!
//! # What a reader must do
//!
//! A reader of a level reads main FIRST and the overlay AFTER: if it saw a parent's
//! new row, the copy of the old one is already there. A cached overlay cannot do
//! that, so `BranchFreeze::mark_in_flight` says a level's table has a copy on the
//! way, and `BranchView::committed_overlay` then loads from storage. A cached copy
//! is invalidated by the write generation the hook moves after the write.
//!
//! # Definitions
//!
//! Rows are frozen here. A table's DEFINITION is frozen on the way into a statement,
//! because the gate is too late for it: `DROP TABLE` has torn its ART registration
//! down by the time its batch gets here, and a definition's unique rules live there.
//! It does not need the statement's batch either: the copy equals what the child
//! sees, so a statement that then fails, or a crash, leaves it true.
//!
//! * Main changing a table (`StorageEngine::freeze_definitions_for_children`, from
//!   the catalog funnels): each live child that reads the table through main gets a
//!   marked record of the definition, not isolated: main stays what it reads the rows
//!   from, and the rows main rewrites or drops are frozen here as they go. A table main
//!   is creating gets a marked tombstone.
//! * A branch changing a table (`StorageEngine::freeze_table_into_children`, from its
//!   schema statements): every such change makes that branch's table isolated or
//!   dropped, so from then on it reads nothing from above, and a child could no longer
//!   read its unchanged rows through it. So the child first takes all of them
//!   (`BranchView::materialize`, in chunks, marked) and an isolated, marked record. It
//!   then depends on nothing above itself for that table.
//!
//! A marked record is what MERGE skips, like a marked row. What a batch only copies
//! (it marks the rows it writes) changes no level's view, so it freezes nothing for the
//! level's own children: this is also why a child's materialization does not cascade.
//!
//! A schema statement on main is wholly before a fork of main or wholly after it, so a
//! child's definition and rows always come from the same side of it. A statement that
//! rewrites rows and changes the definition in several writes could otherwise straddle
//! the registration: it froze no definition, for it saw no child, and the child then
//! took copies of the old rows and read the new definition. So the statement enters
//! [`BranchFreeze::enter_schema_statement`] before it looks for children, and the
//! creation of a branch of main holds new ones out and waits for the running ones
//! ([`BranchFreeze::hold_schema_statements`]) across the registration. One that ran
//! finished before the child existed; one that starts afterwards finds it.
//!
//! # The fork record, and a branch from before all this
//!
//! A branch created by an earlier release has no fork record and nothing frozen. The
//! hook treats it like any other from the first open on, so it reads what its parent
//! held then, and is a fork from there on. MERGE still refuses it for any table it
//! changed rows of: nothing says what they were written for.

use super::branch_catalog::{
    branch_table_key, known_incarnation, BranchTableDefinition, BranchTableRecord, BranchTableState,
};
use super::branch_view::{self, branch_frozen_key, branch_row_key, branch_tombstone_key, BranchView};
use super::catalog::TableShapeChange;
use super::content_addr::ContentAddressedStore;
use super::dictionary::DictionaryManager;
use super::write_gate::WriteGate;
use super::{tde, BranchManager, Catalog, ColumnarStore, StorageEngine, BRANCH_DRAIN_TIMEOUT};
use crate::crypto::KeyManager;
use crate::{Error, Result, Schema, Tuple, Value};
use dashmap::DashMap;
use parking_lot::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use rocksdb::{WriteBatch, WriteBatchIterator, DB};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Main's id in the branch registry (`BranchRegistry::new`). `data:` keys are its
/// rows; it has no `bdata:` level of its own.
pub const MAIN_LEVEL: u64 = 1;

/// How many row stripes. A batch of more rows than this takes most of them.
const ROW_STRIPES: usize = 256;

/// How many definition stripes.
const DEFINITION_STRIPES: usize = 64;

/// How many per-table locks; see "Serialization" in the module docs.
const TABLE_LOCKS: usize = 1024;

/// A batch of more rows than this excludes the writers of its tables, and takes no
/// row stripes.
const BIG_BATCH_ROWS: usize = 64;

/// The most rows frozen in one write.
const FREEZE_CHUNK_ROWS: usize = 10_000;

/// The most bytes of copies in one write of their own.
const FREEZE_CHUNK_BYTES: usize = 64 * 1024 * 1024;

/// A batch this large gets its copies in writes of their own: it may be a merge's
/// table or a materialization's last batch, validated against the write-group limit.
const INLINE_BATCH_LIMIT: usize = 64 * 1024 * 1024;

/// The most bytes of copies added to the batch they freeze for.
const INLINE_COPIES_LIMIT: usize = 16 * 1024 * 1024;

/// A level as the hook knows it. Merged levels stay while a live branch reads
/// through them (B3: M4, M5).
#[derive(Debug, Clone, Copy)]
struct Node {
    parent: u64,
    /// Written to, and read as a branch: Active, or merged with an intent (an
    /// `ABORT` puts it back, so it must not have moved).
    live: bool,
}

/// A live branch that reads through a level.
#[derive(Debug, Clone)]
pub struct LiveChild {
    /// The branch.
    pub(crate) id: u64,
    /// The levels between the parent and it, it last: merged ones, whose overlays
    /// it still reads.
    pub(crate) path: Vec<u64>,
}

/// Which levels exist, as of the last create, drop or merge.
#[derive(Debug, Default)]
pub struct Registry {
    nodes: HashMap<u64, Node>,
    children: HashMap<u64, Arc<[LiveChild]>>,
    chains: HashMap<u64, Arc<[u64]>>,
}

impl Registry {
    fn recompute(&mut self) {
        // A merged level with no live branch under it is gone from here.
        loop {
            let parents: HashSet<u64> = self.nodes.values().map(|node| node.parent).collect();
            let before = self.nodes.len();
            self.nodes.retain(|id, node| node.live || parents.contains(id));
            if self.nodes.len() == before {
                break;
            }
        }
        let mut children: HashMap<u64, Vec<LiveChild>> = HashMap::new();
        let mut chains: HashMap<u64, Arc<[u64]>> = HashMap::new();
        for (&id, node) in &self.nodes {
            let mut chain = vec![id];
            let mut cursor = node.parent;
            let mut hops = 0usize;
            while let Some(parent) = self.nodes.get(&cursor) {
                chain.push(cursor);
                cursor = parent.parent;
                hops += 1;
                if hops > self.nodes.len() {
                    break;
                }
            }
            chain.reverse();
            chains.insert(id, Arc::from(chain));
            if !node.live {
                continue;
            }
            let mut path = vec![id];
            let mut parent = node.parent;
            let mut hops = 0usize;
            while let Some(level) = self.nodes.get(&parent).filter(|level| !level.live) {
                path.insert(0, parent);
                parent = level.parent;
                hops += 1;
                if hops > self.nodes.len() {
                    break;
                }
            }
            children.entry(parent).or_default().push(LiveChild { id, path });
        }
        self.children = children.into_iter().map(|(id, kids)| (id, Arc::from(kids))).collect();
        self.chains = chains;
    }

    fn any_live(&self) -> bool {
        self.nodes.values().any(|node| node.live)
    }

    /// The live branches that read through `level`.
    pub(crate) fn children_of(&self, level: u64) -> Option<&Arc<[LiveChild]>> {
        self.children.get(&level)
    }

    /// `level`'s chain, oldest first, ending with it; main is not in it.
    pub(crate) fn chain_of(&self, level: u64) -> Option<&Arc<[u64]>> {
        self.chains.get(&level)
    }

    fn knows(&self, level: u64) -> bool {
        level == MAIN_LEVEL || self.nodes.contains_key(&level)
    }
}

/// What a batch does to rows, by (level, table).
#[derive(Debug, Default)]
pub struct RowTouches {
    groups: BTreeMap<(u64, String), TableTouches>,
}

#[derive(Debug, Default)]
struct TableTouches {
    rows: BTreeSet<u64>,
    /// Rows this batch marks as copies (`bfrz:` puts). They do not change what the
    /// level sees: see [`BranchFreeze::stage`].
    copies: BTreeSet<u64>,
}

impl RowTouches {
    pub(crate) fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// One row of main.
    fn main_row(table: &str, row: u64) -> Self {
        let mut touches = Self::default();
        touches
            .groups
            .entry((MAIN_LEVEL, table.to_string()))
            .or_default()
            .rows
            .insert(row);
        touches
    }
}

struct TouchCollector(RowTouches);

impl TouchCollector {
    fn note(&mut self, key: &[u8], put: bool) {
        // One or two byte comparisons for a key that is not a row's.
        let first = key.first().copied();
        if first == Some(b'd') {
            if let Some(rest) = key.strip_prefix(b"data:".as_slice()) {
                if let Some((table, row)) = table_and_row(rest) {
                    self.touch(MAIN_LEVEL, table, row);
                }
            }
            return;
        }
        if first != Some(b'b') {
            return;
        }
        let (rest, frozen) = if let Some(rest) = key.strip_prefix(b"bdata:".as_slice()) {
            (rest, false)
        } else if let Some(rest) = key.strip_prefix(b"bdel:".as_slice()) {
            (rest, false)
        } else if let Some(rest) = key.strip_prefix(b"bfrz:".as_slice()) {
            (rest, true)
        } else {
            return;
        };
        let Some((level, rest)) = level_and_rest(rest) else {
            return;
        };
        let Some((table, row)) = table_and_row(rest) else {
            return;
        };
        if frozen {
            if put {
                self.0
                    .groups
                    .entry((level, table.to_string()))
                    .or_default()
                    .copies
                    .insert(row);
            }
            return;
        }
        self.touch(level, table, row);
    }

    fn touch(&mut self, level: u64, table: &str, row: u64) {
        if crate::sql::temp_tables::is_temp_key(table) {
            return;
        }
        self.0
            .groups
            .entry((level, table.to_string()))
            .or_default()
            .rows
            .insert(row);
    }
}

impl WriteBatchIterator for TouchCollector {
    fn put(&mut self, key: Box<[u8]>, _value: Box<[u8]>) {
        self.note(&key, true);
    }

    fn delete(&mut self, key: Box<[u8]>) {
        self.note(&key, false);
    }
}

/// `{table}:{row}`. A table's name may hold a ':'; a row id is the digits after the
/// last one.
fn table_and_row(rest: &[u8]) -> Option<(&str, u64)> {
    let text = std::str::from_utf8(rest).ok()?;
    let (table, row) = text.rsplit_once(':')?;
    if table.is_empty() || row.is_empty() || !row.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((table, row.parse().ok()?))
}

/// `{level}:{rest}` with a decimal level. Anything else is not this format (the
/// binary keys of the `BranchTransaction` API, which no SQL path writes).
pub fn level_and_rest(rest: &[u8]) -> Option<(u64, &[u8])> {
    let colon = rest.iter().position(|byte| *byte == b':')?;
    let digits = rest.get(..colon)?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let level = std::str::from_utf8(digits).ok()?.parse().ok()?;
    Some((level, rest.get(colon + 1..)?))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn row_stripe(table_hash: u64, row: u64) -> usize {
    let mixed = (table_hash ^ row).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (mixed >> 56) as usize % ROW_STRIPES
}

fn table_lock(table_hash: u64) -> usize {
    (table_hash % TABLE_LOCKS as u64) as usize
}

/// See the module docs. One per engine that accepts writes.
pub struct BranchFreeze {
    engine_id: u64,
    key_manager: Option<Arc<KeyManager>>,
    dictionaries: Arc<DictionaryManager>,
    active: Arc<AtomicBool>,
    registry: RwLock<Registry>,
    /// One creation at a time, through its drain.
    creating: Mutex<()>,
    /// Main's schema statements against a fork of main: see "Definitions" in the
    /// module docs.
    schema_statements: Mutex<SchemaStatements>,
    schema_statements_changed: Condvar,
    table_locks: Vec<RwLock<()>>,
    stripes: Vec<Mutex<()>>,
    definition_stripes: Vec<Mutex<()>>,
    /// (level, table) with a copy on its way; see the module docs.
    in_flight: DashMap<(u64, String), usize>,
    in_flight_total: AtomicUsize,
}

/// See [`BranchFreeze::schema_statements`].
#[derive(Debug, Default)]
struct SchemaStatements {
    /// Running on main now.
    running: usize,
    /// A branch of main is registering: new ones wait.
    held: bool,
}

/// New schema statements on main wait while this lives, and none is running.
#[must_use]
pub struct SchemaStatementsHeld<'f> {
    freeze: &'f BranchFreeze,
}

impl Drop for SchemaStatementsHeld<'_> {
    fn drop(&mut self) {
        self.freeze.schema_statements.lock().held = false;
        self.freeze.schema_statements_changed.notify_all();
    }
}

impl std::fmt::Debug for BranchFreeze {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchFreeze")
            .field("engine_id", &self.engine_id)
            .field("active", &self.active.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl BranchFreeze {
    /// A hook with no levels. `active` is the gate's flag.
    pub(crate) fn new(
        engine_id: u64,
        key_manager: Option<Arc<KeyManager>>,
        dictionaries: Arc<DictionaryManager>,
        active: Arc<AtomicBool>,
    ) -> Self {
        Self {
            engine_id,
            key_manager,
            dictionaries,
            active,
            registry: RwLock::new(Registry::default()),
            creating: Mutex::new(()),
            schema_statements: Mutex::new(SchemaStatements::default()),
            schema_statements_changed: Condvar::new(),
            table_locks: (0..TABLE_LOCKS).map(|_| RwLock::new(())).collect(),
            stripes: (0..ROW_STRIPES).map(|_| Mutex::new(())).collect(),
            definition_stripes: (0..DEFINITION_STRIPES).map(|_| Mutex::new(())).collect(),
            in_flight: DashMap::new(),
            in_flight_total: AtomicUsize::new(0),
        }
    }

    /// The engine this hook belongs to.
    pub(crate) fn engine_id(&self) -> u64 {
        self.engine_id
    }

    // ---- the registry ----

    /// Rebuild from the branch registry, at open: `(id, parent, live)`.
    pub(crate) fn rebuild(&self, levels: impl IntoIterator<Item = (u64, u64, bool)>) {
        let mut registry = self.registry.write();
        registry.nodes = levels
            .into_iter()
            .map(|(id, parent, live)| (id, Node { parent, live }))
            .collect();
        registry.recompute();
        self.active.store(registry.any_live(), Ordering::SeqCst);
    }

    /// One creation at a time: `register` and the drain that follows it.
    pub(crate) fn creation_lock(&self, timeout: Duration) -> Option<MutexGuard<'_, ()>> {
        self.creating.try_lock_for(timeout)
    }

    /// A branch is being created: from when this returns, every write of its
    /// parent freezes for it, and no write that read the registry without it is
    /// still running. Returns whether the flag was already up: if it was not, the
    /// writers that had not seen it hold no registry, and the caller waits for them
    /// (`WriteGate::wait_for_unhooked_writes`). `None`: the time ran out.
    pub(crate) fn register(&self, id: u64, parent: u64, timeout: Duration) -> Option<bool> {
        let mut registry = self.registry.try_write_for(timeout)?;
        registry.nodes.insert(id, Node { parent, live: true });
        registry.recompute();
        Some(self.active.swap(true, Ordering::SeqCst))
    }

    /// A branch published without a drain (a manager with no engine in front of
    /// it): writes freeze for it from now on. A no-op for one already registered.
    pub(crate) fn note_published(&self, id: u64, parent: u64) {
        let mut registry = self.registry.write();
        if registry.nodes.contains_key(&id) {
            return;
        }
        registry.nodes.insert(id, Node { parent, live: true });
        registry.recompute();
        self.active.store(true, Ordering::SeqCst);
    }

    /// A branch is gone. When this returns, no write that could still freeze into
    /// it is running: its keys can be deleted.
    pub(crate) fn remove(&self, id: u64) {
        let mut registry = self.registry.write();
        registry.nodes.remove(&id);
        registry.recompute();
        self.active.store(registry.any_live(), Ordering::SeqCst);
    }

    /// A merge of `id` finished. It stays in its live descendants' chains, read and
    /// never written; with none it is gone. As [`Self::remove`] for what is running.
    pub(crate) fn retire(&self, id: u64) {
        let mut registry = self.registry.write();
        if let Some(node) = registry.nodes.get_mut(&id) {
            node.live = false;
        }
        registry.recompute();
        self.active.store(registry.any_live(), Ordering::SeqCst);
    }

    /// The live branches that read through `level`, and `level`'s own chain.
    pub(crate) fn children_and_chain(&self, level: u64) -> (Vec<LiveChild>, Vec<u64>) {
        let registry = self.registry.read();
        let children = registry
            .children_of(level)
            .map(|kids| kids.to_vec())
            .unwrap_or_default();
        let chain = registry.chain_of(level).map(|chain| chain.to_vec()).unwrap_or_default();
        (children, chain)
    }

    /// The registry, shared, for one write. See the module docs.
    pub(crate) fn registry_for_write(&self) -> RwLockReadGuard<'_, Registry> {
        self.registry.read()
    }

    /// Wait for the writes that hold the registry, for at most `timeout`.
    pub(crate) fn wait_for_writers(&self, timeout: Duration) -> bool {
        self.registry.try_write_for(timeout).is_some()
    }

    /// Serialize a level's change of a table's definition with a parent's freeze
    /// of it: the check that the level has no record of its own and the write of
    /// the copy are one step. These stripes are not the rows': a definition's
    /// write may carry rows, which take theirs under this.
    pub(crate) fn lock_definition(&self, table: &str) -> MutexGuard<'_, ()> {
        let index = fnv1a(table.as_bytes()) as usize % DEFINITION_STRIPES;
        match self.definition_stripes.get(index) {
            Some(stripe) => stripe.lock(),
            // The index is reduced by the length just above.
            None => self.creating.lock(),
        }
    }

    // ---- main's schema statements and a fork of main ----

    /// A schema statement on main starts: it waits while a branch of main registers.
    /// Call before looking for children. False: the time ran out, and it did not enter.
    pub(crate) fn enter_schema_statement(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut statements = self.schema_statements.lock();
        while statements.held {
            if self
                .schema_statements_changed
                .wait_until(&mut statements, deadline)
                .timed_out()
            {
                return false;
            }
        }
        statements.running += 1;
        true
    }

    /// A statement [`Self::enter_schema_statement`] let in is finished.
    pub(crate) fn leave_schema_statement(&self) {
        let mut statements = self.schema_statements.lock();
        statements.running = statements.running.saturating_sub(1);
        drop(statements);
        self.schema_statements_changed.notify_all();
    }

    /// A branch of main is about to register: keep new schema statements out, and
    /// wait for the running ones. `None`: the time ran out, and nothing is held.
    pub(crate) fn hold_schema_statements(&self, timeout: Duration) -> Option<SchemaStatementsHeld<'_>> {
        let deadline = Instant::now() + timeout;
        let mut statements = self.schema_statements.lock();
        while statements.held {
            if self
                .schema_statements_changed
                .wait_until(&mut statements, deadline)
                .timed_out()
            {
                return None;
            }
        }
        statements.held = true;
        while statements.running > 0 {
            if self
                .schema_statements_changed
                .wait_until(&mut statements, deadline)
                .timed_out()
                && statements.running > 0
            {
                statements.held = false;
                drop(statements);
                self.schema_statements_changed.notify_all();
                return None;
            }
        }
        Some(SchemaStatementsHeld { freeze: self })
    }

    // ---- what a reader asks ----

    /// Is a copy on its way into `table` on one of `levels`? One relaxed load when
    /// nothing is.
    pub(crate) fn in_flight(&self, levels: &[u64], table: &str) -> bool {
        if self.in_flight_total.load(Ordering::SeqCst) == 0 {
            return false;
        }
        levels
            .iter()
            .any(|level| self.in_flight.contains_key(&(*level, table.to_string())))
    }

    fn mark_in_flight(&self, level: u64, table: &str) {
        self.in_flight_total.fetch_add(1, Ordering::SeqCst);
        *self.in_flight.entry((level, table.to_string())).or_insert(0) += 1;
    }

    fn unmark_in_flight(&self, level: u64, table: &str) {
        if let dashmap::mapref::entry::Entry::Occupied(mut entry) = self.in_flight.entry((level, table.to_string())) {
            let remaining = entry.get().saturating_sub(1);
            if remaining == 0 {
                entry.remove();
            } else {
                *entry.get_mut() = remaining;
            }
        }
        self.in_flight_total.fetch_sub(1, Ordering::SeqCst);
    }

    // ---- one write ----

    /// What `batch` does to rows. Nothing at all for a batch of other keys.
    pub(crate) fn row_touches(batch: &WriteBatch) -> RowTouches {
        let mut collector = TouchCollector(RowTouches::default());
        batch.iterate(&mut collector);
        collector.0
    }

    /// Freeze for `touches`, before `batch` is applied. See the module docs. The
    /// returned work holds the batch's locks and what readers were told; it is
    /// dropped after the batch was applied. On an error the batch must not be applied,
    /// and nothing a child sees has changed.
    pub(crate) fn stage<'f>(
        &'f self,
        registry: &Registry,
        db: &DB,
        touches: &RowTouches,
        batch: &mut WriteBatch,
        flush: &mut dyn FnMut(WriteBatch) -> Result<()>,
    ) -> Result<FreezeWork<'f>> {
        let mut work = FreezeWork {
            freeze: self,
            marked: HashSet::new(),
            shared_tables: Vec::new(),
            alone_tables: Vec::new(),
            stripes: Vec::new(),
        };
        // (group, table hash, row, copy), only levels something depends on.
        let groups: Vec<(&(u64, String), &TableTouches, u64)> = touches
            .groups
            .iter()
            .filter(|((level, _), rows)| registry.knows(*level) && !rows.rows.is_empty())
            .map(|(group, rows)| (group, rows, fnv1a(group.1.as_bytes())))
            .collect();
        let mut flat: Vec<(usize, u64)> = Vec::new();
        for (index, (_, rows, _)) in groups.iter().enumerate() {
            flat.extend(rows.rows.iter().map(|row| (index, *row)));
        }
        let inline = flat.len() <= FREEZE_CHUNK_ROWS && batch.size_in_bytes() <= INLINE_BATCH_LIMIT;
        let mut context = FreezeContext::new(self, db, registry);
        let mut side = WriteBatch::default();
        let mut inline_bytes = 0usize;
        // Once, in ascending order, for the whole batch: "Lock order" in the module docs.
        let big = flat.len() > BIG_BATCH_ROWS;
        let tables: BTreeSet<usize> = groups.iter().map(|group| table_lock(group.2)).collect();
        for lock in tables.iter().filter_map(|index| self.table_locks.get(*index)) {
            if big {
                work.alone_tables.push(lock.write());
            } else {
                work.shared_tables.push(lock.read());
            }
        }
        if !big {
            let wanted: BTreeSet<usize> = flat
                .iter()
                .filter_map(|(index, row)| groups.get(*index).map(|group| row_stripe(group.2, *row)))
                .collect();
            work.stripes = wanted
                .iter()
                .filter_map(|index| self.stripes.get(*index))
                .map(|stripe| stripe.lock())
                .collect();
        }
        for chunk in flat.chunks(FREEZE_CHUNK_ROWS) {
            for (index, row) in chunk {
                let Some(((level, table), rows, _)) = groups.get(*index) else {
                    continue;
                };
                if rows.copies.contains(row) {
                    continue;
                }
                let target: &mut WriteBatch = if inline && inline_bytes < INLINE_COPIES_LIMIT {
                    &mut *batch
                } else {
                    &mut side
                };
                let before = target.size_in_bytes();
                context.freeze_row(&mut work, *level, table, *row, target)?;
                if inline {
                    inline_bytes = inline_bytes.saturating_add(target.size_in_bytes().saturating_sub(before));
                }
                if side.size_in_bytes() >= FREEZE_CHUNK_BYTES {
                    flush(std::mem::take(&mut side))?;
                }
            }
            if !side.is_empty() {
                flush(std::mem::take(&mut side))?;
            }
        }
        Ok(work)
    }
}

/// A batch's freeze, until the batch was applied. See [`BranchFreeze::stage`].
pub struct FreezeWork<'f> {
    freeze: &'f BranchFreeze,
    marked: HashSet<(u64, String)>,
    shared_tables: Vec<RwLockReadGuard<'f, ()>>,
    alone_tables: Vec<RwLockWriteGuard<'f, ()>>,
    stripes: Vec<MutexGuard<'f, ()>>,
}

impl Drop for FreezeWork<'_> {
    fn drop(&mut self) {
        for (level, table) in self.marked.drain() {
            // In this order: whoever finds the mark gone finds the generation moved.
            branch_view::bump_write_generation(self.freeze.engine_id, level, &table);
            self.freeze.unmark_in_flight(level, &table);
        }
        self.stripes.clear();
        self.shared_tables.clear();
        self.alone_tables.clear();
    }
}

/// A level's rows as a reader of it sees them, by what its chain says of a table.
#[derive(Clone)]
struct ViewScope {
    /// The chain position rows are read from; the chain's length for none.
    first_level: usize,
    include_main: bool,
    /// The defining record of an isolated table: what its key index covers.
    isolated: Option<Arc<BranchTableRecord>>,
}

/// What one batch has read, so a table costs its records once.
struct FreezeContext<'a> {
    freeze: &'a BranchFreeze,
    db: &'a DB,
    registry: &'a Registry,
    records: HashMap<(u64, String), Option<Arc<BranchTableRecord>>>,
    scopes: HashMap<(u64, String), ViewScope>,
    main_schemas: HashMap<String, Arc<Schema>>,
}

impl<'a> FreezeContext<'a> {
    fn new(freeze: &'a BranchFreeze, db: &'a DB, registry: &'a Registry) -> Self {
        Self {
            freeze,
            db,
            registry,
            records: HashMap::new(),
            scopes: HashMap::new(),
            main_schemas: HashMap::new(),
        }
    }

    fn key_manager(&self) -> Option<&KeyManager> {
        self.freeze.key_manager.as_deref()
    }

    fn present(&self, key: &[u8]) -> Result<bool> {
        Ok(self
            .db
            .get_pinned(key)
            .map_err(|e| Error::storage(format!("Get failed: {e}")))?
            .is_some())
    }

    fn record(&mut self, level: u64, table: &str) -> Result<Option<Arc<BranchTableRecord>>> {
        let cache_key = (level, table.to_string());
        if let Some(record) = self.records.get(&cache_key) {
            return Ok(record.clone());
        }
        let key = branch_table_key(level, table);
        let raw = self
            .db
            .get(&key)
            .map_err(|e| Error::storage(format!("Get failed: {e}")))?;
        let record = match raw {
            Some(raw) => {
                let plain = tde::open_ref(self.key_manager(), &key, &raw)?;
                Some(Arc::new(BranchTableRecord::decode(table, &plain)?))
            }
            None => None,
        };
        self.records.insert(cache_key, record.clone());
        Ok(record)
    }

    /// Does some level of `path` own `table`: an isolated definition, or a drop? It
    /// then reads nothing of the table from above, so there is nothing to freeze.
    fn path_hides_table(&mut self, path: &[u64], table: &str) -> Result<bool> {
        for level in path {
            if let Some(record) = self.record(*level, table)? {
                if record.isolated || matches!(record.state, BranchTableState::Dropped) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Does some level of `path` hold an entry for the row? Then the branch at its
    /// end reads it from there.
    fn path_holds_row(&self, path: &[u64], table: &str, row: u64) -> Result<bool> {
        for level in path {
            if self.present(&branch_tombstone_key(*level, table, row))?
                || self.present(&branch_row_key(*level, table, row))?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `BranchView::scope`, of `level`, for rows: what a statement on it reads.
    fn view_scope(&mut self, level: u64, table: &str) -> Result<ViewScope> {
        let cache_key = (level, table.to_string());
        if let Some(scope) = self.scopes.get(&cache_key) {
            return Ok(scope.clone());
        }
        let chain: Arc<[u64]> = match self.registry.chain_of(level) {
            Some(chain) => Arc::clone(chain),
            None => Arc::from(Vec::new()),
        };
        let mut scope = ViewScope {
            first_level: 0,
            include_main: true,
            isolated: None,
        };
        for (index, chain_level) in chain.iter().enumerate().rev() {
            let Some(record) = self.record(*chain_level, table)? else {
                continue;
            };
            if matches!(record.state, BranchTableState::Dropped) {
                scope.first_level = chain.len();
                scope.include_main = false;
            } else if record.isolated {
                scope.first_level = index;
                scope.include_main = false;
                scope.isolated = Some(record);
            }
            break;
        }
        self.scopes.insert(cache_key, scope.clone());
        Ok(scope)
    }

    /// `level`'s view of the row before the batch, as logical row bytes.
    fn parent_view(&mut self, level: u64, table: &str, row: u64, scope: &ViewScope) -> Result<Option<Vec<u8>>> {
        if level != MAIN_LEVEL {
            let chain: Arc<[u64]> = match self.registry.chain_of(level) {
                Some(chain) => Arc::clone(chain),
                None => Arc::from(Vec::new()),
            };
            let levels = chain.get(scope.first_level..).unwrap_or(&[]);
            for chain_level in levels.iter().rev() {
                if self.present(&branch_tombstone_key(*chain_level, table, row))? {
                    return Ok(None);
                }
                let key = branch_row_key(*chain_level, table, row);
                let raw = self
                    .db
                    .get(&key)
                    .map_err(|e| Error::storage(format!("Get failed: {e}")))?;
                if let Some(raw) = raw {
                    return Ok(Some(tde::open_ref(self.key_manager(), &key, &raw)?.into_owned()));
                }
            }
            if !scope.include_main {
                return Ok(None);
            }
        }
        self.main_row(table, row)
    }

    /// Main's row, with its references into side storage resolved: an overlay holds
    /// logical rows.
    fn main_row(&mut self, table: &str, row: u64) -> Result<Option<Vec<u8>>> {
        let key = format!("data:{table}:{row}").into_bytes();
        let raw = self
            .db
            .get(&key)
            .map_err(|e| Error::storage(format!("Get failed: {e}")))?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let plain = tde::open_ref(self.key_manager(), &key, &raw)?.into_owned();
        let mut tuple: Tuple =
            bincode::deserialize(&plain).map_err(|e| Error::storage(format!("Failed to deserialize tuple: {e}")))?;
        if !tuple.values.iter().any(is_storage_reference) {
            return Ok(Some(plain));
        }
        let schema = self.schema_for_references(table)?;
        for (position, column) in schema.columns.iter().enumerate() {
            let Some(value) = tuple.values.get(position) else {
                break;
            };
            let resolved = match value {
                Value::DictRef { dict_id } => Some(Value::String(self.freeze.dictionaries.decode(
                    self.db,
                    table,
                    &column.name,
                    *dict_id,
                )?)),
                Value::CasRef { hash } => Some(ContentAddressedStore::resolve(self.db, hash, &column.data_type)?),
                Value::ColumnarRef => ColumnarStore::get(self.db, table, &column.name, row)?,
                _ => None,
            };
            if let (Some(resolved), Some(slot)) = (resolved, tuple.values.get_mut(position)) {
                *slot = resolved;
            }
        }
        Ok(Some(bincode::serialize(&tuple).map_err(|e| {
            Error::storage(format!("Failed to serialize tuple: {e}"))
        })?))
    }

    /// The columns a row's references are resolved by: the catalog's record, as it
    /// is before this batch.
    fn schema_for_references(&mut self, table: &str) -> Result<Arc<Schema>> {
        if let Some(schema) = self.main_schemas.get(table) {
            return Ok(Arc::clone(schema));
        }
        let key = format!("meta:table:{table}").into_bytes();
        let raw = self
            .db
            .get(&key)
            .map_err(|e| Error::storage(format!("Get failed: {e}")))?
            .ok_or_else(|| {
                Error::storage(format!(
                    "a row of table '{table}' holds a reference into side storage, and the table has no definition"
                ))
            })?;
        let plain = tde::open_ref(self.key_manager(), &key, &raw)?;
        let schema: Schema =
            bincode::deserialize(&plain).map_err(|e| Error::storage(format!("Failed to deserialize schema: {e}")))?;
        let schema = Arc::new(schema);
        self.main_schemas.insert(table.to_string(), Arc::clone(&schema));
        Ok(schema)
    }

    /// One row of `level`, for each live branch that reads it through `level`.
    fn freeze_row(
        &mut self,
        work: &mut FreezeWork<'_>,
        level: u64,
        table: &str,
        row: u64,
        target: &mut WriteBatch,
    ) -> Result<()> {
        let Some(children) = self.registry.children_of(level).map(Arc::clone) else {
            return Ok(());
        };
        let mut scope: Option<ViewScope> = None;
        let mut pre_image: Option<Option<Vec<u8>>> = None;
        for child in children.iter() {
            if self.path_hides_table(&child.path, table)? || self.path_holds_row(&child.path, table, row)? {
                continue;
            }
            if scope.is_none() {
                scope = Some(self.view_scope(level, table)?);
            }
            let Some(view) = scope.clone() else {
                continue;
            };
            if pre_image.is_none() {
                pre_image = Some(self.parent_view(level, table, row, &view)?);
            }
            let Some(pre) = pre_image.as_ref() else {
                continue;
            };
            if work.marked.insert((child.id, table.to_string())) {
                self.freeze.mark_in_flight(child.id, table);
            }
            stage_copy(self.key_manager(), target, child.id, table, row, pre.as_deref(), &view)?;
        }
        Ok(())
    }
}

/// A reference into side storage, which only a main row can hold.
fn is_storage_reference(value: &Value) -> bool {
    matches!(value, Value::DictRef { .. } | Value::CasRef { .. } | Value::ColumnarRef)
}

/// The keys of one frozen row of `level`: the image or the tombstone, the marker,
/// and, for a table some level defines isolated, the key index a probe of it needs.
fn stage_copy(
    key_manager: Option<&KeyManager>,
    target: &mut WriteBatch,
    level: u64,
    table: &str,
    row: u64,
    image: Option<&[u8]>,
    scope: &ViewScope,
) -> Result<()> {
    match image {
        Some(plain) => {
            let key = branch_row_key(level, table, row);
            let sealed = tde::seal_row_value(key_manager, &key, plain)?;
            target.put(&key, sealed.as_ref());
            if let Some(definition) = scope.isolated.as_ref().and_then(|record| record.definition()) {
                let sets = branch_view::key_index_sets(definition);
                if !sets.is_empty() {
                    let tuple: Tuple = bincode::deserialize(plain)
                        .map_err(|e| Error::storage(format!("Failed to deserialize tuple: {e}")))?;
                    for set in &sets {
                        if let Some(encoded) = branch_view::encode_positions(&tuple, &set.positions) {
                            target.put(branch_view::key_index_key(level, table, set, &encoded, row), b"");
                        }
                    }
                }
            }
        }
        None => target.put(branch_tombstone_key(level, table, row), b""),
    }
    target.put(branch_frozen_key(level, table, row), b"");
    Ok(())
}

// ---------------------------------------------------------------------------
// What the engine asks of it
// ---------------------------------------------------------------------------

/// A level's copy of a definition: `None`, a tombstone: the table was not visible
/// there. Its `parent_*` are the copied definition's: when the level later changes
/// the table itself, MERGE compares them with the target, which has moved.
fn frozen_record(definition: Option<(BranchTableDefinition, u64)>, isolated: bool) -> BranchTableRecord {
    match definition {
        Some((definition, fingerprint)) => BranchTableRecord {
            parent_fingerprint: Some(fingerprint),
            parent_incarnation: known_incarnation(definition.incarnation),
            state: BranchTableState::Defined(definition),
            isolated,
            frozen: true,
            created: false,
            added_columns: Vec::new(),
        },
        None => BranchTableRecord {
            state: BranchTableState::Dropped,
            isolated: true,
            frozen: true,
            parent_fingerprint: None,
            parent_incarnation: None,
            created: false,
            added_columns: Vec::new(),
        },
    }
}

impl StorageEngine {
    /// Build this engine's hook from the branch registry and hand it to the gate
    /// and the manager. `None` when branching is unavailable.
    pub(super) fn install_branch_freeze(
        engine_id: u64,
        gate: &WriteGate,
        key_manager: Option<&Arc<KeyManager>>,
        dictionaries: &Arc<DictionaryManager>,
        manager: &RwLock<Option<Arc<BranchManager>>>,
    ) -> Result<Option<Arc<BranchFreeze>>> {
        let guard = manager.read();
        let Some(manager) = guard.as_ref() else {
            return Ok(None);
        };
        let freeze = Arc::new(BranchFreeze::new(
            engine_id,
            key_manager.cloned(),
            Arc::clone(dictionaries),
            gate.freeze_flag(),
        ));
        freeze.rebuild(manager.levels_for_freeze()?);
        manager.install_freeze(Arc::clone(&freeze));
        gate.install_freeze(Arc::clone(&freeze));
        Ok(Some(freeze))
    }

    /// Whether a statement on a level can race a parent's write: a reader takes main
    /// first and then asks the levels (the module docs). On a standby the applier's
    /// groups do the same to it. One atomic load on a store with no branch.
    pub(crate) fn branch_reads_may_race(&self) -> bool {
        self.write_gate().active_freeze().is_some() || self.is_physical_standby()
    }

    /// Is a copy of `table` on its way into one of `levels`?
    pub(crate) fn branch_freeze_in_flight(&self, levels: &[u64], table: &str) -> bool {
        self.write_gate()
            .active_freeze()
            .is_some_and(|freeze| freeze.in_flight(levels, table))
    }

    /// A row of main is about to change in a way its `data:` key does not show yet: a
    /// columnar slot is rewritten before the row. Freeze it now, in a write of its own.
    pub(crate) fn freeze_row_before_side_write(&self, table: &str, row: u64) -> Result<()> {
        let Some(freeze) = self.write_gate().active_freeze() else {
            return Ok(());
        };
        let touches = RowTouches::main_row(table, row);
        let gate = self.write_gate();
        let registry = freeze.registry_for_write();
        let mut copies = WriteBatch::default();
        let mut flush = |chunk: WriteBatch| gate.write_without_freeze(&self.db, chunk, None);
        let work = freeze.stage(&registry, &self.db, &touches, &mut copies, &mut flush)?;
        let applied = if copies.is_empty() {
            Ok(())
        } else {
            gate.write_without_freeze(&self.db, copies, None)
        };
        drop(work);
        applied
    }

    /// Sprinter 6bc4a4040eb7 (B4) with spec R5.2: how many live branches of main take a
    /// copy of each row of `table` that main rewrites, from the rewrite's own write
    /// (`WriteGate::write_frozen`): the ones that read the table through main. An upper
    /// bound: a child already holding a row takes no copy of it. A rewrite's cap counts
    /// them (`StorageEngine::stage_added_column_rows`).
    pub(crate) fn branches_copying_main_rows(&self, table: &str) -> Result<u64> {
        let Some(freeze) = self.write_gate().active_freeze() else {
            return Ok(0);
        };
        let (children, _) = freeze.children_and_chain(MAIN_LEVEL);
        let mut copying = 0u64;
        'children: for child in &children {
            for level in &child.path {
                let hides = self
                    .branch_table_record(*level, table)?
                    .is_some_and(|record| record.isolated || matches!(record.state, BranchTableState::Dropped));
                if hides {
                    continue 'children;
                }
            }
            copying = copying.saturating_add(1);
        }
        Ok(copying)
    }

    /// Main's definition of `table` as a copy takes it. During the log's replay at
    /// open main's index registry is not built yet, so the rules come from what the
    /// catalog records.
    fn main_definition_for_freeze(&self, table: &str) -> Result<Option<(BranchTableDefinition, u64)>> {
        let Some((mut definition, fingerprint)) = self.main_table_definition(table)? else {
            return Ok(None);
        };
        if !self.in_open_recovery() {
            return Ok(Some((definition, fingerprint)));
        }
        definition.unique_rules =
            super::branch_catalog::derive_unique_rules(table, &definition.schema, &definition.constraints);
        let fingerprint = super::branch_catalog::definition_fingerprint(
            &definition.schema,
            &definition.constraints,
            &definition.unique_rules,
        )?;
        Ok(Some((definition, fingerprint)))
    }

    /// Does some level of `path` hold a record of `table`? Read from the store: this
    /// is the check a parent's copy turns on.
    fn path_holds_record(&self, path: &[u64], table: &str) -> Result<bool> {
        for level in path {
            let present = self
                .db
                .get_pinned(branch_table_key(*level, table))
                .map_err(|e| Error::storage(format!("Get failed: {e}")))?
                .is_some();
            if present {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Write `record` as the copy `child` keeps of `table`, unless it (or a merged
    /// level on its way) has a record by now. With it, in its batch, `missing`: main's
    /// missing values for the table (spec R5.2), by column name, which the child's
    /// narrow rows read from then on (`BranchView::row_shape`). Main's own may move or
    /// go: a `DROP COLUMN` shifts their positions, a `RENAME TO` or `DROP TABLE` takes
    /// them away, and the rows the child still reads are the ones from before.
    fn write_frozen_record(
        &self,
        freeze: &BranchFreeze,
        child: &LiveChild,
        table: &str,
        record: &BranchTableRecord,
        missing: &[(String, Value)],
    ) -> Result<()> {
        {
            let _definition = freeze.lock_definition(table);
            if self.path_holds_record(&child.path, table)? {
                return Ok(());
            }
            let value = record.encode()?;
            let sealed = tde::seal(self.key_manager_ref(), &value)?;
            let mut batch = WriteBatch::default();
            batch.put(branch_table_key(child.id, table), sealed.as_ref());
            let missing_key = super::branch_catalog::branch_missing_values_key(child.id, table);
            if missing.is_empty() {
                batch.delete(&missing_key);
            } else {
                let encoded = bincode::serialize(missing)
                    .map_err(|e| Error::storage(format!("Failed to serialize missing values: {e}")))?;
                batch.put(&missing_key, tde::seal(self.key_manager_ref(), &encoded)?.as_ref());
            }
            // Spec R5.3: a write of its own inside a schema statement on main, by
            // design (see `freeze_definitions_for_children`): the copy is what the
            // child already reads through main, so it changes nothing any reader sees,
            // and a statement that fails or a crash after it leaves it true.
            let _side = super::ddl_batch::allow_side_write("a branch's frozen copy of the definition it reads");
            self.write_batch_through_gate(batch)?;
        }
        self.after_branch_catalog_write(child.id, table)
    }

    /// Sprinter 6bc4a4040eb7 (B4): a schema statement on MAIN is about to change
    /// `tables`. Each live branch that reads them through main keeps the definition
    /// it sees (or a tombstone, for a table it does not see). Main stays what it reads
    /// the rows from: the ones main rewrites or drops are frozen as main's writes
    /// reach the gate.
    ///
    /// Not in the gate, and not in the DDL's batch: main's index registry, which
    /// holds a definition's unique rules, is already torn down when a `DROP TABLE`'s
    /// batch gets there. Not needing the batch is what makes that possible: the copy
    /// equals what the branch sees, so a DDL that fails or a crash afterwards leaves
    /// it true.
    ///
    /// The caller entered [`BranchFreeze::enter_schema_statement`] first
    /// (`enter_main_statement`, `enter_main_ddl`) and a new branch of main holds
    /// statements out across its registration, so either this sees the branch, or the
    /// statement finished before the branch existed.
    pub(crate) fn freeze_definitions_for_children(&self, tables: &[&str]) -> Result<()> {
        let Some(freeze) = self.write_gate().active_freeze() else {
            return Ok(());
        };
        let (children, _) = freeze.children_and_chain(MAIN_LEVEL);
        for table in tables {
            if crate::sql::temp_tables::is_temp_key(table) {
                continue;
            }
            let mut record: Option<(BranchTableRecord, Vec<(String, Value)>)> = None;
            for child in &children {
                if self.path_holds_record(&child.path, table)? {
                    continue;
                }
                let (copy, missing) = match &record {
                    Some(taken) => taken.clone(),
                    None => {
                        let definition = self.main_definition_for_freeze(table)?;
                        // Spec R5.2: what main's narrow rows of the table read, now,
                        // before the statement changes it; sorted, so every child's
                        // copy is the same bytes.
                        let mut missing: Vec<(String, Value)> = if definition.is_some() {
                            self.stored_missing_values(table)?.into_iter().collect()
                        } else {
                            Vec::new()
                        };
                        missing.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                        (frozen_record(definition, false), missing)
                    }
                };
                self.write_frozen_record(freeze, child, table, &copy, &missing)?;
                record = Some((copy, missing));
            }
        }
        Ok(())
    }

    /// Sprinter 6bc4a4040eb7 (B4): level `level`, a branch, is about to change the
    /// definition of `table`, and from then on it reads none of the table from above
    /// (a `DROP TABLE` or an `ADD COLUMN` make it isolated). Each live branch that
    /// reads the table through it first becomes self-contained for it: every row it
    /// sees goes into its own overlay, marked as a copy, in chunks (`materialize`),
    /// and then the definition, marked too and isolated, so it reads neither `level`
    /// nor anything above it. O(rows it sees), in bounded memory. A crash on the way
    /// leaves marked copies and no record, which is what it read before. One the
    /// table is not visible on gets a tombstone: `level` is creating it.
    ///
    /// The child is held alone for that (its own running statements finish first, and
    /// one of them holding staged writes of the table refuses this 55006), like a
    /// schema statement of its own. Its children read through it and see the same.
    pub(crate) fn freeze_table_into_children(&self, level: u64, table: &str) -> Result<()> {
        let Some(freeze) = self.write_gate().active_freeze() else {
            return Ok(());
        };
        let (children, chain) = freeze.children_and_chain(level);
        for child in &children {
            if self.path_holds_record(&child.path, table)? {
                continue;
            }
            let Some(definition) = self.chain_table_definition(&chain, table)? else {
                self.write_frozen_record(freeze, child, table, &frozen_record(None, true), &[])?;
                continue;
            };
            let manager = self
                .branch_manager()
                .ok_or_else(|| Error::storage("BranchManager not initialized"))?;
            let name = manager
                .get_branch_name(child.id)
                .unwrap_or_else(|| child.id.to_string());
            let _alone = self.branch_drains().exclusive(child.id, &name, BRANCH_DRAIN_TIMEOUT)?;
            if branch_view::pending_writers(self.instance_id(), child.id, table) > 0 {
                return Err(Error::branch_table_busy(table, &name));
            }
            // Spec R5.1: the child's writer lock on the table too, in the order a schema
            // statement of the child's takes it (`branch_ddl_preflight`): its rows are
            // rewritten here. What is left to wait for is a transaction of the child that
            // holds the table with nothing staged.
            let _child_lock = self.lock_tables_exclusive(child.id, &[table], "a parent branch's schema statement")?;
            // It may have defined the table while this waited.
            if self.path_holds_record(&child.path, table)? {
                continue;
            }
            let schema = definition.0.schema.clone();
            let record = frozen_record(Some(definition), true);
            let (_, child_chain) = freeze.children_and_chain(child.id);
            let view = BranchView::for_chain(self, name, child_chain)?;
            view.materialize_with(table, &schema, &record, &|| Ok(Vec::new()))?;
        }
        Ok(())
    }

    /// Sprinter 6bc4a4040eb7 (B4): `level` is a branch being created from main with an
    /// anchor in the past, and every later write of main freezes for it already.
    /// Write the difference between the anchor and now into its overlay, table by
    /// table, marked as copies: a row whose value at the anchor differs from its
    /// current one gets the anchor's; a row created since, a tombstone; a row deleted
    /// since (it has a delete event) the anchor's value. A row deleted before delete
    /// events existed has none, and stays deleted before any anchor.
    ///
    /// A table's definition is its current one, as `AS OF` queries are shaped. That
    /// needs the row as it was to be readable under that definition, and a version
    /// written before a `DROP COLUMN`, `RENAME COLUMN`, `RENAME TO`, `SET STORAGE` or
    /// the `REFRESH` of a materialized view is not (`TableShapeChange`): so an anchor
    /// before one is refused, for the whole store, and past one a row whose version at
    /// the anchor is older than it, and which has changed since, is too. One that has
    /// not changed since is what it is now. No record exists for such a statement of an
    /// earlier release: it reads as it always did.
    ///
    /// Memory: one chunk of copies ([`branch_view::materialize_chunk_rows`] rows or
    /// [`branch_view::MATERIALIZE_CHUNK_BYTES`] bytes), plus the ids of the deleted
    /// rows of the table in hand. The branch does not exist until its creation batch,
    /// so a crash on the way leaves keys of an id nothing names and no session can
    /// bind, which the next open reclaims.
    pub(crate) fn materialize_branch_anchor(&self, level: u64, name: &str, anchor: u64) -> Result<()> {
        // Spec R5.1: rows of a level no session can bind until its creation batch, so
        // no statement of it holds, or can take, a lock on its tables.
        let _unlocked = super::table_locks::allow_unlocked_row_writes("a branch's anchor, before it exists");
        let snapshots = self.snapshot_manager();
        let catalog = Catalog::new(self);
        let mut tables = Vec::new();
        for table in catalog.list_main_tables()? {
            if crate::sql::temp_tables::is_temp_key(&table) {
                continue;
            }
            let shape = catalog.main_table_shape_change(&table)?;
            if let Some(change) = shape.as_ref().filter(|change| anchor < change.changed_at) {
                return Err(Error::branch_anchor_before_shape_change(name, &change.describe(&table)));
            }
            tables.push((table, shape));
        }
        let mut chunk = AnchorChunk::default();
        for (table, shape) in tables {
            let at = AnchorTable {
                level,
                name,
                table: &table,
                anchor,
                shape: shape.as_ref(),
            };
            let deleted = snapshots.rows_with_delete_event(&table)?;
            let prefix = format!("data:{table}:");
            let rows = self.db.iterator_opt(
                rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
                super::time_travel::bounded_read_options(prefix.as_bytes()),
            );
            for item in rows {
                let (key, raw) = item.map_err(|e| Error::storage(format!("Iterator error: {e}")))?;
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                let Some(row) = StorageEngine::parse_row_id_after_prefix(&key, prefix.len()) else {
                    continue;
                };
                let current = tde::open_ref(self.key_manager_ref(), &key, &raw)?;
                self.stage_anchor_row(&at, row, Some(current.as_ref()), &mut chunk)?;
            }
            for row in deleted {
                if self
                    .db
                    .get_pinned(StorageEngine::build_data_key(&table, row))
                    .map_err(|e| Error::storage(format!("Get failed: {e}")))?
                    .is_none()
                {
                    self.stage_anchor_row(&at, row, None, &mut chunk)?;
                }
            }
        }
        self.flush_anchor_chunk(&mut chunk)
    }

    /// Can row `row`'s version at the anchor be read under the table's definition now?
    /// `false`: it is older than the table's last change of shape, and the row has not
    /// changed since the anchor, so the row now is what it was. An error: it has.
    fn anchor_version_is_current_shape(&self, at: &AnchorTable<'_>, row: u64) -> Result<bool> {
        let Some(shape) = at.shape else {
            return Ok(true);
        };
        let (at_or_before, changed_since) = self.snapshot_manager().row_events_around(at.table, row, at.anchor)?;
        let unreadable = match at_or_before {
            // Written for another shape. A delete has no image to read. Spec R5.2's
            // pre-image of a rewrite is the row as the rewrite found it: at the anchor
            // only when the rewrite came after it, so after the shape change too (an
            // anchor before that is refused above), and its shape is the current one.
            Some((version, is_delete)) => {
                !is_delete && version != super::time_travel::REWRITE_PRE_IMAGE_TS && version < shape.changed_at
            }
            // Nothing at the anchor under this name, and something since: a row
            // created since, or one a rename left its history behind for. Its id tells.
            None => changed_since && shape.renamed_through_row.is_some_and(|through| row <= through),
        };
        if !unreadable {
            return Ok(true);
        }
        if changed_since {
            return Err(Error::branch_anchor_before_shape_change(
                at.name,
                &shape.describe(at.table),
            ));
        }
        Ok(false)
    }

    fn stage_anchor_row(
        &self,
        at: &AnchorTable<'_>,
        row: u64,
        current: Option<&[u8]>,
        chunk: &mut AnchorChunk,
    ) -> Result<()> {
        let (level, table) = (at.level, at.table);
        if !self.anchor_version_is_current_shape(at, row)? {
            return Ok(());
        }
        let at_anchor = self.snapshot_manager().read_at_snapshot(table, row, at.anchor)?;
        let row_key = branch_row_key(level, table, row);
        let tombstone_key = branch_tombstone_key(level, table, row);
        // In this order. A main write after the read of `current` froze `current`,
        // which is the anchor's value if they are equal. One before it may have frozen
        // a value that is neither, and only the level's entry tells.
        let same = at_anchor.as_deref() == current;
        if same {
            let held = self
                .db
                .get_pinned(&row_key)
                .map_err(|e| Error::storage(format!("Get failed: {e}")))?
                .is_some()
                || self
                    .db
                    .get_pinned(&tombstone_key)
                    .map_err(|e| Error::storage(format!("Get failed: {e}")))?
                    .is_some();
            if !held {
                return Ok(());
            }
        }
        match at_anchor {
            Some(plain) => {
                let sealed = tde::seal_row_value(self.key_manager_ref(), &row_key, &plain)?;
                chunk.batch.put(&row_key, sealed.as_ref());
                chunk.batch.delete(&tombstone_key);
            }
            None => {
                chunk.batch.delete(&row_key);
                chunk.batch.put(&tombstone_key, b"");
            }
        }
        chunk.batch.put(branch_frozen_key(level, table, row), b"");
        chunk.rows += 1;
        if chunk.rows >= branch_view::materialize_chunk_rows()
            || chunk.batch.size_in_bytes() >= branch_view::MATERIALIZE_CHUNK_BYTES
        {
            self.flush_anchor_chunk(chunk)?;
            self.merge_failpoint_tick(super::branch_catalog::MergeFailSite::Materialize)?;
        }
        Ok(())
    }

    fn flush_anchor_chunk(&self, chunk: &mut AnchorChunk) -> Result<()> {
        if chunk.rows == 0 {
            return Ok(());
        }
        chunk.rows = 0;
        self.write_batch_through_gate(std::mem::take(&mut chunk.batch))
    }

    /// A merge of `id` finished. Nothing can bind it, and nothing is left to finish
    /// it: its keys go, unless a live branch still reads through it, and then they go
    /// when the last of those does.
    pub(crate) fn retire_merged_branch(&self, id: u64) {
        if let Some(freeze) = self.branch_freeze() {
            freeze.retire(id);
        }
        if let Some(manager) = self.branch_manager() {
            manager.reclaim_merged_chain(Some(id));
        }
    }

    /// At a writable open, after `finish_pending_branch_merges`: finish the reclaim
    /// of every level that is gone. It never fails the open: what is left is retried
    /// at the next one. Returns the keys it deleted.
    pub fn reclaim_dead_branches(&self) -> u64 {
        if !self.engine_may_write() || !self.write_gate().is_open() {
            return 0;
        }
        let Some(manager) = self.branch_manager() else {
            return 0;
        };
        match manager.reclaim_dead_levels() {
            Ok(deleted) => {
                if deleted > 0 {
                    tracing::info!("Reclaimed {} keys of branches that are gone", deleted);
                }
                deleted
            }
            Err(e) => {
                tracing::warn!("Could not finish the reclaim of dropped branches: {}", e);
                0
            }
        }
    }
}

/// One table of a branch's anchor being written.
struct AnchorTable<'a> {
    level: u64,
    /// The branch's name, for a refusal.
    name: &'a str,
    table: &'a str,
    anchor: u64,
    shape: Option<&'a TableShapeChange>,
}

/// The batch of a branch's anchor being written.
#[derive(Default)]
struct AnchorChunk {
    batch: WriteBatch,
    rows: usize,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn freeze() -> BranchFreeze {
        BranchFreeze::new(
            u64::MAX,
            None,
            Arc::new(DictionaryManager::new()),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn a_batchs_row_keys_are_told_apart_and_deduplicated() {
        let mut batch = WriteBatch::default();
        batch.put(b"data:t:7", b"x");
        batch.delete(b"bdata:12:t:7");
        batch.put(b"bdel:12:t:7", b"");
        batch.put(b"bdata:12:a:b:9", b"x");
        batch.put(b"v:t:7:42", b"x");
        batch.put(b"meta:table:t", b"x");
        batch.put(b"bmeta:12:table:t", b"x");
        batch.put(b"bkey:12:74:70:ab:7", b"");
        batch.delete(b"bdata:\x00\x00\x00\x00\x00\x00\x00\x01:t");
        let touches = BranchFreeze::row_touches(&batch);
        let found: Vec<(u64, String, Vec<u64>)> = touches
            .groups
            .iter()
            .map(|((level, table), rows)| (*level, table.clone(), rows.rows.iter().copied().collect()))
            .collect();
        assert_eq!(
            found,
            vec![
                (MAIN_LEVEL, "t".to_string(), vec![7]),
                (12, "a:b".to_string(), vec![9]),
                (12, "t".to_string(), vec![7]),
            ]
        );
    }

    #[test]
    fn a_marked_row_of_a_batch_is_a_copy() {
        let mut batch = WriteBatch::default();
        batch.put(b"bdata:3:t:1", b"x");
        batch.put(b"bfrz:3:t:1", b"");
        batch.put(b"bdata:3:t:2", b"x");
        batch.delete(b"bfrz:3:t:2");
        let touches = BranchFreeze::row_touches(&batch);
        let group = touches.groups.get(&(3, "t".to_string())).unwrap();
        assert!(group.copies.contains(&1));
        assert!(!group.copies.contains(&2), "clearing a marker is not a copy");
        assert_eq!(group.rows.len(), 2);
    }

    /// Exact decimal ids: 1 is not 12, and a binary key is no level at all.
    #[test]
    fn a_level_is_its_decimal_id() {
        let level = |text: &str| {
            level_and_rest(text.as_bytes()).map(|(id, rest)| (id, String::from_utf8_lossy(rest).into_owned()))
        };
        let row = |text: &str| table_and_row(text.as_bytes()).map(|(table, id)| (table.to_string(), id));
        assert_eq!(level("12:t:1"), Some((12, "t:1".to_string())));
        assert_eq!(level("1:t:1"), Some((1, "t:1".to_string())));
        assert_eq!(level("\u{0}\u{0}:t"), None);
        assert_eq!(level(":t"), None);
        assert_eq!(row("t:x"), None);
        assert_eq!(row("a:b:3"), Some(("a:b".to_string(), 3)));
    }

    fn stage_rows_of(freeze: &BranchFreeze, db: &DB, table: &str, rows: u64) -> bool {
        let mut batch = WriteBatch::default();
        for row in 1..=rows {
            batch.put(format!("data:{table}:{row}"), b"x");
        }
        let touches = BranchFreeze::row_touches(&batch);
        let registry = freeze.registry_for_write();
        let mut flush = |_: WriteBatch| -> Result<()> { Ok(()) };
        let work = freeze.stage(&registry, db, &touches, &mut batch, &mut flush).unwrap();
        // While a batch holds its locks: can a one-row writer of another table run?
        // Its table's lock, shared, and its row's stripe.
        let other = table_lock(fnv1a(b"another_table"));
        let free = freeze.table_locks[other].try_read().is_some()
            && freeze.stripes.iter().all(|stripe| stripe.try_lock().is_some());
        // And what does this batch exclude of its own table?
        let own = table_lock(fnv1a(table.as_bytes()));
        let alone = freeze.table_locks[own].try_read().is_none();
        let shared = freeze.table_locks[own].try_write().is_none();
        assert!(shared, "a writer of the table is excluded either way from going alone");
        drop(work);
        assert!(freeze.table_locks[own].try_write().is_some(), "released");
        assert_eq!(alone, rows > BIG_BATCH_ROWS as u64, "{rows} rows");
        free
    }

    /// The review's N4: a big batch excludes its own tables, and a writer of another
    /// table goes on. Before, it held every row stripe it touched: nearly all of them.
    #[test]
    fn a_big_batch_excludes_the_writers_of_its_own_tables_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = DB::open_default(dir.path()).unwrap();
        let freeze = freeze();
        freeze.rebuild([(2, MAIN_LEVEL, true)]);
        assert!(
            stage_rows_of(&freeze, &db, "big", 5_000),
            "a writer of another table waited for a big batch"
        );
        assert!(
            !stage_rows_of(&freeze, &db, "small", 3),
            "a small batch holds the stripes of its rows"
        );
    }

    /// A running statement keeps a fork out; a fork registering keeps new ones out.
    #[test]
    fn a_schema_statement_and_a_fork_of_main_take_turns() {
        let freeze = freeze();
        let short = Duration::from_millis(5);
        assert!(freeze.enter_schema_statement(short));
        assert!(
            freeze.enter_schema_statement(short),
            "statements do not exclude each other"
        );
        assert!(freeze.hold_schema_statements(short).is_none(), "two are running");
        // It gave up, and holds nothing.
        assert!(freeze.enter_schema_statement(short));
        freeze.leave_schema_statement();
        freeze.leave_schema_statement();
        freeze.leave_schema_statement();
        let held = freeze.hold_schema_statements(short).expect("none is running");
        assert!(!freeze.enter_schema_statement(short), "held out");
        assert!(freeze.hold_schema_statements(short).is_none(), "one at a time");
        drop(held);
        assert!(freeze.enter_schema_statement(short));
        freeze.leave_schema_statement();
    }

    /// main → b → c with c merged and d under it: d is a child of b through c, and
    /// c is gone from the registry when d is, b's other child unaffected.
    #[test]
    fn the_registry_walks_through_merged_levels() {
        let freeze = freeze();
        freeze.rebuild([(2, MAIN_LEVEL, true), (3, 2, false), (4, 3, true), (5, 2, true)]);
        let (of_b, _) = freeze.children_and_chain(2);
        let mut ids: Vec<(u64, Vec<u64>)> = of_b.iter().map(|kid| (kid.id, kid.path.clone())).collect();
        ids.sort();
        assert_eq!(ids, vec![(4, vec![3, 4]), (5, vec![5])]);
        let (of_main, _) = freeze.children_and_chain(MAIN_LEVEL);
        assert_eq!(of_main.len(), 1);
        assert_eq!(of_main[0].id, 2);
        let (_, chain) = freeze.children_and_chain(4);
        assert_eq!(chain, vec![2, 3, 4]);
        assert!(freeze.registry.read().chain_of(3).is_some(), "d reads through it");
        freeze.remove(4);
        let registry = freeze.registry.read();
        assert!(registry.chain_of(3).is_none(), "merged, and nothing reads through it");
        assert!(registry.knows(5));
    }

    #[test]
    fn the_flag_follows_the_live_levels() {
        let freeze = freeze();
        assert_eq!(freeze.register(2, MAIN_LEVEL, Duration::from_secs(1)), Some(false));
        assert!(freeze.active.load(Ordering::SeqCst));
        assert_eq!(freeze.register(3, 2, Duration::from_secs(1)), Some(true));
        freeze.remove(3);
        assert!(freeze.active.load(Ordering::SeqCst));
        freeze.retire(2);
        assert!(
            !freeze.active.load(Ordering::SeqCst),
            "a merged branch is written by nobody"
        );
    }

    #[test]
    fn a_reader_is_told_about_a_copy_on_its_way() {
        let freeze = freeze();
        assert!(!freeze.in_flight(&[2, 3], "t"));
        freeze.mark_in_flight(3, "t");
        assert!(freeze.in_flight(&[2, 3], "t"));
        assert!(!freeze.in_flight(&[2], "t"));
        assert!(!freeze.in_flight(&[3], "u"));
        freeze.unmark_in_flight(3, "t");
        assert!(!freeze.in_flight(&[2, 3], "t"));
    }
}
