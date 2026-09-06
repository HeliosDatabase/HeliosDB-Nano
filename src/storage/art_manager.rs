//! ART Index Manager
//!
//! Manages the lifecycle of ART indexes with automatic creation for:
//! - Primary Keys (PKs)
//! - Foreign Keys (FKs)
//! - Unique Constraints
//!
//! The manager handles:
//! - Automatic index creation during DDL operations
//! - Constraint enforcement (uniqueness, referential integrity)
//! - Index maintenance on DML operations (INSERT/UPDATE/DELETE)
//! - Index persistence and recovery

use super::art_index::{AdaptiveRadixTree, ArtIndexError, ArtIndexStats, ArtIndexType, ArtResult};
use super::art_node::RowId;
use super::index_snapshot::ArtIndexSnapshot;
use crate::{DataType, Schema, Tuple, Value};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard};

/// Shared handle to a single ART index tree.
///
/// Callers must keep the read/write lock scope as tight as possible and must
/// follow the locking rules documented on [`ArtIndexManager`].
pub type SharedArtIndex = Arc<RwLock<AdaptiveRadixTree>>;

/// A registered index: per-registration metadata plus the shared tree handle.
///
/// The metadata mirrors the immutable identity fields inside the tree
/// (`table`, `columns`, `index_type`) so hot paths can answer "which indexes
/// belong to this table?" without taking any tree lock. The metadata is only
/// mutated while holding the global `indexes` WRITE lock (table rename).
#[derive(Debug, Clone)]
struct IndexEntry {
    /// Table this index belongs to (mirrors `tree.table()`).
    table: String,
    /// Indexed columns (mirrors `tree.columns()`).
    columns: Vec<String>,
    /// Index kind (mirrors `tree.index_type()`).
    index_type: ArtIndexType,
    /// The actual tree, individually locked.
    tree: SharedArtIndex,
}

impl IndexEntry {
    fn new(tree: AdaptiveRadixTree) -> Self {
        Self {
            table: tree.table().to_string(),
            columns: tree.columns().to_vec(),
            index_type: tree.index_type(),
            tree: Arc::new(RwLock::new(tree)),
        }
    }
}

/// Whether the row whose index entries are being maintained is ALREADY STORED
/// at the moment an index refuses its key.
///
/// This one fact decides what a refusal is allowed to do to the ART, and it is
/// a property of the CALLER, not of the row: the same funnels are reached both
/// from a path that is about to unwind the row and from paths that have already
/// written it (or will write it whatever the ART answers). Guessing it wrong in
/// either direction corrupts something:
///
/// * [`RowState::NotStored`] — the caller propagates the refusal and the row
///   never becomes a tuple. Every entry it wrote before the refusal is a
///   PHANTOM: nothing will ever delete it (there is no row whose DELETE would),
///   the next legitimate writer of that value is rejected as a duplicate of a
///   row that does not exist, and a lookup that finds it resolves to a row id
///   holding nothing. So the row is maintained ALL-OR-NOTHING.
/// * [`RowState::Stored`] — the row is durable, or the caller stores it
///   regardless of what the ART returns. Taking its entries back would strip a
///   STORED row of its PRIMARY KEY and of every other unique entry it
///   legitimately owns: a full scan would still count the row while
///   `WHERE pk = …` could not find it, and its primary key would be FREE for a
///   second row to claim. So nothing is ever taken back — the funnel keeps
///   maintaining the row's other indexes and reports the refusal, which means a
///   duplicate is now stored and must be surfaced, never swallowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    /// The row does not exist yet, and the caller unwinds it on a refusal.
    NotStored,
    /// The row is already stored, or will be stored whatever the ART returns.
    Stored,
}

/// Metadata about a foreign key constraint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKeyInfo {
    /// Name of the foreign key constraint
    pub name: String,
    /// Source table
    pub table: String,
    /// Source columns
    pub columns: Vec<String>,
    /// Referenced table
    pub ref_table: String,
    /// Referenced columns
    pub ref_columns: Vec<String>,
    /// Index name for this FK
    pub index_name: String,
}

/// A PK/UNIQUE index that already holds the key a write proposes, and the row
/// that holds it. Produced by [`ArtIndexManager::find_unique_conflict`].
#[derive(Debug, Clone)]
pub struct UniqueConflict {
    /// Name of the PK/UNIQUE index that reported the conflict.
    pub index_name: String,
    /// The indexed columns, in index order.
    pub columns: Vec<String>,
    /// The row_id of the conflicting (existing) row.
    pub row_id: RowId,
    /// Whether the conflict is on the PRIMARY KEY rather than a UNIQUE index.
    pub is_primary_key: bool,
}

/// Statistics for the ART manager
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArtManagerStats {
    /// Total number of indexes
    pub total_indexes: u64,
    /// Number of PK indexes
    pub pk_indexes: u64,
    /// Number of FK indexes
    pub fk_indexes: u64,
    /// Number of UNIQUE indexes
    pub unique_indexes: u64,
    /// Number of manual indexes
    pub manual_indexes: u64,
    /// Total constraint checks performed
    pub constraint_checks: u64,
    /// Number of constraint violations caught
    pub violations_caught: u64,
    /// Number of index renames performed
    pub index_renames: u64,
}

#[derive(Debug, Default)]
struct AtomicArtManagerStats {
    total_indexes: AtomicU64,
    pk_indexes: AtomicU64,
    fk_indexes: AtomicU64,
    unique_indexes: AtomicU64,
    manual_indexes: AtomicU64,
    constraint_checks: AtomicU64,
    violations_caught: AtomicU64,
    index_renames: AtomicU64,
}

impl AtomicArtManagerStats {
    fn snapshot(&self) -> ArtManagerStats {
        ArtManagerStats {
            total_indexes: self.total_indexes.load(Ordering::Relaxed),
            pk_indexes: self.pk_indexes.load(Ordering::Relaxed),
            fk_indexes: self.fk_indexes.load(Ordering::Relaxed),
            unique_indexes: self.unique_indexes.load(Ordering::Relaxed),
            manual_indexes: self.manual_indexes.load(Ordering::Relaxed),
            constraint_checks: self.constraint_checks.load(Ordering::Relaxed),
            violations_caught: self.violations_caught.load(Ordering::Relaxed),
            index_renames: self.index_renames.load(Ordering::Relaxed),
        }
    }

    fn add_index(&self, index_type: ArtIndexType) {
        self.total_indexes.fetch_add(1, Ordering::Relaxed);
        match index_type {
            ArtIndexType::PrimaryKey => {
                self.pk_indexes.fetch_add(1, Ordering::Relaxed);
            }
            ArtIndexType::ForeignKey => {
                self.fk_indexes.fetch_add(1, Ordering::Relaxed);
            }
            ArtIndexType::Unique => {
                self.unique_indexes.fetch_add(1, Ordering::Relaxed);
            }
            ArtIndexType::Manual => {
                self.manual_indexes.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn remove_index(&self, index_type: ArtIndexType) {
        self.total_indexes.fetch_sub(1, Ordering::Relaxed);
        match index_type {
            ArtIndexType::PrimaryKey => {
                self.pk_indexes.fetch_sub(1, Ordering::Relaxed);
            }
            ArtIndexType::ForeignKey => {
                self.fk_indexes.fetch_sub(1, Ordering::Relaxed);
            }
            ArtIndexType::Unique => {
                self.unique_indexes.fetch_sub(1, Ordering::Relaxed);
            }
            ArtIndexType::Manual => {
                self.manual_indexes.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

/// ART Index Manager
///
/// Thread-safe manager for all ART indexes in the database.
///
/// # Locking rules (deadlock safety)
///
/// Each index tree carries its own `RwLock` (see [`SharedArtIndex`]) so that
/// writers on one table no longer block readers/writers on unrelated tables.
///
/// 1. The global `indexes` map lock is taken as READ on all DML/lookup paths
///    (concurrent across tables). It is taken as WRITE only for registry
///    changes: create, drop, and rename.
/// 2. Lock order is strictly `global map lock -> (at most one) tree lock`.
///    A tree lock may be acquired while holding the global lock, but the
///    global lock must NEVER be acquired while holding a tree lock.
/// 3. INVARIANT: never hold two tree locks simultaneously. Iteration loops
///    (`on_insert*`, `on_delete*`, constraint checks, `clear_table_indexes`)
///    lock one tree, release it, then lock the next. In particular, the FK
///    existence check (`check_fk_constraints` / `unique_key_exists` reading
///    another table's PK index) acquires the referenced tree's lock only
///    while holding no other tree lock. Because no thread ever waits on a
///    tree lock while holding another tree lock, no lock-cycle (deadlock)
///    can form regardless of table/FK topology.
/// 4. The name maps (`pk_indexes`, `fk_indexes`, `fk_info`, `unique_indexes`,
///    `table_indexes`) are leaf locks: clone the names you need and release
///    them before taking the global map lock or any tree lock. In particular
///    `table_indexes` is NEVER held while `indexes` (or a tree) is held, so it
///    adds no new lock rank — a writer must not hold `indexes` and
///    `table_indexes` at once (register/drop/rename take each in its own
///    block), which is what keeps the leaf discipline cycle-free.
/// 5. Per-table filtering uses the metadata cached in [`IndexEntry`]
///    (no tree lock needed). The DML mutation loops (`on_insert*`,
///    `on_delete*`) resolve a table's complete index set from `table_indexes`
///    (leaf lock: clone the names, release, then look each entry up in
///    `indexes`) — an O(own indexes) lookup that DOES cover Manual (plain
///    secondary) indexes, unlike the partial `pk_indexes`/`fk_indexes`/
///    `unique_indexes` maps. DDL / snapshot scans (`drop_table_indexes`,
///    `rename_table_indexes`, `export_table_snapshot`, `list_table_indexes`)
///    still filter the entry map by `entry.table`.
#[derive(Debug)]
pub struct ArtIndexManager {
    /// All indexes by name
    indexes: RwLock<HashMap<String, IndexEntry>>,
    /// Primary key index name by table
    pk_indexes: RwLock<HashMap<String, String>>,
    /// Foreign key indexes by table (table -> list of FK index names)
    fk_indexes: RwLock<HashMap<String, Vec<String>>>,
    /// Foreign key metadata by index name
    fk_info: RwLock<HashMap<String, ForeignKeyInfo>>,
    /// Unique constraint indexes by table (table -> list of unique index names)
    unique_indexes: RwLock<HashMap<String, Vec<String>>>,
    /// W3.4 §3.2: complete per-table index name list — PK, FK, Unique, AND
    /// Manual. The DML mutation loops resolve a table's own indexes here in
    /// O(own indexes) instead of scanning the global `indexes` map, whose cost
    /// is O(all registered indexes system-wide) (the "many-table scaling
    /// cliff"). It is a DERIVED index of `indexes`, kept in lock-step at every
    /// register/drop/rename choke point so it can never drift (asserted by the
    /// register/drop/rename/clear consistency test). TRUNCATE
    /// (`clear_table_indexes`) does NOT touch it — the registrations survive,
    /// only the trees are emptied. Leaf lock (see locking rules #4/#5).
    table_indexes: RwLock<HashMap<String, Vec<String>>>,
    /// Statistics
    stats: AtomicArtManagerStats,
    /// R4.2 durable-snapshot validity tracking. `true` means the persisted
    /// snapshot markers (if any) still describe the current in-memory state.
    /// The FIRST mutation after a checkpoint flips it and fires
    /// `snapshot_invalidation_hook` exactly once, which durably deletes the
    /// markers — so a crash can never leave a stale snapshot trusted.
    /// Hot-path cost: one relaxed atomic load per index mutation.
    snapshot_clean: AtomicBool,
    /// Engine-wired callback that deletes the ART snapshot markers in RocksDB.
    snapshot_invalidation_hook: RwLock<Option<InvalidationHook>>,
}

/// Opaque wrapper so the manager can keep `#[derive(Debug)]`.
#[derive(Clone)]
pub struct InvalidationHook(pub Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for InvalidationHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InvalidationHook")
    }
}

impl Default for ArtIndexManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable on-disk tag for [`ArtIndexType`] (snapshot format).
pub(crate) fn index_type_tag(index_type: ArtIndexType) -> u8 {
    match index_type {
        ArtIndexType::PrimaryKey => 0,
        ArtIndexType::ForeignKey => 1,
        ArtIndexType::Unique => 2,
        ArtIndexType::Manual => 3,
    }
}

/// Decode a sign-flipped big-endian integer key (encoding v2 — the inverse of
/// `ArtIndexManager::encode_value_into` for `Int2/Int4/Int8`).
fn decode_int_key(key: &[u8], width: usize) -> Option<i64> {
    match width {
        2 if key.len() == 2 => Some(i64::from((u16::from_be_bytes([key[0], key[1]]) ^ 0x8000) as i16)),
        4 if key.len() == 4 => Some(i64::from(
            (u32::from_be_bytes([key[0], key[1], key[2], key[3]]) ^ 0x8000_0000) as i32,
        )),
        8 if key.len() == 8 => Some(
            (u64::from_be_bytes([key[0], key[1], key[2], key[3], key[4], key[5], key[6], key[7]])
                ^ 0x8000_0000_0000_0000) as i64,
        ),
        _ => None,
    }
}

impl ArtIndexManager {
    /// Create a new ART index manager
    pub fn new() -> Self {
        Self {
            indexes: RwLock::new(HashMap::new()),
            pk_indexes: RwLock::new(HashMap::new()),
            fk_indexes: RwLock::new(HashMap::new()),
            fk_info: RwLock::new(HashMap::new()),
            unique_indexes: RwLock::new(HashMap::new()),
            table_indexes: RwLock::new(HashMap::new()),
            stats: AtomicArtManagerStats::default(),
            // Armed at startup: markers from a previous clean shutdown must
            // be invalidated by the first mutation of this process.
            snapshot_clean: AtomicBool::new(true),
            snapshot_invalidation_hook: RwLock::new(None),
        }
    }

    // =========================================================================
    // R4.2: DURABLE SNAPSHOT SUPPORT
    // =========================================================================

    /// Wire the callback that durably deletes the persisted snapshot markers.
    /// Called once by the storage engine at open.
    pub fn set_snapshot_invalidation_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .snapshot_invalidation_hook
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(InvalidationHook(hook));
    }

    /// Re-arm the validity flag after a checkpoint wrote fresh markers.
    pub fn mark_snapshot_clean(&self) {
        self.snapshot_clean.store(true, Ordering::Release);
    }

    /// True when no index mutation happened since the last checkpoint /
    /// `mark_snapshot_clean`. The checkpoint writer uses this to detect a
    /// concurrent mutation racing the marker write.
    pub fn snapshot_is_clean(&self) -> bool {
        self.snapshot_clean.load(Ordering::Acquire)
    }

    /// Record an index mutation. The first call after a checkpoint fires the
    /// invalidation hook (durable marker delete); subsequent calls are a
    /// single relaxed atomic load.
    #[inline]
    fn note_mutation(&self) {
        if self.snapshot_clean.load(Ordering::Relaxed) && self.snapshot_clean.swap(false, Ordering::AcqRel) {
            let hook = self
                .snapshot_invalidation_hook
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(hook) = hook {
                (hook.0)();
            }
        }
    }

    /// Export every ART index registered on `table` as snapshot entries.
    /// Each tree is read-locked one at a time (manager locking rules hold).
    pub fn export_table_snapshot(&self, table: &str) -> Vec<ArtIndexSnapshot> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for (name, entry) in indexes.iter() {
            if entry.table != table {
                continue;
            }
            let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
            let mut entries: Vec<(Vec<u8>, Vec<u64>)> = Vec::new();
            for (key, row_id) in tree.iter() {
                match entries.last_mut() {
                    Some((last_key, ids)) if *last_key == key => ids.push(row_id),
                    _ => entries.push((key, vec![row_id])),
                }
            }
            out.push(ArtIndexSnapshot {
                name: name.clone(),
                table: entry.table.clone(),
                columns: entry.columns.clone(),
                index_type: index_type_tag(entry.index_type),
                entries,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Bulk-load snapshot entries into an already-registered (empty) index.
    ///
    /// `dense_int_width` carries the byte width of a single-column integer
    /// primary key so the dense-int range-count stats are restored exactly as
    /// the scan path's `on_insert` would have built them.
    pub fn load_index_entries(
        &self,
        name: &str,
        entries: &[(Vec<u8>, Vec<u64>)],
        dense_int_width: Option<usize>,
    ) -> ArtResult<usize> {
        let entry = {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            indexes
                .get(name)
                .cloned()
                .ok_or_else(|| ArtIndexError::IndexNotFound(name.to_string()))?
        };
        let mut tree = entry.tree.write().unwrap_or_else(|e| e.into_inner());
        if tree.len() != 0 {
            return Err(ArtIndexError::Internal(format!(
                "Refusing to load snapshot into non-empty index '{}' ({} entries)",
                name,
                tree.len()
            )));
        }
        let mut loaded = 0usize;
        for (key, row_ids) in entries {
            for row_id in row_ids {
                tree.insert(key, *row_id)?;
                loaded += 1;
            }
            if entry.index_type == ArtIndexType::PrimaryKey {
                if let Some(width) = dense_int_width {
                    if key.len() == width {
                        if let Some(value) = decode_int_key(key, width) {
                            tree.record_dense_int_insert(width, value);
                        }
                    }
                }
            }
        }
        Ok(loaded)
    }

    /// Generate index name for a primary key
    fn pk_index_name(table: &str) -> String {
        format!("{}_pkey", table)
    }

    /// Generate index name for a foreign key
    fn fk_index_name(table: &str, columns: &[String]) -> String {
        format!("{}_{}_fkey", table, columns.join("_"))
    }

    /// Generate index name for a unique constraint
    fn unique_index_name(table: &str, columns: &[String]) -> String {
        format!("{}_{}_key", table, columns.join("_"))
    }

    /// The name [`Self::create_unique_index`] generates when the caller does
    /// not supply one — PostgreSQL's own `{table}_{cols}_key` spelling.
    ///
    /// Public so `Catalog::create_table` can check that name for a collision
    /// BEFORE it persists anything: it fails closed on a registration failure,
    /// and an error over a half-created table is worse than a rejected CREATE.
    pub fn generated_unique_index_name(table: &str, columns: &[String]) -> String {
        Self::unique_index_name(table, columns)
    }

    /// The name [`Self::create_pk_index`] generates — `{table}_pkey`. Public
    /// for the `Catalog::create_table` unwind, which has to find the PK index
    /// it just registered in order to drop it again.
    pub fn generated_pk_index_name(table: &str) -> String {
        Self::pk_index_name(table)
    }

    /// W3.4 §3.2: record a newly-registered index in the per-table entry list.
    /// Called at every `indexes` registration choke point AFTER the entry lands
    /// in `indexes`, so the two never disagree. Leaf lock (rules #4/#5): taken
    /// alone, never while holding `indexes` or a tree lock.
    fn table_index_add(&self, table: &str, name: &str) {
        let mut table_indexes = self.table_indexes.write().unwrap_or_else(|e| e.into_inner());
        table_indexes
            .entry(table.to_string())
            .or_insert_with(Vec::new)
            .push(name.to_string());
    }

    /// W3.4 §3.2: drop an index name from the per-table entry list, removing
    /// the table key once its last index is gone (so the map stays byte-for-byte
    /// identical to a full `indexes` filter — the §3.2 consistency invariant).
    fn table_index_remove(&self, table: &str, name: &str) {
        let mut table_indexes = self.table_indexes.write().unwrap_or_else(|e| e.into_inner());
        if let Some(list) = table_indexes.get_mut(table) {
            list.retain(|n| n != name);
            if list.is_empty() {
                table_indexes.remove(table);
            }
        }
    }

    // =========================================================================
    // INDEX CREATION
    // =========================================================================

    /// Create a primary key index (auto-called on CREATE TABLE with PRIMARY KEY)
    pub fn create_pk_index(&self, table: &str, columns: &[String]) -> ArtResult<String> {
        self.note_mutation();
        let index_name = Self::pk_index_name(table);

        // Check if PK already exists for this table
        {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            if pk_indexes.contains_key(table) {
                return Err(ArtIndexError::IndexAlreadyExists(format!(
                    "Primary key already exists for table '{}'",
                    table
                )));
            }
        }

        // Create the index
        let index = AdaptiveRadixTree::new(&index_name, table, columns.to_vec(), ArtIndexType::PrimaryKey);

        // Register the index
        {
            let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());
            indexes.insert(index_name.clone(), IndexEntry::new(index));
        }

        {
            let mut pk_indexes = self.pk_indexes.write().unwrap_or_else(|e| e.into_inner());
            pk_indexes.insert(table.to_string(), index_name.clone());
        }

        self.table_index_add(table, &index_name);
        self.stats.add_index(ArtIndexType::PrimaryKey);

        Ok(index_name)
    }

    /// Create a foreign key index (auto-called on ALTER TABLE ADD FOREIGN KEY)
    pub fn create_fk_index(
        &self,
        table: &str,
        columns: &[String],
        ref_table: &str,
        ref_columns: &[String],
        constraint_name: Option<&str>,
    ) -> ArtResult<String> {
        self.note_mutation();
        let index_name = constraint_name
            .map(|n| n.to_string())
            .unwrap_or_else(|| Self::fk_index_name(table, columns));

        // Check if index already exists
        {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            if indexes.contains_key(&index_name) {
                return Err(ArtIndexError::IndexAlreadyExists(index_name));
            }
        }

        // Verify that the referenced table has a PK or unique constraint on ref_columns
        // (This would be checked during DDL execution)

        // Create the index
        let index = AdaptiveRadixTree::new(&index_name, table, columns.to_vec(), ArtIndexType::ForeignKey);

        // Create FK info
        let fk_info = ForeignKeyInfo {
            name: index_name.clone(),
            table: table.to_string(),
            columns: columns.to_vec(),
            ref_table: ref_table.to_string(),
            ref_columns: ref_columns.to_vec(),
            index_name: index_name.clone(),
        };

        // Register everything
        {
            let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());
            indexes.insert(index_name.clone(), IndexEntry::new(index));
        }

        {
            let mut fk_indexes = self.fk_indexes.write().unwrap_or_else(|e| e.into_inner());
            fk_indexes
                .entry(table.to_string())
                .or_insert_with(Vec::new)
                .push(index_name.clone());
        }

        {
            let mut fk_info_map = self.fk_info.write().unwrap_or_else(|e| e.into_inner());
            fk_info_map.insert(index_name.clone(), fk_info);
        }

        self.table_index_add(table, &index_name);
        self.stats.add_index(ArtIndexType::ForeignKey);

        Ok(index_name)
    }

    /// Create a unique constraint index (auto-called on CREATE TABLE UNIQUE or ALTER TABLE ADD UNIQUE)
    pub fn create_unique_index(
        &self,
        table: &str,
        columns: &[String],
        constraint_name: Option<&str>,
    ) -> ArtResult<String> {
        self.note_mutation();
        let index_name = constraint_name
            .map(|n| n.to_string())
            .unwrap_or_else(|| Self::unique_index_name(table, columns));

        // Check if index already exists
        {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            if indexes.contains_key(&index_name) {
                return Err(ArtIndexError::IndexAlreadyExists(index_name));
            }
        }

        // Create the index
        let index = AdaptiveRadixTree::new(&index_name, table, columns.to_vec(), ArtIndexType::Unique);

        // Register the index
        {
            let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());
            indexes.insert(index_name.clone(), IndexEntry::new(index));
        }

        {
            let mut unique_indexes = self.unique_indexes.write().unwrap_or_else(|e| e.into_inner());
            unique_indexes
                .entry(table.to_string())
                .or_insert_with(Vec::new)
                .push(index_name.clone());
        }

        self.table_index_add(table, &index_name);
        self.stats.add_index(ArtIndexType::Unique);

        Ok(index_name)
    }

    /// Create a manual index (via CREATE INDEX ... USING ART)
    pub fn create_manual_index(&self, name: &str, table: &str, columns: &[String]) -> ArtResult<String> {
        self.note_mutation();
        // Check if index already exists
        {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            if indexes.contains_key(name) {
                return Err(ArtIndexError::IndexAlreadyExists(name.to_string()));
            }
        }

        // Create the index
        let index = AdaptiveRadixTree::new(name, table, columns.to_vec(), ArtIndexType::Manual);

        // Register the index
        {
            let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());
            indexes.insert(name.to_string(), IndexEntry::new(index));
        }

        self.table_index_add(table, name);
        self.stats.add_index(ArtIndexType::Manual);

        Ok(name.to_string())
    }

    /// Populate an existing manual index from already-materialized table rows.
    ///
    /// This is intentionally scoped to one named manual index. Calling the normal
    /// table insert maintenance path would also touch PK/UNIQUE indexes and hit
    /// duplicates for rows that were already present before CREATE INDEX.
    pub fn backfill_manual_index(&self, name: &str, schema: &Schema, tuples: &[Tuple]) -> ArtResult<usize> {
        self.note_mutation();
        // Global READ is enough: the registry is not changed, only one tree.
        let entry = {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            indexes
                .get(name)
                .cloned()
                .ok_or_else(|| ArtIndexError::IndexNotFound(name.to_string()))?
        };
        if entry.index_type != ArtIndexType::Manual {
            return Err(ArtIndexError::Internal(format!(
                "Index '{}' is not a manual secondary index",
                name
            )));
        }

        let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
        let mut inserted = 0usize;
        for tuple in tuples {
            let Some(row_id) = tuple.row_id else {
                return Err(ArtIndexError::Internal(format!(
                    "Cannot backfill index '{}' from tuple without row_id",
                    name
                )));
            };
            if let Some(values) = Self::index_value_refs_from_tuple(&entry.columns, schema, tuple) {
                let key = Self::encode_key_from_values(values.iter().copied());
                index.insert(&key, row_id)?;
                inserted += 1;
            }
        }

        Ok(inserted)
    }

    /// Backfill a foreign-key index from existing rows. Mirrors
    /// `backfill_manual_index` but for `ForeignKey` indexes. Needed when a
    /// foreign key is added to a table that already holds data: `create_fk_index`
    /// registers an empty tree, and the planner may answer `WHERE fk_col = …`
    /// (and FK-column joins) from it — so without this backfill the pre-existing
    /// rows are invisible and such lookups silently return zero matches.
    pub fn backfill_fk_index(&self, name: &str, schema: &Schema, tuples: &[Tuple]) -> ArtResult<usize> {
        self.note_mutation();
        let entry = {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            indexes
                .get(name)
                .cloned()
                .ok_or_else(|| ArtIndexError::IndexNotFound(name.to_string()))?
        };
        if entry.index_type != ArtIndexType::ForeignKey {
            return Err(ArtIndexError::Internal(format!(
                "Index '{}' is not a foreign-key index",
                name
            )));
        }

        let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
        let mut inserted = 0usize;
        for tuple in tuples {
            let Some(row_id) = tuple.row_id else {
                return Err(ArtIndexError::Internal(format!(
                    "Cannot backfill index '{}' from tuple without row_id",
                    name
                )));
            };
            if let Some(values) = Self::index_value_refs_from_tuple(&entry.columns, schema, tuple) {
                let key = Self::encode_key_from_values(values.iter().copied());
                index.insert(&key, row_id)?;
                inserted += 1;
            }
        }

        Ok(inserted)
    }

    // =========================================================================
    // INDEX REMOVAL
    // =========================================================================

    /// Drop an index by name
    pub fn drop_index(&self, name: &str) -> ArtResult<()> {
        self.note_mutation();
        let index_type;
        let table;

        // Remove from main index map
        {
            let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = indexes.remove(name) {
                index_type = entry.index_type;
                table = entry.table;
            } else {
                return Err(ArtIndexError::IndexNotFound(name.to_string()));
            }
        }

        // W3.4 §3.2: drop the name from the per-table entry list (leaf lock,
        // taken alone — never while `indexes` is held).
        self.table_index_remove(&table, name);

        // Remove from type-specific maps
        match index_type {
            ArtIndexType::PrimaryKey => {
                let mut pk_indexes = self.pk_indexes.write().unwrap_or_else(|e| e.into_inner());
                pk_indexes.retain(|_, v| v != name);
            }
            ArtIndexType::ForeignKey => {
                let mut fk_indexes = self.fk_indexes.write().unwrap_or_else(|e| e.into_inner());
                for fks in fk_indexes.values_mut() {
                    fks.retain(|n| n != name);
                }
                let mut fk_info = self.fk_info.write().unwrap_or_else(|e| e.into_inner());
                fk_info.remove(name);
            }
            ArtIndexType::Unique => {
                let mut unique_indexes = self.unique_indexes.write().unwrap_or_else(|e| e.into_inner());
                for uqs in unique_indexes.values_mut() {
                    uqs.retain(|n| n != name);
                }
            }
            ArtIndexType::Manual => {
                // No additional cleanup needed
            }
        }

        self.stats.remove_index(index_type);

        Ok(())
    }

    /// Drop all indexes for a table (called on DROP TABLE)
    pub fn drop_table_indexes(&self, table: &str) -> ArtResult<()> {
        let mut to_drop = Vec::new();

        // Collect all indexes for this table (metadata only, no tree locks)
        {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            for (name, entry) in indexes.iter() {
                if entry.table == table {
                    to_drop.push(name.clone());
                }
            }
        }

        // Drop each index
        for name in to_drop {
            self.drop_index(&name)?;
        }

        Ok(())
    }

    /// Rename all indexes for a table (called on RENAME TABLE).
    ///
    /// `preserve` names the indexes whose NAME must survive the rename
    /// untouched — the ones that have a durable `meta:index:<name>` definition
    /// record (every `CREATE INDEX` / `CREATE UNIQUE INDEX`). Their entry still
    /// follows the table (`entry.table` and the tree's owning table are
    /// rewritten), only the registry KEY stays put. This is also what
    /// PostgreSQL does: `ALTER TABLE … RENAME TO` does not rename indexes.
    ///
    /// Why it is a parameter rather than a rule inferred here: the manager has
    /// no storage handle, so only [`Catalog::rename_table`] can tell which
    /// names are backed by a record — and it is the same place that rewrites
    /// those records' `table_name`, so the two halves cannot drift.
    ///
    /// Renaming a recorded index WOULD desync it from its record: `DROP INDEX
    /// "Account_email_key"` after `ALTER TABLE "Account" RENAME TO "Account2"`
    /// would find no live index under that name, delete the record, and leave
    /// the renamed entry enforcing forever — an index nobody can name or drop.
    /// The guard used to be `index_type != Manual`, which is not the same
    /// question: `CREATE UNIQUE INDEX` registers as `Unique` and DOES persist a
    /// record, and the infix replacements below never consulted the guard at
    /// all, so a manual `idx_orders_col` was rewritten too.
    pub fn rename_table_indexes(&self, old_table: &str, new_table: &str, preserve: &HashSet<String>) -> ArtResult<()> {
        self.note_mutation();
        // Move the entries under the global WRITE lock. Trees are renamed in
        // place (one tree lock at a time, see locking rules) — no tree clone.
        let mut renames: Vec<(String, String)> = Vec::new();

        {
            let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());

            let matching: Vec<String> = indexes
                .iter()
                .filter(|(_, entry)| entry.table == old_table)
                .map(|(name, _)| name.clone())
                .collect();

            for old_name in matching {
                // An index with a durable definition record keeps its name.
                let mut new_name = if preserve.contains(&old_name) {
                    old_name.clone()
                } else {
                    // Generate new index name by replacing table name
                    old_name
                        .replace(&format!("_{}_", old_table), &format!("_{}_", new_table))
                        .replace(&format!("pk_{}", old_table), &format!("pk_{}", new_table))
                        .replace(&format!("fk_{}", old_table), &format!("fk_{}", new_table))
                        .replace(&format!("unique_{}", old_table), &format!("unique_{}", new_table))
                };

                // The CONSTRAINT namespace is `{table}_pkey` / `{table}_{cols}_key`
                // / `{table}_{cols}_fkey` — a PREFIX, which none of the infix
                // replacements above match. Left un-renamed, the old name goes
                // on squatting the global registry: re-creating a table under
                // the ORIGINAL name then cannot register its own constraint
                // index (and `Catalog::create_table` now fails closed rather
                // than shipping an unenforced constraint). Auto-generated
                // constraint indexes only — anything in `preserve` was skipped
                // above and must not reach here.
                let is_generated_constraint_index = !preserve.contains(&old_name)
                    && indexes
                        .get(&old_name)
                        .is_some_and(|entry| entry.index_type != ArtIndexType::Manual);
                if new_name == old_name && is_generated_constraint_index {
                    if let Some(rest) = old_name.strip_prefix(&format!("{}_", old_table)) {
                        new_name = format!("{}_{}", new_table, rest);
                    }
                }

                if let Some(mut entry) = indexes.remove(&old_name) {
                    {
                        let mut tree = entry.tree.write().unwrap_or_else(|e| e.into_inner());
                        tree.rename(new_table.to_string(), new_name.clone());
                    }
                    entry.table = new_table.to_string();
                    indexes.insert(new_name.clone(), entry);
                    renames.push((old_name, new_name));
                }
            }
        }

        // W3.4 §3.2: move the per-table entry list to the new table key with
        // the renamed index names (leaf lock, taken alone — after the `indexes`
        // WRITE block above has been released).
        {
            let mut table_indexes = self.table_indexes.write().unwrap_or_else(|e| e.into_inner());
            if table_indexes.remove(old_table).is_some() {
                let new_names: Vec<String> = renames.iter().map(|(_, new_name)| new_name.clone()).collect();
                if !new_names.is_empty() {
                    table_indexes.insert(new_table.to_string(), new_names);
                }
            }
        }

        // Apply name-map updates (leaf locks, taken one at a time).
        //
        // ONE pass per map, over the COMPLETE rename set. This used to be a loop
        // over `renames` that did `remove(old_table)` inside each iteration: the
        // first rename moved the list to `new_table`, so the second and later
        // renames found nothing under `old_table` and left their OLD names in
        // the moved list. A stale name resolves to nothing in `indexes`, and
        // `check_unique_constraints` silently SKIPS an index it cannot resolve —
        // so the second UNIQUE index of a renamed table would have stopped being
        // enforced. Latent until now (constraint index names were not actually
        // rewritten by the replacements above); it is a live hazard the moment
        // any of them are, so it is fixed here rather than left as a trap.
        let rename_of = |name: &String| -> String {
            renames
                .iter()
                .find(|(old, _)| old == name)
                .map(|(_, new)| new.clone())
                .unwrap_or_else(|| name.clone())
        };

        {
            let mut pk_indexes = self.pk_indexes.write().unwrap_or_else(|e| e.into_inner());
            if let Some(name) = pk_indexes.remove(old_table) {
                let renamed = rename_of(&name);
                pk_indexes.insert(new_table.to_string(), renamed);
            }
        }

        {
            let mut fk_indexes = self.fk_indexes.write().unwrap_or_else(|e| e.into_inner());
            if let Some(fks) = fk_indexes.remove(old_table) {
                let new_fks: Vec<String> = fks.iter().map(|n| rename_of(n)).collect();
                fk_indexes.insert(new_table.to_string(), new_fks);
            }
        }

        {
            let mut unique_indexes = self.unique_indexes.write().unwrap_or_else(|e| e.into_inner());
            if let Some(uniques) = unique_indexes.remove(old_table) {
                let new_uniques: Vec<String> = uniques.iter().map(|n| rename_of(n)).collect();
                unique_indexes.insert(new_table.to_string(), new_uniques);
            }
        }

        // The FK metadata is keyed by index name too, and its `table` field
        // still points at the old table — both must follow the rename or a FK
        // check on the renamed child resolves nothing.
        {
            let mut fk_info = self.fk_info.write().unwrap_or_else(|e| e.into_inner());
            for (old_name, new_name) in &renames {
                if let Some(mut info) = fk_info.remove(old_name) {
                    info.name = new_name.clone();
                    info.index_name = new_name.clone();
                    if info.table == old_table {
                        info.table = new_table.to_string();
                    }
                    fk_info.insert(new_name.clone(), info);
                }
            }
        }

        self.stats.index_renames.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    // =========================================================================
    // INDEX ACCESS
    // =========================================================================

    /// Read-lock the index registry through the lock-census (W3.1). Off the
    /// `lock-census` feature this inlines to the previous
    /// `self.indexes.read().unwrap_or_else(poison)` at zero cost.
    #[inline]
    fn indexes_read(&self) -> RwLockReadGuard<'_, HashMap<String, IndexEntry>> {
        crate::lock_census::rwlock_read(crate::lock_census::Site::ArtIndexRegistry, &self.indexes)
    }

    /// Read-lock the table→PK-index-name map through the lock-census (W3.1).
    #[inline]
    fn pk_indexes_read(&self) -> RwLockReadGuard<'_, HashMap<String, String>> {
        crate::lock_census::rwlock_read(crate::lock_census::Site::ArtPkRegistry, &self.pk_indexes)
    }

    /// Get a shared handle to an index by name.
    ///
    /// Returns an `Arc<RwLock<…>>` handle instead of cloning the whole tree.
    /// Keep the lock scope on the returned handle as tight as possible.
    pub fn get_index(&self, name: &str) -> Option<SharedArtIndex> {
        let indexes = self.indexes_read();
        indexes.get(name).map(|entry| Arc::clone(&entry.tree))
    }

    /// Get the primary key index for a table
    pub fn get_pk_index(&self, table: &str) -> Option<SharedArtIndex> {
        let pk_name = {
            let pk_indexes = self.pk_indexes_read();
            pk_indexes.get(table).cloned()
        };

        pk_name.and_then(|name| self.get_index(&name))
    }

    /// Get all foreign key indexes for a table
    pub fn get_fk_indexes(&self, table: &str) -> Vec<SharedArtIndex> {
        let fk_names = {
            let fk_indexes = self.fk_indexes.read().unwrap_or_else(|e| e.into_inner());
            fk_indexes.get(table).cloned().unwrap_or_default()
        };

        fk_names.iter().filter_map(|name| self.get_index(name)).collect()
    }

    /// Get all unique indexes for a table
    pub fn get_unique_indexes(&self, table: &str) -> Vec<SharedArtIndex> {
        let unique_names = {
            let unique_indexes = self.unique_indexes.read().unwrap_or_else(|e| e.into_inner());
            unique_indexes.get(table).cloned().unwrap_or_default()
        };

        unique_names.iter().filter_map(|name| self.get_index(name)).collect()
    }

    /// Get FK info by index name
    pub fn get_fk_info(&self, index_name: &str) -> Option<ForeignKeyInfo> {
        let fk_info = self.fk_info.read().unwrap_or_else(|e| e.into_inner());
        fk_info.get(index_name).cloned()
    }

    /// List all indexes
    pub fn list_indexes(&self) -> Vec<(String, String, ArtIndexType, Vec<String>)> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        indexes
            .iter()
            .map(|(name, entry)| {
                (
                    name.clone(),
                    entry.table.clone(),
                    entry.index_type,
                    entry.columns.clone(),
                )
            })
            .collect()
    }

    /// Find an index for a specific column in a table (returns index name if found)
    pub fn find_column_index(&self, table: &str, column: &str) -> Option<String> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        for (name, entry) in indexes.iter() {
            if entry.table == table && entry.columns.len() == 1 {
                if let Some(col) = entry.columns.first() {
                    if col == column {
                        return Some(name.clone());
                    }
                }
            }
        }
        None
    }

    /// Look up all row_ids for a key in a named index (avoids cloning the entire tree)
    pub fn index_get_all(&self, index_name: &str, key: &[u8]) -> Vec<RowId> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = indexes.get(index_name) {
            let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
            tree.get_all(key)
        } else {
            Vec::new()
        }
    }

    /// R4.4: ordered, bounded range scan over a named index.
    ///
    /// Bounds are ENCODED keys (`encode_key_from_values` of a single value of
    /// the indexed column's type — encoding v2 is order-preserving per type).
    /// Returns `(key, row_id)` pairs in ascending key order.
    pub fn index_range_scan(
        &self,
        index_name: &str,
        lower: Option<(&[u8], bool)>,
        upper: Option<(&[u8], bool)>,
        limit: Option<usize>,
    ) -> Vec<(Vec<u8>, RowId)> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = indexes.get(index_name) {
            let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
            tree.range_scan(lower, upper, limit)
        } else {
            Vec::new()
        }
    }

    /// Total `(key, row_id)` entry count of a named index (one entry per
    /// indexed row, including NULL-keyed entries).
    pub fn index_entry_count(&self, index_name: &str) -> Option<u64> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        indexes.get(index_name).map(|entry| {
            let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
            tree.len()
        })
    }

    /// PK index point lookup without cloning the tree (~50μs saved vs get_pk_index)
    pub fn pk_index_lookup(&self, table: &str, key: &[u8]) -> Option<RowId> {
        let pk_name = {
            let pk_indexes = self.pk_indexes_read();
            pk_indexes.get(table).cloned()
        };
        pk_name.and_then(|name| {
            let indexes = self.indexes_read();
            indexes.get(&name).and_then(|entry| {
                let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
                tree.get(key)
            })
        })
    }

    /// Check if a PK value exists without cloning the tree
    pub fn pk_index_contains(&self, table: &str, key: &[u8]) -> Option<bool> {
        let pk_name = {
            let pk_indexes = self.pk_indexes_read();
            pk_indexes.get(table).cloned()
        };
        pk_name.map(|name| {
            let indexes = self.indexes_read();
            indexes.get(&name).is_some_and(|entry| {
                let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
                tree.contains(key)
            })
        })
    }

    /// Count how many encoded keys exist in a table's PK index while holding
    /// the index read lock once. Callers are responsible for SQL-level
    /// duplicate handling before passing keys.
    pub fn pk_index_count_keys(&self, table: &str, keys: &[Vec<u8>]) -> Option<usize> {
        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            pk_indexes.get(table).cloned()
        }?;
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        let entry = indexes.get(&pk_name)?;
        let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
        Some(keys.iter().filter(|key| tree.contains(key)).count())
    }

    /// Return the number of live entries in a table's primary-key index.
    ///
    /// For a PK index this is the table row count. This avoids cloning the ART
    /// and lets COUNT(*) skip a RocksDB key-prefix walk on ordinary PK tables.
    pub fn pk_index_len(&self, table: &str) -> Option<usize> {
        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            pk_indexes.get(table).cloned()
        }?;
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        indexes.get(&pk_name).map(|entry| {
            let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
            tree.len() as usize
        })
    }

    /// Return true when the table has exactly one ART index: a single-column
    /// primary-key index. Such tables can delete by PK without materializing
    /// the old tuple because no secondary index needs old column values.
    pub fn has_only_single_column_pk_index(&self, table: &str) -> bool {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        let mut pk_count = 0usize;

        for entry in indexes.values().filter(|entry| entry.table == table) {
            match entry.index_type {
                ArtIndexType::PrimaryKey if entry.columns.len() == 1 => {
                    pk_count += 1;
                }
                _ => return false,
            }
        }

        pk_count == 1
    }

    /// Count rows in a single-column integer PK index that satisfy an optional
    /// numeric range. Iterates the in-memory ART only; it does not fetch or
    /// deserialize table rows.
    pub fn pk_index_count_int_range(
        &self,
        table: &str,
        pk_type: &DataType,
        lower: Option<(i64, bool)>,
        upper: Option<(i64, bool)>,
    ) -> Option<usize> {
        let key_width = match pk_type {
            DataType::Int2 => 2,
            DataType::Int4 => 4,
            DataType::Int8 => 8,
            _ => return None,
        };
        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            pk_indexes.get(table).cloned()
        }?;
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        let entry = indexes.get(&pk_name)?;
        if entry.columns.len() != 1 {
            return None;
        }
        let index = entry.tree.read().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = index.dense_int_count(key_width, lower, upper) {
            return Some(count);
        }

        Some(
            index
                .iter()
                .filter_map(|(key, _)| decode_int_key(&key, key_width))
                .filter(|value| {
                    lower.map_or(
                        true,
                        |(bound, inclusive)| {
                            if inclusive {
                                *value >= bound
                            } else {
                                *value > bound
                            }
                        },
                    ) && upper.map_or(
                        true,
                        |(bound, inclusive)| {
                            if inclusive {
                                *value <= bound
                            } else {
                                *value < bound
                            }
                        },
                    )
                })
                .count(),
        )
    }

    /// List indexes for a specific table
    pub fn list_table_indexes(&self, table: &str) -> Vec<(String, ArtIndexType, Vec<String>)> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        indexes
            .iter()
            .filter(|(_, entry)| entry.table == table)
            .map(|(name, entry)| (name.clone(), entry.index_type, entry.columns.clone()))
            .collect()
    }

    // =========================================================================
    // CONSTRAINT ENFORCEMENT
    // =========================================================================

    /// Encode a composite key from multiple values
    pub fn encode_key(values: &[Value]) -> Vec<u8> {
        Self::encode_key_from_values(values.iter())
    }

    /// Encode a composite key from borrowed values.
    ///
    /// Hot insert validation paths often already hold references into a tuple;
    /// this avoids cloning strings/arrays solely to call `encode_key`.
    ///
    /// R4.4 — encoding v2, ORDER-PRESERVING for the range-scannable types:
    /// unsigned byte-wise comparison of two encoded single-column keys of the
    /// same column type matches SQL value order. Concretely:
    /// - integers: sign-flipped big-endian (`v XOR MIN` reinterpreted
    ///   unsigned), so negatives sort before positives;
    /// - floats: IEEE-754 total-order transform (positive: flip sign bit;
    ///   negative: flip all bits);
    /// - TEXT/BYTEA: raw bytes (UTF-8 byte order == code-point order).
    /// Composite (multi-column) keys additionally escape `0x00 -> 0x00 0xFF`
    /// inside variable-length values so a value byte can never collide with
    /// the `0x00` column separator. Single-column keys are NEVER escaped —
    /// range scans rely on their raw byte order.
    ///
    /// Persisted index snapshots stamp this version
    /// (`index_snapshot::ART_KEY_ENCODING_VERSION`); a snapshot written with
    /// a different version is ignored and rebuilt from rows, so the encoding
    /// can evolve without an on-disk migration step.
    pub fn encode_key_from_values<'a>(values: impl IntoIterator<Item = &'a Value>) -> Vec<u8> {
        let mut key = Vec::new();
        let mut iter = values.into_iter();
        let Some(first) = iter.next() else {
            return key;
        };
        let Some(second) = iter.next() else {
            // Single-column key: no separator, no escaping.
            Self::encode_value_into(&mut key, first, false);
            return key;
        };
        Self::encode_value_into(&mut key, first, true);
        key.push(0); // Separator
        Self::encode_value_into(&mut key, second, true);
        for value in iter {
            key.push(0); // Separator
            Self::encode_value_into(&mut key, value, true);
        }
        key
    }

    /// Append one value's encoding to `key`. `escape` is true in composite
    /// (multi-column) keys: `0x00` bytes inside variable-length values are
    /// escaped as `0x00 0xFF` so they cannot collide with the separator.
    fn encode_value_into(key: &mut Vec<u8>, value: &Value, escape: bool) {
        match value {
            Value::Null => key.extend_from_slice(b"\x00"),
            Value::Boolean(b) => key.push(if *b { 1 } else { 0 }),
            // Sign-flip: maps signed integer order onto unsigned byte order.
            Value::Int2(v) => key.extend_from_slice(&((*v as u16) ^ 0x8000).to_be_bytes()),
            Value::Int4(v) => key.extend_from_slice(&((*v as u32) ^ 0x8000_0000).to_be_bytes()),
            Value::Int8(v) => key.extend_from_slice(&((*v as u64) ^ 0x8000_0000_0000_0000).to_be_bytes()),
            // IEEE-754 total order: positive floats get the sign bit set,
            // negative floats are bitwise inverted.
            Value::Float4(v) => {
                let bits = v.to_bits();
                let ordered = if bits & 0x8000_0000 == 0 {
                    bits ^ 0x8000_0000
                } else {
                    !bits
                };
                key.extend_from_slice(&ordered.to_be_bytes());
            }
            Value::Float8(v) => {
                let bits = v.to_bits();
                let ordered = if bits & 0x8000_0000_0000_0000 == 0 {
                    bits ^ 0x8000_0000_0000_0000
                } else {
                    !bits
                };
                key.extend_from_slice(&ordered.to_be_bytes());
            }
            Value::String(s) => Self::extend_maybe_escaped(key, s.as_bytes(), escape),
            Value::Bytes(b) => Self::extend_maybe_escaped(key, b, escape),
            Value::Uuid(u) => key.extend_from_slice(u.as_bytes()),
            Value::Numeric(d) => Self::extend_maybe_escaped(key, d.as_bytes(), escape),
            Value::Date(d) => key.extend_from_slice(d.to_string().as_bytes()),
            Value::Time(t) => key.extend_from_slice(t.to_string().as_bytes()),
            Value::Timestamp(ts) => key.extend_from_slice(ts.to_rfc3339().as_bytes()),
            Value::Array(arr) => {
                // Recursively encode array elements
                let nested = Self::encode_key_from_values(arr.iter());
                Self::extend_maybe_escaped(key, &nested, escape);
            }
            Value::Json(j) => Self::extend_maybe_escaped(key, j.as_bytes(), escape),
            Value::Vector(v) => {
                for f in v {
                    key.extend_from_slice(&f.to_be_bytes());
                }
            }
            // Handle storage mode references
            Value::DictRef { dict_id } => key.extend_from_slice(&dict_id.to_be_bytes()),
            Value::CasRef { hash } => key.extend_from_slice(hash),
            Value::ColumnarRef => {
                // Columnar reference doesn't have direct key encoding
                // The actual value should be resolved before indexing
                key.extend_from_slice(b"columnar_ref");
            }
            Value::Interval(iv) => key.extend_from_slice(&iv.to_be_bytes()), // Encode interval microseconds
        }
    }

    fn extend_maybe_escaped(key: &mut Vec<u8>, bytes: &[u8], escape: bool) {
        if !escape || !bytes.contains(&0) {
            key.extend_from_slice(bytes);
            return;
        }
        for &b in bytes {
            if b == 0 {
                key.push(0);
                key.push(0xFF);
            } else {
                key.push(b);
            }
        }
    }

    fn index_value_refs_from_tuple<'a>(
        columns: &[String],
        schema: &Schema,
        tuple: &'a Tuple,
    ) -> Option<Vec<&'a Value>> {
        let mut values = Vec::with_capacity(columns.len());
        for column in columns {
            let idx = schema.get_column_index(column)?;
            values.push(tuple.values.get(idx)?);
        }
        Some(values)
    }

    /// Does this PK/UNIQUE index have to SKIP this row entirely, because one
    /// of its key components is NULL?
    ///
    /// PostgreSQL semantics: **NULLs are DISTINCT under `PRIMARY KEY` /
    /// `UNIQUE`** (the default, `NULLS DISTINCT`). A NULL never equals another
    /// NULL, so a NULL-bearing key is never entered into a uniqueness index at
    /// all — any number of rows may carry it, and none of them conflict.
    ///
    /// The ART encodes `Value::Null` as the single byte `0x00`
    /// ([`Self::encode_value_into`]), so every NULL in the same column produces
    /// the SAME key. Feeding that key to an enforcing tree makes the SECOND
    /// NULL row a "duplicate": the tree refuses it. That refusal used to be
    /// swallowed by the post-fact maintenance call sites, which is why NULLs
    /// looked correct — by accident — while the all-or-nothing undo that a
    /// refusal now triggers would take the row's PRIMARY KEY entry and every
    /// other index entry back out, leaving a STORED row invisible to indexed
    /// lookups and its primary key free for a second row to claim.
    ///
    /// So the skip is the root fix, applied uniformly:
    ///   * INSERT maintenance never writes the key (so the undo never fires for
    ///     a row that is being stored, and the undo log never holds it);
    ///   * DELETE maintenance never tries to remove a key that was never
    ///     written;
    ///   * the probes never look for one.
    ///
    /// Single-column and composite constraints follow the same rule — ANY NULL
    /// component makes the whole key non-enforcing, which is what PostgreSQL's
    /// default `NULLS DISTINCT` does for a multi-column unique index.
    ///
    /// Non-enforcing indexes (`ForeignKey`, `Manual`) are UNAFFECTED: they hold
    /// multiple row ids per key and index NULLs like any other value, so their
    /// lookups keep working.
    ///
    /// A PRIMARY KEY column is `NOT NULL`, so for a PK tree this predicate is
    /// only ever true for a row that some other check must already have
    /// rejected ([`Self::check_pk_constraint`] → `NullPrimaryKey`, and the
    /// NOT NULL validation upstream). Skipping rather than inserting there
    /// cannot open a hole the NOT NULL check does not already close.
    fn key_is_null_distinct<'a, I>(index_type: ArtIndexType, values: I) -> bool
    where
        I: IntoIterator<Item = &'a Value>,
    {
        matches!(index_type, ArtIndexType::PrimaryKey | ArtIndexType::Unique)
            && values.into_iter().any(|v| matches!(v, Value::Null))
    }

    /// W3.4 §3.3 (encode-once): return the encoding of one column value,
    /// building it once per `(schema column index, escape)` and caching it in
    /// `cache` for reuse across the row's other indexes that share the column.
    ///
    /// The cache key includes `escape` because a value's encoding depends on
    /// it: single-column keys are never escaped while multi-column keys escape
    /// every value (`encode_key_from_values`), so a column shared by a
    /// single-column and a multi-column index needs two distinct fragments.
    /// `cache` holds row-scoped fragments and MUST be cleared between rows.
    fn encode_fragment<'c>(
        cache: &'c mut Vec<(usize, bool, Vec<u8>)>,
        col_idx: usize,
        value: &Value,
        escape: bool,
    ) -> &'c [u8] {
        let pos = match cache.iter().position(|(ci, esc, _)| *ci == col_idx && *esc == escape) {
            Some(pos) => pos,
            None => {
                let mut buf = Vec::new();
                Self::encode_value_into(&mut buf, value, escape);
                cache.push((col_idx, escape, buf));
                cache.len() - 1
            }
        };
        &cache[pos].2
    }

    /// W3.4 §3.3 (encode-once): build one index key from already-resolved
    /// `(schema column index, value)` pairs, reusing per-column encoded
    /// fragments from `cache`.
    ///
    /// BYTE-IDENTICAL to `encode_key_from_values(cols.iter().map(|(_, v)| v))`
    /// (ART keys are on-disk-durable via snapshots, so this is load-bearing):
    /// a single-column key is the column's unescaped fragment; a multi-column
    /// key escapes every fragment and joins them with the `0x00` separator —
    /// exactly the two branches of `encode_key_from_values`.
    fn encode_key_cached(cache: &mut Vec<(usize, bool, Vec<u8>)>, cols: &[(usize, &Value)]) -> Vec<u8> {
        let escape = cols.len() > 1;
        let mut key = Vec::new();
        for (i, (sidx, v)) in cols.iter().enumerate() {
            if i > 0 {
                key.push(0); // column separator (matches encode_key_from_values)
            }
            let frag = Self::encode_fragment(cache, *sidx, v, escape);
            key.extend_from_slice(frag);
        }
        key
    }

    /// W3.4 §3.2 + §3.3: maintain every index in `names` (the table's complete
    /// entry list, resolved by the caller) for one inserted row, reusing the
    /// row-scoped encode-once `frag_cache` (cleared per row by the caller).
    ///
    /// `names` are looked up in the already-read `indexes` map (rule 2: the
    /// global read lock is held by the caller, one tree write lock is taken at
    /// a time). A column that does not resolve against `schema`/`tuple` skips
    /// that index — matching `index_value_refs_from_tuple` returning `None`.
    ///
    /// # What a refusal costs is decided by `row_state`, not guessed
    ///
    /// [`RowState::NotStored`] — the caller unwinds the row, so this is
    /// ALL-OR-NOTHING per row: the first index that REFUSES its key ends the
    /// row, the entries it already wrote into the indexes visited BEFORE that
    /// one are removed again ([`Self::undo_row_index_entries`]), the indexes
    /// after it are never touched, and the refusal is returned. A rejected row
    /// must not leave keys behind: nothing would ever clean them up.
    ///
    /// [`RowState::Stored`] — the row is written whatever this returns, so
    /// NOTHING IS EVER TAKEN BACK. The refusing index simply gets no entry for
    /// this row (the entry it holds belongs to the row that claimed the value
    /// first, which is why the next writer of that value is still rejected);
    /// every index visited before it KEEPS the entry it took, every index after
    /// it is still maintained, and the FIRST refusal is returned once the row is
    /// finished. Undoing on this arm was a durability bug: it stripped a STORED
    /// row of its PRIMARY KEY entry, so a full scan still counted the row while
    /// `WHERE pk = …` returned nothing and a later INSERT could claim the same
    /// primary key a second time. The error returned on this arm means A
    /// DUPLICATE IS NOW STORED — callers must surface it (post-fact callers via
    /// `StorageEngine::note_index_maintenance_failure`, which logs a stored
    /// duplicate at ERROR), never drop it.
    ///
    /// Neither arm ever fires for a NULL: NULLs are DISTINCT in PostgreSQL, so a
    /// NULL-bearing PK/UNIQUE key is skipped before it is ever offered to a tree
    /// ([`Self::key_is_null_distinct`]) and can never be refused.
    #[allow(clippy::too_many_arguments)]
    fn insert_row_indexes(
        indexes: &HashMap<String, IndexEntry>,
        names: &[String],
        row_id: RowId,
        schema: &Schema,
        tuple: &Tuple,
        frag_cache: &mut Vec<(usize, bool, Vec<u8>)>,
        wv: bool,
        row_state: RowState,
    ) -> ArtResult<()> {
        // Encode-once only pays off when >1 index may share a column; a
        // single-index table takes the original direct encode (zero overhead).
        let multi_index = names.len() > 1;
        // The row's own undo log, oldest first: what has to come back out if an
        // index further down the list refuses this row. Only ever filled on the
        // `NotStored` arm — a stored row never gives an entry back — so it stays
        // empty (and allocation-free) for every `Stored` caller and for the
        // overwhelmingly common single-index table.
        let mut inserted: Vec<(&IndexEntry, Vec<u8>, Option<i64>)> = Vec::new();
        // `RowState::Stored` reports the FIRST refusal only after the whole row
        // has been maintained (maintenance-shaped, exactly like `on_insert`).
        let mut first_error: Option<ArtIndexError> = None;
        for name in names {
            let Some(entry) = indexes.get(name) else {
                continue;
            };
            let cols = &entry.columns;
            let mut resolved: Vec<(usize, &Value)> = Vec::with_capacity(cols.len());
            let mut missing = false;
            for column in cols {
                let Some(sidx) = schema.get_column_index(column) else {
                    missing = true;
                    break;
                };
                let Some(v) = tuple.values.get(sidx) else {
                    missing = true;
                    break;
                };
                resolved.push((sidx, v));
            }
            if missing {
                continue;
            }
            // NULLs are DISTINCT under PK/UNIQUE: the key is never entered
            // into an enforcing tree, so it can never be refused — neither the
            // `NotStored` undo nor the `Stored` "a duplicate is now stored"
            // report can fire for a legal second NULL. See
            // `key_is_null_distinct`.
            if Self::key_is_null_distinct(entry.index_type, resolved.iter().map(|(_, v)| *v)) {
                continue;
            }
            let key = if multi_index {
                Self::encode_key_cached(frag_cache, &resolved)
            } else {
                Self::encode_key_from_values(resolved.iter().map(|(_, v)| *v))
            };
            // W3.2: one ART entry = encoded key + the u64 row-id payload.
            if wv {
                crate::write_volume::add(crate::write_volume::Category::IndexKey, (key.len() + 8) as u64);
            }
            let enforces = matches!(entry.index_type, ArtIndexType::PrimaryKey | ArtIndexType::Unique);
            let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
            // Bound to a `let` (not matched inline) so the `&key` borrow ends
            // here and the key can be MOVED into the undo log below.
            let outcome = index.insert(&key, row_id);
            match outcome {
                Ok(()) => {
                    // `insert` on a PK/UNIQUE tree refuses a key the tree
                    // already holds, so an `Ok` means THIS row created the
                    // entry — undoing it can only take back our own.
                    let mut dense_int = None;
                    if entry.index_type == ArtIndexType::PrimaryKey && cols.len() == 1 {
                        if let Some((value, key_width)) = Self::int_value_width(resolved[0].1) {
                            index.record_dense_int_insert(key_width, value);
                            dense_int = Some(value);
                        }
                    }
                    drop(index);
                    if row_state == RowState::NotStored {
                        inserted.push((entry, key, dense_int));
                    }
                }
                Err(e) => {
                    // Release this tree BEFORE walking back over the earlier
                    // ones — one tree write lock at a time (rule 3).
                    drop(index);
                    if enforces {
                        match row_state {
                            RowState::NotStored => {
                                Self::undo_row_index_entries(&inserted, row_id);
                                return Err(e);
                            }
                            RowState::Stored => {
                                // The row is stored. Keep every entry it already
                                // owns (its PRIMARY KEY above all) and carry on
                                // maintaining the indexes after this one, so the
                                // row stays findable by every unique column that
                                // did NOT refuse it. The refusal is reported
                                // after the row is finished.
                                if first_error.is_none() {
                                    first_error = Some(Self::stored_duplicate_error(name, row_id, &e));
                                }
                            }
                        }
                    }
                    // A non-unique index accepts duplicates, so it only fails on
                    // a corrupt tree; ignored, exactly as before.
                }
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The error a [`RowState::Stored`] refusal reports: the constraint, the
    /// row, and the original refusal in ONE message.
    ///
    /// Its caller can only LOG this (the row is already stored, or is stored
    /// regardless), so everything an operator needs to find the stored duplicate
    /// has to be inside the message — which index refused, which row, and that
    /// the row is there anyway. Kept a `DuplicateKey` so that a path which does
    /// convert it still maps to SQLSTATE 23505.
    fn stored_duplicate_error(index_name: &str, row_id: RowId, err: &ArtIndexError) -> ArtIndexError {
        ArtIndexError::DuplicateKey(format!(
            "index \"{index_name}\" refused the key of row {row_id}, which is stored anyway ({err}) — \
             that index cannot find this row, and its entry belongs to the row that claimed the value first"
        ))
    }

    /// Take back the ART entries one row already wrote, when a later index of
    /// the same table refused its key.
    ///
    /// # Reachable from [`RowState::NotStored`] arms ONLY
    ///
    /// This is correct exactly where the row is NOT stored when the refusal
    /// happens and the caller unwinds it: then every entry written for it is a
    /// PHANTOM — the next legitimate row carrying one of those keys is rejected
    /// as a duplicate of a row that does not exist, and a lookup that finds the
    /// phantom resolves to a row id holding nothing. Nothing ever cleans them up
    /// (there is no tuple whose DELETE would remove them), so leaving them
    /// behind is permanent index poisoning.
    ///
    /// Calling it for a STORED row is the inverse corruption, and was a real
    /// bug: it takes the row's PRIMARY KEY entry (and every other unique entry
    /// it legitimately owns) away from a row that stays in the heap — a full
    /// scan still counts it, `WHERE pk = …` cannot find it, and a later INSERT
    /// can claim the same primary key a second time. Hence the
    /// [`RowState::Stored`] arms of `insert_row_indexes` and
    /// `on_insert_tuple_collect_index_values` never call this: they keep every
    /// entry and report the refusal instead. The two callers that DO reach it
    /// are those funnels' `NotStored` arms.
    ///
    /// The visit-everything-and-keep-going rule is right wherever the row change
    /// is already a FACT and the ART merely has to keep describing it:
    /// `on_delete`/`on_delete_tuple` after a committed delete, `on_update`'s two
    /// halves (whose re-insert must run even if the delete half complained, or
    /// the row ends up indexed by nothing), and now the `Stored` insert arms.
    /// An INSERT that is being REJECTED is the opposite case: all-or-nothing.
    ///
    /// Only ever reached from a GENUINE refusal: a key the tree legitimately
    /// already holds. NULL-bearing PK/UNIQUE keys never enter the undo log
    /// because they are never inserted ([`Self::key_is_null_distinct`]).
    ///
    /// Walked newest-first, one tree write lock at a time (rule 3). Best effort
    /// by design — the caller returns the original refusal, which is the error
    /// the user has to see; a failure to undo must not replace it.
    fn undo_row_index_entries(inserted: &[(&IndexEntry, Vec<u8>, Option<i64>)], row_id: RowId) {
        for (entry, key, dense_int) in inserted.iter().rev() {
            let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
            match entry.index_type {
                ArtIndexType::PrimaryKey | ArtIndexType::Unique => {
                    if let Ok(Some(_)) = index.remove(key) {
                        if let Some(value) = dense_int {
                            index.record_dense_int_delete(*value);
                        }
                    }
                }
                // Multi-value leaves: take back THIS row's id only, never the
                // whole key (another row may legitimately share it).
                ArtIndexType::ForeignKey | ArtIndexType::Manual => {
                    let _ = index.remove_value(key, row_id);
                }
            }
        }
    }

    /// Check primary key constraint before INSERT
    pub fn check_pk_constraint(&self, table: &str, key_values: &[Value]) -> ArtResult<()> {
        // Check for NULL values
        for v in key_values {
            if matches!(v, Value::Null) {
                return Err(ArtIndexError::NullPrimaryKey);
            }
        }

        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            pk_indexes.get(table).cloned()
        };

        if let Some(pk_name) = pk_name {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = indexes.get(&pk_name) {
                let key = Self::encode_key(key_values);
                let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
                if tree.contains(&key) {
                    return Err(ArtIndexError::DuplicateKey(format!(
                        "Duplicate key value violates PRIMARY KEY constraint \"{}\"",
                        pk_name
                    )));
                }
            }
        }

        self.stats.constraint_checks.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Check unique constraint before INSERT/UPDATE.
    ///
    /// Also checks the PRIMARY KEY index (which is stored separately from
    /// UNIQUE indexes) so that a single call covers both constraint kinds.
    pub fn check_unique_constraints(&self, table: &str, column_values: &HashMap<String, Value>) -> ArtResult<()> {
        // Resolve names from the leaf maps first (released before tree locks).
        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            pk_indexes.get(table).cloned()
        };
        let unique_names: Vec<String> = {
            let unique_indexes = self.unique_indexes.read().unwrap_or_else(|e| e.into_inner());
            unique_indexes.get(table).cloned().unwrap_or_default()
        };

        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());

        // --- Check PK index (stored separately in pk_indexes) ---
        if let Some(pk_name) = pk_name {
            if let Some(entry) = indexes.get(&pk_name) {
                let columns = &entry.columns;
                let mut has_null = false;
                let mut values = Vec::new();

                for col in columns {
                    if let Some(v) = column_values.get(col) {
                        if matches!(v, Value::Null) {
                            has_null = true;
                            break;
                        }
                        values.push(v.clone());
                    }
                }

                if !has_null && values.len() == columns.len() {
                    let key = Self::encode_key(&values);
                    let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
                    if tree.contains(&key) {
                        return Err(ArtIndexError::DuplicateKey(format!(
                            "Duplicate key value violates PRIMARY KEY constraint \"{}\"",
                            pk_name
                        )));
                    }
                }
            }
        }

        // --- Check UNIQUE indexes (one tree lock at a time) ---
        for unique_name in &unique_names {
            if let Some(entry) = indexes.get(unique_name) {
                // Extract values for this unique constraint's columns
                let columns = &entry.columns;
                let mut has_null = false;
                let mut values = Vec::new();

                for col in columns {
                    if let Some(v) = column_values.get(col) {
                        if matches!(v, Value::Null) {
                            has_null = true;
                            break;
                        }
                        values.push(v.clone());
                    }
                }

                // NULL values are allowed in UNIQUE constraints
                if has_null {
                    continue;
                }

                if values.len() == columns.len() {
                    let key = Self::encode_key(&values);
                    let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
                    if tree.contains(&key) {
                        return Err(ArtIndexError::DuplicateKey(format!(
                            "Duplicate key value violates UNIQUE constraint \"{}\"",
                            unique_name
                        )));
                    }
                }
            }
        }

        self.stats.constraint_checks.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Tuple-backed PK/UNIQUE constraint check for storage fast paths.
    ///
    /// This avoids building a column-name map for every inserted row when the
    /// values are already available in schema order.
    pub fn check_unique_constraints_tuple(&self, table: &str, schema: &Schema, tuple: &Tuple) -> ArtResult<()> {
        // Resolve names from the leaf maps first (released before tree locks).
        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            pk_indexes.get(table).cloned()
        };
        let unique_names: Vec<String> = {
            let unique_indexes = self.unique_indexes.read().unwrap_or_else(|e| e.into_inner());
            unique_indexes.get(table).cloned().unwrap_or_default()
        };

        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());

        if let Some(pk_name) = pk_name {
            if let Some(entry) = indexes.get(&pk_name) {
                if let Some(values) = Self::index_value_refs_from_tuple(&entry.columns, schema, tuple) {
                    if !values.iter().any(|v| matches!(**v, Value::Null)) {
                        let key = Self::encode_key_from_values(values.iter().copied());
                        let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
                        if tree.contains(&key) {
                            return Err(ArtIndexError::DuplicateKey(format!(
                                "Duplicate key value violates PRIMARY KEY constraint \"{}\"",
                                pk_name
                            )));
                        }
                    }
                }
            }
        }

        for unique_name in &unique_names {
            if let Some(entry) = indexes.get(unique_name) {
                if let Some(values) = Self::index_value_refs_from_tuple(&entry.columns, schema, tuple) {
                    if values.iter().any(|v| matches!(**v, Value::Null)) {
                        continue;
                    }
                    let key = Self::encode_key_from_values(values.iter().copied());
                    let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
                    if tree.contains(&key) {
                        return Err(ArtIndexError::DuplicateKey(format!(
                            "Duplicate key value violates UNIQUE constraint \"{}\"",
                            unique_name
                        )));
                    }
                }
            }
        }

        self.stats.constraint_checks.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Return whether a PK/UNIQUE ART index already contains this key.
    pub fn unique_key_exists(&self, table: &str, columns: &[String], values: &[Value]) -> bool {
        self.unique_key_taken_by_other_row(table, columns, values, None)
    }

    /// Return whether a PK/UNIQUE ART index holds this key for some row OTHER
    /// than `exclude_row_id`.
    ///
    /// This is the UPDATE-shaped question, and it is not the same as
    /// [`Self::unique_key_exists`]: the row being updated is still IN the index
    /// under its own key, so a row can only ever be found colliding with
    /// itself. PostgreSQL compares the index entry's tid against the row being
    /// written and never reports a self-collision — `UPDATE t SET v = v` and
    /// `UPDATE t SET v = 'x'` on a row that already holds `'x'` are legal
    /// there, and so is re-assigning a value the row itself owns after a
    /// NULL round-trip.
    ///
    /// `None` means "no row is excluded" and reduces to plain existence, which
    /// is what INSERT wants and what a caller with no row id must fall back to
    /// — that direction fails CLOSED (it can only reject more, never less).
    ///
    /// # Which index answers
    ///
    /// The index is selected with `column_sets_match` — the SAME
    /// case-insensitive, order-independent set equality
    /// [`Self::find_unique_conflict`] uses for an `ON CONFLICT` arbiter. It used
    /// to be exact `Vec<String>` equality, which is a FAIL-OPEN mismatch: the
    /// callers hand over column names as the CONSTRAINT RECORD spells them
    /// (`TableConstraints.unique_constraints[i].columns`) or as the SCHEMA
    /// spells them, while `entry.columns` is however the index was registered.
    /// A constraint recorded as `UNIQUE ("Email")` against an index registered
    /// on `email` — or a composite recorded `(w, v)` against an index on
    /// `(v, w)` — matched NO index, so the UPDATE probe silently found nothing
    /// to collide with and let the duplicate through.
    ///
    /// When the two orders differ, the probe key is re-encoded in the INDEX's
    /// column order: an ART key is the ordered concatenation of its columns, so
    /// probing `(w, v)` values against a `(v, w)` tree with the caller's order
    /// would look up a key that tree can never contain — matching the index and
    /// then missing on the key is no better than skipping it.
    pub fn unique_key_taken_by_other_row(
        &self,
        table: &str,
        columns: &[String],
        values: &[Value],
        exclude_row_id: Option<RowId>,
    ) -> bool {
        // NULLs are DISTINCT under PK/UNIQUE, so a NULL-bearing key is taken by
        // nobody — the maintenance paths never wrote it into an enforcing tree
        // in the first place (`key_is_null_distinct`), and probing for it would
        // ask a tree for a key it cannot hold. This mirrors the skip that
        // `check_unique_constraints` / `check_unique_constraints_tuple` /
        // `find_unique_conflict` / `backfill_unique_index` already apply.
        //
        // Only PK/UNIQUE indexes are consulted below, so the answer is
        // unconditional here. It is NOT a fail-open: a NULL PRIMARY KEY is
        // rejected by the NOT NULL / `check_pk_constraint` path before any
        // uniqueness question is asked, and `enforce_unique_on_update` raises
        // on a NULL into a PK column before it ever calls this.
        if values.iter().any(|v| matches!(v, Value::Null)) {
            return false;
        }
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        // Encoded once for the overwhelmingly common case (the index's column
        // order is the caller's); a differing order re-encodes per index.
        let key = Self::encode_key(values);

        // Metadata filter first; tree locks taken one at a time.
        indexes.values().any(|entry| {
            if entry.table != table
                || !matches!(entry.index_type, ArtIndexType::PrimaryKey | ArtIndexType::Unique)
                || !Self::column_sets_match(&entry.columns, columns)
            {
                return false;
            }
            // Same SET, different spelling/order: permute the values into the
            // index's own column order before encoding. `columns` and `values`
            // are parallel, and `column_sets_match` already proved every one of
            // `entry.columns` is present in `columns`.
            let reordered: Option<Vec<u8>> = if entry.columns == columns {
                None
            } else {
                let mut ordered: Vec<&Value> = Vec::with_capacity(entry.columns.len());
                for want in &entry.columns {
                    let Some(pos) = columns.iter().position(|c| c.eq_ignore_ascii_case(want)) else {
                        return false;
                    };
                    let Some(v) = values.get(pos) else {
                        return false;
                    };
                    ordered.push(v);
                }
                Some(Self::encode_key_from_values(ordered.into_iter()))
            };
            let probe: &[u8] = reordered.as_deref().unwrap_or(&key);
            let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
            match exclude_row_id {
                // `get_all`, not `get`: a unique tree should hold one
                // row per key, and if it ever holds more, "some OTHER
                // row owns this key" must still come out true.
                Some(excluded) => tree.get_all(probe).into_iter().any(|row_id| row_id != excluded),
                None => tree.contains(probe),
            }
        })
    }

    /// The unique/PK ART index that would reject `column_values`, and the
    /// row_id of the row it collides with.
    ///
    /// `check_unique_constraints` answers *whether* there is a conflict;
    /// `INSERT … ON CONFLICT DO UPDATE` also needs *which row* to update, and
    /// both executor families used to re-derive that themselves from
    /// `schema.columns` — the column-level `unique` flag plus a PRIMARY KEY
    /// fallback. That derivation missed every constraint the flag does not
    /// describe (a composite `UNIQUE (a, b)`, a `CREATE UNIQUE INDEX`) and, in
    /// the text family, could only decode an `Int4`/`Int8` primary key, so an
    /// upsert on a table with a `UUID`/`TEXT` primary key failed with
    /// "could not find existing row". Resolving the conflict against the ART
    /// registry — the structure that actually holds the keys — covers every
    /// spelling by construction.
    ///
    /// # The ARBITER
    ///
    /// `arbiter` is the `ON CONFLICT (<cols>)` conflict target, already
    /// resolved by the planner. When it is `Some`, ONLY the PK/UNIQUE index
    /// over exactly that column set is probed, because that is the only
    /// constraint PostgreSQL lets the statement handle: a collision on any
    /// OTHER constraint is a plain 23505 there, not an upsert. Returning the
    /// first index that happens to trip instead — which is what this did, with
    /// the PRIMARY KEY chained FIRST — turned
    /// `INSERT INTO oc VALUES (1,'z',5) ON CONFLICT (v) DO UPDATE …` (PK
    /// conflict, target `v` clean) into a silent update of a row the statement
    /// never named, and made a row colliding on BOTH the PK and the target
    /// update the PK's row rather than the target's. The caller re-raises the
    /// original unique violation when an arbitrated probe finds nothing, so the
    /// non-target conflict fails CLOSED.
    ///
    /// Matching is case-insensitive and ORDER-INDEPENDENT (a unique constraint
    /// is a SET of columns), matching the planner's own target resolution.
    ///
    /// With no arbiter (`ON CONFLICT DO UPDATE` without a target, and MySQL's
    /// `ON DUPLICATE KEY UPDATE`, which really do mean "any unique constraint")
    /// PRIMARY KEY is probed first — PostgreSQL reports the PK conflict first
    /// too — then the UNIQUE indexes in registration order. NULL-bearing keys
    /// are skipped exactly as the checks do: a NULL never conflicts.
    pub fn find_unique_conflict(
        &self,
        table: &str,
        column_values: &HashMap<String, Value>,
        arbiter: Option<&[String]>,
    ) -> Option<UniqueConflict> {
        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            pk_indexes.get(table).cloned()
        };
        let unique_names: Vec<String> = {
            let unique_indexes = self.unique_indexes.read().unwrap_or_else(|e| e.into_inner());
            unique_indexes.get(table).cloned().unwrap_or_default()
        };

        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        for (name, is_primary_key) in pk_name
            .iter()
            .map(|n| (n, true))
            .chain(unique_names.iter().map(|n| (n, false)))
        {
            let Some(entry) = indexes.get(name) else {
                continue;
            };
            if let Some(arbiter) = arbiter {
                if !Self::column_sets_match(&entry.columns, arbiter) {
                    continue;
                }
            }
            let mut values = Vec::with_capacity(entry.columns.len());
            let mut usable = true;
            for column in &entry.columns {
                match column_values.get(column) {
                    Some(Value::Null) | None => {
                        usable = false;
                        break;
                    }
                    Some(v) => values.push(v),
                }
            }
            if !usable || values.len() != entry.columns.len() {
                continue;
            }
            let key = Self::encode_key_from_values(values.into_iter());
            let row_id = {
                let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
                tree.get(&key)
            };
            if let Some(row_id) = row_id {
                return Some(UniqueConflict {
                    index_name: name.clone(),
                    columns: entry.columns.clone(),
                    row_id,
                    is_primary_key,
                });
            }
        }
        None
    }

    /// Do two column sets describe the same unique rule?
    ///
    /// Case-insensitive and ORDER-INDEPENDENT: a unique constraint is a SET of
    /// columns, so `(v, w)` and `(w, v)` are the same rule (PostgreSQL agrees).
    /// Allocation-free for the overwhelmingly common single-column case and for
    /// any pair of different arities.
    fn column_sets_match(a: &[String], b: &[String]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        if a.len() == 1 {
            return a.first().zip(b.first()).is_some_and(|(x, y)| x.eq_ignore_ascii_case(y));
        }
        // Multi-column: containment BOTH ways, so a list that repeats a column
        // cannot pass as a set it is not equal to (`["a","a"]` vs `["a","b"]`
        // are the same length and one-way-contained). Lists here are constraint
        // column lists — one or two entries in practice — so the quadratic scan
        // is cheaper than allocating to sort.
        a.iter().all(|x| b.iter().any(|y| x.eq_ignore_ascii_case(y)))
            && b.iter().all(|y| a.iter().any(|x| x.eq_ignore_ascii_case(y)))
    }

    /// The names of every PK/UNIQUE index registered for `table`.
    ///
    /// PERF (the reason this exists): the metadata predicates below used to
    /// answer their question by scanning the SINGLE GLOBAL `indexes` map —
    /// O(every index in the database) per call, on paths that run per UPDATE
    /// ROW and per fast-path spec build. `pk_indexes` / `unique_indexes` are
    /// maintained in lock-step with `indexes` by the only four registration
    /// sites (`create_pk_index`, `create_unique_index`, `drop_index`,
    /// `rename_table_indexes`), so resolving through them is exact AND
    /// O(indexes on THIS table).
    ///
    /// Leaf-lock discipline (locking rule #4): the two name maps are cloned and
    /// released here, so the caller can take the `indexes` lock afterwards
    /// without ever holding both. Returns empty — with no allocation — for a
    /// table that has neither, which is the common miss.
    fn unique_index_names(&self, table: &str) -> Vec<String> {
        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            pk_indexes.get(table).cloned()
        };
        let unique_names: Vec<String> = {
            let unique_indexes = self.unique_indexes.read().unwrap_or_else(|e| e.into_inner());
            unique_indexes.get(table).cloned().unwrap_or_default()
        };
        if pk_name.is_none() && unique_names.is_empty() {
            return Vec::new();
        }
        let mut names = Vec::with_capacity(unique_names.len() + usize::from(pk_name.is_some()));
        names.extend(pk_name);
        names.extend(unique_names);
        names
    }

    /// Is there already a PK/UNIQUE index on exactly `columns` of `table`?
    ///
    /// Metadata only — no tree lock. Used to keep one column set backed by ONE
    /// enforcing index no matter how many catalog records describe it (an
    /// inline `UNIQUE` writes both a column flag and a `TableConstraints`
    /// record), and to let `Catalog::create_table` fail CLOSED: a unique
    /// constraint whose index could not be registered is enforced by nothing.
    pub fn has_unique_index_on(&self, table: &str, columns: &[String]) -> bool {
        let names = self.unique_index_names(table);
        if names.is_empty() {
            return false;
        }
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        names
            .iter()
            .filter_map(|name| indexes.get(name))
            .any(|entry| entry.columns == columns)
    }

    /// Does any PK/UNIQUE index of `table` include `column`?
    ///
    /// The UPDATE fast paths bail on a uniqueness-bearing column so the full
    /// path can run `enforce_unique_on_update`. They used to ask the SCHEMA
    /// (`column.unique`), which is blind to a `CREATE UNIQUE INDEX` and to
    /// `ALTER TABLE … ADD CONSTRAINT … UNIQUE` — so an UPDATE onto a duplicate
    /// value took the fast path and was never checked. Asking the index registry
    /// covers every spelling, including a composite constraint the column merely
    /// participates in.
    pub fn column_in_unique_index(&self, table: &str, column: &str) -> bool {
        let names = self.unique_index_names(table);
        if names.is_empty() {
            return false;
        }
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        names
            .iter()
            .filter_map(|name| indexes.get(name))
            .any(|entry| entry.columns.iter().any(|c| c.eq_ignore_ascii_case(column)))
    }

    /// Every column set on `table` that a live PK/UNIQUE ART index enforces.
    ///
    /// The `ON CONFLICT (…)` conflict-target resolver matches against this, so
    /// every spelling that produced an enforcing index — inline `UNIQUE`,
    /// table-level `UNIQUE (…)`, composite, `CREATE UNIQUE INDEX`,
    /// `ALTER TABLE … ADD CONSTRAINT … UNIQUE` — is a valid target, and
    /// anything else raises 42P10 instead of silently upserting on a different
    /// constraint.
    ///
    /// ALLOCATES one `Vec<String>` per matching index, so callers on a per-ROW
    /// path must hoist it to per-STATEMENT (see `enforce_unique_on_update`,
    /// whose two callers now compute it once before their row loop).
    pub fn unique_column_sets(&self, table: &str) -> Vec<Vec<String>> {
        let names = self.unique_index_names(table);
        if names.is_empty() {
            return Vec::new();
        }
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        names
            .iter()
            .filter_map(|name| indexes.get(name))
            .map(|entry| entry.columns.clone())
            .collect()
    }

    /// Populate a UNIQUE index from the rows already in the table, REJECTING
    /// the first duplicate.
    ///
    /// The unique twin of [`Self::backfill_manual_index`], and the reason
    /// `CREATE UNIQUE INDEX` / `ALTER TABLE … ADD CONSTRAINT … UNIQUE` can fail
    /// closed on an already-duplicated column instead of registering a
    /// constraint that the existing rows violate. NULL-bearing keys are skipped
    /// (PostgreSQL: NULLs are distinct), matching `check_unique_constraints`.
    pub fn backfill_unique_index(&self, name: &str, schema: &Schema, tuples: &[Tuple]) -> ArtResult<usize> {
        self.note_mutation();
        let entry = {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            indexes
                .get(name)
                .cloned()
                .ok_or_else(|| ArtIndexError::IndexNotFound(name.to_string()))?
        };
        if entry.index_type != ArtIndexType::Unique {
            return Err(ArtIndexError::Internal(format!(
                "Index '{}' is not a unique index",
                name
            )));
        }

        let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
        let mut inserted = 0usize;
        for tuple in tuples {
            let Some(row_id) = tuple.row_id else {
                return Err(ArtIndexError::Internal(format!(
                    "Cannot backfill index '{}' from tuple without row_id",
                    name
                )));
            };
            let Some(values) = Self::index_value_refs_from_tuple(&entry.columns, schema, tuple) else {
                continue;
            };
            if values.iter().any(|v| matches!(**v, Value::Null)) {
                continue;
            }
            let key = Self::encode_key_from_values(values.iter().copied());
            if index.contains(&key) {
                return Err(ArtIndexError::DuplicateKey(format!(
                    "Duplicate key value violates UNIQUE constraint \"{}\"",
                    name
                )));
            }
            index.insert(&key, row_id)?;
            inserted += 1;
        }

        Ok(inserted)
    }

    /// Check foreign key constraint before INSERT/UPDATE
    ///
    /// Deadlock safety: the referenced table's PK tree lock is the ONLY tree
    /// lock this function ever holds, and it is released before the next FK
    /// is checked. Leaf name maps are snapshotted up front and released
    /// before any tree lock is taken.
    pub fn check_fk_constraints(&self, table: &str, column_values: &HashMap<String, Value>) -> ArtResult<()> {
        // Snapshot FK metadata from the leaf maps (released before tree locks).
        let fk_names: Vec<String> = {
            let fk_indexes = self.fk_indexes.read().unwrap_or_else(|e| e.into_inner());
            fk_indexes.get(table).cloned().unwrap_or_default()
        };

        if !fk_names.is_empty() {
            let fk_infos: Vec<ForeignKeyInfo> = {
                let fk_info_map = self.fk_info.read().unwrap_or_else(|e| e.into_inner());
                fk_names
                    .iter()
                    .filter_map(|name| fk_info_map.get(name).cloned())
                    .collect()
            };

            for fk_info in &fk_infos {
                // Extract values for FK columns
                let mut values = Vec::new();
                let mut has_null = false;

                for col in &fk_info.columns {
                    if let Some(v) = column_values.get(col) {
                        if matches!(v, Value::Null) {
                            has_null = true;
                            break;
                        }
                        values.push(v.clone());
                    }
                }

                // NULL values in FK columns are allowed (no reference check)
                if has_null {
                    continue;
                }

                // Check if referenced row exists in parent table's PK index
                let ref_table = &fk_info.ref_table;
                let ref_pk_name = {
                    let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
                    pk_indexes.get(ref_table).cloned()
                };

                if let Some(ref_pk_name) = ref_pk_name {
                    let contains = {
                        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
                        indexes.get(&ref_pk_name).map(|entry| {
                            let key = Self::encode_key(&values);
                            let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
                            tree.contains(&key)
                        })
                    };
                    if contains == Some(false) {
                        self.stats.violations_caught.fetch_add(1, Ordering::Relaxed);
                        return Err(ArtIndexError::ForeignKeyViolation(format!(
                            "Key ({:?}) not present in table \"{}\"",
                            values, ref_table
                        )));
                    }
                }
            }
        }

        self.stats.constraint_checks.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    // =========================================================================
    // INDEX MAINTENANCE
    // =========================================================================

    /// Update indexes after INSERT
    ///
    /// Takes the global map lock as READ (concurrent across tables) and the
    /// per-tree WRITE locks one at a time (serializes only same-index writers).
    ///
    /// Every index of the table is visited even when one of them refuses its
    /// key; the FIRST error is returned once the loop is done. This used to
    /// `?` out of the loop on the first failure, which made a single bad key
    /// silently unmaintain every index registered AFTER it — the amplifier
    /// that turned a wrong-arguments bug in the `ON CONFLICT DO UPDATE` leg
    /// into "the updated row vanished from `=` lookups": the PRIMARY KEY
    /// index (registered first) rejected an unchanged key as a duplicate and
    /// the UNIQUE index behind it never got its entry back. Partial
    /// maintenance is bad enough on its own; it must not be contagious.
    ///
    /// This is the MAINTENANCE-shaped entry point: it is reached from
    /// `on_update`'s re-insert half and from call sites that have already
    /// written (or staged) the row and only log what comes back — so the row is
    /// a FACT, and taking its entries away would leave a stored row indexed by
    /// nothing. Rolling one back is the transaction's job
    /// (`ArtUndoOp::RemoveInserted`), not this function's.
    ///
    /// The tuple twins (`insert_row_indexes`,
    /// `on_insert_tuple_collect_index_values`) behave EXACTLY like this function
    /// when their caller passes [`RowState::Stored`] — same reason, same rule.
    /// They differ only on [`RowState::NotStored`], where the row is about to be
    /// unwound and all-or-nothing is the only way to keep phantom keys out of
    /// the ART. `RowState` is what separates the two cases; this entry point has
    /// no `NotStored` caller, so it is fixed at the maintenance rule.
    pub fn on_insert(&self, table: &str, row_id: RowId, column_values: &HashMap<String, Value>) -> ArtResult<()> {
        self.note_mutation();
        // W3.4 §3.2: resolve the table's own indexes from the per-table entry
        // list (leaf lock: clone the names, release) instead of scanning the
        // whole `indexes` map.
        let names = {
            let table_indexes = self.table_indexes.read().unwrap_or_else(|e| e.into_inner());
            match table_indexes.get(table) {
                Some(list) if !list.is_empty() => list.clone(),
                _ => return Ok(()),
            }
        };
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        let mut first_error: Option<ArtIndexError> = None;

        for name in &names {
            let Some(entry) = indexes.get(name) else {
                continue;
            };

            // Extract values for indexed columns
            let values: Vec<Value> = entry
                .columns
                .iter()
                .filter_map(|col| column_values.get(col).cloned())
                .collect();

            // NULLs are DISTINCT under PK/UNIQUE (`key_is_null_distinct`):
            // no key is written, so the second NULL row is not a "duplicate".
            // This is the arm `on_update`'s re-insert half comes through, so it
            // also covers `UPDATE … SET v = NULL` onto a column another row has
            // already set to NULL.
            if values.len() == entry.columns.len() && !Self::key_is_null_distinct(entry.index_type, values.iter()) {
                let key = Self::encode_key(&values);
                // Note: Constraint checking should have already been done
                // For non-unique indexes, we allow "duplicates" (same key, different row_id)
                let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
                match entry.index_type {
                    ArtIndexType::PrimaryKey | ArtIndexType::Unique => {
                        // Already checked, just insert
                        match index.insert(&key, row_id) {
                            Ok(()) => {
                                if entry.index_type == ArtIndexType::PrimaryKey && values.len() == 1 {
                                    if let Some((value, key_width)) = Self::int_value_width(&values[0]) {
                                        index.record_dense_int_insert(key_width, value);
                                    }
                                }
                            }
                            Err(e) => {
                                if first_error.is_none() {
                                    first_error = Some(e);
                                }
                            }
                        }
                    }
                    ArtIndexType::ForeignKey | ArtIndexType::Manual => {
                        // These allow duplicates
                        let _ = index.insert(&key, row_id);
                    }
                }
            }
        }

        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Update indexes after INSERT using an already-materialized tuple.
    ///
    /// [`RowState::Stored`], always: the one production caller
    /// (`StorageEngine::insert_tuple_fast`) has already `put()` the row (and
    /// appended its logical-WAL record) before it gets here, so a refusal cannot
    /// unmake the row — it can only describe it. The row therefore keeps every
    /// entry it already owns, the indexes after the refusing one are still
    /// maintained, and the refusal comes back as an error the caller reports
    /// through `note_index_maintenance_failure` (ERROR for a stored duplicate).
    /// Undoing here would strip a durable row of its PRIMARY KEY entry.
    ///
    /// The funnel that may undo is the transactional twin,
    /// [`Self::on_insert_tuple_collect_index_values`] with
    /// [`RowState::NotStored`] — the only shape whose caller unwinds the row.
    pub fn on_insert_tuple(&self, table: &str, row_id: RowId, schema: &Schema, tuple: &Tuple) -> ArtResult<()> {
        self.note_mutation();
        // W3.2: hoist the write-volume census fast-out (one relaxed load for the
        // whole per-index loop). Attributed to the ambient INSERT class scope.
        let wv = crate::write_volume::enabled();
        // W3.4 §3.2: resolve the table's own indexes (leaf lock, cloned+released).
        let names = {
            let table_indexes = self.table_indexes.read().unwrap_or_else(|e| e.into_inner());
            match table_indexes.get(table) {
                Some(list) if !list.is_empty() => list.clone(),
                _ => return Ok(()),
            }
        };
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        let mut frag_cache: Vec<(usize, bool, Vec<u8>)> = Vec::new();
        Self::insert_row_indexes(
            &indexes,
            &names,
            row_id,
            schema,
            tuple,
            &mut frag_cache,
            wv,
            RowState::Stored,
        )
    }

    /// W3.4 §3.2/§3.3: batch INSERT index maintenance for the COPY funnel.
    ///
    /// Resolves the table's complete index set ONCE (one `table_indexes` read +
    /// one `indexes` read for the whole batch) instead of per row, and reuses a
    /// single encode-once fragment cache across the batch's rows. Tree write
    /// locks are still taken one row at a time (rule 3); ART maintenance runs
    /// post-commit, so single-WriteBatch durability is untouched.
    ///
    /// # Every row here is [`RowState::Stored`]
    ///
    /// This runs AFTER the batch has been written, so a refusal cannot unmake
    /// the row it refers to. The row therefore keeps every entry it already owns
    /// — its PRIMARY KEY above all — the refusing index keeps the entry of
    /// whoever claimed that value first (so the next writer of it is still
    /// rejected), the row's remaining indexes are still maintained, and the rest
    /// of the batch is maintained normally: one bad row costs only the one
    /// constraint it collided on. Undoing the row's entries here would strip a
    /// COMMITTED row of its PRIMARY KEY entry, leaving it countable by a scan
    /// and unreachable by `WHERE pk = …`.
    ///
    /// A refusal is not a debug detail: it means the committed batch holds a row
    /// a unique index cannot find. Each one is logged at ERROR with the table,
    /// the row id and the refusing index, and the FIRST one is RETURNED so the
    /// caller can add its own recovery note. The single caller
    /// (`insert_prepared_tuples_fast_batch`) pre-checks PK/UNIQUE for every row
    /// it hands over, so this stays defensive.
    pub fn on_insert_tuples(&self, table: &str, schema: &Schema, rows: &[(RowId, Tuple)]) -> ArtResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.note_mutation();
        let wv = crate::write_volume::enabled();
        let names = {
            let table_indexes = self.table_indexes.read().unwrap_or_else(|e| e.into_inner());
            match table_indexes.get(table) {
                Some(list) if !list.is_empty() => list.clone(),
                _ => return Ok(()),
            }
        };
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        // One row-scoped fragment cache, cleared per row (fragments are
        // row-specific but the buffer capacity is reused across the batch).
        let mut frag_cache: Vec<(usize, bool, Vec<u8>)> = Vec::new();
        // The batch is already committed, so a refused row is a STORED
        // duplicate: name every one of them (only the first can be returned),
        // and never stop maintaining the rows that follow.
        let mut first_error: Option<ArtIndexError> = None;
        for (row_id, tuple) in rows {
            frag_cache.clear();
            if let Err(e) = Self::insert_row_indexes(
                &indexes,
                &names,
                *row_id,
                schema,
                tuple,
                &mut frag_cache,
                wv,
                RowState::Stored,
            ) {
                tracing::error!(
                    "ART index maintenance refused a committed row of table '{}': {} — the row stays in \
                     the heap, so a full scan and an indexed lookup now disagree for it. The ART is \
                     rebuilt from `data:` when the database is next opened; reopen to restore agreement.",
                    table,
                    e
                );
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Update indexes after INSERT using an already-materialized tuple and
    /// return only the indexed column values needed to undo the insert.
    ///
    /// The buffered/transactional INSERT twin of [`Self::insert_row_indexes`],
    /// and the ONLY funnel with callers of both kinds — which is why the kind is
    /// a parameter rather than an assumption
    /// (`EmbeddedDatabase::insert_prepared_tuple_in_transaction` passes it from
    /// its own `constraints_prechecked` flag):
    ///
    /// * [`RowState::NotStored`] — the caller turns the refusal into
    ///   `Error::constraint_violation` and the row is never written.
    ///   ALL-OR-NOTHING: the first index that refuses this row's key takes back
    ///   the entries the row already wrote into the indexes before it
    ///   ([`Self::undo_row_index_entries`]), leaves the ones after it untouched,
    ///   and returns the refusal. A key left behind would belong to a row that
    ///   never existed.
    /// * [`RowState::Stored`] — the caller has already validated the batch and
    ///   writes this row whatever comes back, so NOTHING is taken back: every
    ///   entry the row owns stays (its PRIMARY KEY included), the indexes after
    ///   the refusing one are still maintained, and the first refusal is
    ///   returned for the caller to REPORT. Undoing here would leave a row that
    ///   is written and committed with no PRIMARY KEY entry — countable by a
    ///   full scan, unreachable by `WHERE pk = …`, and its key free for the next
    ///   INSERT to claim.
    ///
    /// On `Err` the `indexed_values` map is not returned; a `Stored` caller that
    /// still needs it (for its own rollback bookkeeping) rebuilds it from the
    /// tuple, which is a superset of what this collects.
    pub fn on_insert_tuple_collect_index_values(
        &self,
        table: &str,
        row_id: RowId,
        schema: &Schema,
        tuple: &Tuple,
        row_state: RowState,
    ) -> ArtResult<HashMap<String, Value>> {
        self.note_mutation();
        // W3.2: census fast-out for the buffered/txn insert index path.
        let wv = crate::write_volume::enabled();
        // W3.4 §3.2: resolve the table's own indexes (leaf lock, cloned+released).
        let names = {
            let table_indexes = self.table_indexes.read().unwrap_or_else(|e| e.into_inner());
            match table_indexes.get(table) {
                Some(list) if !list.is_empty() => list.clone(),
                _ => return Ok(HashMap::new()),
            }
        };
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        let multi_index = names.len() > 1;
        let mut frag_cache: Vec<(usize, bool, Vec<u8>)> = Vec::new();
        let mut indexed_values = HashMap::new();
        // The row's own undo log, oldest first (see `undo_row_index_entries`).
        // Only filled on the `NotStored` arm — a stored row never gives an entry
        // back.
        let mut inserted: Vec<(&IndexEntry, Vec<u8>, Option<i64>)> = Vec::new();
        // `RowState::Stored` reports the FIRST refusal only after the whole row
        // has been maintained.
        let mut first_error: Option<ArtIndexError> = None;

        for name in &names {
            let Some(entry) = indexes.get(name) else {
                continue;
            };

            let mut resolved: Vec<(usize, &Value)> = Vec::with_capacity(entry.columns.len());
            let mut missing = false;
            for column in &entry.columns {
                let Some(idx) = schema.get_column_index(column) else {
                    missing = true;
                    break;
                };
                let Some(value) = tuple.values.get(idx) else {
                    missing = true;
                    break;
                };
                // Record the value even for an index that later bails on a
                // missing column — the undo log needs every value we resolved
                // (byte-for-byte the pre-change accumulation order).
                indexed_values.entry(column.clone()).or_insert_with(|| value.clone());
                resolved.push((idx, value));
            }

            // NULLs are DISTINCT under PK/UNIQUE (`key_is_null_distinct`).
            // The resolved values above are still recorded in `indexed_values`
            // — the caller's undo map describes the ROW, not the tree — but no
            // key is written, so nothing has to be taken back either.
            if !missing
                && resolved.len() == entry.columns.len()
                && !Self::key_is_null_distinct(entry.index_type, resolved.iter().map(|(_, v)| *v))
            {
                let key = if multi_index {
                    Self::encode_key_cached(&mut frag_cache, &resolved)
                } else {
                    Self::encode_key_from_values(resolved.iter().map(|(_, v)| *v))
                };
                // W3.2: one ART entry = encoded key + the u64 row-id payload.
                if wv {
                    crate::write_volume::add(crate::write_volume::Category::IndexKey, (key.len() + 8) as u64);
                }
                let enforces = matches!(entry.index_type, ArtIndexType::PrimaryKey | ArtIndexType::Unique);
                let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
                // `let`-bound so the `&key` borrow ends before the move below.
                let outcome = index.insert(&key, row_id);
                match outcome {
                    Ok(()) => {
                        let mut dense_int = None;
                        if entry.index_type == ArtIndexType::PrimaryKey && entry.columns.len() == 1 {
                            if let Some((value, key_width)) = Self::int_value_width(resolved[0].1) {
                                index.record_dense_int_insert(key_width, value);
                                dense_int = Some(value);
                            }
                        }
                        drop(index);
                        if row_state == RowState::NotStored {
                            inserted.push((entry, key, dense_int));
                        }
                    }
                    Err(e) => {
                        // Release this tree before walking back over the earlier
                        // ones (one tree write lock at a time).
                        drop(index);
                        if enforces {
                            match row_state {
                                RowState::NotStored => {
                                    Self::undo_row_index_entries(&inserted, row_id);
                                    return Err(e);
                                }
                                RowState::Stored => {
                                    // The caller writes this row whatever we
                                    // answer: keep every entry it already owns
                                    // and keep maintaining the rest, then report.
                                    if first_error.is_none() {
                                        first_error = Some(Self::stored_duplicate_error(name, row_id, &e));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        match first_error {
            Some(e) => Err(e),
            None => Ok(indexed_values),
        }
    }

    /// Update indexes after DELETE
    ///
    /// Global map lock as READ + per-tree WRITE locks one at a time.
    pub fn on_delete(&self, table: &str, row_id: RowId, column_values: &HashMap<String, Value>) -> ArtResult<()> {
        self.note_mutation();
        // W3.4 §3.2: resolve the table's own indexes (leaf lock, cloned+released).
        let names = {
            let table_indexes = self.table_indexes.read().unwrap_or_else(|e| e.into_inner());
            match table_indexes.get(table) {
                Some(list) if !list.is_empty() => list.clone(),
                _ => return Ok(()),
            }
        };
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        // Same rule as `on_insert`: visit every index, report the first error
        // afterwards. Bailing mid-loop leaves the indexes AFTER the failing one
        // still holding the deleted row.
        let mut first_error: Option<ArtIndexError> = None;

        for name in &names {
            let Some(entry) = indexes.get(name) else {
                continue;
            };

            let values: Vec<Value> = entry
                .columns
                .iter()
                .filter_map(|col| column_values.get(col).cloned())
                .collect();

            // A NULL-bearing key was never written to an enforcing tree
            // (`key_is_null_distinct`), so there is nothing to remove — and
            // trying would risk removing an entry this row never owned.
            if values.len() == entry.columns.len() && !Self::key_is_null_distinct(entry.index_type, values.iter()) {
                let key = Self::encode_key(&values);
                let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
                match entry.index_type {
                    ArtIndexType::PrimaryKey | ArtIndexType::Unique => {
                        // Unique indexes: remove entire key entry
                        match index.remove(&key) {
                            Ok(previous) => {
                                if previous.is_some()
                                    && entry.index_type == ArtIndexType::PrimaryKey
                                    && values.len() == 1
                                {
                                    if let Some((value, _)) = Self::int_value_width(&values[0]) {
                                        index.record_dense_int_delete(value);
                                    }
                                }
                            }
                            Err(e) => {
                                if first_error.is_none() {
                                    first_error = Some(e);
                                }
                            }
                        }
                    }
                    ArtIndexType::ForeignKey | ArtIndexType::Manual => {
                        // Non-unique indexes: remove only the specific row_id
                        let _ = index.remove_value(&key, row_id);
                    }
                }
            }
        }

        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Update indexes after DELETE using the already-materialized tuple.
    pub fn on_delete_tuple(&self, table: &str, row_id: RowId, schema: &Schema, tuple: &Tuple) -> ArtResult<()> {
        self.note_mutation();
        // W3.4 §3.2: resolve the table's own indexes (leaf lock, cloned+released).
        let names = {
            let table_indexes = self.table_indexes.read().unwrap_or_else(|e| e.into_inner());
            match table_indexes.get(table) {
                Some(list) if !list.is_empty() => list.clone(),
                _ => return Ok(()),
            }
        };
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        let multi_index = names.len() > 1;
        let mut frag_cache: Vec<(usize, bool, Vec<u8>)> = Vec::new();
        // Same rule as the `HashMap`-shaped `on_delete`: visit every index, then
        // report the first error. Bailing mid-loop left the indexes AFTER the
        // failing one still holding the DELETED row — entries pointing at a row
        // that no longer exists, which then reject the next writer of that key.
        let mut first_error: Option<ArtIndexError> = None;

        for name in &names {
            let Some(entry) = indexes.get(name) else {
                continue;
            };

            let mut resolved: Vec<(usize, &Value)> = Vec::with_capacity(entry.columns.len());
            let mut missing = false;
            for column in &entry.columns {
                let Some(sidx) = schema.get_column_index(column) else {
                    missing = true;
                    break;
                };
                let Some(v) = tuple.values.get(sidx) else {
                    missing = true;
                    break;
                };
                resolved.push((sidx, v));
            }
            if missing {
                continue;
            }
            // Symmetric with the insert twin: a NULL-bearing PK/UNIQUE key was
            // never written, so it must not be removed (`key_is_null_distinct`).
            if Self::key_is_null_distinct(entry.index_type, resolved.iter().map(|(_, v)| *v)) {
                continue;
            }

            let key = if multi_index {
                Self::encode_key_cached(&mut frag_cache, &resolved)
            } else {
                Self::encode_key_from_values(resolved.iter().map(|(_, v)| *v))
            };
            let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
            match entry.index_type {
                ArtIndexType::PrimaryKey | ArtIndexType::Unique => match index.remove(&key) {
                    Ok(previous) => {
                        if previous.is_some()
                            && entry.index_type == ArtIndexType::PrimaryKey
                            && entry.columns.len() == 1
                        {
                            if let Some((value, _)) = Self::int_value_width(resolved[0].1) {
                                index.record_dense_int_delete(value);
                            }
                        }
                    }
                    Err(e) => {
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                    }
                },
                ArtIndexType::ForeignKey | ArtIndexType::Manual => {
                    let _ = index.remove_value(&key, row_id);
                }
            }
        }

        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Remove one single-column primary-key entry when the caller already has
    /// the encoded PK key. This avoids fetching/deserializing the old row for
    /// PK-only DELETE fast paths.
    pub fn remove_single_pk_key(&self, table: &str, key: &[u8], row_id: RowId, pk_value: &Value) -> ArtResult<bool> {
        self.note_mutation();
        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            match pk_indexes.get(table) {
                Some(name) => name.clone(),
                None => return Ok(false),
            }
        };

        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = indexes.get(&pk_name) else {
            return Ok(false);
        };
        if !matches!(entry.index_type, ArtIndexType::PrimaryKey) || entry.columns.len() != 1 {
            return Ok(false);
        }

        let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
        if index.get(key) != Some(row_id) {
            return Ok(false);
        }

        let removed = index.remove(key)?.is_some();
        if removed {
            if let Some((value, _)) = Self::int_value_width(pk_value) {
                index.record_dense_int_delete(value);
            }
        }
        Ok(removed)
    }

    /// Update indexes after UPDATE
    ///
    /// The insert half runs even when the delete half reported a problem, for
    /// the same reason `on_insert` visits every index: half-maintained is bad,
    /// and skipping the re-insert would leave the row indexed by NOTHING. The
    /// first error of the two is returned once both have run.
    pub fn on_update(
        &self,
        table: &str,
        row_id: RowId,
        old_values: &HashMap<String, Value>,
        new_values: &HashMap<String, Value>,
    ) -> ArtResult<()> {
        // Remove old index entries and add new ones
        let deleted = self.on_delete(table, row_id, old_values);
        let inserted = self.on_insert(table, row_id, new_values);
        deleted.and(inserted)
    }

    /// Return true if a tuple update changes any column covered by an ART index.
    ///
    /// Most OLTP updates mutate payload columns while the PK/unique/manual index
    /// columns stay unchanged. In that case callers can skip the expensive
    /// delete+insert index maintenance path entirely.
    pub fn tuple_update_affects_indexes(
        &self,
        table: &str,
        schema: &Schema,
        old_tuple: &Tuple,
        new_tuple: &Tuple,
    ) -> bool {
        // Metadata-only check: no tree locks needed.
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());

        for entry in indexes.values() {
            if entry.table != table {
                continue;
            }

            for column_name in &entry.columns {
                let Some(idx) = schema.get_column_index(column_name) else {
                    return true;
                };
                if old_tuple.values.get(idx) != new_tuple.values.get(idx) {
                    return true;
                }
            }
        }

        false
    }

    /// Return true when any provided column participates in an ART index for
    /// this table. Fast UPDATE paths use this statement-level hint to avoid a
    /// per-row old/new tuple comparison for payload-only updates.
    pub fn columns_affect_indexes(&self, table: &str, column_names: &[String]) -> bool {
        if column_names.is_empty() {
            return false;
        }

        // Metadata-only check: no tree locks needed.
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        for entry in indexes.values() {
            if entry.table != table {
                continue;
            }

            for indexed_column in &entry.columns {
                if column_names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(indexed_column))
                {
                    return true;
                }
            }
        }

        false
    }

    /// Clear all index data for a table without removing the index structures.
    ///
    /// This is used by TRUNCATE TABLE to reset index contents while keeping
    /// the PK/FK/UNIQUE/Manual index registrations intact. After clearing,
    /// the indexes are empty but still exist, so new inserts will correctly
    /// populate them.
    ///
    /// The registry itself is unchanged, so the global lock is taken as READ;
    /// each table tree is write-locked one at a time.
    pub fn clear_table_indexes(&self, table: &str) {
        self.note_mutation();
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        for entry in indexes.values() {
            if entry.table == table {
                let mut tree = entry.tree.write().unwrap_or_else(|e| e.into_inner());
                tree.clear();
            }
        }
    }

    fn int_value_width(value: &Value) -> Option<(i64, usize)> {
        match value {
            Value::Int2(v) => Some((i64::from(*v), 2)),
            Value::Int4(v) => Some((i64::from(*v), 4)),
            Value::Int8(v) => Some((*v, 8)),
            _ => None,
        }
    }

    // =========================================================================
    // STATISTICS
    // =========================================================================

    /// Get manager statistics
    pub fn stats(&self) -> ArtManagerStats {
        self.stats.snapshot()
    }

    /// Get statistics for a specific index
    pub fn index_stats(&self, name: &str) -> Option<ArtIndexStats> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        indexes.get(name).map(|entry| {
            let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
            tree.stats().clone()
        })
    }

    /// Check if a table has foreign key indexes
    pub fn has_fk(&self, table: &str) -> bool {
        let fk_indexes = self.fk_indexes.read().unwrap_or_else(|e| e.into_inner());
        fk_indexes.get(table).is_some_and(|v| !v.is_empty())
    }

    /// Check if a table has a primary key
    pub fn has_pk(&self, table: &str) -> bool {
        let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
        pk_indexes.contains_key(table)
    }

    /// Check if a specific index exists
    pub fn index_exists(&self, name: &str) -> bool {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        indexes.contains_key(name)
    }

    /// The registered kind and owning table of the index called `name`, or
    /// `None` if no index by that name is registered.
    ///
    /// Exists so `DROP INDEX` can REFUSE to touch a `PrimaryKey` / `Unique` /
    /// `ForeignKey` index — dropping one would silently remove constraint
    /// enforcement while every affected INSERT kept reporting success — and can
    /// name the owning table in the refusal the way PostgreSQL does. The
    /// alternative, `list_indexes()`, allocates the whole registry and is
    /// O(indexes); this is a single map probe under the same read lock.
    pub fn index_kind_and_table(&self, name: &str) -> Option<(ArtIndexType, String)> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        indexes.get(name).map(|entry| (entry.index_type, entry.table.clone()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_create_pk_index() {
        let manager = ArtIndexManager::new();

        let result = manager.create_pk_index("users", &["id".to_string()]);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "users_pkey");

        // Duplicate should fail
        let result = manager.create_pk_index("users", &["id".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn test_create_unique_index() {
        let manager = ArtIndexManager::new();

        let result = manager.create_unique_index("users", &["email".to_string()], None);
        assert!(result.is_ok());

        let result = manager.create_unique_index("users", &["username".to_string()], Some("users_username_unique"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "users_username_unique");
    }

    #[test]
    fn test_create_fk_index() {
        let manager = ArtIndexManager::new();

        // Create parent table PK
        manager.create_pk_index("departments", &["id".to_string()]).unwrap();

        // Create FK
        let result = manager.create_fk_index(
            "employees",
            &["dept_id".to_string()],
            "departments",
            &["id".to_string()],
            None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_pk_constraint_check() {
        let manager = ArtIndexManager::new();
        manager.create_pk_index("users", &["id".to_string()]).unwrap();

        // Insert first row
        let mut values = HashMap::new();
        values.insert("id".to_string(), Value::Int8(1));
        manager.check_pk_constraint("users", &[Value::Int8(1)]).unwrap();
        manager.on_insert("users", 1, &values).unwrap();

        // Duplicate should fail
        let result = manager.check_pk_constraint("users", &[Value::Int8(1)]);
        assert!(matches!(result, Err(ArtIndexError::DuplicateKey(_))));

        // Different key should succeed
        let result = manager.check_pk_constraint("users", &[Value::Int8(2)]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_pk_int_range_count_handles_negative_keys() {
        let manager = ArtIndexManager::new();
        manager.create_pk_index("events", &["id".to_string()]).unwrap();

        for (row_id, id) in [(-3_i64), -1, 0, 1, 4].into_iter().enumerate() {
            let mut values = HashMap::new();
            values.insert("id".to_string(), Value::Int8(id));
            manager.on_insert("events", row_id as u64 + 1, &values).unwrap();
        }

        assert_eq!(
            manager.pk_index_count_int_range("events", &DataType::Int8, Some((0, true)), None),
            Some(3)
        );
        assert_eq!(
            manager.pk_index_count_int_range("events", &DataType::Int8, None, Some((0, false))),
            Some(2)
        );
        assert_eq!(
            manager.pk_index_count_int_range("events", &DataType::Int8, Some((-1, true)), Some((1, true))),
            Some(3)
        );
    }

    #[test]
    fn test_dense_pk_int_range_count_stays_exact_after_edge_deletes() {
        let manager = ArtIndexManager::new();
        manager.create_pk_index("events", &["id".to_string()]).unwrap();

        for id in 0_i32..10 {
            let mut values = HashMap::new();
            values.insert("id".to_string(), Value::Int4(id));
            manager.on_insert("events", id as u64 + 1, &values).unwrap();
        }

        assert_eq!(
            manager.pk_index_count_int_range("events", &DataType::Int4, Some((3, true)), None),
            Some(7)
        );
        assert_eq!(
            manager.pk_index_count_int_range("events", &DataType::Int4, Some((2, true)), Some((5, true))),
            Some(4)
        );

        for id in [0_i32, 9] {
            let mut values = HashMap::new();
            values.insert("id".to_string(), Value::Int4(id));
            manager.on_delete("events", id as u64 + 1, &values).unwrap();
        }

        assert_eq!(
            manager.pk_index_count_int_range("events", &DataType::Int4, None, None),
            Some(8)
        );
        assert_eq!(
            manager.pk_index_count_int_range("events", &DataType::Int4, Some((1, true)), Some((8, true))),
            Some(8)
        );
    }

    #[test]
    fn test_dense_pk_int_range_count_falls_back_after_gap_delete() {
        let manager = ArtIndexManager::new();
        manager.create_pk_index("events", &["id".to_string()]).unwrap();

        for id in 0_i32..10 {
            let mut values = HashMap::new();
            values.insert("id".to_string(), Value::Int4(id));
            manager.on_insert("events", id as u64 + 1, &values).unwrap();
        }

        let mut values = HashMap::new();
        values.insert("id".to_string(), Value::Int4(5));
        manager.on_delete("events", 6, &values).unwrap();

        assert_eq!(
            manager.pk_index_count_int_range("events", &DataType::Int4, None, None),
            Some(9)
        );
        assert_eq!(
            manager.pk_index_count_int_range("events", &DataType::Int4, Some((4, true)), Some((6, true))),
            Some(2)
        );
    }

    #[test]
    fn test_unique_constraint_check() {
        let manager = ArtIndexManager::new();
        manager
            .create_unique_index("users", &["email".to_string()], None)
            .unwrap();

        // Insert first row
        let mut values = HashMap::new();
        values.insert("email".to_string(), Value::String("alice@example.com".to_string()));
        manager.check_unique_constraints("users", &values).unwrap();
        manager.on_insert("users", 1, &values).unwrap();

        // Duplicate should fail
        let result = manager.check_unique_constraints("users", &values);
        assert!(matches!(result, Err(ArtIndexError::DuplicateKey(_))));

        // NULL should be allowed
        let mut null_values = HashMap::new();
        null_values.insert("email".to_string(), Value::Null);
        let result = manager.check_unique_constraints("users", &null_values);
        assert!(result.is_ok());
    }

    #[test]
    fn test_tuple_update_affects_indexes_only_for_index_columns() {
        use crate::Column;

        let manager = ArtIndexManager::new();
        manager.create_pk_index("users", &["id".to_string()]).unwrap();
        manager
            .create_unique_index("users", &["email".to_string()], None)
            .unwrap();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4).primary_key(),
            Column::new("email", DataType::Text).unique(),
            Column::new("balance", DataType::Int4),
        ]);

        let old_tuple = Tuple::new(vec![
            Value::Int4(1),
            Value::String("a@example.com".to_string()),
            Value::Int4(10),
        ]);
        let payload_update = Tuple::new(vec![
            Value::Int4(1),
            Value::String("a@example.com".to_string()),
            Value::Int4(11),
        ]);
        let unique_update = Tuple::new(vec![
            Value::Int4(1),
            Value::String("b@example.com".to_string()),
            Value::Int4(10),
        ]);
        let pk_update = Tuple::new(vec![
            Value::Int4(2),
            Value::String("a@example.com".to_string()),
            Value::Int4(10),
        ]);

        assert!(!manager.tuple_update_affects_indexes("users", &schema, &old_tuple, &payload_update));
        assert!(manager.tuple_update_affects_indexes("users", &schema, &old_tuple, &unique_update));
        assert!(manager.tuple_update_affects_indexes("users", &schema, &old_tuple, &pk_update));

        assert!(!manager.columns_affect_indexes("users", &["balance".to_string()]));
        assert!(manager.columns_affect_indexes("users", &["email".to_string()]));
        assert!(manager.columns_affect_indexes("users", &["ID".to_string()]));
    }

    #[test]
    fn test_tuple_backed_insert_constraints_and_index_update() {
        use crate::Column;

        let manager = ArtIndexManager::new();
        manager.create_pk_index("users", &["id".to_string()]).unwrap();
        manager
            .create_unique_index("users", &["email".to_string()], None)
            .unwrap();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4).primary_key(),
            Column::new("email", DataType::Text).unique(),
            Column::new("balance", DataType::Int4),
        ]);
        let tuple = Tuple::new(vec![
            Value::Int4(1),
            Value::String("a@example.com".to_string()),
            Value::Int4(10),
        ]);

        manager
            .check_unique_constraints_tuple("users", &schema, &tuple)
            .unwrap();
        let indexed_values = manager
            .on_insert_tuple_collect_index_values("users", 1, &schema, &tuple, RowState::NotStored)
            .unwrap();
        assert_eq!(indexed_values.len(), 2);
        assert_eq!(indexed_values.get("id"), Some(&Value::Int4(1)));
        assert_eq!(
            indexed_values.get("email"),
            Some(&Value::String("a@example.com".to_string()))
        );
        assert!(!indexed_values.contains_key("balance"));

        let dup_pk = Tuple::new(vec![
            Value::Int4(1),
            Value::String("b@example.com".to_string()),
            Value::Int4(20),
        ]);
        assert!(matches!(
            manager.check_unique_constraints_tuple("users", &schema, &dup_pk),
            Err(ArtIndexError::DuplicateKey(_))
        ));

        let dup_unique = Tuple::new(vec![
            Value::Int4(2),
            Value::String("a@example.com".to_string()),
            Value::Int4(20),
        ]);
        assert!(matches!(
            manager.check_unique_constraints_tuple("users", &schema, &dup_unique),
            Err(ArtIndexError::DuplicateKey(_))
        ));

        let null_unique = Tuple::new(vec![Value::Int4(2), Value::Null, Value::Int4(20)]);
        assert!(manager
            .check_unique_constraints_tuple("users", &schema, &null_unique)
            .is_ok());

        manager.on_delete("users", 1, &indexed_values).unwrap();
        assert!(manager
            .check_unique_constraints_tuple("users", &schema, &dup_pk)
            .is_ok());
    }

    #[test]
    fn test_drop_table_indexes() {
        let manager = ArtIndexManager::new();

        manager.create_pk_index("users", &["id".to_string()]).unwrap();
        manager
            .create_unique_index("users", &["email".to_string()], None)
            .unwrap();

        assert_eq!(manager.stats().total_indexes, 2);

        manager.drop_table_indexes("users").unwrap();

        assert_eq!(manager.stats().total_indexes, 0);
    }

    #[test]
    fn test_list_indexes() {
        let manager = ArtIndexManager::new();

        manager.create_pk_index("users", &["id".to_string()]).unwrap();
        manager
            .create_unique_index("users", &["email".to_string()], None)
            .unwrap();
        manager
            .create_manual_index("users_name_idx", "users", &["name".to_string()])
            .unwrap();

        let indexes = manager.list_indexes();
        assert_eq!(indexes.len(), 3);

        let table_indexes = manager.list_table_indexes("users");
        assert_eq!(table_indexes.len(), 3);
    }

    // W3.4 §3.2: the per-table entry list must stay byte-for-byte identical to
    // a full `indexes`-map filter after every register / drop / rename / clear.
    #[test]
    fn table_indexes_stays_consistent_across_register_drop_rename_clear() {
        // Expected = group the source-of-truth `indexes` map by table.
        fn expected(m: &ArtIndexManager) -> HashMap<String, Vec<String>> {
            let indexes = m.indexes.read().unwrap();
            let mut exp: HashMap<String, Vec<String>> = HashMap::new();
            for (name, entry) in indexes.iter() {
                exp.entry(entry.table.clone()).or_default().push(name.clone());
            }
            for v in exp.values_mut() {
                v.sort();
            }
            exp
        }
        // Actual = the derived `table_indexes` map.
        fn actual(m: &ArtIndexManager) -> HashMap<String, Vec<String>> {
            let ti = m.table_indexes.read().unwrap();
            let mut act: HashMap<String, Vec<String>> = ti.clone();
            for v in act.values_mut() {
                v.sort();
            }
            act
        }

        let m = ArtIndexManager::new();
        assert_eq!(expected(&m), actual(&m));

        // Register all four index kinds, incl. Manual (which the partial
        // pk/fk/unique maps do NOT cover — the whole reason for this map).
        m.create_pk_index("orders", &["id".to_string()]).unwrap();
        m.create_unique_index("orders", &["code".to_string()], None).unwrap();
        m.create_pk_index("cust", &["id".to_string()]).unwrap();
        m.create_fk_index("orders", &["cust_id".to_string()], "cust", &["id".to_string()], None)
            .unwrap();
        m.create_manual_index("orders_total_idx", "orders", &["total".to_string()])
            .unwrap();
        assert_eq!(expected(&m), actual(&m));
        assert!(actual(&m)["orders"].contains(&"orders_total_idx".to_string()));

        // Drop a single (Manual) index.
        m.drop_index("orders_total_idx").unwrap();
        assert_eq!(expected(&m), actual(&m));

        // TRUNCATE (clear): registrations survive → table_indexes UNCHANGED.
        let before_clear = actual(&m);
        m.clear_table_indexes("orders");
        assert_eq!(before_clear, actual(&m));
        assert_eq!(expected(&m), actual(&m));

        // Rename: the whole entry list moves to the new table key.
        m.rename_table_indexes("orders", "orders2", &HashSet::new()).unwrap();
        assert_eq!(expected(&m), actual(&m));
        assert!(!actual(&m).contains_key("orders"));
        assert!(actual(&m).contains_key("orders2"));

        // Drop every index for a table → its key disappears entirely.
        m.drop_table_indexes("orders2").unwrap();
        assert_eq!(expected(&m), actual(&m));
        assert!(!actual(&m).contains_key("orders2"));
        // The untouched parent table's entry is still intact.
        assert_eq!(actual(&m).get("cust"), Some(&vec!["cust_pkey".to_string()]));
    }

    // W3.4 §3.3: the encode-once fragment path must produce BYTE-IDENTICAL keys
    // to `encode_key_from_values` for every DataType, single- and multi-column
    // (ART keys are on-disk-durable via snapshots — a divergence corrupts them).
    #[test]
    fn encode_once_is_byte_identical_to_encode_key_from_values() {
        let samples: Vec<Value> = vec![
            Value::Null,
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Int2(-7),
            Value::Int2(12345),
            Value::Int4(0),
            Value::Int4(i32::MIN),
            Value::Int8(9_000_000_000),
            Value::Float4(-1.5),
            Value::Float4(0.0),
            Value::Float8(3.25),
            Value::String("hello".to_string()),
            Value::String(String::new()),
            // embedded 0x00: single-col (unescaped) vs multi-col (escaped) differ
            Value::String("has\u{0}nul\u{0}bytes".to_string()),
            Value::Bytes(vec![1, 0, 2, 0xFF, 0]),
            Value::Bytes(Vec::new()),
            Value::Numeric("123.45".to_string()),
            Value::Json("{\"k\":1}".to_string()),
            Value::Array(vec![Value::Int4(1), Value::Int4(2)]),
            Value::Array(vec![Value::String("a\u{0}b".to_string())]),
        ];

        // Single-column keys (escape = false).
        for v in &samples {
            let direct = ArtIndexManager::encode_key_from_values(std::iter::once(v));
            let mut cache: Vec<(usize, bool, Vec<u8>)> = Vec::new();
            let cached = ArtIndexManager::encode_key_cached(&mut cache, &[(0usize, v)]);
            assert_eq!(direct, cached, "single-column key mismatch for {:?}", v);
        }

        // Two-column keys (escape = true) — every ordered pair.
        for a in &samples {
            for b in &samples {
                let direct = ArtIndexManager::encode_key_from_values([a, b].iter().copied());
                let mut cache: Vec<(usize, bool, Vec<u8>)> = Vec::new();
                let cached = ArtIndexManager::encode_key_cached(&mut cache, &[(0usize, a), (1usize, b)]);
                assert_eq!(direct, cached, "two-column key mismatch for {:?} + {:?}", a, b);
            }
        }

        // Three columns reusing the SAME column index (shared-fragment reuse
        // must not corrupt later columns).
        for a in &samples {
            let direct = ArtIndexManager::encode_key_from_values([a, a, a].iter().copied());
            let mut cache: Vec<(usize, bool, Vec<u8>)> = Vec::new();
            let cached = ArtIndexManager::encode_key_cached(&mut cache, &[(0usize, a), (0usize, a), (0usize, a)]);
            assert_eq!(direct, cached, "three-column repeated key mismatch for {:?}", a);
        }

        // One column shared by a single-column (escape=false) and a two-column
        // (escape=true) index within one row must cache TWO distinct fragments
        // and stay byte-identical to each direct form.
        let shared = Value::String("x\u{0}y".to_string());
        let other = Value::Int4(7);
        let mut cache: Vec<(usize, bool, Vec<u8>)> = Vec::new();
        let single = ArtIndexManager::encode_key_cached(&mut cache, &[(0usize, &shared)]);
        let composite = ArtIndexManager::encode_key_cached(&mut cache, &[(0usize, &shared), (1usize, &other)]);
        assert_eq!(
            single,
            ArtIndexManager::encode_key_from_values(std::iter::once(&shared))
        );
        assert_eq!(
            composite,
            ArtIndexManager::encode_key_from_values([&shared, &other].iter().copied())
        );
        assert_eq!(
            cache.iter().filter(|(ci, _, _)| *ci == 0).count(),
            2,
            "shared column must cache one unescaped and one escaped fragment"
        );
    }

    // -----------------------------------------------------------------------
    // Prisma P0 — the ON CONFLICT arbiter, per-table metadata lookups, and the
    // RENAME-TABLE name-preservation guard.
    // -----------------------------------------------------------------------

    /// A unique constraint is a SET of columns: case and order do not matter,
    /// but membership does — including for a list that repeats a column, which
    /// must not pass as a same-length set it is not equal to.
    #[test]
    fn column_sets_match_is_case_and_order_insensitive_set_equality() {
        let one = |a: &str| vec![a.to_string()];
        let two = |a: &str, b: &str| vec![a.to_string(), b.to_string()];

        assert!(ArtIndexManager::column_sets_match(&one("v"), &one("V")));
        assert!(!ArtIndexManager::column_sets_match(&one("v"), &one("w")));
        assert!(ArtIndexManager::column_sets_match(&two("v", "w"), &two("W", "V")));
        assert!(!ArtIndexManager::column_sets_match(&two("v", "w"), &one("v")));
        // The repeat case: same length, one-way contained, NOT equal.
        assert!(!ArtIndexManager::column_sets_match(&two("a", "a"), &two("a", "b")));
    }

    /// `find_unique_conflict` must arbitrate on the ON CONFLICT target: the PK
    /// is probed FIRST when there is no arbiter, and NOT AT ALL when the
    /// arbiter names a different column set.
    #[test]
    fn find_unique_conflict_honours_the_arbiter() {
        let m = ArtIndexManager::new();
        m.create_pk_index("oc", &["id".to_string()]).unwrap();
        m.create_unique_index("oc", &["v".to_string()], None).unwrap();

        // Row 1: id = 1, v = 'a'.  Row 2: id = 2, v = 'b'.
        for (row_id, id, v) in [(1u64, 1i32, "a"), (2, 2, "b")] {
            let mut values = HashMap::new();
            values.insert("id".to_string(), Value::Int4(id));
            values.insert("v".to_string(), Value::String(v.to_string()));
            m.on_insert("oc", row_id, &values).unwrap();
        }

        // A row colliding on BOTH: id = 2 (row 2) and v = 'a' (row 1).
        let mut proposed = HashMap::new();
        proposed.insert("id".to_string(), Value::Int4(2));
        proposed.insert("v".to_string(), Value::String("a".to_string()));

        // No arbiter → PK first (row 2), the historical behaviour.
        let any = m.find_unique_conflict("oc", &proposed, None).expect("a conflict");
        assert!(any.is_primary_key, "with no target the PK is reported first");
        assert_eq!(any.row_id, 2);

        // Arbiter (v) → the TARGET's row (row 1), never the PK's.
        let arbiter = vec!["v".to_string()];
        let targeted = m
            .find_unique_conflict("oc", &proposed, Some(&arbiter))
            .expect("a conflict on the arbiter");
        assert!(
            !targeted.is_primary_key,
            "the arbiter names the UNIQUE index, not the PK"
        );
        assert_eq!(targeted.row_id, 1, "*** WRONG ROW *** the arbiter was ignored");

        // Arbiter (v) with a value that does NOT collide on v: no conflict is
        // reported even though the PK collides — the caller re-raises 23505.
        let mut pk_only = HashMap::new();
        pk_only.insert("id".to_string(), Value::Int4(1));
        pk_only.insert("v".to_string(), Value::String("zzz".to_string()));
        assert!(
            m.find_unique_conflict("oc", &pk_only, Some(&arbiter)).is_none(),
            "*** FAIL-OPEN *** a PK conflict was reported for an ON CONFLICT (v) statement"
        );
        assert!(
            m.find_unique_conflict("oc", &pk_only, None).is_some(),
            "the targetless probe must still see the PK conflict"
        );
    }

    /// The per-table metadata predicates must answer from THIS table's indexes
    /// only — the same answers the old global scan gave, without scanning it.
    #[test]
    fn per_table_unique_lookups_ignore_other_tables() {
        let m = ArtIndexManager::new();
        m.create_pk_index("a", &["id".to_string()]).unwrap();
        m.create_unique_index("a", &["email".to_string()], None).unwrap();
        m.create_pk_index("b", &["id".to_string()]).unwrap();
        m.create_manual_index("b_note_idx", "b", &["note".to_string()]).unwrap();

        assert!(m.has_unique_index_on("a", &["email".to_string()]));
        assert!(!m.has_unique_index_on("b", &["email".to_string()]));
        assert!(m.column_in_unique_index("a", "EMAIL"));
        assert!(
            !m.column_in_unique_index("b", "note"),
            "a Manual index is not a constraint"
        );
        assert!(m.column_in_unique_index("b", "id"));

        let mut sets = m.unique_column_sets("a");
        sets.sort();
        assert_eq!(sets, vec![vec!["email".to_string()], vec!["id".to_string()]]);
        assert_eq!(m.unique_column_sets("nosuch"), Vec::<Vec<String>>::new());
    }

    /// RENAME TABLE: a PRESERVED index (one with a durable `meta:index:` record)
    /// keeps its NAME while its entry follows the table; an auto-generated
    /// constraint index is renamed into the new table's namespace so the old
    /// name stops squatting the global registry.
    #[test]
    fn rename_table_indexes_preserves_recorded_names() {
        let m = ArtIndexManager::new();
        m.create_pk_index("Account", &["id".to_string()]).unwrap();
        m.create_unique_index("Account", &["email".to_string()], Some("Account_email_key"))
            .unwrap();

        let mut preserve = HashSet::new();
        preserve.insert("Account_email_key".to_string());
        m.rename_table_indexes("Account", "Account2", &preserve).unwrap();

        // The user index kept its name and moved to the new table…
        assert_eq!(
            m.index_kind_and_table("Account_email_key"),
            Some((ArtIndexType::Unique, "Account2".to_string())),
            "a recorded index must keep its name and follow the table"
        );
        // …and is still resolvable through the per-table maps, so it still
        // enforces (`check_unique_constraints` resolves names through them).
        assert!(m.column_in_unique_index("Account2", "email"));
        // The generated PK index moved into the new namespace.
        assert!(
            m.index_exists("Account2_pkey"),
            "the generated PK index was not renamed"
        );
        assert!(!m.index_exists("Account_pkey"), "the old generated name still squats");
    }

    // -----------------------------------------------------------------------
    // `RowState::NotStored`: an INSERT that is REFUSED and UNWOUND must leave
    // no key of its own behind — and a committed DELETE must still be swept out
    // of every index.
    //
    // This is the ONLY state the undo is correct in. The `Stored` half of the
    // contract (the row is written anyway, so nothing is ever taken back) is
    // pinned further down, and again — through the public API, so it can be run
    // against an unfixed tree — in `tests/prisma_p0_unique_on_conflict.rs`.
    // -----------------------------------------------------------------------

    /// Drive the shared row funnel with the row state spelled out.
    ///
    /// `on_insert_tuple` is hard-wired to [`RowState::Stored`] (its only caller
    /// has already written the row), so the `NotStored` arm of
    /// `insert_row_indexes` is reached in production only through the
    /// transactional twin — and here, directly, which is the point: the
    /// parameter, not the entry point, is what decides.
    fn insert_row_with_state(
        m: &ArtIndexManager,
        table: &str,
        row_id: RowId,
        schema: &Schema,
        tuple: &Tuple,
        row_state: RowState,
    ) -> ArtResult<()> {
        let names = {
            let table_indexes = m.table_indexes.read().unwrap();
            table_indexes.get(table).cloned().unwrap_or_default()
        };
        let indexes = m.indexes.read().unwrap();
        let mut frag_cache = Vec::new();
        ArtIndexManager::insert_row_indexes(
            &indexes,
            &names,
            row_id,
            schema,
            tuple,
            &mut frag_cache,
            false,
            row_state,
        )
    }

    /// A three-index table whose MIDDLE index refuses the row.
    ///
    /// Registration order IS the visit order (`table_indexes` is a push-list),
    /// so `pk(id)`, `unique(v)`, `unique(w)` are visited in that order and a row
    /// that collides only on `v` fails in the middle of the loop.
    ///
    /// [`RowState::NotStored`], stated explicitly at the call: the row is being
    /// REJECTED and unwound, so it never becomes a stored row and every entry
    /// written for it is a phantom that nothing will ever clean up (there is no
    /// tuple whose DELETE would remove it). Two failure modes have to stay dead:
    ///   * carrying on past the refusal — the entry for `w = 'q'` would name a
    ///     row id that holds nothing, and the next real row with `w = 'q'` would
    ///     be rejected as a duplicate of it;
    ///   * keeping the entry the PK index took BEFORE the refusal — same
    ///     poisoning, one index earlier (`id = 2` would be permanently taken).
    ///
    /// So: the refusal is reported, and the table's ART is byte-for-byte what it
    /// was before the attempt.
    #[test]
    fn a_refused_insert_leaves_no_entry_in_any_index() {
        use crate::Column;

        let m = ArtIndexManager::new();
        m.create_pk_index("mid", &["id".to_string()]).unwrap();
        m.create_unique_index("mid", &["v".to_string()], None).unwrap();
        m.create_unique_index("mid", &["w".to_string()], None).unwrap();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4).primary_key(),
            Column::new("v", DataType::Text).unique(),
            Column::new("w", DataType::Text).unique(),
        ]);
        let first = Tuple::new(vec![
            Value::Int4(1),
            Value::String("a".to_string()),
            Value::String("p".to_string()),
        ]);
        insert_row_with_state(&m, "mid", 1, &schema, &first, RowState::NotStored).unwrap();

        // Collides on the MIDDLE index (v = 'a') and on nothing else.
        let second = Tuple::new(vec![
            Value::Int4(2),
            Value::String("a".to_string()),
            Value::String("q".to_string()),
        ]);
        let err = insert_row_with_state(&m, "mid", 2, &schema, &second, RowState::NotStored)
            .expect_err("the duplicate on `v` must be reported");
        assert!(
            matches!(err, ArtIndexError::DuplicateKey(_)),
            "the reported error must be the duplicate, got {err:?}"
        );

        // The index visited BEFORE the refusal gave its entry back…
        assert!(
            !m.unique_key_exists("mid", &["id".to_string()], &[Value::Int4(2)]),
            "*** ART POISONED *** the PK entry of a row that was never stored survived: \
             id = 2 is now permanently taken by nothing"
        );
        // …and the index AFTER it was never touched.
        assert!(
            !m.unique_key_exists("mid", &["w".to_string()], &[Value::String("q".to_string())]),
            "*** ART POISONED *** the rejected row's key was inserted into the index after \
             the failing one: w = 'q' is now taken by a row that does not exist"
        );

        // The row that IS stored is untouched by the failed attempt.
        assert!(m.unique_key_exists("mid", &["id".to_string()], &[Value::Int4(1)]));
        assert!(m.unique_key_exists("mid", &["v".to_string()], &[Value::String("a".to_string())]));
        assert!(m.unique_key_exists("mid", &["w".to_string()], &[Value::String("p".to_string())]));

        // And the proof that the undo restored a USABLE state, not just an
        // empty-looking one: the keys the rejected row had claimed are still
        // free for a legitimate row.
        let third = Tuple::new(vec![
            Value::Int4(2),
            Value::String("b".to_string()),
            Value::String("q".to_string()),
        ]);
        insert_row_with_state(&m, "mid", 2, &schema, &third, RowState::NotStored)
            .expect("a legitimate row must not collide with the phantom keys of a rejected one");
        assert!(m.unique_key_exists("mid", &["w".to_string()], &[Value::String("q".to_string())]));
    }

    /// The same rule for the buffered/transactional insert twin
    /// (`on_insert_tuple_collect_index_values` with [`RowState::NotStored`]) —
    /// the one PRODUCTION path that unwinds the row
    /// (`insert_prepared_tuple_in_transaction` with
    /// `constraints_prechecked = false`, whose `Err` arm is
    /// `return Err(Error::constraint_violation(…))`). A key left behind there
    /// belongs to a row that never existed.
    #[test]
    fn a_refused_insert_leaves_no_entry_in_any_index_collect_twin() {
        use crate::Column;

        let m = ArtIndexManager::new();
        m.create_pk_index("midc", &["id".to_string()]).unwrap();
        m.create_unique_index("midc", &["v".to_string()], None).unwrap();
        m.create_unique_index("midc", &["w".to_string()], None).unwrap();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4).primary_key(),
            Column::new("v", DataType::Text).unique(),
            Column::new("w", DataType::Text).unique(),
        ]);
        let first = Tuple::new(vec![
            Value::Int4(1),
            Value::String("a".to_string()),
            Value::String("p".to_string()),
        ]);
        m.on_insert_tuple_collect_index_values("midc", 1, &schema, &first, RowState::NotStored)
            .unwrap();

        let second = Tuple::new(vec![
            Value::Int4(2),
            Value::String("a".to_string()),
            Value::String("q".to_string()),
        ]);
        let err = m
            .on_insert_tuple_collect_index_values("midc", 2, &schema, &second, RowState::NotStored)
            .expect_err("the duplicate on `v` must be reported");
        assert!(matches!(err, ArtIndexError::DuplicateKey(_)), "got {err:?}");

        assert!(
            !m.unique_key_exists("midc", &["id".to_string()], &[Value::Int4(2)]),
            "*** ART POISONED *** the PK entry taken before the refusal was not given back"
        );
        assert!(
            !m.unique_key_exists("midc", &["w".to_string()], &[Value::String("q".to_string())]),
            "*** ART POISONED *** the rejected row's key was inserted into the index after \
             the failing one"
        );
        assert!(m.unique_key_exists("midc", &["id".to_string()], &[Value::Int4(1)]));
        assert!(m.unique_key_exists("midc", &["w".to_string()], &[Value::String("p".to_string())]));

        let third = Tuple::new(vec![
            Value::Int4(2),
            Value::String("b".to_string()),
            Value::String("q".to_string()),
        ]);
        m.on_insert_tuple_collect_index_values("midc", 2, &schema, &third, RowState::NotStored)
            .expect("a legitimate row must not collide with the phantom keys of a rejected one");
    }

    /// `on_delete_tuple` traverses EVERY index of the table, including the ones
    /// registered after an index that had nothing to remove.
    ///
    /// NOTE on coverage: the delete twin's mid-loop `?` fired on
    /// `AdaptiveRadixTree::remove`, whose only error arms are
    /// `ArtIndexError::Internal` corruption reports ("Missing root node",
    /// "Inconsistent NodeN child") — unreachable through any public API, so this
    /// cannot induce the failure the way the insert tests above do. It pins the
    /// traversal itself (every entry gone, including the last index's), which is
    /// what the `?` removal preserves; the fix is otherwise defensive, and
    /// matches the rule its `HashMap` twin `on_delete` already documents.
    #[test]
    fn on_delete_tuple_visits_every_index() {
        use crate::Column;

        let m = ArtIndexManager::new();
        m.create_pk_index("del", &["id".to_string()]).unwrap();
        m.create_unique_index("del", &["v".to_string()], None).unwrap();
        m.create_unique_index("del", &["w".to_string()], None).unwrap();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4).primary_key(),
            Column::new("v", DataType::Text).unique(),
            Column::new("w", DataType::Text).unique(),
        ]);
        let row = Tuple::new(vec![
            Value::Int4(1),
            Value::String("a".to_string()),
            Value::String("p".to_string()),
        ]);
        m.on_insert_tuple("del", 1, &schema, &row).unwrap();

        m.on_delete_tuple("del", 1, &schema, &row).unwrap();

        assert!(!m.unique_key_exists("del", &["id".to_string()], &[Value::Int4(1)]));
        assert!(!m.unique_key_exists("del", &["v".to_string()], &[Value::String("a".to_string())]));
        assert!(
            !m.unique_key_exists("del", &["w".to_string()], &[Value::String("p".to_string())]),
            "the LAST index still holds an entry for the deleted row"
        );
    }

    // -----------------------------------------------------------------------
    // The UPDATE-shaped unique probe selects its index the same way the
    // ON CONFLICT arbiter does.
    // -----------------------------------------------------------------------

    /// `unique_key_taken_by_other_row` used to select its index by EXACT
    /// `Vec<String>` equality while `find_unique_conflict` used
    /// `column_sets_match`. The callers spell the columns as the CONSTRAINT
    /// RECORD (or the schema) spells them, not as the index was registered, so a
    /// constraint recorded with a different CASE or a different ORDER matched no
    /// index at all and the probe silently found nothing — fail-open on UPDATE.
    ///
    /// Re-ordering also has to reach the KEY: an ART key is the ordered
    /// concatenation of its columns, so a `(w, v)`-spelled probe has to be
    /// re-encoded in the index's `(v, w)` order or it looks up a key the tree
    /// can never hold.
    #[test]
    fn unique_key_taken_by_other_row_matches_a_differently_spelled_column_set() {
        let m = ArtIndexManager::new();
        m.create_unique_index("cs", &["v".to_string(), "w".to_string()], Some("cs_vw_key"))
            .unwrap();

        let mut owned = HashMap::new();
        owned.insert("v".to_string(), Value::String("a".to_string()));
        owned.insert("w".to_string(), Value::String("p".to_string()));
        m.on_insert("cs", 1, &owned).unwrap();

        let vw = vec!["v".to_string(), "w".to_string()];
        let wv = vec!["w".to_string(), "v".to_string()];
        let upper = vec!["V".to_string(), "W".to_string()];
        let taken = [Value::String("a".to_string()), Value::String("p".to_string())];
        let taken_swapped = [Value::String("p".to_string()), Value::String("a".to_string())];

        assert!(
            m.unique_key_taken_by_other_row("cs", &vw, &taken, None),
            "the exact spelling must still match"
        );
        assert!(
            m.unique_key_taken_by_other_row("cs", &upper, &taken, None),
            "*** FAIL-OPEN *** a constraint recorded in a different CASE matched no index"
        );
        assert!(
            m.unique_key_taken_by_other_row("cs", &wv, &taken_swapped, None),
            "*** FAIL-OPEN *** a constraint recorded in a different ORDER matched no index"
        );

        // A key nobody owns stays free in every spelling — the re-encode must
        // not smear one column's value onto another.
        let free = [Value::String("a".to_string()), Value::String("q".to_string())];
        let free_swapped = [Value::String("q".to_string()), Value::String("a".to_string())];
        assert!(!m.unique_key_taken_by_other_row("cs", &vw, &free, None));
        assert!(!m.unique_key_taken_by_other_row("cs", &wv, &free_swapped, None));
        // `(p, a)` read in the index's own order is NOT the stored `(a, p)`.
        assert!(!m.unique_key_taken_by_other_row("cs", &vw, &taken_swapped, None));

        // Excluding the owning row frees the key — in every spelling.
        assert!(!m.unique_key_taken_by_other_row("cs", &vw, &taken, Some(1)));
        assert!(!m.unique_key_taken_by_other_row("cs", &upper, &taken, Some(1)));
        assert!(!m.unique_key_taken_by_other_row("cs", &wv, &taken_swapped, Some(1)));
        // …but excluding a DIFFERENT row does not.
        assert!(m.unique_key_taken_by_other_row("cs", &upper, &taken, Some(99)));

        // A column set that is not this index's set still matches nothing.
        let v_only = ["v".to_string()];
        assert!(!m.unique_key_taken_by_other_row("cs", &v_only, &taken[..1], None));
    }

    // -----------------------------------------------------------------------
    // NULLs are DISTINCT under PK/UNIQUE (PostgreSQL `NULLS DISTINCT`)
    // -----------------------------------------------------------------------

    /// A NULL-bearing key must never reach an enforcing tree — through EVERY
    /// maintenance funnel.
    ///
    /// `Value::Null` encodes to the single byte `0x00`, so all NULLs in one
    /// column share a key. Offering that key to a PK/UNIQUE tree makes the
    /// second NULL row a "duplicate", which the all-or-nothing undo then
    /// punishes by stripping the row of the entries it legitimately owns.
    /// This pins the skip at the level it lives on, for all four funnels:
    /// `insert_row_indexes` (via `on_insert_tuple`), the batch twin
    /// (`on_insert_tuples`), the buffered/txn twin
    /// (`on_insert_tuple_collect_index_values`) and the `HashMap` shape
    /// (`on_insert`, which is also `on_update`'s re-insert half).
    ///
    /// FAILS on the pre-fix tree: every second NULL row comes back as
    /// `DuplicateKey` — and on the tree where the undo still fired for a row
    /// that was being stored, the row's PRIMARY KEY entry had been taken back by
    /// the time the caller saw it. Both halves of that (the skip, and the undo
    /// only on [`RowState::NotStored`]) are what keep this green.
    #[test]
    fn a_null_key_is_never_offered_to_an_enforcing_tree() {
        use crate::Column;

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4).primary_key(),
            Column::new("v", DataType::Text).unique(),
        ]);
        let row = |id: i32| Tuple::new(vec![Value::Int4(id), Value::Null]);

        // --- tuple funnel (`on_insert_tuple` -> `insert_row_indexes`) --------
        let m = ArtIndexManager::new();
        m.create_pk_index("nt", &["id".to_string()]).unwrap();
        m.create_unique_index("nt", &["v".to_string()], None).unwrap();
        for id in 1..=3 {
            m.on_insert_tuple("nt", id as u64, &schema, &row(id))
                .unwrap_or_else(|e| panic!("NULL row {id} refused by the tuple funnel: {e}"));
            assert!(
                m.unique_key_exists("nt", &["id".to_string()], &[Value::Int4(id)]),
                "*** ROW UNINDEXED *** the PRIMARY KEY entry of NULL row {id} is missing"
            );
        }
        // A real duplicate on the same index is still refused.
        let dup = Tuple::new(vec![Value::Int4(9), Value::String("a".to_string())]);
        m.on_insert_tuple("nt", 9, &schema, &dup).unwrap();
        let dup2 = Tuple::new(vec![Value::Int4(10), Value::String("a".to_string())]);
        assert!(
            matches!(
                m.on_insert_tuple("nt", 10, &schema, &dup2),
                Err(ArtIndexError::DuplicateKey(_))
            ),
            "a genuine duplicate must still be refused — the skip is about NULL, not about the column"
        );

        // --- buffered/transactional twin ------------------------------------
        let m = ArtIndexManager::new();
        m.create_pk_index("nc", &["id".to_string()]).unwrap();
        m.create_unique_index("nc", &["v".to_string()], None).unwrap();
        for id in 1..=3 {
            m.on_insert_tuple_collect_index_values("nc", id as u64, &schema, &row(id), RowState::NotStored)
                .unwrap_or_else(|e| panic!("NULL row {id} refused by the transactional funnel: {e}"));
            assert!(
                m.unique_key_exists("nc", &["id".to_string()], &[Value::Int4(id)]),
                "*** ROW UNINDEXED *** the PRIMARY KEY entry of NULL row {id} is missing"
            );
        }

        // --- `HashMap` shape (also `on_update`'s re-insert half) ------------
        let m = ArtIndexManager::new();
        m.create_pk_index("nh", &["id".to_string()]).unwrap();
        m.create_unique_index("nh", &["v".to_string()], None).unwrap();
        for id in 1..=3 {
            let mut cv = HashMap::new();
            cv.insert("id".to_string(), Value::Int4(id));
            cv.insert("v".to_string(), Value::Null);
            m.on_insert("nh", id as u64, &cv)
                .unwrap_or_else(|e| panic!("NULL row {id} refused by the HashMap funnel: {e}"));
            assert!(m.unique_key_exists("nh", &["id".to_string()], &[Value::Int4(id)]));
        }
        // …and the delete half is symmetric: removing a NULL row must not
        // disturb anything (there was never a key to remove).
        let mut cv = HashMap::new();
        cv.insert("id".to_string(), Value::Int4(2));
        cv.insert("v".to_string(), Value::Null);
        m.on_delete("nh", 2, &cv).unwrap();
        assert!(
            m.unique_key_exists("nh", &["id".to_string()], &[Value::Int4(1)])
                && m.unique_key_exists("nh", &["id".to_string()], &[Value::Int4(3)]),
            "deleting a NULL-bearing row disturbed the other rows' entries"
        );
        assert!(!m.unique_key_exists("nh", &["id".to_string()], &[Value::Int4(2)]));

        // --- the UPDATE probe -----------------------------------------------
        // `UPDATE … SET v = NULL` when another row already holds NULL: the
        // probe must report the key as FREE (NULLs never conflict).
        assert!(
            !m.unique_key_taken_by_other_row("nh", &["v".to_string()], &[Value::Null], Some(3)),
            "*** SPURIOUS 23505 *** a NULL was reported as taken by another row"
        );
        assert!(!m.unique_key_taken_by_other_row("nh", &["v".to_string()], &[Value::Null], None));
    }

    /// Composite constraints follow the same rule: ANY NULL component makes the
    /// whole key non-enforcing (PostgreSQL's default `NULLS DISTINCT` on a
    /// multi-column unique index), while a fully non-NULL duplicate still
    /// rejects.
    #[test]
    fn a_composite_key_with_one_null_component_is_also_distinct() {
        use crate::Column;

        let m = ArtIndexManager::new();
        m.create_pk_index("cnp", &["id".to_string()]).unwrap();
        m.create_unique_index("cnp", &["v".to_string(), "w".to_string()], Some("cnp_vw_key"))
            .unwrap();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4).primary_key(),
            Column::new("v", DataType::Text),
            Column::new("w", DataType::Text),
        ]);
        let half_null = |id: i32| Tuple::new(vec![Value::Int4(id), Value::String("a".to_string()), Value::Null]);

        m.on_insert_tuple("cnp", 1, &schema, &half_null(1)).unwrap();
        m.on_insert_tuple("cnp", 2, &schema, &half_null(2))
            .expect("(a, NULL) twice is legal — one NULL component makes the key distinct");
        assert!(
            m.unique_key_exists("cnp", &["id".to_string()], &[Value::Int4(2)]),
            "*** ROW UNINDEXED *** the second (a, NULL) row lost its PRIMARY KEY entry"
        );

        // Fully non-NULL: still enforced.
        let full = |id: i32| {
            Tuple::new(vec![
                Value::Int4(id),
                Value::String("a".to_string()),
                Value::String("p".to_string()),
            ])
        };
        m.on_insert_tuple("cnp", 3, &schema, &full(3)).unwrap();
        assert!(
            matches!(
                m.on_insert_tuple("cnp", 4, &schema, &full(4)),
                Err(ArtIndexError::DuplicateKey(_))
            ),
            "a fully non-NULL composite duplicate must still be refused"
        );
        // …and the refusal cost row 4 exactly the entry it was refused.
        // `on_insert_tuple` is a `RowState::Stored` funnel (its caller has
        // already written the row), so the row KEEPS its primary key: taking it
        // back would hide a stored row from `WHERE id = 4` and leave the key
        // free for a second row to claim.
        assert!(
            m.unique_key_exists("cnp", &["id".to_string()], &[Value::Int4(4)]),
            "*** ROW UNINDEXED *** the stored row lost its PRIMARY KEY entry to a refusal on another index"
        );
        // The composite entry still belongs to the row that got there first.
        assert!(m.unique_key_exists("cnp", &["id".to_string()], &[Value::Int4(3)]));
        assert!(matches!(
            m.on_insert_tuple("cnp", 5, &schema, &full(5)),
            Err(ArtIndexError::DuplicateKey(_))
        ));
    }

    // -----------------------------------------------------------------------
    // `RowState::Stored`: the row is written whatever the ART answers, so a
    // refusal may cost it the ONE entry it was refused and NOTHING else.
    //
    // Three callers are in this state: `StorageEngine::insert_tuple_fast`
    // (`put()` then `on_insert_tuple`), `insert_prepared_tuples_fast_batch`
    // (COPY, `on_insert_tuples`, rows already committed) and the transactional
    // twin when `constraints_prechecked` is true (the caller writes the row
    // whatever comes back). All three pre-check PK/UNIQUE first, so a refusal
    // here means a duplicate raced past that check — the row is stored, and the
    // refusal has to be reported, not swallowed.
    // -----------------------------------------------------------------------

    /// `insert_tuple_fast`'s contract, modelled exactly (was
    /// `a_swallowed_refusal_costs_only_the_refused_row`, which asserted the
    /// OPPOSITE — that the stored row lost every entry including its primary
    /// key).
    ///
    /// The row is durable by the time `on_insert_tuple` runs, so the undo is not
    /// merely unnecessary, it is destructive: it takes the PRIMARY KEY entry of
    /// a row that stays in the heap. That row is then countable by a full scan,
    /// invisible to `SELECT … WHERE id = 3`, and its primary key is FREE — the
    /// next INSERT with `id = 3` is accepted and the table holds two rows with
    /// the same primary key.
    ///
    /// So the contract asserted here is: the refused row keeps its PK entry and
    /// every other unique entry, only the entry it was REFUSED is absent (and it
    /// belongs to the row that claimed the value first, which is still enforced
    /// against the next writer), the other rows are untouched, and the refusal
    /// comes back as an error the caller must report.
    #[test]
    fn a_stored_row_keeps_every_entry_but_the_refused_one() {
        use crate::Column;

        let m = ArtIndexManager::new();
        m.create_pk_index("pf", &["id".to_string()]).unwrap();
        m.create_unique_index("pf", &["v".to_string()], None).unwrap();
        m.create_unique_index("pf", &["w".to_string()], None).unwrap();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4).primary_key(),
            Column::new("v", DataType::Text).unique(),
            Column::new("w", DataType::Text).unique(),
        ]);
        let mk = |id: i32, v: &str, w: &str| {
            Tuple::new(vec![
                Value::Int4(id),
                Value::String(v.to_string()),
                Value::String(w.to_string()),
            ])
        };

        m.on_insert_tuple("pf", 1, &schema, &mk(1, "a", "p")).unwrap();
        m.on_insert_tuple("pf", 2, &schema, &mk(2, "b", "q")).unwrap();

        // Row 3 is STORED (its `put()` is above this call in `insert_tuple_fast`)
        // and collides with row 1 on the MIDDLE index.
        let err = m
            .on_insert_tuple("pf", 3, &schema, &mk(3, "a", "r"))
            .expect_err("the duplicate on `v` must be reported to the caller, not dropped");
        assert!(matches!(err, ArtIndexError::DuplicateKey(_)), "got {err:?}");
        let text = err.to_string();
        assert!(
            text.contains("row 3"),
            "the report must name the row an operator has to go and look at, got: {text}"
        );

        // The entry taken BEFORE the refusal is kept: the stored row is still
        // reachable by its primary key.
        assert!(
            m.unique_key_exists("pf", &["id".to_string()], &[Value::Int4(3)]),
            "*** ROW UNINDEXED *** the stored row lost its PRIMARY KEY entry to the undo: a full scan \
             still counts it, `WHERE id = 3` cannot find it, and `id = 3` is free for a second row"
        );
        // The indexes AFTER the refusing one are still maintained.
        assert!(
            m.unique_key_exists("pf", &["w".to_string()], &[Value::String("r".to_string())]),
            "*** ROW UNINDEXED *** the stored row is not findable by `w`, an index that never refused it"
        );
        // Only the refused entry is missing — and it still belongs to row 1.
        assert!(m.unique_key_exists("pf", &["v".to_string()], &[Value::String("a".to_string())]));
        assert!(
            !m.unique_key_taken_by_other_row("pf", &["v".to_string()], &[Value::String("a".to_string())], Some(1)),
            "the `v = 'a'` entry must still be row 1's — it was there first"
        );

        // Everyone else is untouched.
        for (id, v, w) in [(1, "a", "p"), (2, "b", "q")] {
            assert!(
                m.unique_key_exists("pf", &["id".to_string()], &[Value::Int4(id)])
                    && m.unique_key_exists("pf", &["v".to_string()], &[Value::String(v.to_string())])
                    && m.unique_key_exists("pf", &["w".to_string()], &[Value::String(w.to_string())]),
                "*** COLLATERAL DAMAGE *** row {id} lost an entry to another row's refusal"
            );
        }

        // The next writer of the refused VALUE is still rejected (the winner's
        // entry was never taken away)…
        assert!(
            matches!(
                m.on_insert_tuple("pf", 5, &schema, &mk(5, "a", "s")),
                Err(ArtIndexError::DuplicateKey(_))
            ),
            "*** UNENFORCED CONSTRAINT *** `v = 'a'` was handed out a third time"
        );
        // …and the next writer of the stored row's PRIMARY KEY is too.
        assert!(
            matches!(
                m.on_insert_tuple("pf", 6, &schema, &mk(3, "z", "t")),
                Err(ArtIndexError::DuplicateKey(_))
            ),
            "*** DUPLICATE PRIMARY KEY *** `id = 3` was accepted a second time: the stored row's PK entry \
             is missing from the index"
        );
        // The values the refused row does hold are NOT free either.
        assert!(
            matches!(
                m.on_insert_tuple("pf", 7, &schema, &mk(8, "y", "r")),
                Err(ArtIndexError::DuplicateKey(_))
            ),
            "*** UNENFORCED CONSTRAINT *** `w = 'r'` is held by the stored row and was handed out again"
        );
    }

    /// The same contract for the COPY batch funnel
    /// (`insert_prepared_tuples_fast_batch` -> `on_insert_tuples`): the rows are
    /// COMMITTED before ART maintenance runs, so a refused row keeps everything
    /// but the entry it was refused, the rows around it keep everything, and the
    /// refusal is surfaced (ERROR per row, first one returned) instead of being
    /// dropped at `debug!`.
    ///
    /// The previous version of this test asserted the opposite for the refused
    /// row itself — that it lost its PRIMARY KEY entry too, i.e. that a
    /// COMMITTED COPY row could end up unreachable by `WHERE pk = …` with its
    /// key free for the next INSERT to claim. "Costs only itself" now means what
    /// it says: one entry, on the one constraint it collided on.
    #[test]
    fn a_refused_row_inside_a_batch_costs_only_itself() {
        use crate::Column;

        let m = ArtIndexManager::new();
        m.create_pk_index("bt", &["id".to_string()]).unwrap();
        m.create_unique_index("bt", &["v".to_string()], None).unwrap();
        m.create_unique_index("bt", &["w".to_string()], None).unwrap();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4).primary_key(),
            Column::new("v", DataType::Text).unique(),
            Column::new("w", DataType::Text).unique(),
        ]);
        let mk = |id: i32, v: &str, w: &str| {
            Tuple::new(vec![
                Value::Int4(id),
                Value::String(v.to_string()),
                Value::String(w.to_string()),
            ])
        };

        // Row 2 collides with row 1 on `v`; rows 1 and 3 are clean. Every row of
        // the batch is already durable.
        let rows = vec![
            (1u64, mk(1, "a", "p")),
            (2u64, mk(2, "a", "q")),
            (3u64, mk(3, "b", "s")),
        ];
        let outcome = m.on_insert_tuples("bt", &schema, &rows);

        // The clean rows are FULLY indexed — the refusal did not stop the loop
        // and did not take anything from them.
        for (id, v, w) in [(1, "a", "p"), (3, "b", "s")] {
            assert!(
                m.unique_key_exists("bt", &["id".to_string()], &[Value::Int4(id)])
                    && m.unique_key_exists("bt", &["v".to_string()], &[Value::String(v.to_string())])
                    && m.unique_key_exists("bt", &["w".to_string()], &[Value::String(w.to_string())]),
                "*** COLLATERAL DAMAGE *** batch row {id} is missing an entry"
            );
        }

        // The refused row is COMMITTED, so it keeps its primary key and the
        // entry of the index that never refused it.
        assert!(
            m.unique_key_exists("bt", &["id".to_string()], &[Value::Int4(2)]),
            "*** ROW UNINDEXED *** the committed batch row lost its PRIMARY KEY entry: a full scan still \
             counts it, `WHERE id = 2` cannot find it, and `id = 2` is free for a second row"
        );
        assert!(
            m.unique_key_exists("bt", &["w".to_string()], &[Value::String("q".to_string())]),
            "*** ROW UNINDEXED *** the committed batch row is not findable by `w`, which never refused it"
        );
        // Only the refused entry is absent, and it is row 1's.
        assert!(
            !m.unique_key_taken_by_other_row("bt", &["v".to_string()], &[Value::String("a".to_string())], Some(1)),
            "the `v = 'a'` entry must still be row 1's"
        );

        // A later writer of any of those values is rejected: nothing was freed.
        for (id, v, w, what) in [
            (4, "c", "q", "`w = 'q'`, held by the committed batch row"),
            (2, "d", "z", "`id = 2`, the committed batch row's PRIMARY KEY"),
            (5, "a", "y", "`v = 'a'`, held by batch row 1"),
        ] {
            assert!(
                matches!(
                    m.on_insert_tuple("bt", 90 + id as u64, &schema, &mk(id, v, w)),
                    Err(ArtIndexError::DuplicateKey(_))
                ),
                "*** UNENFORCED CONSTRAINT *** {what} was handed out again"
            );
        }

        // And the batch SURFACED the refusal instead of dropping it at `debug!`:
        // the caller has to be able to tell the operator a duplicate is stored.
        let err = outcome.expect_err("a per-row refusal in a committed batch must be reported");
        let text = err.to_string();
        assert!(
            text.contains("row 2"),
            "the report must name the refused row, got: {text}"
        );
    }

    /// The batch funnel with NULLs: a COPY of many rows carrying NULL in a
    /// nullable UNIQUE column must index every one of them by primary key.
    ///
    /// FAILS on the pre-fix tree: rows 2..n are refused on the (encoded NULL)
    /// UNIQUE key and the undo takes their PRIMARY KEY entries back, so a COPY
    /// leaves most of its rows invisible to indexed lookups.
    #[test]
    fn a_batch_of_null_rows_is_fully_indexed() {
        use crate::Column;

        let m = ArtIndexManager::new();
        m.create_pk_index("bn", &["id".to_string()]).unwrap();
        m.create_unique_index("bn", &["v".to_string()], None).unwrap();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4).primary_key(),
            Column::new("v", DataType::Text).unique(),
        ]);
        let rows: Vec<(RowId, Tuple)> = (1..=5)
            .map(|id| (id as u64, Tuple::new(vec![Value::Int4(id), Value::Null])))
            .collect();
        m.on_insert_tuples("bn", &schema, &rows).unwrap();

        for id in 1..=5 {
            assert!(
                m.unique_key_exists("bn", &["id".to_string()], &[Value::Int4(id)]),
                "*** ROW UNINDEXED *** batch NULL row {id} lost its PRIMARY KEY entry"
            );
        }
    }
}
