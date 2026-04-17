//! SQL adapter integration tests for the graph module.
//!
//! RAG-native (idea 1).

use heliosdb_nano::graph::{
    sql,
    storage::{Direction, Edge, GraphStore},
    traverse::ShortestPathAlgorithm,
};
use heliosdb_nano::Value;
use std::sync::Arc;
use uuid::Uuid;

#[test]
fn parse_helpers_roundtrip() {
    assert_eq!(sql::parse_direction("out").unwrap(), Direction::Outgoing);
    assert_eq!(
        sql::parse_algorithm("dijkstra").unwrap(),
        ShortestPathAlgorithm::Dijkstra
    );
    assert!(sql::parse_direction("nope").is_err());
    assert!(sql::parse_algorithm("astar").is_err());
}

#[test]
fn graph_traverse_via_adapter() {
    let g = Arc::new(GraphStore::new());
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    g.add_edge(Edge::new(a, b, "knows"));
    g.add_edge(Edge::new(b, c, "knows"));

    let rows = sql::graph_traverse(&g, a, Some("knows"), Direction::Outgoing, 5);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].depth, 0);
    assert_eq!(rows[2].depth, 2);
}

#[test]
fn graph_shortest_path_via_adapter() {
    let g = Arc::new(GraphStore::new());
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    g.add_edge(Edge::new(a, b, "x").with_weight(1.0));
    g.add_edge(Edge::new(b, c, "x").with_weight(2.0));

    let p = sql::graph_shortest_path(
        &g,
        a,
        c,
        ShortestPathAlgorithm::Dijkstra,
        Direction::Outgoing,
        Some("x"),
    )
    .expect("path");
    assert!((p.total_weight - 3.0).abs() < f64::EPSILON);
    assert_eq!(p.hops(), 2);
}

#[test]
fn node_id_value_extraction_handles_uuid_and_string() {
    let id = Uuid::new_v4();
    assert_eq!(sql::node_id_from_value(&Value::Uuid(id)).unwrap(), id);
    assert_eq!(sql::node_id_from_value(&Value::String(id.to_string())).unwrap(), id);
    assert!(sql::node_id_from_value(&Value::Int4(7)).is_err());
}
