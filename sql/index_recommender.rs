//! Automatic Index Recommendation System
//!
//! Analyzes query workload and recommends indexes to improve performance.
//! Features:
//! - Workload analysis from query patterns
//! - Missing index detection
//! - Benefit calculation (speedup %)
//! - Cost estimation (storage, maintenance)
//! - CREATE INDEX statement generation
//! - Prioritized recommendations by ROI

use crate::Result;
use super::logical_plan::{LogicalPlan, LogicalExpr};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Index recommendation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexRecommendation {
    /// Table name
    pub table_name: String,

    /// Recommended index columns
    pub columns: Vec<String>,

    /// Index type (BTree, Hash, etc.)
    pub index_type: IndexType,

    /// Estimated benefit
    pub benefit: IndexBenefit,

    /// Estimated cost
    pub cost: IndexCost,

    /// Return on investment score (0-100)
    pub roi_score: f64,

    /// Generated CREATE INDEX statement
    pub create_statement: String,

    /// Reason for recommendation
    pub reason: String,

    /// Query patterns that would benefit
    pub query_patterns: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexType {
    BTree,
    Hash,
    GIN,  // Generalized Inverted Index (for JSON, arrays)
    BRIN, // Block Range Index (for large tables)
}

impl IndexType {
    fn name(&self) -> &'static str {
        match self {
            IndexType::BTree => "BTREE",
            IndexType::Hash => "HASH",
            IndexType::GIN => "GIN",
            IndexType::BRIN => "BRIN",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexBenefit {
    /// Estimated speedup multiplier (e.g., 5.0 = 5x faster)
    pub speedup_multiplier: f64,

    /// Estimated time savings in milliseconds
    pub time_savings_ms: f64,

    /// Number of queries that would benefit
    pub affected_queries: usize,

    /// Percentage improvement
    pub improvement_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexCost {
    /// Estimated storage size in bytes
    pub storage_bytes: usize,

    /// Creation time in milliseconds
    pub creation_time_ms: f64,

    /// Maintenance overhead percentage
    pub maintenance_overhead_percent: f64,

    /// Write penalty (slower INSERT/UPDATE)
    pub write_penalty_percent: f64,
}

/// Index recommender analyzes query workload
pub struct IndexRecommender {
    workload: Vec<LogicalPlan>,
    table_stats: HashMap<String, TableStats>,
}

#[derive(Debug, Clone)]
struct TableStats {
    row_count: usize,
    column_cardinality: HashMap<String, usize>,
    access_patterns: Vec<AccessPattern>,
}

#[derive(Debug, Clone)]
struct AccessPattern {
    columns: Vec<String>,
    operation: AccessOperation,
    frequency: usize,
    selectivity: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessOperation {
    Equality,      // WHERE col = value
    Range,         // WHERE col > value
    In,            // WHERE col IN (...)
    Like,          // WHERE col LIKE 'pattern%'
    Join,          // JOIN ON col
    OrderBy,       // ORDER BY col
    GroupBy,       // GROUP BY col
}

impl IndexRecommender {
    pub fn new() -> Self {
        Self {
            workload: Vec::new(),
            table_stats: HashMap::new(),
        }
    }

    /// Add a query to the workload
    pub fn add_query(&mut self, plan: LogicalPlan) {
        self.analyze_plan(&plan);
        self.workload.push(plan);
    }

    /// Add table statistics
    pub fn add_table_stats(&mut self, table: String, row_count: usize, column_cardinality: HashMap<String, usize>) {
        self.table_stats.insert(table.clone(), TableStats {
            row_count,
            column_cardinality,
            access_patterns: Vec::new(),
        });
    }

    /// Analyze a query plan for access patterns
    fn analyze_plan(&mut self, plan: &LogicalPlan) {
        match plan {
            LogicalPlan::Scan { table_name, .. } => {
                // Record table scan
                self.record_access(table_name, vec![], AccessOperation::Equality, 0.1);
            }

            LogicalPlan::Filter { input, predicate } => {
                if let LogicalPlan::Scan { table_name, .. } = &**input {
                    self.analyze_predicate(table_name, predicate);
                }
                self.analyze_plan(input);
            }

            LogicalPlan::Join { left, right, on, .. } => {
                if let Some(join_cond) = on {
                    self.analyze_join_condition(join_cond);
                }
                self.analyze_plan(left);
                self.analyze_plan(right);
            }

            LogicalPlan::Sort { input, exprs, .. } => {
                if let LogicalPlan::Scan { table_name, .. } = &**input {
                    let columns = self.extract_columns(exprs);
                    self.record_access(table_name, columns, AccessOperation::OrderBy, 1.0);
                }
                self.analyze_plan(input);
            }

            LogicalPlan::Aggregate { input, group_by, .. } => {
                if let LogicalPlan::Scan { table_name, .. } = &**input {
                    let columns = self.extract_columns(group_by);
                    self.record_access(table_name, columns, AccessOperation::GroupBy, 0.5);
                }
                self.analyze_plan(input);
            }

            LogicalPlan::Project { input, .. } |
            LogicalPlan::Limit { input, .. } => {
                self.analyze_plan(input);
            }

            _ => {}
        }
    }

    fn analyze_predicate(&mut self, table: &str, predicate: &LogicalExpr) {
        match predicate {
            LogicalExpr::Column { name, .. } => {
                self.record_access(table, vec![name.clone()], AccessOperation::Equality, 0.1);
            }
            LogicalExpr::BinaryExpr { left, right, .. } => {
                self.analyze_predicate(table, left);
                self.analyze_predicate(table, right);
            }
            _ => {}
        }
    }

    fn analyze_join_condition(&mut self, condition: &LogicalExpr) {
        if let LogicalExpr::Column { name, .. } = condition {
            // Record join column
            // Note: We'd need table context here in a real implementation
        }
    }

    fn extract_columns(&self, exprs: &[LogicalExpr]) -> Vec<String> {
        exprs.iter().filter_map(|expr| {
            if let LogicalExpr::Column { name, .. } = expr {
                Some(name.clone())
            } else {
                None
            }
        }).collect()
    }

    fn record_access(&mut self, table: &str, columns: Vec<String>, operation: AccessOperation, selectivity: f64) {
        let stats = self.table_stats.entry(table.to_string()).or_insert_with(|| TableStats {
            row_count: 1000,
            column_cardinality: HashMap::new(),
            access_patterns: Vec::new(),
        });

        if let Some(pattern) = stats.access_patterns.iter_mut().find(|p| p.columns == columns && p.operation == operation) {
            pattern.frequency += 1;
        } else {
            stats.access_patterns.push(AccessPattern {
                columns,
                operation,
                frequency: 1,
                selectivity,
            });
        }
    }

    /// Generate index recommendations
    pub fn recommend_indexes(&self) -> Vec<IndexRecommendation> {
        let mut recommendations = Vec::new();

        for (table_name, stats) in &self.table_stats {
            // Analyze each access pattern
            for pattern in &stats.access_patterns {
                if pattern.columns.is_empty() {
                    continue;
                }

                let index_type = self.recommend_index_type(&pattern.operation);
                let benefit = self.calculate_benefit(stats, pattern);
                let cost = self.calculate_cost(stats, pattern);
                let roi_score = self.calculate_roi(&benefit, &cost);

                // Only recommend if ROI is good
                if roi_score > 30.0 {
                    recommendations.push(IndexRecommendation {
                        table_name: table_name.clone(),
                        columns: pattern.columns.clone(),
                        index_type,
                        benefit,
                        cost,
                        roi_score,
                        create_statement: self.generate_create_index(
                            table_name,
                            &pattern.columns,
                            index_type
                        ),
                        reason: self.explain_recommendation(pattern),
                        query_patterns: vec![format!("{:?} on {}", pattern.operation, pattern.columns.join(", "))],
                    });
                }
            }
        }

        // Sort by ROI score (best first), handling NaN values safely
        recommendations.sort_by(|a, b| {
            b.roi_score
                .partial_cmp(&a.roi_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        recommendations
    }

    fn recommend_index_type(&self, operation: &AccessOperation) -> IndexType {
        match operation {
            AccessOperation::Equality | AccessOperation::Join => IndexType::Hash,
            AccessOperation::Range | AccessOperation::OrderBy => IndexType::BTree,
            AccessOperation::GroupBy => IndexType::BTree,
            AccessOperation::In => IndexType::BTree,
            AccessOperation::Like => IndexType::BTree,
        }
    }

    fn calculate_benefit(&self, stats: &TableStats, pattern: &AccessPattern) -> IndexBenefit {
        // Estimate speedup based on table size and selectivity
        let scan_cost = stats.row_count as f64;
        let index_cost = (stats.row_count as f64 * pattern.selectivity).max(1.0).log2() * 10.0;

        let speedup_multiplier = (scan_cost / index_cost).max(1.0);
        let time_savings_ms = scan_cost - index_cost;

        IndexBenefit {
            speedup_multiplier,
            time_savings_ms,
            affected_queries: pattern.frequency,
            improvement_percent: ((speedup_multiplier - 1.0) / speedup_multiplier * 100.0).min(99.0),
        }
    }

    fn calculate_cost(&self, stats: &TableStats, pattern: &AccessPattern) -> IndexCost {
        let column_count = pattern.columns.len();
        let row_count = stats.row_count;

        // Estimate storage: rows * columns * avg_column_size
        let storage_bytes = row_count * column_count * 32;

        // Creation time roughly proportional to table size
        let creation_time_ms = (row_count as f64).log10() * 100.0;

        // Maintenance overhead
        let maintenance_overhead_percent = 5.0 + (column_count as f64 * 2.0);

        // Write penalty
        let write_penalty_percent = 3.0 + (column_count as f64 * 1.5);

        IndexCost {
            storage_bytes,
            creation_time_ms,
            maintenance_overhead_percent,
            write_penalty_percent,
        }
    }

    fn calculate_roi(&self, benefit: &IndexBenefit, cost: &IndexCost) -> f64 {
        // ROI = (benefit - cost) / cost * 100

        let benefit_score = benefit.speedup_multiplier * benefit.affected_queries as f64;
        let cost_score = cost.storage_bytes as f64 / 1_000_000.0 + cost.maintenance_overhead_percent;

        if cost_score == 0.0 {
            return 100.0;
        }

        ((benefit_score - cost_score) / cost_score * 100.0).max(0.0).min(100.0)
    }

    fn generate_create_index(&self, table: &str, columns: &[String], index_type: IndexType) -> String {
        let index_name = format!("idx_{}_{}", table, columns.join("_"));
        let column_list = columns.join(", ");

        match index_type {
            IndexType::BTree => {
                format!("CREATE INDEX {} ON {} USING BTREE ({});", index_name, table, column_list)
            }
            IndexType::Hash => {
                format!("CREATE INDEX {} ON {} USING HASH ({});", index_name, table, column_list)
            }
            IndexType::GIN => {
                format!("CREATE INDEX {} ON {} USING GIN ({});", index_name, table, column_list)
            }
            IndexType::BRIN => {
                format!("CREATE INDEX {} ON {} USING BRIN ({});", index_name, table, column_list)
            }
        }
    }

    fn explain_recommendation(&self, pattern: &AccessPattern) -> String {
        match pattern.operation {
            AccessOperation::Equality => {
                format!("Frequent equality lookups on {} columns. Index will speed up WHERE clauses.", pattern.columns.len())
            }
            AccessOperation::Range => {
                format!("Range queries on {}. B-Tree index provides efficient range scans.", pattern.columns.join(", "))
            }
            AccessOperation::Join => {
                format!("Join operations on {}. Index improves join performance significantly.", pattern.columns.join(", "))
            }
            AccessOperation::OrderBy => {
                format!("Frequent ORDER BY on {}. Index eliminates sort operation.", pattern.columns.join(", "))
            }
            AccessOperation::GroupBy => {
                format!("GROUP BY operations on {}. Index speeds up aggregation.", pattern.columns.join(", "))
            }
            AccessOperation::In => {
                format!("IN clause queries on {}. Index reduces sequential scans.", pattern.columns.join(", "))
            }
            AccessOperation::Like => {
                format!("LIKE pattern matching on {}. Index helps with prefix matches.", pattern.columns.join(", "))
            }
        }
    }

    /// Format recommendations as a report
    pub fn format_report(&self, recommendations: &[IndexRecommendation]) -> String {
        let mut report = String::new();

        report.push_str("═══════════════════════════════════════════════════════════════\n");
        report.push_str("              INDEX RECOMMENDATION REPORT                      \n");
        report.push_str("═══════════════════════════════════════════════════════════════\n\n");

        report.push_str(&format!("Total Recommendations: {}\n", recommendations.len()));
        report.push_str(&format!("Workload Queries Analyzed: {}\n\n", self.workload.len()));

        for (i, rec) in recommendations.iter().enumerate() {
            report.push_str(&format!("───────────────────────────────────────────────────────────────\n"));
            report.push_str(&format!("  RECOMMENDATION #{} (ROI Score: {:.1}/100)\n", i + 1, rec.roi_score));
            report.push_str(&format!("───────────────────────────────────────────────────────────────\n\n"));

            report.push_str(&format!("Table: {}\n", rec.table_name));
            report.push_str(&format!("Columns: {}\n", rec.columns.join(", ")));
            report.push_str(&format!("Index Type: {:?}\n\n", rec.index_type));

            report.push_str("BENEFIT:\n");
            report.push_str(&format!("  • Speedup: {:.2}x faster\n", rec.benefit.speedup_multiplier));
            report.push_str(&format!("  • Time Savings: {:.2}ms per query\n", rec.benefit.time_savings_ms));
            report.push_str(&format!("  • Affected Queries: {}\n", rec.benefit.affected_queries));
            report.push_str(&format!("  • Improvement: {:.1}%\n\n", rec.benefit.improvement_percent));

            report.push_str("COST:\n");
            report.push_str(&format!("  • Storage: {} bytes\n", rec.cost.storage_bytes));
            report.push_str(&format!("  • Creation Time: {:.2}ms\n", rec.cost.creation_time_ms));
            report.push_str(&format!("  • Maintenance Overhead: {:.1}%\n", rec.cost.maintenance_overhead_percent));
            report.push_str(&format!("  • Write Penalty: {:.1}%\n\n", rec.cost.write_penalty_percent));

            report.push_str("REASON:\n");
            report.push_str(&format!("  {}\n\n", rec.reason));

            report.push_str("CREATE INDEX STATEMENT:\n");
            report.push_str(&format!("  {}\n\n", rec.create_statement));
        }

        report.push_str("═══════════════════════════════════════════════════════════════\n");

        report
    }
}

impl Default for IndexRecommender {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{Schema, Column, DataType};
    use std::sync::Arc;

    fn create_test_schema() -> Arc<Schema> {
        Arc::new(Schema {
            columns: vec![
                Column {
                    name: "id".to_string(),
                    data_type: DataType::Int4,
                    nullable: false,
                    primary_key: true,
                },
                Column {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                },
                Column {
                    name: "age".to_string(),
                    data_type: DataType::Int4,
                    nullable: true,
                    primary_key: false,
                },
            ],
        })
    }

    #[test]
    fn test_index_recommender_creation() {
        let recommender = IndexRecommender::new();
        assert_eq!(recommender.workload.len(), 0);
    }

    #[test]
    fn test_add_query() {
        let mut recommender = IndexRecommender::new();
        let scan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            schema: create_test_schema(),
            projection: None,
        };

        recommender.add_query(scan);
        assert_eq!(recommender.workload.len(), 1);
    }

    #[test]
    fn test_recommend_for_filter() {
        let mut recommender = IndexRecommender::new();

        // Add table stats
        let mut cardinality = HashMap::new();
        cardinality.insert("age".to_string(), 100);
        recommender.add_table_stats("users".to_string(), 10000, cardinality);

        // Add filtered query
        let scan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            schema: create_test_schema(),
            projection: None,
        };

        let filter = LogicalPlan::Filter {
            input: Box::new(scan),
            predicate: LogicalExpr::Column { table: None, name: "age".to_string()  },
        };

        recommender.add_query(filter);

        let recommendations = recommender.recommend_indexes();
        assert!(!recommendations.is_empty());
    }

    #[test]
    fn test_create_index_statement_generation() {
        let recommender = IndexRecommender::new();
        let statement = recommender.generate_create_index(
            "users",
            &["id".to_string(), "name".to_string()],
            IndexType::BTree
        );

        assert!(statement.contains("CREATE INDEX"));
        assert!(statement.contains("users"));
        assert!(statement.contains("id"));
        assert!(statement.contains("name"));
    }

    #[test]
    fn test_roi_calculation() {
        let recommender = IndexRecommender::new();

        let benefit = IndexBenefit {
            speedup_multiplier: 10.0,
            time_savings_ms: 900.0,
            affected_queries: 100,
            improvement_percent: 90.0,
        };

        let cost = IndexCost {
            storage_bytes: 1_000_000,
            creation_time_ms: 100.0,
            maintenance_overhead_percent: 5.0,
            write_penalty_percent: 3.0,
        };

        let roi = recommender.calculate_roi(&benefit, &cost);
        assert!(roi > 0.0);
    }
}
