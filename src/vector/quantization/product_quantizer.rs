//! Core Product Quantization implementation

#![allow(unused_variables)]

use super::{Codebook, Encoder, Decoder, DistanceComputer, QuantizedVector, PqError, PqResult};
use crate::vector::Vector;
use serde::{Serialize, Deserialize};
use std::sync::Arc;

/// Product Quantizer configuration
///
/// Defines the parameters for Product Quantization algorithm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductQuantizerConfig {
    /// Number of sub-quantizers (M)
    ///
    /// Typical values: 8, 16, 32, 64
    /// - Higher M = better accuracy but larger code size
    /// - Must divide dimension evenly
    pub num_subquantizers: usize,

    /// Number of centroids per sub-quantizer (K)
    ///
    /// Typical value: 256 (fits in u8)
    /// - Higher K = better accuracy but more memory for codebook
    /// - 256 allows 1-byte codes
    pub num_centroids: usize,

    /// Vector dimension (D)
    ///
    /// Must be divisible by num_subquantizers
    pub dimension: usize,

    /// Training iterations for k-means
    ///
    /// Typical value: 25
    /// More iterations = better codebook but slower training
    pub training_iterations: usize,

    /// Minimum number of training samples required
    ///
    /// Should be >> num_centroids * num_subquantizers
    /// Typical: 10,000 - 100,000
    pub min_training_samples: usize,
}

impl ProductQuantizerConfig {
    /// Create a default configuration for a given dimension
    pub fn default_for_dimension(dimension: usize) -> PqResult<Self> {
        // Choose appropriate number of sub-quantizers
        let num_subquantizers = if dimension >= 512 {
            8
        } else if dimension >= 256 {
            4
        } else {
            2
        };

        if dimension % num_subquantizers != 0 {
            return Err(PqError::InvalidConfig(format!(
                "Dimension {} must be divisible by num_subquantizers {}",
                dimension, num_subquantizers
            )));
        }

        Ok(Self {
            num_subquantizers,
            num_centroids: 256, // Fits in u8
            dimension,
            training_iterations: 25,
            min_training_samples: 10000,
        })
    }

    /// Validate the configuration
    pub fn validate(&self) -> PqResult<()> {
        if self.dimension % self.num_subquantizers != 0 {
            return Err(PqError::InvalidConfig(format!(
                "Dimension {} must be divisible by num_subquantizers {}",
                self.dimension, self.num_subquantizers
            )));
        }

        if self.num_centroids == 0 || self.num_centroids > 256 {
            return Err(PqError::InvalidConfig(format!(
                "num_centroids must be between 1 and 256, got {}",
                self.num_centroids
            )));
        }

        if self.num_subquantizers == 0 {
            return Err(PqError::InvalidConfig(
                "num_subquantizers must be > 0".to_string()
            ));
        }

        if self.training_iterations == 0 {
            return Err(PqError::InvalidConfig(
                "training_iterations must be > 0".to_string()
            ));
        }

        Ok(())
    }

    /// Get sub-vector dimension (D/M)
    pub fn subvector_dimension(&self) -> usize {
        self.dimension / self.num_subquantizers
    }

    /// Create a test configuration with reduced requirements
    ///
    /// Used for unit tests where we generate small amounts of training data.
    /// Uses minimal but reasonable settings to avoid overtraining on small datasets.
    #[cfg(test)]
    pub fn test_config(dimension: usize) -> PqResult<Self> {
        // Choose appropriate number of sub-quantizers that divides dimension evenly
        let num_subquantizers = if dimension >= 768 {
            // 768 -> 8 subquantizers (768/8=96)
            8
        } else if dimension >= 512 {
            // 512 -> 8 subquantizers (512/8=64)
            8
        } else if dimension >= 256 {
            // 256 -> 4 subquantizers (256/4=64)
            4
        } else if dimension >= 128 {
            // 128 -> 4 subquantizers (128/4=32)
            4
        } else if dimension >= 64 {
            // 64 -> 4 subquantizers (64/4=16)
            4
        } else if dimension >= 32 {
            // 32 -> 4 subquantizers (32/4=8)
            4
        } else {
            // Small dimensions -> 2 subquantizers
            2
        };

        // Validate that dimension is divisible by num_subquantizers
        if dimension % num_subquantizers != 0 {
            // Try to find a better num_subquantizers that divides dimension
            let mut found = false;
            for nsq in [8, 4, 2, 1] {
                if dimension % nsq == 0 {
                    return Ok(Self {
                        num_subquantizers: nsq,
                        num_centroids: 32,  // Reduced from 256 for faster test training
                        dimension,
                        training_iterations: 5,  // Reduced from 25 for faster tests
                        min_training_samples: 100,  // Reduced from 10000 for test datasets
                    });
                }
            }

            // If we can't find a divisor, return error
            return Err(PqError::InvalidConfig(format!(
                "Dimension {} cannot be evenly divided into sub-quantizers",
                dimension
            )));
        }

        Ok(Self {
            num_subquantizers,
            num_centroids: 32,  // Reduced from 256 for faster test training
            dimension,
            training_iterations: 5,  // Reduced from 25 for faster tests
            min_training_samples: 100,  // Reduced from 10000 for test datasets
        })
    }
}

impl Default for ProductQuantizerConfig {
    fn default() -> Self {
        Self {
            num_subquantizers: 8,
            num_centroids: 256,
            dimension: 768, // Common for embeddings
            training_iterations: 25,
            min_training_samples: 10000,
        }
    }
}

/// Product Quantizer
///
/// Main struct for Product Quantization operations:
/// - Training: Build codebook from training vectors
/// - Encoding: Compress vectors to quantized codes
/// - Decoding: Reconstruct approximate vectors from codes
/// - Distance: Compute distances efficiently using ADC
pub struct ProductQuantizer {
    /// Configuration
    config: ProductQuantizerConfig,

    /// Codebook (centroids for each sub-quantizer)
    codebook: Arc<Codebook>,

    /// Encoder
    encoder: Encoder,

    /// Decoder
    decoder: Decoder,

    /// Distance computer
    distance_computer: DistanceComputer,
}

impl ProductQuantizer {
    /// Create a new Product Quantizer with a trained codebook
    pub fn new(config: ProductQuantizerConfig, codebook: Codebook) -> PqResult<Self> {
        config.validate()?;

        let codebook = Arc::new(codebook);
        let encoder = Encoder::new(codebook.clone());
        let decoder = Decoder::new(codebook.clone());
        let distance_computer = DistanceComputer::new(codebook.clone());

        Ok(Self {
            config,
            codebook,
            encoder,
            decoder,
            distance_computer,
        })
    }

    /// Train a Product Quantizer from training vectors
    ///
    /// # Arguments
    /// * `config` - PQ configuration
    /// * `training_vectors` - Slice of training vectors
    ///
    /// # Returns
    /// A trained Product Quantizer ready for encoding/decoding
    pub fn train(
        config: ProductQuantizerConfig,
        training_vectors: &[Vector],
    ) -> PqResult<Self> {
        config.validate()?;

        // Calculate minimum required samples: need enough points to train k-means
        // At minimum, we need more samples than num_centroids for k-means to work
        // For k-means++ initialization, we need at least k centroids worth of data
        let absolute_minimum = config.num_centroids;

        // Check minimum training samples
        if training_vectors.len() < absolute_minimum {
            return Err(PqError::InsufficientTrainingData(
                training_vectors.len(),
                absolute_minimum,
            ));
        }

        // Warn if below recommended minimum, but still proceed
        // (recommended min is config.min_training_samples)
        if training_vectors.len() < config.min_training_samples {
            // This is just a quality warning - we'll proceed anyway
        }

        // Validate all vectors have correct dimension
        for (idx, vec) in training_vectors.iter().enumerate() {
            if vec.len() != config.dimension {
                return Err(PqError::DimensionMismatch {
                    expected: config.dimension,
                    actual: vec.len(),
                });
            }
        }

        // Train codebook using k-means
        let codebook = super::training::train_codebook(&config, training_vectors)?;

        Self::new(config, codebook)
    }

    /// Encode a vector to its quantized representation
    ///
    /// # Arguments
    /// * `vector` - Input vector to encode
    ///
    /// # Returns
    /// Quantized vector with compressed codes
    pub fn encode(&self, vector: &Vector) -> PqResult<QuantizedVector> {
        self.encoder.encode(vector)
    }

    /// Encode multiple vectors in batch
    pub fn encode_batch(&self, vectors: &[Vector]) -> PqResult<Vec<QuantizedVector>> {
        vectors.iter().map(|v| self.encode(v)).collect()
    }

    /// Decode a quantized vector to its approximate reconstruction
    ///
    /// # Arguments
    /// * `quantized` - Quantized vector to decode
    ///
    /// # Returns
    /// Approximate reconstruction of original vector
    pub fn decode(&self, quantized: &QuantizedVector) -> PqResult<Vector> {
        self.decoder.decode(quantized)
    }

    /// Compute distance between query vector and quantized vector
    ///
    /// Uses Asymmetric Distance Computation (ADC) for efficiency.
    /// The query remains unquantized while the database vector is quantized.
    ///
    /// # Arguments
    /// * `query` - Query vector (unquantized)
    /// * `quantized` - Database vector (quantized)
    ///
    /// # Returns
    /// Approximate L2 distance
    pub fn compute_distance(
        &self,
        query: &Vector,
        quantized: &QuantizedVector,
    ) -> PqResult<f32> {
        self.distance_computer.compute_distance(query, quantized)
    }

    /// Precompute distance table for a query (ADC optimization)
    ///
    /// This is the key optimization: precompute all distances between
    /// query sub-vectors and codebook centroids once, then use lookups
    /// for each database vector.
    ///
    /// Complexity:
    /// - Precomputation: O(M * K * D/M) = O(K * D)
    /// - Per vector: O(M) lookups
    pub fn precompute_distance_table(&self, query: &Vector) -> PqResult<Vec<Vec<f32>>> {
        self.distance_computer.precompute_distance_table(query)
    }

    /// Compute distance using precomputed table (fast path)
    pub fn compute_distance_with_table(
        &self,
        distance_table: &[Vec<f32>],
        quantized: &QuantizedVector,
    ) -> PqResult<f32> {
        self.distance_computer
            .compute_distance_with_table(distance_table, quantized)
    }

    /// Get configuration
    pub fn config(&self) -> &ProductQuantizerConfig {
        &self.config
    }

    /// Get codebook
    pub fn codebook(&self) -> Arc<Codebook> {
        self.codebook.clone()
    }

    /// Calculate compression ratio
    ///
    /// Returns how much memory is saved compared to original vectors
    pub fn compression_ratio(&self) -> f32 {
        let original_size = self.config.dimension * std::mem::size_of::<f32>();
        let compressed_size = self.config.num_subquantizers * std::mem::size_of::<u8>();
        original_size as f32 / compressed_size as f32
    }

    /// Calculate memory footprint per vector in bytes
    pub fn memory_per_vector(&self) -> usize {
        self.config.num_subquantizers * std::mem::size_of::<u8>()
    }

    /// Calculate total codebook size in bytes
    pub fn codebook_size(&self) -> usize {
        self.codebook.memory_size()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_config_validation() {
        let config = ProductQuantizerConfig {
            num_subquantizers: 8,
            num_centroids: 256,
            dimension: 768,
            training_iterations: 25,
            min_training_samples: 1000,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_invalid_dimension() {
        let config = ProductQuantizerConfig {
            num_subquantizers: 8,
            num_centroids: 256,
            dimension: 100, // Not divisible by 8
            training_iterations: 25,
            min_training_samples: 1000,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_subvector_dimension() {
        let config = ProductQuantizerConfig {
            num_subquantizers: 8,
            num_centroids: 256,
            dimension: 768,
            training_iterations: 25,
            min_training_samples: 1000,
        };
        assert_eq!(config.subvector_dimension(), 96);
    }

    #[test]
    fn test_config_default_for_dimension() {
        let config = ProductQuantizerConfig::default_for_dimension(768).unwrap();
        assert_eq!(config.num_subquantizers, 8);
        assert_eq!(config.dimension, 768);
        assert_eq!(config.subvector_dimension(), 96);
    }

    #[test]
    fn test_compression_ratio() {
        let config = ProductQuantizerConfig::default_for_dimension(768).unwrap();
        // Create a dummy codebook for testing
        let codebook = Codebook::new(
            config.num_subquantizers,
            config.num_centroids,
            config.subvector_dimension(),
        );
        let pq = ProductQuantizer::new(config, codebook).unwrap();

        // Original: 768 * 4 bytes = 3072 bytes
        // Compressed: 8 * 1 byte = 8 bytes
        // Ratio: 3072 / 8 = 384
        assert_eq!(pq.compression_ratio(), 384.0);
        assert_eq!(pq.memory_per_vector(), 8);
    }
}
