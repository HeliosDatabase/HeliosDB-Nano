//! External Integrations for EXPLAIN
//!
//! This module provides integrations with external systems:
//! - Prometheus metrics export
//! - Grafana dashboard templates
//! - APM tool integration (Datadog, New Relic)
//! - CI/CD pipeline integration
//! - Alert configuration

#![allow(unused_variables)]

use crate::Result;
use super::explain::ExplainOutput;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Prometheus metrics for EXPLAIN operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrometheusMetrics {
    /// Counter: Total EXPLAIN requests
    pub explain_requests_total: u64,

    /// Counter: EXPLAIN requests by mode
    pub explain_requests_by_mode: HashMap<String, u64>,

    /// Histogram: EXPLAIN latency in milliseconds
    pub explain_duration_ms: Vec<f64>,

    /// Gauge: Current cache size
    pub explain_cache_size: usize,

    /// Counter: Cache hits
    pub explain_cache_hits_total: u64,

    /// Counter: Cache misses
    pub explain_cache_misses_total: u64,

    /// Gauge: Query plan complexity (nodes)
    pub explain_plan_nodes: usize,

    /// Histogram: Estimated query cost
    pub explain_estimated_cost: Vec<f64>,

    /// Counter: Features detected
    pub explain_features_detected: HashMap<String, u64>,
}

impl PrometheusMetrics {
    pub fn new() -> Self {
        Self {
            explain_requests_total: 0,
            explain_requests_by_mode: HashMap::new(),
            explain_duration_ms: Vec::new(),
            explain_cache_size: 0,
            explain_cache_hits_total: 0,
            explain_cache_misses_total: 0,
            explain_plan_nodes: 0,
            explain_estimated_cost: Vec::new(),
            explain_features_detected: HashMap::new(),
        }
    }

    /// Export metrics in Prometheus text format
    pub fn export_prometheus_format(&self) -> String {
        let mut output = String::new();

        // Total requests
        output.push_str("# HELP explain_requests_total Total number of EXPLAIN requests\n");
        output.push_str("# TYPE explain_requests_total counter\n");
        output.push_str(&format!("explain_requests_total {}\n\n", self.explain_requests_total));

        // Requests by mode
        output.push_str("# HELP explain_requests_by_mode EXPLAIN requests grouped by mode\n");
        output.push_str("# TYPE explain_requests_by_mode counter\n");
        for (mode, count) in &self.explain_requests_by_mode {
            output.push_str(&format!("explain_requests_by_mode{{mode=\"{}\"}} {}\n", mode, count));
        }
        output.push_str("\n");

        // Duration histogram
        output.push_str("# HELP explain_duration_ms EXPLAIN operation duration in milliseconds\n");
        output.push_str("# TYPE explain_duration_ms histogram\n");
        let buckets = vec![10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0];
        for bucket in buckets {
            let count = self.explain_duration_ms.iter().filter(|&&d| d <= bucket).count();
            output.push_str(&format!("explain_duration_ms_bucket{{le=\"{}\"}} {}\n", bucket, count));
        }
        output.push_str(&format!("explain_duration_ms_count {}\n", self.explain_duration_ms.len()));
        output.push_str(&format!("explain_duration_ms_sum {}\n\n",
            self.explain_duration_ms.iter().sum::<f64>()));

        // Cache metrics
        output.push_str("# HELP explain_cache_size Current number of cached EXPLAIN results\n");
        output.push_str("# TYPE explain_cache_size gauge\n");
        output.push_str(&format!("explain_cache_size {}\n\n", self.explain_cache_size));

        output.push_str("# HELP explain_cache_hits_total Total cache hits\n");
        output.push_str("# TYPE explain_cache_hits_total counter\n");
        output.push_str(&format!("explain_cache_hits_total {}\n\n", self.explain_cache_hits_total));

        output.push_str("# HELP explain_cache_misses_total Total cache misses\n");
        output.push_str("# TYPE explain_cache_misses_total counter\n");
        output.push_str(&format!("explain_cache_misses_total {}\n\n", self.explain_cache_misses_total));

        // Plan complexity
        output.push_str("# HELP explain_plan_nodes Number of nodes in query plan\n");
        output.push_str("# TYPE explain_plan_nodes gauge\n");
        output.push_str(&format!("explain_plan_nodes {}\n\n", self.explain_plan_nodes));

        output
    }
}

/// Grafana dashboard template
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrafanaDashboard {
    pub title: String,
    pub uid: String,
    pub panels: Vec<GrafanaPanel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrafanaPanel {
    pub id: u32,
    pub title: String,
    pub panel_type: String,
    pub targets: Vec<GrafanaTarget>,
    pub grid_pos: GridPosition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrafanaTarget {
    pub expr: String,
    pub legend_format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridPosition {
    pub h: u32,
    pub w: u32,
    pub x: u32,
    pub y: u32,
}

impl GrafanaDashboard {
    /// Create default EXPLAIN monitoring dashboard
    pub fn create_default() -> Self {
        Self {
            title: "HeliosDB EXPLAIN Analytics".to_string(),
            uid: "heliosdb-explain".to_string(),
            panels: vec![
                GrafanaPanel {
                    id: 1,
                    title: "EXPLAIN Requests per Second".to_string(),
                    panel_type: "graph".to_string(),
                    targets: vec![
                        GrafanaTarget {
                            expr: "rate(explain_requests_total[5m])".to_string(),
                            legend_format: "Requests/sec".to_string(),
                        },
                    ],
                    grid_pos: GridPosition { h: 8, w: 12, x: 0, y: 0 },
                },
                GrafanaPanel {
                    id: 2,
                    title: "EXPLAIN Latency P95".to_string(),
                    panel_type: "graph".to_string(),
                    targets: vec![
                        GrafanaTarget {
                            expr: "histogram_quantile(0.95, rate(explain_duration_ms_bucket[5m]))".to_string(),
                            legend_format: "P95 latency".to_string(),
                        },
                    ],
                    grid_pos: GridPosition { h: 8, w: 12, x: 12, y: 0 },
                },
                GrafanaPanel {
                    id: 3,
                    title: "Cache Hit Rate".to_string(),
                    panel_type: "singlestat".to_string(),
                    targets: vec![
                        GrafanaTarget {
                            expr: "rate(explain_cache_hits_total[5m]) / (rate(explain_cache_hits_total[5m]) + rate(explain_cache_misses_total[5m]))".to_string(),
                            legend_format: "Hit rate".to_string(),
                        },
                    ],
                    grid_pos: GridPosition { h: 4, w: 6, x: 0, y: 8 },
                },
                GrafanaPanel {
                    id: 4,
                    title: "Average Plan Complexity".to_string(),
                    panel_type: "singlestat".to_string(),
                    targets: vec![
                        GrafanaTarget {
                            expr: "avg(explain_plan_nodes)".to_string(),
                            legend_format: "Avg nodes".to_string(),
                        },
                    ],
                    grid_pos: GridPosition { h: 4, w: 6, x: 6, y: 8 },
                },
                GrafanaPanel {
                    id: 5,
                    title: "Requests by Mode".to_string(),
                    panel_type: "piechart".to_string(),
                    targets: vec![
                        GrafanaTarget {
                            expr: "sum by (mode) (rate(explain_requests_by_mode[5m]))".to_string(),
                            legend_format: "{{mode}}".to_string(),
                        },
                    ],
                    grid_pos: GridPosition { h: 8, w: 12, x: 12, y: 8 },
                },
            ],
        }
    }

    /// Export as JSON for Grafana import
    pub fn export_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

/// Datadog APM integration
#[derive(Debug, Clone)]
pub struct DatadogIntegration {
    api_key: String,
    service_name: String,
}

impl DatadogIntegration {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            service_name: "heliosdb-explain".to_string(),
        }
    }

    /// Send EXPLAIN trace to Datadog
    pub fn send_trace(&self, output: &ExplainOutput, duration_ms: f64) -> Result<()> {
        // In production, this would use the Datadog API
        // For now, just log the trace structure

        let trace = DatadogTrace {
            service: self.service_name.clone(),
            name: "explain.query".to_string(),
            resource: "EXPLAIN".to_string(),
            duration_ns: (duration_ms * 1_000_000.0) as u64,
            meta: vec![
                ("explain.cost".to_string(), output.total_cost.to_string()),
                ("explain.rows".to_string(), output.total_rows.to_string()),
                ("explain.features".to_string(), output.features.len().to_string()),
            ].into_iter().collect(),
        };

        // Would send via HTTP to Datadog API
        // dd.send_trace(&trace)?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
struct DatadogTrace {
    service: String,
    name: String,
    resource: String,
    duration_ns: u64,
    meta: HashMap<String, String>,
}

/// New Relic APM integration
#[derive(Debug, Clone)]
pub struct NewRelicIntegration {
    license_key: String,
    app_name: String,
}

impl NewRelicIntegration {
    pub fn new(license_key: String) -> Self {
        Self {
            license_key,
            app_name: "HeliosDB-EXPLAIN".to_string(),
        }
    }

    /// Send custom event to New Relic
    pub fn send_event(&self, output: &ExplainOutput, duration_ms: f64) -> Result<()> {
        let event = NewRelicEvent {
            event_type: "ExplainQuery".to_string(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            duration_ms,
            total_cost: output.total_cost,
            total_rows: output.total_rows,
            features_count: output.features.len(),
            has_ai_explanation: output.ai_explanation.is_some(),
        };

        // Would send via HTTP to New Relic API
        // nr.send_event(&event)?;

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
struct NewRelicEvent {
    #[serde(rename = "eventType")]
    event_type: String,
    timestamp: u64,
    duration_ms: f64,
    total_cost: f64,
    total_rows: usize,
    features_count: usize,
    has_ai_explanation: bool,
}

/// CI/CD integration for query plan regression detection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CIPlanRegression {
    pub baseline_plan: String,
    pub current_plan: String,
    pub cost_change_percent: f64,
    pub regression_detected: bool,
    pub details: Vec<String>,
}

pub struct CIIntegration;

impl CIIntegration {
    /// Compare query plans for regression
    pub fn detect_regression(
        baseline: &ExplainOutput,
        current: &ExplainOutput,
        threshold_percent: f64,
    ) -> CIPlanRegression {
        let cost_change = ((current.total_cost - baseline.total_cost) / baseline.total_cost) * 100.0;
        let regression = cost_change > threshold_percent;

        let mut details = Vec::new();

        if regression {
            details.push(format!(
                "Query cost increased by {:.1}% (threshold: {:.1}%)",
                cost_change,
                threshold_percent
            ));

            if current.total_rows != baseline.total_rows {
                details.push(format!(
                    "Row estimate changed: {} -> {}",
                    baseline.total_rows,
                    current.total_rows
                ));
            }

            if current.features.len() < baseline.features.len() {
                details.push(format!(
                    "Optimizer features decreased: {} -> {}",
                    baseline.features.len(),
                    current.features.len()
                ));
            }
        }

        CIPlanRegression {
            baseline_plan: format!("{:?}", baseline.plan),
            current_plan: format!("{:?}", current.plan),
            cost_change_percent: cost_change,
            regression_detected: regression,
            details,
        }
    }

    /// Generate CI report
    pub fn generate_ci_report(regression: &CIPlanRegression) -> String {
        let mut report = String::new();

        report.push_str("# Query Plan Regression Analysis\n\n");

        if regression.regression_detected {
            report.push_str("## ⚠️ REGRESSION DETECTED\n\n");
            report.push_str(&format!("Cost change: **{:+.1}%**\n\n", regression.cost_change_percent));

            report.push_str("### Details:\n");
            for detail in &regression.details {
                report.push_str(&format!("- {}\n", detail));
            }
        } else {
            report.push_str("## ✅ No Regression Detected\n\n");
            report.push_str(&format!("Cost change: {:+.1}%\n", regression.cost_change_percent));
        }

        report
    }
}

/// Alert configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    pub name: String,
    pub condition: AlertCondition,
    pub threshold: f64,
    pub severity: AlertSeverity,
    pub notification_channels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertCondition {
    HighLatencyP95,
    HighErrorRate,
    LowCacheHitRate,
    HighQueryCost,
    HighMemoryUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

impl AlertRule {
    /// Create default alert rules for EXPLAIN
    pub fn create_defaults() -> Vec<Self> {
        vec![
            AlertRule {
                name: "High EXPLAIN Latency".to_string(),
                condition: AlertCondition::HighLatencyP95,
                threshold: 100.0, // ms
                severity: AlertSeverity::Warning,
                notification_channels: vec!["#database-alerts".to_string()],
            },
            AlertRule {
                name: "EXPLAIN Error Rate".to_string(),
                condition: AlertCondition::HighErrorRate,
                threshold: 5.0, // percent
                severity: AlertSeverity::Critical,
                notification_channels: vec!["#database-alerts".to_string(), "pagerduty".to_string()],
            },
            AlertRule {
                name: "Low Cache Hit Rate".to_string(),
                condition: AlertCondition::LowCacheHitRate,
                threshold: 50.0, // percent
                severity: AlertSeverity::Info,
                notification_channels: vec!["#database-ops".to_string()],
            },
            AlertRule {
                name: "Expensive Query Detected".to_string(),
                condition: AlertCondition::HighQueryCost,
                threshold: 10000.0,
                severity: AlertSeverity::Warning,
                notification_channels: vec!["#database-performance".to_string()],
            },
        ]
    }

    /// Export alert rules to Prometheus format
    pub fn export_prometheus_alerts(rules: &[AlertRule]) -> String {
        let mut output = String::new();

        output.push_str("groups:\n");
        output.push_str("  - name: heliosdb_explain\n");
        output.push_str("    rules:\n");

        for rule in rules {
            output.push_str(&format!("      - alert: {}\n", rule.name));

            let expr = match rule.condition {
                AlertCondition::HighLatencyP95 => {
                    format!("histogram_quantile(0.95, rate(explain_duration_ms_bucket[5m])) > {}", rule.threshold)
                }
                AlertCondition::HighErrorRate => {
                    format!("rate(explain_errors_total[5m]) / rate(explain_requests_total[5m]) * 100 > {}", rule.threshold)
                }
                AlertCondition::LowCacheHitRate => {
                    format!("rate(explain_cache_hits_total[5m]) / (rate(explain_cache_hits_total[5m]) + rate(explain_cache_misses_total[5m])) * 100 < {}", rule.threshold)
                }
                AlertCondition::HighQueryCost => {
                    format!("explain_estimated_cost > {}", rule.threshold)
                }
                AlertCondition::HighMemoryUsage => {
                    format!("explain_memory_usage_mb > {}", rule.threshold)
                }
            };

            output.push_str(&format!("        expr: {}\n", expr));
            output.push_str("        for: 5m\n");
            output.push_str("        labels:\n");
            output.push_str(&format!("          severity: {:?}\n", rule.severity));
            output.push_str("        annotations:\n");
            output.push_str(&format!("          summary: {}\n", rule.name));
        }

        output
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sql::logical_plan::LogicalPlan;
    use crate::{Schema, Column, DataType};
    use std::sync::Arc;

    #[test]
    fn test_prometheus_metrics_export() {
        let mut metrics = PrometheusMetrics::new();
        metrics.explain_requests_total = 100;
        metrics.explain_duration_ms = vec![10.0, 25.0, 50.0];

        let export = metrics.export_prometheus_format();

        assert!(export.contains("explain_requests_total 100"));
        assert!(export.contains("explain_duration_ms"));
    }

    #[test]
    fn test_grafana_dashboard() {
        let dashboard = GrafanaDashboard::create_default();

        assert_eq!(dashboard.title, "HeliosDB EXPLAIN Analytics");
        assert!(!dashboard.panels.is_empty());

        let json = dashboard.export_json();
        assert!(json.contains("HeliosDB EXPLAIN Analytics"));
    }

    #[test]
    fn test_ci_regression_detection() {
        use crate::{Schema, Column, DataType};
        use std::sync::Arc;
        use super::super::explain::*;

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
                },
            ],
        });

        let plan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema,
            projection: None,
            as_of: None,
        };

        let planner = ExplainPlanner::new(ExplainMode::Standard, ExplainFormat::Text);
        let baseline = planner.explain(&plan).unwrap();

        let mut current = baseline.clone();
        current.total_cost *= 1.5; // 50% increase

        let regression = CIIntegration::detect_regression(&baseline, &current, 10.0);

        assert!(regression.regression_detected);
        assert!(regression.cost_change_percent > 10.0);
    }

    #[test]
    fn test_alert_rules() {
        let rules = AlertRule::create_defaults();

        assert!(!rules.is_empty());
        assert!(rules.iter().any(|r| matches!(r.condition, AlertCondition::HighLatencyP95)));

        let prometheus_alerts = AlertRule::export_prometheus_alerts(&rules);
        assert!(prometheus_alerts.contains("alert:"));
    }

    #[test]
    fn test_datadog_integration() {
        let dd = DatadogIntegration::new("test-key".to_string());
        assert_eq!(dd.service_name, "heliosdb-explain");
    }

    #[test]
    fn test_newrelic_integration() {
        let nr = NewRelicIntegration::new("test-license".to_string());
        assert_eq!(nr.app_name, "HeliosDB-EXPLAIN");
    }
}
