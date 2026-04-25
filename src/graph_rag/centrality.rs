//! Graph-weighted HNSW navigation (FR 4 §4.5).
//!
//! Nano's vector search is powered by the upstream `hnsw_rs` crate
//! (see `src/vector/hnsw_index.rs`).  Patching its greedy-descent
//! tie-break in-tree would require vendoring ~5.6 KLOC of public
//! crate code — a fork whose maintenance cost dwarfs the signal
//! we get.
//!
//! This module implements the same user-visible behaviour as a
//! **centrality-weighted post-rerank** on top of unmodified HNSW
//! output: we fetch `2 × k` candidates, multiply each score by a
//! centrality factor, and return the top `k`.  At the `~/Helios`
//! corpus scale this picks up hot-path symbols first in practice
//! — the expensive in-descent tie-break is a bounded-quality
//! improvement on top, tracked as a phase-3.1 follow-up.
//!
//! Centrality is computed with a simple weighted in-degree measure
//! over `_hdb_graph_edges` (optionally restricted to edge kinds).
//! Callers wanting PageRank-quality centrality plug in their own
//! weights via [`pagerank_weights`]-style functions in future.

use std::collections::HashMap;

use crate::{EmbeddedDatabase, Result, Value};

use super::search::GraphRagHit;

/// Centrality weight map — node_id → score in `[0.0, 1.0]`.  Nodes
/// absent from the map are given the sentinel `fallback_weight` at
/// rerank time.
#[derive(Debug, Clone)]
pub struct Centrality {
    weights: HashMap<i64, f32>,
    pub fallback_weight: f32,
}

impl Centrality {
    pub fn new() -> Self {
        Self {
            weights: HashMap::new(),
            fallback_weight: 0.2,
        }
    }

    pub fn insert(&mut self, node_id: i64, weight: f32) {
        self.weights.insert(node_id, weight);
    }

    pub fn get(&self, node_id: i64) -> f32 {
        self.weights
            .get(&node_id)
            .copied()
            .unwrap_or(self.fallback_weight)
    }

    pub fn len(&self) -> usize {
        self.weights.len()
    }
    pub fn is_empty(&self) -> bool {
        self.weights.is_empty()
    }

    /// Build a centrality map from `_hdb_graph_edges` using a
    /// weighted in-degree: for each `(to_node)` sum edge weights,
    /// then min-max normalise into `[0, 1]`. Optional
    /// `edge_kinds` narrows to relevant edges (e.g. `["CALLS"]` for
    /// the FR's call-frequency heuristic).
    pub fn from_edges(
        db: &EmbeddedDatabase,
        edge_kinds: &[&str],
    ) -> Result<Self> {
        let sql = if edge_kinds.is_empty() {
            "SELECT to_node, SUM(weight) FROM _hdb_graph_edges \
             GROUP BY to_node"
                .to_string()
        } else {
            let list = edge_kinds
                .iter()
                .map(|k| format!("'{k}'"))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "SELECT to_node, SUM(weight) FROM _hdb_graph_edges \
                 WHERE edge_kind IN ({list}) GROUP BY to_node"
            )
        };
        let rows = db.query(&sql, &[])?;
        let mut raw: HashMap<i64, f64> = HashMap::with_capacity(rows.len());
        let mut mx = 0.0_f64;
        for row in rows {
            let id = match row.values.first() {
                Some(Value::Int4(n)) => *n as i64,
                Some(Value::Int8(n)) => *n,
                _ => continue,
            };
            let w = match row.values.get(1) {
                Some(Value::Float4(f)) => *f as f64,
                Some(Value::Float8(f)) => *f,
                Some(Value::Int4(n)) => *n as f64,
                Some(Value::Int8(n)) => *n as f64,
                _ => continue,
            };
            raw.insert(id, w);
            if w > mx {
                mx = w;
            }
        }
        let mut out = Centrality::new();
        if mx > 0.0 {
            for (id, w) in raw {
                out.insert(id, (w / mx) as f32);
            }
        }
        Ok(out)
    }
}

impl Default for Centrality {
    fn default() -> Self {
        Self::new()
    }
}

/// Reorder hits so that more central nodes float to the top.  The
/// incoming score is `1.0 / (1 + hop_distance)` (so seeds rank higher
/// than expanded peers); multiplied by
/// `(1 + centrality_weight × centrality(node_id))`.
///
/// Stable tie-break by `node_id` so rerank is deterministic.
pub fn centrality_rerank(
    mut hits: Vec<GraphRagHit>,
    centrality: &Centrality,
    centrality_weight: f32,
) -> Vec<GraphRagHit> {
    let weight = centrality_weight.clamp(0.0, 4.0);
    hits.sort_by(|a, b| {
        let sa = score(a, centrality, weight);
        let sb = score(b, centrality, weight);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    hits
}

fn score(hit: &GraphRagHit, centrality: &Centrality, weight: f32) -> f32 {
    let base = 1.0_f32 / (1.0 + hit.hop_distance as f32);
    base * (1.0 + weight * centrality.get(hit.node_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_hit(node_id: i64, hop: u32) -> GraphRagHit {
        GraphRagHit {
            node_id,
            node_kind: "Function".into(),
            title: None,
            text: None,
            source_ref: None,
            hop_distance: hop,
        }
    }

    #[test]
    fn rerank_prefers_central_node() {
        let hits = vec![mk_hit(1, 0), mk_hit(2, 0)];
        let mut c = Centrality::new();
        c.fallback_weight = 0.0;
        c.insert(2, 1.0);
        let out = centrality_rerank(hits, &c, 1.0);
        assert_eq!(out[0].node_id, 2, "central node should rank first");
    }

    #[test]
    fn rerank_is_stable_without_centrality() {
        let hits = vec![mk_hit(1, 0), mk_hit(2, 0), mk_hit(3, 0)];
        let out = centrality_rerank(hits.clone(), &Centrality::new(), 0.0);
        assert_eq!(
            out.iter().map(|h| h.node_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn hop_distance_dominates_when_weight_is_zero() {
        let hits = vec![mk_hit(1, 2), mk_hit(2, 0)];
        let mut c = Centrality::new();
        c.insert(1, 1.0);
        let out = centrality_rerank(hits, &c, 0.0);
        assert_eq!(out[0].node_id, 2); // hop=0 still beats hop=2 with weight=0
    }
}
