//! Query optimizer
//!
//! Provides rule-based query optimization to improve query execution performance.
//!
//! The optimizer applies a series of transformation rules to logical query plans,
//! producing equivalent but more efficient execution strategies.

pub mod rules;
pub mod planner;
pub mod cost;

use crate::sql::logical_plan::LogicalPlan;
use crate::Result;
use rules::{OptimizationRule, create_default_rules};
use cost::{CostEstimator, StatsCatalog};

/// Query optimizer configuration
#[derive(Debug, Clone)]
pub struct OptimizerConfig {
    /// Maximum number of optimization passes
    pub max_passes: usize,
    /// Enable verbose logging
    pub verbose: bool,
    /// Optimization timeout in milliseconds (0 = no timeout)
    pub timeout_ms: u64,
}

impl Default for OptimizerConfig {
    fn default() -> Self {
        Self {
            max_passes: 10,
            verbose: false,
            timeout_ms: 0,
        }
    }
}

impl OptimizerConfig {
    /// Create a new optimizer configuration
    pub fn new() -> Self {
        Self::default()
    }

    /// Set maximum number of optimization passes
    pub fn with_max_passes(mut self, max_passes: usize) -> Self {
        self.max_passes = max_passes;
        self
    }

    /// Enable verbose logging
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Set optimization timeout
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }
}

/// Query optimizer
///
/// Applies optimization rules to logical plans to improve query performance.
/// The optimizer uses a cost-based approach with table statistics.
pub struct Optimizer {
    /// Optimization rules
    rules: Vec<Box<dyn OptimizationRule>>,
    /// Cost estimator with statistics
    cost_estimator: CostEstimator,
    /// Configuration
    config: OptimizerConfig,
}

impl Optimizer {
    /// Create a new optimizer with default rules
    pub fn new(stats: StatsCatalog) -> Self {
        Self {
            rules: create_default_rules(),
            cost_estimator: CostEstimator::new(stats),
            config: OptimizerConfig::default(),
        }
    }

    /// Create optimizer with custom configuration
    pub fn with_config(stats: StatsCatalog, config: OptimizerConfig) -> Self {
        Self {
            rules: create_default_rules(),
            cost_estimator: CostEstimator::new(stats),
            config,
        }
    }

    /// Create optimizer with custom rules
    pub fn with_rules(
        stats: StatsCatalog,
        rules: Vec<Box<dyn OptimizationRule>>,
        config: OptimizerConfig,
    ) -> Self {
        Self {
            rules,
            cost_estimator: CostEstimator::new(stats),
            config,
        }
    }

    /// Get a reference to the cost estimator
    pub fn cost_estimator(&self) -> &CostEstimator {
        &self.cost_estimator
    }

    /// Get a mutable reference to the cost estimator
    pub fn cost_estimator_mut(&mut self) -> &mut CostEstimator {
        &mut self.cost_estimator
    }

    /// Optimize a logical plan
    ///
    /// Applies optimization rules iteratively until no more improvements
    /// can be made or the maximum number of passes is reached.
    pub fn optimize(&self, plan: LogicalPlan) -> Result<LogicalPlan> {
        let start_time = std::time::Instant::now();
        let mut current_plan = plan;
        let mut pass_count = 0;

        // Calculate initial cost for comparison
        let initial_cost = self.cost_estimator.estimate_cost(&current_plan)
            .unwrap_or(f64::MAX);

        if self.config.verbose {
            eprintln!("=== Query Optimizer ===");
            eprintln!("Initial cost: {:.2}", initial_cost);
            eprintln!("Max passes: {}", self.config.max_passes);
        }

        // Apply optimization rules iteratively
        while pass_count < self.config.max_passes {
            // Check timeout
            if self.config.timeout_ms > 0 {
                let elapsed = start_time.elapsed().as_millis() as u64;
                if elapsed > self.config.timeout_ms {
                    if self.config.verbose {
                        eprintln!("Optimization timeout after {} passes", pass_count);
                    }
                    break;
                }
            }

            pass_count += 1;
            let mut plan_changed = false;

            if self.config.verbose {
                eprintln!("\n--- Pass {} ---", pass_count);
            }

            // Apply each rule
            for rule in &self.rules {
                // Check if rule is applicable
                if !rule.is_applicable(&current_plan) {
                    continue;
                }

                // Try to apply the rule
                match rule.apply(current_plan.clone(), &self.cost_estimator) {
                    Ok(Some(new_plan)) => {
                        // Calculate cost of new plan
                        let new_cost = self.cost_estimator.estimate_cost(&new_plan)
                            .unwrap_or(f64::MAX);
                        let old_cost = self.cost_estimator.estimate_cost(&current_plan)
                            .unwrap_or(f64::MAX);

                        // Only accept if cost improved or stayed the same
                        if new_cost <= old_cost {
                            if self.config.verbose {
                                eprintln!(
                                    "  Applied {}: cost {:.2} -> {:.2}",
                                    rule.name(),
                                    old_cost,
                                    new_cost
                                );
                            }
                            current_plan = new_plan;
                            plan_changed = true;
                        } else if self.config.verbose {
                            eprintln!(
                                "  Rejected {}: cost would increase {:.2} -> {:.2}",
                                rule.name(),
                                old_cost,
                                new_cost
                            );
                        }
                    }
                    Ok(None) => {
                        // Rule not applicable or no change
                    }
                    Err(e) => {
                        // Log error but continue optimization
                        if self.config.verbose {
                            eprintln!("  Error applying {}: {}", rule.name(), e);
                        }
                    }
                }
            }

            // If no rules made changes, we're done
            if !plan_changed {
                if self.config.verbose {
                    eprintln!("No more optimizations possible");
                }
                break;
            }
        }

        // Calculate final cost
        let final_cost = self.cost_estimator.estimate_cost(&current_plan)
            .unwrap_or(f64::MAX);

        if self.config.verbose {
            let improvement = if initial_cost > 0.0 {
                ((initial_cost - final_cost) / initial_cost) * 100.0
            } else {
                0.0
            };

            eprintln!("\n=== Optimization Complete ===");
            eprintln!("Passes: {}", pass_count);
            eprintln!("Initial cost: {:.2}", initial_cost);
            eprintln!("Final cost: {:.2}", final_cost);
            eprintln!("Improvement: {:.1}%", improvement);
            eprintln!("Time: {:?}", start_time.elapsed());
        }

        Ok(current_plan)
    }

    /// Optimize a plan tree recursively
    ///
    /// This method recursively optimizes each node in the plan tree,
    /// applying optimizations bottom-up.
    pub fn optimize_recursive(&self, plan: LogicalPlan) -> Result<LogicalPlan> {
        let optimized = match plan {
            LogicalPlan::Filter { input, predicate } => {
                let optimized_input = self.optimize_recursive(*input)?;
                LogicalPlan::Filter {
                    input: Box::new(optimized_input),
                    predicate,
                }
            }
            LogicalPlan::Project { input, exprs, aliases, distinct, distinct_on } => {
                let optimized_input = self.optimize_recursive(*input)?;
                LogicalPlan::Project {
                    input: Box::new(optimized_input),
                    exprs,
                    aliases,
                    distinct,
                    distinct_on,
                }
            }
            LogicalPlan::Join { left, right, join_type, on } => {
                let optimized_left = self.optimize_recursive(*left)?;
                let optimized_right = self.optimize_recursive(*right)?;
                LogicalPlan::Join {
                    left: Box::new(optimized_left),
                    right: Box::new(optimized_right),
                    join_type,
                    on,
                }
            }
            LogicalPlan::Aggregate { input, group_by, aggr_exprs, having } => {
                let optimized_input = self.optimize_recursive(*input)?;
                LogicalPlan::Aggregate {
                    input: Box::new(optimized_input),
                    group_by,
                    aggr_exprs,
                    having,
                }
            }
            LogicalPlan::Sort { input, exprs, asc } => {
                let optimized_input = self.optimize_recursive(*input)?;
                LogicalPlan::Sort {
                    input: Box::new(optimized_input),
                    exprs,
                    asc,
                }
            }
            LogicalPlan::Limit { input, limit, offset } => {
                let optimized_input = self.optimize_recursive(*input)?;
                LogicalPlan::Limit {
                    input: Box::new(optimized_input),
                    limit,
                    offset,
                }
            }
            // Leaf nodes - no recursion needed
            other => other,
        };

        // Apply optimization rules to this node
        self.optimize(optimized)
    }

    /// Explain optimization decisions for a plan
    ///
    /// Returns a description of what optimizations were applied
    pub fn explain(&self, plan: LogicalPlan) -> Result<String> {
        let mut explanation = String::new();
        let mut current_plan = plan;
        let mut pass_count = 0;

        explanation.push_str("Query Optimization Analysis\n");
        explanation.push_str("===========================\n\n");

        let initial_cost = self.cost_estimator.estimate_cost(&current_plan)
            .unwrap_or(f64::MAX);
        explanation.push_str(&format!("Initial estimated cost: {:.2}\n\n", initial_cost));

        while pass_count < self.config.max_passes {
            pass_count += 1;
            let mut plan_changed = false;

            explanation.push_str(&format!("Pass {}:\n", pass_count));

            for rule in &self.rules {
                if !rule.is_applicable(&current_plan) {
                    continue;
                }

                match rule.apply(current_plan.clone(), &self.cost_estimator) {
                    Ok(Some(new_plan)) => {
                        let new_cost = self.cost_estimator.estimate_cost(&new_plan)
                            .unwrap_or(f64::MAX);
                        let old_cost = self.cost_estimator.estimate_cost(&current_plan)
                            .unwrap_or(f64::MAX);

                        if new_cost <= old_cost {
                            explanation.push_str(&format!(
                                "  ✓ {}: {:.2} -> {:.2} ({:.1}% improvement)\n",
                                rule.name(),
                                old_cost,
                                new_cost,
                                ((old_cost - new_cost) / old_cost * 100.0).max(0.0)
                            ));
                            current_plan = new_plan;
                            plan_changed = true;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        explanation.push_str(&format!("  ✗ {}: Error - {}\n", rule.name(), e));
                    }
                }
            }

            if !plan_changed {
                explanation.push_str("  No optimizations applied\n");
                break;
            }

            explanation.push('\n');
        }

        let final_cost = self.cost_estimator.estimate_cost(&current_plan)
            .unwrap_or(f64::MAX);
        let improvement = if initial_cost > 0.0 {
            ((initial_cost - final_cost) / initial_cost) * 100.0
        } else {
            0.0
        };

        explanation.push_str("===========================\n");
        explanation.push_str(&format!("Final estimated cost: {:.2}\n", final_cost));
        explanation.push_str(&format!("Total improvement: {:.1}%\n", improvement));
        explanation.push_str(&format!("Optimization passes: {}\n", pass_count));

        Ok(explanation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::logical_plan::*;
    use crate::{Schema, Column, DataType, Value};
    use cost::{StatsCatalog, TableStats};
    use std::sync::Arc;

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
                storage_mode: crate::ColumnStorageMode::Default,
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
                storage_mode: crate::ColumnStorageMode::Default,
                },
                Column {
                    name: "age".to_string(),
                    data_type: DataType::Int4,
                    nullable: true,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
                },
            ],
        })
    }

    #[test]
    fn test_optimizer_basic() {
        let mut stats_catalog = StatsCatalog::new();
        stats_catalog.add_table_stats(
            TableStats::new("users".to_string())
                .with_row_count(1000)
                .with_avg_row_size(256)
        );

        let optimizer = Optimizer::new(stats_catalog);
        let schema = create_test_schema();

        let plan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema,
            projection: None,
            as_of: None,
        };

        let result = optimizer.optimize(plan);
        assert!(result.is_ok());
    }

    #[test]
    fn test_optimizer_with_filter() {
        let mut stats_catalog = StatsCatalog::new();
        stats_catalog.add_table_stats(
            TableStats::new("users".to_string())
                .with_row_count(1000)
                .with_avg_row_size(256)
        );

        let config = OptimizerConfig::new().with_verbose(false);
        let optimizer = Optimizer::with_config(stats_catalog, config);
        let schema = create_test_schema();

        // Create a filter plan
        let scan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema,
            projection: None,
            as_of: None,
        };

        let filter = LogicalPlan::Filter {
            input: Box::new(scan),
            predicate: LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column { table: None, name: "id".to_string()  }),
                op: BinaryOperator::Eq,
                right: Box::new(LogicalExpr::Literal(Value::Int4(1))),
            },
        };

        let result = optimizer.optimize(filter);
        assert!(result.is_ok());
    }

    #[test]
    fn test_optimizer_constant_folding() {
        let stats_catalog = StatsCatalog::new();
        let optimizer = Optimizer::new(stats_catalog);
        let schema = create_test_schema();

        // Create plan with constant expression: WHERE 1 + 2 = 3
        let scan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema,
            projection: None,
            as_of: None,
        };

        let filter = LogicalPlan::Filter {
            input: Box::new(scan),
            predicate: LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::BinaryExpr {
                    left: Box::new(LogicalExpr::Literal(Value::Int4(1))),
                    op: BinaryOperator::Plus,
                    right: Box::new(LogicalExpr::Literal(Value::Int4(2))),
                }),
                op: BinaryOperator::Eq,
                right: Box::new(LogicalExpr::Literal(Value::Int4(3))),
            },
        };

        let optimized = optimizer.optimize(filter).unwrap();

        // Check that constant was folded completely
        // The expression (1 + 2) = 3 should fold to true
        if let LogicalPlan::Filter { predicate, .. } = optimized {
            assert!(matches!(predicate, LogicalExpr::Literal(Value::Boolean(true))));
        } else {
            panic!("Expected Filter plan");
        }
    }

    #[test]
    fn test_optimizer_explain() {
        let mut stats_catalog = StatsCatalog::new();
        stats_catalog.add_table_stats(
            TableStats::new("users".to_string())
                .with_row_count(1000)
                .with_avg_row_size(256)
        );

        let optimizer = Optimizer::new(stats_catalog);
        let schema = create_test_schema();

        let plan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema,
            projection: None,
            as_of: None,
        };

        let explanation = optimizer.explain(plan);
        assert!(explanation.is_ok());

        let text = explanation.unwrap();
        assert!(text.contains("Query Optimization Analysis"));
        assert!(text.contains("estimated cost"));
    }

    #[test]
    fn test_optimizer_timeout() {
        let stats_catalog = StatsCatalog::new();
        let config = OptimizerConfig::new()
            .with_timeout_ms(1) // Very short timeout
            .with_max_passes(1000);

        let optimizer = Optimizer::with_config(stats_catalog, config);
        let schema = create_test_schema();

        let plan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema,
            projection: None,
            as_of: None,
        };

        // Should complete despite timeout (plan is simple)
        let result = optimizer.optimize(plan);
        assert!(result.is_ok());
    }
}
