//! Training codebooks using k-means clustering
//!
//! This module implements k-means clustering to learn optimal centroids
//! for each sub-quantizer from training data.

#![allow(unused_variables)]

use super::{Codebook, ProductQuantizerConfig, PqError, PqResult};
use crate::vector::Vector;
use rand::seq::SliceRandom;
use rand::thread_rng;

/// Train a codebook from training vectors using k-means
///
/// Process:
/// 1. Split each training vector into M sub-vectors
/// 2. For each sub-quantizer, run k-means on its sub-vectors
/// 3. Store the learned centroids in the codebook
///
/// # Arguments
/// * `config` - PQ configuration
/// * `training_vectors` - Training data (must have correct dimension)
///
/// # Returns
/// Trained codebook with optimal centroids
pub fn train_codebook(
    config: &ProductQuantizerConfig,
    training_vectors: &[Vector],
) -> PqResult<Codebook> {
    config.validate()?;

    // Calculate minimum required samples for k-means
    // Need more samples than num_centroids for k-means to work properly
    let absolute_minimum = config.num_centroids.max(config.num_subquantizers * 10);

    if training_vectors.len() < absolute_minimum {
        return Err(PqError::InsufficientTrainingData(
            training_vectors.len(),
            absolute_minimum,
        ));
    }

    // Warn if below recommended minimum, but proceed anyway
    if training_vectors.len() < config.min_training_samples {
        // This is just a quality warning - continue anyway
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

    let num_subquantizers = config.num_subquantizers;
    let subvector_dim = config.subvector_dimension();
    let num_centroids = config.num_centroids;

    let mut codebook = Codebook::new(num_subquantizers, num_centroids, subvector_dim);

    // Train each sub-quantizer independently
    for sq_idx in 0..num_subquantizers {
        let start = sq_idx * subvector_dim;
        let end = start + subvector_dim;

        // Extract all sub-vectors for this sub-quantizer
        let subvectors: Vec<Vec<f32>> = training_vectors
            .iter()
            .map(|v| v[start..end].to_vec())
            .collect();

        // Run k-means to find centroids
        let centroids = kmeans(
            &subvectors,
            num_centroids,
            config.training_iterations,
        )?;

        // Store centroids in codebook
        for (c_idx, centroid) in centroids.into_iter().enumerate() {
            codebook.set_centroid(sq_idx, c_idx, centroid)?;
        }
    }

    codebook.validate()?;
    Ok(codebook)
}

/// K-means clustering implementation
///
/// # Arguments
/// * `data` - Input vectors to cluster
/// * `k` - Number of clusters (centroids)
/// * `max_iterations` - Maximum k-means iterations
///
/// # Returns
/// K centroids learned from data
fn kmeans(data: &[Vector], k: usize, max_iterations: usize) -> PqResult<Vec<Vector>> {
    if data.is_empty() {
        return Err(PqError::TrainingError("No data provided".to_string()));
    }

    if k == 0 {
        return Err(PqError::TrainingError("k must be > 0".to_string()));
    }

    if k > data.len() {
        return Err(PqError::TrainingError(format!(
            "k ({}) cannot be larger than number of data points ({})",
            k,
            data.len()
        )));
    }

    let dimension = data[0].len();

    // Initialize centroids using k-means++ for better convergence
    let mut centroids = kmeans_plus_plus_init(data, k, dimension)?;

    // K-means iterations
    for _iteration in 0..max_iterations {
        // Assignment step: assign each point to nearest centroid
        let mut clusters: Vec<Vec<usize>> = vec![Vec::new(); k];

        for (point_idx, point) in data.iter().enumerate() {
            let nearest = find_nearest_centroid(point, &centroids);
            clusters[nearest].push(point_idx);
        }

        // Update step: recompute centroids
        let mut converged = true;
        for (cluster_idx, cluster_points) in clusters.iter().enumerate() {
            if cluster_points.is_empty() {
                // Empty cluster: reinitialize with a random point
                let mut rng = thread_rng();
                if let Some(&random_point_idx) = data.iter().enumerate()
                    .map(|(idx, _)| idx)
                    .collect::<Vec<_>>()
                    .choose(&mut rng)
                {
                    centroids[cluster_idx] = data[random_point_idx].clone();
                }
                converged = false;
                continue;
            }

            // Compute mean of cluster points
            let new_centroid = compute_mean(data, cluster_points, dimension);

            // Check if centroid changed
            if !vectors_equal(&centroids[cluster_idx], &new_centroid, 1e-6) {
                converged = false;
            }

            centroids[cluster_idx] = new_centroid;
        }

        if converged {
            break;
        }
    }

    // Final validation of centroids
    for (idx, centroid) in centroids.iter().enumerate() {
        if centroid.len() != dimension {
            return Err(PqError::TrainingError(format!(
                "k-means produced centroid {} with wrong dimension: expected {}, got {}",
                idx, dimension, centroid.len()
            )));
        }
        for (dim, &value) in centroid.iter().enumerate() {
            if !value.is_finite() {
                return Err(PqError::TrainingError(format!(
                    "k-means produced non-finite value at centroid {}, dimension {}",
                    idx, dim
                )));
            }
        }
    }

    Ok(centroids)
}

/// K-means++ initialization for better centroid seeding
///
/// Algorithm:
/// 1. Choose first centroid uniformly at random
/// 2. For remaining centroids, choose points with probability proportional
///    to squared distance from nearest existing centroid
fn kmeans_plus_plus_init(
    data: &[Vector],
    k: usize,
    dimension: usize,
) -> PqResult<Vec<Vector>> {
    let mut rng = thread_rng();
    let mut centroids = Vec::with_capacity(k);

    // Choose first centroid randomly
    let indices: Vec<usize> = (0..data.len()).collect();
    let first_idx = indices
        .choose(&mut rng)
        .ok_or_else(|| PqError::TrainingError("No data points".to_string()))?;
    centroids.push(data[*first_idx].clone());

    // Choose remaining centroids
    for centroid_idx in 1..k {
        // Compute distance of each point to nearest centroid
        let mut distances: Vec<f32> = data
            .iter()
            .map(|point| {
                let nearest_idx = find_nearest_centroid(point, &centroids);
                l2_distance_squared(point, &centroids[nearest_idx])
            })
            .collect();

        // Convert to probabilities (proportional to squared distance)
        let total: f32 = distances.iter().sum();
        if total == 0.0 || !total.is_finite() {
            // All points are already centroids or numerical issue, choose randomly
            // Avoid selecting already chosen centroids
            let mut attempts = 0;
            let max_attempts = data.len() * 2;
            loop {
                if let Some(&idx) = (0..data.len()).collect::<Vec<_>>().choose(&mut rng) {
                    // Check if this point is already a centroid
                    let is_duplicate = centroids.iter().any(|c| {
                        c.iter().zip(data[idx].iter()).all(|(a, b)| (a - b).abs() < 1e-9)
                    });
                    if !is_duplicate || attempts > max_attempts {
                        centroids.push(data[idx].clone());
                        break;
                    }
                }
                attempts += 1;
                if attempts > max_attempts {
                    // Give up and just add a duplicate
                    centroids.push(data[0].clone());
                    break;
                }
            }
            continue;
        }

        // Normalize to probabilities
        for dist in &mut distances {
            *dist /= total;
        }

        // Choose next centroid based on probability distribution
        let mut cumulative = 0.0;
        let rand_val: f32 = rand::random();
        let mut selected = false;

        for (idx, &prob) in distances.iter().enumerate() {
            cumulative += prob;
            if rand_val <= cumulative {
                centroids.push(data[idx].clone());
                selected = true;
                break;
            }
        }

        // Fallback if no point was selected (due to floating-point precision)
        if !selected {
            // Choose the point with maximum distance
            let max_idx = distances
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(idx, _)| idx)
                .unwrap_or(centroid_idx % data.len());
            centroids.push(data[max_idx].clone());
        }
    }

    // Validate that we have k centroids
    if centroids.len() != k {
        return Err(PqError::TrainingError(format!(
            "k-means++ initialization failed: expected {} centroids, got {}",
            k,
            centroids.len()
        )));
    }

    // Validate that all centroids are finite
    for (idx, centroid) in centroids.iter().enumerate() {
        for (dim, &value) in centroid.iter().enumerate() {
            if !value.is_finite() {
                return Err(PqError::TrainingError(format!(
                    "k-means++ initialization produced non-finite value at centroid {}, dimension {}",
                    idx, dim
                )));
            }
        }
    }

    Ok(centroids)
}

/// Find index of nearest centroid to a point
fn find_nearest_centroid(point: &[f32], centroids: &[Vector]) -> usize {
    let mut min_distance = f32::MAX;
    let mut min_idx = 0;

    for (idx, centroid) in centroids.iter().enumerate() {
        let distance = l2_distance_squared(point, centroid);
        if distance < min_distance {
            min_distance = distance;
            min_idx = idx;
        }
    }

    min_idx
}

/// Compute mean of a set of points
fn compute_mean(data: &[Vector], indices: &[usize], dimension: usize) -> Vector {
    let mut mean = vec![0.0; dimension];
    let count = indices.len() as f32;

    for &idx in indices {
        for (dim, value) in data[idx].iter().enumerate() {
            mean[dim] += value;
        }
    }

    for value in &mut mean {
        *value /= count;
    }

    mean
}

/// Check if two vectors are equal within tolerance
fn vectors_equal(a: &[f32], b: &[f32], tolerance: f32) -> bool {
    if a.len() != b.len() {
        return false;
    }

    a.iter()
        .zip(b.iter())
        .all(|(x, y)| (x - y).abs() < tolerance)
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

    #[test]
    fn test_kmeans_simple() {
        // Create two distinct clusters
        let data = vec![
            vec![0.0, 0.0],
            vec![0.1, 0.1],
            vec![0.0, 0.1],
            vec![10.0, 10.0],
            vec![10.1, 10.1],
            vec![10.0, 10.1],
        ];

        let centroids = kmeans(&data, 2, 10).unwrap();
        assert_eq!(centroids.len(), 2);

        // Centroids should be roughly [0.05, 0.07] and [10.05, 10.07]
        // Check that they're well separated
        let dist = l2_distance_squared(&centroids[0], &centroids[1]);
        assert!(dist > 100.0); // Should be far apart
    }

    #[test]
    fn test_kmeans_insufficient_data() {
        let data = vec![vec![1.0, 2.0]];
        let result = kmeans(&data, 5, 10);
        assert!(result.is_err());
    }

    #[test]
    fn test_train_codebook_basic() {
        // Create simple 4D training data (will split into 2 sub-vectors of 2D)
        let mut training_data = Vec::new();

        // Cluster 1: [0, 0, ?, ?]
        for _ in 0..100 {
            training_data.push(vec![0.0, 0.0, 5.0, 5.0]);
        }

        // Cluster 2: [1, 1, ?, ?]
        for _ in 0..100 {
            training_data.push(vec![1.0, 1.0, 6.0, 6.0]);
        }

        let config = ProductQuantizerConfig {
            num_subquantizers: 2,
            num_centroids: 2,
            dimension: 4,
            training_iterations: 25,
            min_training_samples: 100,
        };

        let codebook = train_codebook(&config, &training_data).unwrap();

        assert_eq!(codebook.num_subquantizers(), 2);
        assert_eq!(codebook.num_centroids(), 2);
        assert_eq!(codebook.subvector_dimension(), 2);
    }

    #[test]
    fn test_train_codebook_dimension_mismatch() {
        let training_data = vec![
            vec![1.0, 2.0, 3.0],
            vec![4.0, 5.0], // Wrong dimension!
        ];

        let config = ProductQuantizerConfig {
            num_subquantizers: 2,
            num_centroids: 2,
            dimension: 4,
            training_iterations: 25,
            min_training_samples: 1,
        };

        let result = train_codebook(&config, &training_data);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_nearest_centroid() {
        let centroids = vec![
            vec![0.0, 0.0],
            vec![1.0, 1.0],
            vec![2.0, 2.0],
        ];

        let point = vec![0.9, 0.9];
        let nearest = find_nearest_centroid(&point, &centroids);
        assert_eq!(nearest, 1); // Closest to [1.0, 1.0]
    }

    #[test]
    fn test_compute_mean() {
        let data = vec![
            vec![0.0, 0.0],
            vec![2.0, 2.0],
            vec![4.0, 4.0],
        ];

        let indices = vec![0, 1, 2];
        let mean = compute_mean(&data, &indices, 2);

        assert_eq!(mean, vec![2.0, 2.0]);
    }

    #[test]
    fn test_vectors_equal() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];
        assert!(vectors_equal(&a, &b, 0.001));

        let c = vec![1.0, 2.1, 3.0];
        assert!(!vectors_equal(&a, &c, 0.001));
    }
}
