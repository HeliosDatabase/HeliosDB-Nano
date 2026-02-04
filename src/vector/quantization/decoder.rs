//! Quantized vector decoding to approximate reconstruction

#![allow(unused_variables)]

use super::{Codebook, QuantizedVector, PqError, PqResult};
use crate::vector::Vector;
use std::sync::Arc;

/// Decoder: reconstructs approximate vectors from quantized codes
pub struct Decoder {
    codebook: Arc<Codebook>,
}

impl Decoder {
    /// Create a new decoder with a codebook
    pub fn new(codebook: Arc<Codebook>) -> Self {
        Self { codebook }
    }

    /// Decode a quantized vector to its approximate reconstruction
    ///
    /// Process:
    /// 1. For each code, look up the corresponding centroid
    /// 2. Concatenate all centroids to form the reconstructed vector
    pub fn decode(&self, quantized: &QuantizedVector) -> PqResult<Vector> {
        let num_subquantizers = self.codebook.num_subquantizers();
        let subvector_dim = self.codebook.subvector_dimension();

        if quantized.codes.len() != num_subquantizers {
            return Err(PqError::DecodingError(format!(
                "Expected {} codes, got {}",
                num_subquantizers,
                quantized.codes.len()
            )));
        }

        let mut reconstructed = Vec::with_capacity(self.codebook.dimension());

        // Look up each centroid and concatenate
        for (sq_idx, &code) in quantized.codes.iter().enumerate() {
            let centroid = self
                .codebook
                .get_centroid(sq_idx, code as usize)?;

            reconstructed.extend_from_slice(centroid);
        }

        Ok(reconstructed)
    }

    /// Decode a batch of quantized vectors
    pub fn decode_batch(&self, quantized_vectors: &[QuantizedVector]) -> PqResult<Vec<Vector>> {
        quantized_vectors
            .iter()
            .map(|qv| self.decode(qv))
            .collect()
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
    fn test_decoder_basic() {
        let codebook = Arc::new(create_test_codebook());
        let decoder = Decoder::new(codebook);

        // Codes [0, 1] should reconstruct to [0.0, 0.0, 3.0, 3.0]
        let quantized = QuantizedVector::new(vec![0, 1]);
        let reconstructed = decoder.decode(&quantized).unwrap();

        assert_eq!(reconstructed, vec![0.0, 0.0, 3.0, 3.0]);
    }

    #[test]
    fn test_decoder_all_codes() {
        let codebook = Arc::new(create_test_codebook());
        let decoder = Decoder::new(codebook);

        // Test all possible code combinations
        let test_cases = vec![
            (vec![0, 0], vec![0.0, 0.0, 2.0, 2.0]),
            (vec![0, 1], vec![0.0, 0.0, 3.0, 3.0]),
            (vec![1, 0], vec![1.0, 1.0, 2.0, 2.0]),
            (vec![1, 1], vec![1.0, 1.0, 3.0, 3.0]),
        ];

        for (codes, expected) in test_cases {
            let quantized = QuantizedVector::new(codes);
            let reconstructed = decoder.decode(&quantized).unwrap();
            assert_eq!(reconstructed, expected);
        }
    }

    #[test]
    fn test_decoder_wrong_num_codes() {
        let codebook = Arc::new(create_test_codebook());
        let decoder = Decoder::new(codebook);

        // Wrong number of codes
        let quantized = QuantizedVector::new(vec![0]); // Expected 2 codes
        let result = decoder.decode(&quantized);

        assert!(result.is_err());
        match result {
            Err(PqError::DecodingError(msg)) => {
                assert!(msg.contains("Expected 2 codes"));
            }
            _ => panic!("Expected DecodingError"),
        }
    }

    #[test]
    fn test_decoder_batch() {
        let codebook = Arc::new(create_test_codebook());
        let decoder = Decoder::new(codebook);

        let quantized_vectors = vec![
            QuantizedVector::new(vec![0, 0]),
            QuantizedVector::new(vec![1, 1]),
        ];

        let reconstructed = decoder.decode_batch(&quantized_vectors).unwrap();
        assert_eq!(reconstructed.len(), 2);
        assert_eq!(reconstructed[0], vec![0.0, 0.0, 2.0, 2.0]);
        assert_eq!(reconstructed[1], vec![1.0, 1.0, 3.0, 3.0]);
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        use super::super::Encoder;

        let codebook = Arc::new(create_test_codebook());
        let encoder = Encoder::new(codebook.clone());
        let decoder = Decoder::new(codebook);

        // Original vector
        let original = vec![0.9, 0.9, 3.1, 3.1];

        // Encode
        let quantized = encoder.encode(&original).unwrap();

        // Decode
        let reconstructed = decoder.decode(&quantized).unwrap();

        // Should be close to centroid values
        assert_eq!(reconstructed, vec![1.0, 1.0, 3.0, 3.0]);
    }
}
