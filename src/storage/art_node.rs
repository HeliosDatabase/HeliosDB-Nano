//! ART (Adaptive Radix Tree) Node Types
//!
//! This module implements the four node types used in ART:
//! - Node4: For 1-4 children (most compact)
//! - Node16: For 5-16 children (SIMD-friendly search)
//! - Node48: For 17-48 children (key index array)
//! - Node256: For 49-256 children (direct lookup, O(1))
//!
//! Nodes automatically grow and shrink as keys are inserted/removed.


/// Maximum key length for prefix compression
pub const MAX_PREFIX_LEN: usize = 10;

/// Node header containing common fields
#[derive(Debug, Clone)]
pub struct NodeHeader {
    /// Number of children currently stored
    pub num_children: u16,
    /// Prefix length (for path compression)
    pub prefix_len: u32,
    /// Compressed prefix bytes
    pub prefix: [u8; MAX_PREFIX_LEN],
    /// Values stored at this inner node (for keys that end here)
    /// Multiple values supported for non-unique indexes
    pub values: Vec<RowId>,
}

impl Default for NodeHeader {
    fn default() -> Self {
        Self {
            num_children: 0,
            prefix_len: 0,
            prefix: [0u8; MAX_PREFIX_LEN],
            values: Vec::new(),
        }
    }
}

#[allow(clippy::indexing_slicing)] // SAFETY: prefix bounded by MAX_PREFIX_LEN (10)
impl NodeHeader {
    /// Create a new header with the given prefix
    pub fn with_prefix(prefix: &[u8]) -> Self {
        let mut header = Self::default();
        header.set_prefix(prefix);
        header
    }

    /// Set the prefix
    pub fn set_prefix(&mut self, prefix: &[u8]) {
        let len = prefix.len().min(MAX_PREFIX_LEN);
        self.prefix_len = prefix.len() as u32;
        self.prefix[..len].copy_from_slice(&prefix[..len]);
    }

    /// Get the prefix bytes (up to MAX_PREFIX_LEN)
    pub fn get_prefix(&self) -> &[u8] {
        let len = (self.prefix_len as usize).min(MAX_PREFIX_LEN);
        &self.prefix[..len]
    }
}

/// Row ID type for leaf nodes
pub type RowId = u64;

/// ART Node enum - the core node type
#[derive(Debug, Clone)]
pub enum ArtNode {
    /// Node with 1-4 children (most compact)
    Node4(Box<Node4>),
    /// Node with 5-16 children (SIMD-friendly)
    Node16(Box<Node16>),
    /// Node with 17-48 children (key index array)
    Node48(Box<Node48>),
    /// Node with 49-256 children (direct lookup)
    Node256(Box<Node256>),
    /// Leaf node containing a value
    Leaf(LeafNode),
}

/// Leaf node containing the actual value(s)
///
/// Optimized for the common single-value case: primary value is stored inline
/// (no heap allocation). Additional values for non-unique indexes spill to `extra`.
#[derive(Debug, Clone)]
pub struct LeafNode {
    /// The full key for this leaf
    pub key: Vec<u8>,
    /// Primary row ID value (always present, stored inline — no heap allocation)
    primary: RowId,
    /// Additional row IDs for non-unique indexes (empty Vec = no heap allocation)
    extra: Vec<RowId>,
}

impl LeafNode {
    /// Create a new leaf node with a single value
    pub fn new(key: Vec<u8>, value: RowId) -> Self {
        Self { key, primary: value, extra: Vec::new() }
    }

    /// Create a leaf node from multiple values (e.g., during node splitting)
    pub fn from_values(key: Vec<u8>, primary: RowId, extra: Vec<RowId>) -> Self {
        Self { key, primary, extra }
    }

    /// Get the first (primary) value
    pub fn value(&self) -> RowId {
        self.primary
    }

    /// Get the total number of values
    pub fn values_count(&self) -> usize {
        1 + self.extra.len()
    }

    /// Add an additional value (for non-unique indexes)
    pub fn push_value(&mut self, value: RowId) {
        self.extra.push(value);
    }

    /// Iterate over all values
    pub fn values_iter(&self) -> impl Iterator<Item = RowId> + '_ {
        std::iter::once(self.primary).chain(self.extra.iter().copied())
    }

    /// Collect all values into a Vec
    pub fn all_values(&self) -> Vec<RowId> {
        let mut v = Vec::with_capacity(1 + self.extra.len());
        v.push(self.primary);
        v.extend_from_slice(&self.extra);
        v
    }

    /// Take all values out, leaving the leaf empty (primary=0, extra cleared)
    /// Returns (primary, extra_values)
    pub fn take_values(&mut self) -> (RowId, Vec<RowId>) {
        let primary = self.primary;
        self.primary = 0;
        (primary, std::mem::take(&mut self.extra))
    }

    /// Remove a specific row_id. Returns true if found and removed.
    /// If the primary value is removed, it's replaced by one from `extra`.
    /// Returns (removed, now_empty) — caller should delete leaf if now_empty.
    pub fn remove_value(&mut self, row_id: RowId) -> (bool, bool) {
        if self.primary == row_id {
            if let Some(replacement) = self.extra.pop() {
                self.primary = replacement;
                return (true, false);
            }
            return (true, true); // Last value removed
        }
        if let Some(pos) = self.extra.iter().position(|&v| v == row_id) {
            self.extra.swap_remove(pos);
            return (true, false);
        }
        (false, false)
    }

    /// Check if the key matches
    pub fn matches(&self, key: &[u8]) -> bool {
        self.key == key
    }
}

/// Node4: Compact node for 1-4 children
#[derive(Debug, Clone)]
pub struct Node4 {
    pub header: NodeHeader,
    /// Key bytes for each child
    pub keys: [u8; 4],
    /// Child pointers (None for empty slots)
    pub children: [Option<ArtNode>; 4],
}

impl Default for Node4 {
    fn default() -> Self {
        Self {
            header: NodeHeader::default(),
            keys: [0u8; 4],
            children: [None, None, None, None],
        }
    }
}

#[allow(clippy::indexing_slicing)] // SAFETY: indices bounded by num_children (max 4)
impl Node4 {
    /// Create a new empty Node4
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a Node4 with a prefix
    pub fn with_prefix(prefix: &[u8]) -> Self {
        Self {
            header: NodeHeader::with_prefix(prefix),
            ..Self::default()
        }
    }

    /// Check if the node is full
    pub fn is_full(&self) -> bool {
        self.header.num_children >= 4
    }

    /// Find the index for a key byte
    pub fn find_child_index(&self, key: u8) -> Option<usize> {
        let n = self.header.num_children as usize;
        for i in 0..n {
            if self.keys[i] == key {
                return Some(i);
            }
        }
        None
    }

    /// Get a child by key byte
    pub fn get_child(&self, key: u8) -> Option<&ArtNode> {
        self.find_child_index(key)
            .and_then(|i| self.children[i].as_ref())
    }

    /// Get a mutable child by key byte
    pub fn get_child_mut(&mut self, key: u8) -> Option<&mut ArtNode> {
        self.find_child_index(key)
            .and_then(|i| self.children[i].as_mut())
    }

    /// Add a child (returns false if full)
    pub fn add_child(&mut self, key: u8, child: ArtNode) -> bool {
        if self.is_full() {
            return false;
        }
        let idx = self.header.num_children as usize;
        self.keys[idx] = key;
        self.children[idx] = Some(child);
        self.header.num_children += 1;
        true
    }

    /// Remove a child
    pub fn remove_child(&mut self, key: u8) -> Option<ArtNode> {
        if let Some(idx) = self.find_child_index(key) {
            let child = self.children[idx].take();
            // Compact: move last child to this position
            let last_idx = (self.header.num_children - 1) as usize;
            if idx != last_idx {
                self.keys[idx] = self.keys[last_idx];
                self.children[idx] = self.children[last_idx].take();
            }
            self.header.num_children -= 1;
            child
        } else {
            None
        }
    }

    /// Grow to Node16
    pub fn grow(self) -> Node16 {
        let mut node16 = Node16::with_prefix(self.header.get_prefix());
        node16.header.prefix_len = self.header.prefix_len;

        for i in 0..4 {
            if let Some(child) = self.children[i].clone() {
                node16.keys[i] = self.keys[i];
                node16.children[i] = Some(child);
            }
        }
        node16.header.num_children = self.header.num_children;
        node16
    }

    /// Iterate over all children
    pub fn iter_children(&self) -> impl Iterator<Item = (u8, &ArtNode)> {
        let n = self.header.num_children as usize;
        (0..n).filter_map(move |i| {
            self.children[i].as_ref().map(|c| (self.keys[i], c))
        })
    }
}

/// Node16: Node for 5-16 children with SIMD-friendly search
#[derive(Debug, Clone)]
pub struct Node16 {
    pub header: NodeHeader,
    /// Key bytes for each child (sorted for binary search)
    pub keys: [u8; 16],
    /// Child pointers
    pub children: [Option<ArtNode>; 16],
}

impl Default for Node16 {
    fn default() -> Self {
        Self {
            header: NodeHeader::default(),
            keys: [0u8; 16],
            children: std::array::from_fn(|_| None),
        }
    }
}

#[allow(clippy::indexing_slicing)] // SAFETY: indices bounded by num_children (max 16)
impl Node16 {
    /// Create a new empty Node16
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a Node16 with a prefix
    pub fn with_prefix(prefix: &[u8]) -> Self {
        Self {
            header: NodeHeader::with_prefix(prefix),
            ..Self::default()
        }
    }

    /// Check if the node is full
    pub fn is_full(&self) -> bool {
        self.header.num_children >= 16
    }

    /// Check if the node should shrink
    pub fn should_shrink(&self) -> bool {
        self.header.num_children <= 4
    }

    /// Find the index for a key byte (linear search, SIMD-friendly)
    pub fn find_child_index(&self, key: u8) -> Option<usize> {
        let n = self.header.num_children as usize;
        // Simple linear search - in practice this is fast due to cache locality
        // and can be vectorized by the compiler
        for i in 0..n {
            if self.keys[i] == key {
                return Some(i);
            }
        }
        None
    }

    /// Get a child by key byte
    pub fn get_child(&self, key: u8) -> Option<&ArtNode> {
        self.find_child_index(key)
            .and_then(|i| self.children[i].as_ref())
    }

    /// Get a mutable child by key byte
    pub fn get_child_mut(&mut self, key: u8) -> Option<&mut ArtNode> {
        self.find_child_index(key)
            .and_then(|i| self.children[i].as_mut())
    }

    /// Add a child (returns false if full)
    pub fn add_child(&mut self, key: u8, child: ArtNode) -> bool {
        if self.is_full() {
            return false;
        }
        let idx = self.header.num_children as usize;
        self.keys[idx] = key;
        self.children[idx] = Some(child);
        self.header.num_children += 1;
        true
    }

    /// Remove a child
    pub fn remove_child(&mut self, key: u8) -> Option<ArtNode> {
        if let Some(idx) = self.find_child_index(key) {
            let child = self.children[idx].take();
            let last_idx = (self.header.num_children - 1) as usize;
            if idx != last_idx {
                self.keys[idx] = self.keys[last_idx];
                self.children[idx] = self.children[last_idx].take();
            }
            self.header.num_children -= 1;
            child
        } else {
            None
        }
    }

    /// Grow to Node48
    pub fn grow(self) -> Node48 {
        let mut node48 = Node48::with_prefix(self.header.get_prefix());
        node48.header.prefix_len = self.header.prefix_len;

        for i in 0..16 {
            if let Some(child) = self.children[i].clone() {
                let key = self.keys[i];
                node48.child_index[key as usize] = i as u8;
                node48.children[i] = Some(child);
            }
        }
        node48.header.num_children = self.header.num_children;
        node48
    }

    /// Shrink to Node4
    pub fn shrink(self) -> Node4 {
        let mut node4 = Node4::with_prefix(self.header.get_prefix());
        node4.header.prefix_len = self.header.prefix_len;

        let mut idx = 0;
        for i in 0..16 {
            if let Some(child) = self.children[i].clone() {
                node4.keys[idx] = self.keys[i];
                node4.children[idx] = Some(child);
                idx += 1;
                if idx >= 4 {
                    break;
                }
            }
        }
        node4.header.num_children = idx as u16;
        node4
    }

    /// Iterate over all children
    pub fn iter_children(&self) -> impl Iterator<Item = (u8, &ArtNode)> {
        let n = self.header.num_children as usize;
        (0..n).filter_map(move |i| {
            self.children[i].as_ref().map(|c| (self.keys[i], c))
        })
    }
}

/// Node48: Node for 17-48 children with key index array
#[derive(Debug, Clone)]
pub struct Node48 {
    pub header: NodeHeader,
    /// Index array: maps key byte to child slot (255 = empty)
    pub child_index: [u8; 256],
    /// Child pointers (up to 48)
    pub children: [Option<ArtNode>; 48],
}

impl Default for Node48 {
    fn default() -> Self {
        Self {
            header: NodeHeader::default(),
            child_index: [255u8; 256],
            children: std::array::from_fn(|_| None),
        }
    }
}

#[allow(clippy::indexing_slicing)] // SAFETY: key is u8 (0-255), child_index is [u8; 256], children bounded by num_children (max 48)
impl Node48 {
    /// Create a new empty Node48
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a Node48 with a prefix
    pub fn with_prefix(prefix: &[u8]) -> Self {
        Self {
            header: NodeHeader::with_prefix(prefix),
            ..Self::default()
        }
    }

    /// Check if the node is full
    pub fn is_full(&self) -> bool {
        self.header.num_children >= 48
    }

    /// Check if the node should shrink
    pub fn should_shrink(&self) -> bool {
        self.header.num_children <= 16
    }

    /// Get a child by key byte
    pub fn get_child(&self, key: u8) -> Option<&ArtNode> {
        let idx = self.child_index[key as usize];
        if idx != 255 {
            self.children[idx as usize].as_ref()
        } else {
            None
        }
    }

    /// Get a mutable child by key byte
    pub fn get_child_mut(&mut self, key: u8) -> Option<&mut ArtNode> {
        let idx = self.child_index[key as usize];
        if idx != 255 {
            self.children[idx as usize].as_mut()
        } else {
            None
        }
    }

    /// Find the next free slot
    fn find_free_slot(&self) -> Option<usize> {
        for i in 0..48 {
            if self.children[i].is_none() {
                return Some(i);
            }
        }
        None
    }

    /// Add a child (returns false if full)
    pub fn add_child(&mut self, key: u8, child: ArtNode) -> bool {
        if self.is_full() {
            return false;
        }
        if let Some(slot) = self.find_free_slot() {
            self.child_index[key as usize] = slot as u8;
            self.children[slot] = Some(child);
            self.header.num_children += 1;
            true
        } else {
            false
        }
    }

    /// Remove a child
    pub fn remove_child(&mut self, key: u8) -> Option<ArtNode> {
        let idx = self.child_index[key as usize];
        if idx != 255 {
            self.child_index[key as usize] = 255;
            self.header.num_children -= 1;
            self.children[idx as usize].take()
        } else {
            None
        }
    }

    /// Grow to Node256
    pub fn grow(self) -> Node256 {
        let mut node256 = Node256::with_prefix(self.header.get_prefix());
        node256.header.prefix_len = self.header.prefix_len;

        for key in 0..256u16 {
            let idx = self.child_index[key as usize];
            if idx != 255 {
                if let Some(child) = &self.children[idx as usize] {
                    node256.children[key as usize] = Some(child.clone());
                }
            }
        }
        node256.header.num_children = self.header.num_children;
        node256
    }

    /// Shrink to Node16
    pub fn shrink(self) -> Node16 {
        let mut node16 = Node16::with_prefix(self.header.get_prefix());
        node16.header.prefix_len = self.header.prefix_len;

        let mut idx = 0;
        for key in 0..256u16 {
            let slot = self.child_index[key as usize];
            if slot != 255 {
                if let Some(child) = &self.children[slot as usize] {
                    node16.keys[idx] = key as u8;
                    node16.children[idx] = Some(child.clone());
                    idx += 1;
                    if idx >= 16 {
                        break;
                    }
                }
            }
        }
        node16.header.num_children = idx as u16;
        node16
    }

    /// Iterate over all children
    pub fn iter_children(&self) -> impl Iterator<Item = (u8, &ArtNode)> + '_ {
        (0..256u16).filter_map(move |key| {
            let idx = self.child_index[key as usize];
            if idx != 255 {
                self.children[idx as usize].as_ref().map(|c| (key as u8, c))
            } else {
                None
            }
        })
    }
}

/// Node256: Node for 49-256 children with direct lookup
#[derive(Debug, Clone)]
pub struct Node256 {
    pub header: NodeHeader,
    /// Direct child array indexed by key byte
    pub children: [Option<ArtNode>; 256],
}

impl Default for Node256 {
    fn default() -> Self {
        // Use unsafe to avoid stack overflow with large array
        Self {
            header: NodeHeader::default(),
            children: std::array::from_fn(|_| None),
        }
    }
}

#[allow(clippy::indexing_slicing)] // SAFETY: key is u8 (0-255), children is [Option<ArtNode>; 256]
impl Node256 {
    /// Create a new empty Node256
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a Node256 with a prefix
    pub fn with_prefix(prefix: &[u8]) -> Self {
        Self {
            header: NodeHeader::with_prefix(prefix),
            ..Self::default()
        }
    }

    /// Check if the node should shrink
    pub fn should_shrink(&self) -> bool {
        self.header.num_children <= 48
    }

    /// Get a child by key byte (O(1))
    pub fn get_child(&self, key: u8) -> Option<&ArtNode> {
        self.children[key as usize].as_ref()
    }

    /// Get a mutable child by key byte (O(1))
    pub fn get_child_mut(&mut self, key: u8) -> Option<&mut ArtNode> {
        self.children[key as usize].as_mut()
    }

    /// Add a child (always succeeds for Node256)
    pub fn add_child(&mut self, key: u8, child: ArtNode) -> bool {
        if self.children[key as usize].is_none() {
            self.header.num_children += 1;
        }
        self.children[key as usize] = Some(child);
        true
    }

    /// Remove a child
    pub fn remove_child(&mut self, key: u8) -> Option<ArtNode> {
        let child = self.children[key as usize].take();
        if child.is_some() {
            self.header.num_children -= 1;
        }
        child
    }

    /// Shrink to Node48
    pub fn shrink(self) -> Node48 {
        let mut node48 = Node48::with_prefix(self.header.get_prefix());
        node48.header.prefix_len = self.header.prefix_len;

        let mut slot = 0;
        for key in 0..256u16 {
            if let Some(child) = &self.children[key as usize] {
                node48.child_index[key as usize] = slot as u8;
                node48.children[slot] = Some(child.clone());
                slot += 1;
                if slot >= 48 {
                    break;
                }
            }
        }
        node48.header.num_children = slot as u16;
        node48
    }

    /// Iterate over all children
    pub fn iter_children(&self) -> impl Iterator<Item = (u8, &ArtNode)> + '_ {
        (0..256u16).filter_map(move |key| {
            self.children[key as usize].as_ref().map(|c| (key as u8, c))
        })
    }
}

// Implement common operations on ArtNode enum
impl ArtNode {
    /// Get the header for any non-leaf node type.
    ///
    /// # Panics
    /// Panics if called on a Leaf node. Use `try_header()` for a safe alternative.
    pub fn header(&self) -> &NodeHeader {
        match self {
            ArtNode::Node4(n) => &n.header,
            ArtNode::Node16(n) => &n.header,
            ArtNode::Node48(n) => &n.header,
            ArtNode::Node256(n) => &n.header,
            ArtNode::Leaf(_) => unreachable!("Leaf nodes don't have headers - use try_header() for safe access"),
        }
    }

    /// Get mutable header for any non-leaf node type.
    ///
    /// # Panics
    /// Panics if called on a Leaf node. Use `try_header_mut()` for a safe alternative.
    pub fn header_mut(&mut self) -> &mut NodeHeader {
        match self {
            ArtNode::Node4(n) => &mut n.header,
            ArtNode::Node16(n) => &mut n.header,
            ArtNode::Node48(n) => &mut n.header,
            ArtNode::Node256(n) => &mut n.header,
            ArtNode::Leaf(_) => unreachable!("Leaf nodes don't have headers - use try_header_mut() for safe access"),
        }
    }

    /// Safely get the header for any node type (returns None for Leaf nodes)
    pub fn try_header(&self) -> Option<&NodeHeader> {
        match self {
            ArtNode::Node4(n) => Some(&n.header),
            ArtNode::Node16(n) => Some(&n.header),
            ArtNode::Node48(n) => Some(&n.header),
            ArtNode::Node256(n) => Some(&n.header),
            ArtNode::Leaf(_) => None,
        }
    }

    /// Safely get mutable header for any node type (returns None for Leaf nodes)
    pub fn try_header_mut(&mut self) -> Option<&mut NodeHeader> {
        match self {
            ArtNode::Node4(n) => Some(&mut n.header),
            ArtNode::Node16(n) => Some(&mut n.header),
            ArtNode::Node48(n) => Some(&mut n.header),
            ArtNode::Node256(n) => Some(&mut n.header),
            ArtNode::Leaf(_) => None,
        }
    }

    /// Check if this is a leaf node
    pub fn is_leaf(&self) -> bool {
        matches!(self, ArtNode::Leaf(_))
    }

    /// Get the leaf value if this is a leaf
    pub fn as_leaf(&self) -> Option<&LeafNode> {
        match self {
            ArtNode::Leaf(leaf) => Some(leaf),
            _ => None,
        }
    }

    /// Get the leaf value mutably if this is a leaf
    pub fn as_leaf_mut(&mut self) -> Option<&mut LeafNode> {
        match self {
            ArtNode::Leaf(leaf) => Some(leaf),
            _ => None,
        }
    }

    /// Get a child by key byte
    pub fn get_child(&self, key: u8) -> Option<&ArtNode> {
        match self {
            ArtNode::Node4(n) => n.get_child(key),
            ArtNode::Node16(n) => n.get_child(key),
            ArtNode::Node48(n) => n.get_child(key),
            ArtNode::Node256(n) => n.get_child(key),
            ArtNode::Leaf(_) => None,
        }
    }

    /// Get a mutable child by key byte
    pub fn get_child_mut(&mut self, key: u8) -> Option<&mut ArtNode> {
        match self {
            ArtNode::Node4(n) => n.get_child_mut(key),
            ArtNode::Node16(n) => n.get_child_mut(key),
            ArtNode::Node48(n) => n.get_child_mut(key),
            ArtNode::Node256(n) => n.get_child_mut(key),
            ArtNode::Leaf(_) => None,
        }
    }

    /// Number of children
    pub fn num_children(&self) -> u16 {
        match self {
            ArtNode::Node4(n) => n.header.num_children,
            ArtNode::Node16(n) => n.header.num_children,
            ArtNode::Node48(n) => n.header.num_children,
            ArtNode::Node256(n) => n.header.num_children,
            ArtNode::Leaf(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node4_basic() {
        let mut node = Node4::new();

        // Add children
        assert!(node.add_child(b'a', ArtNode::Leaf(LeafNode::new(vec![b'a'], 1))));
        assert!(node.add_child(b'b', ArtNode::Leaf(LeafNode::new(vec![b'b'], 2))));
        assert!(node.add_child(b'c', ArtNode::Leaf(LeafNode::new(vec![b'c'], 3))));
        assert!(node.add_child(b'd', ArtNode::Leaf(LeafNode::new(vec![b'd'], 4))));

        // Should be full now
        assert!(node.is_full());
        assert!(!node.add_child(b'e', ArtNode::Leaf(LeafNode::new(vec![b'e'], 5))));

        // Find children
        assert!(node.get_child(b'a').is_some());
        assert!(node.get_child(b'b').is_some());
        assert!(node.get_child(b'e').is_none());

        // Remove child
        let removed = node.remove_child(b'b');
        assert!(removed.is_some());
        assert!(node.get_child(b'b').is_none());
        assert_eq!(node.header.num_children, 3);
    }

    #[test]
    fn test_node4_to_node16_growth() {
        let mut node = Node4::new();
        node.add_child(b'a', ArtNode::Leaf(LeafNode::new(vec![b'a'], 1)));
        node.add_child(b'b', ArtNode::Leaf(LeafNode::new(vec![b'b'], 2)));
        node.add_child(b'c', ArtNode::Leaf(LeafNode::new(vec![b'c'], 3)));
        node.add_child(b'd', ArtNode::Leaf(LeafNode::new(vec![b'd'], 4)));

        let node16 = node.grow();
        assert_eq!(node16.header.num_children, 4);
        assert!(node16.get_child(b'a').is_some());
        assert!(node16.get_child(b'd').is_some());
    }

    #[test]
    fn test_node256_direct_lookup() {
        let mut node = Node256::new();

        // Add many children
        for i in 0..100 {
            node.add_child(i, ArtNode::Leaf(LeafNode::new(vec![i], i as u64)));
        }

        assert_eq!(node.header.num_children, 100);

        // Direct lookup
        for i in 0..100 {
            assert!(node.get_child(i).is_some());
        }
        assert!(node.get_child(200).is_none());
    }

    #[test]
    fn test_prefix_compression() {
        let mut node = Node4::with_prefix(b"prefix");
        assert_eq!(node.header.prefix_len, 6);
        assert_eq!(node.header.get_prefix(), b"prefix");

        // Long prefix (truncated in storage but length preserved)
        let long_prefix = b"this_is_a_very_long_prefix_that_exceeds_max";
        let mut node2 = Node4::with_prefix(long_prefix);
        assert_eq!(node2.header.prefix_len, long_prefix.len() as u32);
        assert_eq!(node2.header.get_prefix(), &long_prefix[..MAX_PREFIX_LEN]);
    }
}
