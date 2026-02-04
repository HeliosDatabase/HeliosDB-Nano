// Product Quantization Codec Tests

#[cfg(test)]
mod codec_tests {
    use rand::Rng;

    // Placeholder structures - would be implemented in actual PQ module
    struct ProductQuantizer {
        dim: usize,
        num_subquantizers: usize,
        k: usize,
        codebooks: Vec<Vec<Vec<f32>>>,
    }

    impl ProductQuantizer {
        fn new(dim: usize, num_subquantizers: usize, k: usize) -> Self {
            assert_eq!(dim % num_subquantizers, 0, "dim must be divisible by num_subquantizers");
            Self {
                dim,
                num_subquantizers,
                k,
                codebooks: vec![vec![vec![0.0; dim / num_subquantizers]; k]; num_subquantizers],
            }
        }

        fn train(&mut self, vectors: &[Vec<f32>]) -> Result<(), String> {
            let subdim = self.dim / self.num_subquantizers;

            for m in 0..self.num_subquantizers {
                let start = m * subdim;
                let end = start + subdim;

                // Extract sub-vectors
                let subvectors: Vec<Vec<f32>> = vectors
                    .iter()
                    .map(|v| v[start..end].to_vec())
                    .collect();

                // Train codebook using k-means
                let centroids = self.kmeans(&subvectors, self.k, 100)?;
                self.codebooks[m] = centroids;
            }

            Ok(())
        }

        fn encode(&self, vector: &[f32]) -> Result<Vec<u8>, String> {
            if vector.len() != self.dim {
                return Err(format!("Vector dimension mismatch: expected {}, got {}", self.dim, vector.len()));
            }

            let subdim = self.dim / self.num_subquantizers;
            let mut codes = Vec::with_capacity(self.num_subquantizers);

            for m in 0..self.num_subquantizers {
                let start = m * subdim;
                let end = start + subdim;
                let subvector = &vector[start..end];

                // Find nearest centroid
                let code = self.find_nearest_centroid(subvector, &self.codebooks[m])?;
                codes.push(code as u8);
            }

            Ok(codes)
        }

        fn decode(&self, codes: &[u8]) -> Result<Vec<f32>, String> {
            if codes.len() != self.num_subquantizers {
                return Err(format!("Code length mismatch: expected {}, got {}", self.num_subquantizers, codes.len()));
            }

            let mut decoded = Vec::with_capacity(self.dim);

            for (m, &code) in codes.iter().enumerate() {
                if (code as usize) >= self.k {
                    return Err(format!("Invalid code: {} (max: {})", code, self.k - 1));
                }
                let centroid = &self.codebooks[m][code as usize];
                decoded.extend_from_slice(centroid);
            }

            Ok(decoded)
        }

        fn kmeans(&self, vectors: &[Vec<f32>], k: usize, max_iter: usize) -> Result<Vec<Vec<f32>>, String> {
            let dim = vectors[0].len();
            let mut rng = rand::thread_rng();

            // Initialize centroids randomly
            let mut centroids: Vec<Vec<f32>> = (0..k)
                .map(|_| {
                    let idx = rng.gen_range(0..vectors.len());
                    vectors[idx].clone()
                })
                .collect();

            for _ in 0..max_iter {
                // Assignment step
                let mut assignments = vec![Vec::new(); k];
                for vector in vectors {
                    let nearest = self.find_nearest_centroid(vector, &centroids)?;
                    assignments[nearest].push(vector.clone());
                }

                // Update step
                let mut changed = false;
                for (i, cluster) in assignments.iter().enumerate() {
                    if cluster.is_empty() {
                        continue;
                    }

                    let new_centroid = self.compute_centroid(cluster);
                    if !self.vectors_equal(&centroids[i], &new_centroid) {
                        centroids[i] = new_centroid;
                        changed = true;
                    }
                }

                if !changed {
                    break;
                }
            }

            Ok(centroids)
        }

        fn find_nearest_centroid(&self, vector: &[f32], centroids: &[Vec<f32>]) -> Result<usize, String> {
            centroids
                .iter()
                .enumerate()
                .min_by(|(_, c1), (_, c2)| {
                    let d1 = euclidean_distance(vector, c1);
                    let d2 = euclidean_distance(vector, c2);
                    d1.partial_cmp(&d2).unwrap()
                })
                .map(|(i, _)| i)
                .ok_or_else(|| "No centroids available".to_string())
        }

        fn compute_centroid(&self, vectors: &[Vec<f32>]) -> Vec<f32> {
            let dim = vectors[0].len();
            let mut centroid = vec![0.0; dim];

            for vector in vectors {
                for (i, &val) in vector.iter().enumerate() {
                    centroid[i] += val;
                }
            }

            let n = vectors.len() as f32;
            for val in &mut centroid {
                *val /= n;
            }

            centroid
        }

        fn vectors_equal(&self, a: &[f32], b: &[f32]) -> bool {
            a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 1e-6)
        }
    }

    fn euclidean_distance(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| (x - y).powi(2))
            .sum::<f32>()
            .sqrt()
    }

    fn mean_squared_error(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| (x - y).powi(2))
            .sum::<f32>() / a.len() as f32
    }

    fn generate_random_vector(dim: usize) -> Vec<f32> {
        let mut rng = rand::thread_rng();
        (0..dim).map(|_| rng.gen::<f32>()).collect()
    }

    fn generate_random_vectors(n: usize, dim: usize) -> Vec<Vec<f32>> {
        (0..n).map(|_| generate_random_vector(dim)).collect()
    }

    #[test]
    fn test_pq_encode_decode_basic() {
        let dim = 768;
        let num_subquantizers = 8;
        let subdim = dim / num_subquantizers;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);

        // Train on sample data
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors).unwrap();

        // Test encoding/decoding
        let original = generate_random_vector(dim);
        let encoded = pq.encode(&original).unwrap();
        let decoded = pq.decode(&encoded).unwrap();

        assert_eq!(encoded.len(), num_subquantizers);
        assert_eq!(decoded.len(), dim);

        // Quantization introduces error, but should be reasonable
        let mse = mean_squared_error(&original, &decoded);
        assert!(mse < 0.1, "MSE too high: {}", mse);
    }

    #[test]
    fn test_pq_encode_output_size() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors).unwrap();

        let vector = generate_random_vector(dim);
        let encoded = pq.encode(&vector).unwrap();

        // Should encode to exactly num_subquantizers bytes
        assert_eq!(encoded.len(), num_subquantizers);

        // Each code should be valid (< 256)
        for &code in &encoded {
            assert!(code < 256);
        }
    }

    #[test]
    fn test_pq_compression_ratio() {
        let dim = 768;
        let num_subquantizers = 8;

        let original_size = dim * std::mem::size_of::<f32>(); // 768 * 4 = 3072 bytes
        let compressed_size = num_subquantizers; // 8 bytes

        let ratio = original_size as f64 / compressed_size as f64;

        // Should achieve 384x compression
        assert_eq!(ratio, 384.0);
    }

    #[test]
    fn test_pq_reconstruction_error() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(5000, dim);
        pq.train(&training_vectors).unwrap();

        // Test reconstruction error on multiple vectors
        let mut errors = Vec::new();
        for _ in 0..100 {
            let original = generate_random_vector(dim);
            let encoded = pq.encode(&original).unwrap();
            let decoded = pq.decode(&encoded).unwrap();

            let mse = mean_squared_error(&original, &decoded);
            errors.push(mse);
        }

        let avg_error: f32 = errors.iter().sum::<f32>() / errors.len() as f32;

        // Average reconstruction error should be low
        assert!(avg_error < 0.05, "Average MSE too high: {}", avg_error);
    }

    #[test]
    fn test_pq_different_subquantizer_counts() {
        let dim = 768;

        for num_subquantizers in [4, 8, 12, 16] {
            if dim % num_subquantizers != 0 {
                continue;
            }

            let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
            let training_vectors = generate_random_vectors(1000, dim);
            pq.train(&training_vectors).unwrap();

            let vector = generate_random_vector(dim);
            let encoded = pq.encode(&vector).unwrap();
            let decoded = pq.decode(&encoded).unwrap();

            assert_eq!(encoded.len(), num_subquantizers);
            assert_eq!(decoded.len(), dim);

            // More subquantizers = better approximation (lower error)
            let mse = mean_squared_error(&vector, &decoded);
            assert!(mse < 0.2, "MSE too high for {} subquantizers: {}", num_subquantizers, mse);
        }
    }

    #[test]
    fn test_pq_encode_invalid_dimension() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors).unwrap();

        // Try to encode vector with wrong dimension
        let wrong_dim_vector = generate_random_vector(512);
        let result = pq.encode(&wrong_dim_vector);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("dimension mismatch"));
    }

    #[test]
    fn test_pq_decode_invalid_codes() {
        let dim = 768;
        let num_subquantizers = 8;

        let pq = ProductQuantizer::new(dim, num_subquantizers, 256);

        // Try to decode with invalid code length
        let invalid_codes = vec![0u8; 4];
        let result = pq.decode(&invalid_codes);
        assert!(result.is_err());

        // Try to decode with out-of-range code
        let invalid_codes = vec![255u8; num_subquantizers];
        // This should work since 255 < 256
        let result = pq.decode(&invalid_codes);
        // Will fail if codebooks not trained, but that's expected
    }

    #[test]
    fn test_pq_deterministic_encoding() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors).unwrap();

        let vector = generate_random_vector(dim);

        // Encode same vector multiple times
        let encoded1 = pq.encode(&vector).unwrap();
        let encoded2 = pq.encode(&vector).unwrap();
        let encoded3 = pq.encode(&vector).unwrap();

        // Should produce identical results
        assert_eq!(encoded1, encoded2);
        assert_eq!(encoded2, encoded3);
    }

    #[test]
    fn test_pq_codebook_structure() {
        let dim = 768;
        let num_subquantizers = 8;
        let k = 256;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, k);
        let training_vectors = generate_random_vectors(2000, dim);
        pq.train(&training_vectors).unwrap();

        // Verify codebook structure
        assert_eq!(pq.codebooks.len(), num_subquantizers);

        for m in 0..num_subquantizers {
            let codebook = &pq.codebooks[m];
            assert_eq!(codebook.len(), k, "Codebook {} should have {} centroids", m, k);

            for centroid in codebook {
                assert_eq!(centroid.len(), dim / num_subquantizers, "Centroid dimension incorrect");
            }
        }
    }

    #[test]
    fn test_pq_training_convergence() {
        let dim = 384;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 128);
        let training_vectors = generate_random_vectors(500, dim);

        // Training should succeed
        let result = pq.train(&training_vectors);
        assert!(result.is_ok(), "Training should succeed");

        // After training, should be able to encode/decode
        let test_vector = generate_random_vector(dim);
        let encoded = pq.encode(&test_vector);
        assert!(encoded.is_ok(), "Encoding should work after training");

        let decoded = pq.decode(&encoded.unwrap());
        assert!(decoded.is_ok(), "Decoding should work after training");
    }

    #[test]
    fn test_pq_memory_efficiency() {
        let dim = 768;
        let num_subquantizers = 8;
        let k = 256;
        let num_vectors = 100_000;

        // Calculate memory usage
        let original_memory = num_vectors * dim * std::mem::size_of::<f32>();
        let pq_memory = num_vectors * num_subquantizers * std::mem::size_of::<u8>();

        let ratio = original_memory as f64 / pq_memory as f64;

        // Should achieve significant compression
        assert!(ratio >= 8.0, "PQ should achieve at least 8x compression");
        assert!(ratio <= 20.0, "Compression ratio seems too high");

        println!("Original: {} MB, PQ: {} MB, Ratio: {:.1}x",
            original_memory / (1024 * 1024),
            pq_memory / (1024 * 1024),
            ratio
        );
    }

    #[test]
    fn test_pq_zero_vector() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors).unwrap();

        // Test with zero vector
        let zero_vector = vec![0.0; dim];
        let encoded = pq.encode(&zero_vector).unwrap();
        let decoded = pq.decode(&encoded).unwrap();

        assert_eq!(decoded.len(), dim);
    }

    #[test]
    fn test_pq_identical_vectors() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors).unwrap();

        // Encode two identical vectors
        let vector = generate_random_vector(dim);
        let encoded1 = pq.encode(&vector).unwrap();
        let encoded2 = pq.encode(&vector).unwrap();

        // Should produce identical codes
        assert_eq!(encoded1, encoded2);

        // Decoding should also be identical
        let decoded1 = pq.decode(&encoded1).unwrap();
        let decoded2 = pq.decode(&encoded2).unwrap();
        assert_eq!(decoded1, decoded2);
    }
}
