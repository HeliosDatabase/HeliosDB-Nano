//! DDL (Data Definition Language) operations
//!
//! This module handles CREATE/DROP INDEX and other DDL operations.

#![allow(elided_lifetimes_in_paths)]

use crate::{Result, Error};
use crate::sql::LogicalPlan;
use super::{PhysicalOperator, Executor};
use super::scan::ScanOperator;
use rocksdb::{IteratorMode, ReadOptions};
use std::sync::Arc;

/// Handle CREATE INDEX logical plan node
pub(super) fn handle_create_index(
    executor: &Executor,
    plan: &LogicalPlan,
) -> Result<Box<dyn PhysicalOperator>> {
    if let LogicalPlan::CreateIndex { name, table_name, column_name, index_type, if_not_exists, options } = plan {
        // For now, return an empty result - actual index creation happens in storage layer
        // This is a placeholder until we integrate proper DDL execution
        if let Some(storage) = executor.storage() {
            // Check if it's a vector index (USING hnsw)
            if let Some(idx_type) = index_type {
                if idx_type == "art" {
                    // Handle ART index creation
                    let art_manager = storage.art_indexes();

                    // Check if index already exists
                    if art_manager.index_exists(name) {
                        if *if_not_exists {
                            return Ok(Box::new(ScanOperator::new(
                                "".to_string(),
                                Arc::new(crate::Schema { columns: vec![] }),
                                None,
                                vec![],
                                vec![],
                            ).with_timeout(executor.timeout_ctx())));
                        } else {
                            return Err(Error::query_execution(format!(
                                "ART index '{}' already exists",
                                name
                            )));
                        }
                    }

                    // Verify table exists
                    let catalog = storage.catalog();
                    let schema = catalog.get_table_schema(table_name)?;

                    // Verify column exists
                    if !schema.columns.iter().any(|c| c.name == *column_name) {
                        return Err(Error::query_execution(format!(
                            "Column '{}' not found in table '{}'",
                            column_name, table_name
                        )));
                    }

                    // Create manual ART index
                    let columns = vec![column_name.clone()];
                    art_manager.create_manual_index(name, table_name, &columns)
                        .map_err(|e| Error::query_execution(format!(
                            "Failed to create ART index: {}", e
                        )))?;

                    tracing::info!("Created ART index '{}' on table '{}' column '{}'",
                        name, table_name, column_name);

                    // Log to WAL for replication
                    if let Err(e) = storage.log_create_index(
                        name,
                        table_name,
                        column_name,
                        Some("art"),
                        &[],
                    ) {
                        tracing::warn!("Failed to log CREATE INDEX to WAL: {}", e);
                    }
                } else if idx_type == "gin" || idx_type == "gist" {
                    // Postgres FTS/GIN/GiST index.
                    //
                    // Accepted for syntactic compatibility (Django, Rails,
                    // and hand-written migrations emit CREATE INDEX ...
                    // USING gin) but does NOT yet build a real inverted
                    // index — the @@ operator walks the table row by row
                    // using the in-evaluator BM25 scorer. On realistic
                    // text volumes this is fine; at scale, consider the
                    // native search::bm25 API until a persistent GIN
                    // backend lands.
                    //
                    // See docs/compatibility/fts.md for the full list of
                    // behaviours we do and do not implement.
                    tracing::info!(
                        "Accepted CREATE INDEX {} USING {} ON {} ({}) — \
                         DDL-only (no backing index yet)",
                        name, idx_type, table_name, column_name
                    );
                    if let Err(e) = storage.log_create_index(
                        name, table_name, column_name, Some(idx_type.as_str()), &[],
                    ) {
                        tracing::warn!("Failed to log CREATE INDEX to WAL: {}", e);
                    }
                } else if idx_type == "hnsw" {
                    // Check if index already exists
                    let vector_indexes = storage.vector_indexes();
                    if vector_indexes.index_exists(name) {
                        if *if_not_exists {
                            // IF NOT EXISTS specified, return silently
                            return Ok(Box::new(ScanOperator::new(
                                "".to_string(),
                                Arc::new(crate::Schema { columns: vec![] }),
                                None,
                                vec![],
                                vec![],
                            ).with_timeout(executor.timeout_ctx())));
                        } else {
                            // Error: index already exists
                            return Err(Error::query_execution(format!(
                                "Index '{}' already exists",
                                name
                            )));
                        }
                    }

                    let catalog = storage.catalog();
                    let schema = catalog.get_table_schema(table_name)?;

                    // Find the column to index
                    let column = schema.get_column(column_name)
                        .ok_or_else(|| Error::query_execution(format!(
                            "Column '{}' not found in table '{}'",
                            column_name, table_name
                        )))?;

                    // Extract vector dimension from Vector(n) type
                    let dimension = match column.data_type {
                        crate::DataType::Vector(dim) => dim,
                        _ => return Err(Error::query_execution(format!(
                            "Column '{}' is not a vector type, cannot create HNSW index",
                            column_name
                        ))),
                    };

                    // Parse quantization options
                    use crate::sql::logical_plan::{IndexOption, QuantizationType};

                    let mut quantization_type = QuantizationType::None;
                    let mut pq_subquantizers: Option<usize> = None;
                    let mut pq_centroids: Option<usize> = None;

                    for option in options {
                        match option {
                            IndexOption::Quantization(qt) => quantization_type = *qt,
                            IndexOption::PqSubquantizers(n) => pq_subquantizers = Some(*n),
                            IndexOption::PqCentroids(n) => pq_centroids = Some(*n),
                            _ => {} // Ignore other options for now
                        }
                    }

                    // Check if we should create a quantized index
                    match quantization_type {
                        QuantizationType::Product => {
                            // Create quantized index
                            use crate::vector::ProductQuantizerConfig;

                            // Build PQ config
                            let mut pq_config = ProductQuantizerConfig::default_for_dimension(dimension)
                                .map_err(|e| Error::query_execution(format!("Invalid PQ config: {}", e)))?;

                            if let Some(n) = pq_subquantizers {
                                pq_config.num_subquantizers = n;
                            }
                            if let Some(n) = pq_centroids {
                                pq_config.num_centroids = n;
                            }

                            // Validate config
                            pq_config.validate()
                                .map_err(|e| Error::query_execution(format!("Invalid PQ config: {}", e)))?;

                            // Collect existing vectors from the table for PQ training
                            let tuples = storage.scan_table(table_name)?;

                            // Find the vector column index
                            let col_idx = schema.get_column_index(column_name)
                                .ok_or_else(|| Error::query_execution(format!(
                                    "Column '{}' not found in schema",
                                    column_name
                                )))?;

                            // Extract vectors from tuples
                            let training_vectors: Vec<crate::vector::Vector> = tuples
                                .iter()
                                .filter_map(|tuple| {
                                    if let Some(crate::Value::Vector(ref vec)) = tuple.values.get(col_idx) {
                                        Some(vec.clone())
                                    } else {
                                        None
                                    }
                                })
                                .collect();

                            vector_indexes.create_quantized_index(
                                name.clone(),
                                table_name.clone(),
                                column_name.clone(),
                                dimension,
                                crate::vector::DistanceMetric::L2,
                                pq_config,
                                &training_vectors,
                            )?;

                            // Log to WAL for replication
                            if let Err(e) = storage.log_create_index(
                                name,
                                table_name,
                                column_name,
                                Some(idx_type.as_str()),
                                &[],
                            ) {
                                tracing::warn!("Failed to log CREATE INDEX to WAL: {}", e);
                            }
                        }
                        _ => {
                            // Create standard non-quantized index
                            vector_indexes.create_index(
                                name.clone(),
                                table_name.clone(),
                                column_name.clone(),
                                dimension,
                                crate::vector::DistanceMetric::L2,
                            )?;

                            // Log to WAL for replication
                            if let Err(e) = storage.log_create_index(
                                name,
                                table_name,
                                column_name,
                                index_type.as_deref(),
                                &[],
                            ) {
                                tracing::warn!("Failed to log CREATE INDEX to WAL: {}", e);
                            }
                        }
                    }
                }
            }
        }

        // Return empty result set for DDL
        Ok(Box::new(ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        ).with_timeout(executor.timeout_ctx())))
    } else {
        Err(Error::query_execution("Expected CreateIndex plan node"))
    }
}

/// Handle DROP TABLE logical plan node
pub(super) fn handle_drop_table(
    executor: &Executor,
    table_name: &str,
    if_exists: bool,
) -> Result<Box<dyn PhysicalOperator>> {
    if let Some(storage) = executor.storage() {
        let catalog = storage.catalog();

        // Check if table exists
        match catalog.get_table_schema(table_name) {
            Ok(_) => {
                // Table exists - drop it
                catalog.drop_table(table_name)?;
                // KanttBan #23 (v3.31.1 phase 2): clean up the
                // identity side-table record. Best-effort; missing
                // record is fine.
                let _ = catalog.drop_identity_columns(table_name);
            }
            Err(_) => {
                // Table doesn't exist
                if !if_exists {
                    return Err(Error::query_execution(format!(
                        "Table '{}' does not exist",
                        table_name
                    )));
                }
                // If IF EXISTS, silently succeed
            }
        }

        // Return empty result set for DDL
        Ok(Box::new(ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        ).with_timeout(executor.timeout_ctx())))
    } else {
        Err(Error::query_execution("No storage engine available"))
    }
}

/// Handle TRUNCATE logical plan node
pub(super) fn handle_truncate(
    executor: &Executor,
    table_name: &str,
) -> Result<Box<dyn PhysicalOperator>> {
    if let Some(storage) = executor.storage() {
        let catalog = storage.catalog();

        // Check if table exists
        if catalog.get_table_schema(table_name).is_err() {
            return Err(Error::query_execution(format!(
                "Table '{}' does not exist",
                table_name
            )));
        }

        // Delete all rows from the table
        let prefix = format!("data:{}:", table_name);
        let prefix_bytes = prefix.as_bytes();
        let mut keys_to_delete = Vec::new();

        // Collect all keys for this table
        // Use total_order_seek to bypass prefix bloom filter for full table scans
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = storage.db.iterator_opt(IteratorMode::Start, read_opts);
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(prefix_bytes) {
                if let (Some(&k), Some(&p)) = (key.first(), prefix_bytes.first()) {
                    if k > p {
                        break;
                    }
                }
                continue;
            }

            keys_to_delete.push(key.to_vec());
        }

        // Delete all collected keys
        for key in &keys_to_delete {
            storage.delete(key)?;
        }

        // Clear ART index entries for this table so that stale PK/UNIQUE
        // values do not block re-insertion of the same values.
        // Skip clearing if branches exist or time-travel snapshots are
        // retained, because branch data and snapshots may still
        // reference the indexed values.
        // Check for user-created branches (exclude the auto-created "main" branch).
        // Branch data uses separate key prefixes and does not share the ART index,
        // but as a safety measure we skip clearing when user branches exist.
        let has_user_branches = storage.list_branches()
            .map(|b| b.iter().any(|br| br.name != "main"))
            .unwrap_or(false);
        if !has_user_branches {
            storage.art_indexes().clear_table_indexes(table_name);
        }

        // Log to WAL for replication
        if let Err(e) = storage.log_truncate(table_name) {
            tracing::warn!("Failed to log TRUNCATE to WAL: {}", e);
        }

        // Return empty result set for DDL
        Ok(Box::new(ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        ).with_timeout(executor.timeout_ctx())))
    } else {
        Err(Error::query_execution("No storage engine available"))
    }
}

// =============================================================================
// HA Operations (ha-tier1 feature)
// =============================================================================

/// Handle SWITCHOVER to target node
/// Example: SELECT helios_switchover('node-uuid')
#[cfg(feature = "ha-tier1")]
pub(super) fn handle_switchover(
    _executor: &Executor,
    target_node: &str,
) -> Result<Box<dyn PhysicalOperator>> {
    use uuid::Uuid;
    use crate::replication::ha_state::ha_state;
    use crate::replication::topology_manager;

    // Resolve target node (can be alias or UUID)
    let target_uuid = topology_manager().resolve_node_id(target_node)
        .or_else(|| {
            // Fallback: try parsing as UUID directly if not in topology
            Uuid::parse_str(target_node).ok()
        })
        .ok_or_else(|| Error::query_execution(format!(
            "Target node '{}' not found. Specify a valid node alias or UUID.",
            target_node
        )))?;

    // Get HA state registry
    let ha_registry = ha_state();

    // Check if this node is primary
    if ha_registry.get_role() != crate::replication::ha_state::HARole::Primary {
        return Err(Error::query_execution(
            "Switchover can only be initiated from the primary node"
        ));
    }

    // Check if target standby exists and is healthy
    let standbys = ha_registry.get_standbys();
    let target_standby = standbys.iter().find(|s| s.node_id == target_uuid);

    if target_standby.is_none() {
        return Err(Error::query_execution(format!(
            "Target standby '{}' ({}) not found or not connected",
            target_node, target_uuid
        )));
    }

    // Get the display name for user feedback
    let display_name = topology_manager()
        .get_node(target_uuid)
        .map(|n| n.display_name())
        .unwrap_or_else(|| target_node.to_string());

    // For now, return a message indicating switchover would be initiated
    // Full implementation requires async coordination with SwitchoverCoordinator
    let msg = format!(
        "Switchover to node {} ({}) initiated. This is a placeholder - full async switchover requires runtime integration.",
        display_name, target_uuid
    );

    Ok(Box::new(super::StatusMessageOperator::new(msg)))
}

/// Handle SWITCHOVER CHECK to validate preconditions
/// Example: SELECT helios_switchover_check('node-uuid') or SELECT helios_switchover_check('alias')
#[cfg(feature = "ha-tier1")]
pub(super) fn handle_switchover_check(
    _executor: &Executor,
    target_node: &str,
) -> Result<Box<dyn PhysicalOperator>> {
    use uuid::Uuid;
    use crate::replication::ha_state::ha_state;
    use crate::replication::topology_manager;
    use crate::{Tuple, Value, Schema, Column, DataType};

    // Resolve target node (can be alias or UUID)
    let target_uuid = topology_manager().resolve_node_id(target_node)
        .or_else(|| {
            // Fallback: try parsing as UUID directly if not in topology
            Uuid::parse_str(target_node).ok()
        })
        .ok_or_else(|| Error::query_execution(format!(
            "Target node '{}' not found. Specify a valid node alias or UUID.",
            target_node
        )))?;

    // Get HA state registry
    let ha_registry = ha_state();

    // Build check result
    let mut can_proceed = true;
    let mut target_healthy = false;
    let mut target_lsn: u64 = 0;
    let primary_lsn = ha_registry.get_lsn();
    let mut warnings = Vec::new();
    let mut blockers = Vec::new();

    // Check if this node is primary
    if ha_registry.get_role() != crate::replication::ha_state::HARole::Primary {
        can_proceed = false;
        blockers.push("This node is not the primary".to_string());
    }

    // Check target standby
    let standbys = ha_registry.get_standbys();
    if let Some(standby) = standbys.iter().find(|s| s.node_id == target_uuid) {
        target_healthy = true;
        target_lsn = standby.apply_lsn;

        let lag = primary_lsn.saturating_sub(target_lsn);
        if lag > 0 {
            warnings.push(format!("Target standby is {} LSN behind", lag));
        }
    } else {
        can_proceed = false;
        blockers.push(format!("Target node {} ({}) not found", target_node, target_uuid));
    }

    let lag_bytes = primary_lsn.saturating_sub(target_lsn) as i64;

    // Create result tuple
    let schema = Arc::new(Schema {
        columns: vec![
            Column::new("can_proceed", DataType::Boolean),
            Column::new("target_healthy", DataType::Boolean),
            Column::new("target_lsn", DataType::Int8),
            Column::new("primary_lsn", DataType::Int8),
            Column::new("lag_bytes", DataType::Int8),
            Column::new("warnings", DataType::Text),
            Column::new("blockers", DataType::Text),
        ],
    });

    let tuple = Tuple::new(vec![
        Value::Boolean(can_proceed),
        Value::Boolean(target_healthy),
        Value::Int8(target_lsn as i64),
        Value::Int8(primary_lsn as i64),
        Value::Int8(lag_bytes),
        Value::String(warnings.join("; ")),
        Value::String(blockers.join("; ")),
    ]);

    Ok(Box::new(SingleTupleOperator::new(tuple, schema)))
}

/// Handle CLUSTER STATUS query
/// Example: SELECT * FROM helios_cluster_status()
#[cfg(feature = "ha-tier1")]
pub(super) fn handle_cluster_status(
    _executor: &Executor,
) -> Result<Box<dyn PhysicalOperator>> {
    use crate::replication::ha_state::{ha_state, HARole};
    use crate::{Tuple, Value, Schema, Column, DataType};

    let ha_registry = ha_state();

    let schema = Arc::new(Schema {
        columns: vec![
            Column::new("node_id", DataType::Text),
            Column::new("role", DataType::Text),
            Column::new("address", DataType::Text),
            Column::new("is_healthy", DataType::Boolean),
            Column::new("lsn", DataType::Int8),
            Column::new("lag_ms", DataType::Int8),
            Column::new("priority", DataType::Int4),
        ],
    });

    let mut tuples = Vec::new();

    // Add primary info if available
    if let Some(config) = ha_registry.get_config() {
        let role_str = match ha_registry.get_role() {
            HARole::Primary => "primary",
            HARole::Standby => "standby",
            HARole::Standalone => "standalone",
            HARole::Observer => "observer",
        };

        tuples.push(Tuple::new(vec![
            Value::String(config.node_id.to_string()),
            Value::String(role_str.to_string()),
            Value::String(config.listen_addr.clone()),
            Value::Boolean(true), // Local node is always "healthy" from its perspective
            Value::Int8(ha_registry.get_lsn() as i64),
            Value::Int8(0), // No lag for self
            Value::Int4(100), // Default priority - config doesn't store priority yet
        ]));
    }

    // Add standby info
    for standby in ha_registry.get_standbys() {
        tuples.push(Tuple::new(vec![
            Value::String(standby.node_id.to_string()),
            Value::String("standby".to_string()),
            Value::String(standby.address.clone()),
            Value::Boolean(true), // Connected standbys are healthy
            Value::Int8(standby.apply_lsn as i64),
            Value::Int8(standby.lag_ms as i64),
            Value::Int4(0), // Priority not stored in StandbyInfo yet
        ]));
    }

    Ok(Box::new(MultiTupleOperator::new(tuples, schema)))
}

/// Single tuple operator for returning one result row
#[cfg(feature = "ha-tier1")]
struct SingleTupleOperator {
    tuple: Option<crate::Tuple>,
    schema: Arc<crate::Schema>,
}

#[cfg(feature = "ha-tier1")]
impl SingleTupleOperator {
    fn new(tuple: crate::Tuple, schema: Arc<crate::Schema>) -> Self {
        Self {
            tuple: Some(tuple),
            schema,
        }
    }
}

#[cfg(feature = "ha-tier1")]
impl super::PhysicalOperator for SingleTupleOperator {
    fn next(&mut self) -> Result<Option<crate::Tuple>> {
        Ok(self.tuple.take())
    }

    fn schema(&self) -> Arc<crate::Schema> {
        self.schema.clone()
    }
}

/// Multi tuple operator for returning multiple result rows
#[cfg(feature = "ha-tier1")]
struct MultiTupleOperator {
    tuples: std::collections::VecDeque<crate::Tuple>,
    schema: Arc<crate::Schema>,
}

#[cfg(feature = "ha-tier1")]
impl MultiTupleOperator {
    fn new(tuples: Vec<crate::Tuple>, schema: Arc<crate::Schema>) -> Self {
        Self {
            tuples: tuples.into_iter().collect(),
            schema,
        }
    }
}

#[cfg(feature = "ha-tier1")]
impl super::PhysicalOperator for MultiTupleOperator {
    fn next(&mut self) -> Result<Option<crate::Tuple>> {
        Ok(self.tuples.pop_front())
    }

    fn schema(&self) -> Arc<crate::Schema> {
        self.schema.clone()
    }
}

/// Handle SET NODE ALIAS command
#[cfg(feature = "ha-tier1")]
pub(super) fn handle_set_node_alias(
    _executor: &Executor,
    node_id: &str,
    alias: &Option<String>,
) -> Result<Box<dyn PhysicalOperator>> {
    use uuid::Uuid;
    use crate::replication::topology_manager;
    use crate::{Tuple, Value, Schema, Column, DataType};

    let topology = topology_manager();

    // Resolve the node_id (could be existing alias or UUID)
    let target_uuid = topology.resolve_node_id(node_id)
        .or_else(|| Uuid::parse_str(node_id).ok())
        .ok_or_else(|| Error::query_execution(format!(
            "Node '{}' not found in cluster topology. Use SHOW TOPOLOGY to see available nodes.",
            node_id
        )))?;

    // Set or clear the alias
    let result_msg = if let Some(ref new_alias) = alias {
        // Validate alias format (no spaces, not a valid UUID pattern)
        if new_alias.contains(' ') {
            return Err(Error::query_execution("Alias cannot contain spaces"));
        }
        if Uuid::parse_str(new_alias).is_ok() {
            return Err(Error::query_execution("Alias cannot be a valid UUID format"));
        }
        if new_alias.is_empty() {
            return Err(Error::query_execution("Alias cannot be empty"));
        }

        if !topology.set_alias(target_uuid, Some(new_alias.clone())) {
            return Err(Error::query_execution(format!(
                "Failed to set alias: '{}' is already in use by another node",
                new_alias
            )));
        }

        format!("Node alias '{}' set for node '{}'", new_alias, target_uuid)
    } else {
        // Clearing alias - this should always succeed if the node exists
        topology.set_alias(target_uuid, None);
        format!("Node alias removed for node '{}'", target_uuid)
    };

    let schema = Arc::new(Schema {
        columns: vec![
            Column::new("result", DataType::Text),
        ],
    });

    let tuple = Tuple::new(vec![Value::String(result_msg)]);

    Ok(Box::new(SingleTupleOperator::new(tuple, schema)))
}

/// Handle SHOW TOPOLOGY command - displays detailed cluster topology
#[cfg(feature = "ha-tier1")]
pub(super) fn handle_show_topology(
    _executor: &Executor,
) -> Result<Box<dyn PhysicalOperator>> {
    use crate::replication::ha_state::{ha_state, HARole};
    use crate::replication::topology_manager;
    use crate::{Tuple, Value, Schema, Column, DataType};

    let ha_registry = ha_state();
    let topology = topology_manager();

    let schema = Arc::new(Schema {
        columns: vec![
            Column::new("node_id", DataType::Text),
            Column::new("alias", DataType::Text),
            Column::new("role", DataType::Text),
            Column::new("client_addr", DataType::Text),
            Column::new("replication_addr", DataType::Text),
            Column::new("healthy", DataType::Boolean),
            Column::new("health_msg", DataType::Text),
            Column::new("last_seen_secs", DataType::Int8),
            Column::new("lsn", DataType::Int8),
            Column::new("lag_ms", DataType::Int8),
            Column::new("priority", DataType::Int4),
            Column::new("weight", DataType::Int4),
        ],
    });

    let mut tuples = Vec::new();

    // Helper to get alias for a node
    let get_alias = |node_id: uuid::Uuid| -> Value {
        topology.get_node(node_id)
            .and_then(|n| n.alias.clone())
            .map(Value::String)
            .unwrap_or(Value::Null)
    };

    // Helper to get node info from topology
    let get_topology_info = |node_id: uuid::Uuid| -> (u32, u32, Option<String>) {
        topology.get_node(node_id)
            .map(|n| (n.priority, n.weight, n.health_message.clone()))
            .unwrap_or((100, 100, None))
    };

    // Add local node info
    if let Some(config) = ha_registry.get_config() {
        let role_str = match ha_registry.get_role() {
            HARole::Primary => "Primary",
            HARole::Standby => "Standby",
            HARole::Standalone => "Standalone",
            HARole::Observer => "Observer",
        };

        let alias = get_alias(config.node_id);
        let (priority, weight, health_msg) = get_topology_info(config.node_id);

        tuples.push(Tuple::new(vec![
            Value::String(config.node_id.to_string()),
            alias,
            Value::String(role_str.to_string()),
            Value::String(config.listen_addr.clone()),
            Value::String(format!("{}:{}", config.listen_addr, config.replication_port)),
            Value::Boolean(true), // Local node is always "healthy" from its perspective
            Value::String(health_msg.unwrap_or_else(|| "OK".to_string())),
            Value::Int8(0), // last seen
            Value::Int8(ha_registry.get_lsn() as i64),
            Value::Int8(0), // No lag for self
            Value::Int4(priority as i32),
            Value::Int4(weight as i32),
        ]));
    }

    // Add standby info from HA registry, enriched with topology data
    for standby in ha_registry.get_standbys() {
        let alias = get_alias(standby.node_id);
        let (priority, weight, health_msg) = get_topology_info(standby.node_id);

        tuples.push(Tuple::new(vec![
            Value::String(standby.node_id.to_string()),
            alias,
            Value::String("Standby".to_string()),
            Value::String(standby.address.clone()),
            Value::String(standby.address.clone()), // replication addr same as client for now
            Value::Boolean(true), // Connected standbys are healthy
            Value::String(health_msg.unwrap_or_else(|| "Connected".to_string())),
            Value::Int8(0), // last seen
            Value::Int8(standby.apply_lsn as i64),
            Value::Int8(standby.lag_ms as i64),
            Value::Int4(priority as i32),
            Value::Int4(weight as i32),
        ]));
    }

    Ok(Box::new(MultiTupleOperator::new(tuples, schema)))
}
