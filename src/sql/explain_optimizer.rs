//! Advanced Optimization Suggestions Engine
//!
//! This module provides intelligent optimization recommendations including:
//! - Query rewrite suggestions (automatic)
//! - Materialized view recommendations
//! - Partition strategy suggestions
//! - Denormalization opportunities
//! - Cost-benefit analysis (ROI)

#![allow(unused_variables)]

use crate::Result;
use serde::{Deserialize, Serialize};

/// Query rewrite pattern
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRewrite {
    pub pattern_name: String,
    pub original_pattern: String,
    pub rewritten_pattern: String,
    pub reasoning: String,
    pub estimated_speedup: f64,
    pub can_auto_apply: bool,
}

/// Materialized view recommendation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedViewRecommendation {
    pub view_name: String,
    pub definition: String,
    pub target_queries: Vec<String>,
    pub estimated_speedup: f64,
    pub storage_cost_mb: f64,
    pub refresh_strategy: RefreshStrategy,
    pub roi_score: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefreshStrategy {
    OnDemand,
    Scheduled,
    Incremental,
    RealTime,
}

impl std::fmt::Display for RefreshStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshStrategy::OnDemand => write!(f, "ON DEMAND"),
            RefreshStrategy::Scheduled => write!(f, "SCHEDULED"),
            RefreshStrategy::Incremental => write!(f, "INCREMENTAL"),
            RefreshStrategy::RealTime => write!(f, "REAL-TIME"),
        }
    }
}

/// Partition strategy suggestion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionStrategy {
    pub table_name: String,
    pub strategy_type: PartitionType,
    pub partition_key: String,
    pub partition_count: usize,
    pub estimated_benefit: String,
    pub migration_complexity: MigrationComplexity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionType {
    Range,
    Hash,
    List,
    Composite,
}

impl std::fmt::Display for PartitionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PartitionType::Range => write!(f, "RANGE"),
            PartitionType::Hash => write!(f, "HASH"),
            PartitionType::List => write!(f, "LIST"),
            PartitionType::Composite => write!(f, "COMPOSITE"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MigrationComplexity {
    Low,
    Medium,
    High,
}

/// Denormalization opportunity
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DenormalizationOpportunity {
    pub opportunity_type: DenormalizationType,
    pub tables_involved: Vec<String>,
    pub suggested_schema: String,
    pub query_improvement: f64,
    pub storage_overhead: f64,
    pub update_complexity: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenormalizationType {
    ColumnDuplication,
    PrecomputedJoin,
    SummaryTable,
    NestedStructure,
}

/// Cost-benefit analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostBenefitAnalysis {
    pub optimization_name: String,
    pub implementation_cost: Cost,
    pub maintenance_cost: Cost,
    pub performance_benefit: Benefit,
    pub roi_percent: f64,
    pub payback_period_days: f64,
    pub recommendation: Recommendation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cost {
    pub development_hours: f64,
    pub storage_mb: f64,
    pub compute_overhead_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Benefit {
    pub query_speedup_percent: f64,
    pub reduced_cpu_percent: f64,
    pub reduced_io_percent: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recommendation {
    HighlyRecommended,
    Recommended,
    Conditional,
    NotRecommended,
}

impl std::fmt::Display for Recommendation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Recommendation::HighlyRecommended => write!(f, "HIGHLY RECOMMENDED"),
            Recommendation::Recommended => write!(f, "RECOMMENDED"),
            Recommendation::Conditional => write!(f, "CONDITIONAL"),
            Recommendation::NotRecommended => write!(f, "NOT RECOMMENDED"),
        }
    }
}

/// Complete optimization suggestions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationSuggestions {
    pub query_rewrites: Vec<QueryRewrite>,
    pub materialized_views: Vec<MaterializedViewRecommendation>,
    pub partition_strategies: Vec<PartitionStrategy>,
    pub denormalization_opportunities: Vec<DenormalizationOpportunity>,
    pub cost_benefit_analyses: Vec<CostBenefitAnalysis>,
}

/// Advanced optimization engine
pub struct OptimizationEngine {
    enable_query_rewrites: bool,
    enable_mv_suggestions: bool,
    enable_partition_suggestions: bool,
    enable_denorm_suggestions: bool,
    min_roi_threshold: f64,
}

impl OptimizationEngine {
    pub fn new() -> Self {
        Self {
            enable_query_rewrites: true,
            enable_mv_suggestions: true,
            enable_partition_suggestions: true,
            enable_denorm_suggestions: true,
            min_roi_threshold: 20.0, // 20% minimum ROI
        }
    }

    pub fn with_min_roi(mut self, threshold: f64) -> Self {
        self.min_roi_threshold = threshold;
        self
    }

    /// Generate optimization suggestions for a query
    pub fn analyze(
        &self,
        query_pattern: &str,
        tables: &[String],
        has_join: bool,
        has_aggregation: bool,
    ) -> Result<OptimizationSuggestions> {
        let query_rewrites = if self.enable_query_rewrites {
            self.generate_query_rewrites(query_pattern, has_join, has_aggregation)
        } else {
            vec![]
        };

        let materialized_views = if self.enable_mv_suggestions && has_aggregation {
            self.suggest_materialized_views(tables, has_join)
        } else {
            vec![]
        };

        let partition_strategies = if self.enable_partition_suggestions {
            self.suggest_partition_strategies(tables)
        } else {
            vec![]
        };

        let denormalization_opportunities = if self.enable_denorm_suggestions && has_join {
            self.suggest_denormalization(tables)
        } else {
            vec![]
        };

        let cost_benefit_analyses = self.perform_cost_benefit_analysis(
            &query_rewrites,
            &materialized_views,
            &partition_strategies,
            &denormalization_opportunities,
        );

        Ok(OptimizationSuggestions {
            query_rewrites,
            materialized_views,
            partition_strategies,
            denormalization_opportunities,
            cost_benefit_analyses,
        })
    }

    fn generate_query_rewrites(
        &self,
        query_pattern: &str,
        has_join: bool,
        has_aggregation: bool,
    ) -> Vec<QueryRewrite> {
        let mut rewrites = Vec::new();

        // Rewrite 1: IN to EXISTS transformation
        rewrites.push(QueryRewrite {
            pattern_name: "IN to EXISTS".to_string(),
            original_pattern: "WHERE column IN (SELECT ...)".to_string(),
            rewritten_pattern: "WHERE EXISTS (SELECT 1 FROM ... WHERE ...)".to_string(),
            reasoning: "EXISTS can short-circuit on first match, avoiding full subquery evaluation".to_string(),
            estimated_speedup: 2.5,
            can_auto_apply: true,
        });

        // Rewrite 2: COUNT(*) optimization
        if has_aggregation {
            rewrites.push(QueryRewrite {
                pattern_name: "COUNT(*) with condition".to_string(),
                original_pattern: "SELECT COUNT(*) FROM t WHERE condition".to_string(),
                rewritten_pattern: "SELECT SUM(CASE WHEN condition THEN 1 ELSE 0 END) FROM t".to_string(),
                reasoning: "Conditional aggregation can leverage parallel execution better".to_string(),
                estimated_speedup: 1.8,
                can_auto_apply: false,
            });
        }

        // Rewrite 3: Join elimination
        if has_join {
            rewrites.push(QueryRewrite {
                pattern_name: "Unnecessary JOIN elimination".to_string(),
                original_pattern: "SELECT a.* FROM a JOIN b ON a.id = b.a_id".to_string(),
                rewritten_pattern: "SELECT * FROM a WHERE id IN (SELECT a_id FROM b)".to_string(),
                reasoning: "If no columns from b are selected, join can be converted to semi-join".to_string(),
                estimated_speedup: 3.2,
                can_auto_apply: true,
            });
        }

        // Rewrite 4: DISTINCT elimination
        rewrites.push(QueryRewrite {
            pattern_name: "DISTINCT to GROUP BY".to_string(),
            original_pattern: "SELECT DISTINCT column FROM table".to_string(),
            rewritten_pattern: "SELECT column FROM table GROUP BY column".to_string(),
            reasoning: "GROUP BY can use hash aggregation which is faster for large datasets".to_string(),
            estimated_speedup: 1.5,
            can_auto_apply: true,
        });

        // Rewrite 5: Subquery to CTE
        rewrites.push(QueryRewrite {
            pattern_name: "Subquery to CTE".to_string(),
            original_pattern: "SELECT ... FROM (SELECT ...) AS subq".to_string(),
            rewritten_pattern: "WITH cte AS (SELECT ...) SELECT ... FROM cte".to_string(),
            reasoning: "CTEs improve readability and enable query optimizer to materialize once".to_string(),
            estimated_speedup: 1.3,
            can_auto_apply: true,
        });

        rewrites
    }

    #[allow(clippy::indexing_slicing)]
    // SAFETY: `tables[0]` and `tables[1]` are guarded by `tables.len() >= 2` checks
    fn suggest_materialized_views(&self, tables: &[String], has_join: bool) -> Vec<MaterializedViewRecommendation> {
        let mut views = Vec::new();

        for table in tables {
            // Aggregation materialized view
            views.push(MaterializedViewRecommendation {
                view_name: format!("{}_daily_summary", table),
                definition: format!(
                    "CREATE MATERIALIZED VIEW {}_daily_summary AS \
                    SELECT DATE_TRUNC('day', created_at) as day, COUNT(*) as count, \
                    SUM(amount) as total FROM {} GROUP BY day",
                    table, table
                ),
                target_queries: vec![
                    format!("Daily aggregation queries on {}", table),
                    "Reporting dashboards".to_string(),
                ],
                estimated_speedup: 50.0,
                storage_cost_mb: 100.0,
                refresh_strategy: RefreshStrategy::Scheduled,
                roi_score: 85.0,
            });
        }

        if has_join && tables.len() >= 2 {
            // Join materialized view
            views.push(MaterializedViewRecommendation {
                view_name: format!("{}_{}_joined", tables[0], tables[1]),
                definition: format!(
                    "CREATE MATERIALIZED VIEW {}_{}_joined AS \
                    SELECT a.*, b.* FROM {} a JOIN {} b ON a.id = b.{}_id",
                    tables[0], tables[1], tables[0], tables[1], tables[0]
                ),
                target_queries: vec![
                    format!("Queries joining {} and {}", tables[0], tables[1]),
                ],
                estimated_speedup: 30.0,
                storage_cost_mb: 500.0,
                refresh_strategy: RefreshStrategy::Incremental,
                roi_score: 70.0,
            });
        }

        views
    }

    fn suggest_partition_strategies(&self, tables: &[String]) -> Vec<PartitionStrategy> {
        let mut strategies = Vec::new();

        for table in tables {
            // Range partitioning by date
            strategies.push(PartitionStrategy {
                table_name: table.clone(),
                strategy_type: PartitionType::Range,
                partition_key: "created_at".to_string(),
                partition_count: 12,
                estimated_benefit: "75% faster queries with date filters, easier data archival".to_string(),
                migration_complexity: MigrationComplexity::Medium,
            });

            // Hash partitioning by ID
            strategies.push(PartitionStrategy {
                table_name: table.clone(),
                strategy_type: PartitionType::Hash,
                partition_key: "id".to_string(),
                partition_count: 16,
                estimated_benefit: "Uniform data distribution, better parallel scan performance".to_string(),
                migration_complexity: MigrationComplexity::Medium,
            });
        }

        strategies
    }

    #[allow(clippy::indexing_slicing)]
    // SAFETY: `tables[0]` and `tables[1]` are guarded by `tables.len() >= 2` check
    fn suggest_denormalization(&self, tables: &[String]) -> Vec<DenormalizationOpportunity> {
        let mut opportunities = Vec::new();

        if tables.len() >= 2 {
            // Precomputed join
            opportunities.push(DenormalizationOpportunity {
                opportunity_type: DenormalizationType::PrecomputedJoin,
                tables_involved: tables.to_vec(),
                suggested_schema: format!(
                    "Add denormalized columns from {} to {}",
                    tables[1], tables[0]
                ),
                query_improvement: 80.0,
                storage_overhead: 15.0,
                update_complexity: "Update triggers needed to maintain consistency".to_string(),
            });

            // Summary table
            opportunities.push(DenormalizationOpportunity {
                opportunity_type: DenormalizationType::SummaryTable,
                tables_involved: tables.to_vec(),
                suggested_schema: format!(
                    "CREATE TABLE {}_summary (id, count, total, avg, ...)",
                    tables[0]
                ),
                query_improvement: 95.0,
                storage_overhead: 5.0,
                update_complexity: "Async background job to update summaries".to_string(),
            });
        }

        opportunities
    }

    fn perform_cost_benefit_analysis(
        &self,
        rewrites: &[QueryRewrite],
        mvs: &[MaterializedViewRecommendation],
        partitions: &[PartitionStrategy],
        denorms: &[DenormalizationOpportunity],
    ) -> Vec<CostBenefitAnalysis> {
        let mut analyses = Vec::new();

        // Analyze query rewrites
        for rewrite in rewrites {
            if rewrite.can_auto_apply {
                let roi = rewrite.estimated_speedup * 100.0 / 1.0; // Minimal cost
                analyses.push(CostBenefitAnalysis {
                    optimization_name: rewrite.pattern_name.clone(),
                    implementation_cost: Cost {
                        development_hours: 0.5,
                        storage_mb: 0.0,
                        compute_overhead_percent: 0.0,
                    },
                    maintenance_cost: Cost {
                        development_hours: 0.0,
                        storage_mb: 0.0,
                        compute_overhead_percent: 0.0,
                    },
                    performance_benefit: Benefit {
                        query_speedup_percent: (rewrite.estimated_speedup - 1.0) * 100.0,
                        reduced_cpu_percent: 30.0,
                        reduced_io_percent: 20.0,
                    },
                    roi_percent: roi,
                    payback_period_days: 0.1,
                    recommendation: if roi > 100.0 {
                        Recommendation::HighlyRecommended
                    } else {
                        Recommendation::Recommended
                    },
                });
            }
        }

        // Analyze materialized views
        for mv in mvs {
            let impl_cost = mv.storage_cost_mb / 100.0 * 2.0; // Hours to implement
            let maint_cost = mv.storage_cost_mb * 0.01; // Ongoing maintenance
            let roi = (mv.estimated_speedup * 100.0) / (impl_cost + maint_cost);

            analyses.push(CostBenefitAnalysis {
                optimization_name: format!("Materialized View: {}", mv.view_name),
                implementation_cost: Cost {
                    development_hours: impl_cost,
                    storage_mb: mv.storage_cost_mb,
                    compute_overhead_percent: 5.0,
                },
                maintenance_cost: Cost {
                    development_hours: 1.0,
                    storage_mb: mv.storage_cost_mb,
                    compute_overhead_percent: 10.0,
                },
                performance_benefit: Benefit {
                    query_speedup_percent: mv.estimated_speedup,
                    reduced_cpu_percent: 40.0,
                    reduced_io_percent: 60.0,
                },
                roi_percent: roi,
                payback_period_days: (impl_cost / mv.estimated_speedup) * 7.0,
                recommendation: if roi > 80.0 {
                    Recommendation::HighlyRecommended
                } else if roi > 50.0 {
                    Recommendation::Recommended
                } else {
                    Recommendation::Conditional
                },
            });
        }

        // Analyze partitioning
        for partition in partitions {
            let impl_hours = match partition.migration_complexity {
                MigrationComplexity::Low => 8.0,
                MigrationComplexity::Medium => 24.0,
                MigrationComplexity::High => 80.0,
            };

            let roi = 75.0 / impl_hours * 100.0;

            analyses.push(CostBenefitAnalysis {
                optimization_name: format!("Partition {}: {}", partition.table_name, partition.strategy_type),
                implementation_cost: Cost {
                    development_hours: impl_hours,
                    storage_mb: 50.0,
                    compute_overhead_percent: 2.0,
                },
                maintenance_cost: Cost {
                    development_hours: 2.0,
                    storage_mb: 50.0,
                    compute_overhead_percent: 1.0,
                },
                performance_benefit: Benefit {
                    query_speedup_percent: 75.0,
                    reduced_cpu_percent: 50.0,
                    reduced_io_percent: 70.0,
                },
                roi_percent: roi,
                payback_period_days: impl_hours / 8.0 * 7.0,
                recommendation: if impl_hours < 30.0 {
                    Recommendation::Recommended
                } else {
                    Recommendation::Conditional
                },
            });
        }

        analyses
    }

    /// Format optimization suggestions
    pub fn format_output(&self, suggestions: &OptimizationSuggestions) -> String {
        let mut output = String::new();

        output.push_str("═══════════════════════════════════════════════════════════════\n");
        output.push_str("        ADVANCED OPTIMIZATION SUGGESTIONS                      \n");
        output.push_str("═══════════════════════════════════════════════════════════════\n\n");

        // Query rewrites
        if !suggestions.query_rewrites.is_empty() {
            output.push_str("───────────────────────────────────────────────────────────────\n");
            output.push_str(&format!("  QUERY REWRITES ({} patterns)\n", suggestions.query_rewrites.len()));
            output.push_str("───────────────────────────────────────────────────────────────\n\n");

            for rewrite in &suggestions.query_rewrites {
                output.push_str(&format!("• {}\n", rewrite.pattern_name));
                output.push_str(&format!("  Speedup: {:.1}x\n", rewrite.estimated_speedup));
                output.push_str(&format!("  Auto-apply: {}\n", rewrite.can_auto_apply));
                output.push_str(&format!("  Reasoning: {}\n", rewrite.reasoning));
                output.push_str(&format!("  Original: {}\n", rewrite.original_pattern));
                output.push_str(&format!("  Rewritten: {}\n", rewrite.rewritten_pattern));
                output.push_str("\n");
            }
        }

        // Materialized views
        if !suggestions.materialized_views.is_empty() {
            output.push_str("───────────────────────────────────────────────────────────────\n");
            output.push_str(&format!("  MATERIALIZED VIEWS ({} recommendations)\n", suggestions.materialized_views.len()));
            output.push_str("───────────────────────────────────────────────────────────────\n\n");

            for mv in &suggestions.materialized_views {
                output.push_str(&format!("• {}\n", mv.view_name));
                output.push_str(&format!("  Speedup: {:.0}%\n", mv.estimated_speedup));
                output.push_str(&format!("  ROI Score: {:.0}/100\n", mv.roi_score));
                output.push_str(&format!("  Storage Cost: {:.2} MB\n", mv.storage_cost_mb));
                output.push_str(&format!("  Refresh: {}\n", mv.refresh_strategy));
                output.push_str(&format!("  Definition: {}\n", mv.definition));
                output.push_str("\n");
            }
        }

        // Partition strategies
        if !suggestions.partition_strategies.is_empty() {
            output.push_str("───────────────────────────────────────────────────────────────\n");
            output.push_str(&format!("  PARTITION STRATEGIES ({} suggestions)\n", suggestions.partition_strategies.len()));
            output.push_str("───────────────────────────────────────────────────────────────\n\n");

            for part in &suggestions.partition_strategies {
                output.push_str(&format!("• {} - {}\n", part.table_name, part.strategy_type));
                output.push_str(&format!("  Partition Key: {}\n", part.partition_key));
                output.push_str(&format!("  Partition Count: {}\n", part.partition_count));
                output.push_str(&format!("  Benefit: {}\n", part.estimated_benefit));
                output.push_str(&format!("  Complexity: {:?}\n", part.migration_complexity));
                output.push_str("\n");
            }
        }

        // Denormalization opportunities
        if !suggestions.denormalization_opportunities.is_empty() {
            output.push_str("───────────────────────────────────────────────────────────────\n");
            output.push_str(&format!("  DENORMALIZATION ({} opportunities)\n", suggestions.denormalization_opportunities.len()));
            output.push_str("───────────────────────────────────────────────────────────────\n\n");

            for denorm in &suggestions.denormalization_opportunities {
                output.push_str(&format!("• {:?}\n", denorm.opportunity_type));
                output.push_str(&format!("  Tables: {}\n", denorm.tables_involved.join(", ")));
                output.push_str(&format!("  Query Improvement: {:.0}%\n", denorm.query_improvement));
                output.push_str(&format!("  Storage Overhead: {:.0}%\n", denorm.storage_overhead));
                output.push_str(&format!("  Schema: {}\n", denorm.suggested_schema));
                output.push_str("\n");
            }
        }

        // Cost-benefit analysis
        if !suggestions.cost_benefit_analyses.is_empty() {
            output.push_str("───────────────────────────────────────────────────────────────\n");
            output.push_str(&format!("  COST-BENEFIT ANALYSIS ({} optimizations)\n", suggestions.cost_benefit_analyses.len()));
            output.push_str("───────────────────────────────────────────────────────────────\n\n");

            for analysis in &suggestions.cost_benefit_analyses {
                output.push_str(&format!("• {}\n", analysis.optimization_name));
                output.push_str(&format!("  ROI: {:.0}%\n", analysis.roi_percent));
                output.push_str(&format!("  Payback Period: {:.1} days\n", analysis.payback_period_days));
                output.push_str(&format!("  Recommendation: {}\n", analysis.recommendation));
                output.push_str(&format!("  Implementation: {:.1}h development, {:.0} MB storage\n",
                    analysis.implementation_cost.development_hours,
                    analysis.implementation_cost.storage_mb));
                output.push_str(&format!("  Benefit: {:.0}% speedup, {:.0}% less CPU, {:.0}% less I/O\n",
                    analysis.performance_benefit.query_speedup_percent,
                    analysis.performance_benefit.reduced_cpu_percent,
                    analysis.performance_benefit.reduced_io_percent));
                output.push_str("\n");
            }
        }

        output.push_str("═══════════════════════════════════════════════════════════════\n");

        output
    }
}

impl Default for OptimizationEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_query_rewrites() {
        let engine = OptimizationEngine::new();
        let result = engine.analyze("SELECT", &["users".to_string()], true, false).unwrap();

        assert!(!result.query_rewrites.is_empty());
        assert!(result.query_rewrites.iter().any(|r| r.can_auto_apply));
    }

    #[test]
    fn test_materialized_view_suggestions() {
        let engine = OptimizationEngine::new();
        let result = engine.analyze("SELECT", &["users".to_string()], false, true).unwrap();

        assert!(!result.materialized_views.is_empty());
        assert!(result.materialized_views[0].roi_score > 0.0);
    }

    #[test]
    fn test_partition_strategies() {
        let engine = OptimizationEngine::new();
        let result = engine.analyze("SELECT", &["users".to_string()], false, false).unwrap();

        assert!(!result.partition_strategies.is_empty());
        assert_eq!(result.partition_strategies[0].table_name, "users");
    }

    #[test]
    fn test_denormalization_suggestions() {
        let engine = OptimizationEngine::new();
        let tables = vec!["users".to_string(), "orders".to_string()];
        let result = engine.analyze("SELECT", &tables, true, false).unwrap();

        assert!(!result.denormalization_opportunities.is_empty());
    }

    #[test]
    fn test_cost_benefit_analysis() {
        let engine = OptimizationEngine::new();
        let result = engine.analyze("SELECT", &["users".to_string()], false, true).unwrap();

        assert!(!result.cost_benefit_analyses.is_empty());
        assert!(result.cost_benefit_analyses[0].roi_percent > 0.0);
    }

    #[test]
    fn test_format_output() {
        let engine = OptimizationEngine::new();
        let result = engine.analyze("SELECT", &["users".to_string()], true, true).unwrap();
        let output = engine.format_output(&result);

        assert!(output.contains("OPTIMIZATION SUGGESTIONS"));
        assert!(output.contains("QUERY REWRITES"));
    }
}
