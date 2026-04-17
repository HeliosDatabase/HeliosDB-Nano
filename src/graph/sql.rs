//! SQL adapter for graph traversal.
//!
//! RAG-native (idea 1).
//!
//! Wires the in-memory `GraphStore` up to typed SQL values so it can be
//! called from callers that speak `crate::Value`. Full integration with
//! the SQL function registry is deferred to a follow-up (the registry
//! currently expects procedural-language function bodies, not native
//! Rust handlers); these adapters are used by the MCP tools (idea 5)
//! and can be re-used when the registry grows native-function support.

use std::sync::Arc;

use crate::{Error, Result, Value};

use super::storage::{Direction, GraphStore, NodeId};
use super::traverse::{self, Path, ShortestPathAlgorithm, TraversalLimits};

/// Parse a text direction into the internal enum.
pub fn parse_direction(s: &str) -> Result<Direction> {
    match s.to_ascii_lowercase().as_str() {
        "out" | "outgoing" => Ok(Direction::Outgoing),
        "in" | "incoming" => Ok(Direction::Incoming),
        "both" | "any" => Ok(Direction::Both),
        other => Err(Error::query_execution(format!(
            "graph direction '{other}' not recognised (expected one of: out, in, both)"
        ))),
    }
}

/// Parse a text algorithm into the internal enum.
pub fn parse_algorithm(s: &str) -> Result<ShortestPathAlgorithm> {
    match s.to_ascii_lowercase().as_str() {
        "bfs" => Ok(ShortestPathAlgorithm::Bfs),
        "dijkstra" => Ok(ShortestPathAlgorithm::Dijkstra),
        "bidi" | "bidirectional" | "bidirectional_bfs" => Ok(ShortestPathAlgorithm::BidirectionalBfs),
        other => Err(Error::query_execution(format!(
            "graph algorithm '{other}' not recognised (expected one of: bfs, dijkstra, bidi)"
        ))),
    }
}

/// Extract a node id from a `Value` (accepts `Value::Uuid` or a parseable string).
pub fn node_id_from_value(v: &Value) -> Result<NodeId> {
    match v {
        Value::Uuid(u) => Ok(*u),
        Value::String(s) => {
            uuid::Uuid::parse_str(s).map_err(|e| Error::query_execution(format!("invalid uuid '{s}': {e}")))
        }
        other => Err(Error::query_execution(format!(
            "expected UUID-valued node id, got {other:?}"
        ))),
    }
}

/// Row shape returned by [`graph_traverse`].
#[derive(Debug, Clone, PartialEq)]
pub struct TraverseRow {
    pub node: NodeId,
    pub depth: usize,
}

/// BFS traversal -- equivalent SQL surface:
/// `graph_traverse(start, edge_label, direction, depth) -> TABLE(node, depth)`.
pub fn graph_traverse(
    graph: &Arc<GraphStore>,
    start: NodeId,
    edge_label: Option<&str>,
    direction: Direction,
    max_depth: usize,
) -> Vec<TraverseRow> {
    let limits = TraversalLimits {
        max_depth,
        ..Default::default()
    };
    traverse::bfs(graph, start, direction, edge_label, limits)
        .into_iter()
        .map(|(node, depth)| TraverseRow { node, depth })
        .collect()
}

/// Shortest-path -- equivalent SQL surface:
/// `graph_shortest_path(from, to, algorithm, weight_col_ignored) -> TABLE(node, ...)`.
///
/// `weight_col` is accepted for parity with the plan's SQL signature but
/// currently ignored -- weights live on the edges themselves.
pub fn graph_shortest_path(
    graph: &Arc<GraphStore>,
    from: NodeId,
    to: NodeId,
    algorithm: ShortestPathAlgorithm,
    direction: Direction,
    edge_label: Option<&str>,
) -> Option<Path> {
    traverse::shortest_path(
        graph,
        from,
        to,
        algorithm,
        direction,
        edge_label,
        TraversalLimits::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::storage::Edge;
    use std::sync::Arc;

    #[test]
    fn parse_direction_round_trip() {
        assert_eq!(parse_direction("out").unwrap(), Direction::Outgoing);
        assert_eq!(parse_direction("INCOMING").unwrap(), Direction::Incoming);
        assert_eq!(parse_direction("both").unwrap(), Direction::Both);
        assert!(parse_direction("sideways").is_err());
    }

    #[test]
    fn parse_algorithm_variants() {
        assert_eq!(parse_algorithm("bfs").unwrap(), ShortestPathAlgorithm::Bfs);
        assert_eq!(parse_algorithm("DIJKSTRA").unwrap(), ShortestPathAlgorithm::Dijkstra);
        assert_eq!(
            parse_algorithm("bidi").unwrap(),
            ShortestPathAlgorithm::BidirectionalBfs
        );
        assert!(parse_algorithm("a_star").is_err());
    }

    #[test]
    fn node_id_from_value_accepts_uuid_and_string() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(node_id_from_value(&Value::Uuid(id)).unwrap(), id);
        assert_eq!(node_id_from_value(&Value::String(id.to_string())).unwrap(), id);
        assert!(node_id_from_value(&Value::Int4(42)).is_err());
        assert!(node_id_from_value(&Value::String("not-a-uuid".into())).is_err());
    }

    #[test]
    fn traverse_adapter_returns_bfs_rows() {
        let g = Arc::new(GraphStore::new());
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let c = uuid::Uuid::new_v4();
        g.add_edge(Edge::new(a, b, "x"));
        g.add_edge(Edge::new(b, c, "x"));
        let rows = graph_traverse(&g, a, Some("x"), Direction::Outgoing, 5);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[2].depth, 2);
    }

    #[test]
    fn shortest_path_adapter_returns_path() {
        let g = Arc::new(GraphStore::new());
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        g.add_edge(Edge::new(a, b, "x").with_weight(3.0));
        let p = graph_shortest_path(
            &g,
            a,
            b,
            ShortestPathAlgorithm::Dijkstra,
            Direction::Outgoing,
            Some("x"),
        )
        .expect("path");
        assert!((p.total_weight - 3.0).abs() < f64::EPSILON);
    }
}
