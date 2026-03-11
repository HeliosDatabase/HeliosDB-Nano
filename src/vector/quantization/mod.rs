//! Product Quantization for Vector Compression
//!
//! Implements the Product Quantization (PQ) algorithm for reducing vector memory footprint
//! by 8-16x while maintaining 95-98% search accuracy.
//!
//! Based on: "Product Quantization for Nearest Neighbor Search" (Jégou et al., CVPR 2011)
//! Paper: <https://lear.inrialpes.fr/pubs/2011/JDS11/jegou_searching_with_quantization.pdf>

pub mod product_quantizer;
pub mod codebook;
pub mod encoder;
pub mod decoder;
pub mod distance;
pub mod training;

pub use product_quantizer::{ProductQuantizer, ProductQuantizerConfig};
pub use codebook::Codebook;
pub use encoder::Encoder;
pub use decoder::Decoder;
pub use distance::DistanceComputer;
pub use training::train_codebook;

use serde::{Serialize, Deserialize};

/// Quantized vector representation
///
/// A vector compressed using Product Quantization. Instead of storing
/// the full vector (e.g., 768 floats × 4 bytes = 3KB), we store only
/// sub-quantizer codes (e.g., 8 codes × 1 byte = 8 bytes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuantizedVector {
    /// Sub-quantizer codes (one per sub-vector)
    pub codes: Vec<u8>,
}

impl QuantizedVector {
    /// Create a new quantized vector
    pub fn new(codes: Vec<u8>) -> Self {
        Self { codes }
    }

    /// Get the memory size in bytes
    pub fn memory_size(&self) -> usize {
        self.codes.len()
    }
}

/// Product Quantization error types
#[derive(Debug, thiserror::Error)]
pub enum PqError {
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },

    #[error("Training error: {0}")]
    TrainingError(String),

    #[error("Encoding error: {0}")]
    EncodingError(String),

    #[error("Decoding error: {0}")]
    DecodingError(String),

    #[error("Invalid sub-quantizer index: {0}")]
    InvalidSubQuantizerIndex(usize),

    #[error("Invalid centroid index: {0}")]
    InvalidCentroidIndex(usize),

    #[error("Insufficient training data: got {0} samples, need at least {1}")]
    InsufficientTrainingData(usize, usize),
}

pub type PqResult<T> = std::result::Result<T, PqError>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_quantized_vector_creation() {
        let codes = vec![23, 156, 87, 12, 200, 45, 178, 91];
        let qv = QuantizedVector::new(codes.clone());
        assert_eq!(qv.codes, codes);
        assert_eq!(qv.memory_size(), 8);
    }
}
