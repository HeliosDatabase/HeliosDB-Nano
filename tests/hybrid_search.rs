//! End-to-end hybrid search assertions.
//!
//! HelixDB-inspired (idea 2).

use heliosdb_nano::search::hybrid::bm25_hits;
use heliosdb_nano::search::{hybrid_search, Bm25Index, FusionMethod, ScoredHit};

#[test]
fn bm25_only_path_returns_text_match() {
    let idx = Bm25Index::new();
    idx.add_document(1, "machine learning embeddings");
    idx.add_document(2, "ancient roman aqueducts");
    let bm25 = bm25_hits(&idx, "embeddings", None);
    let res = hybrid_search(&bm25, &[], FusionMethod::Rrf, 0.5, 5);
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].doc_id, 1);
}

#[test]
fn vector_only_path_returns_vector_hits() {
    let vec_hits = vec![
        ScoredHit {
            doc_id: 7,
            score: 0.9,
            vector: None,
        },
        ScoredHit {
            doc_id: 8,
            score: 0.7,
            vector: None,
        },
    ];
    let res = hybrid_search(&[], &vec_hits, FusionMethod::Rrf, 0.5, 5);
    assert_eq!(res.len(), 2);
    assert_eq!(res[0].doc_id, 7);
}

#[test]
fn hybrid_promotes_documents_in_both_lists() {
    let idx = Bm25Index::new();
    idx.add_document(1, "rust async");
    idx.add_document(2, "python pandas");
    idx.add_document(3, "rust embeddings vector search");
    let bm25 = bm25_hits(&idx, "rust", None);
    let vec_hits = vec![
        ScoredHit {
            doc_id: 3,
            score: 0.95,
            vector: None,
        },
        ScoredHit {
            doc_id: 4,
            score: 0.8,
            vector: None,
        },
    ];
    let res = hybrid_search(&bm25, &vec_hits, FusionMethod::Rrf, 0.5, 10);
    // Doc 3 is in both lists -> top result.
    assert_eq!(res[0].doc_id, 3);
}

#[test]
fn linear_fusion_with_zero_lambda_is_pure_bm25() {
    let idx = Bm25Index::new();
    idx.add_document(1, "alpha");
    idx.add_document(2, "beta");
    let bm25 = bm25_hits(&idx, "alpha", None);
    let vec_hits = vec![ScoredHit {
        doc_id: 2,
        score: 1.0,
        vector: None,
    }];
    let res = hybrid_search(&bm25, &vec_hits, FusionMethod::Linear, 0.0, 5);
    assert_eq!(res[0].doc_id, 1);
}

#[test]
fn linear_fusion_with_full_lambda_is_pure_vector() {
    let idx = Bm25Index::new();
    idx.add_document(1, "alpha");
    let bm25 = bm25_hits(&idx, "alpha", None);
    let vec_hits = vec![ScoredHit {
        doc_id: 99,
        score: 1.0,
        vector: None,
    }];
    let res = hybrid_search(&bm25, &vec_hits, FusionMethod::Linear, 1.0, 5);
    assert_eq!(res[0].doc_id, 99);
}
