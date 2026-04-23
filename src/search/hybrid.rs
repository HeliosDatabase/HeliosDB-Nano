//! Hybrid (BM25 + vector) search orchestration.
//!
//! RAG-native (idea 2).

use super::bm25::{Bm25Index, Bm25Score};
use super::reranker::{mmr_rerank, rrf_fuse, RrfParams};

/// Method used to combine BM25 + vector hits into a single ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FusionMethod {
    /// Reciprocal Rank Fusion (Cormack et al. 2009).
    Rrf,
    /// Maximal Marginal Relevance reranking.
    ///
    /// MMR requires per-document vectors -- the caller must populate
    /// the `vector` field on each input. Documents missing vectors
    /// are dropped from the MMR pool.
    Mmr,
    /// Weighted linear combination of normalised scores.
    /// `score = lambda * vec_norm + (1 - lambda) * bm25_norm`.
    Linear,
}

/// A scored hit against a single ranking source.
#[derive(Debug, Clone)]
pub struct ScoredHit {
    pub doc_id: u64,
    pub score: f64,
    /// Optional dense vector for the doc (required by MMR fusion).
    pub vector: Option<Vec<f32>>,
}

/// Final hybrid result for a single document.
#[derive(Debug, Clone, PartialEq)]
pub struct HybridHit {
    pub doc_id: u64,
    pub score: f64,
}

/// Run a hybrid query that fuses BM25 + vector hits.
///
/// `bm25_hits`  -- pre-ranked BM25 results (highest score first).
/// `vector_hits` -- pre-ranked vector results (highest score first).
/// `method`     -- fusion strategy.
/// `lambda`     -- weight knob (used by MMR + Linear; ignored by RRF).
/// `limit`      -- max number of fused results to return.
#[must_use]
pub fn hybrid_search(
    bm25_hits: &[ScoredHit],
    vector_hits: &[ScoredHit],
    method: FusionMethod,
    lambda: f64,
    limit: usize,
) -> Vec<HybridHit> {
    match method {
        FusionMethod::Rrf => {
            let lists = vec![
                bm25_hits.iter().map(|h| h.doc_id).collect::<Vec<_>>(),
                vector_hits.iter().map(|h| h.doc_id).collect::<Vec<_>>(),
            ];
            let fused = rrf_fuse(&lists, RrfParams::default());
            fused
                .into_iter()
                .take(limit)
                .map(|(doc_id, score)| HybridHit { doc_id, score })
                .collect()
        }
        FusionMethod::Mmr => {
            // Build a candidate set keyed by doc_id; carry the vector and
            // use the vector-side score as the query similarity. (The BM25
            // signal is already implicit in the candidate selection.)
            use std::collections::HashMap;
            let mut by_id: HashMap<u64, ScoredHit> = HashMap::new();
            for h in vector_hits.iter().chain(bm25_hits.iter()) {
                by_id.entry(h.doc_id).or_insert_with(|| h.clone());
            }
            let candidates: Vec<_> = by_id
                .into_values()
                .filter_map(|h| h.vector.map(|v| (h.doc_id, h.score, v)))
                .collect();
            let reranked = mmr_rerank(candidates, lambda, limit);
            reranked
                .into_iter()
                .map(|(doc_id, score)| HybridHit { doc_id, score })
                .collect()
        }
        FusionMethod::Linear => {
            let bm25_norm = normalised_scores(bm25_hits);
            let vec_norm = normalised_scores(vector_hits);
            use std::collections::HashMap;
            let mut combined: HashMap<u64, f64> = HashMap::new();
            for (id, score) in &bm25_norm {
                *combined.entry(*id).or_insert(0.0) += (1.0 - lambda) * score;
            }
            for (id, score) in &vec_norm {
                *combined.entry(*id).or_insert(0.0) += lambda * score;
            }
            let mut out: Vec<HybridHit> = combined
                .into_iter()
                .map(|(doc_id, score)| HybridHit { doc_id, score })
                .collect();
            out.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.doc_id.cmp(&b.doc_id))
            });
            out.truncate(limit);
            out
        }
    }
}

/// Min-max normalise a list of scored hits to `[0, 1]`.
///
/// Edge cases:
/// - Empty input -> empty output.
/// - Single element (or all scores tied) -> every element maps to `1.0`
///   (treating "the only candidate" as fully relevant rather than zeroing it out,
///   which would defeat single-side-only fusion).
fn normalised_scores(hits: &[ScoredHit]) -> Vec<(u64, f64)> {
    if hits.is_empty() {
        return Vec::new();
    }
    let mut max = f64::MIN;
    let mut min = f64::MAX;
    for h in hits {
        if h.score > max {
            max = h.score;
        }
        if h.score < min {
            min = h.score;
        }
    }
    let range = max - min;
    if range <= f64::EPSILON {
        return hits.iter().map(|h| (h.doc_id, 1.0)).collect();
    }
    hits.iter().map(|h| (h.doc_id, (h.score - min) / range)).collect()
}

/// Convenience: run BM25 against an in-memory index and lift the
/// results into [`ScoredHit`] form (vector left empty).
#[must_use]
pub fn bm25_hits(index: &Bm25Index, query: &str, limit: Option<usize>) -> Vec<ScoredHit> {
    index
        .score(query, limit)
        .into_iter()
        .map(|Bm25Score { doc_id, score }| ScoredHit {
            doc_id,
            score,
            vector: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(ids: &[u64], scores: &[f64]) -> Vec<ScoredHit> {
        ids.iter()
            .zip(scores.iter())
            .map(|(id, s)| ScoredHit {
                doc_id: *id,
                score: *s,
                vector: None,
            })
            .collect()
    }

    #[test]
    fn rrf_combines_bm25_and_vec_lists() {
        let bm25 = hits(&[1, 2, 3], &[3.0, 2.0, 1.0]);
        let vec = hits(&[3, 1, 4], &[0.9, 0.8, 0.7]);
        let res = hybrid_search(&bm25, &vec, FusionMethod::Rrf, 0.5, 10);
        let ids: Vec<_> = res.iter().map(|h| h.doc_id).collect();
        // doc 1 (top of BM25, mid of vec) ranks first; doc 3 second.
        assert_eq!(ids[0], 1);
        assert!(ids.contains(&3));
        assert!(ids.contains(&4));
    }

    #[test]
    fn linear_fusion_respects_lambda() {
        let bm25 = hits(&[1, 2], &[10.0, 1.0]);
        let vec = hits(&[2, 1], &[10.0, 1.0]);
        // lambda=1 -> pure vector ordering -> doc 2 first.
        let r1 = hybrid_search(&bm25, &vec, FusionMethod::Linear, 1.0, 10);
        assert_eq!(r1[0].doc_id, 2);
        // lambda=0 -> pure BM25 ordering -> doc 1 first.
        let r0 = hybrid_search(&bm25, &vec, FusionMethod::Linear, 0.0, 10);
        assert_eq!(r0[0].doc_id, 1);
    }

    #[test]
    fn mmr_uses_vectors_when_provided() {
        let bm25 = hits(&[1, 2, 3], &[1.0, 1.0, 1.0]);
        let vec = vec![
            ScoredHit {
                doc_id: 1,
                score: 0.9,
                vector: Some(vec![1.0, 0.0]),
            },
            ScoredHit {
                doc_id: 2,
                score: 0.85,
                vector: Some(vec![1.0, 0.0]),
            },
            ScoredHit {
                doc_id: 3,
                score: 0.7,
                vector: Some(vec![0.0, 1.0]),
            },
        ];
        let res = hybrid_search(&bm25, &vec, FusionMethod::Mmr, 0.3, 3);
        assert_eq!(res.len(), 3);
        // First pick is highest q_sim (doc 1); second prefers diverse
        // doc 3 over the near-duplicate doc 2 because lambda is low.
        assert_eq!(res[0].doc_id, 1);
        assert_eq!(res[1].doc_id, 3);
    }

    #[test]
    fn limit_caps_output() {
        let bm25 = hits(&[1, 2, 3, 4, 5], &[5.0, 4.0, 3.0, 2.0, 1.0]);
        let vec = hits(&[5, 4, 3, 2, 1], &[0.9, 0.8, 0.7, 0.6, 0.5]);
        let res = hybrid_search(&bm25, &vec, FusionMethod::Rrf, 0.5, 2);
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn empty_inputs_safe() {
        let res = hybrid_search(&[], &[], FusionMethod::Rrf, 0.5, 5);
        assert!(res.is_empty());
        let res2 = hybrid_search(&[], &[], FusionMethod::Linear, 0.5, 5);
        assert!(res2.is_empty());
    }

    #[test]
    fn bm25_hits_helper_lifts_index() {
        let idx = Bm25Index::new();
        idx.add_document(1, "alpha beta");
        idx.add_document(2, "alpha gamma");
        let h = bm25_hits(&idx, "alpha", None);
        assert_eq!(h.len(), 2);
        assert!(h.iter().all(|x| x.vector.is_none()));
    }
}
