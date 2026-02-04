//! Shard Router - Tier 3 Sharding
//!
//! Routes queries to appropriate shards and aggregates results.
//! Handles cross-shard query planning and execution.

use super::hash_ring::{HashRing, ShardNode};
use super::{ReplicationError, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Shard key extraction strategy
#[derive(Debug, Clone)]
pub enum ShardKeyStrategy {
    /// Use a specific column as shard key
    Column { name: String },
    /// Use composite columns as shard key
    Composite { columns: Vec<String> },
    /// Hash the entire row
    RowHash,
    /// Use primary key
    PrimaryKey,
}

/// Table sharding configuration
#[derive(Debug, Clone)]
pub struct TableShardConfig {
    /// Table name
    pub table: String,
    /// Shard key strategy
    pub strategy: ShardKeyStrategy,
    /// Replication factor (how many nodes store each key)
    pub replication_factor: usize,
}

/// Query routing decision
#[derive(Debug, Clone)]
pub enum RoutingDecision {
    /// Route to a single shard
    SingleShard { node_id: Uuid },
    /// Route to multiple specific shards
    MultiShard { node_ids: Vec<Uuid> },
    /// Broadcast to all shards
    Broadcast,
    /// Route to coordinator for cross-shard processing
    Coordinator,
}

/// Query type for routing decisions
#[derive(Debug, Clone)]
pub enum QueryType {
    /// Point query with known shard key
    PointQuery { shard_key: Vec<u8> },
    /// Range query
    RangeQuery { start: Option<Vec<u8>>, end: Option<Vec<u8>> },
    /// Full table scan
    Scan,
    /// Aggregation query
    Aggregation { has_group_by: bool },
    /// Join query
    Join { tables: Vec<String> },
    /// Insert/Update/Delete with known key
    Mutation { shard_key: Vec<u8> },
}

/// Query execution plan across shards
#[derive(Debug, Clone)]
pub struct ShardedQueryPlan {
    /// Plan ID
    pub id: Uuid,
    /// Target shards
    pub shards: Vec<Uuid>,
    /// Query to execute on each shard
    pub shard_query: String,
    /// Whether results need aggregation
    pub needs_aggregation: bool,
    /// Aggregation type (if needed)
    pub aggregation: Option<AggregationType>,
    /// Sort requirements (if any)
    pub sort: Option<SortSpec>,
    /// Limit (if any)
    pub limit: Option<usize>,
}

/// Aggregation type for cross-shard queries
#[derive(Debug, Clone)]
pub enum AggregationType {
    /// COUNT aggregation
    Count,
    /// SUM aggregation
    Sum { column: String },
    /// AVG aggregation (needs sum + count)
    Avg { column: String },
    /// MIN aggregation
    Min { column: String },
    /// MAX aggregation
    Max { column: String },
    /// Custom aggregation
    Custom { name: String },
}

/// Sort specification
#[derive(Debug, Clone)]
pub struct SortSpec {
    /// Column to sort by
    pub column: String,
    /// Ascending or descending
    pub ascending: bool,
}

/// Result from a single shard
#[derive(Debug, Clone)]
pub struct ShardResult {
    /// Shard ID
    pub shard_id: Uuid,
    /// Result rows (serialized)
    pub rows: Vec<Vec<u8>>,
    /// Row count
    pub row_count: usize,
    /// Partial aggregate values (for aggregation queries)
    pub partial_aggregates: HashMap<String, f64>,
    /// Execution time
    pub execution_time_ms: u64,
    /// Error (if any)
    pub error: Option<String>,
}

/// Shard Router
pub struct ShardRouter {
    /// Hash ring for shard distribution
    ring: Arc<RwLock<HashRing>>,
    /// Table shard configurations
    table_configs: Arc<RwLock<HashMap<String, TableShardConfig>>>,
    /// Default replication factor
    default_replication_factor: usize,
}

impl ShardRouter {
    /// Create a new shard router
    pub fn new() -> Self {
        Self {
            ring: Arc::new(RwLock::new(HashRing::new())),
            table_configs: Arc::new(RwLock::new(HashMap::new())),
            default_replication_factor: 1,
        }
    }

    /// Create with custom replication factor
    pub fn with_replication_factor(replication_factor: usize) -> Self {
        Self {
            ring: Arc::new(RwLock::new(HashRing::new())),
            table_configs: Arc::new(RwLock::new(HashMap::new())),
            default_replication_factor: replication_factor,
        }
    }

    /// Add a shard node
    pub async fn add_shard(&self, node: ShardNode) -> Result<()> {
        self.ring.write().await.add_node(node)
    }

    /// Remove a shard node
    pub async fn remove_shard(&self, node_id: &Uuid) -> Result<ShardNode> {
        self.ring.write().await.remove_node(node_id)
    }

    /// Configure sharding for a table
    pub async fn configure_table(&self, config: TableShardConfig) {
        self.table_configs.write().await.insert(config.table.clone(), config);
    }

    /// Get table sharding configuration
    pub async fn get_table_config(&self, table: &str) -> Option<TableShardConfig> {
        self.table_configs.read().await.get(table).cloned()
    }

    /// Route a query to appropriate shard(s)
    pub async fn route(&self, table: &str, query_type: &QueryType) -> Result<RoutingDecision> {
        let ring = self.ring.read().await;

        if ring.node_count() == 0 {
            return Err(ReplicationError::Sharding("No shards available".to_string()));
        }

        match query_type {
            QueryType::PointQuery { shard_key } => {
                let node = ring.get_node(shard_key).ok_or_else(|| {
                    ReplicationError::Sharding("Failed to find shard for key".to_string())
                })?;
                Ok(RoutingDecision::SingleShard { node_id: node.id })
            }
            QueryType::Mutation { shard_key } => {
                let config = self.table_configs.read().await;
                let replication_factor = config
                    .get(table)
                    .map(|c| c.replication_factor)
                    .unwrap_or(self.default_replication_factor);

                if replication_factor == 1 {
                    let node = ring.get_node(shard_key).ok_or_else(|| {
                        ReplicationError::Sharding("Failed to find shard for key".to_string())
                    })?;
                    Ok(RoutingDecision::SingleShard { node_id: node.id })
                } else {
                    let nodes = ring.get_healthy_nodes(shard_key, replication_factor);
                    Ok(RoutingDecision::MultiShard {
                        node_ids: nodes.iter().map(|n| n.id).collect(),
                    })
                }
            }
            QueryType::RangeQuery { .. } | QueryType::Scan => {
                Ok(RoutingDecision::Broadcast)
            }
            QueryType::Aggregation { .. } => {
                Ok(RoutingDecision::Broadcast)
            }
            QueryType::Join { tables } => {
                // Check if all tables have same sharding
                let configs = self.table_configs.read().await;
                let strategies: Vec<_> = tables
                    .iter()
                    .filter_map(|t| configs.get(t))
                    .collect();

                if strategies.len() == tables.len() {
                    // All tables configured - could potentially do co-located joins
                    // For now, broadcast to be safe
                    Ok(RoutingDecision::Broadcast)
                } else {
                    Ok(RoutingDecision::Coordinator)
                }
            }
        }
    }

    /// Create an execution plan for a query
    pub async fn plan_query(
        &self,
        table: &str,
        query: &str,
        query_type: &QueryType,
    ) -> Result<ShardedQueryPlan> {
        let routing = self.route(table, query_type).await?;
        let ring = self.ring.read().await;

        let shards = match routing {
            RoutingDecision::SingleShard { node_id } => vec![node_id],
            RoutingDecision::MultiShard { node_ids } => node_ids,
            RoutingDecision::Broadcast | RoutingDecision::Coordinator => {
                ring.nodes().map(|n| n.id).collect()
            }
        };

        let (needs_aggregation, aggregation) = match query_type {
            QueryType::Aggregation { .. } => (true, None), // TODO: Parse aggregation type
            _ => (false, None),
        };

        Ok(ShardedQueryPlan {
            id: Uuid::new_v4(),
            shards,
            shard_query: query.to_string(),
            needs_aggregation,
            aggregation,
            sort: None,
            limit: None,
        })
    }

    /// Aggregate results from multiple shards
    pub fn aggregate_results(
        &self,
        results: Vec<ShardResult>,
        aggregation: Option<&AggregationType>,
    ) -> Result<ShardResult> {
        if results.is_empty() {
            return Ok(ShardResult {
                shard_id: Uuid::nil(),
                rows: vec![],
                row_count: 0,
                partial_aggregates: HashMap::new(),
                execution_time_ms: 0,
                error: None,
            });
        }

        // Check for errors
        let errors: Vec<&str> = results.iter().filter_map(|r| r.error.as_deref()).collect();
        if !errors.is_empty() {
            return Err(ReplicationError::Sharding(format!(
                "Shard errors: {}",
                errors.join(", ")
            )));
        }

        let total_rows: usize = results.iter().map(|r| r.row_count).sum();
        let max_time = results.iter().map(|r| r.execution_time_ms).max().unwrap_or(0);

        // Combine rows
        let all_rows: Vec<Vec<u8>> = results.iter().flat_map(|r| r.rows.clone()).collect();

        // Aggregate partial results if needed
        let mut aggregates = HashMap::new();
        if let Some(agg_type) = aggregation {
            match agg_type {
                AggregationType::Count => {
                    let total: f64 = results
                        .iter()
                        .filter_map(|r| r.partial_aggregates.get("count"))
                        .sum();
                    aggregates.insert("count".to_string(), total);
                }
                AggregationType::Sum { column } => {
                    let total: f64 = results
                        .iter()
                        .filter_map(|r| r.partial_aggregates.get(column))
                        .sum();
                    aggregates.insert(column.clone(), total);
                }
                AggregationType::Min { column } => {
                    if let Some(min) = results
                        .iter()
                        .filter_map(|r| r.partial_aggregates.get(column))
                        .copied()
                        .reduce(f64::min)
                    {
                        aggregates.insert(column.clone(), min);
                    }
                }
                AggregationType::Max { column } => {
                    if let Some(max) = results
                        .iter()
                        .filter_map(|r| r.partial_aggregates.get(column))
                        .copied()
                        .reduce(f64::max)
                    {
                        aggregates.insert(column.clone(), max);
                    }
                }
                AggregationType::Avg { column } => {
                    let sum: f64 = results
                        .iter()
                        .filter_map(|r| r.partial_aggregates.get(&format!("{}_sum", column)))
                        .sum();
                    let count: f64 = results
                        .iter()
                        .filter_map(|r| r.partial_aggregates.get(&format!("{}_count", column)))
                        .sum();
                    if count > 0.0 {
                        aggregates.insert(column.clone(), sum / count);
                    }
                }
                AggregationType::Custom { .. } => {
                    // Custom aggregation handling would go here
                }
            }
        }

        Ok(ShardResult {
            shard_id: Uuid::nil(), // Aggregated result
            rows: all_rows,
            row_count: total_rows,
            partial_aggregates: aggregates,
            execution_time_ms: max_time,
            error: None,
        })
    }

    /// Get shard for a specific key
    pub async fn get_shard_for_key(&self, key: &[u8]) -> Option<Uuid> {
        let ring = self.ring.read().await;
        ring.get_node(key).map(|n| n.id)
    }

    /// Get all shard IDs
    pub async fn all_shards(&self) -> Vec<Uuid> {
        let ring = self.ring.read().await;
        ring.nodes().map(|n| n.id).collect()
    }

    /// Get healthy shard count
    pub async fn healthy_shard_count(&self) -> usize {
        let ring = self.ring.read().await;
        ring.nodes().filter(|n| n.healthy).count()
    }
}

impl Default for ShardRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_router_creation() {
        let router = ShardRouter::new();
        assert_eq!(router.healthy_shard_count().await, 0);
    }

    #[tokio::test]
    async fn test_add_shards() {
        let router = ShardRouter::new();

        for i in 0..3 {
            let node = ShardNode::new(format!("shard{}", i), "localhost", 5432 + i);
            router.add_shard(node).await.expect("add failed");
        }

        assert_eq!(router.healthy_shard_count().await, 3);
    }

    #[tokio::test]
    async fn test_point_query_routing() {
        let router = ShardRouter::new();

        let node = ShardNode::new("shard0", "localhost", 5432);
        let node_id = node.id;
        router.add_shard(node).await.expect("add failed");

        let decision = router
            .route("users", &QueryType::PointQuery { shard_key: b"user_123".to_vec() })
            .await
            .expect("route failed");

        match decision {
            RoutingDecision::SingleShard { node_id: id } => assert_eq!(id, node_id),
            _ => panic!("Expected SingleShard routing"),
        }
    }

    #[tokio::test]
    async fn test_scan_routing() {
        let router = ShardRouter::new();

        for i in 0..3 {
            let node = ShardNode::new(format!("shard{}", i), "localhost", 5432 + i);
            router.add_shard(node).await.expect("add failed");
        }

        let decision = router
            .route("users", &QueryType::Scan)
            .await
            .expect("route failed");

        match decision {
            RoutingDecision::Broadcast => {}
            _ => panic!("Expected Broadcast routing"),
        }
    }

    #[tokio::test]
    async fn test_table_config() {
        let router = ShardRouter::new();

        router
            .configure_table(TableShardConfig {
                table: "users".to_string(),
                strategy: ShardKeyStrategy::Column { name: "user_id".to_string() },
                replication_factor: 3,
            })
            .await;

        let config = router.get_table_config("users").await;
        assert!(config.is_some());
        assert_eq!(config.unwrap().replication_factor, 3);
    }

    #[tokio::test]
    async fn test_result_aggregation() {
        let router = ShardRouter::new();

        let results = vec![
            ShardResult {
                shard_id: Uuid::new_v4(),
                rows: vec![vec![1, 2, 3]],
                row_count: 100,
                partial_aggregates: [("count".to_string(), 100.0)].into_iter().collect(),
                execution_time_ms: 10,
                error: None,
            },
            ShardResult {
                shard_id: Uuid::new_v4(),
                rows: vec![vec![4, 5, 6]],
                row_count: 200,
                partial_aggregates: [("count".to_string(), 200.0)].into_iter().collect(),
                execution_time_ms: 20,
                error: None,
            },
        ];

        let aggregated = router
            .aggregate_results(results, Some(&AggregationType::Count))
            .expect("aggregate failed");

        assert_eq!(aggregated.row_count, 300);
        assert_eq!(aggregated.rows.len(), 2);
        assert_eq!(aggregated.partial_aggregates.get("count"), Some(&300.0));
    }
}
