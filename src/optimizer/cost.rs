//! Cost model and statistics for query optimization
//!
//! Provides table statistics, cardinality estimation, and cost calculations
//! for choosing optimal query execution plans.

use crate::sql::logical_plan::{LogicalPlan, LogicalExpr, BinaryOperator};
use crate::{Result, Error};
use std::collections::HashMap;

/// Table statistics for cost estimation
#[derive(Debug, Clone)]
pub struct TableStats {
    /// Table name
    pub table_name: String,
    /// Estimated number of rows
    pub row_count: u64,
    /// Average row size in bytes
    pub avg_row_size: usize,
    /// Column statistics
    pub column_stats: HashMap<String, ColumnStats>,
}

/// Column-level statistics
#[derive(Debug, Clone)]
pub struct ColumnStats {
    /// Column name
    pub name: String,
    /// Number of distinct values (cardinality)
    pub distinct_count: u64,
    /// Number of null values
    pub null_count: u64,
    /// Minimum value (if applicable)
    pub min_value: Option<String>,
    /// Maximum value (if applicable)
    pub max_value: Option<String>,
    /// Whether an index exists on this column
    pub has_index: bool,
    /// Index type if exists (btree, hash, hnsw, gin, etc.)
    pub index_type: Option<String>,
}

impl Default for TableStats {
    fn default() -> Self {
        Self {
            table_name: String::new(),
            row_count: 0,
            avg_row_size: 0,
            column_stats: HashMap::new(),
        }
    }
}

impl TableStats {
    /// Create new table statistics
    pub fn new(table_name: String) -> Self {
        Self {
            table_name,
            row_count: 0,
            avg_row_size: 0,
            column_stats: HashMap::new(),
        }
    }

    /// Update row count
    pub fn with_row_count(mut self, row_count: u64) -> Self {
        self.row_count = row_count;
        self
    }

    /// Update average row size
    pub fn with_avg_row_size(mut self, avg_row_size: usize) -> Self {
        self.avg_row_size = avg_row_size;
        self
    }

    /// Add column statistics
    pub fn with_column_stats(mut self, stats: ColumnStats) -> Self {
        self.column_stats.insert(stats.name.clone(), stats);
        self
    }

    /// Get estimated table size in bytes
    pub fn estimated_size(&self) -> u64 {
        self.row_count * (self.avg_row_size as u64)
    }

    /// Get column statistics by name
    pub fn get_column_stats(&self, column_name: &str) -> Option<&ColumnStats> {
        self.column_stats.get(column_name)
    }
}

impl ColumnStats {
    /// Create new column statistics
    pub fn new(name: String) -> Self {
        Self {
            name,
            distinct_count: 0,
            null_count: 0,
            min_value: None,
            max_value: None,
            has_index: false,
            index_type: None,
        }
    }

    /// Update distinct count
    pub fn with_distinct_count(mut self, count: u64) -> Self {
        self.distinct_count = count;
        self
    }

    /// Update null count
    pub fn with_null_count(mut self, count: u64) -> Self {
        self.null_count = count;
        self
    }

    /// Mark as indexed
    pub fn with_index(mut self, index_type: String) -> Self {
        self.has_index = true;
        self.index_type = Some(index_type);
        self
    }

    /// Calculate selectivity (0.0 to 1.0)
    /// Returns fraction of rows expected to pass through filter
    pub fn estimate_selectivity(&self, _operator: &BinaryOperator) -> f64 {
        // Default selectivity estimates based on operator type
        // This is a simplified model; a real implementation would use histograms
        match _operator {
            BinaryOperator::Eq => {
                // For equality, selectivity = 1 / distinct_count
                if self.distinct_count > 0 {
                    1.0 / (self.distinct_count as f64)
                } else {
                    0.1 // Default estimate
                }
            }
            BinaryOperator::NotEq => {
                // NOT equals is complement of equals
                if self.distinct_count > 0 {
                    1.0 - (1.0 / (self.distinct_count as f64))
                } else {
                    0.9 // Default estimate
                }
            }
            BinaryOperator::Lt | BinaryOperator::LtEq |
            BinaryOperator::Gt | BinaryOperator::GtEq => {
                // Range predicates - assume 1/3 selectivity
                0.33
            }
            BinaryOperator::Like | BinaryOperator::NotLike => {
                // LIKE is highly variable, use conservative estimate
                0.5
            }
            _ => 0.5, // Default for other operators
        }
    }
}

/// Statistics catalog - maintains statistics for all tables
#[derive(Debug, Clone, Default)]
pub struct StatsCatalog {
    /// Table statistics indexed by table name
    tables: HashMap<String, TableStats>,
}

impl StatsCatalog {
    /// Create a new statistics catalog
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
        }
    }

    /// Add or update table statistics
    pub fn add_table_stats(&mut self, stats: TableStats) {
        self.tables.insert(stats.table_name.clone(), stats);
    }

    /// Get table statistics
    pub fn get_table_stats(&self, table_name: &str) -> Option<&TableStats> {
        self.tables.get(table_name)
    }

    /// Get mutable table statistics
    pub fn get_table_stats_mut(&mut self, table_name: &str) -> Option<&mut TableStats> {
        self.tables.get_mut(table_name)
    }

    /// Remove table statistics
    pub fn remove_table_stats(&mut self, table_name: &str) {
        self.tables.remove(table_name);
    }

    /// Create default statistics for a table (used when no stats available)
    pub fn create_default_stats(&mut self, table_name: &str) {
        let stats = TableStats::new(table_name.to_string())
            .with_row_count(1000) // Default estimate
            .with_avg_row_size(256); // Default estimate
        self.add_table_stats(stats);
    }
}

/// Cost estimator for query plans
pub struct CostEstimator {
    /// Statistics catalog
    stats: StatsCatalog,
    /// Cost parameters
    params: CostParameters,
}

/// Cost calculation parameters
#[derive(Debug, Clone)]
pub struct CostParameters {
    /// Cost per row scanned
    pub seq_scan_cost: f64,
    /// Cost per row from index lookup
    pub index_scan_cost: f64,
    /// Cost per row comparison (CPU cost)
    pub cpu_tuple_cost: f64,
    /// Cost per page random I/O
    pub random_page_cost: f64,
    /// Cost per page sequential I/O
    pub seq_page_cost: f64,
    /// Page size in bytes
    pub page_size: usize,
}

impl Default for CostParameters {
    fn default() -> Self {
        Self {
            // PostgreSQL-inspired defaults
            seq_scan_cost: 1.0,
            index_scan_cost: 0.005,
            cpu_tuple_cost: 0.01,
            random_page_cost: 4.0,
            seq_page_cost: 1.0,
            page_size: 8192, // 8KB pages
        }
    }
}

impl CostEstimator {
    /// Create a new cost estimator with statistics
    pub fn new(stats: StatsCatalog) -> Self {
        Self {
            stats,
            params: CostParameters::default(),
        }
    }

    /// Create with custom parameters
    pub fn with_params(stats: StatsCatalog, params: CostParameters) -> Self {
        Self { stats, params }
    }

    /// Get statistics catalog reference
    pub fn stats(&self) -> &StatsCatalog {
        &self.stats
    }

    /// Get mutable statistics catalog reference
    pub fn stats_mut(&mut self) -> &mut StatsCatalog {
        &mut self.stats
    }

    /// Estimate the cost of executing a logical plan
    pub fn estimate_cost(&self, plan: &LogicalPlan) -> Result<f64> {
        match plan {
            LogicalPlan::Scan { table_name, projection, .. } => {
                self.estimate_scan_cost(table_name, projection.as_ref())
            }
            LogicalPlan::Filter { input, predicate } => {
                let input_cost = self.estimate_cost(input)?;
                let filter_cost = self.estimate_filter_cost(input, predicate)?;
                Ok(input_cost + filter_cost)
            }
            LogicalPlan::Project { input, exprs, .. } => {
                let input_cost = self.estimate_cost(input)?;
                let cardinality = self.estimate_cardinality(input)?;
                let project_cost = cardinality * self.params.cpu_tuple_cost * (exprs.len() as f64);
                Ok(input_cost + project_cost)
            }
            LogicalPlan::Join { left, right, .. } => {
                self.estimate_join_cost(left, right)
            }
            LogicalPlan::Aggregate { input, group_by, aggr_exprs, .. } => {
                let input_cost = self.estimate_cost(input)?;
                let cardinality = self.estimate_cardinality(input)?;
                // Cost of aggregation is roughly O(n log n) for sorting/hashing
                let agg_cost = cardinality * cardinality.ln() * self.params.cpu_tuple_cost
                    * ((group_by.len() + aggr_exprs.len()) as f64);
                Ok(input_cost + agg_cost)
            }
            LogicalPlan::Sort { input, exprs, .. } => {
                let input_cost = self.estimate_cost(input)?;
                let cardinality = self.estimate_cardinality(input)?;
                // O(n log n) sorting cost
                let sort_cost = cardinality * cardinality.ln() * self.params.cpu_tuple_cost
                    * (exprs.len() as f64);
                Ok(input_cost + sort_cost)
            }
            LogicalPlan::Limit { input, limit, .. } => {
                let input_cost = self.estimate_cost(input)?;
                let cardinality = self.estimate_cardinality(input)?;
                // Limit can short-circuit execution
                let limit_factor = (*limit as f64) / cardinality.max(1.0);
                Ok(input_cost * limit_factor.min(1.0))
            }
            _ => {
                // For DDL and other operations, return minimal cost
                Ok(1.0)
            }
        }
    }

    /// Estimate cardinality (number of rows) for a plan
    pub fn estimate_cardinality(&self, plan: &LogicalPlan) -> Result<f64> {
        match plan {
            LogicalPlan::Scan { table_name, .. } => {
                let stats = self.stats.get_table_stats(table_name)
                    .ok_or_else(|| Error::query_execution(
                        format!("No statistics available for table '{}'", table_name)
                    ))?;
                Ok(stats.row_count as f64)
            }
            LogicalPlan::Filter { input, predicate } => {
                let input_cardinality = self.estimate_cardinality(input)?;
                let selectivity = self.estimate_selectivity(input, predicate)?;
                Ok(input_cardinality * selectivity)
            }
            LogicalPlan::Project { input, .. } => {
                // Projection doesn't change cardinality
                self.estimate_cardinality(input)
            }
            LogicalPlan::Join { left, right, .. } => {
                // For simplicity, assume cross product / 10 (very conservative)
                let left_card = self.estimate_cardinality(left)?;
                let right_card = self.estimate_cardinality(right)?;
                Ok((left_card * right_card) / 10.0)
            }
            LogicalPlan::Aggregate { input, group_by, .. } => {
                if group_by.is_empty() {
                    // No GROUP BY - single aggregate row
                    Ok(1.0)
                } else {
                    // Estimate based on distinct values in group by columns
                    let input_cardinality = self.estimate_cardinality(input)?;
                    // Assume 10% reduction in cardinality from grouping
                    Ok(input_cardinality * 0.1)
                }
            }
            LogicalPlan::Sort { input, .. } => {
                // Sort doesn't change cardinality
                self.estimate_cardinality(input)
            }
            LogicalPlan::Limit { limit, .. } => {
                Ok(*limit as f64)
            }
            _ => Ok(1.0),
        }
    }

    /// Estimate scan cost
    fn estimate_scan_cost(&self, table_name: &str, projection: Option<&Vec<usize>>) -> Result<f64> {
        let stats = self.stats.get_table_stats(table_name)
            .ok_or_else(|| Error::query_execution(
                format!("No statistics available for table '{}'", table_name)
            ))?;

        let row_count = stats.row_count as f64;
        let row_size = stats.avg_row_size as f64;

        // Calculate number of pages to scan
        let total_bytes = row_count * row_size;
        let pages = (total_bytes / self.params.page_size as f64).ceil();

        // Sequential scan cost = pages * seq_page_cost + rows * cpu_tuple_cost
        let io_cost = pages * self.params.seq_page_cost;
        let cpu_cost = row_count * self.params.cpu_tuple_cost;

        // If projection, reduce CPU cost proportionally
        let projection_factor = if let Some(proj) = projection {
            (proj.len() as f64) / stats.column_stats.len().max(1) as f64
        } else {
            1.0
        };

        Ok(io_cost + (cpu_cost * projection_factor))
    }

    /// Estimate filter cost
    fn estimate_filter_cost(&self, input: &LogicalPlan, predicate: &LogicalExpr) -> Result<f64> {
        let cardinality = self.estimate_cardinality(input)?;
        // Cost of evaluating predicate on each row
        let eval_cost = cardinality * self.params.cpu_tuple_cost * Self::estimate_expr_complexity(predicate);
        Ok(eval_cost)
    }

    /// Estimate join cost
    fn estimate_join_cost(&self, left: &LogicalPlan, right: &LogicalPlan) -> Result<f64> {
        let left_cost = self.estimate_cost(left)?;
        let right_cost = self.estimate_cost(right)?;
        let left_card = self.estimate_cardinality(left)?;
        let right_card = self.estimate_cardinality(right)?;

        // For now, assume hash join: O(left + right)
        // Build hash table on smaller side
        let build_cost = left_card.min(right_card) * self.params.cpu_tuple_cost * 2.0;
        let probe_cost = left_card.max(right_card) * self.params.cpu_tuple_cost;

        Ok(left_cost + right_cost + build_cost + probe_cost)
    }

    /// Estimate selectivity of a predicate (fraction of rows that pass)
    fn estimate_selectivity(&self, plan: &LogicalPlan, predicate: &LogicalExpr) -> Result<f64> {
        match predicate {
            LogicalExpr::BinaryExpr { left, op, right } => {
                match op {
                    BinaryOperator::And => {
                        // Selectivity of AND is product of selectivities
                        let left_sel = self.estimate_selectivity(plan, left)?;
                        let right_sel = self.estimate_selectivity(plan, right)?;
                        Ok(left_sel * right_sel)
                    }
                    BinaryOperator::Or => {
                        // Selectivity of OR is 1 - (1-a)(1-b)
                        let left_sel = self.estimate_selectivity(plan, left)?;
                        let right_sel = self.estimate_selectivity(plan, right)?;
                        Ok(1.0 - (1.0 - left_sel) * (1.0 - right_sel))
                    }
                    _ => {
                        // For comparison operators, try to use column statistics
                        if let LogicalExpr::Column { name, .. } = left.as_ref() {
                            if let Some(table_name) = Self::extract_table_name(plan) {
                                if let Some(stats) = self.stats.get_table_stats(&table_name) {
                                    if let Some(col_stats) = stats.get_column_stats(name) {
                                        return Ok(col_stats.estimate_selectivity(op));
                                    }
                                }
                            }
                        }
                        // Default selectivity
                        Ok(0.33)
                    }
                }
            }
            LogicalExpr::UnaryExpr { op: _, expr } => {
                // NOT inverts selectivity
                let inner_sel = self.estimate_selectivity(plan, expr)?;
                Ok(1.0 - inner_sel)
            }
            LogicalExpr::IsNull { .. } => Ok(0.1), // Assume 10% null values
            LogicalExpr::InList { list, .. } => {
                // Selectivity proportional to list size, capped at 50%
                Ok((list.len() as f64 * 0.1).min(0.5))
            }
            _ => Ok(0.5), // Default conservative estimate
        }
    }

    /// Estimate expression complexity (for cost calculation)
    fn estimate_expr_complexity(expr: &LogicalExpr) -> f64 {
        match expr {
            LogicalExpr::Column { .. } | LogicalExpr::Literal(_) => 1.0,
            LogicalExpr::BinaryExpr { left, right, .. } => {
                2.0 + Self::estimate_expr_complexity(left) + Self::estimate_expr_complexity(right)
            }
            LogicalExpr::UnaryExpr { expr, .. } => {
                1.5 + Self::estimate_expr_complexity(expr)
            }
            LogicalExpr::ScalarFunction { args, .. } => {
                let arg_complexity: f64 = args.iter()
                    .map(|arg| Self::estimate_expr_complexity(arg))
                    .sum();
                5.0 + arg_complexity // Functions are more expensive
            }
            LogicalExpr::AggregateFunction { args, .. } => {
                let arg_complexity: f64 = args.iter()
                    .map(|arg| Self::estimate_expr_complexity(arg))
                    .sum();
                10.0 + arg_complexity // Aggregates are expensive
            }
            LogicalExpr::Case { when_then, else_result, .. } => {
                let when_cost: f64 = when_then.iter()
                    .map(|(cond, result)| {
                        Self::estimate_expr_complexity(cond) + Self::estimate_expr_complexity(result)
                    })
                    .sum();
                let else_cost = else_result.as_ref()
                    .map(|e| Self::estimate_expr_complexity(e))
                    .unwrap_or(0.0);
                3.0 + when_cost + else_cost
            }
            LogicalExpr::Cast { expr, .. } => {
                2.0 + Self::estimate_expr_complexity(expr)
            }
            LogicalExpr::IsNull { expr, .. } => {
                1.5 + Self::estimate_expr_complexity(expr)
            }
            LogicalExpr::Between { expr, low, high, .. } => {
                3.0 + Self::estimate_expr_complexity(expr)
                    + Self::estimate_expr_complexity(low)
                    + Self::estimate_expr_complexity(high)
            }
            LogicalExpr::InList { expr, list, .. } => {
                let list_cost: f64 = list.iter()
                    .map(|e| Self::estimate_expr_complexity(e))
                    .sum();
                2.0 + Self::estimate_expr_complexity(expr) + list_cost
            }
            LogicalExpr::InSet { expr, values, .. } => {
                // HashSet lookup is O(1) per value, so cost is just the expr + constant per entry
                2.0 + Self::estimate_expr_complexity(expr) + values.len() as f64
            }
            LogicalExpr::InSubquery { expr, .. } => {
                // Subqueries are expensive - estimate high cost
                100.0 + Self::estimate_expr_complexity(expr)
            }
            LogicalExpr::ScalarSubquery { .. } => {
                // Scalar subqueries — full plan execution
                100.0
            }
            LogicalExpr::DefaultValue => 0.0,
            LogicalExpr::Exists { .. } => {
                // EXISTS subqueries are expensive
                100.0
            }
            LogicalExpr::NewRow { .. } | LogicalExpr::OldRow { .. } => 1.0, // Similar to column access
            LogicalExpr::ArraySubscript { array, index } => {
                3.0 + Self::estimate_expr_complexity(array) + Self::estimate_expr_complexity(index)
            }
            LogicalExpr::WindowFunction { args, partition_by, order_by, .. } => {
                // Window functions are expensive due to partitioning and sorting
                let arg_cost: f64 = args.iter()
                    .map(|e| Self::estimate_expr_complexity(e))
                    .sum();
                let partition_cost: f64 = partition_by.iter()
                    .map(|e| Self::estimate_expr_complexity(e))
                    .sum();
                let order_cost: f64 = order_by.iter()
                    .map(|(e, _)| Self::estimate_expr_complexity(e))
                    .sum();
                50.0 + arg_cost + partition_cost + order_cost
            }
            LogicalExpr::Tuple { items } => {
                1.0 + items.iter().map(Self::estimate_expr_complexity).sum::<f64>()
            }
            LogicalExpr::Wildcard | LogicalExpr::Parameter { .. } => 1.0,
        }
    }

    /// Extract table name from a plan (for statistics lookup)
    fn extract_table_name(plan: &LogicalPlan) -> Option<String> {
        match plan {
            LogicalPlan::Scan { table_name, .. } => Some(table_name.clone()),
            LogicalPlan::Filter { input, .. } |
            LogicalPlan::Project { input, .. } |
            LogicalPlan::Sort { input, .. } |
            LogicalPlan::Limit { input, .. } |
            LogicalPlan::Aggregate { input, .. } => Self::extract_table_name(input),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::logical_plan::*;
    use crate::{Schema, Column, DataType};
    use std::sync::Arc;

    #[test]
    fn test_table_stats_creation() {
        let stats = TableStats::new("users".to_string())
            .with_row_count(1000)
            .with_avg_row_size(256);

        assert_eq!(stats.table_name, "users");
        assert_eq!(stats.row_count, 1000);
        assert_eq!(stats.avg_row_size, 256);
        assert_eq!(stats.estimated_size(), 256_000);
    }

    #[test]
    fn test_column_stats_selectivity() {
        let col_stats = ColumnStats::new("status".to_string())
            .with_distinct_count(5); // 5 distinct statuses

        // Equality on column with 5 distinct values should have ~20% selectivity
        let selectivity = col_stats.estimate_selectivity(&BinaryOperator::Eq);
        assert!((selectivity - 0.2).abs() < 0.01);
    }

    #[test]
    fn test_stats_catalog() {
        let mut catalog = StatsCatalog::new();

        let stats = TableStats::new("users".to_string())
            .with_row_count(1000);

        catalog.add_table_stats(stats);

        assert!(catalog.get_table_stats("users").is_some());
        assert!(catalog.get_table_stats("orders").is_none());
    }

    #[test]
    fn test_cost_estimation_scan() {
        let mut stats_catalog = StatsCatalog::new();
        stats_catalog.add_table_stats(
            TableStats::new("users".to_string())
                .with_row_count(1000)
                .with_avg_row_size(256)
        );

        let estimator = CostEstimator::new(stats_catalog);

        let schema = Arc::new(Schema {
            columns: vec![
                Column {
                    name: "id".to_string(),
                    data_type: DataType::Int4,
                    nullable: false,
                    primary_key: true,
                    source_table: None,
                    source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
                }
            ],
        });

        let plan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema,
            projection: None,
            as_of: None,
        };

        let cost = estimator.estimate_cost(&plan);
        assert!(cost.is_ok());
        assert!(cost.unwrap() > 0.0);
    }

    #[test]
    fn test_cardinality_estimation() {
        let mut stats_catalog = StatsCatalog::new();
        stats_catalog.add_table_stats(
            TableStats::new("users".to_string())
                .with_row_count(1000)
                .with_avg_row_size(256)
        );

        let estimator = CostEstimator::new(stats_catalog);

        let schema = Arc::new(Schema {
            columns: vec![
                Column {
                    name: "id".to_string(),
                    data_type: DataType::Int4,
                    nullable: false,
                    primary_key: true,
                    source_table: None,
                    source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
                }
            ],
        });

        let scan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema: schema.clone(),
            projection: None,
            as_of: None,
        };

        let filter = LogicalPlan::Filter {
            input: Box::new(scan),
            predicate: LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column { table: None, name: "id".to_string()  }),
                op: BinaryOperator::Eq,
                right: Box::new(LogicalExpr::Literal(crate::Value::Int4(1))),
            },
        };

        let cardinality = estimator.estimate_cardinality(&filter);
        assert!(cardinality.is_ok());
        // Filter should reduce cardinality
        assert!(cardinality.unwrap() < 1000.0);
    }
}
