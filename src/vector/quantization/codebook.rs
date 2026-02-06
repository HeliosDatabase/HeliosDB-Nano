//! Codebook management for Product Quantization
//!
//! The codebook stores the centroids for each sub-quantizer.
//! For M sub-quantizers with K centroids each, the codebook contains M×K centroids.

use super::{PqError, PqResult};
use serde::{Serialize, Deserialize};

/// Codebook: stores centroids for each sub-quantizer
///
/// Structure:
/// - M sub-quantizers (one for each sub-vector)
/// - K centroids per sub-quantizer (learned via k-means)
/// - Each centroid is a sub-vector of dimension D/M
///
/// Total size: M × K × (D/M) × 4 bytes (for f32)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Codebook {
    /// Number of sub-quantizers (M)
    num_subquantizers: usize,

    /// Number of centroids per sub-quantizer (K)
    num_centroids: usize,

    /// Sub-vector dimension (D/M)
    subvector_dimension: usize,

    /// Centroids: shape [M][K][D/M]
    ///
    /// centroids[i][j] = j-th centroid of i-th sub-quantizer
    /// Each centroid is a vector of length D/M
    centroids: Vec<Vec<Vec<f32>>>,
}

// SAFETY: All indexing in this impl is bounded by num_subquantizers and num_centroids
// which are validated via bounds checks (returning PqError) before every index operation.
// from_centroids validates shape consistency of the input centroids array.
#[allow(clippy::indexing_slicing)]
impl Codebook {
    /// Create a new empty codebook
    pub fn new(
        num_subquantizers: usize,
        num_centroids: usize,
        subvector_dimension: usize,
    ) -> Self {
        let centroids = vec![
            vec![vec![0.0; subvector_dimension]; num_centroids];
            num_subquantizers
        ];

        Self {
            num_subquantizers,
            num_centroids,
            subvector_dimension,
            centroids,
        }
    }

    /// Create codebook from pre-computed centroids
    pub fn from_centroids(centroids: Vec<Vec<Vec<f32>>>) -> PqResult<Self> {
        if centroids.is_empty() {
            return Err(PqError::InvalidConfig(
                "Centroids cannot be empty".to_string(),
            ));
        }

        let num_subquantizers = centroids.len();
        let num_centroids = centroids[0].len();
        let subvector_dimension = centroids[0][0].len();

        // Validate shape consistency
        for (sq_idx, sq_centroids) in centroids.iter().enumerate() {
            if sq_centroids.len() != num_centroids {
                return Err(PqError::InvalidConfig(format!(
                    "Sub-quantizer {} has {} centroids, expected {}",
                    sq_idx,
                    sq_centroids.len(),
                    num_centroids
                )));
            }

            for (c_idx, centroid) in sq_centroids.iter().enumerate() {
                if centroid.len() != subvector_dimension {
                    return Err(PqError::InvalidConfig(format!(
                        "Sub-quantizer {}, centroid {} has dimension {}, expected {}",
                        sq_idx,
                        c_idx,
                        centroid.len(),
                        subvector_dimension
                    )));
                }
            }
        }

        Ok(Self {
            num_subquantizers,
            num_centroids,
            subvector_dimension,
            centroids,
        })
    }

    /// Get centroid for a specific sub-quantizer and centroid index
    pub fn get_centroid(&self, subquantizer_idx: usize, centroid_idx: usize) -> PqResult<&[f32]> {
        if subquantizer_idx >= self.num_subquantizers {
            return Err(PqError::InvalidSubQuantizerIndex(subquantizer_idx));
        }

        if centroid_idx >= self.num_centroids {
            return Err(PqError::InvalidCentroidIndex(centroid_idx));
        }

        Ok(&self.centroids[subquantizer_idx][centroid_idx])
    }

    /// Set centroid for a specific sub-quantizer and centroid index
    pub fn set_centroid(
        &mut self,
        subquantizer_idx: usize,
        centroid_idx: usize,
        centroid: Vec<f32>,
    ) -> PqResult<()> {
        if subquantizer_idx >= self.num_subquantizers {
            return Err(PqError::InvalidSubQuantizerIndex(subquantizer_idx));
        }

        if centroid_idx >= self.num_centroids {
            return Err(PqError::InvalidCentroidIndex(centroid_idx));
        }

        if centroid.len() != self.subvector_dimension {
            return Err(PqError::DimensionMismatch {
                expected: self.subvector_dimension,
                actual: centroid.len(),
            });
        }

        self.centroids[subquantizer_idx][centroid_idx] = centroid;
        Ok(())
    }

    /// Get all centroids for a specific sub-quantizer
    pub fn get_subquantizer_centroids(&self, subquantizer_idx: usize) -> PqResult<&[Vec<f32>]> {
        if subquantizer_idx >= self.num_subquantizers {
            return Err(PqError::InvalidSubQuantizerIndex(subquantizer_idx));
        }

        Ok(&self.centroids[subquantizer_idx])
    }

    /// Get number of sub-quantizers
    pub fn num_subquantizers(&self) -> usize {
        self.num_subquantizers
    }

    /// Get number of centroids per sub-quantizer
    pub fn num_centroids(&self) -> usize {
        self.num_centroids
    }

    /// Get sub-vector dimension
    pub fn subvector_dimension(&self) -> usize {
        self.subvector_dimension
    }

    /// Get full vector dimension (D = M × D/M)
    pub fn dimension(&self) -> usize {
        self.num_subquantizers * self.subvector_dimension
    }

    /// Calculate memory size in bytes
    pub fn memory_size(&self) -> usize {
        // M × K × (D/M) × sizeof(f32)
        self.num_subquantizers
            * self.num_centroids
            * self.subvector_dimension
            * std::mem::size_of::<f32>()
    }

    /// Find nearest centroid index for a sub-vector
    ///
    /// Used during encoding to find which centroid best represents a sub-vector.
    pub fn find_nearest_centroid(
        &self,
        subquantizer_idx: usize,
        subvector: &[f32],
    ) -> PqResult<u8> {
        if subquantizer_idx >= self.num_subquantizers {
            return Err(PqError::InvalidSubQuantizerIndex(subquantizer_idx));
        }

        if subvector.len() != self.subvector_dimension {
            return Err(PqError::DimensionMismatch {
                expected: self.subvector_dimension,
                actual: subvector.len(),
            });
        }

        let centroids = &self.centroids[subquantizer_idx];
        let mut min_distance = f32::MAX;
        let mut min_idx = 0;

        for (idx, centroid) in centroids.iter().enumerate() {
            let distance = l2_distance_squared(subvector, centroid);
            if distance < min_distance {
                min_distance = distance;
                min_idx = idx;
            }
        }

        Ok(min_idx as u8)
    }

    /// Validate codebook integrity
    pub fn validate(&self) -> PqResult<()> {
        if self.num_subquantizers == 0 {
            return Err(PqError::InvalidConfig(
                "num_subquantizers must be > 0".to_string(),
            ));
        }

        if self.num_centroids == 0 || self.num_centroids > 256 {
            return Err(PqError::InvalidConfig(format!(
                "num_centroids must be between 1 and 256, got {}",
                self.num_centroids
            )));
        }

        if self.subvector_dimension == 0 {
            return Err(PqError::InvalidConfig(
                "subvector_dimension must be > 0".to_string(),
            ));
        }

        // Check that all centroids are finite
        for (sq_idx, sq_centroids) in self.centroids.iter().enumerate() {
            for (c_idx, centroid) in sq_centroids.iter().enumerate() {
                for (dim_idx, &value) in centroid.iter().enumerate() {
                    if !value.is_finite() {
                        return Err(PqError::InvalidConfig(format!(
                            "Non-finite value at subquantizer {}, centroid {}, dimension {}",
                            sq_idx, c_idx, dim_idx
                        )));
                    }
                }
            }
        }

        Ok(())
    }
}

/// Compute L2 distance squared between two vectors
fn l2_distance_squared(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y).powi(2))
        .sum()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_codebook_creation() {
        let codebook = Codebook::new(8, 256, 96);
        assert_eq!(codebook.num_subquantizers(), 8);
        assert_eq!(codebook.num_centroids(), 256);
        assert_eq!(codebook.subvector_dimension(), 96);
        assert_eq!(codebook.dimension(), 768);
    }

    #[test]
    fn test_codebook_get_set_centroid() {
        let mut codebook = Codebook::new(2, 4, 3);
        let centroid = vec![1.0, 2.0, 3.0];

        codebook.set_centroid(0, 0, centroid.clone()).unwrap();
        let retrieved = codebook.get_centroid(0, 0).unwrap();

        assert_eq!(retrieved, &centroid[..]);
    }

    #[test]
    fn test_codebook_invalid_indices() {
        let codebook = Codebook::new(2, 4, 3);

        // Invalid sub-quantizer index
        assert!(codebook.get_centroid(5, 0).is_err());

        // Invalid centroid index
        assert!(codebook.get_centroid(0, 10).is_err());
    }

    #[test]
    fn test_find_nearest_centroid() {
        let mut codebook = Codebook::new(1, 3, 2);

        // Set up 3 centroids
        codebook.set_centroid(0, 0, vec![0.0, 0.0]).unwrap();
        codebook.set_centroid(0, 1, vec![1.0, 0.0]).unwrap();
        codebook.set_centroid(0, 2, vec![0.0, 1.0]).unwrap();

        // Test point closest to centroid 1
        let nearest = codebook
            .find_nearest_centroid(0, &[0.9, 0.1])
            .unwrap();
        assert_eq!(nearest, 1);

        // Test point closest to centroid 2
        let nearest = codebook
            .find_nearest_centroid(0, &[0.1, 0.9])
            .unwrap();
        assert_eq!(nearest, 2);
    }

    #[test]
    fn test_codebook_memory_size() {
        let codebook = Codebook::new(8, 256, 96);
        // 8 * 256 * 96 * 4 = 786,432 bytes ≈ 768 KB
        assert_eq!(codebook.memory_size(), 786_432);
    }

    #[test]
    fn test_l2_distance_squared() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let dist = l2_distance_squared(&a, &b);
        // (1-0)^2 + (0-1)^2 + (0-0)^2 = 2
        assert_eq!(dist, 2.0);
    }

    #[test]
    fn test_codebook_validation() {
        let codebook = Codebook::new(8, 256, 96);
        assert!(codebook.validate().is_ok());
    }

    #[test]
    fn test_codebook_from_centroids() {
        let centroids = vec![
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            vec![vec![5.0, 6.0], vec![7.0, 8.0]],
        ];

        let codebook = Codebook::from_centroids(centroids).unwrap();
        assert_eq!(codebook.num_subquantizers(), 2);
        assert_eq!(codebook.num_centroids(), 2);
        assert_eq!(codebook.subvector_dimension(), 2);

        let centroid = codebook.get_centroid(0, 0).unwrap();
        assert_eq!(centroid, &[1.0, 2.0]);
    }
}
