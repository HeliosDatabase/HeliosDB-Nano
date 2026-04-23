//! Known fusion outputs for RRF and MMR.
//!
//! RAG-native (idea 2).

use heliosdb_nano::search::{mmr_rerank, rrf_fuse, RrfParams};

#[test]
fn rrf_known_pair() {
    let lists = vec![vec![1u64, 2, 3], vec![3u64, 1, 2]];
    let fused = rrf_fuse(&lists, RrfParams { k: 60.0 });
    // Doc 1: 1/61 + 1/62, doc 3: 1/63 + 1/61, doc 2: 1/62 + 1/63
    // Numerically:
    // 1: 0.01639 + 0.01613 = 0.03252
    // 3: 0.01587 + 0.01639 = 0.03226
    // 2: 0.01613 + 0.01587 = 0.03200
    let ids: Vec<u64> = fused.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids[0], 1);
    assert_eq!(ids[1], 3);
    assert_eq!(ids[2], 2);
}

#[test]
fn rrf_uniform_documents() {
    // Same doc top of every list -> always ranks first.
    let lists = vec![vec![42u64], vec![42u64], vec![42u64]];
    let fused = rrf_fuse(&lists, RrfParams::default());
    assert_eq!(fused[0].0, 42);
    let expected = 3.0 / 61.0;
    assert!((fused[0].1 - expected).abs() < 1e-9);
}

#[test]
fn mmr_lambda_one_matches_relevance_order() {
    let candidates = vec![
        (1u64, 0.95, vec![1.0, 0.0]),
        (2, 0.80, vec![0.9, 0.1]),
        (3, 0.40, vec![0.0, 1.0]),
    ];
    let res = mmr_rerank(candidates, 1.0, 3);
    assert_eq!(res[0].0, 1);
    assert_eq!(res[1].0, 2);
    assert_eq!(res[2].0, 3);
}

#[test]
fn mmr_low_lambda_promotes_diversity() {
    let candidates = vec![
        (1u64, 0.95, vec![1.0, 0.0]),
        (2, 0.94, vec![1.0, 0.0]), // near-duplicate of doc 1
        (3, 0.50, vec![0.0, 1.0]), // orthogonal
    ];
    let res = mmr_rerank(candidates, 0.2, 3);
    assert_eq!(res[0].0, 1);
    // Diverse pick wins second slot.
    assert_eq!(res[1].0, 3);
    assert_eq!(res[2].0, 2);
}

#[test]
fn mmr_top_k_truncation() {
    let candidates = vec![
        (1u64, 0.9, vec![1.0]),
        (2, 0.8, vec![1.0]),
        (3, 0.7, vec![1.0]),
        (4, 0.6, vec![1.0]),
    ];
    let res = mmr_rerank(candidates, 0.5, 2);
    assert_eq!(res.len(), 2);
}
