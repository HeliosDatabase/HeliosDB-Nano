//! Vector encoding to quantized representation

use super::{Codebook, QuantizedVector, PqError, PqResult};
use crate::vector::Vector;
use std::sync::Arc;

/// Encoder: converts full vectors to quantized codes
pub struct Encoder {
    codebook: Arc<Codebook>,
}

impl Encoder {
    /// Create a new encoder with a codebook
    pub fn new(codebook: Arc<Codebook>) -> Self {
        Self { codebook }
    }

    /// Encode a vector to its quantized representation
    ///
    /// Process:
    /// 1. Split vector into M sub-vectors
    /// 2. For each sub-vector, find nearest centroid
    /// 3. Store centroid indices as codes
    pub fn encode(&self, vector: &Vector) -> PqResult<QuantizedVector> {
        let dimension = self.codebook.dimension();
        if vector.len() != dimension {
            return Err(PqError::DimensionMismatch {
                expected: dimension,
                actual: vector.len(),
            });
        }

        let num_subquantizers = self.codebook.num_subquantizers();
        let subvector_dim = self.codebook.subvector_dimension();
        let mut codes = Vec::with_capacity(num_subquantizers);

        // Split vector into sub-vectors and encode each
        for sq_idx in 0..num_subquantizers {
            let start = sq_idx * subvector_dim;
            let end = start + subvector_dim;
            let subvector = vector.get(start..end).ok_or(PqError::DimensionMismatch {
                expected: end,
                actual: vector.len(),
            })?;

            // Find nearest centroid for this sub-vector
            let code = self.codebook.find_nearest_centroid(sq_idx, subvector)?;
            codes.push(code);
        }

        Ok(QuantizedVector::new(codes))
    }

    /// Encode a batch of vectors
    pub fn encode_batch(&self, vectors: &[Vector]) -> PqResult<Vec<QuantizedVector>> {
        vectors.iter().map(|v| self.encode(v)).collect()
    }

    /// Get the codebook
    pub fn codebook(&self) -> Arc<Codebook> {
        self.codebook.clone()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
    fn test_encoder_basic() {
        let codebook = Arc::new(create_test_codebook());
        let encoder = Encoder::new(codebook);

        // Vector that should map to centroids [1, 1]
        let vector = vec![0.9, 0.9, 3.1, 3.1];
        let quantized = encoder.encode(&vector).unwrap();

        assert_eq!(quantized.codes.len(), 2);
        // First sub-vector [0.9, 0.9] is closer to [1.0, 1.0] (code 1)
        assert_eq!(quantized.codes[0], 1);
        // Second sub-vector [3.1, 3.1] is closer to [3.0, 3.0] (code 1)
        assert_eq!(quantized.codes[1], 1);
    }

    #[test]
    fn test_encoder_dimension_mismatch() {
        let codebook = Arc::new(create_test_codebook());
        let encoder = Encoder::new(codebook);

        let wrong_vector = vec![1.0, 2.0, 3.0]; // Wrong dimension
        let result = encoder.encode(&wrong_vector);

        assert!(result.is_err());
        match result {
            Err(PqError::DimensionMismatch { expected, actual }) => {
                assert_eq!(expected, 4);
                assert_eq!(actual, 3);
            }
            _ => panic!("Expected DimensionMismatch error"),
        }
    }

    #[test]
    fn test_encoder_batch() {
        let codebook = Arc::new(create_test_codebook());
        let encoder = Encoder::new(codebook);

        let vectors = vec![
            vec![0.1, 0.1, 2.1, 2.1],
            vec![0.9, 0.9, 3.1, 3.1],
        ];

        let quantized = encoder.encode_batch(&vectors).unwrap();
        assert_eq!(quantized.len(), 2);
        assert_eq!(quantized[0].codes, vec![0, 0]);
        assert_eq!(quantized[1].codes, vec![1, 1]);
    }
}
