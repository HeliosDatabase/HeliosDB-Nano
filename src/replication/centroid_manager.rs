//! Centroid Manager - Tier 3 Sharding (Vector-Aware)
//!
//! Manages centroid computation, updates, and drift detection for vector partitioning.
//! Handles periodic recomputation and rebalancing triggers.

use super::vector_partitioner::{DistanceMetric, Vector, VectorPartitioner};
use super::{ReplicationError, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

/// Centroid drift threshold for rebalancing
const DEFAULT_DRIFT_THRESHOLD: f32 = 0.1;

/// Minimum vectors per partition before considering rebalance
const MIN_VECTORS_FOR_REBALANCE: usize = 100;

/// Centroid state
#[derive(Debug, Clone)]
pub struct CentroidState {
    /// Centroid ID (partition ID)
    pub id: usize,
    /// Current centroid vector
    pub centroid: Vector,
    /// Previous centroid (for drift calculation)
    pub previous_centroid: Option<Vector>,
    /// Drift from previous centroid
    pub drift: f32,
    /// Number of vectors in this partition
    pub vector_count: usize,
    /// Sum of distances to centroid
    pub total_distance: f64,
    /// Last updated timestamp
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Centroid update event
#[derive(Debug, Clone)]
pub enum CentroidEvent {
    /// Centroid was updated
    Updated { partition_id: usize, drift: f32 },
    /// Significant drift detected
    DriftDetected { partition_id: usize, drift: f32 },
    /// Rebalance recommended
    RebalanceRecommended { partitions: Vec<usize>, reason: String },
    /// Centroids were recomputed
    Recomputed { partition_count: usize },
}

/// Statistics for a partition
#[derive(Debug, Clone)]
pub struct PartitionStats {
    /// Partition ID
    pub partition_id: usize,
    /// Vector count
    pub vector_count: usize,
    /// Average distance to centroid
    pub avg_distance: f32,
    /// Standard deviation of distances
    pub std_dev: f32,
    /// Min distance
    pub min_distance: f32,
    /// Max distance
    pub max_distance: f32,
    /// Percentage of total vectors
    pub percentage: f32,
}

/// Centroid Manager
pub struct CentroidManager {
    /// Vector partitioner
    partitioner: Arc<RwLock<VectorPartitioner>>,
    /// Centroid states
    states: Arc<RwLock<Vec<CentroidState>>>,
    /// Distance metric
    distance_metric: DistanceMetric,
    /// Dimension of vectors
    dimension: usize,
    /// Drift threshold for alerts
    drift_threshold: f32,
    /// Event channel sender
    event_tx: mpsc::Sender<CentroidEvent>,
    /// Event channel receiver
    event_rx: Option<mpsc::Receiver<CentroidEvent>>,
    /// Vector samples for recomputation
    samples: Arc<RwLock<Vec<Vector>>>,
    /// Max samples to keep
    max_samples: usize,
}

impl CentroidManager {
    /// Create a new centroid manager
    pub fn new(partition_count: usize, dimension: usize) -> Self {
        let (event_tx, event_rx) = mpsc::channel(100);
        let partitioner = VectorPartitioner::new(partition_count, dimension);

        Self {
            partitioner: Arc::new(RwLock::new(partitioner)),
            states: Arc::new(RwLock::new(Vec::new())),
            distance_metric: DistanceMetric::Euclidean,
            dimension,
            drift_threshold: DEFAULT_DRIFT_THRESHOLD,
            event_tx,
            event_rx: Some(event_rx),
            samples: Arc::new(RwLock::new(Vec::new())),
            max_samples: 10000,
        }
    }

    /// Set drift threshold
    pub fn with_drift_threshold(mut self, threshold: f32) -> Self {
        self.drift_threshold = threshold;
        self
    }

    /// Set distance metric
    pub fn with_distance_metric(mut self, metric: DistanceMetric) -> Self {
        self.distance_metric = metric;
        self
    }

    /// Set max samples
    pub fn with_max_samples(mut self, max: usize) -> Self {
        self.max_samples = max;
        self
    }

    /// Initialize centroids from sample vectors
    pub async fn initialize(&self, samples: &[Vector]) -> Result<()> {
        let mut partitioner = self.partitioner.write().await;
        partitioner.initialize_centroids(samples)?;

        // Initialize states
        let mut states = self.states.write().await;
        states.clear();

        for (i, centroid) in partitioner.all_partitions().iter().enumerate() {
            states.push(CentroidState {
                id: i,
                centroid: centroid.centroid.clone(),
                previous_centroid: None,
                drift: 0.0,
                vector_count: 0,
                total_distance: 0.0,
                updated_at: chrono::Utc::now(),
            });
        }

        let _ = self.event_tx.send(CentroidEvent::Recomputed {
            partition_count: states.len(),
        }).await;

        Ok(())
    }

    /// Load centroids from saved state
    pub async fn load_centroids(&self, centroids: Vec<Vector>) -> Result<()> {
        let mut partitioner = self.partitioner.write().await;
        partitioner.set_centroids(centroids.clone())?;

        // Initialize states
        let mut states = self.states.write().await;
        states.clear();

        for (i, centroid) in centroids.into_iter().enumerate() {
            states.push(CentroidState {
                id: i,
                centroid,
                previous_centroid: None,
                drift: 0.0,
                vector_count: 0,
                total_distance: 0.0,
                updated_at: chrono::Utc::now(),
            });
        }

        Ok(())
    }

    /// Record a vector for statistics and sampling
    pub async fn record_vector(&self, vector: &Vector) -> Result<usize> {
        let partitioner = self.partitioner.read().await;
        let assignment = partitioner.get_partition(vector)?;

        // Update state
        let mut states = self.states.write().await;
        if let Some(state) = states.get_mut(assignment.partition_id) {
            state.vector_count += 1;
            state.total_distance += assignment.distance_to_centroid as f64;
        }

        // Maybe add to samples
        let mut samples = self.samples.write().await;
        if samples.len() < self.max_samples {
            samples.push(vector.clone());
        } else {
            // Reservoir sampling
            let idx = rand::random::<usize>() % (samples.len() + 1);
            if idx < samples.len() {
                samples[idx] = vector.clone();
            }
        }

        Ok(assignment.partition_id)
    }

    /// Get partition for a vector
    pub async fn get_partition(&self, vector: &Vector) -> Result<usize> {
        let partitioner = self.partitioner.read().await;
        Ok(partitioner.get_partition(vector)?.partition_id)
    }

    /// Get shard for a vector
    pub async fn get_shard(&self, vector: &Vector) -> Result<Uuid> {
        let partitioner = self.partitioner.read().await;
        partitioner.get_shard(vector)
    }

    /// Map partition to shard
    pub async fn map_partition_to_shard(&self, partition_id: usize, shard_id: Uuid) -> Result<()> {
        let mut partitioner = self.partitioner.write().await;
        partitioner.map_partition_to_shard(partition_id, shard_id)
    }

    /// Recompute centroids using current samples
    pub async fn recompute_centroids(&self) -> Result<Vec<f32>> {
        let samples = self.samples.read().await;
        if samples.len() < MIN_VECTORS_FOR_REBALANCE {
            return Err(ReplicationError::Sharding(format!(
                "Need at least {} samples to recompute centroids",
                MIN_VECTORS_FOR_REBALANCE
            )));
        }

        // Get current centroids for drift calculation
        let mut old_centroids: HashMap<usize, Vector> = HashMap::new();
        {
            let states = self.states.read().await;
            for state in states.iter() {
                old_centroids.insert(state.id, state.centroid.clone());
            }
        }

        // Recompute
        let mut partitioner = self.partitioner.write().await;
        let vectors: Vec<(Uuid, Vector)> = samples
            .iter()
            .map(|v| (Uuid::new_v4(), v.clone()))
            .collect();
        let updates = partitioner.update_centroids(&vectors)?;

        // Calculate drift and update states
        let mut drifts = Vec::new();
        let mut states = self.states.write().await;

        for state in states.iter_mut() {
            if let Some(info) = partitioner.partition_info(state.id) {
                if let Some(old) = old_centroids.get(&state.id) {
                    let drift = self.calculate_distance(old, &info.centroid);
                    state.drift = drift;
                    drifts.push(drift);

                    if drift > self.drift_threshold {
                        let _ = self.event_tx.send(CentroidEvent::DriftDetected {
                            partition_id: state.id,
                            drift,
                        }).await;
                    }
                }

                state.previous_centroid = Some(state.centroid.clone());
                state.centroid = info.centroid;
                state.updated_at = chrono::Utc::now();

                let _ = self.event_tx.send(CentroidEvent::Updated {
                    partition_id: state.id,
                    drift: state.drift,
                }).await;
            }
        }

        if updates > 0 {
            let _ = self.event_tx.send(CentroidEvent::Recomputed {
                partition_count: states.len(),
            }).await;
        }

        Ok(drifts)
    }

    /// Check if rebalancing is needed
    pub async fn check_rebalance_needed(&self) -> Option<Vec<usize>> {
        let states = self.states.read().await;

        // Check for significant drift
        let drifted: Vec<usize> = states
            .iter()
            .filter(|s| s.drift > self.drift_threshold)
            .map(|s| s.id)
            .collect();

        if !drifted.is_empty() {
            let _ = self.event_tx.send(CentroidEvent::RebalanceRecommended {
                partitions: drifted.clone(),
                reason: "Significant centroid drift detected".to_string(),
            }).await;
            return Some(drifted);
        }

        // Check for imbalanced partitions
        let total_vectors: usize = states.iter().map(|s| s.vector_count).sum();
        if total_vectors == 0 {
            return None;
        }

        let expected = total_vectors / states.len();
        let threshold = expected / 2; // 50% imbalance

        let imbalanced: Vec<usize> = states
            .iter()
            .filter(|s| {
                s.vector_count < expected.saturating_sub(threshold)
                    || s.vector_count > expected + threshold
            })
            .map(|s| s.id)
            .collect();

        if imbalanced.len() > states.len() / 3 {
            let _ = self.event_tx.send(CentroidEvent::RebalanceRecommended {
                partitions: imbalanced.clone(),
                reason: "Significant partition imbalance".to_string(),
            }).await;
            return Some(imbalanced);
        }

        None
    }

    /// Get partition statistics
    pub async fn partition_stats(&self) -> Vec<PartitionStats> {
        let states = self.states.read().await;
        let total_vectors: usize = states.iter().map(|s| s.vector_count).sum();

        states
            .iter()
            .map(|s| {
                let avg_distance = if s.vector_count > 0 {
                    (s.total_distance / s.vector_count as f64) as f32
                } else {
                    0.0
                };

                PartitionStats {
                    partition_id: s.id,
                    vector_count: s.vector_count,
                    avg_distance,
                    std_dev: 0.0, // Would need full distance tracking
                    min_distance: 0.0,
                    max_distance: 0.0,
                    percentage: if total_vectors > 0 {
                        (s.vector_count as f32 / total_vectors as f32) * 100.0
                    } else {
                        0.0
                    },
                }
            })
            .collect()
    }

    /// Get current centroids
    pub async fn get_centroids(&self) -> Vec<Vector> {
        let states = self.states.read().await;
        states.iter().map(|s| s.centroid.clone()).collect()
    }

    /// Get centroid state
    pub async fn get_state(&self, partition_id: usize) -> Option<CentroidState> {
        let states = self.states.read().await;
        states.get(partition_id).cloned()
    }

    /// Clear samples
    pub async fn clear_samples(&self) {
        self.samples.write().await.clear();
    }

    /// Reset statistics
    pub async fn reset_stats(&self) {
        let mut states = self.states.write().await;
        for state in states.iter_mut() {
            state.vector_count = 0;
            state.total_distance = 0.0;
        }
    }

    /// Calculate distance between two vectors
    fn calculate_distance(&self, a: &Vector, b: &Vector) -> f32 {
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
                -dot
            }
            DistanceMetric::Manhattan => {
                a.iter().zip(b.iter()).map(|(x, y)| (x - y).abs()).sum()
            }
        }
    }

    /// Take the event receiver
    pub fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<CentroidEvent>> {
        self.event_rx.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_manager_creation() {
        let manager = CentroidManager::new(4, 128);
        let centroids = manager.get_centroids().await;
        assert!(centroids.is_empty()); // Not initialized yet
    }

    #[tokio::test]
    async fn test_initialize() {
        let manager = CentroidManager::new(3, 4);

        let samples: Vec<Vector> = vec![
            vec![1.0, 0.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0, 0.0],
            vec![0.0, 0.0, 1.0, 0.0],
            vec![0.0, 0.0, 0.0, 1.0],
            vec![1.0, 1.0, 0.0, 0.0],
        ];

        manager.initialize(&samples).await.expect("init failed");

        let centroids = manager.get_centroids().await;
        assert_eq!(centroids.len(), 3);
    }

    #[tokio::test]
    async fn test_record_vector() {
        let manager = CentroidManager::new(2, 3);

        // Initialize with simple centroids
        let samples = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
        ];
        manager.initialize(&samples).await.expect("init failed");

        // Record a vector
        let v = vec![0.9, 0.1, 0.0];
        let partition = manager.record_vector(&v).await.expect("record failed");

        // Should be assigned to some partition
        assert!(partition < 2);

        // Check stats
        let stats = manager.partition_stats().await;
        assert!(!stats.is_empty());
    }

    #[tokio::test]
    async fn test_load_centroids() {
        let manager = CentroidManager::new(2, 3);

        let centroids = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
        ];

        manager.load_centroids(centroids.clone()).await.expect("load failed");

        let loaded = manager.get_centroids().await;
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0], centroids[0]);
        assert_eq!(loaded[1], centroids[1]);
    }
}
