//! Query Performance Regression Detection
//!
//! Stores execution plan history and detects performance regressions.
//! Features:
//! - Execution plan history storage
//! - Plan comparison across executions
//! - Regression detection (better/worse)
//! - Performance degradation alerts
//! - Optimizer decision tracking over time

use crate::Result;
use super::explain::{ExplainOutput, PlanNode};
use super::realtime_explain::ExecutionStats;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

/// Historical execution record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    /// Unique query fingerprint (normalized SQL)
    pub query_fingerprint: String,

    /// Execution timestamp
    pub timestamp: SystemTime,

    /// Query plan used
    pub plan: PlanNode,

    /// Estimated cost
    pub estimated_cost: f64,

    /// Actual execution time
    pub actual_time_ms: f64,

    /// Rows processed
    pub rows_processed: usize,

    /// Execution statistics
    pub stats: Option<ExecutionStats>,

    /// Optimizer version/configuration
    pub optimizer_version: String,
}

/// Plan comparison result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanComparison {
    /// Query fingerprint
    pub query_fingerprint: String,

    /// Previous execution
    pub previous: ExecutionSummary,

    /// Current execution
    pub current: ExecutionSummary,

    /// Change type
    pub change_type: ChangeType,

    /// Performance delta
    pub performance_delta: PerformanceDelta,

    /// Plan differences
    pub plan_differences: Vec<String>,

    /// Recommendations
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSummary {
    pub timestamp: SystemTime,
    pub estimated_cost: f64,
    pub actual_time_ms: f64,
    pub rows_processed: usize,
    pub plan_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeType {
    Improvement,   // Performance got better
    Regression,    // Performance got worse
    PlanChange,    // Plan changed but similar performance
    NoChange,      // Same plan, similar performance
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceDelta {
    /// Time change in milliseconds
    pub time_delta_ms: f64,

    /// Time change percentage
    pub time_delta_percent: f64,

    /// Cost change
    pub cost_delta: f64,

    /// Rows change
    pub rows_delta: i64,

    /// Severity (0-100, higher = more severe)
    pub severity: f64,
}

/// Regression detector
pub struct RegressionDetector {
    history: HashMap<String, Vec<ExecutionRecord>>,
    max_history_per_query: usize,
    regression_threshold_percent: f64,
}

impl RegressionDetector {
    pub fn new() -> Self {
        Self {
            history: HashMap::new(),
            max_history_per_query: 100,
            regression_threshold_percent: 20.0, // Alert if >20% slower
        }
    }

    pub fn with_threshold(mut self, threshold_percent: f64) -> Self {
        self.regression_threshold_percent = threshold_percent;
        self
    }

    /// Record an execution
    pub fn record_execution(
        &mut self,
        query_fingerprint: String,
        plan: PlanNode,
        estimated_cost: f64,
        actual_time_ms: f64,
        rows_processed: usize,
        stats: Option<ExecutionStats>,
    ) {
        let record = ExecutionRecord {
            query_fingerprint: query_fingerprint.clone(),
            timestamp: SystemTime::now(),
            plan,
            estimated_cost,
            actual_time_ms,
            rows_processed,
            stats,
            optimizer_version: "v1.0".to_string(),
        };

        let history = self.history.entry(query_fingerprint).or_insert_with(Vec::new);
        history.push(record);

        // Keep only recent history
        if history.len() > self.max_history_per_query {
            history.remove(0);
        }
    }

    /// Compare current execution with historical baseline
    pub fn compare_with_history(
        &self,
        query_fingerprint: &str,
        current_plan: &PlanNode,
        current_time_ms: f64,
        current_rows: usize,
    ) -> Option<PlanComparison> {
        let history = self.history.get(query_fingerprint)?;
        if history.is_empty() {
            return None;
        }

        // Calculate baseline from recent executions
        let baseline = self.calculate_baseline(history)?;

        let previous = ExecutionSummary {
            timestamp: baseline.timestamp,
            estimated_cost: baseline.estimated_cost,
            actual_time_ms: baseline.actual_time_ms,
            rows_processed: baseline.rows_processed,
            plan_hash: self.hash_plan(&baseline.plan),
        };

        let current = ExecutionSummary {
            timestamp: SystemTime::now(),
            estimated_cost: current_plan.cost,
            actual_time_ms: current_time_ms,
            rows_processed: current_rows,
            plan_hash: self.hash_plan(current_plan),
        };

        let performance_delta = self.calculate_delta(&baseline, current_time_ms, current_rows);
        let change_type = self.classify_change(&previous, &current, &performance_delta);
        let plan_differences = self.find_plan_differences(&baseline.plan, current_plan);
        let recommendations = self.generate_recommendations(&change_type, &performance_delta, &plan_differences);

        Some(PlanComparison {
            query_fingerprint: query_fingerprint.to_string(),
            previous,
            current,
            change_type,
            performance_delta,
            plan_differences,
            recommendations,
        })
    }

    /// Calculate baseline from historical executions
    /// Returns None if history is empty
    fn calculate_baseline(&self, history: &[ExecutionRecord]) -> Option<ExecutionRecord> {
        if history.is_empty() {
            return None;
        }

        // Use median of recent executions
        let recent_count = 10.min(history.len());
        let recent = &history[history.len().saturating_sub(recent_count)..];

        let mut times: Vec<f64> = recent.iter().map(|r| r.actual_time_ms).collect();
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let median_time = times.get(times.len() / 2).copied().unwrap_or(0.0);

        // Find record closest to median, with fallback to first record
        recent.iter()
            .min_by_key(|r| ((r.actual_time_ms - median_time).abs() * 1000.0) as i64)
            .or_else(|| recent.first())
            .cloned()
    }

    /// Calculate performance delta
    fn calculate_delta(
        &self,
        baseline: &ExecutionRecord,
        current_time_ms: f64,
        current_rows: usize,
    ) -> PerformanceDelta {
        let time_delta_ms = current_time_ms - baseline.actual_time_ms;
        let time_delta_percent = if baseline.actual_time_ms > 0.0 {
            (time_delta_ms / baseline.actual_time_ms) * 100.0
        } else {
            0.0
        };

        let cost_delta = current_time_ms - baseline.estimated_cost;
        let rows_delta = current_rows as i64 - baseline.rows_processed as i64;

        // Severity based on time delta percentage
        let severity = if time_delta_percent.abs() < 10.0 {
            10.0
        } else if time_delta_percent.abs() < 20.0 {
            30.0
        } else if time_delta_percent.abs() < 50.0 {
            60.0
        } else {
            90.0
        };

        PerformanceDelta {
            time_delta_ms,
            time_delta_percent,
            cost_delta,
            rows_delta,
            severity,
        }
    }

    /// Classify type of change
    fn classify_change(
        &self,
        previous: &ExecutionSummary,
        current: &ExecutionSummary,
        delta: &PerformanceDelta,
    ) -> ChangeType {
        let plan_changed = previous.plan_hash != current.plan_hash;

        if delta.time_delta_percent.abs() < 5.0 {
            if plan_changed {
                ChangeType::PlanChange
            } else {
                ChangeType::NoChange
            }
        } else if delta.time_delta_percent > self.regression_threshold_percent {
            ChangeType::Regression
        } else if delta.time_delta_percent < -10.0 {
            ChangeType::Improvement
        } else if plan_changed {
            ChangeType::PlanChange
        } else {
            ChangeType::NoChange
        }
    }

    /// Find differences between plans
    fn find_plan_differences(&self, previous: &PlanNode, current: &PlanNode) -> Vec<String> {
        let mut differences = Vec::new();

        // Compare node types
        if previous.node_type != current.node_type {
            differences.push(format!(
                "Node type changed: {} → {}",
                previous.node_type, current.node_type
            ));
        }

        // Compare operations
        if previous.operation != current.operation {
            differences.push(format!(
                "Operation changed: {} → {}",
                previous.operation, current.operation
            ));
        }

        // Compare costs
        let cost_change_percent = ((current.cost - previous.cost) / previous.cost.max(1.0)) * 100.0;
        if cost_change_percent.abs() > 20.0 {
            differences.push(format!(
                "Cost changed by {:.1}%: {:.2} → {:.2}",
                cost_change_percent, previous.cost, current.cost
            ));
        }

        // Compare row estimates
        let rows_change_percent = ((current.rows as f64 - previous.rows as f64) / previous.rows.max(1) as f64) * 100.0;
        if rows_change_percent.abs() > 20.0 {
            differences.push(format!(
                "Row estimate changed by {:.1}%: {} → {}",
                rows_change_percent, previous.rows, current.rows
            ));
        }

        // Compare children count
        if previous.children.len() != current.children.len() {
            differences.push(format!(
                "Number of child nodes changed: {} → {}",
                previous.children.len(), current.children.len()
            ));
        }

        // Recursively check children
        for (i, (prev_child, curr_child)) in previous.children.iter().zip(current.children.iter()).enumerate() {
            let child_diffs = self.find_plan_differences(prev_child, curr_child);
            for diff in child_diffs {
                differences.push(format!("Child {}: {}", i, diff));
            }
        }

        differences
    }

    /// Generate recommendations based on changes
    fn generate_recommendations(
        &self,
        change_type: &ChangeType,
        delta: &PerformanceDelta,
        differences: &[String],
    ) -> Vec<String> {
        let mut recommendations = Vec::new();

        match change_type {
            ChangeType::Regression => {
                recommendations.push(format!(
                    "⚠️  PERFORMANCE REGRESSION DETECTED: Query is {:.1}% slower ({:.2}ms increase)",
                    delta.time_delta_percent, delta.time_delta_ms
                ));

                if !differences.is_empty() {
                    recommendations.push("Query plan has changed. Possible causes:".to_string());
                    for diff in differences.iter().take(3) {
                        recommendations.push(format!("  • {}", diff));
                    }
                }

                recommendations.push("Recommended actions:".to_string());
                recommendations.push("  1. Update table statistics (ANALYZE)".to_string());
                recommendations.push("  2. Review recent schema changes".to_string());
                recommendations.push("  3. Check for missing or dropped indexes".to_string());
                recommendations.push("  4. Consider query rewrite".to_string());
            }

            ChangeType::Improvement => {
                recommendations.push(format!(
                    "✓ Performance improved: Query is {:.1}% faster ({:.2}ms reduction)",
                    -delta.time_delta_percent, -delta.time_delta_ms
                ));

                if !differences.is_empty() {
                    recommendations.push("Query plan improvements:".to_string());
                    for diff in differences.iter().take(3) {
                        recommendations.push(format!("  • {}", diff));
                    }
                }
            }

            ChangeType::PlanChange => {
                recommendations.push("Query plan changed but performance is similar".to_string());
                recommendations.push("Monitor future executions for stability".to_string());
            }

            ChangeType::NoChange => {
                recommendations.push("Query performance is stable".to_string());
            }
        }

        recommendations
    }

    /// Hash a plan for comparison
    fn hash_plan(&self, plan: &PlanNode) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        plan.node_type.hash(&mut hasher);
        plan.operation.hash(&mut hasher);
        plan.children.len().hash(&mut hasher);

        format!("{:x}", hasher.finish())
    }

    /// Get execution history for a query
    pub fn get_history(&self, query_fingerprint: &str) -> Option<&Vec<ExecutionRecord>> {
        self.history.get(query_fingerprint)
    }

    /// Get trend analysis for a query
    pub fn analyze_trend(&self, query_fingerprint: &str) -> Option<TrendAnalysis> {
        let history = self.history.get(query_fingerprint)?;
        if history.len() < 3 {
            return None;
        }

        let times: Vec<f64> = history.iter().map(|r| r.actual_time_ms).collect();

        // Calculate linear regression
        let n = times.len() as f64;
        let x_mean = (n - 1.0) / 2.0;
        let y_mean = times.iter().sum::<f64>() / n;

        let mut numerator = 0.0;
        let mut denominator = 0.0;

        for (i, &time) in times.iter().enumerate() {
            let x = i as f64;
            numerator += (x - x_mean) * (time - y_mean);
            denominator += (x - x_mean).powi(2);
        }

        let slope = if denominator != 0.0 {
            numerator / denominator
        } else {
            0.0
        };

        let trend = if slope > 0.5 {
            Trend::Degrading
        } else if slope < -0.5 {
            Trend::Improving
        } else {
            Trend::Stable
        };

        Some(TrendAnalysis {
            query_fingerprint: query_fingerprint.to_string(),
            executions_analyzed: history.len(),
            trend,
            slope,
            average_time_ms: y_mean,
            min_time_ms: times.iter().cloned().fold(f64::INFINITY, f64::min),
            max_time_ms: times.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        })
    }

    /// Format comparison report
    pub fn format_comparison_report(&self, comparison: &PlanComparison) -> String {
        let mut report = String::new();

        report.push_str("═══════════════════════════════════════════════════════════════\n");
        report.push_str("           QUERY PERFORMANCE COMPARISON REPORT                \n");
        report.push_str("═══════════════════════════════════════════════════════════════\n\n");

        report.push_str(&format!("Query: {}\n", comparison.query_fingerprint));
        report.push_str(&format!("Change Type: {:?}\n", comparison.change_type));
        report.push_str(&format!("Severity: {:.1}/100\n\n", comparison.performance_delta.severity));

        report.push_str("───────────────────────────────────────────────────────────────\n");
        report.push_str("  PERFORMANCE METRICS\n");
        report.push_str("───────────────────────────────────────────────────────────────\n\n");

        report.push_str(&format!("Execution Time:\n"));
        report.push_str(&format!("  Previous: {:.2}ms\n", comparison.previous.actual_time_ms));
        report.push_str(&format!("  Current:  {:.2}ms\n", comparison.current.actual_time_ms));
        report.push_str(&format!("  Delta:    {:+.2}ms ({:+.1}%)\n\n",
            comparison.performance_delta.time_delta_ms,
            comparison.performance_delta.time_delta_percent));

        report.push_str(&format!("Rows Processed:\n"));
        report.push_str(&format!("  Previous: {}\n", comparison.previous.rows_processed));
        report.push_str(&format!("  Current:  {}\n", comparison.current.rows_processed));
        report.push_str(&format!("  Delta:    {:+}\n\n", comparison.performance_delta.rows_delta));

        if !comparison.plan_differences.is_empty() {
            report.push_str("───────────────────────────────────────────────────────────────\n");
            report.push_str("  PLAN DIFFERENCES\n");
            report.push_str("───────────────────────────────────────────────────────────────\n\n");

            for diff in &comparison.plan_differences {
                report.push_str(&format!("  • {}\n", diff));
            }
            report.push_str("\n");
        }

        if !comparison.recommendations.is_empty() {
            report.push_str("───────────────────────────────────────────────────────────────\n");
            report.push_str("  RECOMMENDATIONS\n");
            report.push_str("───────────────────────────────────────────────────────────────\n\n");

            for rec in &comparison.recommendations {
                report.push_str(&format!("{}\n", rec));
            }
            report.push_str("\n");
        }

        report.push_str("═══════════════════════════════════════════════════════════════\n");

        report
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrendAnalysis {
    pub query_fingerprint: String,
    pub executions_analyzed: usize,
    pub trend: Trend,
    pub slope: f64,
    pub average_time_ms: f64,
    pub min_time_ms: f64,
    pub max_time_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trend {
    Improving,
    Stable,
    Degrading,
}

impl Default for RegressionDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn create_test_plan(cost: f64, rows: usize) -> PlanNode {
        PlanNode {
            node_type: "Scan".to_string(),
            operation: "Seq Scan".to_string(),
            cost,
            rows,
            details: HashMap::new(),
            children: vec![],
        }
    }

    #[test]
    fn test_regression_detector_creation() {
        let detector = RegressionDetector::new();
        assert_eq!(detector.history.len(), 0);
    }

    #[test]
    fn test_record_execution() {
        let mut detector = RegressionDetector::new();
        let plan = create_test_plan(100.0, 1000);

        detector.record_execution(
            "SELECT * FROM users".to_string(),
            plan,
            100.0,
            95.0,
            1000,
            None,
        );

        assert_eq!(detector.history.len(), 1);
    }

    #[test]
    fn test_detect_regression() {
        let mut detector = RegressionDetector::new();
        let fingerprint = "SELECT * FROM users WHERE age > 25".to_string();

        // Record baseline
        for _ in 0..5 {
            detector.record_execution(
                fingerprint.clone(),
                create_test_plan(100.0, 1000),
                100.0,
                95.0,
                1000,
                None,
            );
        }

        // Record regression
        let comparison = detector.compare_with_history(
            &fingerprint,
            &create_test_plan(100.0, 1000),
            200.0, // 2x slower
            1000,
        );

        assert!(comparison.is_some());
        let comp = comparison.unwrap();
        assert_eq!(comp.change_type, ChangeType::Regression);
    }

    #[test]
    fn test_detect_improvement() {
        let mut detector = RegressionDetector::new();
        let fingerprint = "SELECT * FROM orders".to_string();

        // Record baseline
        for _ in 0..5 {
            detector.record_execution(
                fingerprint.clone(),
                create_test_plan(100.0, 1000),
                100.0,
                100.0,
                1000,
                None,
            );
        }

        // Record improvement
        let comparison = detector.compare_with_history(
            &fingerprint,
            &create_test_plan(50.0, 1000),
            45.0, // Much faster
            1000,
        );

        assert!(comparison.is_some());
        let comp = comparison.unwrap();
        assert_eq!(comp.change_type, ChangeType::Improvement);
    }

    #[test]
    fn test_plan_difference_detection() {
        let detector = RegressionDetector::new();

        let plan1 = create_test_plan(100.0, 1000);
        let plan2 = create_test_plan(150.0, 1500);

        let differences = detector.find_plan_differences(&plan1, &plan2);

        assert!(!differences.is_empty());
    }

    #[test]
    fn test_trend_analysis() {
        let mut detector = RegressionDetector::new();
        let fingerprint = "SELECT COUNT(*) FROM sales".to_string();

        // Record degrading trend
        for i in 0..10 {
            detector.record_execution(
                fingerprint.clone(),
                create_test_plan(100.0, 1000),
                100.0,
                50.0 + (i as f64 * 5.0), // Getting slower
                1000,
                None,
            );
        }

        let trend = detector.analyze_trend(&fingerprint);
        assert!(trend.is_some());

        let trend = trend.unwrap();
        assert_eq!(trend.trend, Trend::Degrading);
    }
}
