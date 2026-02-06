//! SIMD-accelerated distance calculations
//!
//! Provides optimized implementations of common distance metrics with automatic
//! CPU feature detection and fallback to scalar code.

#![allow(unused_variables)]

use super::cpu_features;

/// Calculate L2 (Euclidean) distance between two vectors using SIMD when available
///
/// This function automatically dispatches to the best available implementation:
/// - AVX2 SIMD on x86_64 with AVX2 support
/// - Scalar fallback on all platforms
///
/// # Performance
/// - Expected 2-4x speedup with AVX2 for vectors with 128+ dimensions
///
/// # Panics
/// Panics if vector dimensions don't match
pub fn l2_distance(a: &[f32], b: &[f32]) -> f32 {
    l2_distance_squared(a, b).sqrt()
}

/// Calculate squared L2 distance (avoids sqrt for better performance when comparing distances)
///
/// # Performance
/// - Expected 2-4x speedup with AVX2 for vectors with 128+ dimensions
///
/// # Panics
/// Panics if vector dimensions don't match
pub fn l2_distance_squared(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "Vector dimensions must match");

    #[cfg(target_arch = "x86_64")]
    {
        let features = cpu_features();
        if features.avx2 && a.len() >= 8 {
            // AVX2 path - process 8 floats at a time
            return unsafe { l2_distance_squared_avx2(a, b) };
        }
    }

    // Scalar fallback
    l2_distance_squared_scalar(a, b)
}

/// Calculate cosine distance (1 - cosine similarity) using SIMD when available
///
/// # Performance
/// - Expected 2-5x speedup with AVX2 for vectors with 128+ dimensions
///
/// # Panics
/// Panics if vector dimensions don't match
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "Vector dimensions must match");

    #[cfg(target_arch = "x86_64")]
    {
        let features = cpu_features();
        if features.avx2 && a.len() >= 8 {
            return unsafe { cosine_distance_avx2(a, b) };
        }
    }

    // Scalar fallback
    cosine_distance_scalar(a, b)
}

/// Calculate dot product using SIMD when available
///
/// # Performance
/// - Expected 3-6x speedup with AVX2 for vectors with 128+ dimensions
///
/// # Panics
/// Panics if vector dimensions don't match
pub fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "Vector dimensions must match");

    #[cfg(target_arch = "x86_64")]
    {
        let features = cpu_features();
        if features.avx2 && a.len() >= 8 {
            return unsafe { dot_product_avx2(a, b) };
        }
    }

    // Scalar fallback
    dot_product_scalar(a, b)
}

// ============================================================================
// Scalar implementations (portable, always available)
// ============================================================================

#[inline]
fn l2_distance_squared_scalar(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let diff = x - y;
            diff * diff
        })
        .sum()
}

#[inline]
fn cosine_distance_scalar(a: &[f32], b: &[f32]) -> f32 {
    let mut dot_product = 0.0f32;
    let mut norm_a_sq = 0.0f32;
    let mut norm_b_sq = 0.0f32;

    for (&x, &y) in a.iter().zip(b.iter()) {
        dot_product += x * y;
        norm_a_sq += x * x;
        norm_b_sq += y * y;
    }

    let norm_a = norm_a_sq.sqrt();
    let norm_b = norm_b_sq.sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 1.0; // Maximum distance for zero vectors
    }

    1.0 - (dot_product / (norm_a * norm_b))
}

#[inline]
fn dot_product_scalar(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

// ============================================================================
// AVX2 implementations (x86_64 only)
// ============================================================================

// SAFETY: `i` iterates from remainder_start..len where len = a.len() = b.len()
// (asserted equal by the caller). All indices are bounded by the slice length.
#[allow(clippy::indexing_slicing)]
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2", enable = "fma")]
unsafe fn l2_distance_squared_avx2(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let len = a.len();
    let chunks = len / 8;
    let remainder = len % 8;

    let mut sum = _mm256_setzero_ps();

    // Process 8 floats at a time
    let a_ptr = a.as_ptr();
    let b_ptr = b.as_ptr();

    for i in 0..chunks {
        let offset = i * 8;
        let va = _mm256_loadu_ps(a_ptr.add(offset));
        let vb = _mm256_loadu_ps(b_ptr.add(offset));

        // diff = a - b
        let diff = _mm256_sub_ps(va, vb);

        // sum += diff * diff (using FMA: sum = diff * diff + sum)
        sum = _mm256_fmadd_ps(diff, diff, sum);
    }

    // Horizontal sum of the 8 lanes
    let mut result = horizontal_sum_avx2(sum);

    // Handle remainder with scalar code
    let remainder_start = chunks * 8;
    for i in remainder_start..len {
        let diff = a[i] - b[i];
        result += diff * diff;
    }

    result
}

// SAFETY: `i` iterates from remainder_start..len where len = a.len() = b.len()
// (asserted equal by the caller). All indices are bounded by the slice length.
#[allow(clippy::indexing_slicing)]
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2", enable = "fma")]
unsafe fn cosine_distance_avx2(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let len = a.len();
    let chunks = len / 8;
    let remainder = len % 8;

    let mut dot = _mm256_setzero_ps();
    let mut norm_a = _mm256_setzero_ps();
    let mut norm_b = _mm256_setzero_ps();

    let a_ptr = a.as_ptr();
    let b_ptr = b.as_ptr();

    // Process 8 floats at a time
    for i in 0..chunks {
        let offset = i * 8;
        let va = _mm256_loadu_ps(a_ptr.add(offset));
        let vb = _mm256_loadu_ps(b_ptr.add(offset));

        // dot += a * b
        dot = _mm256_fmadd_ps(va, vb, dot);

        // norm_a += a * a
        norm_a = _mm256_fmadd_ps(va, va, norm_a);

        // norm_b += b * b
        norm_b = _mm256_fmadd_ps(vb, vb, norm_b);
    }

    // Horizontal sums
    let mut dot_sum = horizontal_sum_avx2(dot);
    let mut norm_a_sum = horizontal_sum_avx2(norm_a);
    let mut norm_b_sum = horizontal_sum_avx2(norm_b);

    // Handle remainder with scalar code
    let remainder_start = chunks * 8;
    for i in remainder_start..len {
        let ax = a[i];
        let bx = b[i];
        dot_sum += ax * bx;
        norm_a_sum += ax * ax;
        norm_b_sum += bx * bx;
    }

    let norm_a_val = norm_a_sum.sqrt();
    let norm_b_val = norm_b_sum.sqrt();

    if norm_a_val == 0.0 || norm_b_val == 0.0 {
        return 1.0;
    }

    1.0 - (dot_sum / (norm_a_val * norm_b_val))
}

// SAFETY: `i` iterates from remainder_start..len where len = a.len() = b.len()
// (asserted equal by the caller). All indices are bounded by the slice length.
#[allow(clippy::indexing_slicing)]
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2", enable = "fma")]
unsafe fn dot_product_avx2(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let len = a.len();
    let chunks = len / 8;
    let remainder = len % 8;

    let mut sum = _mm256_setzero_ps();

    let a_ptr = a.as_ptr();
    let b_ptr = b.as_ptr();

    // Process 8 floats at a time
    for i in 0..chunks {
        let offset = i * 8;
        let va = _mm256_loadu_ps(a_ptr.add(offset));
        let vb = _mm256_loadu_ps(b_ptr.add(offset));

        // sum += a * b
        sum = _mm256_fmadd_ps(va, vb, sum);
    }

    // Horizontal sum
    let mut result = horizontal_sum_avx2(sum);

    // Handle remainder with scalar code
    let remainder_start = chunks * 8;
    for i in remainder_start..len {
        result += a[i] * b[i];
    }

    result
}

/// Horizontal sum of 8 floats in an AVX2 register
#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn horizontal_sum_avx2(v: std::arch::x86_64::__m256) -> f32 {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    // v = [a, b, c, d, e, f, g, h]
    // Extract high and low 128-bit halves
    let low = _mm256_castps256_ps128(v);           // [a, b, c, d]
    let high = _mm256_extractf128_ps(v, 1);        // [e, f, g, h]

    // Add them: [a+e, b+f, c+g, d+h]
    let sum128 = _mm_add_ps(low, high);

    // Horizontal add within 128 bits
    // hadd: [a+e+b+f, c+g+d+h, a+e+b+f, c+g+d+h]
    let hadd1 = _mm_hadd_ps(sum128, sum128);

    // hadd again: [sum_all, sum_all, sum_all, sum_all]
    let hadd2 = _mm_hadd_ps(hadd1, hadd1);

    // Extract the first element
    _mm_cvtss_f32(hadd2)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const EPSILON: f32 = 1e-5;

    fn assert_approx_eq(a: f32, b: f32, msg: &str) {
        // Use adaptive tolerance based on magnitude and expected accumulation error
        let max_val = a.abs().max(b.abs());

        // For very large values (accumulated sums), use relative tolerance
        // SIMD operations can accumulate floating-point errors differently than scalar
        let tolerance = if max_val > 10000.0 {
            // Very large accumulated values: 1% relative tolerance
            max_val * 1e-2
        } else if max_val > 1000.0 {
            // Larger relative tolerance for accumulated large values: 0.5%
            max_val * 5e-3
        } else if max_val > 100.0 {
            // Medium values: 0.1% relative tolerance
            max_val * 1e-3
        } else if max_val > 1.0 {
            // Relative tolerance: 0.01% for moderate values
            max_val * 1e-4
        } else {
            // Absolute tolerance for small values
            EPSILON
        };

        assert!(
            (a - b).abs() < tolerance,
            "{}: {} != {} (diff: {}, tolerance: {})",
            msg,
            a,
            b,
            (a - b).abs(),
            tolerance
        );
    }

    #[test]
    fn test_l2_distance_small() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];

        let dist = l2_distance(&a, &b);
        let expected = 2.0f32.sqrt();

        assert_approx_eq(dist, expected, "L2 distance");
    }

    #[test]
    fn test_l2_distance_large() {
        // Test with vector large enough for SIMD
        let a: Vec<f32> = (0..128).map(|i| i as f32).collect();
        let b: Vec<f32> = (0..128).map(|i| (i as f32) * 0.5).collect();

        let dist_simd = l2_distance(&a, &b);
        let dist_scalar = l2_distance_squared_scalar(&a, &b).sqrt();

        assert_approx_eq(dist_simd, dist_scalar, "L2 SIMD vs scalar");
    }

    #[test]
    fn test_l2_distance_squared() {
        let a = vec![3.0, 4.0];
        let b = vec![0.0, 0.0];

        let dist_sq = l2_distance_squared(&a, &b);
        assert_approx_eq(dist_sq, 25.0, "L2 squared");
    }

    #[test]
    fn test_cosine_distance_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];

        let dist = cosine_distance(&a, &b);
        assert_approx_eq(dist, 1.0, "Cosine distance (orthogonal)");
    }

    #[test]
    fn test_cosine_distance_parallel() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![2.0, 4.0, 6.0];

        let dist = cosine_distance(&a, &b);
        assert_approx_eq(dist, 0.0, "Cosine distance (parallel)");
    }

    #[test]
    fn test_cosine_distance_large() {
        // Test with vector large enough for SIMD
        let a: Vec<f32> = (0..256).map(|i| (i as f32).sin()).collect();
        let b: Vec<f32> = (0..256).map(|i| (i as f32).cos()).collect();

        let dist_simd = cosine_distance(&a, &b);
        let dist_scalar = cosine_distance_scalar(&a, &b);

        assert_approx_eq(dist_simd, dist_scalar, "Cosine SIMD vs scalar");
    }

    #[test]
    fn test_dot_product_simple() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 5.0, 6.0];

        let dot = dot_product(&a, &b);
        assert_approx_eq(dot, 32.0, "Dot product");
    }

    #[test]
    fn test_dot_product_large() {
        // Test with vector large enough for SIMD
        let a: Vec<f32> = (0..512).map(|i| i as f32).collect();
        let b: Vec<f32> = (0..512).map(|i| (i as f32) * 2.0).collect();

        let dot_simd = dot_product(&a, &b);
        let dot_scalar = dot_product_scalar(&a, &b);

        assert_approx_eq(dot_simd, dot_scalar, "Dot product SIMD vs scalar");
    }

    #[test]
    fn test_zero_vectors() {
        let a = vec![0.0; 64];
        let b = vec![1.0; 64];

        let cosine = cosine_distance(&a, &b);
        assert_approx_eq(cosine, 1.0, "Cosine with zero vector");
    }

    #[test]
    fn test_simd_correctness_random() {
        // Generate random vectors and ensure SIMD matches scalar
        use rand::Rng;
        let mut rng = rand::thread_rng();

        for size in [8, 16, 32, 64, 128, 256, 384, 512] {
            let a: Vec<f32> = (0..size).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let b: Vec<f32> = (0..size).map(|_| rng.gen_range(-1.0..1.0)).collect();

            // L2 distance
            let l2_simd = l2_distance_squared(&a, &b);
            let l2_scalar = l2_distance_squared_scalar(&a, &b);
            assert_approx_eq(l2_simd, l2_scalar, &format!("L2 size {}", size));

            // Cosine distance
            let cos_simd = cosine_distance(&a, &b);
            let cos_scalar = cosine_distance_scalar(&a, &b);
            assert_approx_eq(cos_simd, cos_scalar, &format!("Cosine size {}", size));

            // Dot product
            let dot_simd = dot_product(&a, &b);
            let dot_scalar = dot_product_scalar(&a, &b);
            assert_approx_eq(dot_simd, dot_scalar, &format!("Dot product size {}", size));
        }
    }

    #[test]
    #[should_panic(expected = "Vector dimensions must match")]
    fn test_dimension_mismatch() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0];

        let _ = l2_distance(&a, &b);
    }
}
