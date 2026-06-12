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
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

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
/// 4. The name maps (`pk_indexes`, `fk_indexes`, `fk_info`, `unique_indexes`)
///    are leaf locks: clone the names you need and release them before
///    taking the global map lock or any tree lock.
/// 5. Per-table filtering uses the metadata cached in [`IndexEntry`]
///    (no tree lock needed). The per-table name maps do not cover Manual
///    (plain secondary) indexes, so iteration loops filter the entry map by
///    `entry.table` instead of relying on those maps.
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
            (u64::from_be_bytes([
                key[0], key[1], key[2], key[3], key[4], key[5], key[6], key[7],
            ]) ^ 0x8000_0000_0000_0000) as i64,
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

    // =========================================================================
    // INDEX REMOVAL
    // =========================================================================

    /// Drop an index by name
    pub fn drop_index(&self, name: &str) -> ArtResult<()> {
        self.note_mutation();
        let index_type;

        // Remove from main index map
        {
            let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = indexes.remove(name) {
                index_type = entry.index_type;
            } else {
                return Err(ArtIndexError::IndexNotFound(name.to_string()));
            }
        }

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

    /// Rename all indexes for a table (called on RENAME TABLE)
    pub fn rename_table_indexes(&self, old_table: &str, new_table: &str) -> ArtResult<()> {
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
                // Generate new index name by replacing table name
                let new_name = old_name
                    .replace(&format!("_{}_", old_table), &format!("_{}_", new_table))
                    .replace(&format!("pk_{}", old_table), &format!("pk_{}", new_table))
                    .replace(&format!("fk_{}", old_table), &format!("fk_{}", new_table))
                    .replace(&format!("unique_{}", old_table), &format!("unique_{}", new_table));

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

        // Apply name-map updates (leaf locks, taken one at a time)
        for (old_name, new_name) in renames {
            // Update pk_indexes mapping
            {
                let mut pk_indexes = self.pk_indexes.write().unwrap_or_else(|e| e.into_inner());
                if pk_indexes.get(old_table) == Some(&old_name) {
                    pk_indexes.remove(old_table);
                    pk_indexes.insert(new_table.to_string(), new_name.clone());
                }
            }

            // Update fk_indexes mapping
            {
                let mut fk_indexes = self.fk_indexes.write().unwrap_or_else(|e| e.into_inner());
                if let Some(fks) = fk_indexes.remove(old_table) {
                    let new_fks: Vec<String> = fks
                        .iter()
                        .map(|n| if n == &old_name { new_name.clone() } else { n.clone() })
                        .collect();
                    fk_indexes.insert(new_table.to_string(), new_fks);
                }
            }

            // Update unique_indexes mapping
            {
                let mut unique_indexes = self.unique_indexes.write().unwrap_or_else(|e| e.into_inner());
                if let Some(uniques) = unique_indexes.remove(old_table) {
                    let new_uniques: Vec<String> = uniques
                        .iter()
                        .map(|n| if n == &old_name { new_name.clone() } else { n.clone() })
                        .collect();
                    unique_indexes.insert(new_table.to_string(), new_uniques);
                }
            }
        }

        self.stats.index_renames.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    // =========================================================================
    // INDEX ACCESS
    // =========================================================================

    /// Get a shared handle to an index by name.
    ///
    /// Returns an `Arc<RwLock<…>>` handle instead of cloning the whole tree.
    /// Keep the lock scope on the returned handle as tight as possible.
    pub fn get_index(&self, name: &str) -> Option<SharedArtIndex> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        indexes.get(name).map(|entry| Arc::clone(&entry.tree))
    }

    /// Get the primary key index for a table
    pub fn get_pk_index(&self, table: &str) -> Option<SharedArtIndex> {
        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
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
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            pk_indexes.get(table).cloned()
        };
        pk_name.and_then(|name| {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            indexes.get(&name).and_then(|entry| {
                let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
                tree.get(key)
            })
        })
    }

    /// Check if a PK value exists without cloning the tree
    pub fn pk_index_contains(&self, table: &str, key: &[u8]) -> Option<bool> {
        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            pk_indexes.get(table).cloned()
        };
        pk_name.map(|name| {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            indexes.get(&name).is_some_and(|entry| {
                let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
                tree.contains(key)
            })
        })
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
                let ordered = if bits & 0x8000_0000 == 0 { bits ^ 0x8000_0000 } else { !bits };
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
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        let key = Self::encode_key(values);

        // Metadata filter first; tree locks taken one at a time.
        indexes.values().any(|entry| {
            entry.table == table
                && matches!(entry.index_type, ArtIndexType::PrimaryKey | ArtIndexType::Unique)
                && entry.columns == columns
                && {
                    let tree = entry.tree.read().unwrap_or_else(|e| e.into_inner());
                    tree.contains(&key)
                }
        })
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
    pub fn on_insert(&self, table: &str, row_id: RowId, column_values: &HashMap<String, Value>) -> ArtResult<()> {
        self.note_mutation();
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());

        // Update all indexes for this table (metadata filter, no tree lock)
        for entry in indexes.values() {
            if entry.table != table {
                continue;
            }

            // Extract values for indexed columns
            let values: Vec<Value> = entry
                .columns
                .iter()
                .filter_map(|col| column_values.get(col).cloned())
                .collect();

            if values.len() == entry.columns.len() {
                let key = Self::encode_key(&values);
                // Note: Constraint checking should have already been done
                // For non-unique indexes, we allow "duplicates" (same key, different row_id)
                let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
                match entry.index_type {
                    ArtIndexType::PrimaryKey | ArtIndexType::Unique => {
                        // Already checked, just insert
                        index.insert(&key, row_id)?;
                        if entry.index_type == ArtIndexType::PrimaryKey && values.len() == 1 {
                            if let Some((value, key_width)) = Self::int_value_width(&values[0]) {
                                index.record_dense_int_insert(key_width, value);
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

        Ok(())
    }

    /// Update indexes after INSERT using an already-materialized tuple.
    pub fn on_insert_tuple(&self, table: &str, row_id: RowId, schema: &Schema, tuple: &Tuple) -> ArtResult<()> {
        self.note_mutation();
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());

        for entry in indexes.values() {
            if entry.table != table {
                continue;
            }

            if let Some(values) = Self::index_value_refs_from_tuple(&entry.columns, schema, tuple) {
                let key = Self::encode_key_from_values(values.iter().copied());
                let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
                match entry.index_type {
                    ArtIndexType::PrimaryKey | ArtIndexType::Unique => {
                        index.insert(&key, row_id)?;
                        if entry.index_type == ArtIndexType::PrimaryKey && values.len() == 1 {
                            if let Some((value, key_width)) = Self::int_value_width(values[0]) {
                                index.record_dense_int_insert(key_width, value);
                            }
                        }
                    }
                    ArtIndexType::ForeignKey | ArtIndexType::Manual => {
                        let _ = index.insert(&key, row_id);
                    }
                }
            }
        }

        Ok(())
    }

    /// Update indexes after INSERT using an already-materialized tuple and
    /// return only the indexed column values needed to undo the insert.
    pub fn on_insert_tuple_collect_index_values(
        &self,
        table: &str,
        row_id: RowId,
        schema: &Schema,
        tuple: &Tuple,
    ) -> ArtResult<HashMap<String, Value>> {
        self.note_mutation();
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        let mut indexed_values = HashMap::new();

        for entry in indexes.values() {
            if entry.table != table {
                continue;
            }

            let mut values = Vec::with_capacity(entry.columns.len());
            for column in &entry.columns {
                let Some(idx) = schema.get_column_index(column) else {
                    values.clear();
                    break;
                };
                let Some(value) = tuple.values.get(idx) else {
                    values.clear();
                    break;
                };
                indexed_values.entry(column.clone()).or_insert_with(|| value.clone());
                values.push(value);
            }

            if values.len() == entry.columns.len() {
                let key = Self::encode_key_from_values(values.iter().copied());
                let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
                match entry.index_type {
                    ArtIndexType::PrimaryKey | ArtIndexType::Unique => {
                        index.insert(&key, row_id)?;
                        if entry.index_type == ArtIndexType::PrimaryKey && values.len() == 1 {
                            if let Some((value, key_width)) = Self::int_value_width(&values[0]) {
                                index.record_dense_int_insert(key_width, value);
                            }
                        }
                    }
                    ArtIndexType::ForeignKey | ArtIndexType::Manual => {
                        let _ = index.insert(&key, row_id);
                    }
                }
            }
        }

        Ok(indexed_values)
    }

    /// Update indexes after DELETE
    ///
    /// Global map lock as READ + per-tree WRITE locks one at a time.
    pub fn on_delete(&self, table: &str, row_id: RowId, column_values: &HashMap<String, Value>) -> ArtResult<()> {
        self.note_mutation();
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());

        for entry in indexes.values() {
            if entry.table != table {
                continue;
            }

            let values: Vec<Value> = entry
                .columns
                .iter()
                .filter_map(|col| column_values.get(col).cloned())
                .collect();

            if values.len() == entry.columns.len() {
                let key = Self::encode_key(&values);
                let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
                match entry.index_type {
                    ArtIndexType::PrimaryKey | ArtIndexType::Unique => {
                        // Unique indexes: remove entire key entry
                        let removed = index.remove(&key)?.is_some();
                        if removed && entry.index_type == ArtIndexType::PrimaryKey && values.len() == 1 {
                            if let Some((value, _)) = Self::int_value_width(&values[0]) {
                                index.record_dense_int_delete(value);
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

        Ok(())
    }

    /// Update indexes after DELETE using the already-materialized tuple.
    pub fn on_delete_tuple(&self, table: &str, row_id: RowId, schema: &Schema, tuple: &Tuple) -> ArtResult<()> {
        self.note_mutation();
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());

        for entry in indexes.values() {
            if entry.table != table {
                continue;
            }

            if let Some(values) = Self::index_value_refs_from_tuple(&entry.columns, schema, tuple) {
                let key = Self::encode_key_from_values(values.iter().copied());
                let mut index = entry.tree.write().unwrap_or_else(|e| e.into_inner());
                match entry.index_type {
                    ArtIndexType::PrimaryKey | ArtIndexType::Unique => {
                        let removed = index.remove(&key)?.is_some();
                        if removed && entry.index_type == ArtIndexType::PrimaryKey && values.len() == 1 {
                            if let Some((value, _)) = Self::int_value_width(&values[0]) {
                                index.record_dense_int_delete(value);
                            }
                        }
                    }
                    ArtIndexType::ForeignKey | ArtIndexType::Manual => {
                        let _ = index.remove_value(&key, row_id);
                    }
                }
            }
        }

        Ok(())
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
    pub fn on_update(
        &self,
        table: &str,
        row_id: RowId,
        old_values: &HashMap<String, Value>,
        new_values: &HashMap<String, Value>,
    ) -> ArtResult<()> {
        // Remove old index entries and add new ones
        self.on_delete(table, row_id, old_values)?;
        self.on_insert(table, row_id, new_values)?;
        Ok(())
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
            .on_insert_tuple_collect_index_values("users", 1, &schema, &tuple)
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
}
