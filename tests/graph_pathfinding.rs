//! Shortest-path correctness tests (BFS, Dijkstra, bidirectional BFS).
//!
//! HelixDB-inspired (idea 1).

use heliosdb_nano::graph::{
    storage::{Direction, Edge, GraphStore},
    traverse::{self, ShortestPathAlgorithm, TraversalLimits},
};
use uuid::Uuid;

fn limits() -> TraversalLimits {
    TraversalLimits::default()
}

#[test]
fn bfs_finds_minimum_hop_path() {
    let g = GraphStore::new();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();
    // a -> b -> c -> d  (3 hops)
    // a -> d            (1 hop, but heavier weight; BFS doesn't care)
    g.add_edge(Edge::new(a, b, "x"));
    g.add_edge(Edge::new(b, c, "x"));
    g.add_edge(Edge::new(c, d, "x"));
    g.add_edge(Edge::new(a, d, "x"));

    let p = traverse::shortest_path(
        &g,
        a,
        d,
        ShortestPathAlgorithm::Bfs,
        Direction::Outgoing,
        None,
        limits(),
    )
    .expect("path");
    assert_eq!(p.hops(), 1);
}

#[test]
fn dijkstra_minimises_total_weight() {
    let g = GraphStore::new();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();
    // Cheap detour: a -> b -> c -> d  (1+1+1 = 3)
    // Direct heavy: a -> d            (10)
    g.add_edge(Edge::new(a, b, "x").with_weight(1.0));
    g.add_edge(Edge::new(b, c, "x").with_weight(1.0));
    g.add_edge(Edge::new(c, d, "x").with_weight(1.0));
    g.add_edge(Edge::new(a, d, "x").with_weight(10.0));

    let p = traverse::shortest_path(
        &g,
        a,
        d,
        ShortestPathAlgorithm::Dijkstra,
        Direction::Outgoing,
        None,
        limits(),
    )
    .expect("path");
    assert!((p.total_weight - 3.0).abs() < f64::EPSILON);
    assert_eq!(p.nodes.len(), 4);
}

#[test]
fn dijkstra_returns_none_for_unreachable() {
    let g = GraphStore::new();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    g.add_edge(Edge::new(a, b, "x").with_weight(1.0));
    // c is isolated.
    let p = traverse::shortest_path(
        &g,
        a,
        c,
        ShortestPathAlgorithm::Dijkstra,
        Direction::Outgoing,
        None,
        limits(),
    );
    assert!(p.is_none());
}

#[test]
fn bidirectional_bfs_matches_bfs_path_length() {
    let g = GraphStore::new();
    let nodes: Vec<_> = (0..8).map(|_| Uuid::new_v4()).collect();
    for w in nodes.windows(2) {
        g.add_edge(Edge::new(w[0], w[1], "next"));
    }
    let p1 = traverse::shortest_path(
        &g,
        nodes[0],
        nodes[7],
        ShortestPathAlgorithm::Bfs,
        Direction::Both,
        None,
        limits(),
    )
    .expect("bfs path");
    let p2 = traverse::shortest_path(
        &g,
        nodes[0],
        nodes[7],
        ShortestPathAlgorithm::BidirectionalBfs,
        Direction::Both,
        None,
        limits(),
    )
    .expect("bidi path");
    assert_eq!(p1.hops(), p2.hops());
    assert_eq!(p1.hops(), 7);
}

#[test]
fn dijkstra_ignores_negative_weight_edge() {
    let g = GraphStore::new();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    g.add_edge(Edge::new(a, b, "x").with_weight(-5.0));
    g.add_edge(Edge::new(a, c, "x").with_weight(2.0));
    g.add_edge(Edge::new(c, b, "x").with_weight(2.0));
    // The negative-weight edge would give a "free shortcut" -- but
    // the implementation skips it, so the path goes a -> c -> b @ 4.
    let p = traverse::shortest_path(
        &g,
        a,
        b,
        ShortestPathAlgorithm::Dijkstra,
        Direction::Outgoing,
        None,
        limits(),
    )
    .expect("path");
    assert!((p.total_weight - 4.0).abs() < f64::EPSILON);
}
