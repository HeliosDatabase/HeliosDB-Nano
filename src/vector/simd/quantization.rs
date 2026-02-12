//! SIMD-accelerated Product Quantization operations
//!
//! Provides optimized implementations for PQ encoding and distance computation
//! using SIMD instructions when available.

#![allow(unused_variables)]

use super::cpu_features;

/// Compute asymmetric distance for Product Quantization using SIMD when available
///
/// Asymmetric distance: compare a full precision query vector with a quantized database vector.
/// This is the main operation used during PQ search.
///
/// # Arguments
/// * `query` - Full precision query vector (dimension D)
/// * `codes` - Quantized vector codes (M sub-quantizers)
/// * `distance_table` - Pre-computed distances [M x K] where K is codebook size
/// * `num_subquantizers` - Number of sub-quantizers (M)
/// * `codebook_size` - Size of each codebook (K, typically 256)
///
/// # Performance
/// - Expected 2-3x speedup with AVX2 for typical PQ configurations
pub fn asymmetric_distance_simd(
    query: &[f32],
    codes: &[u8],
    distance_table: &[f32],
    num_subquantizers: usize,
    codebook_size: usize,
) -> f32 {
    assert_eq!(codes.len(), num_subquantizers, "Codes length must match number of sub-quantizers");
    assert_eq!(distance_table.len(), num_subquantizers * codebook_size,
               "Distance table size mismatch");

    #[cfg(target_arch = "x86_64")]
    {
        let features = cpu_features();
        if features.avx2 && num_subquantizers >= 8 {
            return unsafe {
                asymmetric_distance_avx2(codes, distance_table, num_subquantizers, codebook_size)
            };
        }
    }

    // Scalar fallback
    asymmetric_distance_scalar(codes, distance_table, num_subquantizers, codebook_size)
}

/// Compute distance table for a query vector
///
/// Pre-computes distances from the query to all centroids in all sub-quantizers.
/// This table is then used for fast asymmetric distance computation.
///
/// # Arguments
/// * `query` - Query vector
/// * `codebooks` - Codebook centroids [M x K x D/M]
/// * `num_subquantizers` - Number of sub-quantizers (M)
/// * `codebook_size` - Size of each codebook (K)
/// * `subvector_dim` - Dimension of each sub-vector (D/M)
///
/// # Returns
/// Distance table [M x K] flattened
// SAFETY: All indices are bounded by num_subquantizers, codebook_size, and subvector_dim
// which are validated by the caller and used to construct the iteration bounds.
#[allow(clippy::indexing_slicing)]
pub fn compute_distance_table(
    query: &[f32],
    codebooks: &[Vec<Vec<f32>>],
    num_subquantizers: usize,
    codebook_size: usize,
    subvector_dim: usize,
) -> Vec<f32> {
    let mut table = vec![0.0f32; num_subquantizers * codebook_size];

    for m in 0..num_subquantizers {
        let query_offset = m * subvector_dim;
        let query_subvec = &query[query_offset..query_offset + subvector_dim];

        for k in 0..codebook_size {
            if k < codebooks[m].len() {
                let centroid = &codebooks[m][k];
                // Use SIMD-accelerated L2 distance for computing table
                let dist = super::distance::l2_distance_squared(query_subvec, centroid);
                table[m * codebook_size + k] = dist;
            } else {
                table[m * codebook_size + k] = f32::MAX;
            }
        }
    }

    table
}

// ============================================================================
// Scalar implementation
// ============================================================================

// SAFETY: `m` iterates 0..num_subquantizers which matches codes.len() (asserted by caller).
// `table_offset = m * codebook_size + code` is bounded by distance_table.len() (asserted by caller).
#[allow(clippy::indexing_slicing, clippy::needless_range_loop)]
#[inline]
fn asymmetric_distance_scalar(
    codes: &[u8],
    distance_table: &[f32],
    num_subquantizers: usize,
    codebook_size: usize,
) -> f32 {
    let mut sum = 0.0f32;

    for m in 0..num_subquantizers {
        let code = codes[m] as usize;
        let table_offset = m * codebook_size + code;
        sum += distance_table[table_offset];
    }

    sum
}

// ============================================================================
// AVX2 implementation
// ============================================================================

// SAFETY: All indices bounded by num_subquantizers, codebook_size, and codes.len()
// which are validated by the calling function via assertions.
#[allow(clippy::indexing_slicing, clippy::needless_range_loop)]
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn asymmetric_distance_avx2(
    codes: &[u8],
    distance_table: &[f32],
    num_subquantizers: usize,
    codebook_size: usize,
) -> f32 {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let chunks = num_subquantizers / 8;
    let remainder = num_subquantizers % 8;

    let mut sum = _mm256_setzero_ps();

    // Process 8 sub-quantizers at a time
    // Note: This requires gathering from non-contiguous memory locations
    for i in 0..chunks {
        let offset = i * 8;

        // Gather 8 distances based on codes
        // Since we can't easily use _mm256_i32gather_ps with u8 codes,
        // we'll manually load the values
        let mut distances = [0.0f32; 8];
        for j in 0..8 {
            let m = offset + j;
            let code = codes[m] as usize;
            let table_offset = m * codebook_size + code;
            distances[j] = distance_table[table_offset];
        }

        let v = _mm256_loadu_ps(distances.as_ptr());
        sum = _mm256_add_ps(sum, v);
    }

    // Horizontal sum
    let mut result = horizontal_sum_avx2(sum);

    // Handle remainder
    let remainder_start = chunks * 8;
    for m in remainder_start..num_subquantizers {
        let code = codes[m] as usize;
        let table_offset = m * codebook_size + code;
        result += distance_table[table_offset];
    }

    result
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn horizontal_sum_avx2(v: std::arch::x86_64::__m256) -> f32 {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let low = _mm256_castps256_ps128(v);
    let high = _mm256_extractf128_ps(v, 1);
    let sum128 = _mm_add_ps(low, high);
    let hadd1 = _mm_hadd_ps(sum128, sum128);
    let hadd2 = _mm_hadd_ps(hadd1, hadd1);
    _mm_cvtss_f32(hadd2)
}

/// Encode a vector into PQ codes using SIMD-accelerated distance computation
///
/// # Arguments
/// * `vector` - Input vector to encode
/// * `codebooks` - Codebook centroids for each sub-quantizer
/// * `num_subquantizers` - Number of sub-quantizers
/// * `codebook_size` - Size of each codebook
/// * `subvector_dim` - Dimension of each sub-vector
///
/// # Returns
/// Vector of codes (one per sub-quantizer)
// SAFETY: `m` iterates 0..num_subquantizers; `offset + subvector_dim` is bounded by
// vector.len() which equals num_subquantizers * subvector_dim (validated by caller).
// codebooks[m] is bounded by num_subquantizers.
#[allow(clippy::indexing_slicing, clippy::needless_range_loop)]
pub fn encode_vector_simd(
    vector: &[f32],
    codebooks: &[Vec<Vec<f32>>],
    num_subquantizers: usize,
    _codebook_size: usize,
    subvector_dim: usize,
) -> Vec<u8> {
    let mut codes = Vec::with_capacity(num_subquantizers);

    for m in 0..num_subquantizers {
        let offset = m * subvector_dim;
        let subvector = &vector[offset..offset + subvector_dim];

        // Find nearest centroid using SIMD-accelerated distance
        let mut min_dist = f32::MAX;
        let mut best_code = 0u8;

        for (k, centroid) in codebooks[m].iter().enumerate() {
            let dist = super::distance::l2_distance_squared(subvector, centroid);
            if dist < min_dist {
                min_dist = dist;
                best_code = k as u8;
            }
        }

        codes.push(best_code);
    }

    codes
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_asymmetric_distance_simple() {
        let codes = vec![0, 1, 2, 3];
        let distance_table = vec![
            1.0, 2.0, 3.0, 4.0,  // sub-quantizer 0
            5.0, 6.0, 7.0, 8.0,  // sub-quantizer 1
            9.0, 10.0, 11.0, 12.0,  // sub-quantizer 2
            13.0, 14.0, 15.0, 16.0,  // sub-quantizer 3
        ];

        let dist = asymmetric_distance_simd(&[], &codes, &distance_table, 4, 4);

        // codes[0]=0 -> table[0]=1.0
        // codes[1]=1 -> table[4+1]=6.0
        // codes[2]=2 -> table[8+2]=11.0
        // codes[3]=3 -> table[12+3]=16.0
        // sum = 34.0
        assert_eq!(dist, 34.0);
    }

    #[test]
    fn test_asymmetric_distance_large() {
        // Test with enough sub-quantizers for SIMD
        let num_subquantizers = 16;
        let codebook_size = 256;

        let codes: Vec<u8> = (0..num_subquantizers).map(|i| (i * 13) as u8).collect();
        let distance_table: Vec<f32> = (0..num_subquantizers * codebook_size)
            .map(|i| i as f32 * 0.1)
            .collect();

        let dist_simd = asymmetric_distance_simd(&[], &codes, &distance_table, num_subquantizers, codebook_size);
        let dist_scalar = asymmetric_distance_scalar(&codes, &distance_table, num_subquantizers, codebook_size);

        // Use relative tolerance for larger accumulated values
        let max_dist = dist_simd.max(dist_scalar);
        let tolerance = if max_dist > 100.0 {
            max_dist * 1e-3
        } else if max_dist > 10.0 {
            max_dist * 1e-4
        } else {
            1e-5
        };

        assert!((dist_simd - dist_scalar).abs() < tolerance,
                "SIMD ({}) != Scalar ({}), diff: {}", dist_simd, dist_scalar, (dist_simd - dist_scalar).abs());
    }

    #[test]
    fn test_compute_distance_table() {
        // Create simple codebooks
        let codebooks = vec![
            vec![
                vec![1.0, 0.0],
                vec![0.0, 1.0],
            ],
            vec![
                vec![2.0, 0.0],
                vec![0.0, 2.0],
            ],
        ];

        let query = vec![1.0, 1.0, 1.0, 1.0];

        let table = compute_distance_table(&query, &codebooks, 2, 2, 2);

        // Verify table has correct size
        assert_eq!(table.len(), 4); // 2 sub-quantizers × 2 centroids

        // Check some values (distances should be >= 0)
        for &dist in &table {
            assert!(dist >= 0.0);
        }
    }

    #[test]
    fn test_encode_vector_simd() {
        let codebooks = vec![
            vec![
                vec![0.0, 0.0],
                vec![1.0, 1.0],
            ],
            vec![
                vec![0.0, 0.0],
                vec![2.0, 2.0],
            ],
        ];

        let vector = vec![0.9, 0.9, 1.9, 1.9];

        let codes = encode_vector_simd(&vector, &codebooks, 2, 2, 2);

        assert_eq!(codes.len(), 2);
        // First subvector [0.9, 0.9] should be closer to [1.0, 1.0] (code 1)
        assert_eq!(codes[0], 1);
        // Second subvector [1.9, 1.9] should be closer to [2.0, 2.0] (code 1)
        assert_eq!(codes[1], 1);
    }

    #[test]
    fn test_simd_correctness_random() {
        use rand::Rng;
        let mut rng = rand::thread_rng();

        for num_subquantizers in [8, 16, 32] {
            let codebook_size = 256;

            let codes: Vec<u8> = (0..num_subquantizers)
                .map(|_| rng.gen::<u8>())
                .collect();

            let distance_table: Vec<f32> = (0..num_subquantizers * codebook_size)
                .map(|_| rng.gen_range(0.0..10.0))
                .collect();

            let dist_simd = asymmetric_distance_simd(
                &[], &codes, &distance_table, num_subquantizers, codebook_size
            );
            let dist_scalar = asymmetric_distance_scalar(
                &codes, &distance_table, num_subquantizers, codebook_size
            );

            // Use adaptive tolerance based on magnitude
            let max_dist = dist_simd.max(dist_scalar);
            let tolerance = if max_dist > 100.0 {
                max_dist * 1e-3  // 0.1% relative tolerance for large values
            } else if max_dist > 10.0 {
                max_dist * 1e-4  // 0.01% relative tolerance
            } else {
                1e-4  // Absolute tolerance for small values
            };

            assert!(
                (dist_simd - dist_scalar).abs() < tolerance,
                "M={}: SIMD ({}) != Scalar ({}), diff: {}, tolerance: {}",
                num_subquantizers,
                dist_simd,
                dist_scalar,
                (dist_simd - dist_scalar).abs(),
                tolerance
            );
        }
    }
}
