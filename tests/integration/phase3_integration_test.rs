//! Phase 3 Integration Tests
//!
//! Comprehensive tests for Phase 3 features:
//! - Product Quantization for vector compression
//! - Database branching SQL syntax
//! - Time-travel queries
//! - Materialized views with auto-refresh

use heliosdb_nano::*;

#[test]
fn test_product_quantization_end_to_end() {
    use heliosdb_nano::vector::quantization::{ProductQuantizer, ProductQuantizerConfig};
    use heliosdb_nano::vector::Vector;

    // Configuration for 128-dimensional vectors
    let config = ProductQuantizerConfig::default_for_dimension(128).unwrap();

    // Generate training data
    let training_vectors: Vec<Vector> = (0..1000)
        .map(|i| {
            (0..128)
                .map(|j| ((i * 128 + j) as f32).sin() * 0.5)
                .collect()
        })
        .collect();

    // Train Product Quantizer
    let pq = ProductQuantizer::train(config.clone(), &training_vectors).unwrap();

    // Verify compression ratio
    assert_eq!(pq.compression_ratio(), 64.0); // 128*4 / 8 = 64x

    // Test encoding
    let test_vector: Vector = (0..128).map(|i| (i as f32).sin()).collect();
    let quantized = pq.encode(&test_vector).unwrap();

    // Verify quantized size
    assert_eq!(quantized.memory_size(), config.num_subquantizers);

    // Test decoding
    let reconstructed = pq.decode(&quantized).unwrap();
    assert_eq!(reconstructed.len(), 128);

    // Test distance computation
    let query: Vector = (0..128).map(|i| (i as f32).cos()).collect();
    let distance_table = pq.precompute_distance_table(&query).unwrap();
    let distance = pq.compute_distance_with_table(&distance_table, &quantized).unwrap();

    assert!(distance >= 0.0);
    assert!(distance.is_finite());
}

#[test]
fn test_quantized_hnsw_integration() {
    use heliosdb_nano::vector::{QuantizedHnswIndex, QuantizedHnswConfig, Vector};

    // Create config for 128D vectors
    let config = QuantizedHnswConfig::default_for_dimension(128).unwrap();

    // Generate training data
    let training_vectors: Vec<Vector> = (0..1000)
        .map(|i| {
            (0..128)
                .map(|j| ((i * 128 + j) as f32).sin())
                .collect()
        })
        .collect();

    // Train and create index
    let index = QuantizedHnswIndex::train(config, &training_vectors).unwrap();

    // Insert vectors
    for (i, vector) in training_vectors.iter().take(100).enumerate() {
        index.insert(i as u64, vector).unwrap();
    }

    assert_eq!(index.len(), 100);

    // Search for nearest neighbors
    let query = &training_vectors[0];
    let results = index.search(query, 10).unwrap();

    // Verify results
    assert!(!results.is_empty());
    assert!(results.len() <= 10);

    // First result should be the query itself
    assert_eq!(results[0].0, 0);
    assert!(results[0].1 < 0.01); // Very small distance

    // Check memory stats
    let stats = index.memory_stats();
    assert!(stats.compression_ratio > 1.0);
    println!("Memory stats: {}", stats.format());
}

#[test]
fn test_branching_sql_parsing() {
    use heliosdb_nano::sql::phase3::BranchingParser;

    // Test CREATE BRANCH
    let plan = BranchingParser::parse_create_branch(
        "test_branch".to_string(),
        Some("main".to_string()),
        "TIMESTAMP '2025-11-18 00:00:00'",
        None,
    );
    assert!(plan.is_ok());

    // Test DROP BRANCH
    let plan = BranchingParser::parse_drop_branch("test_branch".to_string(), true);
    assert!(plan.is_ok());

    // Test MERGE BRANCH
    let plan = BranchingParser::parse_merge_branch(
        "source".to_string(),
        "target".to_string(),
        Some("conflict_resolution='branch_wins'"),
    );
    assert!(plan.is_ok());
}

#[test]
fn test_materialized_view_parsing() {
    use heliosdb_nano::sql::phase3::MaterializedViewParser;

    // Test MV options parsing
    let options_str = "auto_refresh=true, max_cpu_percent=15, threshold_dml_rate=100";
    let options = MaterializedViewParser::parse_mv_options(options_str);
    assert!(options.is_ok());

    let opts = options.unwrap();
    assert_eq!(opts.len(), 3);
}

#[test]
fn test_pq_accuracy() {
    use heliosdb_nano::vector::quantization::{ProductQuantizer, ProductQuantizerConfig};
    use heliosdb_nano::vector::Vector;

    // Test that PQ maintains high accuracy for similar vectors
    let config = ProductQuantizerConfig {
        num_subquantizers: 8,
        num_centroids: 256,
        dimension: 64,
        training_iterations: 25,
        min_training_samples: 100,
    };

    // Generate clustered training data
    let mut training_vectors = Vec::new();

    // Cluster 1: vectors around [0.5, 0.5, ...]
    for _ in 0..200 {
        let vec: Vector = (0..64).map(|_| 0.5 + (rand::random::<f32>() - 0.5) * 0.1).collect();
        training_vectors.push(vec);
    }

    // Cluster 2: vectors around [-0.5, -0.5, ...]
    for _ in 0..200 {
        let vec: Vector = (0..64).map(|_| -0.5 + (rand::random::<f32>() - 0.5) * 0.1).collect();
        training_vectors.push(vec);
    }

    let pq = ProductQuantizer::train(config, &training_vectors).unwrap();

    // Test: query from cluster 1 should have small distance to cluster 1 vectors
    let query1: Vector = (0..64).map(|_| 0.5).collect();
    let vec1: Vector = (0..64).map(|_| 0.5).collect();
    let vec2: Vector = (0..64).map(|_| -0.5).collect();

    let qv1 = pq.encode(&vec1).unwrap();
    let qv2 = pq.encode(&vec2).unwrap();

    let dist1 = pq.compute_distance(&query1, &qv1).unwrap();
    let dist2 = pq.compute_distance(&query1, &qv2).unwrap();

    // Distance to same cluster should be much smaller
    assert!(dist1 < dist2);
    println!("Distance to same cluster: {:.4}", dist1);
    println!("Distance to different cluster: {:.4}", dist2);
}

#[test]
fn test_pq_batch_performance() {
    use heliosdb_nano::vector::quantization::{ProductQuantizer, ProductQuantizerConfig};
    use heliosdb_nano::vector::Vector;
    use std::time::Instant;

    let config = ProductQuantizerConfig::default_for_dimension(768).unwrap();

    // Generate training data
    let training_vectors: Vec<Vector> = (0..10000)
        .map(|i| {
            (0..768)
                .map(|j| ((i * 768 + j) as f32).sin() * 0.5)
                .collect()
        })
        .collect();

    println!("Training PQ on 10,000 vectors...");
    let start = Instant::now();
    let pq = ProductQuantizer::train(config, &training_vectors).unwrap();
    let training_time = start.elapsed();
    println!("Training time: {:?}", training_time);

    // Encode all vectors
    println!("Encoding 10,000 vectors...");
    let start = Instant::now();
    let quantized: Vec<_> = training_vectors
        .iter()
        .map(|v| pq.encode(v).unwrap())
        .collect();
    let encoding_time = start.elapsed();
    println!("Encoding time: {:?}", encoding_time);
    println!("Encoding rate: {:.0} vectors/sec", 10000.0 / encoding_time.as_secs_f64());

    // Search performance test
    let query = &training_vectors[0];
    println!("Searching with precomputed table...");
    let start = Instant::now();
    let distance_table = pq.precompute_distance_table(query).unwrap();
    let precompute_time = start.elapsed();

    let start = Instant::now();
    let _distances: Vec<_> = quantized
        .iter()
        .map(|qv| pq.compute_distance_with_table(&distance_table, qv).unwrap())
        .collect();
    let search_time = start.elapsed();

    println!("Precompute time: {:?}", precompute_time);
    println!("Search time: {:?}", search_time);
    println!("Search rate: {:.0} vectors/sec", 10000.0 / search_time.as_secs_f64());
}
