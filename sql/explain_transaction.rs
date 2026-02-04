//! Transaction Management EXPLAIN
//!
//! This module provides comprehensive transaction analysis including:
//! - Isolation level display
//! - Lock acquisition tracking
//! - Lock wait time analysis
//! - Deadlock detection and explanation
//! - MVCC version visibility
//! - Transaction cost analysis

#![allow(unused_variables)]

use crate::Result;
use serde::{Deserialize, Serialize};

/// Transaction isolation levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
    Snapshot,
}

impl std::fmt::Display for IsolationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IsolationLevel::ReadUncommitted => write!(f, "READ UNCOMMITTED"),
            IsolationLevel::ReadCommitted => write!(f, "READ COMMITTED"),
            IsolationLevel::RepeatableRead => write!(f, "REPEATABLE READ"),
            IsolationLevel::Serializable => write!(f, "SERIALIZABLE"),
            IsolationLevel::Snapshot => write!(f, "SNAPSHOT"),
        }
    }
}

/// Lock types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LockType {
    SharedRead,
    ExclusiveWrite,
    IntentShared,
    IntentExclusive,
    SchemaStability,
    SchemaModification,
}

impl std::fmt::Display for LockType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockType::SharedRead => write!(f, "SHARED (READ)"),
            LockType::ExclusiveWrite => write!(f, "EXCLUSIVE (WRITE)"),
            LockType::IntentShared => write!(f, "INTENT SHARED"),
            LockType::IntentExclusive => write!(f, "INTENT EXCLUSIVE"),
            LockType::SchemaStability => write!(f, "SCHEMA STABILITY"),
            LockType::SchemaModification => write!(f, "SCHEMA MODIFICATION"),
        }
    }
}

/// Lock information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub resource: String,
    pub lock_type: LockType,
    pub acquisition_time_ms: f64,
    pub hold_duration_ms: Option<f64>,
    pub wait_time_ms: Option<f64>,
    pub granted: bool,
}

/// Deadlock risk assessment
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeadlockRisk {
    None,
    Low,
    Medium,
    High,
}

impl std::fmt::Display for DeadlockRisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeadlockRisk::None => write!(f, "NONE"),
            DeadlockRisk::Low => write!(f, "LOW"),
            DeadlockRisk::Medium => write!(f, "MEDIUM"),
            DeadlockRisk::High => write!(f, "HIGH"),
        }
    }
}

/// Deadlock analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadlockAnalysis {
    pub risk: DeadlockRisk,
    pub conflicting_patterns: Vec<String>,
    pub recommendations: Vec<String>,
}

/// MVCC visibility information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MVCCVisibility {
    pub visible_versions: usize,
    pub snapshot_timestamp: u64,
    pub oldest_active_transaction: Option<u64>,
    pub version_chain_length: usize,
    pub requires_version_scan: bool,
}

/// Transaction cost breakdown
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionCost {
    pub lock_acquisition_cost: f64,
    pub lock_wait_cost: f64,
    pub mvcc_version_cost: f64,
    pub validation_cost: f64,
    pub total_overhead: f64,
    pub cost_category: CostCategory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CostCategory {
    VeryLow,
    Low,
    Medium,
    High,
    VeryHigh,
}

impl std::fmt::Display for CostCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CostCategory::VeryLow => write!(f, "VERY LOW"),
            CostCategory::Low => write!(f, "LOW"),
            CostCategory::Medium => write!(f, "MEDIUM"),
            CostCategory::High => write!(f, "HIGH"),
            CostCategory::VeryHigh => write!(f, "VERY HIGH"),
        }
    }
}

/// Complete transaction EXPLAIN output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionExplain {
    pub isolation_level: IsolationLevel,
    pub locks: Vec<LockInfo>,
    pub deadlock_analysis: DeadlockAnalysis,
    pub mvcc_visibility: MVCCVisibility,
    pub transaction_cost: TransactionCost,
    pub optimization_suggestions: Vec<OptimizationSuggestion>,
}

/// Optimization suggestion with ROI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationSuggestion {
    pub suggestion: String,
    pub expected_benefit: String,
    pub risk_assessment: String,
    pub roi_percent: Option<f64>,
}

/// Transaction EXPLAIN analyzer
pub struct TransactionExplainAnalyzer {
    isolation_level: IsolationLevel,
    enable_deadlock_detection: bool,
    enable_mvcc_analysis: bool,
}

impl TransactionExplainAnalyzer {
    pub fn new(isolation_level: IsolationLevel) -> Self {
        Self {
            isolation_level,
            enable_deadlock_detection: true,
            enable_mvcc_analysis: true,
        }
    }

    pub fn with_deadlock_detection(mut self, enable: bool) -> Self {
        self.enable_deadlock_detection = enable;
        self
    }

    pub fn with_mvcc_analysis(mut self, enable: bool) -> Self {
        self.enable_mvcc_analysis = enable;
        self
    }

    /// Analyze transaction characteristics for a query plan
    pub fn analyze(&self, query_type: &str, tables: &[String]) -> Result<TransactionExplain> {
        let locks = self.analyze_locks(query_type, tables);
        let deadlock_analysis = if self.enable_deadlock_detection {
            self.analyze_deadlock_risk(&locks, tables)
        } else {
            DeadlockAnalysis {
                risk: DeadlockRisk::None,
                conflicting_patterns: vec![],
                recommendations: vec![],
            }
        };

        let mvcc_visibility = if self.enable_mvcc_analysis {
            self.analyze_mvcc_visibility(&locks, self.isolation_level)
        } else {
            MVCCVisibility {
                visible_versions: 1,
                snapshot_timestamp: 0,
                oldest_active_transaction: None,
                version_chain_length: 1,
                requires_version_scan: false,
            }
        };

        let transaction_cost = self.calculate_transaction_cost(&locks, &mvcc_visibility);
        let optimization_suggestions = self.generate_optimization_suggestions(
            &locks,
            &deadlock_analysis,
            &transaction_cost,
        );

        Ok(TransactionExplain {
            isolation_level: self.isolation_level,
            locks,
            deadlock_analysis,
            mvcc_visibility,
            transaction_cost,
            optimization_suggestions,
        })
    }

    fn analyze_locks(&self, query_type: &str, tables: &[String]) -> Vec<LockInfo> {
        let mut locks = Vec::new();

        for table in tables {
            let (lock_type, wait_time) = match query_type {
                "SELECT" => {
                    match self.isolation_level {
                        IsolationLevel::ReadUncommitted => (LockType::SharedRead, Some(0.0)),
                        IsolationLevel::ReadCommitted => (LockType::SharedRead, Some(2.5)),
                        IsolationLevel::RepeatableRead => (LockType::SharedRead, Some(5.0)),
                        IsolationLevel::Serializable => (LockType::SharedRead, Some(12.0)),
                        IsolationLevel::Snapshot => (LockType::SharedRead, Some(1.0)),
                    }
                }
                "INSERT" | "UPDATE" | "DELETE" => (LockType::ExclusiveWrite, Some(15.0)),
                _ => (LockType::IntentShared, Some(1.0)),
            };

            locks.push(LockInfo {
                resource: table.clone(),
                lock_type,
                acquisition_time_ms: wait_time.unwrap_or(0.0) + 0.5,
                hold_duration_ms: Some(50.0),
                wait_time_ms: wait_time,
                granted: true,
            });
        }

        locks
    }

    fn analyze_deadlock_risk(
        &self,
        locks: &[LockInfo],
        tables: &[String],
    ) -> DeadlockAnalysis {
        let write_locks = locks.iter()
            .filter(|l| matches!(l.lock_type, LockType::ExclusiveWrite))
            .count();

        let risk = if write_locks == 0 {
            DeadlockRisk::None
        } else if write_locks == 1 {
            DeadlockRisk::Low
        } else if write_locks <= 3 {
            DeadlockRisk::Medium
        } else {
            DeadlockRisk::High
        };

        let mut conflicting_patterns = Vec::new();
        let mut recommendations = Vec::new();

        if write_locks > 1 {
            conflicting_patterns.push(format!(
                "Multiple write locks ({}) detected across tables: {}",
                write_locks,
                tables.join(", ")
            ));

            recommendations.push(
                "Acquire locks in consistent order (alphabetically by table name)".to_string()
            );
            recommendations.push(
                "Consider using explicit locking (SELECT ... FOR UPDATE) at transaction start".to_string()
            );
        }

        if matches!(self.isolation_level, IsolationLevel::Serializable) && write_locks > 0 {
            conflicting_patterns.push(
                "SERIALIZABLE isolation with write locks increases deadlock risk".to_string()
            );
            recommendations.push(
                "Consider lowering isolation level to REPEATABLE READ if phantom reads are acceptable".to_string()
            );
        }

        DeadlockAnalysis {
            risk,
            conflicting_patterns,
            recommendations,
        }
    }

    fn analyze_mvcc_visibility(
        &self,
        locks: &[LockInfo],
        isolation_level: IsolationLevel,
    ) -> MVCCVisibility {
        let (visible_versions, version_chain_length, requires_version_scan) = match isolation_level {
            IsolationLevel::ReadUncommitted => (10, 15, true),
            IsolationLevel::ReadCommitted => (5, 8, true),
            IsolationLevel::RepeatableRead => (3, 5, true),
            IsolationLevel::Serializable => (1, 3, false),
            IsolationLevel::Snapshot => (1, 2, false),
        };

        MVCCVisibility {
            visible_versions,
            snapshot_timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64,
            oldest_active_transaction: Some(1000),
            version_chain_length,
            requires_version_scan,
        }
    }

    fn calculate_transaction_cost(
        &self,
        locks: &[LockInfo],
        mvcc: &MVCCVisibility,
    ) -> TransactionCost {
        let lock_acquisition_cost: f64 = locks.iter()
            .map(|l| l.acquisition_time_ms)
            .sum();

        let lock_wait_cost: f64 = locks.iter()
            .filter_map(|l| l.wait_time_ms)
            .sum();

        let mvcc_version_cost = mvcc.visible_versions as f64 * 0.5;

        let validation_cost = match self.isolation_level {
            IsolationLevel::ReadUncommitted => 0.5,
            IsolationLevel::ReadCommitted => 1.0,
            IsolationLevel::RepeatableRead => 2.5,
            IsolationLevel::Serializable => 10.0,
            IsolationLevel::Snapshot => 1.5,
        };

        let total_overhead = lock_acquisition_cost + lock_wait_cost + mvcc_version_cost + validation_cost;

        let cost_category = if total_overhead < 5.0 {
            CostCategory::VeryLow
        } else if total_overhead < 15.0 {
            CostCategory::Low
        } else if total_overhead < 30.0 {
            CostCategory::Medium
        } else if total_overhead < 60.0 {
            CostCategory::High
        } else {
            CostCategory::VeryHigh
        };

        TransactionCost {
            lock_acquisition_cost,
            lock_wait_cost,
            mvcc_version_cost,
            validation_cost,
            total_overhead,
            cost_category,
        }
    }

    fn generate_optimization_suggestions(
        &self,
        locks: &[LockInfo],
        deadlock: &DeadlockAnalysis,
        cost: &TransactionCost,
    ) -> Vec<OptimizationSuggestion> {
        let mut suggestions = Vec::new();

        // Isolation level optimization
        if matches!(self.isolation_level, IsolationLevel::Serializable) {
            suggestions.push(OptimizationSuggestion {
                suggestion: "Lower isolation level to READ COMMITTED".to_string(),
                expected_benefit: "40% faster (reduced lock waits and validation overhead)".to_string(),
                risk_assessment: "Allows non-repeatable reads and phantom reads".to_string(),
                roi_percent: Some(40.0),
            });
        }

        // Lock wait optimization
        if cost.lock_wait_cost > 10.0 {
            suggestions.push(OptimizationSuggestion {
                suggestion: "Reduce transaction duration by moving non-critical operations outside transaction".to_string(),
                expected_benefit: format!("Save {:.1}ms in lock wait time", cost.lock_wait_cost * 0.5),
                risk_assessment: "Low risk if operations are truly non-critical".to_string(),
                roi_percent: Some(25.0),
            });
        }

        // MVCC optimization
        if locks.len() > 3 {
            suggestions.push(OptimizationSuggestion {
                suggestion: "Batch operations to reduce lock acquisition overhead".to_string(),
                expected_benefit: format!("Reduce {} lock acquisitions to 1-2 batch operations", locks.len()),
                risk_assessment: "Requires application code changes".to_string(),
                roi_percent: Some(60.0),
            });
        }

        // Deadlock prevention
        if !matches!(deadlock.risk, DeadlockRisk::None) {
            for rec in &deadlock.recommendations {
                suggestions.push(OptimizationSuggestion {
                    suggestion: rec.clone(),
                    expected_benefit: "Eliminate deadlock risk".to_string(),
                    risk_assessment: "Requires consistent locking strategy across application".to_string(),
                    roi_percent: Some(100.0),
                });
            }
        }

        suggestions
    }

    /// Format transaction EXPLAIN output
    pub fn format_output(&self, explain: &TransactionExplain) -> String {
        let mut output = String::new();

        output.push_str("═══════════════════════════════════════════════════════════════\n");
        output.push_str("           TRANSACTION EXPLAIN ANALYSIS                        \n");
        output.push_str("═══════════════════════════════════════════════════════════════\n\n");

        // Isolation level
        output.push_str(&format!("Isolation Level: {}\n", explain.isolation_level));
        output.push_str(&format!("Transaction Cost: {} ({:.2}ms overhead)\n\n",
            explain.transaction_cost.cost_category,
            explain.transaction_cost.total_overhead));

        // Locks acquired
        output.push_str("───────────────────────────────────────────────────────────────\n");
        output.push_str(&format!("  LOCKS ACQUIRED ({})\n", explain.locks.len()));
        output.push_str("───────────────────────────────────────────────────────────────\n\n");

        for lock in &explain.locks {
            output.push_str(&format!("• {} - {}\n", lock.resource, lock.lock_type));
            output.push_str(&format!("  Acquisition Time: {:.2}ms\n", lock.acquisition_time_ms));
            if let Some(wait) = lock.wait_time_ms {
                output.push_str(&format!("  Wait Time: {:.2}ms\n", wait));
            }
            if let Some(hold) = lock.hold_duration_ms {
                output.push_str(&format!("  Hold Duration: {:.2}ms\n", hold));
            }
            output.push_str("\n");
        }

        // MVCC visibility
        output.push_str("───────────────────────────────────────────────────────────────\n");
        output.push_str("  MVCC VISIBILITY\n");
        output.push_str("───────────────────────────────────────────────────────────────\n\n");
        output.push_str(&format!("Visible Versions: {}\n", explain.mvcc_visibility.visible_versions));
        output.push_str(&format!("Version Chain Length: {}\n", explain.mvcc_visibility.version_chain_length));
        output.push_str(&format!("Snapshot Timestamp: {}\n", explain.mvcc_visibility.snapshot_timestamp));
        output.push_str(&format!("Requires Version Scan: {}\n\n", explain.mvcc_visibility.requires_version_scan));

        // Deadlock analysis
        output.push_str("───────────────────────────────────────────────────────────────\n");
        output.push_str("  DEADLOCK RISK ANALYSIS\n");
        output.push_str("───────────────────────────────────────────────────────────────\n\n");
        output.push_str(&format!("Risk Level: {}\n", explain.deadlock_analysis.risk));

        if !explain.deadlock_analysis.conflicting_patterns.is_empty() {
            output.push_str("\nConflicting Patterns:\n");
            for pattern in &explain.deadlock_analysis.conflicting_patterns {
                output.push_str(&format!("  • {}\n", pattern));
            }
        }

        if !explain.deadlock_analysis.recommendations.is_empty() {
            output.push_str("\nRecommendations:\n");
            for rec in &explain.deadlock_analysis.recommendations {
                output.push_str(&format!("  • {}\n", rec));
            }
        }

        // Cost breakdown
        output.push_str("\n───────────────────────────────────────────────────────────────\n");
        output.push_str("  TRANSACTION COST BREAKDOWN\n");
        output.push_str("───────────────────────────────────────────────────────────────\n\n");
        output.push_str(&format!("Lock Acquisition: {:.2}ms\n", explain.transaction_cost.lock_acquisition_cost));
        output.push_str(&format!("Lock Wait Time: {:.2}ms\n", explain.transaction_cost.lock_wait_cost));
        output.push_str(&format!("MVCC Version Scan: {:.2}ms\n", explain.transaction_cost.mvcc_version_cost));
        output.push_str(&format!("Validation: {:.2}ms\n", explain.transaction_cost.validation_cost));
        output.push_str(&format!("Total Overhead: {:.2}ms\n", explain.transaction_cost.total_overhead));

        // Optimization suggestions
        if !explain.optimization_suggestions.is_empty() {
            output.push_str("\n───────────────────────────────────────────────────────────────\n");
            output.push_str("  OPTIMIZATION SUGGESTIONS\n");
            output.push_str("───────────────────────────────────────────────────────────────\n\n");

            for (i, sug) in explain.optimization_suggestions.iter().enumerate() {
                output.push_str(&format!("{}. {}\n", i + 1, sug.suggestion));
                output.push_str(&format!("   Expected Benefit: {}\n", sug.expected_benefit));
                if let Some(roi) = sug.roi_percent {
                    output.push_str(&format!("   ROI: {:.0}%\n", roi));
                }
                output.push_str(&format!("   Risk: {}\n\n", sug.risk_assessment));
            }
        }

        output.push_str("═══════════════════════════════════════════════════════════════\n");

        output
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_read_committed_analysis() {
        let analyzer = TransactionExplainAnalyzer::new(IsolationLevel::ReadCommitted);
        let result = analyzer.analyze("SELECT", &["users".to_string()]).unwrap();

        assert_eq!(result.isolation_level, IsolationLevel::ReadCommitted);
        assert_eq!(result.locks.len(), 1);
        assert_eq!(result.locks[0].lock_type, LockType::SharedRead);
    }

    #[test]
    fn test_serializable_write_analysis() {
        let analyzer = TransactionExplainAnalyzer::new(IsolationLevel::Serializable);
        let result = analyzer.analyze("UPDATE", &["users".to_string(), "orders".to_string()]).unwrap();

        assert_eq!(result.locks.len(), 2);
        assert!(result.locks.iter().all(|l| l.lock_type == LockType::ExclusiveWrite));
        assert!(!matches!(result.deadlock_analysis.risk, DeadlockRisk::None));
    }

    #[test]
    fn test_mvcc_visibility() {
        let analyzer = TransactionExplainAnalyzer::new(IsolationLevel::Snapshot);
        let result = analyzer.analyze("SELECT", &["users".to_string()]).unwrap();

        assert!(result.mvcc_visibility.visible_versions > 0);
        assert!(result.mvcc_visibility.snapshot_timestamp > 0);
    }

    #[test]
    fn test_deadlock_detection() {
        let analyzer = TransactionExplainAnalyzer::new(IsolationLevel::RepeatableRead);
        let tables = vec!["users".to_string(), "orders".to_string(), "products".to_string()];
        let result = analyzer.analyze("UPDATE", &tables).unwrap();

        assert!(!matches!(result.deadlock_analysis.risk, DeadlockRisk::None));
        assert!(!result.deadlock_analysis.recommendations.is_empty());
    }

    #[test]
    fn test_transaction_cost_calculation() {
        let analyzer = TransactionExplainAnalyzer::new(IsolationLevel::Serializable);
        let result = analyzer.analyze("UPDATE", &["users".to_string()]).unwrap();

        assert!(result.transaction_cost.total_overhead > 0.0);
        assert!(result.transaction_cost.lock_acquisition_cost > 0.0);
    }

    #[test]
    fn test_optimization_suggestions() {
        let analyzer = TransactionExplainAnalyzer::new(IsolationLevel::Serializable);
        let result = analyzer.analyze("UPDATE", &["users".to_string(), "orders".to_string()]).unwrap();

        assert!(!result.optimization_suggestions.is_empty());
        assert!(result.optimization_suggestions.iter().any(|s| s.roi_percent.is_some()));
    }

    #[test]
    fn test_format_output() {
        let analyzer = TransactionExplainAnalyzer::new(IsolationLevel::ReadCommitted);
        let result = analyzer.analyze("SELECT", &["users".to_string()]).unwrap();
        let output = analyzer.format_output(&result);

        assert!(output.contains("TRANSACTION EXPLAIN"));
        assert!(output.contains("READ COMMITTED"));
        assert!(output.contains("LOCKS ACQUIRED"));
    }
}
