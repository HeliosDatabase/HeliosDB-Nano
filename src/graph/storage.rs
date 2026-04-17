//! Adjacency-list graph store.
//!
//! RAG-native (idea 1).

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

/// Stable opaque node identifier (UUID under the hood).
pub type NodeId = Uuid;

/// Stable opaque edge identifier (UUID under the hood).
pub type EdgeId = Uuid;

/// Edge traversal direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Follow outgoing edges (`from -> to`).
    Outgoing,
    /// Follow incoming edges (`to <- from`).
    Incoming,
    /// Treat the graph as undirected -- follow both adjacency lists.
    Both,
}

/// A directed, labelled, weighted edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    pub from: NodeId,
    pub to: NodeId,
    pub label: String,
    /// Edge weight. Defaults to `1.0`. Used by Dijkstra / A*.
    pub weight: f64,
    /// Free-form JSON-shaped properties.
    #[serde(default)]
    pub properties: serde_json::Value,
}

impl Edge {
    /// Construct a new edge with auto-generated `id` and `weight = 1.0`.
    #[must_use]
    pub fn new(from: NodeId, to: NodeId, label: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            from,
            to,
            label: label.into(),
            weight: 1.0,
            properties: serde_json::Value::Null,
        }
    }

    /// Builder: set edge weight.
    #[must_use]
    pub fn with_weight(mut self, w: f64) -> Self {
        self.weight = w;
        self
    }

    /// Builder: set free-form properties.
    #[must_use]
    pub fn with_properties(mut self, props: serde_json::Value) -> Self {
        self.properties = props;
        self
    }
}

/// In-memory graph store.
///
/// Uses two `DashMap`s for lock-free concurrent reads of adjacency
/// lists. Writes take per-node entry locks.
#[derive(Debug, Default)]
pub struct GraphStore {
    /// `node_id -> outgoing edges`
    outgoing: DashMap<NodeId, Vec<Arc<Edge>>>,
    /// `node_id -> incoming edges`
    incoming: DashMap<NodeId, Vec<Arc<Edge>>>,
    /// `edge_id -> edge` for O(1) lookup / deletion
    by_id: DashMap<EdgeId, Arc<Edge>>,
}

impl GraphStore {
    /// Construct an empty graph store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert an edge. Returns the assigned `EdgeId`.
    pub fn add_edge(&self, edge: Edge) -> EdgeId {
        let id = edge.id;
        let arc = Arc::new(edge);
        self.outgoing.entry(arc.from).or_default().push(arc.clone());
        self.incoming.entry(arc.to).or_default().push(arc.clone());
        self.by_id.insert(id, arc);
        id
    }

    /// Remove an edge by id. Returns `true` if it existed.
    pub fn remove_edge(&self, id: EdgeId) -> bool {
        let Some((_, edge)) = self.by_id.remove(&id) else {
            return false;
        };
        if let Some(mut out) = self.outgoing.get_mut(&edge.from) {
            out.retain(|e| e.id != id);
        }
        if let Some(mut inc) = self.incoming.get_mut(&edge.to) {
            inc.retain(|e| e.id != id);
        }
        true
    }

    /// Look up an edge by id.
    #[must_use]
    pub fn get_edge(&self, id: EdgeId) -> Option<Arc<Edge>> {
        self.by_id.get(&id).map(|e| e.value().clone())
    }

    /// Total number of edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.by_id.len()
    }

    /// Number of distinct nodes that participate in any edge.
    #[must_use]
    pub fn node_count(&self) -> usize {
        // Union of outgoing.keys() and incoming.keys() -- we approximate
        // by collecting then deduping; cheap because graphs are typically
        // small enough to enumerate for stats purposes.
        let mut nodes: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        for kv in self.outgoing.iter() {
            nodes.insert(*kv.key());
        }
        for kv in self.incoming.iter() {
            nodes.insert(*kv.key());
        }
        nodes.len()
    }

    /// Return outgoing edges from `node`, optionally filtered by label.
    #[must_use]
    pub fn outgoing(&self, node: NodeId, label: Option<&str>) -> Vec<Arc<Edge>> {
        self.outgoing
            .get(&node)
            .map(|v| filter_label(v.value(), label))
            .unwrap_or_default()
    }

    /// Return incoming edges to `node`, optionally filtered by label.
    #[must_use]
    pub fn incoming(&self, node: NodeId, label: Option<&str>) -> Vec<Arc<Edge>> {
        self.incoming
            .get(&node)
            .map(|v| filter_label(v.value(), label))
            .unwrap_or_default()
    }

    /// Return edges in the requested `direction` from `node`.
    ///
    /// For `Direction::Both` outgoing and incoming are concatenated,
    /// duplicate edge ids are stripped.
    #[must_use]
    pub fn neighbors(&self, node: NodeId, dir: Direction, label: Option<&str>) -> Vec<Arc<Edge>> {
        match dir {
            Direction::Outgoing => self.outgoing(node, label),
            Direction::Incoming => self.incoming(node, label),
            Direction::Both => {
                let mut out = self.outgoing(node, label);
                let mut seen: std::collections::HashSet<EdgeId> = out.iter().map(|e| e.id).collect();
                for e in self.incoming(node, label) {
                    if seen.insert(e.id) {
                        out.push(e);
                    }
                }
                out
            }
        }
    }

    /// Drop every edge in the store.
    pub fn clear(&self) {
        self.outgoing.clear();
        self.incoming.clear();
        self.by_id.clear();
    }
}

fn filter_label(edges: &[Arc<Edge>], label: Option<&str>) -> Vec<Arc<Edge>> {
    match label {
        Some(l) => edges.iter().filter(|e| e.label == l).cloned().collect(),
        None => edges.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n() -> NodeId {
        Uuid::new_v4()
    }

    #[test]
    fn add_and_lookup_edge() {
        let g = GraphStore::new();
        let a = n();
        let b = n();
        let id = g.add_edge(Edge::new(a, b, "follows"));
        assert_eq!(g.edge_count(), 1);
        let e = g.get_edge(id).expect("edge");
        assert_eq!(e.from, a);
        assert_eq!(e.to, b);
        assert_eq!(e.label, "follows");
    }

    #[test]
    fn outgoing_and_incoming_indexes_match() {
        let g = GraphStore::new();
        let a = n();
        let b = n();
        let c = n();
        g.add_edge(Edge::new(a, b, "x"));
        g.add_edge(Edge::new(a, c, "x"));
        g.add_edge(Edge::new(b, a, "x"));
        assert_eq!(g.outgoing(a, None).len(), 2);
        assert_eq!(g.incoming(a, None).len(), 1);
        assert_eq!(g.incoming(b, None).len(), 1);
    }

    #[test]
    fn label_filter_applies() {
        let g = GraphStore::new();
        let a = n();
        let b = n();
        g.add_edge(Edge::new(a, b, "follows"));
        g.add_edge(Edge::new(a, b, "blocks"));
        assert_eq!(g.outgoing(a, Some("follows")).len(), 1);
        assert_eq!(g.outgoing(a, Some("blocks")).len(), 1);
        assert_eq!(g.outgoing(a, Some("missing")).len(), 0);
        assert_eq!(g.outgoing(a, None).len(), 2);
    }

    #[test]
    fn remove_edge_clears_both_indexes() {
        let g = GraphStore::new();
        let a = n();
        let b = n();
        let id = g.add_edge(Edge::new(a, b, "x"));
        assert!(g.remove_edge(id));
        assert_eq!(g.edge_count(), 0);
        assert!(g.outgoing(a, None).is_empty());
        assert!(g.incoming(b, None).is_empty());
        assert!(!g.remove_edge(id), "second removal returns false");
    }

    #[test]
    fn neighbors_both_dedups() {
        let g = GraphStore::new();
        let a = n();
        let b = n();
        // Two distinct edges in opposite directions -- should yield 2.
        g.add_edge(Edge::new(a, b, "x"));
        g.add_edge(Edge::new(b, a, "x"));
        let both = g.neighbors(a, Direction::Both, None);
        assert_eq!(both.len(), 2);
    }

    #[test]
    fn weight_and_properties_roundtrip() {
        let g = GraphStore::new();
        let a = n();
        let b = n();
        let id = g.add_edge(
            Edge::new(a, b, "road")
                .with_weight(2.5)
                .with_properties(serde_json::json!({"surface": "gravel"})),
        );
        let e = g.get_edge(id).expect("edge");
        assert!((e.weight - 2.5).abs() < f64::EPSILON);
        assert_eq!(e.properties["surface"], "gravel");
    }
}
