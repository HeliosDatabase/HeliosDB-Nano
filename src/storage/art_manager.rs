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
use crate::{DataType, Schema, Tuple, Value};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

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

/// ART Index Manager
///
/// Thread-safe manager for all ART indexes in the database.
#[derive(Debug)]
pub struct ArtIndexManager {
    /// All indexes by name
    indexes: RwLock<HashMap<String, AdaptiveRadixTree>>,
    /// Primary key index name by table
    pk_indexes: RwLock<HashMap<String, String>>,
    /// Foreign key indexes by table (table -> list of FK index names)
    fk_indexes: RwLock<HashMap<String, Vec<String>>>,
    /// Foreign key metadata by index name
    fk_info: RwLock<HashMap<String, ForeignKeyInfo>>,
    /// Unique constraint indexes by table (table -> list of unique index names)
    unique_indexes: RwLock<HashMap<String, Vec<String>>>,
    /// Statistics
    stats: RwLock<ArtManagerStats>,
}

impl Default for ArtIndexManager {
    fn default() -> Self {
        Self::new()
    }
}

fn decode_int_key(key: &[u8], width: usize) -> Option<i64> {
    match width {
        2 if key.len() == 2 => Some(i64::from(i16::from_be_bytes([key[0], key[1]]))),
        4 if key.len() == 4 => Some(i64::from(i32::from_be_bytes([key[0], key[1], key[2], key[3]]))),
        8 if key.len() == 8 => Some(i64::from_be_bytes([
            key[0], key[1], key[2], key[3], key[4], key[5], key[6], key[7],
        ])),
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
            stats: RwLock::new(ArtManagerStats::default()),
        }
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
            indexes.insert(index_name.clone(), index);
        }

        {
            let mut pk_indexes = self.pk_indexes.write().unwrap_or_else(|e| e.into_inner());
            pk_indexes.insert(table.to_string(), index_name.clone());
        }

        {
            let mut stats = self.stats.write().unwrap_or_else(|e| e.into_inner());
            stats.total_indexes += 1;
            stats.pk_indexes += 1;
        }

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
            indexes.insert(index_name.clone(), index);
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

        {
            let mut stats = self.stats.write().unwrap_or_else(|e| e.into_inner());
            stats.total_indexes += 1;
            stats.fk_indexes += 1;
        }

        Ok(index_name)
    }

    /// Create a unique constraint index (auto-called on CREATE TABLE UNIQUE or ALTER TABLE ADD UNIQUE)
    pub fn create_unique_index(
        &self,
        table: &str,
        columns: &[String],
        constraint_name: Option<&str>,
    ) -> ArtResult<String> {
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
            indexes.insert(index_name.clone(), index);
        }

        {
            let mut unique_indexes = self.unique_indexes.write().unwrap_or_else(|e| e.into_inner());
            unique_indexes
                .entry(table.to_string())
                .or_insert_with(Vec::new)
                .push(index_name.clone());
        }

        {
            let mut stats = self.stats.write().unwrap_or_else(|e| e.into_inner());
            stats.total_indexes += 1;
            stats.unique_indexes += 1;
        }

        Ok(index_name)
    }

    /// Create a manual index (via CREATE INDEX ... USING ART)
    pub fn create_manual_index(&self, name: &str, table: &str, columns: &[String]) -> ArtResult<String> {
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
            indexes.insert(name.to_string(), index);
        }

        {
            let mut stats = self.stats.write().unwrap_or_else(|e| e.into_inner());
            stats.total_indexes += 1;
            stats.manual_indexes += 1;
        }

        Ok(name.to_string())
    }

    // =========================================================================
    // INDEX REMOVAL
    // =========================================================================

    /// Drop an index by name
    pub fn drop_index(&self, name: &str) -> ArtResult<()> {
        let index_type;

        // Remove from main index map
        {
            let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());
            if let Some(idx) = indexes.remove(name) {
                index_type = idx.index_type();
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

        // Update stats
        {
            let mut stats = self.stats.write().unwrap_or_else(|e| e.into_inner());
            stats.total_indexes -= 1;
            match index_type {
                ArtIndexType::PrimaryKey => stats.pk_indexes -= 1,
                ArtIndexType::ForeignKey => stats.fk_indexes -= 1,
                ArtIndexType::Unique => stats.unique_indexes -= 1,
                ArtIndexType::Manual => stats.manual_indexes -= 1,
            }
        }

        Ok(())
    }

    /// Drop all indexes for a table (called on DROP TABLE)
    pub fn drop_table_indexes(&self, table: &str) -> ArtResult<()> {
        let mut to_drop = Vec::new();

        // Collect all indexes for this table
        {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            for (name, idx) in indexes.iter() {
                if idx.table() == table {
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
        // Collect indexes to rename
        let mut renames: Vec<(String, String, AdaptiveRadixTree)> = Vec::new();

        {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            for (name, idx) in indexes.iter() {
                if idx.table() == old_table {
                    // Generate new index name by replacing table name
                    let new_name = name
                        .replace(&format!("_{}_", old_table), &format!("_{}_", new_table))
                        .replace(&format!("pk_{}", old_table), &format!("pk_{}", new_table))
                        .replace(&format!("fk_{}", old_table), &format!("fk_{}", new_table))
                        .replace(&format!("unique_{}", old_table), &format!("unique_{}", new_table));

                    // Clone and rename the index
                    let mut new_idx = idx.clone();
                    new_idx.rename(new_table.to_string(), new_name.clone());
                    renames.push((name.clone(), new_name, new_idx));
                }
            }
        }

        // Apply renames
        for (old_name, new_name, new_idx) in renames {
            // Remove old index
            {
                let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());
                indexes.remove(&old_name);
                indexes.insert(new_name.clone(), new_idx);
            }

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

        // Update stats
        {
            let mut stats = self.stats.write().unwrap_or_else(|e| e.into_inner());
            stats.index_renames += 1;
        }

        Ok(())
    }

    // =========================================================================
    // INDEX ACCESS
    // =========================================================================

    /// Get a reference to an index by name
    pub fn get_index(&self, name: &str) -> Option<AdaptiveRadixTree> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        indexes.get(name).cloned()
    }

    /// Get the primary key index for a table
    pub fn get_pk_index(&self, table: &str) -> Option<AdaptiveRadixTree> {
        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            pk_indexes.get(table).cloned()
        };

        pk_name.and_then(|name| self.get_index(&name))
    }

    /// Get all foreign key indexes for a table
    pub fn get_fk_indexes(&self, table: &str) -> Vec<AdaptiveRadixTree> {
        let fk_names = {
            let fk_indexes = self.fk_indexes.read().unwrap_or_else(|e| e.into_inner());
            fk_indexes.get(table).cloned().unwrap_or_default()
        };

        fk_names.iter().filter_map(|name| self.get_index(name)).collect()
    }

    /// Get all unique indexes for a table
    pub fn get_unique_indexes(&self, table: &str) -> Vec<AdaptiveRadixTree> {
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
            .values()
            .map(|idx| {
                (
                    idx.name().to_string(),
                    idx.table().to_string(),
                    idx.index_type(),
                    idx.columns().to_vec(),
                )
            })
            .collect()
    }

    /// Find an index for a specific column in a table (returns index name if found)
    pub fn find_column_index(&self, table: &str, column: &str) -> Option<String> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        for idx in indexes.values() {
            if idx.table() == table && idx.columns().len() == 1 {
                if let Some(col) = idx.columns().first() {
                    if col == column {
                        return Some(idx.name().to_string());
                    }
                }
            }
        }
        None
    }

    /// Look up all row_ids for a key in a named index (avoids cloning the entire tree)
    pub fn index_get_all(&self, index_name: &str, key: &[u8]) -> Vec<RowId> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        if let Some(idx) = indexes.get(index_name) {
            idx.get_all(key)
        } else {
            Vec::new()
        }
    }

    /// PK index point lookup without cloning the tree (~50μs saved vs get_pk_index)
    pub fn pk_index_lookup(&self, table: &str, key: &[u8]) -> Option<RowId> {
        let pk_name = {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            pk_indexes.get(table).cloned()
        };
        pk_name.and_then(|name| {
            let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
            indexes.get(&name).and_then(|idx| idx.get(key))
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
            indexes.get(&name).is_some_and(|idx| idx.contains(key))
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
        indexes.get(&pk_name).map(|idx| idx.len() as usize)
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
        let index = indexes.get(&pk_name)?;
        if index.columns().len() != 1 {
            return None;
        }
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
            .values()
            .filter(|idx| idx.table() == table)
            .map(|idx| (idx.name().to_string(), idx.index_type(), idx.columns().to_vec()))
            .collect()
    }

    // =========================================================================
    // CONSTRAINT ENFORCEMENT
    // =========================================================================

    /// Encode a composite key from multiple values
    pub fn encode_key(values: &[Value]) -> Vec<u8> {
        let mut key = Vec::new();
        for (i, value) in values.iter().enumerate() {
            if i > 0 {
                key.push(0); // Separator
            }
            match value {
                Value::Null => key.extend_from_slice(b"\x00"),
                Value::Boolean(b) => key.push(if *b { 1 } else { 0 }),
                Value::Int2(v) => key.extend_from_slice(&v.to_be_bytes()),
                Value::Int4(v) => key.extend_from_slice(&v.to_be_bytes()),
                Value::Int8(v) => key.extend_from_slice(&v.to_be_bytes()),
                Value::Float4(v) => key.extend_from_slice(&v.to_be_bytes()),
                Value::Float8(v) => key.extend_from_slice(&v.to_be_bytes()),
                Value::String(s) => key.extend_from_slice(s.as_bytes()),
                Value::Bytes(b) => key.extend_from_slice(b),
                Value::Uuid(u) => key.extend_from_slice(u.as_bytes()),
                Value::Numeric(d) => key.extend_from_slice(d.as_bytes()),
                Value::Date(d) => key.extend_from_slice(d.to_string().as_bytes()),
                Value::Time(t) => key.extend_from_slice(t.to_string().as_bytes()),
                Value::Timestamp(ts) => key.extend_from_slice(ts.to_rfc3339().as_bytes()),
                Value::Array(arr) => {
                    // Recursively encode array elements
                    let nested = Self::encode_key(arr);
                    key.extend_from_slice(&nested);
                }
                Value::Json(j) => key.extend_from_slice(j.as_bytes()),
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
        key
    }

    fn index_values_from_tuple(columns: &[String], schema: &Schema, tuple: &Tuple) -> Option<Vec<Value>> {
        let mut values = Vec::with_capacity(columns.len());
        for column in columns {
            let idx = schema.get_column_index(column)?;
            values.push(tuple.values.get(idx)?.clone());
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

        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());

        if let Some(pk_name) = pk_indexes.get(table) {
            if let Some(index) = indexes.get(pk_name) {
                let key = Self::encode_key(key_values);
                if index.contains(&key) {
                    return Err(ArtIndexError::DuplicateKey(format!(
                        "Duplicate key value violates PRIMARY KEY constraint \"{}\"",
                        pk_name
                    )));
                }
            }
        }

        {
            let mut stats = self.stats.write().unwrap_or_else(|e| e.into_inner());
            stats.constraint_checks += 1;
        }

        Ok(())
    }

    /// Check unique constraint before INSERT/UPDATE.
    ///
    /// Also checks the PRIMARY KEY index (which is stored separately from
    /// UNIQUE indexes) so that a single call covers both constraint kinds.
    pub fn check_unique_constraints(&self, table: &str, column_values: &HashMap<String, Value>) -> ArtResult<()> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());

        // --- Check PK index (stored separately in pk_indexes) ---
        {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            if let Some(pk_name) = pk_indexes.get(table) {
                if let Some(pk_index) = indexes.get(pk_name) {
                    let columns = pk_index.columns();
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
                        if pk_index.contains(&key) {
                            return Err(ArtIndexError::DuplicateKey(format!(
                                "Duplicate key value violates PRIMARY KEY constraint \"{}\"",
                                pk_name
                            )));
                        }
                    }
                }
            }
        }

        // --- Check UNIQUE indexes ---
        let unique_indexes = self.unique_indexes.read().unwrap_or_else(|e| e.into_inner());

        if let Some(unique_names) = unique_indexes.get(table) {
            for unique_name in unique_names {
                if let Some(index) = indexes.get(unique_name) {
                    // Extract values for this unique constraint's columns
                    let columns = index.columns();
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
                        if index.contains(&key) {
                            return Err(ArtIndexError::DuplicateKey(format!(
                                "Duplicate key value violates UNIQUE constraint \"{}\"",
                                unique_name
                            )));
                        }
                    }
                }
            }
        }

        {
            let mut stats = self.stats.write().unwrap_or_else(|e| e.into_inner());
            stats.constraint_checks += 1;
        }

        Ok(())
    }

    /// Tuple-backed PK/UNIQUE constraint check for storage fast paths.
    ///
    /// This avoids building a column-name map for every inserted row when the
    /// values are already available in schema order.
    pub fn check_unique_constraints_tuple(&self, table: &str, schema: &Schema, tuple: &Tuple) -> ArtResult<()> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());

        {
            let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());
            if let Some(pk_name) = pk_indexes.get(table) {
                if let Some(pk_index) = indexes.get(pk_name) {
                    if let Some(values) = Self::index_values_from_tuple(pk_index.columns(), schema, tuple) {
                        if !values.iter().any(|v| matches!(v, Value::Null)) {
                            let key = Self::encode_key(&values);
                            if pk_index.contains(&key) {
                                return Err(ArtIndexError::DuplicateKey(format!(
                                    "Duplicate key value violates PRIMARY KEY constraint \"{}\"",
                                    pk_name
                                )));
                            }
                        }
                    }
                }
            }
        }

        let unique_indexes = self.unique_indexes.read().unwrap_or_else(|e| e.into_inner());
        if let Some(unique_names) = unique_indexes.get(table) {
            for unique_name in unique_names {
                if let Some(index) = indexes.get(unique_name) {
                    if let Some(values) = Self::index_values_from_tuple(index.columns(), schema, tuple) {
                        if values.iter().any(|v| matches!(v, Value::Null)) {
                            continue;
                        }
                        let key = Self::encode_key(&values);
                        if index.contains(&key) {
                            return Err(ArtIndexError::DuplicateKey(format!(
                                "Duplicate key value violates UNIQUE constraint \"{}\"",
                                unique_name
                            )));
                        }
                    }
                }
            }
        }

        {
            let mut stats = self.stats.write().unwrap_or_else(|e| e.into_inner());
            stats.constraint_checks += 1;
        }

        Ok(())
    }

    /// Check foreign key constraint before INSERT/UPDATE
    pub fn check_fk_constraints(&self, table: &str, column_values: &HashMap<String, Value>) -> ArtResult<()> {
        let fk_indexes = self.fk_indexes.read().unwrap_or_else(|e| e.into_inner());
        let fk_info_map = self.fk_info.read().unwrap_or_else(|e| e.into_inner());
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());

        if let Some(fk_names) = fk_indexes.get(table) {
            for fk_name in fk_names {
                if let Some(fk_info) = fk_info_map.get(fk_name) {
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
                    let pk_indexes = self.pk_indexes.read().unwrap_or_else(|e| e.into_inner());

                    if let Some(ref_pk_name) = pk_indexes.get(ref_table) {
                        if let Some(ref_index) = indexes.get(ref_pk_name) {
                            let key = Self::encode_key(&values);
                            if !ref_index.contains(&key) {
                                let mut stats = self.stats.write().unwrap_or_else(|e| e.into_inner());
                                stats.violations_caught += 1;
                                return Err(ArtIndexError::ForeignKeyViolation(format!(
                                    "Key ({:?}) not present in table \"{}\"",
                                    values, ref_table
                                )));
                            }
                        }
                    }
                }
            }
        }

        {
            let mut stats = self.stats.write().unwrap_or_else(|e| e.into_inner());
            stats.constraint_checks += 1;
        }

        Ok(())
    }

    // =========================================================================
    // INDEX MAINTENANCE
    // =========================================================================

    /// Update indexes after INSERT
    pub fn on_insert(&self, table: &str, row_id: RowId, column_values: &HashMap<String, Value>) -> ArtResult<()> {
        let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());

        // Update all indexes for this table
        for index in indexes.values_mut() {
            if index.table() != table {
                continue;
            }

            // Extract values for indexed columns
            let columns = index.columns().to_vec();
            let values: Vec<Value> = columns
                .iter()
                .filter_map(|col| column_values.get(col).cloned())
                .collect();

            if values.len() == columns.len() {
                let key = Self::encode_key(&values);
                // Note: Constraint checking should have already been done
                // For non-unique indexes, we allow "duplicates" (same key, different row_id)
                match index.index_type() {
                    ArtIndexType::PrimaryKey | ArtIndexType::Unique => {
                        // Already checked, just insert
                        index.insert(&key, row_id)?;
                        if index.index_type() == ArtIndexType::PrimaryKey && values.len() == 1 {
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
        let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());

        for index in indexes.values_mut() {
            if index.table() != table {
                continue;
            }

            if let Some(values) = Self::index_values_from_tuple(index.columns(), schema, tuple) {
                let key = Self::encode_key(&values);
                match index.index_type() {
                    ArtIndexType::PrimaryKey | ArtIndexType::Unique => {
                        index.insert(&key, row_id)?;
                        if index.index_type() == ArtIndexType::PrimaryKey && values.len() == 1 {
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

        Ok(())
    }

    /// Update indexes after DELETE
    pub fn on_delete(&self, table: &str, row_id: RowId, column_values: &HashMap<String, Value>) -> ArtResult<()> {
        let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());

        for index in indexes.values_mut() {
            if index.table() != table {
                continue;
            }

            let columns = index.columns().to_vec();
            let values: Vec<Value> = columns
                .iter()
                .filter_map(|col| column_values.get(col).cloned())
                .collect();

            if values.len() == columns.len() {
                let key = Self::encode_key(&values);
                match index.index_type() {
                    ArtIndexType::PrimaryKey | ArtIndexType::Unique => {
                        // Unique indexes: remove entire key entry
                        let removed = index.remove(&key)?.is_some();
                        if removed && index.index_type() == ArtIndexType::PrimaryKey && values.len() == 1 {
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
        let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());

        for index in indexes.values_mut() {
            if index.table() != table {
                continue;
            }

            if let Some(values) = Self::index_values_from_tuple(index.columns(), schema, tuple) {
                let key = Self::encode_key(&values);
                match index.index_type() {
                    ArtIndexType::PrimaryKey | ArtIndexType::Unique => {
                        let removed = index.remove(&key)?.is_some();
                        if removed && index.index_type() == ArtIndexType::PrimaryKey && values.len() == 1 {
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
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());

        for index in indexes.values() {
            if index.table() != table {
                continue;
            }

            for column_name in index.columns() {
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

        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        for index in indexes.values() {
            if index.table() != table {
                continue;
            }

            for indexed_column in index.columns() {
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
    pub fn clear_table_indexes(&self, table: &str) {
        let mut indexes = self.indexes.write().unwrap_or_else(|e| e.into_inner());
        for index in indexes.values_mut() {
            if index.table() == table {
                index.clear();
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
        self.stats.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Get statistics for a specific index
    pub fn index_stats(&self, name: &str) -> Option<ArtIndexStats> {
        let indexes = self.indexes.read().unwrap_or_else(|e| e.into_inner());
        indexes.get(name).map(|idx| idx.stats().clone())
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
        manager.on_insert_tuple("users", 1, &schema, &tuple).unwrap();

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
