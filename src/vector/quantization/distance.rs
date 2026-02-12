//! Asymmetric Distance Computation (ADC) for efficient similarity search
//!
//! ADC is the key optimization in PQ-based search:
//! - Query vector remains unquantized (high precision)
//! - Database vectors are quantized (compressed)
//! - Pre-compute distance table once per query
//! - Use table lookups for each database vector (O(M) instead of O(D))

use super::{Codebook, QuantizedVector, PqError, PqResult};
use crate::vector::Vector;
use std::sync::Arc;

/// Distance computer for PQ-based similarity search
pub struct DistanceComputer {
    codebook: Arc<Codebook>,
}

// SAFETY: All indexing in this impl is bounded by codebook dimensions (num_subquantizers,
// num_centroids, subvector_dimension) which are validated on construction and checked
// at each method entry via dimension/length validation before any indexing occurs.
#[allow(clippy::indexing_slicing)]
impl DistanceComputer {
    /// Create a new distance computer
    pub fn new(codebook: Arc<Codebook>) -> Self {
        Self { codebook }
    }

    /// Compute L2 distance between query and quantized vector
    ///
    /// This is the slow path - use precomputed table for batch queries.
    pub fn compute_distance(
        &self,
        query: &Vector,
        quantized: &QuantizedVector,
    ) -> PqResult<f32> {
        // Validate dimensions
        let dimension = self.codebook.dimension();
        if query.len() != dimension {
            return Err(PqError::DimensionMismatch {
                expected: dimension,
                actual: query.len(),
            });
        }

        let num_subquantizers = self.codebook.num_subquantizers();
        if quantized.codes.len() != num_subquantizers {
            return Err(PqError::EncodingError(format!(
                "Expected {} codes, got {}",
                num_subquantizers,
                quantized.codes.len()
            )));
        }

        let subvector_dim = self.codebook.subvector_dimension();
        let mut distance = 0.0_f32;

        // For each sub-vector
        for sq_idx in 0..num_subquantizers {
            let start = sq_idx * subvector_dim;
            let end = start + subvector_dim;
            let query_subvector = &query[start..end];

            // Get the centroid for this code
            let code = quantized.codes[sq_idx];
            let centroid = self.codebook.get_centroid(sq_idx, code as usize)?;

            // Add squared distance for this sub-vector
            distance += l2_distance_squared(query_subvector, centroid);
        }

        Ok(distance.sqrt())
    }

    /// Precompute distance table for a query (ADC optimization)
    ///
    /// Creates a table of shape [M][K] where:
    /// - M = number of sub-quantizers
    /// - K = number of centroids per sub-quantizer
    /// - table[i][j] = squared L2 distance between query sub-vector i and centroid j
    ///
    /// Complexity: O(M * K * D/M) = O(K * D)
    ///
    /// After precomputation, distance to any database vector is just O(M) lookups.
    pub fn precompute_distance_table(&self, query: &Vector) -> PqResult<Vec<Vec<f32>>> {
        let dimension = self.codebook.dimension();
        if query.len() != dimension {
            return Err(PqError::DimensionMismatch {
                expected: dimension,
                actual: query.len(),
            });
        }

        let num_subquantizers = self.codebook.num_subquantizers();
        let num_centroids = self.codebook.num_centroids();
        let subvector_dim = self.codebook.subvector_dimension();

        let mut table = vec![vec![0.0_f32; num_centroids]; num_subquantizers];

        // For each sub-quantizer
        #[allow(clippy::needless_range_loop)]
        for sq_idx in 0..num_subquantizers {
            let start = sq_idx * subvector_dim;
            let end = start + subvector_dim;
            let query_subvector = &query[start..end];

            // Compute distance to each centroid
            for c_idx in 0..num_centroids {
                let centroid = self.codebook.get_centroid(sq_idx, c_idx)?;
                table[sq_idx][c_idx] = l2_distance_squared(query_subvector, centroid);
            }
        }

        Ok(table)
    }

    /// Compute distance using precomputed table (fast path)
    ///
    /// Complexity: O(M) lookups + additions
    pub fn compute_distance_with_table(
        &self,
        distance_table: &[Vec<f32>],
        quantized: &QuantizedVector,
    ) -> PqResult<f32> {
        let num_subquantizers = self.codebook.num_subquantizers();

        if distance_table.len() != num_subquantizers {
            return Err(PqError::EncodingError(format!(
                "Distance table has wrong number of rows: expected {}, got {}",
                num_subquantizers,
                distance_table.len()
            )));
        }

        if quantized.codes.len() != num_subquantizers {
            return Err(PqError::EncodingError(format!(
                "Expected {} codes, got {}",
                num_subquantizers,
                quantized.codes.len()
            )));
        }

        let mut distance_squared = 0.0_f32;

        // Sum up pre-computed squared distances
        #[allow(clippy::needless_range_loop)]
        for sq_idx in 0..num_subquantizers {
            let code = quantized.codes[sq_idx] as usize;
            if code >= distance_table[sq_idx].len() {
                return Err(PqError::InvalidCentroidIndex(code));
            }
            distance_squared += distance_table[sq_idx][code];
        }

        Ok(distance_squared.sqrt())
    }

    /// Compute distances to multiple quantized vectors using precomputed table
    ///
    /// This is the main use case: precompute once, then scan many vectors efficiently.
    pub fn compute_distances_batch(
        &self,
        distance_table: &[Vec<f32>],
        quantized_vectors: &[QuantizedVector],
    ) -> PqResult<Vec<f32>> {
        quantized_vectors
            .iter()
            .map(|qv| self.compute_distance_with_table(distance_table, qv))
            .collect()
    }

    /// Get the codebook
    pub fn codebook(&self) -> Arc<Codebook> {
        self.codebook.clone()
    }
}

/// Compute L2 distance squared between two vectors
#[inline]
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
    use super::super::Encoder;

    fn create_test_codebook() -> Codebook {
        let mut codebook = Codebook::new(2, 2, 2);

        // Sub-quantizer 0 centroids
        codebook.set_centroid(0, 0, vec![0.0, 0.0]).unwrap();
        codebook.set_centroid(0, 1, vec![1.0, 1.0]).unwrap();

        // Sub-quantizer 1 centroids
        codebook.set_centroid(1, 0, vec![2.0, 2.0]).unwrap();
        codebook.set_centroid(1, 1, vec![3.0, 3.0]).unwrap();

        codebook
    }

    #[test]
    fn test_distance_computation_basic() {
        let codebook = Arc::new(create_test_codebook());
        let distance_computer = DistanceComputer::new(codebook.clone());
        let encoder = Encoder::new(codebook);

        // Query and database vector
        let query = vec![0.5, 0.5, 2.5, 2.5];
        let db_vector = vec![1.0, 1.0, 3.0, 3.0];

        // Encode database vector
        let quantized = encoder.encode(&db_vector).unwrap();

        // Compute distance (should be close to actual L2 distance)
        let pq_distance = distance_computer
            .compute_distance(&query, &quantized)
            .unwrap();

        // Actual L2 distance
        let actual_distance = ((0.5_f32 - 1.0_f32).powi(2)
            + (0.5_f32 - 1.0_f32).powi(2)
            + (2.5_f32 - 3.0_f32).powi(2)
            + (2.5_f32 - 3.0_f32).powi(2))
        .sqrt();

        // PQ distance should be exact in this case (vectors align with centroids)
        assert!((pq_distance - actual_distance).abs() < 0.01);
    }

    #[test]
    fn test_precompute_distance_table() {
        let codebook = Arc::new(create_test_codebook());
        let distance_computer = DistanceComputer::new(codebook);

        let query = vec![0.5, 0.5, 2.5, 2.5];
        let table = distance_computer
            .precompute_distance_table(&query)
            .unwrap();

        // Should have shape [2][2] (2 sub-quantizers, 2 centroids each)
        assert_eq!(table.len(), 2);
        assert_eq!(table[0].len(), 2);
        assert_eq!(table[1].len(), 2);

        // Verify some values manually
        // Sub-quantizer 0, centroid 0: distance from [0.5, 0.5] to [0.0, 0.0]
        let expected: f32 = 0.5_f32.powi(2) + 0.5_f32.powi(2);
        assert!((table[0][0] - expected).abs() < 0.001);

        // Sub-quantizer 0, centroid 1: distance from [0.5, 0.5] to [1.0, 1.0]
        let expected: f32 = 0.5_f32.powi(2) + 0.5_f32.powi(2);
        assert!((table[0][1] - expected).abs() < 0.001);
    }

    #[test]
    fn test_distance_with_table() {
        let codebook = Arc::new(create_test_codebook());
        let distance_computer = DistanceComputer::new(codebook.clone());
        let encoder = Encoder::new(codebook);

        let query = vec![0.5, 0.5, 2.5, 2.5];
        let db_vector = vec![1.0, 1.0, 3.0, 3.0];

        // Precompute table
        let table = distance_computer
            .precompute_distance_table(&query)
            .unwrap();

        // Encode database vector
        let quantized = encoder.encode(&db_vector).unwrap();

        // Compute distance with table
        let distance_with_table = distance_computer
            .compute_distance_with_table(&table, &quantized)
            .unwrap();

        // Compute distance without table
        let distance_without_table = distance_computer
            .compute_distance(&query, &quantized)
            .unwrap();

        // Should be identical
        assert!((distance_with_table - distance_without_table).abs() < 0.0001);
    }

    #[test]
    fn test_distance_batch() {
        let codebook = Arc::new(create_test_codebook());
        let distance_computer = DistanceComputer::new(codebook.clone());
        let encoder = Encoder::new(codebook);

        let query = vec![0.5, 0.5, 2.5, 2.5];
        let db_vectors = vec![
            vec![0.0, 0.0, 2.0, 2.0],
            vec![1.0, 1.0, 3.0, 3.0],
        ];

        // Precompute table
        let table = distance_computer
            .precompute_distance_table(&query)
            .unwrap();

        // Encode all database vectors
        let quantized: Vec<_> = db_vectors
            .iter()
            .map(|v| encoder.encode(v).unwrap())
            .collect();

        // Compute distances in batch
        let distances = distance_computer
            .compute_distances_batch(&table, &quantized)
            .unwrap();

        assert_eq!(distances.len(), 2);

        // Verify distances are reasonable
        for distance in distances {
            assert!(distance >= 0.0);
            assert!(distance.is_finite());
        }
    }

    #[test]
    fn test_l2_distance_squared() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 5.0, 6.0];
        let dist_sq = l2_distance_squared(&a, &b);
        // (1-4)^2 + (2-5)^2 + (3-6)^2 = 9 + 9 + 9 = 27
        assert_eq!(dist_sq, 27.0);
    }
}
