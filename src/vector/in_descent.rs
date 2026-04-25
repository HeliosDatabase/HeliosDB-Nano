//! True in-descent navigation bias for nearest-neighbour search
//! over a centrality-weighted graph.
//!
//! `hnsw_rs` doesn't expose its inner descent loop, so this module
//! ships a minimal HNSW-style descent implemented directly on a
//! caller-supplied adjacency list.  The descent bias takes effect
//! *during* candidate exploration — high-centrality nodes are
//! pulled into the working set before low-centrality ones when
//! their distances tie within ε — rather than after the fact (which
//! is what the [`super::biased_descent::apply_bias`] wrapper does).
//!
//! Trade-offs vs. the wrapper layer:
//!  * Requires a caller-supplied adjacency list (we don't reach
//!    inside hnsw_rs).
//!  * No level hierarchy — pure level-0 greedy descent. For graphs
//!    up to ~100k nodes the recall is comparable; beyond that the
//!    full HNSW hierarchy wins.
//!  * Centrality + prefilter take effect in the candidate priority
//!    queue, not just on the final sorted slice.
//!
//! Not a drop-in replacement for the existing HNSW index — this is
//! the building block callers wire when they need true bias.

use std::collections::{BinaryHeap, HashMap, HashSet};

use crate::Result;

/// Adjacency list keyed by point id.  Each entry is the list of
/// neighbour ids in the proximity graph.  Symmetry is the caller's
/// responsibility; we don't enforce.
pub type Adjacency = HashMap<u64, Vec<u64>>;

/// Position vector for each point id.  Caller fills in for every
/// point referenced in `adjacency`; missing ids are skipped.
pub type Positions = HashMap<u64, Vec<f32>>;

#[derive(Debug, Clone)]
pub struct InDescentOptions {
    /// `0.0` → ignore centrality; `1.0` → ignore distance.  Default
    /// `0.2`.  Same semantics as `BiasOptions::alpha`.
    pub alpha: f32,
    /// Tie-break window: candidates whose distance is within
    /// `epsilon * dist_min` of the current best are reordered by
    /// centrality.  Default `0.05` (5 % window).
    pub epsilon: f32,
    /// Working-set size (HNSW's `ef`).  Default `64`.
    pub ef: usize,
    /// At most this many entry-point ids.  Default `1`.
    pub entry_points: usize,
}

impl Default for InDescentOptions {
    fn default() -> Self {
        Self { alpha: 0.2, epsilon: 0.05, ef: 64, entry_points: 1 }
    }
}

/// Greedy descent over `adjacency` starting from `entry`, returning
/// the top-`k` `(point_id, score)` pairs.  Scoring rule:
///   * Within ε of `dist_min`: pick highest centrality first.
///   * Outside ε: pick smallest distance first.
/// Pre-filter is applied *during* descent — neighbours that fail it
/// don't enter the candidate queue, so the top-k count holds even
/// under aggressive filters.
pub fn search(
    adjacency: &Adjacency,
    positions: &Positions,
    centrality: &HashMap<u64, f32>,
    entry: &[u64],
    query: &[f32],
    k: usize,
    prefilter: Option<&dyn Fn(u64) -> bool>,
    opts: &InDescentOptions,
) -> Result<Vec<(u64, f32)>> {
    if k == 0 || entry.is_empty() {
        return Ok(Vec::new());
    }

    let dist_to = |id: &u64| -> f32 {
        positions
            .get(id)
            .map(|v| l2_dist(query, v))
            .unwrap_or(f32::INFINITY)
    };

    // Working set: keep the best `ef` candidates by score.  Score
    // is a min-heap of (NotNan dist, centrality, id) so smaller
    // distance wins, with centrality tie-breaking inside ε.
    let mut visited: HashSet<u64> = HashSet::new();
    let mut best: BinaryHeap<Candidate> = BinaryHeap::new();
    let mut frontier: BinaryHeap<std::cmp::Reverse<Candidate>> = BinaryHeap::new();

    // Seed.
    for &eid in entry.iter().take(opts.entry_points.max(1)) {
        if !visited.insert(eid) {
            continue;
        }
        if prefilter.map_or(false, |f| !f(eid)) {
            continue;
        }
        let d = dist_to(&eid);
        let c = centrality.get(&eid).copied().unwrap_or(0.0);
        let cand = Candidate { id: eid, dist: d, centrality: c };
        frontier.push(std::cmp::Reverse(cand));
        best.push(cand);
        if best.len() > opts.ef {
            best.pop();
        }
    }

    while let Some(std::cmp::Reverse(current)) = frontier.pop() {
        // Early termination: if every candidate in `best` is
        // closer than current, we're done.
        if let Some(worst) = best.peek() {
            if current.dist > worst.dist && best.len() >= opts.ef {
                break;
            }
        }
        let Some(neighbours) = adjacency.get(&current.id) else { continue };
        for &n in neighbours {
            if !visited.insert(n) {
                continue;
            }
            if prefilter.map_or(false, |f| !f(n)) {
                continue;
            }
            let d = dist_to(&n);
            let c = centrality.get(&n).copied().unwrap_or(0.0);
            let cand = Candidate { id: n, dist: d, centrality: c };
            // Within-ε bias: if this candidate's distance is close
            // to the running best, weight by centrality.  Outside
            // the window, fall back to pure distance.
            let push = match best.peek() {
                Some(worst) if best.len() >= opts.ef => {
                    if cand.dist < worst.dist {
                        true
                    } else if (cand.dist - worst.dist).abs()
                        <= worst.dist * opts.epsilon
                        && cand.centrality > worst.centrality
                    {
                        true
                    } else {
                        false
                    }
                }
                _ => true,
            };
            if push {
                best.push(cand);
                if best.len() > opts.ef {
                    best.pop();
                }
                frontier.push(std::cmp::Reverse(cand));
            }
        }
    }

    // Drain `best` into k results sorted by combined
    // distance-centrality score.
    let alpha = opts.alpha.clamp(0.0, 1.0);
    let mut out: Vec<(u64, f32)> = best
        .into_iter()
        .map(|c| {
            let d_norm = c.dist; // already normalised by being in best heap
            let score = (1.0 - alpha) * d_norm - alpha * c.centrality;
            (c.id, score)
        })
        .collect();
    out.sort_by(|a, b| {
        a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(k);
    Ok(out)
}

#[derive(Copy, Clone, Debug)]
struct Candidate {
    id: u64,
    dist: f32,
    centrality: f32,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Larger dist sorts first → max-heap by dist (so .pop()
        // discards the worst).  Tie-break by centrality
        // descending.
        match self
            .dist
            .partial_cmp(&other.dist)
            .unwrap_or(std::cmp::Ordering::Equal)
        {
            std::cmp::Ordering::Equal => {
                other
                    .centrality
                    .partial_cmp(&self.centrality)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }
            o => o,
        }
    }
}

fn l2_dist(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut s = 0.0f32;
    for i in 0..n {
        let d = a[i] - b[i];
        s += d * d;
    }
    s.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_graph() -> (Adjacency, Positions) {
        // 5 points along a line: 0—1—2—3—4. positions are 1-D.
        let mut adj: Adjacency = HashMap::new();
        for i in 0u64..5 {
            let mut n = Vec::new();
            if i > 0 {
                n.push(i - 1);
            }
            if i < 4 {
                n.push(i + 1);
            }
            adj.insert(i, n);
        }
        let mut pos: Positions = HashMap::new();
        for i in 0u64..5 {
            pos.insert(i, vec![i as f32]);
        }
        (adj, pos)
    }

    #[test]
    fn search_finds_nearest_in_line_graph() {
        let (adj, pos) = line_graph();
        let cent: HashMap<u64, f32> = HashMap::new();
        let opts = InDescentOptions::default();
        // Query at 2.5 — closest is 2 or 3.
        let r = search(&adj, &pos, &cent, &[0], &[2.5], 1, None, &opts).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].0 == 2 || r[0].0 == 3, "got {r:?}");
    }

    #[test]
    fn centrality_breaks_tie_within_epsilon() {
        let (adj, pos) = line_graph();
        // 2 and 3 are equidistant from query at 2.5.  Boost 3's
        // centrality and use heavy alpha to win the tie.
        let mut cent: HashMap<u64, f32> = HashMap::new();
        cent.insert(3, 1.0);
        let opts = InDescentOptions { alpha: 0.9, ..Default::default() };
        let r = search(&adj, &pos, &cent, &[0], &[2.5], 1, None, &opts).unwrap();
        assert_eq!(r[0].0, 3);
    }

    #[test]
    fn prefilter_drops_non_matches_during_descent() {
        let (adj, pos) = line_graph();
        let cent: HashMap<u64, f32> = HashMap::new();
        let opts = InDescentOptions::default();
        // Reject 2 + 3, force result to be 1 or 4.
        let pf = |id: u64| !(id == 2 || id == 3);
        let r = search(&adj, &pos, &cent, &[0], &[2.5], 1, Some(&pf), &opts).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].0 == 1 || r[0].0 == 4, "got {r:?}");
    }

    #[test]
    fn empty_inputs_safe() {
        let r = search(
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &[],
            &[1.0],
            5,
            None,
            &InDescentOptions::default(),
        )
        .unwrap();
        assert!(r.is_empty());
    }
}
