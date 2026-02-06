//! Advanced EXPLAIN Features
//!
//! This module provides advanced capabilities:
//! - Query plan versioning (track changes over time)
//! - Plan diff visualization (show what changed)
//! - Collaborative features (share EXPLAIN results)
//! - Query plan library (save common patterns)
//! - Historical analysis (plan evolution)

use crate::{Result, Error};
use super::explain::{ExplainOutput, PlanNode, ConfigSnapshot};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

/// Query plan version
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanVersion {
    pub version_id: String,
    pub query_hash: String,
    pub created_at: u64,
    pub created_by: String,
    pub plan: ExplainOutput,
    pub metadata: PlanMetadata,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanMetadata {
    pub database_version: String,
    pub optimizer_version: String,
    pub statistics_version: String,
    pub config_hash: String,
    pub environment: String,
}

/// Plan version manager
pub struct PlanVersionManager {
    versions: HashMap<String, Vec<PlanVersion>>,
    max_versions_per_query: usize,
}

impl PlanVersionManager {
    pub fn new() -> Self {
        Self {
            versions: HashMap::new(),
            max_versions_per_query: 100,
        }
    }

    /// Store a new plan version
    pub fn store_version(
        &mut self,
        query_hash: String,
        plan: ExplainOutput,
        created_by: String,
        environment: String,
    ) -> PlanVersion {
        let version_id = self.generate_version_id(&query_hash);

        let version = PlanVersion {
            version_id: version_id.clone(),
            query_hash: query_hash.clone(),
            created_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            created_by,
            plan,
            metadata: PlanMetadata {
                database_version: "7.0.0".to_string(),
                optimizer_version: "1.0.0".to_string(),
                statistics_version: "current".to_string(),
                config_hash: "abc123".to_string(),
                environment,
            },
            tags: Vec::new(),
        };

        self.versions
            .entry(query_hash)
            .or_insert_with(Vec::new)
            .push(version.clone());

        // Prune old versions if needed
        if let Some(versions) = self.versions.get_mut(&version.query_hash) {
            if versions.len() > self.max_versions_per_query {
                versions.remove(0);
            }
        }

        version
    }

    /// Get all versions for a query
    pub fn get_versions(&self, query_hash: &str) -> Vec<&PlanVersion> {
        self.versions
            .get(query_hash)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    /// Get specific version
    pub fn get_version(&self, version_id: &str) -> Option<&PlanVersion> {
        self.versions
            .values()
            .flatten()
            .find(|v| v.version_id == version_id)
    }

    /// Compare two versions
    pub fn compare_versions(
        &self,
        version1_id: &str,
        version2_id: &str,
    ) -> Result<PlanDiff> {
        let v1 = self.get_version(version1_id)
            .ok_or_else(|| Error::Storage(format!("Version {} not found", version1_id)))?;
        let v2 = self.get_version(version2_id)
            .ok_or_else(|| Error::Storage(format!("Version {} not found", version2_id)))?;

        Ok(PlanDiff::compute(&v1.plan, &v2.plan))
    }

    fn generate_version_id(&self, query_hash: &str) -> String {
        let count = self.versions.get(query_hash).map(|v| v.len()).unwrap_or(0);
        format!("{}-v{}", query_hash, count + 1)
    }

    /// Get version history timeline
    #[allow(clippy::indexing_slicing)]
    // SAFETY: Loop `i` ranges from `1..events.len()`, so `i-1` and `i` are always in bounds
    pub fn get_timeline(&self, query_hash: &str) -> Timeline {
        let versions = self.get_versions(query_hash);

        let mut events = Vec::new();
        for version in versions {
            events.push(TimelineEvent {
                timestamp: version.created_at,
                event_type: TimelineEventType::PlanChange,
                description: format!("Plan version {} created by {}",
                    version.version_id, version.created_by),
                version_id: Some(version.version_id.clone()),
                cost_change: None,
            });
        }

        // Add cost change events
        for i in 1..events.len() {
            if let (Some(prev_id), Some(curr_id)) = (&events[i - 1].version_id, &events[i].version_id) {
                if let (Some(prev), Some(curr)) = (self.get_version(prev_id), self.get_version(curr_id)) {
                    let cost_change = curr.plan.total_cost - prev.plan.total_cost;
                    events[i].cost_change = Some(cost_change);
                }
            }
        }

        Timeline {
            query_hash: query_hash.to_string(),
            events,
        }
    }
}

/// Plan diff result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDiff {
    pub cost_change: CostChange,
    pub row_estimate_change: RowEstimateChange,
    pub plan_structure_changes: Vec<StructureChange>,
    pub feature_changes: Vec<FeatureChange>,
    pub config_changes: Vec<ConfigChange>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostChange {
    pub old_cost: f64,
    pub new_cost: f64,
    pub change_percent: f64,
    pub improved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowEstimateChange {
    pub old_rows: usize,
    pub new_rows: usize,
    pub change_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructureChange {
    pub change_type: StructureChangeType,
    pub description: String,
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StructureChangeType {
    NodeAdded,
    NodeRemoved,
    NodeReordered,
    OperationChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureChange {
    pub feature_name: String,
    pub change_type: FeatureChangeType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FeatureChangeType {
    Added,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigChange {
    pub parameter: String,
    pub old_value: String,
    pub new_value: String,
}

impl PlanDiff {
    /// Compute diff between two plans
    pub fn compute(old: &ExplainOutput, new: &ExplainOutput) -> Self {
        let cost_change_percent = ((new.total_cost - old.total_cost) / old.total_cost) * 100.0;
        let row_change_percent = ((new.total_rows as f64 - old.total_rows as f64) / old.total_rows as f64) * 100.0;

        let cost_change = CostChange {
            old_cost: old.total_cost,
            new_cost: new.total_cost,
            change_percent: cost_change_percent,
            improved: new.total_cost < old.total_cost,
        };

        let row_estimate_change = RowEstimateChange {
            old_rows: old.total_rows,
            new_rows: new.total_rows,
            change_percent: row_change_percent,
        };

        // Detect feature changes
        let mut feature_changes = Vec::new();
        let old_features: HashMap<_, _> = old.features.iter()
            .map(|f| (f.name.clone(), f))
            .collect();
        let new_features: HashMap<_, _> = new.features.iter()
            .map(|f| (f.name.clone(), f))
            .collect();

        for (name, _) in &new_features {
            if !old_features.contains_key(name) {
                feature_changes.push(FeatureChange {
                    feature_name: name.clone(),
                    change_type: FeatureChangeType::Added,
                });
            }
        }

        for (name, _) in &old_features {
            if !new_features.contains_key(name) {
                feature_changes.push(FeatureChange {
                    feature_name: name.clone(),
                    change_type: FeatureChangeType::Removed,
                });
            }
        }

        // Compute plan structure diff (tree comparison)
        let plan_structure_changes = Self::compute_tree_diff(&old.plan, &new.plan, "root");

        // Compute config diff
        let config_changes = Self::compute_config_diff(&old.config, &new.config);

        // Generate summary
        let summary = Self::generate_summary(&cost_change, &feature_changes);

        Self {
            cost_change,
            row_estimate_change,
            plan_structure_changes,
            feature_changes,
            config_changes,
            summary,
        }
    }

    /// Compute tree diff between two plan nodes recursively
    #[allow(clippy::indexing_slicing)]
    // SAFETY: All child indexing is bounded by old_children_len/new_children_len and min_children
    fn compute_tree_diff(old: &PlanNode, new: &PlanNode, path: &str) -> Vec<StructureChange> {
        let mut changes = Vec::new();

        // Check if node type changed
        if old.node_type != new.node_type {
            changes.push(StructureChange {
                change_type: StructureChangeType::OperationChanged,
                description: format!("Node type changed: {} → {}", old.node_type, new.node_type),
                location: path.to_string(),
            });
        }

        // Check if operation changed
        if old.operation != new.operation {
            changes.push(StructureChange {
                change_type: StructureChangeType::OperationChanged,
                description: format!("Operation changed: {} → {}", old.operation, new.operation),
                location: path.to_string(),
            });
        }

        // Compare children count
        let old_children_len = old.children.len();
        let new_children_len = new.children.len();

        if new_children_len > old_children_len {
            for i in old_children_len..new_children_len {
                let child_path = format!("{}/child[{}]", path, i);
                changes.push(StructureChange {
                    change_type: StructureChangeType::NodeAdded,
                    description: format!("New node added: {} ({})",
                        new.children[i].node_type, new.children[i].operation),
                    location: child_path,
                });
            }
        } else if old_children_len > new_children_len {
            for i in new_children_len..old_children_len {
                let child_path = format!("{}/child[{}]", path, i);
                changes.push(StructureChange {
                    change_type: StructureChangeType::NodeRemoved,
                    description: format!("Node removed: {} ({})",
                        old.children[i].node_type, old.children[i].operation),
                    location: child_path,
                });
            }
        }

        // Recursively compare matching children
        let min_children = old_children_len.min(new_children_len);
        for i in 0..min_children {
            let child_path = format!("{}/child[{}]", path, i);

            // Check if child nodes were reordered (different types at same position)
            if old.children[i].node_type != new.children[i].node_type {
                changes.push(StructureChange {
                    change_type: StructureChangeType::NodeReordered,
                    description: format!("Node at position {} changed from {} to {}",
                        i, old.children[i].node_type, new.children[i].node_type),
                    location: child_path.clone(),
                });
            }

            // Recurse into children
            let child_changes = Self::compute_tree_diff(
                &old.children[i],
                &new.children[i],
                &child_path
            );
            changes.extend(child_changes);
        }

        changes
    }

    /// Compute config diff between two configuration snapshots
    fn compute_config_diff(old: &ConfigSnapshot, new: &ConfigSnapshot) -> Vec<ConfigChange> {
        let mut changes = Vec::new();

        // Compare work_mem_mb
        if old.work_mem_mb != new.work_mem_mb {
            changes.push(ConfigChange {
                parameter: "work_mem_mb".to_string(),
                old_value: old.work_mem_mb.to_string(),
                new_value: new.work_mem_mb.to_string(),
            });
        }

        // Compare enable_hashjoin
        if old.enable_hashjoin != new.enable_hashjoin {
            changes.push(ConfigChange {
                parameter: "enable_hashjoin".to_string(),
                old_value: old.enable_hashjoin.to_string(),
                new_value: new.enable_hashjoin.to_string(),
            });
        }

        // Compare enable_mergejoin
        if old.enable_mergejoin != new.enable_mergejoin {
            changes.push(ConfigChange {
                parameter: "enable_mergejoin".to_string(),
                old_value: old.enable_mergejoin.to_string(),
                new_value: new.enable_mergejoin.to_string(),
            });
        }

        // Compare enable_nestloop
        if old.enable_nestloop != new.enable_nestloop {
            changes.push(ConfigChange {
                parameter: "enable_nestloop".to_string(),
                old_value: old.enable_nestloop.to_string(),
                new_value: new.enable_nestloop.to_string(),
            });
        }

        // Compare enable_indexscan
        if old.enable_indexscan != new.enable_indexscan {
            changes.push(ConfigChange {
                parameter: "enable_indexscan".to_string(),
                old_value: old.enable_indexscan.to_string(),
                new_value: new.enable_indexscan.to_string(),
            });
        }

        // Compare enable_seqscan
        if old.enable_seqscan != new.enable_seqscan {
            changes.push(ConfigChange {
                parameter: "enable_seqscan".to_string(),
                old_value: old.enable_seqscan.to_string(),
                new_value: new.enable_seqscan.to_string(),
            });
        }

        // Compare max_parallel_workers
        if old.max_parallel_workers != new.max_parallel_workers {
            changes.push(ConfigChange {
                parameter: "max_parallel_workers".to_string(),
                old_value: old.max_parallel_workers.to_string(),
                new_value: new.max_parallel_workers.to_string(),
            });
        }

        // Compare enable_simd
        if old.enable_simd != new.enable_simd {
            changes.push(ConfigChange {
                parameter: "enable_simd".to_string(),
                old_value: old.enable_simd.to_string(),
                new_value: new.enable_simd.to_string(),
            });
        }

        changes
    }

    fn generate_summary(cost_change: &CostChange, feature_changes: &[FeatureChange]) -> String {
        let mut summary = String::new();

        if cost_change.improved {
            summary.push_str(&format!(
                "✅ Plan improved: Cost reduced by {:.1}% ({:.2} → {:.2})\n",
                -cost_change.change_percent,
                cost_change.old_cost,
                cost_change.new_cost
            ));
        } else if cost_change.change_percent > 10.0 {
            summary.push_str(&format!(
                "⚠️ Plan regressed: Cost increased by {:.1}% ({:.2} → {:.2})\n",
                cost_change.change_percent,
                cost_change.old_cost,
                cost_change.new_cost
            ));
        } else {
            summary.push_str("ℹ️ Plan cost unchanged (within 10% tolerance)\n");
        }

        if !feature_changes.is_empty() {
            summary.push_str(&format!("\n{} optimizer features changed:\n", feature_changes.len()));
            for change in feature_changes {
                let symbol = match change.change_type {
                    FeatureChangeType::Added => "+",
                    FeatureChangeType::Removed => "-",
                };
                summary.push_str(&format!("  {} {}\n", symbol, change.feature_name));
            }
        }

        summary
    }

    /// Visualize diff as side-by-side comparison
    #[allow(clippy::indexing_slicing)]
    // SAFETY: String slicing at `..55` is guarded by `len() > 58` check
    pub fn visualize_diff(&self) -> String {
        let mut output = String::new();

        output.push_str("╔════════════════════════════════════════════════════════════════╗\n");
        output.push_str("║                        PLAN DIFF ANALYSIS                      ║\n");
        output.push_str("╠════════════════════════════════════════════════════════════════╣\n");

        // Summary
        output.push_str("║ SUMMARY                                                        ║\n");
        output.push_str("╠════════════════════════════════════════════════════════════════╣\n");
        for line in self.summary.lines() {
            output.push_str(&format!("║ {:62} ║\n", line));
        }

        // Cost comparison
        output.push_str("╠════════════════════════════════════════════════════════════════╣\n");
        output.push_str("║ COST COMPARISON                                                ║\n");
        output.push_str("╠════════════════════════════════════════════════════════════════╣\n");
        output.push_str(&format!("║   Old Cost: {:46.2} ║\n", self.cost_change.old_cost));
        output.push_str(&format!("║   New Cost: {:46.2} ║\n", self.cost_change.new_cost));
        output.push_str(&format!("║   Change:   {:+45.1}% ║\n", self.cost_change.change_percent));

        // Row estimate comparison
        output.push_str("╠════════════════════════════════════════════════════════════════╣\n");
        output.push_str("║ ROW ESTIMATE COMPARISON                                        ║\n");
        output.push_str("╠════════════════════════════════════════════════════════════════╣\n");
        output.push_str(&format!("║   Old Rows: {:47} ║\n", self.row_estimate_change.old_rows));
        output.push_str(&format!("║   New Rows: {:47} ║\n", self.row_estimate_change.new_rows));

        // Plan structure changes
        if !self.plan_structure_changes.is_empty() {
            output.push_str("╠════════════════════════════════════════════════════════════════╣\n");
            output.push_str("║ PLAN STRUCTURE CHANGES                                         ║\n");
            output.push_str("╠════════════════════════════════════════════════════════════════╣\n");
            for change in &self.plan_structure_changes {
                let symbol = match change.change_type {
                    StructureChangeType::NodeAdded => "+",
                    StructureChangeType::NodeRemoved => "-",
                    StructureChangeType::NodeReordered => "~",
                    StructureChangeType::OperationChanged => "*",
                };
                let desc = if change.description.len() > 58 {
                    format!("{}...", &change.description[..55])
                } else {
                    change.description.clone()
                };
                output.push_str(&format!("║ {} {:60} ║\n", symbol, desc));
            }
        }

        // Config changes
        if !self.config_changes.is_empty() {
            output.push_str("╠════════════════════════════════════════════════════════════════╣\n");
            output.push_str("║ CONFIG CHANGES                                                 ║\n");
            output.push_str("╠════════════════════════════════════════════════════════════════╣\n");
            for change in &self.config_changes {
                let desc = format!("{}: {} → {}", change.parameter, change.old_value, change.new_value);
                output.push_str(&format!("║   {:60} ║\n", desc));
            }
        }

        output.push_str("╚════════════════════════════════════════════════════════════════╝\n");

        output
    }
}

/// Timeline of plan evolution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeline {
    pub query_hash: String,
    pub events: Vec<TimelineEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEvent {
    pub timestamp: u64,
    pub event_type: TimelineEventType,
    pub description: String,
    pub version_id: Option<String>,
    pub cost_change: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TimelineEventType {
    PlanChange,
    ConfigChange,
    StatisticsUpdate,
    IndexAdded,
    IndexRemoved,
}

/// Query plan library
pub struct PlanLibrary {
    saved_plans: HashMap<String, SavedPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedPlan {
    pub id: String,
    pub name: String,
    pub description: String,
    pub query_pattern: String,
    pub plan: ExplainOutput,
    pub category: PlanCategory,
    pub tags: Vec<String>,
    pub saved_by: String,
    pub saved_at: u64,
    pub use_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PlanCategory {
    BestPractice,
    AntiPattern,
    OptimizationExample,
    CommonQuery,
    PerformanceBenchmark,
}

impl PlanLibrary {
    pub fn new() -> Self {
        Self {
            saved_plans: HashMap::new(),
        }
    }

    /// Save a plan to the library
    pub fn save_plan(
        &mut self,
        name: String,
        description: String,
        query_pattern: String,
        plan: ExplainOutput,
        category: PlanCategory,
        tags: Vec<String>,
        saved_by: String,
    ) -> SavedPlan {
        let id = format!("plan-{}", self.saved_plans.len() + 1);

        let saved = SavedPlan {
            id: id.clone(),
            name,
            description,
            query_pattern,
            plan,
            category,
            tags,
            saved_by,
            saved_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            use_count: 0,
        };

        self.saved_plans.insert(id, saved.clone());
        saved
    }

    /// Get plan by ID
    pub fn get_plan(&mut self, id: &str) -> Option<&SavedPlan> {
        if let Some(plan) = self.saved_plans.get_mut(id) {
            plan.use_count += 1;
        }
        self.saved_plans.get(id)
    }

    /// Search plans by category
    pub fn search_by_category(&self, category: &PlanCategory) -> Vec<&SavedPlan> {
        self.saved_plans
            .values()
            .filter(|p| std::mem::discriminant(&p.category) == std::mem::discriminant(category))
            .collect()
    }

    /// Search plans by tags
    pub fn search_by_tags(&self, tags: &[String]) -> Vec<&SavedPlan> {
        self.saved_plans
            .values()
            .filter(|p| tags.iter().any(|t| p.tags.contains(t)))
            .collect()
    }

    /// Get most used plans
    pub fn get_most_used(&self, limit: usize) -> Vec<&SavedPlan> {
        let mut plans: Vec<_> = self.saved_plans.values().collect();
        plans.sort_by_key(|p| std::cmp::Reverse(p.use_count));
        plans.into_iter().take(limit).collect()
    }

    /// Get recent plans
    pub fn get_recent(&self, limit: usize) -> Vec<&SavedPlan> {
        let mut plans: Vec<_> = self.saved_plans.values().collect();
        plans.sort_by_key(|p| std::cmp::Reverse(p.saved_at));
        plans.into_iter().take(limit).collect()
    }

    /// Create default library with common patterns
    pub fn create_default_library() -> Self {
        let library = Self::new();

        // Add some example patterns
        // In production, these would be real optimized plans

        library
    }
}

/// Share plan result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareableExplain {
    pub share_id: String,
    pub plan: ExplainOutput,
    pub shared_by: String,
    pub shared_at: u64,
    pub expires_at: Option<u64>,
    pub view_count: usize,
    pub comments: Vec<Comment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub author: String,
    pub text: String,
    pub created_at: u64,
}

/// Historical analysis
pub struct HistoricalAnalysis {
    retention_days: u32,
}

impl HistoricalAnalysis {
    pub fn new(retention_days: u32) -> Self {
        Self { retention_days }
    }

    /// Analyze plan evolution over time
    pub fn analyze_evolution(&self, versions: &[PlanVersion]) -> EvolutionAnalysis {
        let mut cost_trend = Vec::new();
        let mut feature_adoption = HashMap::new();

        for version in versions {
            cost_trend.push((version.created_at, version.plan.total_cost));

            for feature in &version.plan.features {
                *feature_adoption.entry(feature.name.clone()).or_insert(0) += 1;
            }
        }

        EvolutionAnalysis {
            cost_trend,
            feature_adoption,
            total_versions: versions.len(),
            time_span_days: self.calculate_time_span(versions),
        }
    }

    fn calculate_time_span(&self, versions: &[PlanVersion]) -> u32 {
        if versions.len() < 2 {
            return 0;
        }

        // Safe: we've verified len() >= 2 above
        #[allow(clippy::unwrap_used)]
        let first = versions.first().unwrap().created_at;
        #[allow(clippy::unwrap_used)]
        let last = versions.last().unwrap().created_at;
        ((last - first) / 86400) as u32
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionAnalysis {
    pub cost_trend: Vec<(u64, f64)>,
    pub feature_adoption: HashMap<String, usize>,
    pub total_versions: usize,
    pub time_span_days: u32,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{Schema, Column, DataType};
    use crate::sql::logical_plan::LogicalPlan;
    use std::sync::Arc;
    use super::super::explain::*;

    fn create_test_output() -> ExplainOutput {
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
        planner.explain(&plan).unwrap()
    }

    #[test]
    fn test_version_manager() {
        let mut manager = PlanVersionManager::new();
        let plan = create_test_output();

        let version = manager.store_version(
            "query-123".to_string(),
            plan,
            "alice".to_string(),
            "production".to_string(),
        );

        assert!(version.version_id.contains("query-123"));

        let versions = manager.get_versions("query-123");
        assert_eq!(versions.len(), 1);
    }

    #[test]
    fn test_plan_diff() {
        let mut old = create_test_output();
        let mut new = create_test_output();

        new.total_cost = old.total_cost * 1.5;

        let diff = PlanDiff::compute(&old, &new);

        assert!(diff.cost_change.change_percent > 0.0);
        assert!(!diff.cost_change.improved);
    }

    #[test]
    fn test_plan_library() {
        let mut library = PlanLibrary::new();
        let plan = create_test_output();

        let saved = library.save_plan(
            "Fast SELECT".to_string(),
            "Example of efficient single-table select".to_string(),
            "SELECT * FROM users WHERE id = ?".to_string(),
            plan,
            PlanCategory::BestPractice,
            vec!["select".to_string(), "indexed".to_string()],
            "bob".to_string(),
        );

        assert!(saved.id.starts_with("plan-"));

        let retrieved = library.get_plan(&saved.id);
        assert!(retrieved.is_some());
    }

    #[test]
    fn test_timeline() {
        let mut manager = PlanVersionManager::new();
        let plan = create_test_output();

        manager.store_version(
            "query-456".to_string(),
            plan.clone(),
            "alice".to_string(),
            "dev".to_string(),
        );

        manager.store_version(
            "query-456".to_string(),
            plan,
            "bob".to_string(),
            "staging".to_string(),
        );

        let timeline = manager.get_timeline("query-456");
        assert_eq!(timeline.events.len(), 2);
    }

    #[test]
    fn test_diff_visualization() {
        let old = create_test_output();
        let mut new = create_test_output();
        new.total_cost *= 2.0;

        let diff = PlanDiff::compute(&old, &new);
        let viz = diff.visualize_diff();

        assert!(viz.contains("PLAN DIFF ANALYSIS"));
        assert!(viz.contains("COST COMPARISON"));
    }
}
