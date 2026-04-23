//! Basic graph store tests -- create / lookup / iterate.
//!
//! RAG-native (idea 1).

use heliosdb_nano::graph::{
    storage::{Direction, Edge, GraphStore},
    traverse::{self, TraversalLimits},
};
use uuid::Uuid;

#[test]
fn one_hop_traversal() {
    let g = GraphStore::new();
    let alice = Uuid::new_v4();
    let bob = Uuid::new_v4();
    g.add_edge(Edge::new(alice, bob, "follows"));

    let visited = traverse::bfs(
        &g,
        alice,
        Direction::Outgoing,
        Some("follows"),
        TraversalLimits::default(),
    );
    assert_eq!(visited.len(), 2);
    assert_eq!(visited[0], (alice, 0));
    assert_eq!(visited[1], (bob, 1));
}

#[test]
fn two_hop_traversal_with_label_filter() {
    let g = GraphStore::new();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();
    g.add_edge(Edge::new(a, b, "follows"));
    g.add_edge(Edge::new(b, c, "follows"));
    g.add_edge(Edge::new(c, d, "blocks"));

    let visited = traverse::bfs(
        &g,
        a,
        Direction::Outgoing,
        Some("follows"),
        TraversalLimits {
            max_depth: 2,
            ..Default::default()
        },
    );
    let nodes: Vec<_> = visited.iter().map(|(n, _)| *n).collect();
    assert!(nodes.contains(&a));
    assert!(nodes.contains(&b));
    assert!(nodes.contains(&c));
    assert!(!nodes.contains(&d), "blocks edge filtered out");
}

#[test]
fn neighbors_outgoing_vs_incoming() {
    let g = GraphStore::new();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    g.add_edge(Edge::new(a, b, "x"));
    g.add_edge(Edge::new(c, a, "x"));
    assert_eq!(g.outgoing(a, None).len(), 1);
    assert_eq!(g.incoming(a, None).len(), 1);
    assert_eq!(g.neighbors(a, Direction::Both, None).len(), 2);
}

#[test]
fn node_and_edge_counts_are_consistent() {
    let g = GraphStore::new();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    g.add_edge(Edge::new(a, b, "x"));
    g.add_edge(Edge::new(b, c, "x"));
    assert_eq!(g.edge_count(), 2);
    assert_eq!(g.node_count(), 3);
    g.clear();
    assert_eq!(g.edge_count(), 0);
    assert_eq!(g.node_count(), 0);
}
