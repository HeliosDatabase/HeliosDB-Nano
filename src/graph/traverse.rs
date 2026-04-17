//! Graph traversal algorithms.
//!
//! RAG-native (idea 1).
//!
//! Implements:
//! - BFS (k-hop neighbor enumeration)
//! - Dijkstra (single-source shortest path with non-negative weights)
//! - Bidirectional BFS (unweighted shortest hop count)
//!
//! All algorithms use bounded scratch buffers and respect a
//! [`TraversalLimits`] guard so a runaway traversal can't OOM the
//! process.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use super::storage::{Direction, Edge, GraphStore, NodeId};

/// Algorithm choice for [`shortest_path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortestPathAlgorithm {
    /// Breadth-first search (treats edges as unweighted -- minimises hop count).
    Bfs,
    /// Dijkstra (uses `Edge::weight` -- requires non-negative weights).
    Dijkstra,
    /// Bidirectional BFS (unweighted, faster for "find a path" queries).
    BidirectionalBfs,
}

/// Safety bounds for traversal algorithms.
///
/// Prevents pathological queries from running indefinitely.
#[derive(Debug, Clone, Copy)]
pub struct TraversalLimits {
    /// Maximum number of nodes to visit before aborting.
    pub max_nodes: usize,
    /// Maximum traversal depth (in hops).
    pub max_depth: usize,
}

impl Default for TraversalLimits {
    fn default() -> Self {
        Self {
            max_nodes: 10_000,
            max_depth: 32,
        }
    }
}

/// A discovered path through the graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    pub nodes: Vec<NodeId>,
    pub edges: Vec<super::storage::EdgeId>,
    pub total_weight: f64,
}

impl Path {
    /// Number of hops (edges) in the path. `0` for a path containing only the start node.
    #[must_use]
    pub fn hops(&self) -> usize {
        self.edges.len()
    }
}

// -- BFS k-hop neighbor enumeration ---------------------------------------

/// Breadth-first traversal starting at `start`.
///
/// Returns nodes in BFS visitation order, paired with the depth at which
/// they were discovered. The start node is returned at depth 0.
pub fn bfs(
    graph: &GraphStore,
    start: NodeId,
    direction: Direction,
    label: Option<&str>,
    limits: TraversalLimits,
) -> Vec<(NodeId, usize)> {
    let mut visited = HashSet::with_capacity(64);
    let mut queue = VecDeque::with_capacity(64);
    let mut out = Vec::with_capacity(64);

    queue.push_back((start, 0_usize));
    visited.insert(start);

    while let Some((node, depth)) = queue.pop_front() {
        out.push((node, depth));
        if out.len() >= limits.max_nodes || depth >= limits.max_depth {
            continue;
        }
        for edge in graph.neighbors(node, direction, label) {
            let next = other_end(&edge, node, direction);
            if visited.insert(next) {
                queue.push_back((next, depth + 1));
            }
        }
    }
    out
}

/// Public entry point for shortest-path queries.
pub fn shortest_path(
    graph: &GraphStore,
    from: NodeId,
    to: NodeId,
    algo: ShortestPathAlgorithm,
    direction: Direction,
    label: Option<&str>,
    limits: TraversalLimits,
) -> Option<Path> {
    if from == to {
        return Some(Path {
            nodes: vec![from],
            edges: Vec::new(),
            total_weight: 0.0,
        });
    }
    match algo {
        ShortestPathAlgorithm::Bfs => bfs_shortest_path(graph, from, to, direction, label, limits),
        ShortestPathAlgorithm::BidirectionalBfs => bidi_bfs_shortest_path(graph, from, to, direction, label, limits),
        ShortestPathAlgorithm::Dijkstra => dijkstra_shortest_path(graph, from, to, direction, label, limits),
    }
}

// -- BFS shortest path ----------------------------------------------------

fn bfs_shortest_path(
    graph: &GraphStore,
    from: NodeId,
    to: NodeId,
    direction: Direction,
    label: Option<&str>,
    limits: TraversalLimits,
) -> Option<Path> {
    let mut parent: HashMap<NodeId, (NodeId, std::sync::Arc<Edge>)> = HashMap::new();
    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();

    queue.push_back((from, 0_usize));
    visited.insert(from);

    while let Some((node, depth)) = queue.pop_front() {
        if node == to {
            return Some(reconstruct_path(from, to, &parent));
        }
        if depth >= limits.max_depth || visited.len() >= limits.max_nodes {
            continue;
        }
        for edge in graph.neighbors(node, direction, label) {
            let next = other_end(&edge, node, direction);
            if visited.insert(next) {
                parent.insert(next, (node, edge));
                queue.push_back((next, depth + 1));
            }
        }
    }
    None
}

// -- Bidirectional BFS ----------------------------------------------------

fn bidi_bfs_shortest_path(
    graph: &GraphStore,
    from: NodeId,
    to: NodeId,
    direction: Direction,
    label: Option<&str>,
    limits: TraversalLimits,
) -> Option<Path> {
    // Bidirectional only makes sense when we can traverse both directions.
    // For asymmetric directions we fall back to plain BFS.
    if direction != Direction::Both {
        return bfs_shortest_path(graph, from, to, direction, label, limits);
    }

    let mut fwd_parent: HashMap<NodeId, (NodeId, std::sync::Arc<Edge>)> = HashMap::new();
    let mut bwd_parent: HashMap<NodeId, (NodeId, std::sync::Arc<Edge>)> = HashMap::new();
    let mut fwd_visited: HashSet<NodeId> = HashSet::from([from]);
    let mut bwd_visited: HashSet<NodeId> = HashSet::from([to]);
    let mut fwd_queue: VecDeque<(NodeId, usize)> = VecDeque::from([(from, 0)]);
    let mut bwd_queue: VecDeque<(NodeId, usize)> = VecDeque::from([(to, 0)]);

    while !fwd_queue.is_empty() && !bwd_queue.is_empty() {
        if let Some(meet) = step(
            graph,
            &mut fwd_queue,
            &mut fwd_visited,
            &mut fwd_parent,
            &bwd_visited,
            label,
            limits,
        ) {
            return Some(stitch(from, to, meet, &fwd_parent, &bwd_parent));
        }
        if let Some(meet) = step(
            graph,
            &mut bwd_queue,
            &mut bwd_visited,
            &mut bwd_parent,
            &fwd_visited,
            label,
            limits,
        ) {
            return Some(stitch(from, to, meet, &fwd_parent, &bwd_parent));
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn step(
    graph: &GraphStore,
    queue: &mut VecDeque<(NodeId, usize)>,
    visited: &mut HashSet<NodeId>,
    parent: &mut HashMap<NodeId, (NodeId, std::sync::Arc<Edge>)>,
    other_visited: &HashSet<NodeId>,
    label: Option<&str>,
    limits: TraversalLimits,
) -> Option<NodeId> {
    let Some((node, depth)) = queue.pop_front() else {
        return None;
    };
    if depth >= limits.max_depth || visited.len() >= limits.max_nodes {
        return None;
    }
    for edge in graph.neighbors(node, Direction::Both, label) {
        let next = other_end(&edge, node, Direction::Both);
        if visited.insert(next) {
            parent.insert(next, (node, edge));
            if other_visited.contains(&next) {
                return Some(next);
            }
            queue.push_back((next, depth + 1));
        }
    }
    None
}

fn stitch(
    from: NodeId,
    to: NodeId,
    meet: NodeId,
    fwd_parent: &HashMap<NodeId, (NodeId, std::sync::Arc<Edge>)>,
    bwd_parent: &HashMap<NodeId, (NodeId, std::sync::Arc<Edge>)>,
) -> Path {
    let fwd = reconstruct_path(from, meet, fwd_parent);
    let bwd = reconstruct_path(to, meet, bwd_parent);
    // Reverse the backward half (from `meet` back to `to`) and append.
    let mut nodes = fwd.nodes;
    let mut edges = fwd.edges;
    let mut weight = fwd.total_weight;
    let bwd_nodes_rev: Vec<_> = bwd.nodes.iter().rev().skip(1).copied().collect();
    let bwd_edges_rev: Vec<_> = bwd.edges.iter().rev().copied().collect();
    for n in bwd_nodes_rev {
        nodes.push(n);
    }
    for e in bwd_edges_rev {
        edges.push(e);
    }
    weight += bwd.total_weight;
    Path {
        nodes,
        edges,
        total_weight: weight,
    }
}

// -- Dijkstra -------------------------------------------------------------

#[derive(Copy, Clone, PartialEq)]
struct DijkNode {
    node: NodeId,
    dist: f64,
}

impl Eq for DijkNode {}
impl Ord for DijkNode {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; invert so smaller dist == higher priority.
        other.dist.partial_cmp(&self.dist).unwrap_or(Ordering::Equal)
    }
}
impl PartialOrd for DijkNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn dijkstra_shortest_path(
    graph: &GraphStore,
    from: NodeId,
    to: NodeId,
    direction: Direction,
    label: Option<&str>,
    limits: TraversalLimits,
) -> Option<Path> {
    let mut dist: HashMap<NodeId, f64> = HashMap::new();
    let mut parent: HashMap<NodeId, (NodeId, std::sync::Arc<Edge>)> = HashMap::new();
    let mut heap = BinaryHeap::new();

    dist.insert(from, 0.0);
    heap.push(DijkNode { node: from, dist: 0.0 });

    while let Some(DijkNode { node, dist: d }) = heap.pop() {
        if node == to {
            return Some(reconstruct_path(from, to, &parent));
        }
        if dist.len() >= limits.max_nodes {
            continue;
        }
        // Skip if we've found a shorter path already.
        if let Some(&best) = dist.get(&node) {
            if d > best {
                continue;
            }
        }
        for edge in graph.neighbors(node, direction, label) {
            if edge.weight < 0.0 {
                // Dijkstra invariant: no negative weights.
                continue;
            }
            let next = other_end(&edge, node, direction);
            let nd = d + edge.weight;
            if dist.get(&next).is_none_or(|&cur| nd < cur) {
                dist.insert(next, nd);
                parent.insert(next, (node, edge.clone()));
                heap.push(DijkNode { node: next, dist: nd });
            }
        }
    }
    None
}

// -- Helpers --------------------------------------------------------------

fn other_end(edge: &Edge, from: NodeId, direction: Direction) -> NodeId {
    match direction {
        Direction::Outgoing => edge.to,
        Direction::Incoming => edge.from,
        Direction::Both => {
            if edge.from == from {
                edge.to
            } else {
                edge.from
            }
        }
    }
}

fn reconstruct_path(from: NodeId, to: NodeId, parent: &HashMap<NodeId, (NodeId, std::sync::Arc<Edge>)>) -> Path {
    let mut nodes = vec![to];
    let mut edges = Vec::new();
    let mut weight = 0.0;
    let mut cursor = to;
    while cursor != from {
        let Some((prev, edge)) = parent.get(&cursor) else {
            break;
        };
        nodes.push(*prev);
        edges.push(edge.id);
        weight += edge.weight;
        cursor = *prev;
    }
    nodes.reverse();
    edges.reverse();
    Path {
        nodes,
        edges,
        total_weight: weight,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::storage::Edge;
    use uuid::Uuid;

    fn line_graph(g: &GraphStore, n: usize) -> Vec<NodeId> {
        let nodes: Vec<NodeId> = (0..n).map(|_| Uuid::new_v4()).collect();
        for w in nodes.windows(2) {
            g.add_edge(Edge::new(w[0], w[1], "next"));
        }
        nodes
    }

    #[test]
    fn bfs_visits_all_reachable() {
        let g = GraphStore::new();
        let nodes = line_graph(&g, 5);
        let visited = bfs(&g, nodes[0], Direction::Outgoing, None, TraversalLimits::default());
        assert_eq!(visited.len(), 5);
        assert_eq!(visited[0].1, 0);
        assert_eq!(visited[4].1, 4);
    }

    #[test]
    fn bfs_respects_depth_limit() {
        let g = GraphStore::new();
        let nodes = line_graph(&g, 10);
        let visited = bfs(
            &g,
            nodes[0],
            Direction::Outgoing,
            None,
            TraversalLimits {
                max_depth: 2,
                ..Default::default()
            },
        );
        // depth 0, 1, 2 -> 3 nodes
        assert_eq!(visited.len(), 3);
        assert_eq!(visited.last().unwrap().1, 2);
    }

    #[test]
    fn bfs_shortest_path_finds_min_hops() {
        let g = GraphStore::new();
        let nodes = line_graph(&g, 5);
        let p = shortest_path(
            &g,
            nodes[0],
            nodes[4],
            ShortestPathAlgorithm::Bfs,
            Direction::Outgoing,
            None,
            TraversalLimits::default(),
        )
        .expect("path exists");
        assert_eq!(p.hops(), 4);
        assert_eq!(p.nodes.first(), Some(&nodes[0]));
        assert_eq!(p.nodes.last(), Some(&nodes[4]));
    }

    #[test]
    fn dijkstra_picks_lighter_path() {
        let g = GraphStore::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let d = Uuid::new_v4();
        // a -> b -> d (weight 10 each, total 20)
        g.add_edge(Edge::new(a, b, "x").with_weight(10.0));
        g.add_edge(Edge::new(b, d, "x").with_weight(10.0));
        // a -> c -> d (weight 1 each, total 2)
        g.add_edge(Edge::new(a, c, "x").with_weight(1.0));
        g.add_edge(Edge::new(c, d, "x").with_weight(1.0));
        let p = shortest_path(
            &g,
            a,
            d,
            ShortestPathAlgorithm::Dijkstra,
            Direction::Outgoing,
            None,
            TraversalLimits::default(),
        )
        .expect("path exists");
        assert!((p.total_weight - 2.0).abs() < f64::EPSILON);
        assert_eq!(p.nodes, vec![a, c, d]);
    }

    #[test]
    fn dijkstra_self_loop_returns_zero() {
        let g = GraphStore::new();
        let a = Uuid::new_v4();
        let p = shortest_path(
            &g,
            a,
            a,
            ShortestPathAlgorithm::Dijkstra,
            Direction::Outgoing,
            None,
            TraversalLimits::default(),
        )
        .expect("self path");
        assert!((p.total_weight - 0.0).abs() < f64::EPSILON);
        assert_eq!(p.hops(), 0);
    }

    #[test]
    fn shortest_path_returns_none_when_disconnected() {
        let g = GraphStore::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // No edges at all.
        let p = shortest_path(
            &g,
            a,
            b,
            ShortestPathAlgorithm::Bfs,
            Direction::Outgoing,
            None,
            TraversalLimits::default(),
        );
        assert!(p.is_none());
    }

    #[test]
    fn label_filter_in_traversal() {
        let g = GraphStore::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        g.add_edge(Edge::new(a, b, "follows"));
        g.add_edge(Edge::new(b, c, "blocks"));
        // Following only "follows" we can't reach c.
        let p = shortest_path(
            &g,
            a,
            c,
            ShortestPathAlgorithm::Bfs,
            Direction::Outgoing,
            Some("follows"),
            TraversalLimits::default(),
        );
        assert!(p.is_none());
        // Following only "blocks" we can't reach b from a.
        let p2 = shortest_path(
            &g,
            a,
            b,
            ShortestPathAlgorithm::Bfs,
            Direction::Outgoing,
            Some("blocks"),
            TraversalLimits::default(),
        );
        assert!(p2.is_none());
    }

    #[test]
    fn bidirectional_bfs_matches_bfs_for_undirected() {
        let g = GraphStore::new();
        let nodes = line_graph(&g, 6);
        // Undirected traversal: bidirectional BFS should find 5 hops.
        let p = shortest_path(
            &g,
            nodes[0],
            nodes[5],
            ShortestPathAlgorithm::BidirectionalBfs,
            Direction::Both,
            None,
            TraversalLimits::default(),
        )
        .expect("path exists");
        assert_eq!(p.hops(), 5);
    }
}
