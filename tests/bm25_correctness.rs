//! BM25 correctness against a known small corpus.
//!
//! HelixDB-inspired (idea 2).

use heliosdb_nano::search::Bm25Index;

fn build() -> Bm25Index {
    let idx = Bm25Index::new();
    idx.add_document(1, "the quick brown fox jumps over the lazy dog");
    idx.add_document(2, "the lazy dog sleeps under the warm sun");
    idx.add_document(3, "stock market closes higher on tuesday afternoon");
    idx.add_document(4, "tuesday afternoon is a good time for a long walk");
    idx
}

#[test]
fn known_query_hits_known_doc() {
    let idx = build();
    let res = idx.score("market closes", None);
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].doc_id, 3);
}

#[test]
fn shared_terms_rank_above_disjoint_docs() {
    let idx = build();
    let res = idx.score("dog", None);
    assert_eq!(res.len(), 2);
    let ids: Vec<_> = res.iter().map(|s| s.doc_id).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
}

#[test]
fn limit_truncates_correctly() {
    let idx = build();
    let res = idx.score("the dog tuesday", Some(2));
    assert_eq!(res.len(), 2);
}

#[test]
fn empty_index_yields_no_results() {
    let idx = Bm25Index::new();
    let res = idx.score("anything", None);
    assert!(res.is_empty());
}

#[test]
fn unseen_query_term_yields_no_results() {
    let idx = build();
    assert!(idx.score("zebra penguin antarctica", None).is_empty());
}

#[test]
fn matches_returns_true_for_present_term() {
    let idx = build();
    assert!(idx.matches(1, "fox"));
    assert!(!idx.matches(1, "market"));
}
