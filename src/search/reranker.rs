//! Reranking algorithms: Reciprocal Rank Fusion + Maximal Marginal Relevance.
//!
//! HelixDB-inspired (idea 2).

use std::collections::HashMap;

/// Hyperparameters for [`rrf_fuse`].
#[derive(Debug, Clone, Copy)]
pub struct RrfParams {
    /// Smoothing constant -- typical value `60.0` (Cormack et al. 2009).
    pub k: f64,
}

impl Default for RrfParams {
    fn default() -> Self {
        Self { k: 60.0 }
    }
}

/// Fuse multiple ranked lists into a single ranking via RRF.
///
/// Each input list contributes `1 / (k + rank)` to the document's
/// fused score. Documents are returned sorted by descending score.
///
/// `lists[i]` is treated as a ranking from highest- to lowest-relevance.
#[must_use]
pub fn rrf_fuse(lists: &[Vec<u64>], params: RrfParams) -> Vec<(u64, f64)> {
    let mut scores: HashMap<u64, f64> = HashMap::new();
    for list in lists {
        for (rank, doc_id) in list.iter().enumerate() {
            // rank is zero-based; use rank+1 so the top result gets weight 1/(k+1).
            let contrib = 1.0 / (params.k + (rank + 1) as f64);
            *scores.entry(*doc_id).or_insert(0.0) += contrib;
        }
    }
    let mut out: Vec<_> = scores.into_iter().collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Deterministic tie-break on doc id.
            .then(a.0.cmp(&b.0))
    });
    out
}

/// Maximal Marginal Relevance reranking.
///
/// Picks results that balance relevance to the query against diversity
/// from already-selected results, using the standard formula
/// `MMR = lambda * sim(q, d) - (1 - lambda) * max_{d' in S} sim(d, d')`.
///
/// Inputs:
/// - `candidates`: `(doc_id, query_similarity, doc_vector)` for each candidate.
/// - `lambda`: trade-off knob in `[0, 1]`. `1.0` = pure relevance,
///   `0.0` = pure diversity. Typical: `0.5..=0.8`.
/// - `top_k`: number of results to return.
///
/// Document similarity is cosine similarity over the supplied vectors.
#[must_use]
pub fn mmr_rerank(mut candidates: Vec<(u64, f64, Vec<f32>)>, lambda: f64, top_k: usize) -> Vec<(u64, f64)> {
    let lambda = lambda.clamp(0.0, 1.0);
    let mut selected: Vec<(u64, f64, Vec<f32>)> = Vec::with_capacity(top_k.min(candidates.len()));

    // Sort initial candidates by descending query similarity so the first
    // pick is the most relevant -- a standard tie-break for empty selection.
    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    while !candidates.is_empty() && selected.len() < top_k {
        let mut best_idx = 0;
        let mut best_score = f64::MIN;
        for (i, (_, q_sim, vec)) in candidates.iter().enumerate() {
            let max_sim = selected
                .iter()
                .map(|(_, _, sv)| cosine(vec, sv))
                .fold(0.0_f64, f64::max);
            let mmr = lambda * q_sim - (1.0 - lambda) * max_sim;
            if mmr > best_score {
                best_score = mmr;
                best_idx = i;
            }
        }
        let chosen = candidates.swap_remove(best_idx);
        selected.push((chosen.0, best_score, chosen.2));
    }
    selected.into_iter().map(|(id, score, _)| (id, score)).collect()
}

/// Cosine similarity. Returns `0.0` if either vector is zero-length.
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let n = a.len().min(b.len());
    let mut dot = 0.0_f64;
    let mut na = 0.0_f64;
    let mut nb = 0.0_f64;
    for i in 0..n {
        let x = f64::from(a[i]);
        let y = f64::from(b[i]);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na.sqrt() * nb.sqrt()).max(f64::MIN_POSITIVE);
    dot / denom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_fuses_two_lists() {
        // List 1: [1, 2, 3]   -- contributions: 1/61, 1/62, 1/63
        // List 2: [3, 1, 4]   -- contributions: 1/61, 1/62, 1/63
        // Doc 1 appears at ranks 1 and 2 -> 1/61 + 1/62 = highest.
        // Doc 3 appears at ranks 3 and 1 -> 1/63 + 1/61.
        let lists = vec![vec![1u64, 2, 3], vec![3, 1, 4]];
        let fused = rrf_fuse(&lists, RrfParams::default());
        let ids: Vec<_> = fused.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids[0], 1, "doc 1 should rank first");
        // Doc 3 and doc 2 both get 1/61 + 1/63 vs 1/62 -- 3 wins.
        assert_eq!(ids[1], 3);
    }

    #[test]
    fn rrf_empty_lists_yields_empty() {
        let fused = rrf_fuse(&[], RrfParams::default());
        assert!(fused.is_empty());
    }

    #[test]
    fn rrf_known_smoke_values() {
        let lists = vec![vec![10u64], vec![10u64]];
        let fused = rrf_fuse(&lists, RrfParams { k: 60.0 });
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].0, 10);
        let expected = 2.0 / 61.0;
        assert!((fused[0].1 - expected).abs() < 1e-9);
    }

    #[test]
    fn mmr_picks_top_relevance_first() {
        let candidates = vec![
            (1u64, 0.9, vec![1.0, 0.0]),
            (2, 0.8, vec![1.0, 0.0]), // very similar to doc 1
            (3, 0.5, vec![0.0, 1.0]), // orthogonal -- diverse
        ];
        let res = mmr_rerank(candidates, 1.0, 3);
        // Pure relevance (lambda=1) -> insertion order by q_sim.
        assert_eq!(res[0].0, 1);
        assert_eq!(res[1].0, 2);
        assert_eq!(res[2].0, 3);
    }

    #[test]
    fn mmr_balances_diversity_when_lambda_low() {
        let candidates = vec![
            (1u64, 0.9, vec![1.0, 0.0]),
            (2, 0.85, vec![1.0, 0.0]), // very similar to doc 1
            (3, 0.7, vec![0.0, 1.0]),  // orthogonal
        ];
        let res = mmr_rerank(candidates, 0.3, 3);
        assert_eq!(res[0].0, 1);
        // Second pick should prefer the orthogonal doc 3 over the
        // near-duplicate doc 2.
        assert_eq!(res[1].0, 3);
    }

    #[test]
    fn mmr_top_k_bounds_output() {
        let candidates = vec![(1u64, 0.9, vec![1.0]), (2, 0.8, vec![1.0]), (3, 0.7, vec![1.0])];
        let res = mmr_rerank(candidates, 0.5, 2);
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn cosine_handles_zero_vector() {
        assert_eq!(cosine(&[], &[1.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[]), 0.0);
    }

    #[test]
    fn cosine_unit_vectors() {
        let s = cosine(&[1.0, 0.0], &[1.0, 0.0]);
        assert!((s - 1.0).abs() < 1e-9);
        let o = cosine(&[1.0, 0.0], &[0.0, 1.0]);
        assert!(o.abs() < 1e-9);
    }
}
