//! Vector Partitioner - Tier 3 Sharding (Vector-Aware)
//!
//! Partitions vectors by similarity for optimized ANN query locality.
//! Uses k-means clustering to group similar vectors on the same shard.

use super::{ReplicationError, Result};
use std::collections::HashMap;
use uuid::Uuid;

/// A vector for partitioning
pub type Vector = Vec<f32>;

/// Vector partition assignment
#[derive(Debug, Clone)]
pub struct PartitionAssignment {
    /// Vector ID
    pub vector_id: Uuid,
    /// Assigned partition (shard)
    pub partition_id: usize,
    /// Distance to partition centroid
    pub distance_to_centroid: f32,
    /// Confidence score (0.0 to 1.0)
    pub confidence: f32,
}

/// Partition information
#[derive(Debug, Clone)]
pub struct PartitionInfo {
    /// Partition ID
    pub id: usize,
    /// Shard ID (maps to physical node)
    pub shard_id: Uuid,
    /// Centroid vector
    pub centroid: Vector,
    /// Number of vectors in this partition
    pub vector_count: usize,
    /// Average distance to centroid
    pub avg_distance: f32,
}

/// Distance metric for vector comparison
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistanceMetric {
    /// Euclidean distance (L2)
    Euclidean,
    /// Cosine similarity (1 - cosine)
    Cosine,
    /// Inner product (for normalized vectors)
    InnerProduct,
    /// Manhattan distance (L1)
    Manhattan,
}

/// Vector Partitioner
pub struct VectorPartitioner {
    /// Number of partitions
    partition_count: usize,
    /// Partition centroids
    centroids: Vec<Vector>,
    /// Partition to shard mapping
    partition_to_shard: HashMap<usize, Uuid>,
    /// Distance metric
    distance_metric: DistanceMetric,
    /// Vector dimension
    dimension: usize,
}

impl VectorPartitioner {
    /// Create a new vector partitioner
    pub fn new(partition_count: usize, dimension: usize) -> Self {
        Self {
            partition_count,
            centroids: Vec::with_capacity(partition_count),
            partition_to_shard: HashMap::new(),
            distance_metric: DistanceMetric::Euclidean,
            dimension,
        }
    }

    /// Set the distance metric
    pub fn with_distance_metric(mut self, metric: DistanceMetric) -> Self {
        self.distance_metric = metric;
        self
    }

    /// Initialize centroids using k-means++ algorithm
    pub fn initialize_centroids(&mut self, sample_vectors: &[Vector]) -> Result<()> {
        if sample_vectors.is_empty() {
            return Err(ReplicationError::Sharding(
                "Cannot initialize centroids with empty sample".to_string(),
            ));
        }

        if sample_vectors.len() < self.partition_count {
            return Err(ReplicationError::Sharding(format!(
                "Need at least {} samples for {} partitions",
                self.partition_count,
                self.partition_count
            )));
        }

        // k-means++ initialization
        self.centroids.clear();

        // First centroid: random sample
        self.centroids.push(sample_vectors[0].clone());

        // Remaining centroids: proportional to squared distance
        for _ in 1..self.partition_count {
            let mut distances: Vec<f32> = sample_vectors
                .iter()
                .map(|v| {
                    self.centroids
                        .iter()
                        .map(|c| self.distance(v, c))
                        .reduce(f32::min)
                        .unwrap_or(f32::MAX)
                })
                .collect();

            // Square distances for probability weighting
            let total: f32 = distances.iter().map(|d| d * d).sum();
            if total == 0.0 {
                // All remaining vectors are at centroid positions
                break;
            }

            distances.iter_mut().for_each(|d| *d = *d * *d / total);

            // Select next centroid (simplified: pick max distance)
            let (idx, _) = distances
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap();

            self.centroids.push(sample_vectors[idx].clone());
        }

        Ok(())
    }

    /// Set centroids directly (for loading saved state)
    pub fn set_centroids(&mut self, centroids: Vec<Vector>) -> Result<()> {
        if centroids.len() != self.partition_count {
            return Err(ReplicationError::Sharding(format!(
                "Expected {} centroids, got {}",
                self.partition_count,
                centroids.len()
            )));
        }

        for centroid in &centroids {
            if centroid.len() != self.dimension {
                return Err(ReplicationError::Sharding(format!(
                    "Expected dimension {}, got {}",
                    self.dimension,
                    centroid.len()
                )));
            }
        }

        self.centroids = centroids;
        Ok(())
    }

    /// Map partition to shard
    pub fn map_partition_to_shard(&mut self, partition_id: usize, shard_id: Uuid) -> Result<()> {
        if partition_id >= self.partition_count {
            return Err(ReplicationError::Sharding(format!(
                "Partition {} out of range (max {})",
                partition_id,
                self.partition_count - 1
            )));
        }

        self.partition_to_shard.insert(partition_id, shard_id);
        Ok(())
    }

    /// Get the partition (and shard) for a vector
    pub fn get_partition(&self, vector: &Vector) -> Result<PartitionAssignment> {
        if self.centroids.is_empty() {
            return Err(ReplicationError::Sharding(
                "Centroids not initialized".to_string(),
            ));
        }

        if vector.len() != self.dimension {
            return Err(ReplicationError::Sharding(format!(
                "Vector dimension {} doesn't match expected {}",
                vector.len(),
                self.dimension
            )));
        }

        // Find nearest centroid
        let (partition_id, distance) = self
            .centroids
            .iter()
            .enumerate()
            .map(|(i, c)| (i, self.distance(vector, c)))
            .min_by(|(_, d1), (_, d2)| d1.partial_cmp(d2).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap();

        // Calculate confidence based on distance to nearest vs second nearest
        let confidence = if self.centroids.len() > 1 {
            let mut distances: Vec<f32> = self
                .centroids
                .iter()
                .map(|c| self.distance(vector, c))
                .collect();
            distances.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

            if distances[1] > 0.0 {
                1.0 - (distances[0] / distances[1])
            } else {
                1.0
            }
        } else {
            1.0
        };

        Ok(PartitionAssignment {
            vector_id: Uuid::nil(), // Caller sets this
            partition_id,
            distance_to_centroid: distance,
            confidence,
        })
    }

    /// Get the shard for a vector
    pub fn get_shard(&self, vector: &Vector) -> Result<Uuid> {
        let assignment = self.get_partition(vector)?;
        self.partition_to_shard.get(&assignment.partition_id).cloned().ok_or_else(|| {
            ReplicationError::Sharding(format!(
                "No shard mapped for partition {}",
                assignment.partition_id
            ))
        })
    }

    /// Get K nearest partitions for multi-probe search
    pub fn get_nearest_partitions(&self, vector: &Vector, k: usize) -> Result<Vec<PartitionAssignment>> {
        if self.centroids.is_empty() {
            return Err(ReplicationError::Sharding(
                "Centroids not initialized".to_string(),
            ));
        }

        let mut distances: Vec<(usize, f32)> = self
            .centroids
            .iter()
            .enumerate()
            .map(|(i, c)| (i, self.distance(vector, c)))
            .collect();

        distances.sort_by(|(_, d1), (_, d2)| d1.partial_cmp(d2).unwrap_or(std::cmp::Ordering::Equal));

        Ok(distances
            .into_iter()
            .take(k)
            .map(|(partition_id, distance)| PartitionAssignment {
                vector_id: Uuid::nil(),
                partition_id,
                distance_to_centroid: distance,
                confidence: 1.0, // Not meaningful for multi-probe
            })
            .collect())
    }

    /// Update centroids with new vectors (incremental k-means)
    pub fn update_centroids(&mut self, vectors: &[(Uuid, Vector)]) -> Result<usize> {
        if self.centroids.is_empty() {
            return Err(ReplicationError::Sharding(
                "Centroids not initialized".to_string(),
            ));
        }

        // Assign vectors to partitions
        let mut partition_vectors: Vec<Vec<&Vector>> = vec![vec![]; self.partition_count];
        for (_, vector) in vectors {
            let assignment = self.get_partition(vector)?;
            partition_vectors[assignment.partition_id].push(vector);
        }

        // Update centroids
        let mut updates = 0;
        for (i, vecs) in partition_vectors.iter().enumerate() {
            if !vecs.is_empty() {
                let new_centroid = self.compute_centroid(vecs);
                let old_centroid = &self.centroids[i];

                // Check if centroid moved significantly
                let movement = self.distance(old_centroid, &new_centroid);
                if movement > 0.001 {
                    self.centroids[i] = new_centroid;
                    updates += 1;
                }
            }
        }

        Ok(updates)
    }

    /// Get partition information
    pub fn partition_info(&self, partition_id: usize) -> Option<PartitionInfo> {
        if partition_id >= self.partition_count {
            return None;
        }

        let centroid = self.centroids.get(partition_id)?.clone();
        let shard_id = self.partition_to_shard.get(&partition_id).copied()?;

        Some(PartitionInfo {
            id: partition_id,
            shard_id,
            centroid,
            vector_count: 0, // Would need external tracking
            avg_distance: 0.0,
        })
    }

    /// Get all partition info
    pub fn all_partitions(&self) -> Vec<PartitionInfo> {
        (0..self.partition_count)
            .filter_map(|i| self.partition_info(i))
            .collect()
    }

    /// Calculate distance between two vectors
    fn distance(&self, a: &Vector, b: &Vector) -> f32 {
        match self.distance_metric {
            DistanceMetric::Euclidean => {
                a.iter()
                    .zip(b.iter())
                    .map(|(x, y)| (x - y).powi(2))
                    .sum::<f32>()
                    .sqrt()
            }
            DistanceMetric::Cosine => {
                let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
                let norm_a: f32 = a.iter().map(|x| x.powi(2)).sum::<f32>().sqrt();
                let norm_b: f32 = b.iter().map(|x| x.powi(2)).sum::<f32>().sqrt();
                if norm_a > 0.0 && norm_b > 0.0 {
                    1.0 - (dot / (norm_a * norm_b))
                } else {
                    1.0
                }
            }
            DistanceMetric::InnerProduct => {
                let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
                -dot // Negate so lower is better
            }
            DistanceMetric::Manhattan => {
                a.iter().zip(b.iter()).map(|(x, y)| (x - y).abs()).sum()
            }
        }
    }

    /// Compute centroid of vectors
    fn compute_centroid(&self, vectors: &[&Vector]) -> Vector {
        if vectors.is_empty() {
            return vec![0.0; self.dimension];
        }

        let mut centroid = vec![0.0; self.dimension];
        for v in vectors {
            for (i, val) in v.iter().enumerate() {
                centroid[i] += val;
            }
        }

        let count = vectors.len() as f32;
        for val in &mut centroid {
            *val /= count;
        }

        centroid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partitioner_creation() {
        let partitioner = VectorPartitioner::new(4, 128);
        assert!(partitioner.centroids.is_empty());
    }

    #[test]
    fn test_initialize_centroids() {
        let mut partitioner = VectorPartitioner::new(3, 4);

        let samples: Vec<Vector> = vec![
            vec![1.0, 0.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0, 0.0],
            vec![0.0, 0.0, 1.0, 0.0],
            vec![0.0, 0.0, 0.0, 1.0],
            vec![1.0, 1.0, 0.0, 0.0],
        ];

        partitioner.initialize_centroids(&samples).expect("init failed");
        assert_eq!(partitioner.centroids.len(), 3);
    }

    #[test]
    fn test_get_partition() {
        let mut partitioner = VectorPartitioner::new(2, 3);

        // Set centroids manually
        partitioner
            .set_centroids(vec![
                vec![1.0, 0.0, 0.0], // Partition 0
                vec![0.0, 1.0, 0.0], // Partition 1
            ])
            .expect("set centroids failed");

        // Map partitions to shards
        let shard0 = Uuid::new_v4();
        let shard1 = Uuid::new_v4();
        partitioner.map_partition_to_shard(0, shard0).unwrap();
        partitioner.map_partition_to_shard(1, shard1).unwrap();

        // Test vector closer to partition 0
        let v0 = vec![0.9, 0.1, 0.0];
        let assignment = partitioner.get_partition(&v0).expect("get partition failed");
        assert_eq!(assignment.partition_id, 0);

        // Test vector closer to partition 1
        let v1 = vec![0.1, 0.9, 0.0];
        let assignment = partitioner.get_partition(&v1).expect("get partition failed");
        assert_eq!(assignment.partition_id, 1);
    }

    #[test]
    fn test_nearest_partitions() {
        let mut partitioner = VectorPartitioner::new(3, 2);

        partitioner
            .set_centroids(vec![
                vec![0.0, 0.0],
                vec![1.0, 0.0],
                vec![0.5, 1.0],
            ])
            .expect("set centroids failed");

        let v = vec![0.5, 0.5];
        let nearest = partitioner.get_nearest_partitions(&v, 2).expect("get nearest failed");

        assert_eq!(nearest.len(), 2);
        // Should return the 2 closest partitions
    }

    #[test]
    fn test_distance_metrics() {
        let partitioner = VectorPartitioner::new(1, 3).with_distance_metric(DistanceMetric::Euclidean);

        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];

        let dist = partitioner.distance(&a, &b);
        assert!((dist - 1.414).abs() < 0.01); // sqrt(2)

        // Test cosine
        let partitioner = VectorPartitioner::new(1, 3).with_distance_metric(DistanceMetric::Cosine);
        let dist = partitioner.distance(&a, &b);
        assert!((dist - 1.0).abs() < 0.01); // Orthogonal vectors have cosine distance of 1
    }
}
