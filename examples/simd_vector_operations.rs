//! SIMD Vector Operations Example
//!
//! Demonstrates the usage of SIMD-accelerated vector operations and
//! shows how to check for available CPU features.

use heliosdb_nano::vector::simd;

fn main() {
    println!("=== SIMD Vector Operations Demo ===\n");

    // Check CPU features
    let features = simd::cpu_features();
    println!("Detected CPU features: {}", features.description());
    println!("  - AVX2:     {}", features.avx2);
    println!("  - AVX-512:  {}", features.avx512f);
    println!("  - SSE4.2:   {}", features.sse42);
    println!();

    // Create sample vectors
    let vec_a: Vec<f32> = (0..768).map(|i| (i as f32).sin()).collect();
    let vec_b: Vec<f32> = (0..768).map(|i| (i as f32).cos()).collect();

    println!("Vector dimension: {}", vec_a.len());
    println!();

    // Measure L2 distance
    let start = std::time::Instant::now();
    let l2_dist = simd::l2_distance(&vec_a, &vec_b);
    let l2_time = start.elapsed();
    println!("L2 Distance:      {:.6} (computed in {:?})", l2_dist, l2_time);

    // Measure L2 distance squared (faster - no sqrt)
    let start = std::time::Instant::now();
    let l2_sq_dist = simd::l2_distance_squared(&vec_a, &vec_b);
    let l2_sq_time = start.elapsed();
    println!("L2² Distance:     {:.6} (computed in {:?})", l2_sq_dist, l2_sq_time);

    // Measure cosine distance
    let start = std::time::Instant::now();
    let cos_dist = simd::cosine_distance(&vec_a, &vec_b);
    let cos_time = start.elapsed();
    println!("Cosine Distance:  {:.6} (computed in {:?})", cos_dist, cos_time);

    // Measure dot product
    let start = std::time::Instant::now();
    let dot = simd::dot_product(&vec_a, &vec_b);
    let dot_time = start.elapsed();
    println!("Dot Product:      {:.6} (computed in {:?})", dot, dot_time);
    println!();

    // Batch processing example
    println!("=== Batch Processing Example ===");
    let query = vec_a.clone();
    let database: Vec<Vec<f32>> = (0..100)
        .map(|i| (0..768).map(|j| ((i * j) as f32).sin()).collect())
        .collect();

    let start = std::time::Instant::now();
    let mut distances = Vec::with_capacity(database.len());
    for vec in &database {
        distances.push(simd::cosine_distance(&query, vec));
    }
    let batch_time = start.elapsed();

    println!("Computed {} distances in {:?}", distances.len(), batch_time);
    println!("Average time per distance: {:?}", batch_time / distances.len() as u32);

    // Find nearest neighbor
    let (min_idx, min_dist) = distances
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
        .unwrap();

    println!("Nearest neighbor: index {} with distance {:.6}", min_idx, min_dist);
    println!();

    // Different vector sizes comparison
    println!("=== Performance across dimensions ===");
    for dim in [64, 128, 256, 512, 768, 1024, 1536] {
        let a: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.1).collect();
        let b: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.2).collect();

        let start = std::time::Instant::now();
        let mut sum = 0.0f32;
        for _ in 0..1000 {
            sum += simd::cosine_distance(&a, &b);
        }
        let elapsed = start.elapsed();

        println!(
            "Dimension {:4}: {:8.2} µs/op (1000 iterations, sum: {:.6})",
            dim,
            elapsed.as_micros() as f64 / 1000.0,
            sum
        );
    }
    println!();

    // Product Quantization example
    println!("=== Product Quantization Distance ===");

    let num_subquantizers = 16;
    let codebook_size = 256;
    let subvector_dim = 48; // 768 / 16

    // Create sample codebooks
    let codebooks: Vec<Vec<Vec<f32>>> = (0..num_subquantizers)
        .map(|m| {
            (0..codebook_size)
                .map(|k| {
                    (0..subvector_dim)
                        .map(|d| ((m * codebook_size + k + d) as f32) * 0.001)
                        .collect()
                })
                .collect()
        })
        .collect();

    let query_vec: Vec<f32> = (0..768).map(|i| (i as f32) * 0.01).collect();

    // Encode a vector
    let start = std::time::Instant::now();
    let codes = simd::quantization::encode_vector_simd(
        &query_vec,
        &codebooks,
        num_subquantizers,
        codebook_size,
        subvector_dim,
    );
    let encode_time = start.elapsed();

    println!("Encoded {} dimensional vector into {} codes in {:?}",
             query_vec.len(), codes.len(), encode_time);

    // Compute distance table
    let start = std::time::Instant::now();
    let distance_table = simd::quantization::compute_distance_table(
        &query_vec,
        &codebooks,
        num_subquantizers,
        codebook_size,
        subvector_dim,
    );
    let table_time = start.elapsed();

    println!("Computed distance table in {:?}", table_time);

    // Compute asymmetric distance
    let start = std::time::Instant::now();
    let pq_dist = simd::quantization::asymmetric_distance_simd(
        &query_vec,
        &codes,
        &distance_table,
        num_subquantizers,
        codebook_size,
    );
    let dist_time = start.elapsed();

    println!("Computed PQ distance: {:.6} in {:?}", pq_dist, dist_time);
    println!();

    println!("=== Summary ===");
    if features.avx2 {
        println!("✓ SIMD acceleration is ACTIVE (AVX2)");
        println!("  Expected speedup: 2-6x depending on operation");
    } else if features.sse42 {
        println!("⚠ Limited SIMD support (SSE4.2 only)");
        println!("  Consider upgrading CPU for AVX2 support");
    } else {
        println!("⚠ No SIMD acceleration available");
        println!("  Using portable scalar fallback");
    }
}
