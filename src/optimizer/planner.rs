//! Physical query planner
//!
//! Converts optimized logical plans into executable physical plans.
//! The physical planner makes concrete decisions about execution strategies,
//! index selection, join algorithms, and operator implementations.

use crate::sql::logical_plan::{LogicalPlan, LogicalExpr, JoinType, AsOfClause};
use crate::optimizer::cost::CostEstimator;
use crate::{Result, Error, Schema};
use std::sync::Arc;

/// Physical execution plan
///
/// Represents a concrete execution strategy with specific operator implementations.
#[derive(Debug, Clone)]
pub enum PhysicalPlan {
    /// Sequential table scan
    TableScan {
        /// Table name
        table_name: String,
        /// Table schema
        schema: Arc<Schema>,
        /// Column indices to read (None = all columns)
        projection: Option<Vec<usize>>,
        /// Time-travel AS OF clause
        as_of: Option<AsOfClause>,
    },

    /// Filter operator (evaluates predicate for each row)
    Filter {
        /// Input plan
        input: Box<PhysicalPlan>,
        /// Filter predicate
        predicate: LogicalExpr,
    },

    /// Projection operator (selects columns)
    Projection {
        /// Input plan
        input: Box<PhysicalPlan>,
        /// Expressions to evaluate
        exprs: Vec<LogicalExpr>,
        /// Output column names
        aliases: Vec<String>,
    },

    /// Hash join operator (optimal for large tables with equality joins)
    HashJoin {
        /// Left (build) side
        left: Box<PhysicalPlan>,
        /// Right (probe) side
        right: Box<PhysicalPlan>,
        /// Join type
        join_type: JoinType,
        /// Join condition
        on: Option<LogicalExpr>,
    },

    /// Nested loop join operator (optimal for small tables or non-equality joins)
    NestedLoopJoin {
        /// Left (outer) side
        left: Box<PhysicalPlan>,
        /// Right (inner) side
        right: Box<PhysicalPlan>,
        /// Join type
        join_type: JoinType,
        /// Join condition
        on: Option<LogicalExpr>,
    },

    /// Hash aggregate operator
    HashAggregate {
        /// Input plan
        input: Box<PhysicalPlan>,
        /// Group by expressions
        group_by: Vec<LogicalExpr>,
        /// Aggregate expressions
        aggr_exprs: Vec<LogicalExpr>,
        /// Optional HAVING filter
        having: Option<LogicalExpr>,
    },

    /// Sort operator
    Sort {
        /// Input plan
        input: Box<PhysicalPlan>,
        /// Sort expressions
        exprs: Vec<LogicalExpr>,
        /// Sort direction (true = ascending)
        asc: Vec<bool>,
    },

    /// Limit operator
    Limit {
        /// Input plan
        input: Box<PhysicalPlan>,
        /// Number of rows to return
        limit: usize,
        /// Number of rows to skip
        offset: usize,
    },
}

/// Physical query planner
///
/// Converts logical plans (which describe WHAT to do) into physical plans
/// (which describe HOW to do it). Makes concrete decisions about:
/// - Join algorithms (hash join vs nested loop)
/// - Index selection
/// - Operator implementations
/// - Parallelization strategies
pub struct Planner {
    /// Enable verbose planning output
    verbose: bool,
    /// Cost estimator for making optimization decisions
    cost_estimator: Option<CostEstimator>,
}

impl Default for Planner {
    fn default() -> Self {
        Self::new()
    }
}

impl Planner {
    /// Create a new physical planner without cost estimation
    pub fn new() -> Self {
        Self {
            verbose: false,
            cost_estimator: None,
        }
    }

    /// Create a planner with verbose output
    pub fn with_verbose(verbose: bool) -> Self {
        Self {
            verbose,
            cost_estimator: None,
        }
    }

    /// Create a planner with cost-based optimization
    pub fn with_cost_estimator(cost_estimator: CostEstimator) -> Self {
        Self {
            verbose: false,
            cost_estimator: Some(cost_estimator),
        }
    }

    /// Create a planner with both verbose output and cost estimation
    pub fn with_verbose_and_cost(verbose: bool, cost_estimator: CostEstimator) -> Self {
        Self {
            verbose,
            cost_estimator: Some(cost_estimator),
        }
    }

    /// Convert logical plan to physical plan
    ///
    /// This is the main entry point for physical planning. It recursively
    /// converts each logical operator into its physical counterpart.
    pub fn plan(&self, logical: LogicalPlan) -> Result<PhysicalPlan> {
        self.plan_recursive(logical)
    }

    /// Recursively convert logical plan nodes to physical plan nodes
    fn plan_recursive(&self, logical: LogicalPlan) -> Result<PhysicalPlan> {
        let physical = match logical {
            // Table scan - direct conversion
            LogicalPlan::Scan { table_name, alias, schema, projection, as_of } => {
                if self.verbose {
                    eprintln!("Planning: TableScan({})", table_name);
                }
                PhysicalPlan::TableScan {
                    table_name,
                    schema,
                    projection,
                    as_of,
                }
            }

            // Filtered scan - table scan with predicate pushed down to storage layer
            LogicalPlan::FilteredScan { table_name, alias: _, schema, projection, predicate, as_of } => {
                if self.verbose {
                    eprintln!("Planning: FilteredScan({})", table_name);
                }
                let scan = PhysicalPlan::TableScan {
                    table_name,
                    schema,
                    projection,
                    as_of,
                };
                // If there's a predicate, wrap in a filter
                if let Some(pred) = predicate {
                    PhysicalPlan::Filter {
                        input: Box::new(scan),
                        predicate: pred,
                    }
                } else {
                    scan
                }
            }

            // Filter - plan input, then add filter
            LogicalPlan::Filter { input, predicate } => {
                if self.verbose {
                    eprintln!("Planning: Filter");
                }
                let physical_input = self.plan_recursive(*input)?;
                PhysicalPlan::Filter {
                    input: Box::new(physical_input),
                    predicate,
                }
            }

            // Projection - plan input, then add projection
            LogicalPlan::Project { input, exprs, aliases, distinct: _, distinct_on: _ } => {
                if self.verbose {
                    eprintln!("Planning: Projection ({} columns)", exprs.len());
                }
                let physical_input = self.plan_recursive(*input)?;
                PhysicalPlan::Projection {
                    input: Box::new(physical_input),
                    exprs,
                    aliases,
                }
            }

            // Join - use cost-based decision between hash join and nested loop join
            LogicalPlan::Join { left, right, join_type, on } => {
                // Choose join algorithm based on cost estimation (before consuming left/right)
                let use_hash_join = self.should_use_hash_join(&left, &right, &on)?;

                let physical_left = self.plan_recursive(*left)?;
                let physical_right = self.plan_recursive(*right)?;

                if use_hash_join {
                    if self.verbose {
                        eprintln!("Planning: HashJoin ({:?})", join_type);
                    }
                    PhysicalPlan::HashJoin {
                        left: Box::new(physical_left),
                        right: Box::new(physical_right),
                        join_type,
                        on,
                    }
                } else {
                    if self.verbose {
                        eprintln!("Planning: NestedLoopJoin ({:?})", join_type);
                    }
                    PhysicalPlan::NestedLoopJoin {
                        left: Box::new(physical_left),
                        right: Box::new(physical_right),
                        join_type,
                        on,
                    }
                }
            }

            // Aggregation - use hash aggregation
            LogicalPlan::Aggregate { input, group_by, aggr_exprs, having } => {
                if self.verbose {
                    eprintln!("Planning: HashAggregate");
                }
                let physical_input = self.plan_recursive(*input)?;
                PhysicalPlan::HashAggregate {
                    input: Box::new(physical_input),
                    group_by,
                    aggr_exprs,
                    having,
                }
            }

            // Sort - direct conversion
            LogicalPlan::Sort { input, exprs, asc } => {
                if self.verbose {
                    eprintln!("Planning: Sort");
                }
                let physical_input = self.plan_recursive(*input)?;
                PhysicalPlan::Sort {
                    input: Box::new(physical_input),
                    exprs,
                    asc,
                }
            }

            // Limit - direct conversion
            LogicalPlan::Limit { input, limit, offset } => {
                if self.verbose {
                    eprintln!("Planning: Limit({}, offset={})", limit, offset);
                }
                let physical_input = self.plan_recursive(*input)?;
                PhysicalPlan::Limit {
                    input: Box::new(physical_input),
                    limit,
                    offset,
                }
            }

            // DML/DDL operations - not supported in physical planner
            // These are handled directly by the executor
            LogicalPlan::Insert { .. } |
            LogicalPlan::Update { .. } |
            LogicalPlan::Delete { .. } |
            LogicalPlan::CreateTable { .. } |
            LogicalPlan::DropTable { .. } |
            LogicalPlan::CreateIndex { .. } |
            LogicalPlan::AlterColumnStorage { .. } |
            LogicalPlan::Truncate { .. } |
            LogicalPlan::CreateBranch { .. } |
            LogicalPlan::DropBranch { .. } |
            LogicalPlan::MergeBranch { .. } |
            LogicalPlan::UseBranch { .. } |
            LogicalPlan::ShowBranches |
            LogicalPlan::CreateMaterializedView { .. } |
            LogicalPlan::RefreshMaterializedView { .. } |
            LogicalPlan::DropMaterializedView { .. } |
            LogicalPlan::AlterMaterializedView { .. } |
            LogicalPlan::SystemView { .. } |
            LogicalPlan::With { .. } |
            LogicalPlan::CreateTrigger { .. } |
            LogicalPlan::DropTrigger { .. } |
            LogicalPlan::CreateFunction { .. } |
            LogicalPlan::CreateProcedure { .. } |
            LogicalPlan::DropFunction { .. } |
            LogicalPlan::DropProcedure { .. } |
            LogicalPlan::Call { .. } |
            LogicalPlan::Explain { .. } |
            LogicalPlan::StartTransaction |
            LogicalPlan::Commit |
            LogicalPlan::Rollback |
            LogicalPlan::SetConstraints { .. } |
            LogicalPlan::Union { .. } |
            LogicalPlan::Intersect { .. } |
            LogicalPlan::Except { .. } |
            LogicalPlan::DualScan => {
                return Err(Error::internal(
                    "DML/DDL/CTE/TRIGGER/EXPLAIN/Transaction/Procedural/SetOps/DualScan operations should be executed directly, not planned"
                ));
            }

            // HA Operations (ha-tier1) - handled separately due to cfg attribute limitations
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::Switchover { .. } => {
                return Err(Error::internal(
                    "HA Switchover should be executed directly, not planned"
                ));
            }
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::SwitchoverCheck { .. } => {
                return Err(Error::internal(
                    "HA SwitchoverCheck should be executed directly, not planned"
                ));
            }
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::ClusterStatus => {
                return Err(Error::internal(
                    "HA ClusterStatus should be executed directly, not planned"
                ));
            }
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::SetNodeAlias { .. } => {
                return Err(Error::internal(
                    "HA SetNodeAlias should be executed directly, not planned"
                ));
            }
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::ShowTopology => {
                return Err(Error::internal(
                    "HA ShowTopology should be executed directly, not planned"
                ));
            }
        };

        Ok(physical)
    }

    /// Decide whether to use hash join or nested loop join
    ///
    /// Uses cost estimation if available, otherwise falls back to heuristics:
    /// - Hash join for large tables with equality predicates
    /// - Nested loop join for small tables or non-equality predicates
    fn should_use_hash_join(
        &self,
        left: &LogicalPlan,
        right: &LogicalPlan,
        on: &Option<LogicalExpr>,
    ) -> Result<bool> {
        // If we have a cost estimator, use cost-based decision
        if let Some(ref estimator) = self.cost_estimator {
            // Estimate cardinality of both sides
            let left_card = estimator.estimate_cardinality(left)?;
            let right_card = estimator.estimate_cardinality(right)?;

            // Calculate hash join cost: O(left + right) + hash table build cost
            let hash_build_cost = left_card.min(right_card) * 2.0; // Build hash table on smaller side
            let hash_probe_cost = left_card.max(right_card);
            let hash_join_cost = hash_build_cost + hash_probe_cost;

            // Calculate nested loop join cost: O(left * right)
            let nested_loop_cost = left_card * right_card;

            if self.verbose {
                eprintln!("Join cost estimation:");
                eprintln!("  Left cardinality: {:.0}", left_card);
                eprintln!("  Right cardinality: {:.0}", right_card);
                eprintln!("  Hash join cost: {:.2}", hash_join_cost);
                eprintln!("  Nested loop cost: {:.2}", nested_loop_cost);
            }

            // Choose the algorithm with lower cost
            return Ok(hash_join_cost < nested_loop_cost);
        }

        // Fallback to heuristics if no cost estimator available
        // Use nested loop join only if:
        // 1. The join condition is not an equality (hash join requires equality)
        // 2. Both tables are very small (heuristic: assume <100 rows)

        // Check if join condition contains equality
        let has_equality = on.as_ref().map_or(false, |expr| {
            self.contains_equality(expr)
        });

        // If no equality predicate, must use nested loop
        if !has_equality {
            if self.verbose {
                eprintln!("Using nested loop join: no equality predicate");
            }
            return Ok(false);
        }

        // Default to hash join for equality predicates
        // (hash join is generally better for medium to large tables)
        Ok(true)
    }

    /// Check if an expression contains an equality operator
    fn contains_equality(&self, expr: &LogicalExpr) -> bool {
        use crate::sql::logical_plan::BinaryOperator;

        match expr {
            LogicalExpr::BinaryExpr { left, op, right } => {
                match op {
                    BinaryOperator::Eq => true,
                    BinaryOperator::And => {
                        self.contains_equality(left) || self.contains_equality(right)
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// Explain the physical plan in human-readable format
    pub fn explain(&self, plan: &PhysicalPlan) -> String {
        self.explain_recursive(plan, 0)
    }

    /// Recursively format physical plan with indentation
    fn explain_recursive(&self, plan: &PhysicalPlan, depth: usize) -> String {
        let indent = "  ".repeat(depth);
        match plan {
            PhysicalPlan::TableScan { table_name, projection, as_of, .. } => {
                let proj_str = match projection {
                    Some(cols) => format!(" (columns: {:?})", cols),
                    None => " (all columns)".to_string(),
                };
                let as_of_str = match as_of {
                    Some(_) => " [time-travel]",
                    None => "",
                };
                format!("{}TableScan: {}{}{}", indent, table_name, proj_str, as_of_str)
            }
            PhysicalPlan::Filter { input, .. } => {
                format!("{}Filter\n{}", indent, self.explain_recursive(input, depth + 1))
            }
            PhysicalPlan::Projection { input, exprs, .. } => {
                format!("{}Projection ({} columns)\n{}",
                    indent, exprs.len(), self.explain_recursive(input, depth + 1))
            }
            PhysicalPlan::HashJoin { left, right, join_type, .. } => {
                format!("{}HashJoin ({:?})\n{}\n{}",
                    indent, join_type,
                    self.explain_recursive(left, depth + 1),
                    self.explain_recursive(right, depth + 1))
            }
            PhysicalPlan::NestedLoopJoin { left, right, join_type, .. } => {
                format!("{}NestedLoopJoin ({:?})\n{}\n{}",
                    indent, join_type,
                    self.explain_recursive(left, depth + 1),
                    self.explain_recursive(right, depth + 1))
            }
            PhysicalPlan::HashAggregate { input, group_by, aggr_exprs, .. } => {
                format!("{}HashAggregate (group_by: {}, aggr: {})\n{}",
                    indent, group_by.len(), aggr_exprs.len(),
                    self.explain_recursive(input, depth + 1))
            }
            PhysicalPlan::Sort { input, exprs, .. } => {
                format!("{}Sort ({} columns)\n{}",
                    indent, exprs.len(), self.explain_recursive(input, depth + 1))
            }
            PhysicalPlan::Limit { input, limit, offset } => {
                format!("{}Limit ({}, offset={})\n{}",
                    indent, limit, offset, self.explain_recursive(input, depth + 1))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Column, DataType};

    fn create_test_schema() -> Arc<Schema> {
        Arc::new(Schema {
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
                },
                Column {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                default_expr: None,
                unique: false,
                },
            ],
        })
    }

    #[test]
    fn test_plan_table_scan() {
        let planner = Planner::new();
        let schema = create_test_schema();

        let logical = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema: schema.clone(),
            projection: None,
            as_of: None,
        };

        let physical = planner.plan(logical).unwrap();

        match physical {
            PhysicalPlan::TableScan { table_name, .. } => {
                assert_eq!(table_name, "users");
            }
            _ => panic!("Expected TableScan"),
        }
    }

    #[test]
    fn test_plan_filter() {
        let planner = Planner::new();
        let schema = create_test_schema();

        let scan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema,
            projection: None,
            as_of: None,
        };

        let filter = LogicalPlan::Filter {
            input: Box::new(scan),
            predicate: LogicalExpr::Column { table: None, name: "id".to_string()  },
        };

        let physical = planner.plan(filter).unwrap();

        match physical {
            PhysicalPlan::Filter { input, .. } => {
                assert!(matches!(*input, PhysicalPlan::TableScan { .. }));
            }
            _ => panic!("Expected Filter"),
        }
    }

    #[test]
    fn test_explain_plan() {
        let planner = Planner::new();
        let schema = create_test_schema();

        let scan = PhysicalPlan::TableScan {
            table_name: "users".to_string(),
            schema,
            projection: None,
            as_of: None,
        };

        let explanation = planner.explain(&scan);
        assert!(explanation.contains("TableScan"));
        assert!(explanation.contains("users"));
    }
}
