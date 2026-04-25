//! Centrality-biased + prefilter-aware HNSW search.
//!
//! `hnsw_rs` does not expose its inner descent loop, and forking
//! the whole 5600-line crate just to add a tie-break would be
//! perpetual maintenance burden.  Instead, we wrap the existing
//! `search_with_ef` API:
//!
//! 1. Over-fetch — request `k × over_fetch_multiplier` candidates
//!    from HNSW with a proportionally higher `ef_search`.
//! 2. Apply the prefilter at the candidate level (drop rejects
//!    before re-ranking).
//! 3. Re-score by centrality when a centrality vector is supplied:
//!    `score = (1 - alpha) × normalised_distance + alpha × centrality`.
//! 4. Truncate to k.
//!
//! Equivalent to the FR's "Option B (post-rerank)" relevance lift,
//! delivered without forking hnsw_rs.  Truer in-descent bias would
//! require modifying the navigation graph itself.

use std::collections::HashMap;

use crate::Result;

/// Caller-supplied per-row centrality, normalised to `[0, 1]`.
/// Missing rows default to `0.0`.  Build with
/// `graph_rag::Centrality::from_edges` or any external scoring.
pub type CentralityMap = HashMap<u64, f32>;

#[derive(Debug, Clone)]
pub struct BiasOptions {
    /// `0.0` → ignore centrality (pure distance).
    /// `1.0` → ignore distance (pure centrality).
    /// Default `0.2` — light bias.
    pub alpha: f32,
    /// Pull `k × over_fetch_multiplier` candidates from HNSW so the
    /// re-rank has room to reorder.  Set higher when prefilter is
    /// aggressive.  Default `4`.
    pub over_fetch_multiplier: usize,
}

impl Default for BiasOptions {
    fn default() -> Self {
        Self { alpha: 0.2, over_fetch_multiplier: 4 }
    }
}

/// Apply centrality bias + a row-level prefilter to a candidate
/// set already returned by HNSW. Candidates that fail
/// `prefilter(row_id) == false` are dropped.  Re-scored as
/// described above.  Returns the top-`k` `(row_id, score)` pairs
/// sorted by score ascending (lower = better, matching distance
/// semantics).
pub fn apply_bias(
    candidates: Vec<(u64, f32)>,
    k: usize,
    centrality: Option<&CentralityMap>,
    prefilter: Option<&dyn Fn(u64) -> bool>,
    opts: &BiasOptions,
) -> Result<Vec<(u64, f32)>> {
    if candidates.is_empty() || k == 0 {
        return Ok(Vec::new());
    }
    // Filter first.
    let kept: Vec<(u64, f32)> = candidates
        .into_iter()
        .filter(|(id, _)| prefilter.map_or(true, |f| f(*id)))
        .collect();
    if kept.is_empty() {
        return Ok(Vec::new());
    }

    // Re-score with centrality bias if one was supplied.  Without
    // a centrality map this is a pure top-k by distance, which the
    // input order already provides.
    let mut scored: Vec<(u64, f32)> = if let Some(cent) = centrality {
        let max_dist = kept
            .iter()
            .map(|(_, d)| *d)
            .fold(f32::MIN, f32::max);
        let alpha = opts.alpha.clamp(0.0, 1.0);
        kept.into_iter()
            .map(|(id, dist)| {
                let normalised = if max_dist > 0.0 { dist / max_dist } else { 0.0 };
                let cent_v = cent.get(&id).copied().unwrap_or(0.0).clamp(0.0, 1.0);
                // Lower score = better.  Centrality is "more = better"
                // so we subtract.
                let score = (1.0 - alpha) * normalised - alpha * cent_v;
                (id, score)
            })
            .collect()
    } else {
        kept
    };
    scored.sort_by(|a, b| {
        a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(k);
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cent_map(pairs: &[(u64, f32)]) -> CentralityMap {
        pairs.iter().copied().collect()
    }

    #[test]
    fn no_bias_no_prefilter_preserves_top_k() {
        let cands = vec![(1, 0.1), (2, 0.2), (3, 0.3), (4, 0.4)];
        let r = apply_bias(cands, 2, None, None, &BiasOptions::default()).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0, 1);
        assert_eq!(r[1].0, 2);
    }

    #[test]
    fn prefilter_drops_non_matches_before_truncation() {
        let cands = vec![(1, 0.1), (2, 0.2), (3, 0.3), (4, 0.4)];
        let pf = |id: u64| id % 2 == 0; // only evens pass
        let r = apply_bias(cands, 2, None, Some(&pf), &BiasOptions::default()).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0, 2);
        assert_eq!(r[1].0, 4);
    }

    #[test]
    fn centrality_promotes_well_connected_node() {
        // Without bias: 1 wins on distance.  With heavy centrality
        // weighting, 3 (high centrality) overtakes.
        let cands = vec![(1, 0.10), (2, 0.20), (3, 0.30)];
        let cent = cent_map(&[(1, 0.0), (2, 0.0), (3, 1.0)]);
        let opts = BiasOptions { alpha: 0.9, over_fetch_multiplier: 1 };
        let r = apply_bias(cands, 1, Some(&cent), None, &opts).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 3, "high-centrality node should win");
    }

    #[test]
    fn alpha_zero_ignores_centrality() {
        let cands = vec![(1, 0.10), (2, 0.20), (3, 0.30)];
        let cent = cent_map(&[(1, 0.0), (2, 0.0), (3, 1.0)]);
        let opts = BiasOptions { alpha: 0.0, ..BiasOptions::default() };
        let r = apply_bias(cands, 1, Some(&cent), None, &opts).unwrap();
        assert_eq!(r[0].0, 1, "alpha=0 must fall back to pure distance");
    }

    #[test]
    fn empty_inputs_are_safe() {
        let r = apply_bias(Vec::new(), 5, None, None, &BiasOptions::default()).unwrap();
        assert!(r.is_empty());
        let cands = vec![(1, 0.1)];
        let r = apply_bias(cands, 0, None, None, &BiasOptions::default()).unwrap();
        assert!(r.is_empty());
    }
}
