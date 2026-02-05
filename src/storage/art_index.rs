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
}

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
                return Err(ArtIndexError::DuplicateKey(
                    format!("Key already exists in {} index", self.index_type)
                ));
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
        let root = self.root.take()
            .ok_or_else(|| ArtIndexError::Internal("Missing root node during insert".to_string()))?;
        self.root = Some(self.insert_into_node(root, key, value, depth)?);
        Ok(())
    }

    /// Insert into a specific node
    fn insert_into_node(&mut self, mut node: ArtNode, key: &[u8], value: RowId, depth: usize) -> ArtResult<ArtNode> {
        // Handle leaf node
        if let ArtNode::Leaf(ref leaf) = node {
            // If same key, this is a duplicate (already checked above)
            // Create a new inner node
            let existing_key = &leaf.key;
            let existing_value = leaf.value;

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
                let existing_leaf = ArtNode::Leaf(LeafNode::new(existing_key.clone(), existing_value));
                new_node.add_child(existing_key[new_depth], existing_leaf);
            } else {
                // Existing key ends at this node - store value here
                new_node.header.value = Some(existing_value);
            }
            if new_depth < key.len() {
                let new_leaf = ArtNode::Leaf(LeafNode::new(key.to_vec(), value));
                new_node.add_child(key[new_depth], new_leaf);
            } else {
                // New key ends at this node - store value here
                new_node.header.value = Some(value);
            }

            self.stats.node4_count += 1;
            self.stats.leaf_count += 1; // New leaf added
            return Ok(ArtNode::Node4(Box::new(new_node)));
        }

        // Handle inner nodes
        let header = node.header();
        let prefix_len = header.prefix_len as usize;
        let prefix = header.get_prefix();

        // Check prefix match
        let mut mismatch_pos = 0;
        while mismatch_pos < prefix_len.min(MAX_PREFIX_LEN)
            && depth + mismatch_pos < key.len()
            && prefix[mismatch_pos] == key[depth + mismatch_pos]
        {
            mismatch_pos += 1;
        }

        // Prefix mismatch - need to split
        if mismatch_pos < prefix_len.min(MAX_PREFIX_LEN) {
            return self.split_node(node, key, value, depth, mismatch_pos);
        }

        // Full prefix match - continue to child
        let new_depth = depth + prefix_len;
        if new_depth >= key.len() {
            // Key exhausted at inner node - store value here
            if self.index_type == ArtIndexType::PrimaryKey || self.index_type == ArtIndexType::Unique {
                if node.header().value.is_some() {
                    return Err(ArtIndexError::DuplicateKey(format!(
                        "Key already exists in {} index '{}'",
                        if self.index_type == ArtIndexType::PrimaryKey { "primary key" } else { "unique" },
                        self.name
                    )));
                }
            }
            node.header_mut().value = Some(value);
            self.size += 1;
            return Ok(node);
        }

        let next_byte = key[new_depth];

        // Try to find existing child
        if let Some(_) = node.get_child(next_byte) {
            // Recurse into child
            match &mut node {
                ArtNode::Node4(n) => {
                    if let Some(idx) = n.find_child_index(next_byte) {
                        let child = n.children[idx].take()
                            .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node4 child".to_string()))?;
                        n.children[idx] = Some(self.insert_into_node(child, key, value, new_depth + 1)?);
                    }
                }
                ArtNode::Node16(n) => {
                    if let Some(idx) = n.find_child_index(next_byte) {
                        let child = n.children[idx].take()
                            .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node16 child".to_string()))?;
                        n.children[idx] = Some(self.insert_into_node(child, key, value, new_depth + 1)?);
                    }
                }
                ArtNode::Node48(n) => {
                    let idx = n.child_index[next_byte as usize];
                    if idx != 255 {
                        let child = n.children[idx as usize].take()
                            .ok_or_else(|| ArtIndexError::Internal("Inconsistent Node48 child".to_string()))?;
                        n.children[idx as usize] = Some(self.insert_into_node(child, key, value, new_depth + 1)?);
                    }
                }
                ArtNode::Node256(n) => {
                    let child = n.children[next_byte as usize].take()
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
    fn split_node(&mut self, mut node: ArtNode, key: &[u8], value: RowId, depth: usize, mismatch_pos: usize) -> ArtResult<ArtNode> {
        let header = node.header();
        let old_prefix = header.get_prefix().to_vec();
        let old_prefix_len = header.prefix_len as usize;

        // Create new parent node with common prefix
        let common_prefix = &old_prefix[..mismatch_pos];
        let mut new_parent = Node4::with_prefix(common_prefix);

        // Update the old node's prefix
        let remaining_prefix = if mismatch_pos + 1 < old_prefix_len {
            old_prefix[mismatch_pos + 1..old_prefix_len.min(MAX_PREFIX_LEN)].to_vec()
        } else {
            vec![]
        };
        node.header_mut().set_prefix(&remaining_prefix);
        node.header_mut().prefix_len = (old_prefix_len - mismatch_pos - 1) as u32;

        // Add old node as child
        let old_key = old_prefix[mismatch_pos];
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
            new_parent.header.value = Some(value);
        }

        self.stats.node4_count += 1;

        Ok(ArtNode::Node4(Box::new(new_parent)))
    }

    /// Get the value for a key
    pub fn get(&self, key: &[u8]) -> Option<RowId> {
        let node = self.root.as_ref()?;
        self.get_recursive(node, key, 0)
    }

    /// Internal recursive get
    fn get_recursive(&self, node: &ArtNode, key: &[u8], depth: usize) -> Option<RowId> {
        match node {
            ArtNode::Leaf(leaf) => {
                if leaf.matches(key) {
                    Some(leaf.value)
                } else {
                    None
                }
            }
            _ => {
                let header = node.header();
                let prefix_len = header.prefix_len as usize;
                let prefix = header.get_prefix();

                // Check prefix
                for i in 0..prefix_len.min(MAX_PREFIX_LEN) {
                    if depth + i >= key.len() || prefix[i] != key[depth + i] {
                        return None;
                    }
                }

                let new_depth = depth + prefix_len;
                if new_depth >= key.len() {
                    // Key exhausted at inner node - return stored value if any
                    return header.value;
                }

                let next_byte = key[new_depth];
                let child = node.get_child(next_byte)?;
                self.get_recursive(child, key, new_depth + 1)
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
        let root = self.root.take()
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

    /// Internal recursive remove
    fn remove_recursive(&mut self, node: ArtNode, key: &[u8], depth: usize) -> ArtResult<(Option<ArtNode>, Option<RowId>)> {
        match node {
            ArtNode::Leaf(leaf) => {
                if leaf.matches(key) {
                    Ok((None, Some(leaf.value)))
                } else {
                    Ok((Some(ArtNode::Leaf(leaf)), None))
                }
            }
            mut inner => {
                let header = inner.header();
                let prefix_len = header.prefix_len as usize;
                let prefix = header.get_prefix().to_vec();

                // Check prefix
                for i in 0..prefix_len.min(MAX_PREFIX_LEN) {
                    if depth + i >= key.len() || prefix[i] != key[depth + i] {
                        return Ok((Some(inner), None));
                    }
                }

                let new_depth = depth + prefix_len;
                if new_depth >= key.len() {
                    // Key exhausted at inner node - remove value here if present
                    let value = inner.header_mut().value.take();
                    return Ok((Some(inner), value));
                }

                let next_byte = key[new_depth];

                // Remove from child
                let removed = match &mut inner {
                    ArtNode::Node4(n) => {
                        if let Some(idx) = n.find_child_index(next_byte) {
                            let child = n.children[idx].take()
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
                            let child = n.children[idx].take()
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
                            let child = n.children[idx as usize].take()
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

    /// Range scan from start (inclusive) to end (exclusive)
    pub fn range<'a>(&'a self, start: &'a [u8], end: &'a [u8]) -> impl Iterator<Item = (Vec<u8>, RowId)> + 'a {
        self.iter().filter(move |(k, _)| k.as_slice() >= start && k.as_slice() < end)
    }

    /// Prefix scan - find all keys with the given prefix
    pub fn prefix_scan<'a>(&'a self, prefix: &'a [u8]) -> impl Iterator<Item = (Vec<u8>, RowId)> + 'a {
        self.iter().filter(move |(k, _)| k.starts_with(prefix))
    }

    /// Clear all entries from the index
    pub fn clear(&mut self) {
        self.root = None;
        self.size = 0;
        self.stats = ArtIndexStats::default();
    }
}

/// Iterator over ART key-value pairs
pub struct ArtIterator<'a> {
    /// Stack of nodes to visit (node, key_prefix)
    stack: VecDeque<(&'a ArtNode, Vec<u8>)>,
    /// Pending value from inner node (if any)
    pending_inner_value: Option<(Vec<u8>, RowId)>,
}

impl<'a> ArtIterator<'a> {
    fn new(tree: &'a AdaptiveRadixTree) -> Self {
        let mut stack = VecDeque::new();
        if let Some(root) = &tree.root {
            stack.push_back((root, Vec::new()));
        }
        Self { stack, pending_inner_value: None }
    }
}

impl<'a> Iterator for ArtIterator<'a> {
    type Item = (Vec<u8>, RowId);

    fn next(&mut self) -> Option<Self::Item> {
        // Return pending inner value first
        if let Some(item) = self.pending_inner_value.take() {
            return Some(item);
        }

        while let Some((node, key_prefix)) = self.stack.pop_front() {
            match node {
                ArtNode::Leaf(leaf) => {
                    return Some((leaf.key.clone(), leaf.value));
                }
                ArtNode::Node4(n) => {
                    // Build key with node's prefix
                    let mut node_key = key_prefix.clone();
                    node_key.extend_from_slice(n.header.get_prefix());

                    // Check for value at this inner node
                    if let Some(value) = n.header.value {
                        self.pending_inner_value = Some((node_key.clone(), value));
                    }

                    // Collect children to push in reverse order
                    let children: Vec<_> = n.iter_children().collect();
                    for (byte, child) in children.into_iter().rev() {
                        let mut child_key = node_key.clone();
                        child_key.push(byte);
                        self.stack.push_front((child, child_key));
                    }

                    if self.pending_inner_value.is_some() {
                        return self.pending_inner_value.take();
                    }
                }
                ArtNode::Node16(n) => {
                    let mut node_key = key_prefix.clone();
                    node_key.extend_from_slice(n.header.get_prefix());

                    if let Some(value) = n.header.value {
                        self.pending_inner_value = Some((node_key.clone(), value));
                    }

                    let children: Vec<_> = n.iter_children().collect();
                    for (byte, child) in children.into_iter().rev() {
                        let mut child_key = node_key.clone();
                        child_key.push(byte);
                        self.stack.push_front((child, child_key));
                    }

                    if self.pending_inner_value.is_some() {
                        return self.pending_inner_value.take();
                    }
                }
                ArtNode::Node48(n) => {
                    let mut node_key = key_prefix.clone();
                    node_key.extend_from_slice(n.header.get_prefix());

                    if let Some(value) = n.header.value {
                        self.pending_inner_value = Some((node_key.clone(), value));
                    }

                    let children: Vec<_> = n.iter_children().collect();
                    for (byte, child) in children.into_iter().rev() {
                        let mut child_key = node_key.clone();
                        child_key.push(byte);
                        self.stack.push_front((child, child_key));
                    }

                    if self.pending_inner_value.is_some() {
                        return self.pending_inner_value.take();
                    }
                }
                ArtNode::Node256(n) => {
                    let mut node_key = key_prefix.clone();
                    node_key.extend_from_slice(n.header.get_prefix());

                    if let Some(value) = n.header.value {
                        self.pending_inner_value = Some((node_key.clone(), value));
                    }

                    let children: Vec<_> = n.iter_children().collect();
                    for (byte, child) in children.into_iter().rev() {
                        let mut child_key = node_key.clone();
                        child_key.push(byte);
                        self.stack.push_front((child, child_key));
                    }

                    if self.pending_inner_value.is_some() {
                        return self.pending_inner_value.take();
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
}
