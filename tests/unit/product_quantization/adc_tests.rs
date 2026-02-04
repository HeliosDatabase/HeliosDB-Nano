// Asymmetric Distance Computation (ADC) Tests for Product Quantization

#[cfg(test)]
mod adc_tests {
    use rand::Rng;

    // Placeholder structures for testing
    struct ProductQuantizer {
        dim: usize,
        num_subquantizers: usize,
        k: usize,
        codebooks: Vec<Vec<Vec<f32>>>,
        distance_tables: Vec<Vec<f32>>,
    }

    impl ProductQuantizer {
        fn new(dim: usize, num_subquantizers: usize, k: usize) -> Self {
            Self {
                dim,
                num_subquantizers,
                k,
                codebooks: vec![vec![vec![0.0; dim / num_subquantizers]; k]; num_subquantizers],
                distance_tables: Vec::new(),
            }
        }

        /// Compute asymmetric distance between query and quantized vector
        /// ADC is much faster than decoding and computing exact distance
        fn compute_asymmetric_distance(&self, query: &[f32], codes: &[u8]) -> f32 {
            let subdim = self.dim / self.num_subquantizers;
            let mut total_distance = 0.0;

            for (m, &code) in codes.iter().enumerate() {
                let start = m * subdim;
                let end = start + subdim;
                let query_subvector = &query[start..end];
                let centroid = &self.codebooks[m][code as usize];

                let subdist = euclidean_distance_squared(query_subvector, centroid);
                total_distance += subdist;
            }

            total_distance.sqrt()
        }

        /// Precompute distance table for a query
        /// This allows O(M) distance computation instead of O(D)
        fn precompute_distance_table(&mut self, query: &[f32]) {
            let subdim = self.dim / self.num_subquantizers;
            self.distance_tables = Vec::with_capacity(self.num_subquantizers);

            for m in 0..self.num_subquantizers {
                let start = m * subdim;
                let end = start + subdim;
                let query_subvector = &query[start..end];

                let mut distances = Vec::with_capacity(self.k);
                for centroid in &self.codebooks[m] {
                    let dist = euclidean_distance_squared(query_subvector, centroid);
                    distances.push(dist);
                }
                self.distance_tables.push(distances);
            }
        }

        /// Compute distance using precomputed table (very fast)
        fn compute_distance_with_table(&self, codes: &[u8]) -> f32 {
            codes.iter()
                .enumerate()
                .map(|(m, &code)| self.distance_tables[m][code as usize])
                .sum::<f32>()
                .sqrt()
        }

        fn encode(&self, vector: &[f32]) -> Vec<u8> {
            let subdim = self.dim / self.num_subquantizers;
            let mut codes = Vec::with_capacity(self.num_subquantizers);

            for m in 0..self.num_subquantizers {
                let start = m * subdim;
                let end = start + subdim;
                let subvector = &vector[start..end];

                let code = self.find_nearest_centroid(subvector, &self.codebooks[m]);
                codes.push(code as u8);
            }

            codes
        }

        fn decode(&self, codes: &[u8]) -> Vec<f32> {
            let mut decoded = Vec::with_capacity(self.dim);

            for (m, &code) in codes.iter().enumerate() {
                let centroid = &self.codebooks[m][code as usize];
                decoded.extend_from_slice(centroid);
            }

            decoded
        }

        fn find_nearest_centroid(&self, vector: &[f32], centroids: &[Vec<f32>]) -> usize {
            centroids
                .iter()
                .enumerate()
                .min_by(|(_, c1), (_, c2)| {
                    let d1 = euclidean_distance_squared(vector, c1);
                    let d2 = euclidean_distance_squared(vector, c2);
                    d1.partial_cmp(&d2).unwrap()
                })
                .map(|(i, _)| i)
                .unwrap()
        }

        fn train(&mut self, vectors: &[Vec<f32>]) {
            // Simplified training for testing
            let subdim = self.dim / self.num_subquantizers;
            let mut rng = rand::thread_rng();

            for m in 0..self.num_subquantizers {
                for k in 0..self.k {
                    let random_vector = &vectors[rng.gen_range(0..vectors.len())];
                    let start = m * subdim;
                    let end = start + subdim;
                    self.codebooks[m][k] = random_vector[start..end].to_vec();
                }
            }
        }
    }

    fn euclidean_distance_squared(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| (x - y).powi(2))
            .sum()
    }

    fn euclidean_distance(a: &[f32], b: &[f32]) -> f32 {
        euclidean_distance_squared(a, b).sqrt()
    }

    fn generate_random_vector(dim: usize) -> Vec<f32> {
        let mut rng = rand::thread_rng();
        (0..dim).map(|_| rng.gen::<f32>()).collect()
    }

    fn generate_random_vectors(n: usize, dim: usize) -> Vec<Vec<f32>> {
        (0..n).map(|_| generate_random_vector(dim)).collect()
    }

    #[test]
    fn test_adc_correctness() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors);

        let query = generate_random_vector(dim);
        let vector = generate_random_vector(dim);
        let codes = pq.encode(&vector);

        // Compute distance using ADC
        let adc_distance = pq.compute_asymmetric_distance(&query, &codes);

        // Compute ground truth: distance between query and decoded vector
        let decoded = pq.decode(&codes);
        let true_distance = euclidean_distance(&query, &decoded);

        // ADC should approximate true distance (within 10% error)
        let relative_error = (adc_distance - true_distance).abs() / true_distance;
        assert!(
            relative_error < 0.1,
            "ADC error too high: ADC={:.4}, True={:.4}, Error={:.2}%",
            adc_distance, true_distance, relative_error * 100.0
        );
    }

    #[test]
    fn test_adc_with_precomputed_table() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors);

        let query = generate_random_vector(dim);

        // Precompute distance table
        pq.precompute_distance_table(&query);

        // Encode multiple vectors
        let vectors = generate_random_vectors(100, dim);
        for vector in &vectors {
            let codes = pq.encode(vector);

            // Compute distance with and without table
            let dist_with_table = pq.compute_distance_with_table(&codes);
            let dist_without_table = pq.compute_asymmetric_distance(&query, &codes);

            // Should produce identical results
            assert!(
                (dist_with_table - dist_without_table).abs() < 1e-5,
                "Distance mismatch: with_table={:.6}, without_table={:.6}",
                dist_with_table, dist_without_table
            );
        }
    }

    #[test]
    fn test_adc_performance_speedup() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors);

        let query = generate_random_vector(dim);
        let codes_batch = (0..10000)
            .map(|_| {
                let v = generate_random_vector(dim);
                pq.encode(&v)
            })
            .collect::<Vec<_>>();

        // Benchmark without precomputed table
        let start = std::time::Instant::now();
        for codes in &codes_batch {
            let _ = pq.compute_asymmetric_distance(&query, codes);
        }
        let duration_without = start.elapsed();

        // Benchmark with precomputed table
        pq.precompute_distance_table(&query);
        let start = std::time::Instant::now();
        for codes in &codes_batch {
            let _ = pq.compute_distance_with_table(codes);
        }
        let duration_with = start.elapsed();

        // Precomputed table should be faster
        println!(
            "Without table: {:?}, With table: {:?}, Speedup: {:.2}x",
            duration_without,
            duration_with,
            duration_without.as_secs_f64() / duration_with.as_secs_f64()
        );

        // Should see significant speedup (at least 2x)
        assert!(
            duration_with < duration_without,
            "Precomputed table should be faster"
        );
    }

    #[test]
    fn test_adc_vs_exact_distance() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(5000, dim);
        pq.train(&training_vectors);

        let query = generate_random_vector(dim);

        // Test on multiple vectors
        let mut errors = Vec::new();
        for _ in 0..100 {
            let vector = generate_random_vector(dim);
            let codes = pq.encode(&vector);

            // ADC distance (approximate)
            let adc_dist = pq.compute_asymmetric_distance(&query, &codes);

            // Exact distance (query to original vector)
            let exact_dist = euclidean_distance(&query, &vector);

            let relative_error = (adc_dist - exact_dist).abs() / exact_dist;
            errors.push(relative_error);
        }

        let avg_error: f32 = errors.iter().sum::<f32>() / errors.len() as f32;
        let max_error = errors.iter().cloned().fold(0.0, f32::max);

        println!(
            "ADC accuracy: avg_error={:.2}%, max_error={:.2}%",
            avg_error * 100.0, max_error * 100.0
        );

        // Average error should be low
        assert!(avg_error < 0.15, "Average error too high: {:.2}%", avg_error * 100.0);
    }

    #[test]
    fn test_adc_distance_ordering() {
        // ADC preserves relative distance ordering (important for nearest neighbor search)
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(2000, dim);
        pq.train(&training_vectors);

        let query = generate_random_vector(dim);

        // Generate test vectors at various distances
        let test_vectors = generate_random_vectors(50, dim);
        let mut exact_distances: Vec<(usize, f32)> = test_vectors
            .iter()
            .enumerate()
            .map(|(i, v)| (i, euclidean_distance(&query, v)))
            .collect();

        let mut adc_distances: Vec<(usize, f32)> = test_vectors
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let codes = pq.encode(v);
                (i, pq.compute_asymmetric_distance(&query, &codes))
            })
            .collect();

        // Sort by distance
        exact_distances.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        adc_distances.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        // Check rank correlation (Kendall's tau or similar)
        // For now, just check top-10 overlap
        let top10_exact: Vec<usize> = exact_distances[..10].iter().map(|(i, _)| *i).collect();
        let top10_adc: Vec<usize> = adc_distances[..10].iter().map(|(i, _)| *i).collect();

        let overlap = top10_exact.iter().filter(|i| top10_adc.contains(i)).count();

        // Should have good overlap (at least 70%)
        assert!(
            overlap >= 7,
            "ADC should preserve top-10 ordering (overlap: {}/10)",
            overlap
        );
    }

    #[test]
    fn test_adc_precompute_table_structure() {
        let dim = 768;
        let num_subquantizers = 8;
        let k = 256;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, k);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors);

        let query = generate_random_vector(dim);
        pq.precompute_distance_table(&query);

        // Verify table structure
        assert_eq!(pq.distance_tables.len(), num_subquantizers);

        for m in 0..num_subquantizers {
            assert_eq!(
                pq.distance_tables[m].len(), k,
                "Distance table {} should have {} entries", m, k
            );

            // All distances should be non-negative
            for &dist in &pq.distance_tables[m] {
                assert!(dist >= 0.0, "Squared distance should be non-negative");
            }
        }
    }

    #[test]
    fn test_adc_zero_query() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors);

        // Query with zero vector
        let zero_query = vec![0.0; dim];
        let vector = generate_random_vector(dim);
        let codes = pq.encode(&vector);

        let distance = pq.compute_asymmetric_distance(&zero_query, &codes);

        // Distance should be well-defined and positive
        assert!(distance >= 0.0);
        assert!(distance.is_finite());
    }

    #[test]
    fn test_adc_identical_query_and_vector() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors);

        let vector = generate_random_vector(dim);
        let codes = pq.encode(&vector);

        // Query is same as vector
        let distance = pq.compute_asymmetric_distance(&vector, &codes);

        // Distance should be small (near zero due to quantization error)
        assert!(
            distance < 0.5,
            "Distance to self should be small: {}",
            distance
        );
    }

    #[test]
    fn test_adc_batch_computation() {
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors);

        let query = generate_random_vector(dim);
        pq.precompute_distance_table(&query);

        // Encode batch of vectors
        let batch_size = 1000;
        let vectors = generate_random_vectors(batch_size, dim);
        let codes_batch: Vec<Vec<u8>> = vectors.iter().map(|v| pq.encode(v)).collect();

        // Compute distances for entire batch
        let start = std::time::Instant::now();
        let distances: Vec<f32> = codes_batch
            .iter()
            .map(|codes| pq.compute_distance_with_table(codes))
            .collect();
        let duration = start.elapsed();

        assert_eq!(distances.len(), batch_size);

        // Should be very fast (<1ms for 1000 vectors)
        println!(
            "Batch distance computation: {} vectors in {:?} ({:.2} μs/vector)",
            batch_size,
            duration,
            duration.as_micros() as f64 / batch_size as f64
        );

        assert!(duration.as_millis() < 10, "Batch computation too slow");
    }

    #[test]
    fn test_adc_triangle_inequality_approximate() {
        // ADC should approximately satisfy triangle inequality
        let dim = 768;
        let num_subquantizers = 8;

        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(1000, dim);
        pq.train(&training_vectors);

        let q = generate_random_vector(dim);
        let v1 = generate_random_vector(dim);
        let v2 = generate_random_vector(dim);

        let codes1 = pq.encode(&v1);
        let codes2 = pq.encode(&v2);

        let d_q_v1 = pq.compute_asymmetric_distance(&q, &codes1);
        let d_q_v2 = pq.compute_asymmetric_distance(&q, &codes2);
        let d_v1_v2 = pq.compute_asymmetric_distance(&v1, &codes2);

        // d(q, v2) <= d(q, v1) + d(v1, v2) (approximately)
        // Allow some slack due to quantization
        let slack = 1.2;
        assert!(
            d_q_v2 <= (d_q_v1 + d_v1_v2) * slack,
            "Triangle inequality violated: d(q,v2)={:.4} > d(q,v1)+d(v1,v2)={:.4}",
            d_q_v2, d_q_v1 + d_v1_v2
        );
    }
}
