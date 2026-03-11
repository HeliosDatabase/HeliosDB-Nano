//! Vector search implementation
//!
//! HNSW index using published research (no proprietary algorithms).
//! Based on "Efficient and robust approximate nearest neighbor search using
//! Hierarchical Navigable Small World graphs" (<https://arxiv.org/abs/1603.09320>)

pub mod hnsw_index;
pub mod quantization;
pub mod quantized_hnsw;
pub mod simd;

pub use hnsw_index::{HnswIndex, HnswConfig, MultiMetricHnswIndex};
pub use quantization::{
    ProductQuantizer, ProductQuantizerConfig, Codebook, QuantizedVector,
    Encoder, Decoder, DistanceComputer, PqError, PqResult,
};
pub use quantized_hnsw::{QuantizedHnswIndex, QuantizedHnswConfig, MemoryStats};
use serde::{Serialize, Deserialize};

/// Vector dimension
pub type Dimension = usize;

/// Vector (embedding)
pub type Vector = Vec<f32>;

/// Distance metric
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DistanceMetric {
    /// Cosine similarity
    Cosine,
    /// L2 (Euclidean) distance
    L2,
    /// Inner product (dot product)
    InnerProduct,
}

/// Calculate L2 (Euclidean) distance between two vectors
///
/// Automatically uses SIMD acceleration when available (AVX2 on x86_64).
/// Expected speedup: 2-4x on 128+ dimensional vectors with AVX2.
pub fn l2_distance(a: &[f32], b: &[f32]) -> f32 {
    simd::l2_distance(a, b)
}

/// Calculate cosine distance (1 - cosine similarity) between two vectors
///
/// Automatically uses SIMD acceleration when available (AVX2 on x86_64).
/// Expected speedup: 2-5x on 128+ dimensional vectors with AVX2.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    simd::cosine_distance(a, b)
}

/// Calculate negative inner product (dot product) between two vectors
/// Returns negative because we want smaller values to be "closer"
///
/// Automatically uses SIMD acceleration when available (AVX2 on x86_64).
/// Expected speedup: 3-6x on 128+ dimensional vectors with AVX2.
pub fn inner_product_distance(a: &[f32], b: &[f32]) -> f32 {
    -simd::dot_product(a, b)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_l2_distance() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let dist = l2_distance(&a, &b);
        assert!((dist - 1.414).abs() < 0.01);
    }

    #[test]
    fn test_cosine_distance() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let dist = cosine_distance(&a, &b);
        assert!((dist - 1.0).abs() < 0.001); // Orthogonal vectors
    }

    #[test]
    fn test_inner_product() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 5.0, 6.0];
        let dist = inner_product_distance(&a, &b);
        assert_eq!(dist, -32.0); // -(4 + 10 + 18)
    }
}
