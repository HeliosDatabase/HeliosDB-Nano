//! Table and index scanning operators
//!
//! This module provides operators for reading data from tables and indexes.

#![allow(elided_lifetimes_in_paths)]

use crate::{Result, Error, Tuple, Schema};
use crate::sql::LogicalPlan;
use super::{PhysicalOperator, TimeoutContext, Executor};
use std::sync::Arc;

/// Table scan operator
///
/// Reads tuples from a table.
pub struct ScanOperator {
    table_name: String,
    schema: Arc<Schema>,
    projection: Option<Vec<usize>>,
    tuples: Vec<Tuple>,
    current_index: usize,
    timeout_ctx: Option<TimeoutContext>,
    #[allow(dead_code)]
    parameters: Vec<crate::Value>,
}

impl ScanOperator {
    pub fn new(
        table_name: String,
        schema: Arc<Schema>,
        projection: Option<Vec<usize>>,
        tuples: Vec<Tuple>,
        parameters: Vec<crate::Value>,
    ) -> Self {
        Self {
            table_name,
            schema,
            projection,
            tuples,
            current_index: 0,
            timeout_ctx: None,
            parameters,
        }
    }

    pub fn with_timeout(mut self, timeout_ctx: Option<TimeoutContext>) -> Self {
        self.timeout_ctx = timeout_ctx;
        self
    }
}

impl PhysicalOperator for ScanOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        // Check timeout before processing
        if let Some(ref ctx) = self.timeout_ctx {
            ctx.check_timeout()?;
        }

        if self.current_index >= self.tuples.len() {
            return Ok(None);
        }

        let tuple = self.tuples[self.current_index].clone();
        self.current_index += 1;

        // Apply projection if specified
        if let Some(indices) = &self.projection {
            let projected_values: Vec<_> = indices.iter()
                .filter_map(|&i| tuple.get(i).cloned())
                .collect();
            let mut projected_tuple = Tuple::new(projected_values);
            // Preserve row_id through projection for DML operations
            projected_tuple.row_id = tuple.row_id;
            Ok(Some(projected_tuple))
        } else {
            Ok(Some(tuple))
        }
    }

    fn schema(&self) -> Arc<Schema> {
        if let Some(indices) = &self.projection {
            let columns: Vec<_> = indices.iter()
                .map(|&i| self.schema.columns[i].clone())
                .collect();
            Arc::new(Schema { columns })
        } else {
            self.schema.clone()
        }
    }
}

/// Vector similarity search operator (k-NN search using HNSW index)
///
/// Performs efficient nearest neighbor search using HNSW indexes.
/// This operator is used when a query has the pattern:
/// ```sql
/// SELECT * FROM table ORDER BY embedding <-> query_vector LIMIT k
/// ```
pub struct VectorScanOperator {
    table_name: String,
    schema: Arc<Schema>,
    /// Pre-computed k-NN results (row_id, distance)
    results: Vec<(u64, f32)>,
    /// Full tuples from storage
    tuples: Vec<Tuple>,
    /// Current iteration index
    current_index: usize,
}

impl VectorScanOperator {
    /// Create a new vector scan operator
    ///
    /// # Arguments
    /// * `table_name` - Name of the table to scan
    /// * `schema` - Schema of the table
    /// * `results` - Pre-computed k-NN search results (row_id, distance)
    /// * `tuples` - Full tuples from storage (ordered by k-NN results)
    pub fn new(
        table_name: String,
        schema: Arc<Schema>,
        results: Vec<(u64, f32)>,
        tuples: Vec<Tuple>,
    ) -> Self {
        Self {
            table_name,
            schema,
            results,
            tuples,
            current_index: 0,
        }
    }

    /// Get the distance for the current tuple (if available)
    #[allow(dead_code)]
    pub fn current_distance(&self) -> Option<f32> {
        if self.current_index > 0 && self.current_index <= self.results.len() {
            Some(self.results[self.current_index - 1].1)
        } else {
            None
        }
    }
}

impl PhysicalOperator for VectorScanOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.current_index >= self.tuples.len() {
            return Ok(None);
        }

        let tuple = self.tuples[self.current_index].clone();
        self.current_index += 1;

        Ok(Some(tuple))
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

/// Materialized operator
///
/// Holds pre-computed tuples in memory, useful for system views and subqueries.
/// Similar to ScanOperator but without table_name or projection support.
pub struct MaterializedOperator {
    schema: Arc<Schema>,
    tuples: Vec<Tuple>,
    current_index: usize,
}

impl MaterializedOperator {
    /// Create a new materialized operator with pre-computed tuples
    pub fn new(tuples: Vec<Tuple>, schema: Arc<Schema>) -> Self {
        Self {
            schema,
            tuples,
            current_index: 0,
        }
    }
}

impl PhysicalOperator for MaterializedOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.current_index >= self.tuples.len() {
            return Ok(None);
        }

        let tuple = self.tuples[self.current_index].clone();
        self.current_index += 1;

        Ok(Some(tuple))
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

/// Handle Scan logical plan node
pub(super) fn handle_scan(
    executor: &Executor,
    plan: &LogicalPlan,
) -> Result<Box<dyn PhysicalOperator>> {
    if let LogicalPlan::Scan { table_name, alias, schema: _plan_schema, projection, as_of } = plan {
        // Use alias for column source_table (for JOIN disambiguation), fallback to table_name
        let source_name = alias.as_ref().unwrap_or(table_name);

        // First, check if this table name is a CTE reference
        if let Some(cte_data) = executor.get_cte(table_name) {
            // Return the materialized CTE data
            let mut schema_with_source = (*cte_data.schema).clone();
            for col in &mut schema_with_source.columns {
                col.source_table = Some(source_name.clone());
                col.source_table_name = Some(table_name.clone());
            }

            return Ok(Box::new(ScanOperator::new(
                table_name.clone(),
                Arc::new(schema_with_source),
                projection.clone(),
                cte_data.tuples.clone(),
                executor.parameters().to_vec(),
            ).with_timeout(executor.timeout_ctx())));
        }

        // Fetch actual schema from storage and scan table
        let (actual_schema, tuples) = if let Some(storage) = executor.storage() {
            let catalog = storage.catalog();
            let mv_catalog = storage.mv_catalog();

            // First check if it's a materialized view
            // We need to do this first because MVs are stored in __mv_<name> tables
            let (schema, actual_table_name) = if mv_catalog.view_exists(table_name)? {
                let mv_metadata = mv_catalog.get_view(table_name)?;
                let mv_data_table = crate::storage::MaterializedViewCatalog::mv_data_table_name(table_name);

                // Check if MV data table exists (view has been refreshed)
                if !catalog.table_exists(&mv_data_table)? {
                    return Err(Error::query_execution(format!(
                        "Materialized view '{}' exists but has never been refreshed. Run: REFRESH MATERIALIZED VIEW {}",
                        table_name, table_name
                    )));
                }

                (mv_metadata.schema, mv_data_table)
            } else {
                // Not an MV, try regular table
                match catalog.get_table_schema(table_name) {
                    Ok(schema) => (schema, table_name.clone()),
                    Err(e) => return Err(e),
                }
            };

            // Handle time-travel or transactional queries
            let tuples = if let Some(txn) = executor.transaction() {
                // Transactional scan: read at transaction's snapshot
                let base_tuples = storage.scan_table_at_snapshot(&actual_table_name, txn.snapshot_id())?;
                
                // Merge with write set from transaction for read-your-own-writes
                txn.merge_with_write_set(&actual_table_name, base_tuples)?
            } else if let Some(as_of_clause) = as_of {
                tracing::debug!(
                    "Time-travel query on table '{}' (actual: '{}') with AS OF clause: {:?}",
                    table_name,
                    actual_table_name,
                    as_of_clause
                );

                let snapshot_mgr = storage.snapshot_manager();

                // Handle VERSIONS BETWEEN separately - returns all versions in range
                if let crate::sql::logical_plan::AsOfClause::VersionsBetween { start, end } = as_of_clause {
                    tracing::debug!(
                        "VERSIONS BETWEEN query: start={:?}, end={:?}",
                        start, end
                    );

                    // Resolve start and end to internal LSN timestamps for version lookup
                    let start_ts = snapshot_mgr.resolve_timestamp_for_range(start, true)?;
                    let end_ts = snapshot_mgr.resolve_timestamp_for_range(end, false)?;

                    tracing::debug!(
                        "Resolved VERSIONS BETWEEN timestamps: {} to {}",
                        start_ts, end_ts
                    );

                    // Scan all versions in range
                    let versions = snapshot_mgr.scan_versions_between(&actual_table_name, start_ts, end_ts)?;

                    tracing::debug!(
                        "VERSIONS BETWEEN scan returned {} versions from table '{}'",
                        versions.len(),
                        table_name
                    );

                    // Convert raw version bytes to tuples (RocksDB handles decompression at block level)
                    let mut tuples = Vec::with_capacity(versions.len());
                    for (row_id, timestamp, value_bytes) in versions {
                        // Deserialize tuple directly (RocksDB LZ4 handles decompression)
                        match bincode::deserialize::<crate::Tuple>(&value_bytes) {
                            Ok(mut tuple) => {
                                tuple.row_id = Some(row_id);
                                tuples.push(tuple);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to deserialize version at row_id={}, timestamp={}: {} (data len={})",
                                    row_id, timestamp, e, value_bytes.len()
                                );
                            }
                        }
                    }

                    tuples
                } else {
                    // Regular AS OF query - single point in time
                    // Resolve AS OF clause to snapshot timestamp
                    // Supports: AS OF TIMESTAMP '...', AS OF TRANSACTION <id>, AS OF SCN <id>
                    let snapshot_ts = snapshot_mgr.resolve_as_of(as_of_clause)
                        .map_err(|e| {
                            tracing::error!(
                                "Failed to resolve AS OF clause {:?} for table '{}': {}",
                                as_of_clause,
                                table_name,
                                e
                            );
                            e
                        })?;

                    tracing::debug!(
                        "Resolved AS OF clause to snapshot timestamp {} for table '{}'",
                        snapshot_ts,
                        table_name
                    );

                    // Scan at historical snapshot (use actual_table_name for MV support)
                    let result = storage.scan_table_at_snapshot(&actual_table_name, snapshot_ts)?;

                    tracing::debug!(
                        "Time-travel scan returned {} tuples from table '{}' at snapshot {}",
                        result.len(),
                        table_name,
                        snapshot_ts
                    );

                    result
                }
            } else {
                // Normal scan (current data) with branch isolation
                // Use actual_table_name to support materialized views
                storage.scan_table_branch_aware(&actual_table_name)?
            };

            // Set source_table (alias) and source_table_name (actual) on each column for JOIN disambiguation
            // This allows both `e.name` (alias) and `employees.name` (full name) syntax in queries
            let schema_with_source = Schema {
                columns: schema.columns.into_iter().map(|mut col| {
                    col.source_table = Some(source_name.clone());
                    col.source_table_name = Some(table_name.clone());
                    col
                }).collect(),
            };
            (Arc::new(schema_with_source), tuples)
        } else {
            // No storage, use placeholder schema from plan
            (_plan_schema.clone(), Vec::new())
        };

        Ok(Box::new(ScanOperator::new(
            table_name.clone(),
            actual_schema,
            projection.clone(),
            tuples,
            executor.parameters().to_vec(),
        ).with_timeout(executor.timeout_ctx())))
    } else {
        Err(Error::query_execution("Expected Scan plan node"))
    }
}

/// Handle FilteredScan logical plan node
///
/// This handles scans with storage-level predicate pushdown, using bloom filters,
/// zone maps, and SIMD-accelerated filtering for improved performance.
pub(super) fn handle_filtered_scan(
    executor: &Executor,
    plan: &LogicalPlan,
) -> Result<Box<dyn PhysicalOperator>> {
    if let LogicalPlan::FilteredScan { table_name, alias, schema: _plan_schema, projection, predicate, as_of } = plan {
        // Use alias for column source_table (for JOIN disambiguation), fallback to table_name
        let source_name = alias.as_ref().unwrap_or(table_name);

        // First, check if this table name is a CTE reference
        if let Some(cte_data) = executor.get_cte(table_name) {
            // Return the materialized CTE data with filter applied
            let mut schema_with_source = (*cte_data.schema).clone();
            for col in &mut schema_with_source.columns {
                col.source_table = Some(source_name.clone());
                col.source_table_name = Some(table_name.clone());
            }

            let schema_arc = Arc::new(schema_with_source);
            let scan_op = Box::new(ScanOperator::new(
                table_name.clone(),
                schema_arc.clone(),
                projection.clone(),
                cte_data.tuples.clone(),
                executor.parameters().to_vec(),
            ).with_timeout(executor.timeout_ctx()));

            // Apply filter if predicate exists
            if let Some(pred) = predicate {
                let materialized_pred = executor.materialize_subqueries(pred)?;
                return Ok(Box::new(super::filter::FilterOperator::new(
                    scan_op,
                    materialized_pred,
                    executor.parameters().to_vec(),
                )));
            }

            return Ok(scan_op);
        }

        // Fetch actual schema from storage and scan table with filtering
        let (actual_schema, tuples) = if let Some(storage) = executor.storage() {
            let catalog = storage.catalog();
            let mv_catalog = storage.mv_catalog();

            // First check if it's a materialized view
            let (schema, actual_table_name) = if mv_catalog.view_exists(table_name)? {
                let mv_metadata = mv_catalog.get_view(table_name)?;
                let mv_data_table = crate::storage::MaterializedViewCatalog::mv_data_table_name(table_name);

                // Check if MV data table exists (view has been refreshed)
                if !catalog.table_exists(&mv_data_table)? {
                    return Err(Error::query_execution(format!(
                        "Materialized view '{}' exists but has never been refreshed. Run: REFRESH MATERIALIZED VIEW {}",
                        table_name, table_name
                    )));
                }

                (mv_metadata.schema, mv_data_table)
            } else {
                // Not an MV, try regular table
                match catalog.get_table_schema(table_name) {
                    Ok(schema) => (schema, table_name.clone()),
                    Err(e) => return Err(e),
                }
            };

            // Materialize any IN subqueries before storage-level pushdown
            // This converts InSubquery to InList which storage layer can handle
            let materialized_predicate = if let Some(pred) = predicate {
                Some(executor.materialize_subqueries(pred)?)
            } else {
                None
            };

            // Analyze the predicate for storage-level pushdown
            let analyzed_predicates = if let Some(ref pred) = materialized_predicate {
                storage.predicate_pushdown().analyze_predicate(pred, &schema)
            } else {
                Vec::new()
            };

            tracing::debug!(
                "FilteredScan on table '{}': analyzed {} predicates for pushdown",
                table_name,
                analyzed_predicates.len()
            );

            // Handle time-travel or transactional queries with filtered scan
            let tuples = if let Some(txn) = executor.transaction() {
                // Transactional scan: read at transaction's snapshot
                let base_tuples = storage.scan_table_at_snapshot(&actual_table_name, txn.snapshot_id())?;
                
                // Merge with write set
                let merged_tuples = txn.merge_with_write_set(&actual_table_name, base_tuples)?;
                
                // Apply storage-level filtering (on the merged set)
                storage.predicate_pushdown().scan_with_pushdown(
                    &actual_table_name,
                    merged_tuples,
                    &analyzed_predicates,
                    &schema,
                    None,
                )
            } else if let Some(as_of_clause) = as_of {
                tracing::debug!(
                    "Time-travel FilteredScan on table '{}' with AS OF clause: {:?}",
                    table_name,
                    as_of_clause
                );

                // Resolve AS OF clause to snapshot timestamp
                let snapshot_mgr = storage.snapshot_manager();
                let snapshot_ts = snapshot_mgr.resolve_as_of(as_of_clause)?;

                // Scan at historical snapshot, then apply filtering
                let base_tuples = storage.scan_table_at_snapshot(&actual_table_name, snapshot_ts)?;

                // Apply storage-level filtering
                storage.predicate_pushdown().scan_with_pushdown(
                    &actual_table_name,
                    base_tuples,
                    &analyzed_predicates,
                    &schema,
                    None, // No limit at storage level
                )
            } else {
                // Normal filtered scan (current data) with branch isolation
                let base_tuples = storage.scan_table_branch_aware(&actual_table_name)?;

                // Apply storage-level filtering through predicate pushdown manager
                storage.predicate_pushdown().scan_with_pushdown(
                    &actual_table_name,
                    base_tuples,
                    &analyzed_predicates,
                    &schema,
                    None, // No limit at storage level
                )
            };

            tracing::debug!(
                "FilteredScan returned {} tuples after predicate pushdown",
                tuples.len()
            );

            // Set source_table (alias) and source_table_name (actual) on each column for JOIN disambiguation
            // This allows both `e.name` (alias) and `employees.name` (full name) syntax in queries
            let schema_with_source = Schema {
                columns: schema.columns.into_iter().map(|mut col| {
                    col.source_table = Some(source_name.clone());
                    col.source_table_name = Some(table_name.clone());
                    col
                }).collect(),
            };
            (Arc::new(schema_with_source), tuples)
        } else {
            // No storage, use placeholder schema from plan
            (_plan_schema.clone(), Vec::new())
        };

        Ok(Box::new(ScanOperator::new(
            table_name.clone(),
            actual_schema,
            projection.clone(),
            tuples,
            executor.parameters().to_vec(),
        ).with_timeout(executor.timeout_ctx())))
    } else {
        Err(Error::query_execution("Expected FilteredScan plan node"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Column;
    use crate::DataType;

    #[test]
    fn test_scan_operator_empty() {
        let schema = Arc::new(Schema {
            columns: vec![Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            }],
        });

        let mut scan = ScanOperator::new("test".to_string(), schema.clone(), None, Vec::new(), Vec::new());
        assert!(scan.next().expect("Failed to execute scan").is_none());
    }
}
