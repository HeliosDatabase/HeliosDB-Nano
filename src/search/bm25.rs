//! In-memory BM25 inverted index.
//!
//! HelixDB-inspired (idea 2).
//!
//! Implements Okapi BM25 with the canonical `(k1, b)` parameters. The
//! index is in-memory and reusable: callers `add_document` once and
//! then `score` many queries against it.
//!
//! ## Algorithm
//!
//! For each term `t` in the query, the contribution to a document `d`'s
//! score is:
//!
//! ```text
//! IDF(t) * (tf(t,d) * (k1 + 1)) / (tf(t,d) + k1 * (1 - b + b * dl/avgdl))
//! ```
//!
//! where `IDF(t) = ln((N - df(t) + 0.5) / (df(t) + 0.5) + 1)` (the
//! "BM25+1" smoothing, identical to Lucene's default).

use std::collections::HashMap;

use parking_lot::RwLock;

use super::tokenizer::tokenize;

/// BM25 hyperparameters.
#[derive(Debug, Clone, Copy)]
pub struct Bm25Params {
    /// Term-frequency saturation knob. Lucene default: `1.2`.
    pub k1: f64,
    /// Document-length normalisation knob. Lucene default: `0.75`.
    pub b: f64,
}

impl Default for Bm25Params {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

/// Result of a BM25 query for a single document.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bm25Score {
    pub doc_id: u64,
    pub score: f64,
}

#[derive(Default)]
struct IndexState {
    /// `term -> [(doc_id, term_frequency)]`
    postings: HashMap<String, Vec<(u64, u32)>>,
    /// `doc_id -> document length (token count)`
    doc_len: HashMap<u64, u32>,
    /// Total tokens across all docs (for avg).
    total_tokens: u64,
}

/// Thread-safe BM25 inverted index.
///
/// Cheap to clone-by-Arc; expensive to clone-by-value (deep copy of
/// the postings map).
pub struct Bm25Index {
    params: Bm25Params,
    state: RwLock<IndexState>,
}

impl Bm25Index {
    /// Create a new index with default parameters.
    #[must_use]
    pub fn new() -> Self {
        Self::with_params(Bm25Params::default())
    }

    /// Create a new index with custom hyperparameters.
    #[must_use]
    pub fn with_params(params: Bm25Params) -> Self {
        Self {
            params,
            state: RwLock::new(IndexState::default()),
        }
    }

    /// Index `text` under `doc_id`. Replacing an existing doc requires
    /// the caller to first call [`Self::remove_document`].
    pub fn add_document(&self, doc_id: u64, text: &str) {
        let tokens = tokenize(text);
        let mut tf: HashMap<String, u32> = HashMap::new();
        for tok in &tokens {
            *tf.entry(tok.clone()).or_insert(0) += 1;
        }
        let len = tokens.len() as u32;
        let mut s = self.state.write();
        for (term, count) in tf {
            s.postings.entry(term).or_default().push((doc_id, count));
        }
        s.doc_len.insert(doc_id, len);
        s.total_tokens += u64::from(len);
    }

    /// Remove a document from the index. Returns `true` if it existed.
    pub fn remove_document(&self, doc_id: u64) -> bool {
        let mut s = self.state.write();
        let Some(len) = s.doc_len.remove(&doc_id) else {
            return false;
        };
        s.total_tokens = s.total_tokens.saturating_sub(u64::from(len));
        // Strip the doc from every posting list. Could be slow for large
        // vocabularies; acceptable for now since this is in-memory and
        // deletion is uncommon in typical full-text workloads.
        for postings in s.postings.values_mut() {
            postings.retain(|(d, _)| *d != doc_id);
        }
        // Drop empty posting lists so IDF doesn't see phantom terms.
        s.postings.retain(|_, p| !p.is_empty());
        true
    }

    /// Number of documents currently indexed.
    #[must_use]
    pub fn doc_count(&self) -> usize {
        self.state.read().doc_len.len()
    }

    /// Average document length (in tokens). Returns `0.0` for an empty index.
    #[must_use]
    pub fn average_doc_length(&self) -> f64 {
        let s = self.state.read();
        if s.doc_len.is_empty() {
            0.0
        } else {
            s.total_tokens as f64 / s.doc_len.len() as f64
        }
    }

    /// Score every document that contains at least one query term and
    /// return them sorted by descending score (top-`limit` if requested).
    pub fn score(&self, query: &str, limit: Option<usize>) -> Vec<Bm25Score> {
        let q_tokens = tokenize(query);
        if q_tokens.is_empty() {
            return Vec::new();
        }
        let s = self.state.read();
        let n = s.doc_len.len() as f64;
        if n == 0.0 {
            return Vec::new();
        }
        let avgdl = s.total_tokens as f64 / n;
        let Bm25Params { k1, b } = self.params;

        let mut scores: HashMap<u64, f64> = HashMap::new();
        // De-duplicate query tokens -- BM25 sums per-term contributions
        // but multiple occurrences of the same term in the query are
        // typically counted once for the IDF-weighted score (consistent
        // with Lucene's default).
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::with_capacity(q_tokens.len());
        for q in &q_tokens {
            if !seen.insert(q.as_str()) {
                continue;
            }
            let Some(postings) = s.postings.get(q) else {
                continue;
            };
            let df = postings.len() as f64;
            // Lucene-style IDF with +1 smoothing -- always non-negative.
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for &(doc_id, tf) in postings {
                let dl = f64::from(*s.doc_len.get(&doc_id).unwrap_or(&0));
                let tf_f = f64::from(tf);
                let denom = tf_f + k1 * (1.0 - b + b * (dl / avgdl).max(0.0));
                let contribution = idf * (tf_f * (k1 + 1.0)) / denom.max(f64::MIN_POSITIVE);
                *scores.entry(doc_id).or_insert(0.0) += contribution;
            }
        }
        let mut out: Vec<Bm25Score> = scores
            .into_iter()
            .map(|(doc_id, score)| Bm25Score { doc_id, score })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Stable tie-break on doc_id so callers get deterministic ordering.
                .then(a.doc_id.cmp(&b.doc_id))
        });
        if let Some(k) = limit {
            out.truncate(k);
        }
        out
    }

    /// Convenience: returns `true` iff the document has *any* non-zero
    /// BM25 score for the query (i.e. shares at least one term).
    pub fn matches(&self, doc_id: u64, query: &str) -> bool {
        let q_tokens = tokenize(query);
        if q_tokens.is_empty() {
            return false;
        }
        let s = self.state.read();
        for q in &q_tokens {
            if let Some(postings) = s.postings.get(q) {
                if postings.iter().any(|(d, _)| *d == doc_id) {
                    return true;
                }
            }
        }
        false
    }
}

impl Default for Bm25Index {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Bm25Index {
        let idx = Bm25Index::new();
        idx.add_document(1, "the quick brown fox jumps over the lazy dog");
        idx.add_document(2, "a fast brown fox leaps over a sleepy dog");
        idx.add_document(3, "the lazy cat sat on the mat");
        idx.add_document(4, "stock market closes higher on tuesday");
        idx
    }

    #[test]
    fn empty_query_returns_empty() {
        let idx = corpus();
        assert!(idx.score("", None).is_empty());
        assert!(idx.score("   ", None).is_empty());
    }

    #[test]
    fn single_term_picks_relevant_doc() {
        let idx = corpus();
        let res = idx.score("market", None);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].doc_id, 4);
        assert!(res[0].score > 0.0);
    }

    #[test]
    fn multi_term_ranks_dog_docs_first() {
        let idx = corpus();
        let res = idx.score("brown fox dog", None);
        // Docs 1 and 2 contain all three terms, doc 3 contains none -> excluded.
        assert!(res.len() >= 2);
        let top_ids: Vec<_> = res.iter().take(2).map(|s| s.doc_id).collect();
        assert!(top_ids.contains(&1));
        assert!(top_ids.contains(&2));
        assert!(!res.iter().any(|s| s.doc_id == 3));
        assert!(!res.iter().any(|s| s.doc_id == 4));
    }

    #[test]
    fn limit_caps_result_count() {
        let idx = corpus();
        let res = idx.score("the dog cat fox market", Some(2));
        assert!(res.len() <= 2);
    }

    #[test]
    fn matches_returns_true_for_known_doc() {
        let idx = corpus();
        assert!(idx.matches(1, "fox"));
        assert!(!idx.matches(1, "elephant"));
        assert!(!idx.matches(99, "fox"));
    }

    #[test]
    fn doc_count_and_average_length_are_correct() {
        let idx = corpus();
        assert_eq!(idx.doc_count(), 4);
        let avg = idx.average_doc_length();
        // Lengths: 9 + 9 + 7 + 6 = 31, /4 = 7.75
        assert!((avg - 7.75).abs() < 0.01);
    }

    #[test]
    fn remove_document_drops_postings_and_score() {
        let idx = corpus();
        assert!(idx.remove_document(4));
        assert_eq!(idx.doc_count(), 3);
        let res = idx.score("market", None);
        assert!(res.is_empty(), "doc 4 should be gone");
        assert!(!idx.remove_document(4));
    }

    #[test]
    fn duplicate_query_tokens_dont_double_count() {
        let idx = corpus();
        let single = idx.score("fox", None);
        let dupe = idx.score("fox fox fox", None);
        // De-duplication means scores should be identical.
        assert_eq!(single.len(), dupe.len());
        for (s, d) in single.iter().zip(dupe.iter()) {
            assert_eq!(s.doc_id, d.doc_id);
            assert!((s.score - d.score).abs() < 1e-9);
        }
    }

    #[test]
    fn idf_is_non_negative_for_common_term() {
        // "the" appears in many docs; with the +1 smoothing IDF stays >= 0.
        let idx = corpus();
        let res = idx.score("the", None);
        for r in &res {
            assert!(r.score >= 0.0);
        }
    }
}
