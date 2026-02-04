// K-means Clustering Tests for Product Quantization

#[cfg(test)]
mod kmeans_tests {
    use rand::Rng;

    // Placeholder types - these would be implemented in the actual PQ module
    struct KMeans {
        k: usize,
        max_iterations: usize,
    }

    impl KMeans {
        fn new(k: usize, max_iterations: usize) -> Self {
            Self { k, max_iterations }
        }

        fn fit(&self, vectors: &[Vec<f32>]) -> (Vec<Vec<f32>>, Vec<usize>) {
            // Simplified k-means implementation for testing
            let dim = vectors[0].len();
            let mut centroids = self.initialize_centroids(vectors);
            let mut labels = vec![0; vectors.len()];
            let mut changed = true;
            let mut iteration = 0;

            while changed && iteration < self.max_iterations {
                changed = false;
                iteration += 1;

                // Assignment step
                for (i, vector) in vectors.iter().enumerate() {
                    let new_label = self.find_nearest_centroid(vector, &centroids);
                    if labels[i] != new_label {
                        labels[i] = new_label;
                        changed = true;
                    }
                }

                // Update step
                centroids = self.update_centroids(vectors, &labels);
            }

            (centroids, labels)
        }

        fn initialize_centroids(&self, vectors: &[Vec<f32>]) -> Vec<Vec<f32>> {
            // Use k-means++ initialization
            let mut rng = rand::thread_rng();
            let mut centroids = Vec::new();

            // First centroid: random
            let first_idx = rng.gen_range(0..vectors.len());
            centroids.push(vectors[first_idx].clone());

            // Remaining centroids: weighted by distance
            for _ in 1..self.k {
                let mut distances: Vec<f32> = vectors
                    .iter()
                    .map(|v| {
                        centroids
                            .iter()
                            .map(|c| euclidean_distance(v, c))
                            .fold(f32::INFINITY, f32::min)
                    })
                    .collect();

                let sum: f32 = distances.iter().sum();
                let threshold: f32 = rng.gen::<f32>() * sum;

                let mut cumsum = 0.0;
                let mut selected_idx = 0;
                for (i, &dist) in distances.iter().enumerate() {
                    cumsum += dist;
                    if cumsum >= threshold {
                        selected_idx = i;
                        break;
                    }
                }

                centroids.push(vectors[selected_idx].clone());
            }

            centroids
        }

        fn find_nearest_centroid(&self, vector: &[f32], centroids: &[Vec<f32>]) -> usize {
            centroids
                .iter()
                .enumerate()
                .min_by(|(_, c1), (_, c2)| {
                    let d1 = euclidean_distance(vector, c1);
                    let d2 = euclidean_distance(vector, c2);
                    d1.partial_cmp(&d2).unwrap()
                })
                .map(|(i, _)| i)
                .unwrap()
        }

        fn update_centroids(&self, vectors: &[Vec<f32>], labels: &[usize]) -> Vec<Vec<f32>> {
            let dim = vectors[0].len();
            let mut centroids = vec![vec![0.0; dim]; self.k];
            let mut counts = vec![0; self.k];

            for (vector, &label) in vectors.iter().zip(labels.iter()) {
                for (i, &val) in vector.iter().enumerate() {
                    centroids[label][i] += val;
                }
                counts[label] += 1;
            }

            for (i, centroid) in centroids.iter_mut().enumerate() {
                if counts[i] > 0 {
                    for val in centroid.iter_mut() {
                        *val /= counts[i] as f32;
                    }
                }
            }

            centroids
        }
    }

    fn euclidean_distance(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| (x - y).powi(2))
            .sum::<f32>()
            .sqrt()
    }

    fn generate_random_vectors(n: usize, dim: usize) -> Vec<Vec<f32>> {
        let mut rng = rand::thread_rng();
        (0..n)
            .map(|_| (0..dim).map(|_| rng.gen::<f32>()).collect())
            .collect()
    }

    #[test]
    fn test_kmeans_basic_clustering() {
        let vectors = generate_random_vectors(1000, 96);
        let k = 256;

        let kmeans = KMeans::new(k, 100);
        let (centroids, labels) = kmeans.fit(&vectors);

        assert_eq!(centroids.len(), k);
        assert_eq!(labels.len(), vectors.len());

        // All labels should be valid
        for &label in &labels {
            assert!(label < k);
        }
    }

    #[test]
    fn test_kmeans_convergence() {
        let vectors = generate_random_vectors(500, 96);
        let k = 256;

        let kmeans = KMeans::new(k, 100);
        let (centroids, labels) = kmeans.fit(&vectors);

        // Verify convergence: all vectors assigned to nearest centroid
        for (i, &label) in labels.iter().enumerate() {
            let assigned_centroid = &centroids[label];
            let distance = euclidean_distance(&vectors[i], assigned_centroid);

            // Check all other centroids are farther or equal
            for (j, centroid) in centroids.iter().enumerate() {
                if j != label {
                    let other_distance = euclidean_distance(&vectors[i], centroid);
                    assert!(
                        distance <= other_distance + 1e-5,
                        "Vector {} not assigned to nearest centroid", i
                    );
                }
            }
        }
    }

    #[test]
    fn test_kmeans_cluster_balance() {
        let vectors = generate_random_vectors(1000, 96);
        let k = 10; // Use fewer clusters for balance test

        let kmeans = KMeans::new(k, 100);
        let (_, labels) = kmeans.fit(&vectors);

        // Count cluster sizes
        let mut cluster_sizes = vec![0; k];
        for &label in &labels {
            cluster_sizes[label] += 1;
        }

        // No cluster should be empty
        for (i, &size) in cluster_sizes.iter().enumerate() {
            assert!(size > 0, "Cluster {} is empty", i);
        }

        // Clusters should be somewhat balanced (no cluster > 50% of data)
        let max_size = *cluster_sizes.iter().max().unwrap();
        assert!(
            max_size < vectors.len() / 2,
            "Clusters are too imbalanced"
        );
    }

    #[test]
    fn test_kmeans_with_gaussian_data() {
        // Generate data from 3 known Gaussians
        let mut rng = rand::thread_rng();
        let mut vectors = Vec::new();

        // Cluster 1: center at (0, 0, ...)
        for _ in 0..100 {
            let v: Vec<f32> = (0..96).map(|_| rng.gen::<f32>() * 0.1).collect();
            vectors.push(v);
        }

        // Cluster 2: center at (1, 1, ...)
        for _ in 0..100 {
            let v: Vec<f32> = (0..96).map(|_| 1.0 + rng.gen::<f32>() * 0.1).collect();
            vectors.push(v);
        }

        // Cluster 3: center at (-1, -1, ...)
        for _ in 0..100 {
            let v: Vec<f32> = (0..96).map(|_| -1.0 + rng.gen::<f32>() * 0.1).collect();
            vectors.push(v);
        }

        let kmeans = KMeans::new(3, 100);
        let (centroids, labels) = kmeans.fit(&vectors);

        // Should find 3 clusters
        let unique_labels: std::collections::HashSet<_> = labels.iter().cloned().collect();
        assert_eq!(unique_labels.len(), 3, "Should find exactly 3 clusters");

        // Each cluster should have roughly 100 vectors
        let mut cluster_sizes = vec![0; 3];
        for &label in &labels {
            cluster_sizes[label] += 1;
        }

        for &size in &cluster_sizes {
            assert!(
                size >= 80 && size <= 120,
                "Cluster sizes should be roughly equal"
            );
        }
    }

    #[test]
    fn test_kmeans_deterministic_with_same_seed() {
        let vectors = generate_random_vectors(500, 96);

        // Run k-means multiple times (in real implementation, we'd use a seed)
        let kmeans = KMeans::new(10, 100);
        let (centroids1, labels1) = kmeans.fit(&vectors);

        // Note: In a real implementation, we'd set a random seed to ensure determinism
        // For now, we just verify the output structure is consistent
        assert_eq!(centroids1.len(), 10);
        assert_eq!(labels1.len(), vectors.len());
    }

    #[test]
    fn test_kmeans_handles_empty_clusters() {
        // Test with k much larger than natural clusters
        let mut vectors = Vec::new();
        let mut rng = rand::thread_rng();

        // Only 2 tight clusters
        for _ in 0..100 {
            vectors.push((0..96).map(|_| rng.gen::<f32>() * 0.01).collect());
        }
        for _ in 0..100 {
            vectors.push((0..96).map(|_| 1.0 + rng.gen::<f32>() * 0.01).collect());
        }

        let kmeans = KMeans::new(10, 100);
        let (centroids, labels) = kmeans.fit(&vectors);

        // Should still produce valid output
        assert_eq!(centroids.len(), 10);
        assert_eq!(labels.len(), vectors.len());

        // Some clusters may be empty or very small
        let mut cluster_sizes = vec![0; 10];
        for &label in &labels {
            cluster_sizes[label] += 1;
        }

        // At least 2 clusters should have data
        let non_empty = cluster_sizes.iter().filter(|&&s| s > 0).count();
        assert!(non_empty >= 2, "Should have at least 2 non-empty clusters");
    }

    #[test]
    fn test_kmeans_performance() {
        // Performance test: should complete in reasonable time
        let start = std::time::Instant::now();

        let vectors = generate_random_vectors(10000, 96);
        let kmeans = KMeans::new(256, 50);
        let (centroids, labels) = kmeans.fit(&vectors);

        let duration = start.elapsed();

        assert_eq!(centroids.len(), 256);
        assert_eq!(labels.len(), 10000);

        // Should complete in under 5 seconds for 10K vectors
        assert!(
            duration.as_secs() < 5,
            "K-means too slow: {:?}",
            duration
        );
    }

    #[test]
    fn test_euclidean_distance_correctness() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 5.0, 6.0];

        let dist = euclidean_distance(&a, &b);

        // sqrt((4-1)^2 + (5-2)^2 + (6-3)^2) = sqrt(9+9+9) = sqrt(27) ≈ 5.196
        let expected = 5.196152;
        assert!((dist - expected).abs() < 0.001);
    }

    #[test]
    fn test_euclidean_distance_identical_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];

        let dist = euclidean_distance(&a, &b);
        assert_eq!(dist, 0.0);
    }
}
