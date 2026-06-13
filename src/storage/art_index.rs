//! ART (Adaptive Radix Tree) Index Implementation
//!
//! A high-performance in-memory index structure with O(k) lookup time
//! where k is the key length. ART indexes are automatically created for:
//! - Primary Keys (PKs)
//! - Foreign Keys (FKs)
//! - Unique Columns
//!
//! Features:
//! - Adaptive node sizes (4, 16, 48, 256 children)
//! - Path compression for common prefixes
//! - O(k) lookup, insert, delete where k = key length
//! - Memory-efficient for sparse keyspaces
//! - Range and prefix scan support

use super::art_node::*;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fmt;

/// Type of ART index
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtIndexType {
    /// Primary key index (auto-created, enforces uniqueness, NOT NULL)
    PrimaryKey,
    /// Foreign key index (auto-created, for FK lookups)
    ForeignKey,
    /// Unique constraint index (auto-created, enforces uniqueness, allows NULL)
    Unique,
    /// Manually created index via CREATE INDEX
    Manual,
}

impl fmt::Display for ArtIndexType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArtIndexType::PrimaryKey => write!(f, "PRIMARY KEY"),
            ArtIndexType::ForeignKey => write!(f, "FOREIGN KEY"),
            ArtIndexType::Unique => write!(f, "UNIQUE"),
            ArtIndexType::Manual => write!(f, "MANUAL"),
        }
    }
}

/// Error types for ART index operations
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtIndexError {
    /// Duplicate key in unique index
    DuplicateKey(String),
    /// Key not found
    KeyNotFound,
    /// Referenced key not found (FK violation)
    ForeignKeyViolation(String),
    /// Null value in primary key
    NullPrimaryKey,
    /// Index already exists
    IndexAlreadyExists(String),
    /// Index not found
    IndexNotFound(String),
    /// Internal error
    Internal(String),
}

impl fmt::Display for ArtIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArtIndexError::DuplicateKey(key) => write!(f, "Duplicate key: {}", key),
            ArtIndexError::KeyNotFound => write!(f, "Key not found"),
            ArtIndexError::ForeignKeyViolation(msg) => write!(f, "Foreign key violation: {}", msg),
            ArtIndexError::NullPrimaryKey => write!(f, "NULL value not allowed in primary key"),
            ArtIndexError::IndexAlreadyExists(name) => write!(f, "Index '{}' already exists", name),
            ArtIndexError::IndexNotFound(name) => write!(f, "Index '{}' not found", name),
            ArtIndexError::Internal(msg) => write!(f, "Internal error: {}", msg),
        }
    }
}

impl std::error::Error for ArtIndexError {}

/// Result type for ART operations
pub type ArtResult<T> = Result<T, ArtIndexError>;

/// Statistics for an ART index
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArtIndexStats {
    /// Total number of keys in the index
    pub key_count: u64,
    /// Number of Node4 nodes
    pub node4_count: u64,
    /// Number of Node16 nodes
    pub node16_count: u64,
    /// Number of Node48 nodes
    pub node48_count: u64,
    /// Number of Node256 nodes
    pub node256_count: u64,
    /// Number of leaf nodes
    pub leaf_count: u64,
    /// Estimated memory usage in bytes
    pub memory_bytes: u64,
    /// Number of lookups performed
    pub lookup_count: u64,
    /// Number of inserts performed
    pub insert_count: u64,
    /// Number of deletes performed
    pub delete_count: u64,
}

#[derive(Debug, Clone)]
struct DenseIntStats {
    key_width: usize,
    len: usize,
    min: i64,
    max: i64,
    dense: bool,
    valid: bool,
}

impl DenseIntStats {
    fn new(key_width: usize, value: i64) -> Self {
        Self {
            key_width,
            len: 1,
            min: value,
            max: value,
            dense: true,
            valid: true,
        }
    }

    fn insert(&mut self, key_width: usize, value: i64) {
        if !self.valid || self.key_width != key_width {
            self.valid = false;
            return;
        }
        self.len += 1;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.dense = self.is_contiguous();
    }

    fn delete(&mut self, value: i64) {
        if !self.valid || self.len == 0 {
            self.valid = false;
            return;
        }
        if self.len == 1 {
            self.len = 0;
            self.dense = true;
            self.min = 0;
            self.max = 0;
            return;
        }

        self.len -= 1;
        if self.dense {
            if value == self.min {
                self.min = self.min.saturating_add(1);
            } else if value == self.max {
                self.max = self.max.saturating_sub(1);
            } else {
                self.dense = false;
            }
        } else {
            // Without storing every key, deleting from a sparse set can make
            // min/max stale. Keep the stat present but force the exact fallback.
            self.valid = false;
        }
    }

    fn is_contiguous(&self) -> bool {
        if !self.valid {
            return false;
        }
        let span = i128::from(self.max) - i128::from(self.min) + 1;
        span >= 0 && span == self.len as i128
    }

    fn count_range(&self, key_width: usize, lower: Option<(i64, bool)>, upper: Option<(i64, bool)>) -> Option<usize> {
        if !self.valid || !self.dense || self.len == 0 || self.key_width != key_width {
            return None;
        }

        let mut lo = i128::from(self.min);
        let mut hi = i128::from(self.max);
        if let Some((bound, inclusive)) = lower {
            let bound = i128::from(bound) + if inclusive { 0 } else { 1 };
            lo = lo.max(bound);
        }
        if let Some((bound, inclusive)) = upper {
            let bound = i128::from(bound) - if inclusive { 0 } else { 1 };
            hi = hi.min(bound);
        }

        if lo > hi {
            return Some(0);
        }
        usize::try_from(hi - lo + 1).ok()
    }
}

impl ArtIndexStats {
    /// Total number of internal nodes
    pub fn total_nodes(&self) -> u64 {
        self.node4_count + self.node16_count + self.node48_count + self.node256_count
    }
}

/// Adaptive Radix Tree Index
#[derive(Debug, Clone)]
pub struct AdaptiveRadixTree {
    /// Root node of the tree
    root: Option<ArtNode>,
    /// Index name
    name: String,
    /// Table this index belongs to
    table: String,
    /// Columns covered by this index
    columns: Vec<String>,
    /// Type of index
    index_type: ArtIndexType,
    /// Number of keys in the tree
    size: u64,
    /// Statistics
    stats: ArtIndexStats,
    /// Exact O(1) range-count metadata for dense single-column integer PKs.
    dense_int_stats: Option<DenseIntStats>,
}

#[allow(clippy::indexing_slicing)] // SAFETY: key[depth] access bounded by depth < key.len() checks; node child access bounded by node type invariants
impl AdaptiveRadixTree {
    /// Create a new ART index
    pub fn new(name: &str, table: &str, columns: Vec<String>, index_type: ArtIndexType) -> Self {
        Self {
            root: None,
            name: name.to_string(),
            table: table.to_string(),
            columns,
            index_type,
            size: 0,
            stats: ArtIndexStats::default(),
            dense_int_stats: None,
        }
    }

    /// Get the index name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the table name
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Get the columns
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Get the index type
    pub fn index_type(&self) -> ArtIndexType {
        self.index_type
    }

    /// Rename this index (for table rename operations)
    pub fn rename(&mut self, new_table: String, new_name: String) {
        self.table = new_table;
        self.name = new_name;
    }

    /// Get the number of keys
    pub fn len(&self) -> u64 {
        self.size
    }

    /// Check if the index is empty
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Get statistics
    pub fn stats(&self) -> &ArtIndexStats {
        &self.stats
    }

    /// Record a single-column integer primary-key insert for O(1) dense range counts.
    pub fn record_dense_int_insert(&mut self, key_width: usize, value: i64) {
        self.dense_int_stats
            .as_mut()
            .map(|stats| stats.insert(key_width, value))
            .unwrap_or_else(|| self.dense_int_stats = Some(DenseIntStats::new(key_width, value)));
    }

    /// Record a single-column integer primary-key delete.
    pub fn record_dense_int_delete(&mut self, value: i64) {
        if let Some(stats) = self.dense_int_stats.as_mut() {
            stats.delete(value);
            if stats.len == 0 {
                self.dense_int_stats = None;
            }
        }
    }

    /// Count a range from dense integer PK metadata, if the table remains contiguous.
    pub fn dense_int_count(
        &self,
        key_width: usize,
        lower: Option<(i64, bool)>,
        upper: Option<(i64, bool)>,
    ) -> Option<usize> {
        self.dense_int_stats
            .as_ref()
            .and_then(|stats| stats.count_range(key_width, lower, upper))
    }

    /// Insert a key-value pair
    ///
    /// For PK and UNIQUE indexes, fails if key already exists.
    /// For FK and Manual indexes, allows duplicates (updates value).
    pub fn insert(&mut self, key: &[u8], value: RowId) -> ArtResult<()> {
        self.stats.insert_count += 1;

        if key.is_empty() {
            if self.index_type == ArtIndexType::PrimaryKey {
                return Err(ArtIndexError::NullPrimaryKey);
            }
            // Allow empty keys for other index types
        }

        // Check for duplicates in PK and UNIQUE indexes
        if matches!(self.index_type, ArtIndexType::PrimaryKey | ArtIndexType::Unique) {
            if self.contains(key) {
                return Err(ArtIndexError::DuplicateKey(format!(
                    "Key already exists in {} index",
                    self.index_type
                )));
            }
        }

        // Perform the insert
        if self.root.is_none() {
            // Empty tree - create leaf
            self.root = Some(ArtNode::Leaf(LeafNode::new(key.to_vec(), value)));
            self.size = 1;
            self.stats.key_count = 1;
            self.stats.leaf_count = 1;
            return Ok(());
        }

        self.insert_recursive(key, value, 0)?;
        self.size += 1;
        self.stats.key_count = self.size;
        Ok(())
    }

    /// Internal recursive insert
    fn insert_recursive(&mut self, key: &[u8], value: RowId, depth: usize) -> ArtResult<()> {
        let root = self
            .root
            .take()
            .ok_or_else(|| ArtIndexError::Internal("Missing root node during insert".to_string()))?;
        self.root = Some(self.insert_into_node(root, key, value, depth)?);
        Ok(())
    }

    /// Insert into a specific node
    fn insert_into_node(&mut self, mut node: ArtNode, key: &[u8], value: RowId, depth: usize) -> ArtResult<ArtNode> {
        // Handle leaf node
        if let ArtNode::Leaf(ref mut leaf) = node {
            // Same key - add value to multi-value leaf (for non-unique indexes)
            if leaf.matches(key) {
                leaf.push_value(value);
                return Ok(node);
            }

            // Different key - create a new inner node
            // Take ownership of key and values (avoids clone since leaf is being replaced)
            let existing_key = std::mem::take(&mut leaf.key);
            let (primary, extra) = leaf.take_values();

            // Find the common prefix length
            let mut prefix_len = 0;
            while depth + prefix_len < key.len()
                && depth + prefix_len < existing_key.len()
                && key[depth + prefix_len] == existing_key[depth + prefix_len]
            {
                prefix_len += 1;
            }

            // Create new Node4 with common prefix
            let prefix = if prefix_len > 0 {
                &key[depth..depth + prefix_len]
            } else {
                &[]
            };
            let mut new_node = Node4::with_prefix(prefix);

            // Add both leaves as children (or store value at node if key exhausted)
            let new_depth = depth + prefix_len;
            if new_depth < existing_key.len() {
                let child_byte = existing_key[new_depth];
                let existing_leaf = ArtNode::Leaf(LeafNode::from_values(existing_key, primary, extra));
                new_node.add_child(child_byte, existing_leaf);
            } else {
                // Existing key ends at this node - store all values here
                new_node.header.values.push(primary);
                new_node.header.values.extend(extra);
            }
            if new_depth < key.len() {
                let new_leaf = ArtNode::Leaf(LeafNode::new(key.to_vec(), value));
                new_node.add_child(key[new_depth], new_leaf);
            } else {
                // New key ends at this node - add value here
                new_node.header.values.push(value);
            }

            self.stats.node4_count += 1;
            self.stats.leaf_count += 1; // New leaf added
            return Ok(ArtNode::Node4(Box::new(new_node)));
        }

        // Handle inner nodes
        let header = node.header();
        let prefix_len = header.prefix_len as usize;

        // Check prefix match. Long compressed prefixes store only the first
        // MAX_PREFIX_LEN bytes in the node header, so compare the hidden tail
        // against a descendant leaf before deciding that the full prefix
        // matched. Otherwise keys with a long shared prefix can be routed to
        // the wrong child and falsely collide in UNIQUE indexes.
        let mismatch_pos = Self::prefix_mismatch_pos(&node, key, depth);

        // Prefix mismatch - need to split
        if mismatch_pos < prefix_len.min(MAX_PREFIX_LEN) {
            return self.split_node(node, key, value, depth, mismatch_pos);
        }
        if mismatch_pos < prefix_len {
            return self.split_node(node, key, value, depth, mismatch_pos);
        }

        // Full prefix match - continue to child
        let new_depth = depth + prefix_len;
        if new_depth >= key.len() {
            // Key exhausted at inner node - store value here
            if self.index_type == ArtIndexType::PrimaryKey || self.index_type == ArtIndexType::Unique {
                if !node.header().values.is_empty() {
                    return Err(ArtIndexError::DuplicateKey(format!(
                        "Key already exists in {} index '{}'",
                        if self.index_type == ArtIndexType::PrimaryKey {
                            "primary key"
                        } else {
                            "unique"
                        },
                        self.name
                    )));
                }
            }
            node.header_mut().values.push(value);
            return Ok(node);
        }

        let next_byte = key[new_depth];

        // Try to find existing child
        if let Some(_) = node.get_child(next_byte) {
            // Recurse into child
            match &mut node {
                ArtNode::Node4(n) => {
                    if let Some(idx) = n.find_child_index(next_byte) {
                        let child = n.children[idx]
                            .take()
                            .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node4 child".to_string()))?;
                        n.children[idx] = Some(self.insert_into_node(child, key, value, new_depth + 1)?);
                    }
                }
                ArtNode::Node16(n) => {
                    if let Some(idx) = n.find_child_index(next_byte) {
                        let child = n.children[idx]
                            .take()
                            .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node16 child".to_string()))?;
                        n.children[idx] = Some(self.insert_into_node(child, key, value, new_depth + 1)?);
                    }
                }
                ArtNode::Node48(n) => {
                    let idx = n.child_index[next_byte as usize];
                    if idx != 255 {
                        let child = n.children[idx as usize]
                            .take()
                            .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node48 child".to_string()))?;
                        n.children[idx as usize] = Some(self.insert_into_node(child, key, value, new_depth + 1)?);
                    }
                }
                ArtNode::Node256(n) => {
                    let child = n.children[next_byte as usize]
                        .take()
                        .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node256 child".to_string()))?;
                    n.children[next_byte as usize] = Some(self.insert_into_node(child, key, value, new_depth + 1)?);
                }
                ArtNode::Leaf(_) => unreachable!(),
            }
            return Ok(node);
        }

        // No existing child - add new leaf
        let new_leaf = ArtNode::Leaf(LeafNode::new(key.to_vec(), value));
        self.stats.leaf_count += 1;

        // Add child, growing node if necessary
        match node {
            ArtNode::Node4(mut n) => {
                if n.is_full() {
                    let mut grown = n.grow();
                    self.stats.node4_count -= 1;
                    self.stats.node16_count += 1;
                    grown.add_child(next_byte, new_leaf);
                    Ok(ArtNode::Node16(Box::new(grown)))
                } else {
                    n.add_child(next_byte, new_leaf);
                    Ok(ArtNode::Node4(n))
                }
            }
            ArtNode::Node16(mut n) => {
                if n.is_full() {
                    let mut grown = n.grow();
                    self.stats.node16_count -= 1;
                    self.stats.node48_count += 1;
                    grown.add_child(next_byte, new_leaf);
                    Ok(ArtNode::Node48(Box::new(grown)))
                } else {
                    n.add_child(next_byte, new_leaf);
                    Ok(ArtNode::Node16(n))
                }
            }
            ArtNode::Node48(mut n) => {
                if n.is_full() {
                    let mut grown = n.grow();
                    self.stats.node48_count -= 1;
                    self.stats.node256_count += 1;
                    grown.add_child(next_byte, new_leaf);
                    Ok(ArtNode::Node256(Box::new(grown)))
                } else {
                    n.add_child(next_byte, new_leaf);
                    Ok(ArtNode::Node48(n))
                }
            }
            ArtNode::Node256(mut n) => {
                n.add_child(next_byte, new_leaf);
                Ok(ArtNode::Node256(n))
            }
            ArtNode::Leaf(_) => unreachable!(),
        }
    }

    /// Split a node when prefix doesn't match
    fn split_node(
        &mut self,
        mut node: ArtNode,
        key: &[u8],
        value: RowId,
        depth: usize,
        mismatch_pos: usize,
    ) -> ArtResult<ArtNode> {
        let header = node.header();
        let old_prefix_len = header.prefix_len as usize;
        let old_stored_prefix = header.get_prefix().to_vec();
        let representative_key = Self::first_leaf_key(&node).map(<[u8]>::to_vec);

        // Create new parent node with common prefix
        let common_prefix = key
            .get(depth..depth + mismatch_pos)
            .ok_or_else(|| ArtIndexError::Internal("Invalid ART split prefix range".to_string()))?;
        let mut new_parent = Node4::with_prefix(common_prefix);

        // Update the old node's prefix
        let remaining_prefix = Self::prefix_bytes_for_split(
            depth,
            mismatch_pos + 1,
            old_prefix_len,
            &old_stored_prefix,
            representative_key.as_deref(),
        )?;
        node.header_mut().set_prefix(&remaining_prefix);

        // Add old node as child
        let old_key =
            Self::prefix_byte_for_split(depth, mismatch_pos, &old_stored_prefix, representative_key.as_deref())?;
        new_parent.add_child(old_key, node);

        // Add new key - check if key is exhausted (one key is prefix of another)
        let new_key_pos = depth + mismatch_pos;
        if new_key_pos < key.len() {
            // Key has more bytes - add as leaf child
            let new_key = key[new_key_pos];
            let new_leaf = ArtNode::Leaf(LeafNode::new(key.to_vec(), value));
            new_parent.add_child(new_key, new_leaf);
            self.stats.leaf_count += 1;
        } else {
            // Key exhausted at this node - store value in header
            new_parent.header.values.push(value);
        }

        self.stats.node4_count += 1;

        Ok(ArtNode::Node4(Box::new(new_parent)))
    }

    fn first_leaf_key(node: &ArtNode) -> Option<&[u8]> {
        match node {
            ArtNode::Leaf(leaf) => Some(&leaf.key),
            ArtNode::Node4(n) => n.iter_children().find_map(|(_, child)| Self::first_leaf_key(child)),
            ArtNode::Node16(n) => n.iter_children().find_map(|(_, child)| Self::first_leaf_key(child)),
            ArtNode::Node48(n) => n.iter_children().find_map(|(_, child)| Self::first_leaf_key(child)),
            ArtNode::Node256(n) => n.iter_children().find_map(|(_, child)| Self::first_leaf_key(child)),
        }
    }

    fn prefix_mismatch_pos(node: &ArtNode, key: &[u8], depth: usize) -> usize {
        let header = node.header();
        let prefix_len = header.prefix_len as usize;
        let prefix = header.get_prefix();

        let mut mismatch_pos = 0;
        while mismatch_pos < prefix.len()
            && depth + mismatch_pos < key.len()
            && prefix[mismatch_pos] == key[depth + mismatch_pos]
        {
            mismatch_pos += 1;
        }

        if mismatch_pos < prefix.len() || mismatch_pos >= prefix_len {
            return mismatch_pos;
        }

        let Some(representative_key) = Self::first_leaf_key(node) else {
            return mismatch_pos;
        };

        while mismatch_pos < prefix_len
            && depth + mismatch_pos < key.len()
            && depth + mismatch_pos < representative_key.len()
            && representative_key[depth + mismatch_pos] == key[depth + mismatch_pos]
        {
            mismatch_pos += 1;
        }

        mismatch_pos
    }

    fn prefix_byte_for_split(
        depth: usize,
        prefix_pos: usize,
        stored_prefix: &[u8],
        representative_key: Option<&[u8]>,
    ) -> ArtResult<u8> {
        if let Some(byte) = stored_prefix.get(prefix_pos) {
            return Ok(*byte);
        }
        representative_key
            .and_then(|key| key.get(depth + prefix_pos))
            .copied()
            .ok_or_else(|| ArtIndexError::Internal("Cannot recover hidden ART prefix byte".to_string()))
    }

    fn prefix_bytes_for_split(
        depth: usize,
        start_prefix_pos: usize,
        old_prefix_len: usize,
        stored_prefix: &[u8],
        representative_key: Option<&[u8]>,
    ) -> ArtResult<Vec<u8>> {
        let mut bytes = Vec::with_capacity(old_prefix_len.saturating_sub(start_prefix_pos));
        for pos in start_prefix_pos..old_prefix_len {
            bytes.push(Self::prefix_byte_for_split(
                depth,
                pos,
                stored_prefix,
                representative_key,
            )?);
        }
        Ok(bytes)
    }

    /// Get the value for a key
    pub fn get(&self, key: &[u8]) -> Option<RowId> {
        let node = self.root.as_ref()?;
        self.get_recursive(node, key, 0)
    }

    /// Internal recursive get
    #[allow(clippy::self_only_used_in_recursion)]
    fn get_recursive(&self, node: &ArtNode, key: &[u8], depth: usize) -> Option<RowId> {
        match node {
            ArtNode::Leaf(leaf) => {
                if leaf.matches(key) {
                    Some(leaf.value())
                } else {
                    None
                }
            }
            _ => {
                let header = node.header();
                let prefix_len = header.prefix_len as usize;

                if Self::prefix_mismatch_pos(node, key, depth) < prefix_len {
                    return None;
                }

                let new_depth = depth + prefix_len;
                if new_depth >= key.len() {
                    // Key exhausted at inner node - return first stored value if any
                    return header.values.first().copied();
                }

                let next_byte = key[new_depth];
                let child = node.get_child(next_byte)?;
                self.get_recursive(child, key, new_depth + 1)
            }
        }
    }

    /// Get all values for a key (for non-unique indexes with multiple row IDs)
    pub fn get_all(&self, key: &[u8]) -> Vec<RowId> {
        let Some(node) = self.root.as_ref() else {
            return Vec::new();
        };
        self.get_all_recursive(node, key, 0)
    }

    /// Internal recursive get_all
    #[allow(clippy::self_only_used_in_recursion)]
    fn get_all_recursive(&self, node: &ArtNode, key: &[u8], depth: usize) -> Vec<RowId> {
        match node {
            ArtNode::Leaf(leaf) => {
                if leaf.matches(key) {
                    leaf.all_values()
                } else {
                    Vec::new()
                }
            }
            _ => {
                let header = node.header();
                let prefix_len = header.prefix_len as usize;

                if Self::prefix_mismatch_pos(node, key, depth) < prefix_len {
                    return Vec::new();
                }

                let new_depth = depth + prefix_len;
                if new_depth >= key.len() {
                    return header.values.clone();
                }

                let next_byte = key[new_depth];
                let Some(child) = node.get_child(next_byte) else {
                    return Vec::new();
                };
                self.get_all_recursive(child, key, new_depth + 1)
            }
        }
    }

    /// Check if a key exists in the index
    pub fn contains(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    /// Remove a key from the index
    pub fn remove(&mut self, key: &[u8]) -> ArtResult<Option<RowId>> {
        self.stats.delete_count += 1;

        if self.root.is_none() {
            return Ok(None);
        }

        // Take the root to avoid borrow issues
        let root = self
            .root
            .take()
            .ok_or_else(|| ArtIndexError::Internal("Missing root node during remove".to_string()))?;
        let (new_root, removed_value) = self.remove_recursive(root, key, 0)?;
        self.root = new_root;

        if removed_value.is_some() {
            self.size -= 1;
            self.stats.key_count = self.size;
            self.stats.leaf_count -= 1;
        }

        Ok(removed_value)
    }

    /// Internal recursive remove (removes ALL values for the key)
    fn remove_recursive(
        &mut self,
        node: ArtNode,
        key: &[u8],
        depth: usize,
    ) -> ArtResult<(Option<ArtNode>, Option<RowId>)> {
        match node {
            ArtNode::Leaf(leaf) => {
                if leaf.matches(key) {
                    let first_value = Some(leaf.value());
                    let count = leaf.values_count() as u64;
                    // Adjust size for extra values beyond the first (first is handled by caller)
                    if count > 1 {
                        self.size -= count - 1;
                        self.stats.key_count = self.size;
                    }
                    Ok((None, first_value))
                } else {
                    Ok((Some(ArtNode::Leaf(leaf)), None))
                }
            }
            mut inner => {
                let header = inner.header();
                let prefix_len = header.prefix_len as usize;

                if Self::prefix_mismatch_pos(&inner, key, depth) < prefix_len {
                    return Ok((Some(inner), None));
                }

                let new_depth = depth + prefix_len;
                if new_depth >= key.len() {
                    // Key exhausted at inner node - remove all values here
                    let values = std::mem::take(&mut inner.header_mut().values);
                    let first_value = values.first().copied();
                    // Adjust size for extra values beyond the first
                    if values.len() > 1 {
                        self.size -= (values.len() - 1) as u64;
                        self.stats.key_count = self.size;
                    }
                    return Ok((Some(inner), first_value));
                }

                let next_byte = key[new_depth];

                // Remove from child
                let removed = match &mut inner {
                    ArtNode::Node4(n) => {
                        if let Some(idx) = n.find_child_index(next_byte) {
                            let child = n.children[idx]
                                .take()
                                .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node4 child".to_string()))?;
                            let (new_child, value) = self.remove_recursive(child, key, new_depth + 1)?;
                            if new_child.is_some() {
                                n.children[idx] = new_child;
                            } else {
                                // Child was deleted
                                n.remove_child(next_byte);
                            }
                            value
                        } else {
                            None
                        }
                    }
                    ArtNode::Node16(n) => {
                        if let Some(idx) = n.find_child_index(next_byte) {
                            let child = n.children[idx]
                                .take()
                                .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node16 child".to_string()))?;
                            let (new_child, value) = self.remove_recursive(child, key, new_depth + 1)?;
                            if new_child.is_some() {
                                n.children[idx] = new_child;
                            } else {
                                n.remove_child(next_byte);
                            }
                            value
                        } else {
                            None
                        }
                    }
                    ArtNode::Node48(n) => {
                        let idx = n.child_index[next_byte as usize];
                        if idx != 255 {
                            let child = n.children[idx as usize]
                                .take()
                                .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node48 child".to_string()))?;
                            let (new_child, value) = self.remove_recursive(child, key, new_depth + 1)?;
                            if new_child.is_some() {
                                n.children[idx as usize] = new_child;
                            } else {
                                n.remove_child(next_byte);
                            }
                            value
                        } else {
                            None
                        }
                    }
                    ArtNode::Node256(n) => {
                        if let Some(child) = n.children[next_byte as usize].take() {
                            let (new_child, value) = self.remove_recursive(child, key, new_depth + 1)?;
                            n.children[next_byte as usize] = new_child;
                            if n.children[next_byte as usize].is_none() {
                                n.header.num_children -= 1;
                            }
                            value
                        } else {
                            None
                        }
                    }
                    ArtNode::Leaf(_) => unreachable!(),
                };

                // Shrink node if necessary
                let final_node = self.maybe_shrink_node(inner);
                Ok((Some(final_node), removed))
            }
        }
    }

    /// Remove a specific row_id value for a key (for non-unique indexes)
    /// Only removes the leaf/node if no values remain
    pub fn remove_value(&mut self, key: &[u8], row_id: RowId) -> ArtResult<bool> {
        self.stats.delete_count += 1;

        if self.root.is_none() {
            return Ok(false);
        }

        let root = self
            .root
            .take()
            .ok_or_else(|| ArtIndexError::Internal("Missing root node during remove_value".to_string()))?;
        let (new_root, removed) = self.remove_value_recursive(root, key, row_id, 0)?;
        self.root = new_root;

        if removed {
            self.size -= 1;
            self.stats.key_count = self.size;
        }

        Ok(removed)
    }

    /// Internal recursive remove_value - removes a specific row_id from a key's values
    fn remove_value_recursive(
        &mut self,
        node: ArtNode,
        key: &[u8],
        row_id: RowId,
        depth: usize,
    ) -> ArtResult<(Option<ArtNode>, bool)> {
        match node {
            ArtNode::Leaf(mut leaf) => {
                if leaf.matches(key) {
                    let (removed, now_empty) = leaf.remove_value(row_id);
                    if removed && now_empty {
                        // No values left - remove the leaf entirely
                        self.stats.leaf_count -= 1;
                        Ok((None, true))
                    } else if removed {
                        Ok((Some(ArtNode::Leaf(leaf)), true))
                    } else {
                        Ok((Some(ArtNode::Leaf(leaf)), false))
                    }
                } else {
                    Ok((Some(ArtNode::Leaf(leaf)), false))
                }
            }
            mut inner => {
                let header = inner.header();
                let prefix_len = header.prefix_len as usize;

                if Self::prefix_mismatch_pos(&inner, key, depth) < prefix_len {
                    return Ok((Some(inner), false));
                }

                let new_depth = depth + prefix_len;
                if new_depth >= key.len() {
                    // Key exhausted at inner node - remove specific value
                    let values = &mut inner.header_mut().values;
                    if let Some(pos) = values.iter().position(|&v| v == row_id) {
                        values.swap_remove(pos);
                        return Ok((Some(inner), true));
                    }
                    return Ok((Some(inner), false));
                }

                let next_byte = key[new_depth];

                let removed = match &mut inner {
                    ArtNode::Node4(n) => {
                        if let Some(idx) = n.find_child_index(next_byte) {
                            let child = n.children[idx]
                                .take()
                                .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node4 child".to_string()))?;
                            let (new_child, removed) =
                                self.remove_value_recursive(child, key, row_id, new_depth + 1)?;
                            if new_child.is_some() {
                                n.children[idx] = new_child;
                            } else {
                                n.remove_child(next_byte);
                            }
                            removed
                        } else {
                            false
                        }
                    }
                    ArtNode::Node16(n) => {
                        if let Some(idx) = n.find_child_index(next_byte) {
                            let child = n.children[idx]
                                .take()
                                .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node16 child".to_string()))?;
                            let (new_child, removed) =
                                self.remove_value_recursive(child, key, row_id, new_depth + 1)?;
                            if new_child.is_some() {
                                n.children[idx] = new_child;
                            } else {
                                n.remove_child(next_byte);
                            }
                            removed
                        } else {
                            false
                        }
                    }
                    ArtNode::Node48(n) => {
                        let idx = n.child_index[next_byte as usize];
                        if idx != 255 {
                            let child = n.children[idx as usize]
                                .take()
                                .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node48 child".to_string()))?;
                            let (new_child, removed) =
                                self.remove_value_recursive(child, key, row_id, new_depth + 1)?;
                            if new_child.is_some() {
                                n.children[idx as usize] = new_child;
                            } else {
                                n.remove_child(next_byte);
                            }
                            removed
                        } else {
                            false
                        }
                    }
                    ArtNode::Node256(n) => {
                        if let Some(child) = n.children[next_byte as usize].take() {
                            let (new_child, removed) =
                                self.remove_value_recursive(child, key, row_id, new_depth + 1)?;
                            n.children[next_byte as usize] = new_child;
                            if n.children[next_byte as usize].is_none() {
                                n.header.num_children -= 1;
                            }
                            removed
                        } else {
                            false
                        }
                    }
                    ArtNode::Leaf(_) => unreachable!(),
                };

                let final_node = self.maybe_shrink_node(inner);
                Ok((Some(final_node), removed))
            }
        }
    }

    /// Shrink a node if it has too few children
    fn maybe_shrink_node(&mut self, node: ArtNode) -> ArtNode {
        match node {
            ArtNode::Node16(n) if n.should_shrink() => {
                self.stats.node16_count -= 1;
                self.stats.node4_count += 1;
                ArtNode::Node4(Box::new(n.shrink()))
            }
            ArtNode::Node48(n) if n.should_shrink() => {
                self.stats.node48_count -= 1;
                self.stats.node16_count += 1;
                ArtNode::Node16(Box::new(n.shrink()))
            }
            ArtNode::Node256(n) if n.should_shrink() => {
                self.stats.node256_count -= 1;
                self.stats.node48_count += 1;
                ArtNode::Node48(Box::new(n.shrink()))
            }
            other => other,
        }
    }

    /// Iterate over all key-value pairs in order
    pub fn iter(&self) -> ArtIterator<'_> {
        ArtIterator::new(self)
    }

    /// Range scan from start (inclusive) to end (exclusive).
    ///
    /// R4.4: now a tree-guided bounded scan (subtree pruning) instead of a
    /// full-tree iteration + filter — O(log n + k) instead of O(n).
    pub fn range(&self, start: &[u8], end: &[u8]) -> impl Iterator<Item = (Vec<u8>, RowId)> {
        self.range_scan(Some((start, true)), Some((end, false)), None)
            .into_iter()
    }

    /// Prefix scan - find all keys with the given prefix
    pub fn prefix_scan<'a>(&'a self, prefix: &'a [u8]) -> impl Iterator<Item = (Vec<u8>, RowId)> + 'a {
        self.iter().filter(move |(k, _)| k.starts_with(prefix))
    }

    /// R4.4: ordered, bounded range scan.
    ///
    /// Returns `(key, row_id)` pairs in ascending key order, restricted to
    /// `lower`/`upper` (each `(bound, inclusive)`, `None` = unbounded), with
    /// an optional result cap. Subtrees that cannot intersect the bounds are
    /// pruned at descent time, so cost is O(log n + k) rather than the full
    /// tree walk `range()` used to do. Encoding v2 keys are order-preserving
    /// per column type, so key order here equals value order.
    pub fn range_scan(
        &self,
        lower: Option<(&[u8], bool)>,
        upper: Option<(&[u8], bool)>,
        limit: Option<usize>,
    ) -> Vec<(Vec<u8>, RowId)> {
        let mut out = Vec::new();
        let Some(root) = &self.root else {
            return out;
        };
        let cap = limit.unwrap_or(usize::MAX);
        if cap == 0 {
            return out;
        }
        let mut stack: VecDeque<(&ArtNode, Vec<u8>)> = VecDeque::new();
        stack.push_front((root, Vec::new()));

        while let Some((node, key_prefix)) = stack.pop_front() {
            if out.len() >= cap {
                break;
            }
            if let ArtNode::Leaf(leaf) = node {
                if Self::key_in_range(&leaf.key, lower, upper) {
                    for v in leaf.values_iter() {
                        out.push((leaf.key.clone(), v));
                        if out.len() >= cap {
                            break;
                        }
                    }
                }
                continue;
            }

            // Inner node: extend the accumulated key with this node's
            // compressed prefix, then prune the whole subtree if possible.
            let header = node.header();
            let mut node_key = key_prefix;
            node_key.extend_from_slice(header.get_prefix());
            if !Self::prefix_may_intersect(&node_key, lower, upper) {
                continue;
            }

            // Inner-node values represent the key == node_key itself, which
            // sorts before every extension below it.
            if !header.values.is_empty() && Self::key_in_range(&node_key, lower, upper) {
                for &v in &header.values {
                    out.push((node_key.clone(), v));
                    if out.len() >= cap {
                        break;
                    }
                }
            }

            let mut children: Vec<(u8, &ArtNode)> = match node {
                ArtNode::Node4(n) => n.iter_children().collect(),
                ArtNode::Node16(n) => n.iter_children().collect(),
                ArtNode::Node48(n) => n.iter_children().collect(),
                ArtNode::Node256(n) => n.iter_children().collect(),
                ArtNode::Leaf(_) => unreachable!("leaf handled above"),
            };
            // Node4/16 store children in insertion order; sort so the DFS
            // yields keys in ascending byte order.
            children.sort_unstable_by_key(|(byte, _)| *byte);
            for (byte, child) in children.into_iter().rev() {
                let mut child_key = node_key.clone();
                child_key.push(byte);
                if Self::prefix_may_intersect(&child_key, lower, upper) {
                    stack.push_front((child, child_key));
                }
            }
        }
        out
    }

    /// Exact bound check for a complete key.
    fn key_in_range(key: &[u8], lower: Option<(&[u8], bool)>, upper: Option<(&[u8], bool)>) -> bool {
        if let Some((bound, inclusive)) = lower {
            match key.cmp(bound) {
                std::cmp::Ordering::Less => return false,
                std::cmp::Ordering::Equal if !inclusive => return false,
                _ => {}
            }
        }
        if let Some((bound, inclusive)) = upper {
            match key.cmp(bound) {
                std::cmp::Ordering::Greater => return false,
                std::cmp::Ordering::Equal if !inclusive => return false,
                _ => {}
            }
        }
        true
    }

    /// Conservative subtree pruning test: every key in the subtree starts
    /// with `prefix`, so the subtree's smallest possible key is `prefix`
    /// itself and its keys are unbounded above within that prefix. Returns
    /// false only when NO key with this prefix can satisfy the bounds; exact
    /// per-key filtering still happens in [`Self::key_in_range`].
    fn prefix_may_intersect(prefix: &[u8], lower: Option<(&[u8], bool)>, upper: Option<(&[u8], bool)>) -> bool {
        if let Some((bound, _)) = lower {
            let m = prefix.len().min(bound.len());
            // If the prefix is already byte-wise below the bound's prefix,
            // every extension stays below the bound.
            #[allow(clippy::indexing_slicing)] // SAFETY: m = min(lengths)
            if prefix[..m] < bound[..m] {
                return false;
            }
        }
        if let Some((bound, inclusive)) = upper {
            let m = prefix.len().min(bound.len());
            #[allow(clippy::indexing_slicing)] // SAFETY: m = min(lengths)
            match prefix[..m].cmp(&bound[..m]) {
                std::cmp::Ordering::Greater => return false,
                std::cmp::Ordering::Equal => {
                    // prefix extends past (or equals) the bound: prefix >= bound.
                    if prefix.len() > bound.len() || (prefix.len() == bound.len() && !inclusive) {
                        return false;
                    }
                }
                std::cmp::Ordering::Less => {}
            }
        }
        true
    }

    /// Clear all entries from the index
    pub fn clear(&mut self) {
        self.root = None;
        self.size = 0;
        self.stats = ArtIndexStats::default();
        self.dense_int_stats = None;
    }
}

/// Iterator over ART key-value pairs
pub struct ArtIterator<'a> {
    /// Stack of nodes to visit (node, key_prefix)
    stack: VecDeque<(&'a ArtNode, Vec<u8>)>,
    /// Pending values to yield (from multi-value leaves or inner nodes)
    pending_values: VecDeque<(Vec<u8>, RowId)>,
}

impl<'a> ArtIterator<'a> {
    fn new(tree: &'a AdaptiveRadixTree) -> Self {
        let mut stack = VecDeque::new();
        if let Some(root) = &tree.root {
            stack.push_back((root, Vec::new()));
        }
        Self {
            stack,
            pending_values: VecDeque::new(),
        }
    }
}

#[allow(clippy::indexing_slicing)] // SAFETY: node child indexing bounded by node type invariants
impl Iterator for ArtIterator<'_> {
    type Item = (Vec<u8>, RowId);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.pending_values.pop_front() {
            return Some(item);
        }

        while let Some((node, key_prefix)) = self.stack.pop_front() {
            match node {
                ArtNode::Leaf(leaf) => {
                    // Fast path: single value (common case for unique indexes)
                    if leaf.values_count() == 1 {
                        return Some((leaf.key.clone(), leaf.value()));
                    }
                    // Multi-value: queue all values
                    for v in leaf.values_iter() {
                        self.pending_values.push_back((leaf.key.clone(), v));
                    }
                    if let Some(item) = self.pending_values.pop_front() {
                        return Some(item);
                    }
                }
                ArtNode::Node4(n) => {
                    let mut node_key = key_prefix.clone();
                    node_key.extend_from_slice(n.header.get_prefix());

                    if let Some(&value) = n.header.values.first() {
                        if n.header.values.len() == 1 {
                            self.pending_values.push_back((node_key.clone(), value));
                        } else {
                            for &v in &n.header.values {
                                self.pending_values.push_back((node_key.clone(), v));
                            }
                        }
                    }

                    // R4.4: Node4 stores children in insertion order — sort
                    // by key byte so iteration yields ascending key order.
                    let mut children: Vec<_> = n.iter_children().collect();
                    children.sort_unstable_by_key(|(byte, _)| *byte);
                    for (byte, child) in children.into_iter().rev() {
                        let mut child_key = node_key.clone();
                        child_key.push(byte);
                        self.stack.push_front((child, child_key));
                    }

                    if let Some(item) = self.pending_values.pop_front() {
                        return Some(item);
                    }
                }
                ArtNode::Node16(n) => {
                    let mut node_key = key_prefix.clone();
                    node_key.extend_from_slice(n.header.get_prefix());

                    for &v in &n.header.values {
                        self.pending_values.push_back((node_key.clone(), v));
                    }

                    // R4.4: Node16 stores children in insertion order — sort
                    // by key byte so iteration yields ascending key order.
                    let mut children: Vec<_> = n.iter_children().collect();
                    children.sort_unstable_by_key(|(byte, _)| *byte);
                    for (byte, child) in children.into_iter().rev() {
                        let mut child_key = node_key.clone();
                        child_key.push(byte);
                        self.stack.push_front((child, child_key));
                    }

                    if let Some(item) = self.pending_values.pop_front() {
                        return Some(item);
                    }
                }
                ArtNode::Node48(n) => {
                    let mut node_key = key_prefix.clone();
                    node_key.extend_from_slice(n.header.get_prefix());

                    for &v in &n.header.values {
                        self.pending_values.push_back((node_key.clone(), v));
                    }

                    let children: Vec<_> = n.iter_children().collect();
                    for (byte, child) in children.into_iter().rev() {
                        let mut child_key = node_key.clone();
                        child_key.push(byte);
                        self.stack.push_front((child, child_key));
                    }

                    if let Some(item) = self.pending_values.pop_front() {
                        return Some(item);
                    }
                }
                ArtNode::Node256(n) => {
                    let mut node_key = key_prefix.clone();
                    node_key.extend_from_slice(n.header.get_prefix());

                    for &v in &n.header.values {
                        self.pending_values.push_back((node_key.clone(), v));
                    }

                    let children: Vec<_> = n.iter_children().collect();
                    for (byte, child) in children.into_iter().rev() {
                        let mut child_key = node_key.clone();
                        child_key.push(byte);
                        self.stack.push_front((child, child_key));
                    }

                    if let Some(item) = self.pending_values.pop_front() {
                        return Some(item);
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_insert_get() {
        let mut tree = AdaptiveRadixTree::new("test_idx", "test_table", vec!["id".to_string()], ArtIndexType::Manual);

        tree.insert(b"hello", 1).unwrap();
        tree.insert(b"world", 2).unwrap();
        tree.insert(b"helios", 3).unwrap();

        assert_eq!(tree.get(b"hello"), Some(1));
        assert_eq!(tree.get(b"world"), Some(2));
        assert_eq!(tree.get(b"helios"), Some(3));
        assert_eq!(tree.get(b"notfound"), None);
    }

    #[test]
    fn test_primary_key_uniqueness() {
        let mut tree = AdaptiveRadixTree::new("pk_idx", "users", vec!["id".to_string()], ArtIndexType::PrimaryKey);

        tree.insert(b"user1", 1).unwrap();

        // Duplicate should fail
        let result = tree.insert(b"user1", 2);
        assert!(matches!(result, Err(ArtIndexError::DuplicateKey(_))));
    }

    #[test]
    fn test_unique_constraint() {
        let mut tree = AdaptiveRadixTree::new("email_idx", "users", vec!["email".to_string()], ArtIndexType::Unique);

        tree.insert(b"alice@example.com", 1).unwrap();

        // Duplicate should fail
        let result = tree.insert(b"alice@example.com", 2);
        assert!(matches!(result, Err(ArtIndexError::DuplicateKey(_))));

        // Different key should succeed
        tree.insert(b"bob@example.com", 2).unwrap();
    }

    #[test]
    fn test_remove() {
        let mut tree = AdaptiveRadixTree::new("test_idx", "test_table", vec!["id".to_string()], ArtIndexType::Manual);

        tree.insert(b"key1", 1).unwrap();
        tree.insert(b"key2", 2).unwrap();
        tree.insert(b"key3", 3).unwrap();

        assert_eq!(tree.len(), 3);

        let removed = tree.remove(b"key2").unwrap();
        assert_eq!(removed, Some(2));
        assert_eq!(tree.len(), 2);
        assert_eq!(tree.get(b"key2"), None);

        // Remove non-existent key
        let removed = tree.remove(b"notfound").unwrap();
        assert_eq!(removed, None);
    }

    #[test]
    fn test_iteration() {
        let mut tree = AdaptiveRadixTree::new("test_idx", "test_table", vec!["id".to_string()], ArtIndexType::Manual);

        tree.insert(b"c", 3).unwrap();
        tree.insert(b"a", 1).unwrap();
        tree.insert(b"b", 2).unwrap();

        let mut results: Vec<_> = tree.iter().collect();
        results.sort_by_key(|(k, _)| k.clone());

        assert_eq!(results.len(), 3);
        assert_eq!(results[0], (b"a".to_vec(), 1));
        assert_eq!(results[1], (b"b".to_vec(), 2));
        assert_eq!(results[2], (b"c".to_vec(), 3));
    }

    #[test]
    fn test_prefix_scan() {
        let mut tree = AdaptiveRadixTree::new("test_idx", "test_table", vec!["path".to_string()], ArtIndexType::Manual);

        tree.insert(b"/users/alice", 1).unwrap();
        tree.insert(b"/users/bob", 2).unwrap();
        tree.insert(b"/posts/1", 3).unwrap();
        tree.insert(b"/posts/2", 4).unwrap();

        let users: Vec<_> = tree.prefix_scan(b"/users/").collect();
        assert_eq!(users.len(), 2);

        let posts: Vec<_> = tree.prefix_scan(b"/posts/").collect();
        assert_eq!(posts.len(), 2);
    }

    #[test]
    fn test_range_scan() {
        let mut tree = AdaptiveRadixTree::new("test_idx", "test_table", vec!["id".to_string()], ArtIndexType::Manual);

        tree.insert(b"a", 1).unwrap();
        tree.insert(b"b", 2).unwrap();
        tree.insert(b"c", 3).unwrap();
        tree.insert(b"d", 4).unwrap();
        tree.insert(b"e", 5).unwrap();

        let range: Vec<_> = tree.range(b"b", b"e").collect();
        assert_eq!(range.len(), 3); // b, c, d
    }

    #[test]
    fn test_many_keys() {
        let mut tree = AdaptiveRadixTree::new("test_idx", "test_table", vec!["id".to_string()], ArtIndexType::Manual);

        // Insert 1000 keys
        for i in 0..1000u64 {
            let key = format!("key_{:06}", i);
            tree.insert(key.as_bytes(), i).unwrap();
        }

        assert_eq!(tree.len(), 1000);

        // Verify all keys exist
        for i in 0..1000u64 {
            let key = format!("key_{:06}", i);
            assert_eq!(tree.get(key.as_bytes()), Some(i));
        }
    }

    #[test]
    fn test_node_growth() {
        let mut tree = AdaptiveRadixTree::new("test_idx", "test_table", vec!["id".to_string()], ArtIndexType::Manual);

        // Insert enough keys to trigger node growth
        // Start with single character keys to force growth
        for i in 0..100u8 {
            let key = [i];
            tree.insert(&key, i as u64).unwrap();
        }

        assert_eq!(tree.len(), 100);
        assert!(tree.stats().node256_count > 0 || tree.stats().node48_count > 0);
    }

    #[test]
    fn test_prefix_key() {
        // Test case where one key is a prefix of another
        let mut tree = AdaptiveRadixTree::new("test_idx", "test_table", vec!["path".to_string()], ArtIndexType::Manual);

        // Insert longer key first
        tree.insert(b"/users/admin", 1).unwrap();
        // Insert prefix key (shorter)
        tree.insert(b"/users", 2).unwrap();
        // Insert even shorter prefix
        tree.insert(b"/", 3).unwrap();

        // All keys should be retrievable
        assert_eq!(tree.get(b"/users/admin"), Some(1));
        assert_eq!(tree.get(b"/users"), Some(2));
        assert_eq!(tree.get(b"/"), Some(3));
        assert_eq!(tree.len(), 3);

        // Iterate should return all values
        let items: Vec<_> = tree.iter().collect();
        assert_eq!(items.len(), 3);

        // Remove prefix key
        assert_eq!(tree.remove(b"/users").unwrap(), Some(2));
        assert_eq!(tree.get(b"/users"), None);
        assert_eq!(tree.get(b"/users/admin"), Some(1)); // Longer key still works
    }

    #[test]
    fn test_prefix_key_reverse_order() {
        // Test inserting prefix first, then longer key
        let mut tree = AdaptiveRadixTree::new("test_idx", "test_table", vec!["path".to_string()], ArtIndexType::Manual);

        tree.insert(b"/api", 1).unwrap();
        tree.insert(b"/api/v1", 2).unwrap();
        tree.insert(b"/api/v1/users", 3).unwrap();

        assert_eq!(tree.get(b"/api"), Some(1));
        assert_eq!(tree.get(b"/api/v1"), Some(2));
        assert_eq!(tree.get(b"/api/v1/users"), Some(3));
        assert_eq!(tree.len(), 3);
    }

    #[test]
    fn test_multi_value_insert() {
        // Non-unique (Manual) index should support multiple row_ids per key
        let mut tree = AdaptiveRadixTree::new("idx", "orders", vec!["user_id".to_string()], ArtIndexType::Manual);

        // Insert same key with different row_ids
        tree.insert(b"user42", 100).unwrap();
        tree.insert(b"user42", 200).unwrap();
        tree.insert(b"user42", 300).unwrap();

        // get() returns first value
        assert_eq!(tree.get(b"user42"), Some(100));

        // get_all() returns all values
        let all = tree.get_all(b"user42");
        assert_eq!(all.len(), 3);
        assert!(all.contains(&100));
        assert!(all.contains(&200));
        assert!(all.contains(&300));

        // len() counts total entries
        assert_eq!(tree.len(), 3);
    }

    #[test]
    fn test_multi_value_remove_specific() {
        let mut tree = AdaptiveRadixTree::new("idx", "orders", vec!["user_id".to_string()], ArtIndexType::Manual);

        tree.insert(b"user42", 100).unwrap();
        tree.insert(b"user42", 200).unwrap();
        tree.insert(b"user42", 300).unwrap();

        // Remove specific row_id
        assert!(tree.remove_value(b"user42", 200).unwrap());

        let all = tree.get_all(b"user42");
        assert_eq!(all.len(), 2);
        assert!(all.contains(&100));
        assert!(!all.contains(&200));
        assert!(all.contains(&300));
        assert_eq!(tree.len(), 2);

        // Remove non-existent row_id
        assert!(!tree.remove_value(b"user42", 999).unwrap());

        // Remove remaining values
        assert!(tree.remove_value(b"user42", 100).unwrap());
        assert!(tree.remove_value(b"user42", 300).unwrap());

        assert_eq!(tree.get(b"user42"), None);
        assert_eq!(tree.len(), 0);
    }

    #[test]
    fn test_multi_value_iteration() {
        let mut tree = AdaptiveRadixTree::new("idx", "orders", vec!["user_id".to_string()], ArtIndexType::Manual);

        tree.insert(b"key_a", 1).unwrap();
        tree.insert(b"key_a", 2).unwrap();
        tree.insert(b"key_b", 3).unwrap();
        tree.insert(b"key_b", 4).unwrap();
        tree.insert(b"key_b", 5).unwrap();

        let results: Vec<_> = tree.iter().collect();
        assert_eq!(results.len(), 5);

        // All key_a values present
        let key_a_vals: Vec<_> = results.iter().filter(|(k, _)| k == b"key_a").map(|(_, v)| *v).collect();
        assert_eq!(key_a_vals.len(), 2);
        assert!(key_a_vals.contains(&1));
        assert!(key_a_vals.contains(&2));

        // All key_b values present
        let key_b_vals: Vec<_> = results.iter().filter(|(k, _)| k == b"key_b").map(|(_, v)| *v).collect();
        assert_eq!(key_b_vals.len(), 3);
    }

    #[test]
    fn test_multi_value_fk_index() {
        // Simulate FK index behavior
        let mut tree = AdaptiveRadixTree::new("idx", "orders", vec!["user_id".to_string()], ArtIndexType::ForeignKey);

        // Multiple orders for same user
        tree.insert(b"\x00\x00\x00\x2a", 1).unwrap(); // user_id=42, row 1
        tree.insert(b"\x00\x00\x00\x2a", 2).unwrap(); // user_id=42, row 2
        tree.insert(b"\x00\x00\x00\x2a", 3).unwrap(); // user_id=42, row 3
        tree.insert(b"\x00\x00\x00\x01", 4).unwrap(); // user_id=1, row 4

        // All 4 entries stored
        assert_eq!(tree.len(), 4);

        // Lookup by key
        let user42_orders = tree.get_all(b"\x00\x00\x00\x2a");
        assert_eq!(user42_orders.len(), 3);

        let user1_orders = tree.get_all(b"\x00\x00\x00\x01");
        assert_eq!(user1_orders.len(), 1);

        // Remove specific order
        tree.remove_value(b"\x00\x00\x00\x2a", 2).unwrap();
        let user42_orders = tree.get_all(b"\x00\x00\x00\x2a");
        assert_eq!(user42_orders.len(), 2);
        assert_eq!(tree.len(), 3);
    }
}
