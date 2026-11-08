//! Dictionary encoding module for low-cardinality string columns
//!
//! Provides dictionary-based storage optimization where string values are
//! replaced with compact integer IDs. This significantly reduces storage
//! for columns with few unique values (e.g., status codes, country codes).
//!
//! # Key Format
//!
//! ```text
//! dict:{table}:{column} -> bincode-serialized ColumnDictionary
//! ```
//!
//! # Example
//!
//! ```sql
//! CREATE TABLE orders (
//!     id INT PRIMARY KEY,
//!     status TEXT STORAGE DICTIONARY  -- 'pending', 'shipped', 'delivered'
//! );
//! ```

use parking_lot::{ReentrantMutex, ReentrantMutexGuard, RwLock};
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::write_gate::WriteGate;
use crate::{Error, Result};

/// Single column dictionary mapping strings to compact IDs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnDictionary {
    /// Forward mapping: string value -> dict_id
    value_to_id: HashMap<String, u32>,
    /// Reverse mapping: dict_id -> string value (index = dict_id)
    id_to_value: Vec<String>,
}

impl ColumnDictionary {
    /// Create a new empty dictionary
    pub fn new() -> Self {
        Self {
            value_to_id: HashMap::new(),
            id_to_value: Vec::new(),
        }
    }

    /// Encode a string value to its dict_id, inserting if new
    ///
    /// # Returns
    /// The dict_id for the given string value
    pub fn encode(&mut self, value: &str) -> u32 {
        if let Some(&id) = self.value_to_id.get(value) {
            return id;
        }

        // Insert new value
        let id = self.id_to_value.len() as u32;
        self.id_to_value.push(value.to_string());
        self.value_to_id.insert(value.to_string(), id);
        id
    }

    /// Decode a dict_id back to its string value
    ///
    /// # Returns
    /// The original string value, or None if the ID is invalid
    pub fn decode(&self, id: u32) -> Option<&str> {
        self.id_to_value.get(id as usize).map(|s| s.as_str())
    }

    /// Get the number of unique values in the dictionary
    pub fn len(&self) -> usize {
        self.id_to_value.len()
    }

    /// Check if dictionary is empty
    pub fn is_empty(&self) -> bool {
        self.id_to_value.is_empty()
    }

    /// Check if dictionary contains a value
    pub fn contains(&self, value: &str) -> bool {
        self.value_to_id.contains_key(value)
    }

    /// Get all values in the dictionary
    pub fn values(&self) -> &[String] {
        &self.id_to_value
    }
}

impl Default for ColumnDictionary {
    fn default() -> Self {
        Self::new()
    }
}

/// Cache entry for a column dictionary
struct DictCacheEntry {
    /// The dictionary itself
    dict: ColumnDictionary,
    /// Whether the dictionary has been modified since last persist
    dirty: bool,
}

/// Manages dictionaries for all tables and columns
///
/// Provides caching and persistence of dictionaries to RocksDB.
pub struct DictionaryManager {
    /// Serializes writers through their durable group. Reentrant so a guarded
    /// batch can call encode/stage while legacy immediate writers use the same
    /// protocol. Decode does not take this lock: the branch freeze hook must
    /// read OLD dictionary IDs while the batch is still being applied.
    writers: ReentrantMutex<()>,
    /// Cache: (table, column) -> dictionary entry
    cache: RwLock<HashMap<(String, String), DictCacheEntry>>,
}

impl DictionaryManager {
    /// Create a new dictionary manager
    pub fn new() -> Self {
        Self {
            writers: ReentrantMutex::new(()),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Build the RocksDB key for a column dictionary
    fn dict_key(table: &str, column: &str) -> Vec<u8> {
        format!("dict:{}:{}", table, column).into_bytes()
    }

    /// Load a dictionary from RocksDB
    fn load_dict(db: &DB, table: &str, column: &str) -> Result<ColumnDictionary> {
        let key = Self::dict_key(table, column);
        match db
            .get(&key)
            .map_err(|e| Error::storage(format!("Failed to load dictionary: {}", e)))?
        {
            Some(data) => bincode::deserialize(&data)
                .map_err(|e| Error::storage(format!("Failed to deserialize dictionary: {}", e))),
            None => Ok(ColumnDictionary::new()),
        }
    }

    /// Save a dictionary to RocksDB, through the gate of the engine that owns
    /// `db`.
    fn save_dict(db: &DB, gate: &WriteGate, table: &str, column: &str, dict: &ColumnDictionary) -> Result<()> {
        let key = Self::dict_key(table, column);
        let data =
            bincode::serialize(dict).map_err(|e| Error::storage(format!("Failed to serialize dictionary: {}", e)))?;
        gate.put(db, &key, &data, None)
            .map_err(|e| Error::storage(format!("Failed to save dictionary: {}", e)))?;
        Ok(())
    }

    /// Encode a string value for a table.column, loading/creating dictionary as needed
    ///
    /// # Arguments
    /// * `db` - RocksDB instance for persistence
    /// * `table` - Table name
    /// * `column` - Column name
    /// * `value` - String value to encode
    ///
    /// # Returns
    /// The dict_id for the encoded value
    pub fn encode(&self, db: &DB, table: &str, column: &str, value: &str) -> Result<u32> {
        let _writer = self.writers.lock();
        let cache_key = (table.to_string(), column.to_string());

        // Try read-only lookup first (common case - value already exists)
        {
            let cache = self.cache.read();
            if let Some(entry) = cache.get(&cache_key) {
                if let Some(&id) = entry.dict.value_to_id.get(value) {
                    return Ok(id);
                }
            }
        }

        // Value not found or dict not loaded - need write lock
        let mut cache = self.cache.write();

        // Load dictionary if not in cache
        if !cache.contains_key(&cache_key) {
            let dict = Self::load_dict(db, table, column)?;
            cache.insert(cache_key.clone(), DictCacheEntry { dict, dirty: false });
        }

        // Get mutable reference to entry
        let entry = cache
            .get_mut(&cache_key)
            .ok_or_else(|| Error::storage("Dictionary cache entry missing"))?;

        // Encode the value (may insert new entry)
        let old_len = entry.dict.len();
        let id = entry.dict.encode(value);

        // Mark dirty if we added a new value
        if entry.dict.len() > old_len {
            entry.dirty = true;
        }

        Ok(id)
    }

    /// Decode a dict_id back to its string value
    ///
    /// # Arguments
    /// * `db` - RocksDB instance for loading dictionary
    /// * `table` - Table name
    /// * `column` - Column name
    /// * `dict_id` - Dictionary ID to decode
    ///
    /// # Returns
    /// The original string value
    pub fn decode(&self, db: &DB, table: &str, column: &str, dict_id: u32) -> Result<String> {
        let cache_key = (table.to_string(), column.to_string());

        // Try read-only lookup first
        {
            let cache = self.cache.read();
            if let Some(entry) = cache.get(&cache_key) {
                if let Some(value) = entry.dict.decode(dict_id) {
                    return Ok(value.to_string());
                }
            }
        }

        // Dictionary not loaded - need write lock to load it
        let mut cache = self.cache.write();

        // Load dictionary if not in cache
        if !cache.contains_key(&cache_key) {
            let dict = Self::load_dict(db, table, column)?;
            cache.insert(cache_key.clone(), DictCacheEntry { dict, dirty: false });
        }

        // Decode the value
        let entry = cache
            .get(&cache_key)
            .ok_or_else(|| Error::storage("Dictionary cache entry missing"))?;

        entry
            .dict
            .decode(dict_id)
            .map(|s| s.to_string())
            .ok_or_else(|| Error::storage(format!("Invalid dict_id {} for {}.{}", dict_id, table, column)))
    }

    /// Flush all dirty dictionaries to RocksDB
    ///
    /// # Arguments
    /// * `db` - RocksDB instance for persistence
    /// * `gate` - the write gate of the engine that owns `db`
    pub fn flush(&self, db: &DB, gate: &WriteGate) -> Result<()> {
        let _writer = self.writers.lock();
        let mut cache = self.cache.write();

        for ((table, column), entry) in cache.iter_mut() {
            if entry.dirty {
                Self::save_dict(db, gate, table, column, &entry.dict)?;
                entry.dirty = false;
            }
        }

        Ok(())
    }

    /// Spec R5.5: [`Self::flush_column`] staged into `batch`, for a schema statement
    /// that writes one group (`SET STORAGE DICTIONARY`). The entry is marked clean
    /// now; if the batch is not written, the caller forgets the table's dictionaries
    /// (`StorageEngine::forget_table_dictionaries`), so the next encode reloads what
    /// is durable.
    pub fn stage_column(&self, table: &str, column: &str, batch: &mut rocksdb::WriteBatch) -> Result<()> {
        let _writer = self.writers.lock();
        let cache_key = (table.to_string(), column.to_string());
        let mut cache = self.cache.write();
        if let Some(entry) = cache.get_mut(&cache_key) {
            if entry.dirty {
                let data = bincode::serialize(&entry.dict)
                    .map_err(|e| Error::storage(format!("Failed to serialize dictionary: {}", e)))?;
                batch.put(Self::dict_key(table, column), &data);
                entry.dirty = false;
            }
        }
        Ok(())
    }

    /// Flush dictionary for a specific table.column
    pub fn flush_column(&self, db: &DB, gate: &WriteGate, table: &str, column: &str) -> Result<()> {
        let _writer = self.writers.lock();
        let cache_key = (table.to_string(), column.to_string());
        let mut cache = self.cache.write();

        if let Some(entry) = cache.get_mut(&cache_key) {
            if entry.dirty {
                Self::save_dict(db, gate, table, column, &entry.dict)?;
                entry.dirty = false;
            }
        }

        Ok(())
    }

    /// Get statistics for a dictionary
    pub fn stats(&self, db: &DB, table: &str, column: &str) -> Result<DictionaryStats> {
        let cache_key = (table.to_string(), column.to_string());

        // Try read from cache first
        {
            let cache = self.cache.read();
            if let Some(entry) = cache.get(&cache_key) {
                return Ok(DictionaryStats {
                    unique_values: entry.dict.len(),
                    dirty: entry.dirty,
                });
            }
        }

        // Load from disk
        let dict = Self::load_dict(db, table, column)?;
        Ok(DictionaryStats {
            unique_values: dict.len(),
            dirty: false,
        })
    }

    /// Clear the dictionary cache (for testing)
    pub fn clear_cache(&self) {
        let _writer = self.writers.lock();
        self.cache.write().clear();
    }

    /// Forget every cached dictionary of `table`, dirty or not, without
    /// writing any of them — for `DROP TABLE`, which deletes the table's
    /// `dict:{table}:*` records. A dirty entry left here would be written back
    /// by the next [`Self::flush`], resurrecting a dictionary for a table that
    /// no longer exists (and handing it to a new table of the same name).
    pub fn forget_table(&self, table: &str) {
        let _writer = self.writers.lock();
        self.cache.write().retain(|(cached_table, _), _| cached_table != table);
    }

    /// Spec R5.3 (review F8): the column a rename maps `column` to — every column
    /// keeps its name under `RENAME TO` (`renamed` is `None`); under `RENAME COLUMN`
    /// (`Some((old, new))`) only `old` moves, to `new`.
    fn renamed_column(column: &str, renamed: Option<(&str, &str)>) -> Option<String> {
        match renamed {
            None => Some(column.to_string()),
            Some((old, new)) if column == old => Some(new.to_string()),
            Some(_) => None,
        }
    }

    /// Spec R5.3 (review F8): stage into `batch` the move of `table`'s dictionaries
    /// to `new_table`: every one of them for `RENAME TO` (`renamed` is `None`), the
    /// one of column `old` to `new` for `RENAME COLUMN` (`Some((old, new))`). A
    /// dictionary is keyed by table and column NAME, so one left behind made every
    /// read of the renamed table or column fail (`Invalid dict_id`), and a value
    /// encoded under the new name got an id the old dictionary gave another value:
    /// read back after a rename back, the row held the wrong value.
    ///
    /// What moves is the dictionary as this process knows it: the cached one, dirty
    /// values included, else the stored one. Whatever was cached under the target
    /// name is forgotten, and under `RENAME COLUMN` whatever is stored under it is
    /// deleted first (a dropped column's dictionary, unlike a dropped table's, stays
    /// behind). A moved cached entry is marked written here, so no concurrent
    /// [`Self::flush`] writes it back under the old key after the batch deleted it;
    /// the returned names are the ones that were dirty, for [`Self::mark_dirty`] if
    /// the batch is not written. Nothing is marked until every read that can fail has
    /// succeeded. The caller holds the tables' writer locks until the publication, so
    /// nothing encodes into them meanwhile, and publishes with [`Self::forget_moved`]
    /// after the write. `table` and `new_table`, or `old` and `new`, differ.
    pub(crate) fn stage_move(
        &self,
        db: &DB,
        table: &str,
        new_table: &str,
        renamed: Option<(&str, &str)>,
        batch: &mut rocksdb::WriteBatch,
    ) -> Result<Vec<String>> {
        let mut cached: Vec<(String, String, Vec<u8>)> = Vec::new();
        {
            let cache = self.cache.read();
            for ((cached_table, column), entry) in cache.iter() {
                if cached_table != table {
                    continue;
                }
                let Some(new_column) = Self::renamed_column(column, renamed) else {
                    continue;
                };
                let data = bincode::serialize(&entry.dict)
                    .map_err(|e| Error::storage(format!("Failed to serialize dictionary: {}", e)))?;
                cached.push((column.clone(), new_column, data));
            }
        }
        let staged: std::collections::HashSet<&str> = cached.iter().map(|(column, _, _)| column.as_str()).collect();
        let mut stored: Vec<(Vec<u8>, String, Vec<u8>)> = Vec::new();
        let prefix = format!("dict:{}:", table);
        let candidates = match renamed {
            None => Self::entries_under(db, prefix.as_bytes())?,
            Some((old, _)) => {
                let key = Self::dict_key(table, old);
                let value = db
                    .get(&key)
                    .map_err(|e| Error::storage(format!("Failed to load dictionary: {}", e)))?;
                value.map(|value| (key, value)).into_iter().collect()
            }
        };
        for (key, value) in candidates {
            let Some(column) = key.get(prefix.len()..) else {
                continue;
            };
            let column = String::from_utf8_lossy(column).into_owned();
            if staged.contains(column.as_str()) {
                continue;
            }
            if let Some(new_column) = Self::renamed_column(&column, renamed) {
                stored.push((key, new_column, value));
            }
        }
        // A dropped column's dictionary is never deleted, so the target column's
        // name may still have one: it goes first, and the move below then wins.
        // (A dropped table's went with it; under `RENAME TO` the moves overwrite,
        // as the rows' do, which a crash replay relies on.)
        if let Some((_, new)) = renamed {
            batch.delete(Self::dict_key(new_table, new));
        }
        for (column, new_column, data) in &cached {
            batch.put(Self::dict_key(new_table, new_column), data);
            batch.delete(Self::dict_key(table, column));
        }
        for (key, new_column, value) in &stored {
            batch.put(Self::dict_key(new_table, new_column), value);
            batch.delete(key);
        }
        let mut were_dirty = Vec::new();
        let mut cache = self.cache.write();
        match renamed {
            None => cache.retain(|(cached_table, _), _| cached_table != new_table),
            Some((_, new)) => {
                cache.remove(&(new_table.to_string(), new.to_string()));
            }
        }
        for (column, _, _) in &cached {
            if let Some(entry) = cache.get_mut(&(table.to_string(), column.clone())) {
                if entry.dirty {
                    entry.dirty = false;
                    were_dirty.push(column.clone());
                }
            }
        }
        Ok(were_dirty)
    }

    /// Stage all net column renames together, reading every source before any
    /// destination is cleared. The caller retains writer_lock through durable
    /// application and cache settlement. Cached sources remain in place for B4
    /// preimage decoding, including on an aborted statement.
    pub(crate) fn stage_column_renames(
        &self,
        db: &DB,
        table: &str,
        renames: &[(String, String)],
        batch: &mut rocksdb::WriteBatch,
    ) -> Result<Vec<String>> {
        let _writer = self.writers.lock();
        let mut moves = Vec::with_capacity(renames.len());
        let mut dirty = Vec::new();
        {
            let cache = self.cache.read();
            for (old, new) in renames {
                let value =
                    match cache.get(&(table.to_string(), old.clone())) {
                        Some(entry) => {
                            if entry.dirty {
                                dirty.push(old.clone());
                            }
                            Some(bincode::serialize(&entry.dict).map_err(|e| {
                                Error::storage(format!("Failed to serialize a renamed dictionary: {e}"))
                            })?)
                        }
                        None => db
                            .get(Self::dict_key(table, old))
                            .map_err(|e| Error::storage(format!("Failed to read a renamed dictionary: {e}")))?,
                    };
                moves.push((Self::dict_key(table, old), Self::dict_key(table, new), value));
            }
        }
        // A swap must not delete a destination that an earlier move populated.
        for (old, new, _) in &moves {
            batch.delete(old);
            batch.delete(new);
        }
        for (_, new, value) in &moves {
            if let Some(value) = value {
                batch.put(new, value);
            }
        }
        let mut cache = self.cache.write();
        for old in &dirty {
            if let Some(entry) = cache.get_mut(&(table.to_string(), old.clone())) {
                entry.dirty = false;
            }
        }
        Ok(dirty)
    }

    /// Forget both endpoints only after the group settled. Keeping sources while
    /// staging lets a refused group restore dirty entries without losing IDs.
    pub(crate) fn forget_column_renames(&self, table: &str, renames: &[(String, String)]) {
        let mut cache = self.cache.write();
        for (old, new) in renames {
            cache.remove(&(table.to_string(), old.clone()));
            cache.remove(&(table.to_string(), new.clone()));
        }
    }

    /// The dictionaries [`Self::stage_move`] marked written were not: dirty again.
    pub(crate) fn mark_dirty(&self, table: &str, columns: &[String]) {
        let mut cache = self.cache.write();
        for column in columns {
            if let Some(entry) = cache.get_mut(&(table.to_string(), column.clone())) {
                entry.dirty = true;
            }
        }
    }

    /// Publish a [`Self::stage_move`] once its batch is written: every cached entry on
    /// either side of the move is forgotten, and the next use loads the moved
    /// dictionary from the store, which the batch wrote whole. Moving the cached
    /// entries instead could carry one that a reader loaded between the write and
    /// this call — from a key the write had just deleted, so empty — onto the new
    /// name, where the next insert would hand out ids the stored dictionary had
    /// already given to other values.
    pub(crate) fn forget_moved(&self, table: &str, new_table: &str, renamed: Option<(&str, &str)>) {
        let mut cache = self.cache.write();
        match renamed {
            None => cache.retain(|(cached_table, _), _| cached_table != table && cached_table != new_table),
            Some((old, new)) => {
                cache.remove(&(table.to_string(), old.to_string()));
                cache.remove(&(new_table.to_string(), new.to_string()));
            }
        }
    }

    /// Every key and value under `prefix`, read with a total-order seek.
    fn entries_under(db: &DB, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = db.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            read_opts,
        );
        let mut out = Vec::new();
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Dictionary iterator error: {}", e)))?;
            if !key.starts_with(prefix) {
                break;
            }
            out.push((key.to_vec(), value.to_vec()));
        }
        Ok(out)
    }

    /// Drop a dictionary from cache and storage
    pub fn drop_dictionary(&self, db: &DB, gate: &WriteGate, table: &str, column: &str) -> Result<()> {
        let _writer = self.writers.lock();
        let cache_key = (table.to_string(), column.to_string());

        // Remove from cache
        self.cache.write().remove(&cache_key);

        // Remove from storage
        let key = Self::dict_key(table, column);
        gate.delete(db, &key, None)
            .map_err(|e| Error::storage(format!("Failed to delete dictionary: {}", e)))?;

        Ok(())
    }

    pub(crate) fn write_group(&self) -> DictionaryWriteGroup<'_> {
        DictionaryWriteGroup {
            manager: self,
            _writer: self.writers.lock(),
            before: Vec::new(),
            staged: Vec::new(),
            settled: false,
        }
    }

    pub(crate) fn writer_lock(&self) -> ReentrantMutexGuard<'_, ()> {
        self.writers.lock()
    }

    /// A raw transaction may append a dictionary, but must never overwrite IDs
    /// a concurrently committed row already uses. Called under writer_lock.
    pub(crate) fn validate_staged(db: &DB, key: &[u8], proposed: &[u8]) -> Result<()> {
        let proposed: ColumnDictionary = bincode::deserialize(proposed)
            .map_err(|e| Error::storage(format!("staged dictionary does not decode: {e}")))?;
        if proposed.value_to_id.len() != proposed.id_to_value.len()
            || proposed
                .id_to_value
                .iter()
                .enumerate()
                .any(|(id, value)| proposed.value_to_id.get(value) != Some(&(id as u32)))
        {
            return Err(Error::storage("staged dictionary has inconsistent IDs"));
        }
        if let Some(raw) = db.get(key)? {
            let current: ColumnDictionary = bincode::deserialize(&raw)
                .map_err(|e| Error::storage(format!("durable dictionary does not decode: {e}")))?;
            if !proposed.id_to_value.starts_with(&current.id_to_value) {
                return Err(Error::transaction(
                    "serialization failure: dictionary IDs changed; retry the transaction",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn validate_cached_staged(&self, key: &[u8], proposed: &[u8]) -> Result<()> {
        let proposed: ColumnDictionary = bincode::deserialize(proposed)
            .map_err(|e| Error::storage(format!("staged dictionary does not decode: {e}")))?;
        // Include IDs an immediate writer encoded before our writer guard and
        // has not flushed yet; that writer must keep the same mapping later.
        for ((table, column), entry) in self.cache.read().iter() {
            if Self::dict_key(table, column) == key && !proposed.id_to_value.starts_with(&entry.dict.id_to_value) {
                return Err(Error::transaction(
                    "serialization failure: cached dictionary IDs changed; retry the transaction",
                ));
            }
        }
        Ok(())
    }
}

/// Holds the writer serialization lock, never the decode cache lock, through
/// the row's group. A failed plan restores the old cache. A failed write checks
/// whether its dictionary actually landed before deciding which cache is valid.
pub(crate) struct DictionaryWriteGroup<'a> {
    manager: &'a DictionaryManager,
    _writer: ReentrantMutexGuard<'a, ()>,
    before: Vec<((String, String), Option<(usize, bool)>)>,
    staged: Vec<((String, String), Vec<u8>)>,
    settled: bool,
}

impl DictionaryWriteGroup<'_> {
    pub(crate) fn encode(&mut self, db: &DB, table: &str, column: &str, value: &str) -> Result<u32> {
        let key = (table.to_string(), column.to_string());
        if !self.before.iter().any(|(saved, _)| saved == &key) {
            let before = self
                .manager
                .cache
                .read()
                .get(&key)
                .map(|entry| (entry.dict.len(), entry.dirty));
            self.before.push((key, before));
        }
        self.manager.encode(db, table, column, value)
    }

    pub(crate) fn stage_column(&mut self, table: &str, column: &str, batch: &mut rocksdb::WriteBatch) -> Result<()> {
        let key = (table.to_string(), column.to_string());
        let mut cache = self.manager.cache.write();
        if let Some(entry) = cache.get_mut(&key) {
            if entry.dirty {
                let bytes = bincode::serialize(&entry.dict)
                    .map_err(|e| Error::storage(format!("Failed to serialize dictionary: {e}")))?;
                batch.put(DictionaryManager::dict_key(table, column), &bytes);
                entry.dirty = false;
                self.staged.push((key, bytes));
            }
        }
        Ok(())
    }

    /// Must run before publication and ACK waits, releasing this group's writer
    /// lock. An I/O error can follow an applied batch; never roll its cache back
    /// to IDs that no longer describe the durable dictionary.
    pub(crate) fn finish(mut self, db: &DB, written: bool) {
        if !written {
            let mut cache = self.manager.cache.write();
            for (key, before) in self.before.drain(..) {
                let staged = self
                    .staged
                    .iter()
                    .find(|(saved, _)| saved == &key)
                    .map(|(_, bytes)| bytes);
                let durable = db.get(DictionaryManager::dict_key(&key.0, &key.1));
                match (staged, durable) {
                    (Some(staged), Ok(Some(durable))) if staged.as_slice() == durable.as_slice() => {}
                    (_, Err(_)) => {
                        // Keep the append-only superset if storage cannot answer.
                        // It preserves every pre-existing ID in either outcome.
                        if let Some(entry) = cache.get_mut(&key) {
                            entry.dirty = true;
                        }
                    }
                    _ => match before {
                        Some((length, dirty)) => {
                            if let Some(entry) = cache.get_mut(&key) {
                                for value in entry.dict.id_to_value.drain(length..) {
                                    entry.dict.value_to_id.remove(&value);
                                }
                                entry.dirty = dirty;
                            }
                        }
                        None => {
                            cache.remove(&key);
                        }
                    },
                }
            }
        }
        self.settled = true;
    }
}

impl Drop for DictionaryWriteGroup<'_> {
    fn drop(&mut self) {
        if !self.settled {
            let mut cache = self.manager.cache.write();
            for (key, before) in self.before.drain(..) {
                match before {
                    Some((length, dirty)) => {
                        if let Some(entry) = cache.get_mut(&key) {
                            for value in entry.dict.id_to_value.drain(length..) {
                                entry.dict.value_to_id.remove(&value);
                            }
                            entry.dirty = dirty;
                        }
                    }
                    None => {
                        cache.remove(&key);
                    }
                }
            }
        }
    }
}

impl Default for DictionaryManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics for a dictionary
#[derive(Debug, Clone)]
pub struct DictionaryStats {
    /// Number of unique values in the dictionary
    pub unique_values: usize,
    /// Whether the dictionary has unsaved changes
    pub dirty: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_column_dictionary_basic() {
        let mut dict = ColumnDictionary::new();

        // Encode values
        let id1 = dict.encode("pending");
        let id2 = dict.encode("shipped");
        let id3 = dict.encode("pending"); // Duplicate

        assert_eq!(id1, 0);
        assert_eq!(id2, 1);
        assert_eq!(id3, 0); // Same as id1

        // Decode values
        assert_eq!(dict.decode(0), Some("pending"));
        assert_eq!(dict.decode(1), Some("shipped"));
        assert_eq!(dict.decode(2), None);

        // Length
        assert_eq!(dict.len(), 2);
    }

    #[test]
    fn test_column_dictionary_serialization() {
        let mut dict = ColumnDictionary::new();
        dict.encode("foo");
        dict.encode("bar");
        dict.encode("baz");

        // Serialize
        let data = bincode::serialize(&dict).unwrap();

        // Deserialize
        let restored: ColumnDictionary = bincode::deserialize(&data).unwrap();

        assert_eq!(restored.len(), 3);
        assert_eq!(restored.decode(0), Some("foo"));
        assert_eq!(restored.decode(1), Some("bar"));
        assert_eq!(restored.decode(2), Some("baz"));
    }
}
