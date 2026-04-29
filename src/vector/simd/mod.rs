//! SIMD-accelerated vector operations
//!
//! This module provides SIMD-optimized implementations of vector distance calculations
//! with runtime CPU feature detection and automatic fallback to scalar implementations.
//!
//! ## Supported Operations
//!
//! - **L2 Distance**: Euclidean distance with SIMD acceleration
//! - **Cosine Distance**: Cosine similarity-based distance
//! - **Dot Product**: Inner product calculation
//!
//! ## Feature Detection
//!
//! The module automatically detects available CPU features at runtime:
//! - AVX2 (256-bit SIMD on x86_64)
//! - AVX-512 (512-bit SIMD on x86_64, when available)
//! - Scalar fallback for all platforms
//!
//! ## Performance
//!
//! Expected speedups on SIMD-enabled platforms:
//! - L2 Distance: 2-4x faster with AVX2, 4-8x with AVX-512
//! - Cosine Distance: 2-5x faster with AVX2
//! - Dot Product: 3-6x faster with AVX2
//!
//! ## Example
//!
//! ```rust
//! use heliosdb_nano::vector::simd;
//!
//! let a = vec![1.0, 2.0, 3.0, 4.0];
//! let b = vec![5.0, 6.0, 7.0, 8.0];
//!
//! // Automatically uses SIMD if available
//! let dist = simd::l2_distance(&a, &b);
//! ```

pub mod distance;
pub mod quantization;

pub use distance::{
    l2_distance, cosine_distance, dot_product, l2_distance_squared,
};

/// CPU feature detection results
#[derive(Debug, Clone, Copy)]
pub struct CpuFeatures {
    pub avx2: bool,
    pub avx512f: bool,
    pub sse42: bool,
}

impl CpuFeatures {
    /// Detect available CPU features at runtime
    #[cfg(target_arch = "x86_64")]
    pub fn detect() -> Self {
        Self {
            avx2: is_x86_feature_detected!("avx2"),
            avx512f: is_x86_feature_detected!("avx512f"),
            sse42: is_x86_feature_detected!("sse4.2"),
        }
    }

    /// For non-x86_64 platforms, return no SIMD features
    #[cfg(not(target_arch = "x86_64"))]
    pub fn detect() -> Self {
        Self {
            avx2: false,
            avx512f: false,
            sse42: false,
        }
    }

    /// Get a string describing available features
    pub fn description(&self) -> String {
        let mut features = Vec::new();
        if self.avx512f {
            features.push("AVX-512");
        }
        if self.avx2 {
            features.push("AVX2");
        }
        if self.sse42 {
            features.push("SSE4.2");
        }
        if features.is_empty() {
            "Scalar (no SIMD)".to_string()
        } else {
            features.join(", ")
        }
    }
}

/// Get the detected CPU features (cached)
pub fn cpu_features() -> CpuFeatures {
    static FEATURES: std::sync::OnceLock<CpuFeatures> = std::sync::OnceLock::new();
    *FEATURES.get_or_init(CpuFeatures::detect)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_cpu_detection() {
        let features = cpu_features();
        println!("Detected CPU features: {}", features.description());

        // On x86_64, we should detect at least some features
        #[cfg(target_arch = "x86_64")]
        {
            // Most modern x86_64 CPUs have SSE4.2
            // but we can't guarantee it in CI
            println!("AVX2: {}", features.avx2);
            println!("AVX-512: {}", features.avx512f);
            println!("SSE4.2: {}", features.sse42);
        }

        // On non-x86_64, should be all false
        #[cfg(not(target_arch = "x86_64"))]
        {
            assert!(!features.avx2);
            assert!(!features.avx512f);
            assert!(!features.sse42);
        }
    }

    #[test]
    fn test_feature_description() {
        let features = cpu_features();
        let desc = features.description();
        assert!(!desc.is_empty());
        println!("Features: {}", desc);
    }
}
