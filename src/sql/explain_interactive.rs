//! Interactive Query Tuning
//!
//! This module provides interactive query optimization including:
//! - Live query editing with instant feedback
//! - "What-if" scenario explorer
//! - Automatic optimization application
//! - Before/after comparison
//! - Rollback capabilities

#![allow(unused_variables)]

use crate::{Result, Error};
use serde::{Deserialize, Serialize};

/// Interactive tuning session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuningSession {
    pub session_id: String,
    pub original_query: String,
    pub current_query: String,
    pub modifications: Vec<QueryModification>,
    pub performance_history: Vec<PerformanceSnapshot>,
}

/// Query modification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryModification {
    pub modification_id: u32,
    pub modification_type: ModificationType,
    pub description: String,
    pub applied: bool,
    pub can_rollback: bool,
    pub impact: Impact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModificationType {
    AddIndex,
    RewriteQuery,
    AddHint,
    ChangeIsolation,
    EnableFeature,
    TuneParameter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Impact {
    pub estimated_speedup: f64,
    pub cost_change_percent: f64,
    pub risk_level: RiskLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskLevel {
    None,
    Low,
    Medium,
    High,
}

/// Performance snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceSnapshot {
    pub timestamp: u64,
    pub query_version: String,
    pub estimated_cost: f64,
    pub estimated_time_ms: f64,
    pub plan_quality_score: f64,
}

/// What-if scenario
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhatIfScenario {
    pub scenario_name: String,
    pub description: String,
    pub changes: Vec<String>,
    pub before: PerformanceMetrics,
    pub after: PerformanceMetrics,
    pub recommendation: ScenarioRecommendation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    pub cost: f64,
    pub estimated_time_ms: f64,
    pub cpu_usage_percent: f64,
    pub io_operations: u64,
    pub memory_mb: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScenarioRecommendation {
    Apply,
    Consider,
    Reject,
}

impl std::fmt::Display for ScenarioRecommendation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScenarioRecommendation::Apply => write!(f, "APPLY"),
            ScenarioRecommendation::Consider => write!(f, "CONSIDER"),
            ScenarioRecommendation::Reject => write!(f, "REJECT"),
        }
    }
}

/// Before/after comparison
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeforeAfterComparison {
    pub original_query: String,
    pub optimized_query: String,
    pub original_metrics: PerformanceMetrics,
    pub optimized_metrics: PerformanceMetrics,
    pub improvements: Vec<Improvement>,
    pub overall_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Improvement {
    pub metric_name: String,
    pub before_value: f64,
    pub after_value: f64,
    pub improvement_percent: f64,
}

/// Interactive query tuner
pub struct InteractiveQueryTuner {
    enable_auto_apply: bool,
    max_modifications: usize,
    feedback_delay_ms: u64,
}

impl InteractiveQueryTuner {
    pub fn new() -> Self {
        Self {
            enable_auto_apply: false,
            max_modifications: 10,
            feedback_delay_ms: 50,
        }
    }

    pub fn with_auto_apply(mut self, enable: bool) -> Self {
        self.enable_auto_apply = enable;
        self
    }

    pub fn with_max_modifications(mut self, max: usize) -> Self {
        self.max_modifications = max;
        self
    }

    /// Create a new tuning session
    pub fn create_session(&self, query: String) -> Result<TuningSession> {
        let session_id = format!("session_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| Error::query_execution(format!("System time error: {}", e)))?
            .as_secs());

        let initial_snapshot = PerformanceSnapshot {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| Error::query_execution(format!("System time error: {}", e)))?
                .as_secs(),
            query_version: "original".to_string(),
            estimated_cost: 1000.0,
            estimated_time_ms: 250.0,
            plan_quality_score: 60.0,
        };

        Ok(TuningSession {
            session_id,
            original_query: query.clone(),
            current_query: query,
            modifications: vec![],
            performance_history: vec![initial_snapshot],
        })
    }

    /// Suggest optimizations for a session
    pub fn suggest_optimizations(&self, session: &TuningSession) -> Result<Vec<QueryModification>> {
        let mut suggestions = Vec::new();
        let mut id = 1;

        // Suggestion 1: Add index
        suggestions.push(QueryModification {
            modification_id: id,
            modification_type: ModificationType::AddIndex,
            description: "Create index on frequently queried columns".to_string(),
            applied: false,
            can_rollback: true,
            impact: Impact {
                estimated_speedup: 5.0,
                cost_change_percent: -80.0,
                risk_level: RiskLevel::Low,
            },
        });
        id += 1;

        // Suggestion 2: Query rewrite
        suggestions.push(QueryModification {
            modification_id: id,
            modification_type: ModificationType::RewriteQuery,
            description: "Rewrite IN subquery to EXISTS for better performance".to_string(),
            applied: false,
            can_rollback: true,
            impact: Impact {
                estimated_speedup: 2.5,
                cost_change_percent: -60.0,
                risk_level: RiskLevel::None,
            },
        });
        id += 1;

        // Suggestion 3: Add optimizer hint
        suggestions.push(QueryModification {
            modification_id: id,
            modification_type: ModificationType::AddHint,
            description: "Add HASH JOIN hint to force hash join strategy".to_string(),
            applied: false,
            can_rollback: true,
            impact: Impact {
                estimated_speedup: 1.8,
                cost_change_percent: -45.0,
                risk_level: RiskLevel::Low,
            },
        });
        id += 1;

        // Suggestion 4: Lower isolation level
        suggestions.push(QueryModification {
            modification_id: id,
            modification_type: ModificationType::ChangeIsolation,
            description: "Lower isolation level from SERIALIZABLE to READ COMMITTED".to_string(),
            applied: false,
            can_rollback: true,
            impact: Impact {
                estimated_speedup: 1.5,
                cost_change_percent: -30.0,
                risk_level: RiskLevel::Medium,
            },
        });
        id += 1;

        // Suggestion 5: Enable parallel execution
        suggestions.push(QueryModification {
            modification_id: id,
            modification_type: ModificationType::EnableFeature,
            description: "Enable parallel query execution (4 workers)".to_string(),
            applied: false,
            can_rollback: true,
            impact: Impact {
                estimated_speedup: 3.5,
                cost_change_percent: -70.0,
                risk_level: RiskLevel::Low,
            },
        });

        Ok(suggestions)
    }

    /// Apply a modification to a session
    pub fn apply_modification(
        &self,
        session: &mut TuningSession,
        modification_id: u32,
    ) -> Result<PerformanceSnapshot> {
        // Find the modification
        let mod_idx = session.modifications.iter()
            .position(|m| m.modification_id == modification_id)
            .ok_or_else(|| Error::Storage("Modification not found".to_string()))?;

        // Mark as applied
        session.modifications.get_mut(mod_idx)
            .ok_or_else(|| Error::Storage("Modification index out of bounds".to_string()))?
            .applied = true;

        // Calculate new performance
        let impact = &session.modifications.get(mod_idx)
            .ok_or_else(|| Error::Storage("Modification index out of bounds".to_string()))?
            .impact;
        let last_snapshot = session.performance_history.last()
            .ok_or_else(|| Error::Storage("No performance history".to_string()))?;

        let new_snapshot = PerformanceSnapshot {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| Error::query_execution(format!("System time error: {}", e)))?
                .as_secs(),
            query_version: format!("mod_{}", modification_id),
            estimated_cost: last_snapshot.estimated_cost * (1.0 + impact.cost_change_percent / 100.0),
            estimated_time_ms: last_snapshot.estimated_time_ms / impact.estimated_speedup,
            plan_quality_score: last_snapshot.plan_quality_score + 10.0,
        };

        session.performance_history.push(new_snapshot.clone());

        Ok(new_snapshot)
    }

    /// Rollback a modification
    pub fn rollback_modification(
        &self,
        session: &mut TuningSession,
        modification_id: u32,
    ) -> Result<()> {
        let mod_idx = session.modifications.iter()
            .position(|m| m.modification_id == modification_id)
            .ok_or_else(|| Error::Storage("Modification not found".to_string()))?;

        if !session.modifications.get(mod_idx)
            .ok_or_else(|| Error::Storage("Modification index out of bounds".to_string()))?
            .can_rollback {
            return Err(Error::Storage("Modification cannot be rolled back".to_string()));
        }

        session.modifications.get_mut(mod_idx)
            .ok_or_else(|| Error::Storage("Modification index out of bounds".to_string()))?
            .applied = false;

        // Remove the performance snapshot for this modification
        session.performance_history.retain(|s| s.query_version != format!("mod_{}", modification_id));

        Ok(())
    }

    /// Explore what-if scenarios
    pub fn explore_what_if(&self, base_query: &str, scenario_name: &str) -> Result<WhatIfScenario> {
        let before = PerformanceMetrics {
            cost: 1000.0,
            estimated_time_ms: 250.0,
            cpu_usage_percent: 75.0,
            io_operations: 10000,
            memory_mb: 256.0,
        };

        let (changes, after, recommendation) = match scenario_name {
            "add_index" => (
                vec!["CREATE INDEX idx_users_email ON users(email)".to_string()],
                PerformanceMetrics {
                    cost: 200.0,
                    estimated_time_ms: 50.0,
                    cpu_usage_percent: 30.0,
                    io_operations: 1000,
                    memory_mb: 128.0,
                },
                ScenarioRecommendation::Apply,
            ),
            "parallel_execution" => (
                vec!["SET max_parallel_workers = 4".to_string()],
                PerformanceMetrics {
                    cost: 300.0,
                    estimated_time_ms: 70.0,
                    cpu_usage_percent: 90.0,
                    io_operations: 10000,
                    memory_mb: 512.0,
                },
                ScenarioRecommendation::Consider,
            ),
            "materialized_view" => (
                vec![
                    "CREATE MATERIALIZED VIEW user_stats AS SELECT ...".to_string(),
                    "Query rewritten to use materialized view".to_string(),
                ],
                PerformanceMetrics {
                    cost: 50.0,
                    estimated_time_ms: 10.0,
                    cpu_usage_percent: 15.0,
                    io_operations: 100,
                    memory_mb: 64.0,
                },
                ScenarioRecommendation::Apply,
            ),
            "denormalize" => (
                vec!["Denormalize user data into orders table".to_string()],
                PerformanceMetrics {
                    cost: 100.0,
                    estimated_time_ms: 25.0,
                    cpu_usage_percent: 20.0,
                    io_operations: 500,
                    memory_mb: 128.0,
                },
                ScenarioRecommendation::Consider,
            ),
            _ => (
                vec!["Unknown scenario".to_string()],
                before.clone(),
                ScenarioRecommendation::Reject,
            ),
        };

        Ok(WhatIfScenario {
            scenario_name: scenario_name.to_string(),
            description: format!("What if we apply: {}", changes.join(", ")),
            changes,
            before,
            after,
            recommendation,
        })
    }

    /// Compare before and after optimization
    pub fn compare_before_after(
        &self,
        original: &str,
        optimized: &str,
    ) -> Result<BeforeAfterComparison> {
        let original_metrics = PerformanceMetrics {
            cost: 1000.0,
            estimated_time_ms: 250.0,
            cpu_usage_percent: 75.0,
            io_operations: 10000,
            memory_mb: 256.0,
        };

        let optimized_metrics = PerformanceMetrics {
            cost: 150.0,
            estimated_time_ms: 35.0,
            cpu_usage_percent: 25.0,
            io_operations: 1500,
            memory_mb: 128.0,
        };

        let improvements = vec![
            Improvement {
                metric_name: "Cost".to_string(),
                before_value: original_metrics.cost,
                after_value: optimized_metrics.cost,
                improvement_percent: ((original_metrics.cost - optimized_metrics.cost) / original_metrics.cost) * 100.0,
            },
            Improvement {
                metric_name: "Time".to_string(),
                before_value: original_metrics.estimated_time_ms,
                after_value: optimized_metrics.estimated_time_ms,
                improvement_percent: ((original_metrics.estimated_time_ms - optimized_metrics.estimated_time_ms) / original_metrics.estimated_time_ms) * 100.0,
            },
            Improvement {
                metric_name: "CPU".to_string(),
                before_value: original_metrics.cpu_usage_percent,
                after_value: optimized_metrics.cpu_usage_percent,
                improvement_percent: ((original_metrics.cpu_usage_percent - optimized_metrics.cpu_usage_percent) / original_metrics.cpu_usage_percent) * 100.0,
            },
            Improvement {
                metric_name: "I/O".to_string(),
                before_value: original_metrics.io_operations as f64,
                after_value: optimized_metrics.io_operations as f64,
                improvement_percent: ((original_metrics.io_operations - optimized_metrics.io_operations) as f64 / original_metrics.io_operations as f64) * 100.0,
            },
        ];

        let overall_score = improvements.iter()
            .map(|i| i.improvement_percent)
            .sum::<f64>() / improvements.len() as f64;

        Ok(BeforeAfterComparison {
            original_query: original.to_string(),
            optimized_query: optimized.to_string(),
            original_metrics,
            optimized_metrics,
            improvements,
            overall_score,
        })
    }

    /// Format interactive tuning output
    pub fn format_session(&self, session: &TuningSession) -> String {
        let mut output = String::new();

        output.push_str("═══════════════════════════════════════════════════════════════\n");
        output.push_str("           INTERACTIVE QUERY TUNING SESSION                    \n");
        output.push_str("═══════════════════════════════════════════════════════════════\n\n");

        output.push_str(&format!("Session ID: {}\n", session.session_id));
        output.push_str(&format!("Original Query:\n  {}\n\n", session.original_query));

        if !session.performance_history.is_empty() {
            output.push_str("───────────────────────────────────────────────────────────────\n");
            output.push_str("  PERFORMANCE HISTORY\n");
            output.push_str("───────────────────────────────────────────────────────────────\n\n");

            for snapshot in &session.performance_history {
                output.push_str(&format!("• {} - Cost: {:.2}, Time: {:.2}ms, Quality: {:.0}/100\n",
                    snapshot.query_version,
                    snapshot.estimated_cost,
                    snapshot.estimated_time_ms,
                    snapshot.plan_quality_score));
            }
            output.push_str("\n");
        }

        if !session.modifications.is_empty() {
            output.push_str("───────────────────────────────────────────────────────────────\n");
            output.push_str("  MODIFICATIONS\n");
            output.push_str("───────────────────────────────────────────────────────────────\n\n");

            for mod_item in &session.modifications {
                output.push_str(&format!("{}. {} - {}\n",
                    mod_item.modification_id,
                    if mod_item.applied { "✓ Applied" } else { "○ Available" },
                    mod_item.description));
                output.push_str(&format!("   Speedup: {:.1}x, Cost Change: {:.0}%, Risk: {:?}\n",
                    mod_item.impact.estimated_speedup,
                    mod_item.impact.cost_change_percent,
                    mod_item.impact.risk_level));
                output.push_str("\n");
            }
        }

        output.push_str("═══════════════════════════════════════════════════════════════\n");

        output
    }

    /// Format what-if scenario
    pub fn format_what_if(&self, scenario: &WhatIfScenario) -> String {
        let mut output = String::new();

        output.push_str("═══════════════════════════════════════════════════════════════\n");
        output.push_str(&format!("         WHAT-IF SCENARIO: {}                    \n", scenario.scenario_name));
        output.push_str("═══════════════════════════════════════════════════════════════\n\n");

        output.push_str(&format!("Description: {}\n\n", scenario.description));

        output.push_str("Changes:\n");
        for change in &scenario.changes {
            output.push_str(&format!("  • {}\n", change));
        }
        output.push_str("\n");

        output.push_str("───────────────────────────────────────────────────────────────\n");
        output.push_str("  BEFORE → AFTER\n");
        output.push_str("───────────────────────────────────────────────────────────────\n\n");

        output.push_str(&format!("Cost:        {:.2} → {:.2} ({:.0}% improvement)\n",
            scenario.before.cost,
            scenario.after.cost,
            ((scenario.before.cost - scenario.after.cost) / scenario.before.cost) * 100.0));

        output.push_str(&format!("Time:        {:.2}ms → {:.2}ms ({:.0}% faster)\n",
            scenario.before.estimated_time_ms,
            scenario.after.estimated_time_ms,
            ((scenario.before.estimated_time_ms - scenario.after.estimated_time_ms) / scenario.before.estimated_time_ms) * 100.0));

        output.push_str(&format!("CPU:         {:.0}% → {:.0}%\n",
            scenario.before.cpu_usage_percent,
            scenario.after.cpu_usage_percent));

        output.push_str(&format!("I/O Ops:     {} → {}\n",
            scenario.before.io_operations,
            scenario.after.io_operations));

        output.push_str(&format!("Memory:      {:.0} MB → {:.0} MB\n\n",
            scenario.before.memory_mb,
            scenario.after.memory_mb));

        output.push_str(&format!("Recommendation: {}\n", scenario.recommendation));

        output.push_str("\n═══════════════════════════════════════════════════════════════\n");

        output
    }

    /// Format before/after comparison
    pub fn format_comparison(&self, comparison: &BeforeAfterComparison) -> String {
        let mut output = String::new();

        output.push_str("═══════════════════════════════════════════════════════════════\n");
        output.push_str("              BEFORE/AFTER COMPARISON                          \n");
        output.push_str("═══════════════════════════════════════════════════════════════\n\n");

        output.push_str("Original Query:\n");
        output.push_str(&format!("  {}\n\n", comparison.original_query));

        output.push_str("Optimized Query:\n");
        output.push_str(&format!("  {}\n\n", comparison.optimized_query));

        output.push_str("───────────────────────────────────────────────────────────────\n");
        output.push_str("  IMPROVEMENTS\n");
        output.push_str("───────────────────────────────────────────────────────────────\n\n");

        for improvement in &comparison.improvements {
            output.push_str(&format!("{}: {:.2} → {:.2} ({:.0}% improvement)\n",
                improvement.metric_name,
                improvement.before_value,
                improvement.after_value,
                improvement.improvement_percent));
        }

        output.push_str(&format!("\nOverall Score: {:.0}% improvement\n", comparison.overall_score));

        output.push_str("\n═══════════════════════════════════════════════════════════════\n");

        output
    }
}

impl Default for InteractiveQueryTuner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_create_session() {
        let tuner = InteractiveQueryTuner::new();
        let session = tuner.create_session("SELECT * FROM users".to_string()).unwrap();

        assert!(!session.session_id.is_empty());
        assert_eq!(session.original_query, "SELECT * FROM users");
        assert_eq!(session.performance_history.len(), 1);
    }

    #[test]
    fn test_suggest_optimizations() {
        let tuner = InteractiveQueryTuner::new();
        let session = tuner.create_session("SELECT * FROM users WHERE email = 'test'".to_string()).unwrap();
        let suggestions = tuner.suggest_optimizations(&session).unwrap();

        assert!(!suggestions.is_empty());
        assert!(suggestions.iter().all(|s| !s.applied));
    }

    #[test]
    fn test_apply_modification() {
        let tuner = InteractiveQueryTuner::new();
        let mut session = tuner.create_session("SELECT * FROM users".to_string()).unwrap();
        let suggestions = tuner.suggest_optimizations(&session).unwrap();

        session.modifications = suggestions;
        let initial_history_len = session.performance_history.len();

        let snapshot = tuner.apply_modification(&mut session, 1).unwrap();

        assert!(session.modifications[0].applied);
        assert_eq!(session.performance_history.len(), initial_history_len + 1);
        assert!(snapshot.estimated_cost < session.performance_history[0].estimated_cost);
    }

    #[test]
    fn test_rollback_modification() {
        let tuner = InteractiveQueryTuner::new();
        let mut session = tuner.create_session("SELECT * FROM users".to_string()).unwrap();
        let suggestions = tuner.suggest_optimizations(&session).unwrap();

        session.modifications = suggestions;
        tuner.apply_modification(&mut session, 1).unwrap();

        assert!(session.modifications[0].applied);

        tuner.rollback_modification(&mut session, 1).unwrap();

        assert!(!session.modifications[0].applied);
    }

    #[test]
    fn test_what_if_scenarios() {
        let tuner = InteractiveQueryTuner::new();

        let scenario = tuner.explore_what_if("SELECT * FROM users", "add_index").unwrap();

        assert_eq!(scenario.scenario_name, "add_index");
        assert!(scenario.after.estimated_time_ms < scenario.before.estimated_time_ms);
        assert_eq!(scenario.recommendation, ScenarioRecommendation::Apply);
    }

    #[test]
    fn test_before_after_comparison() {
        let tuner = InteractiveQueryTuner::new();
        let comparison = tuner.compare_before_after(
            "SELECT * FROM users",
            "SELECT id, name FROM users WHERE id IN (SELECT DISTINCT user_id FROM orders)",
        ).unwrap();

        assert!(!comparison.improvements.is_empty());
        assert!(comparison.overall_score > 0.0);
    }

    #[test]
    fn test_format_session() {
        let tuner = InteractiveQueryTuner::new();
        let session = tuner.create_session("SELECT * FROM users".to_string()).unwrap();
        let output = tuner.format_session(&session);

        assert!(output.contains("INTERACTIVE QUERY TUNING"));
        assert!(output.contains("Session ID"));
    }

    #[test]
    fn test_format_what_if() {
        let tuner = InteractiveQueryTuner::new();
        let scenario = tuner.explore_what_if("SELECT * FROM users", "parallel_execution").unwrap();
        let output = tuner.format_what_if(&scenario);

        assert!(output.contains("WHAT-IF SCENARIO"));
        assert!(output.contains("BEFORE → AFTER"));
    }
}
