//! A/B Testing Metrics - Analytics and Statistics
//!
//! Collects and reports metrics for A/B testing experiments.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Branch-level metrics
#[derive(Debug, Clone, Default)]
pub struct BranchMetrics {
    /// Branch name
    pub branch: String,
    /// Total queries
    pub query_count: u64,
    /// Successful queries
    pub success_count: u64,
    /// Failed queries
    pub failure_count: u64,
    /// Total latency (ms) - for computing average
    pub total_latency_ms: f64,
    /// Min latency (ms)
    pub min_latency_ms: f64,
    /// Max latency (ms)
    pub max_latency_ms: f64,
    /// Custom events: event_name -> (count, total_value)
    pub events: HashMap<String, (u64, f64)>,
    /// First query timestamp
    pub first_query: Option<chrono::DateTime<chrono::Utc>>,
    /// Last query timestamp
    pub last_query: Option<chrono::DateTime<chrono::Utc>>,
}

impl BranchMetrics {
    /// Create new branch metrics
    pub fn new(branch: impl Into<String>) -> Self {
        Self {
            branch: branch.into(),
            min_latency_ms: f64::MAX,
            max_latency_ms: f64::MIN,
            ..Default::default()
        }
    }

    /// Record a query
    pub fn record_query(&mut self, latency_ms: f64, success: bool) {
        self.query_count += 1;
        self.total_latency_ms += latency_ms;

        if success {
            self.success_count += 1;
        } else {
            self.failure_count += 1;
        }

        if latency_ms < self.min_latency_ms {
            self.min_latency_ms = latency_ms;
        }
        if latency_ms > self.max_latency_ms {
            self.max_latency_ms = latency_ms;
        }

        let now = chrono::Utc::now();
        if self.first_query.is_none() {
            self.first_query = Some(now);
        }
        self.last_query = Some(now);
    }

    /// Record a custom event
    pub fn record_event(&mut self, event_name: &str, value: f64) {
        let entry = self.events.entry(event_name.to_string()).or_default();
        entry.0 += 1;
        entry.1 += value;
    }

    /// Get average latency
    pub fn avg_latency_ms(&self) -> f64 {
        if self.query_count == 0 {
            0.0
        } else {
            self.total_latency_ms / self.query_count as f64
        }
    }

    /// Get success rate
    pub fn success_rate(&self) -> f64 {
        if self.query_count == 0 {
            0.0
        } else {
            self.success_count as f64 / self.query_count as f64
        }
    }

    /// Get error rate
    pub fn error_rate(&self) -> f64 {
        if self.query_count == 0 {
            0.0
        } else {
            self.failure_count as f64 / self.query_count as f64
        }
    }

    /// Get event average
    pub fn event_average(&self, event_name: &str) -> f64 {
        if let Some((count, total)) = self.events.get(event_name) {
            if *count == 0 {
                0.0
            } else {
                *total / *count as f64
            }
        } else {
            0.0
        }
    }
}

/// Experiment-level metrics
#[derive(Debug, Clone)]
pub struct ExperimentMetrics {
    /// Experiment name
    pub experiment: String,
    /// Per-branch metrics
    pub branches: HashMap<String, BranchMetrics>,
    /// Total queries across all branches
    pub total_queries: u64,
    /// Experiment start time (first query)
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Last activity
    pub last_activity: Option<chrono::DateTime<chrono::Utc>>,
}

impl ExperimentMetrics {
    /// Create new experiment metrics
    pub fn new(experiment: impl Into<String>) -> Self {
        Self {
            experiment: experiment.into(),
            branches: HashMap::new(),
            total_queries: 0,
            started_at: None,
            last_activity: None,
        }
    }

    /// Get or create branch metrics
    pub fn get_branch_mut(&mut self, branch: &str) -> &mut BranchMetrics {
        self.branches
            .entry(branch.to_string())
            .or_insert_with(|| BranchMetrics::new(branch))
    }

    /// Get branch metrics
    pub fn get_branch(&self, branch: &str) -> Option<&BranchMetrics> {
        self.branches.get(branch)
    }

    /// Record a query for a branch
    pub fn record_query(&mut self, branch: &str, latency_ms: f64, success: bool) {
        let metrics = self.get_branch_mut(branch);
        metrics.record_query(latency_ms, success);

        self.total_queries += 1;

        let now = chrono::Utc::now();
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }
        self.last_activity = Some(now);
    }

    /// Record a custom event for a branch
    pub fn record_event(&mut self, branch: &str, event_name: &str, value: f64) {
        let metrics = self.get_branch_mut(branch);
        metrics.record_event(event_name, value);
    }

    /// Compare branches (simple statistical comparison)
    pub fn compare(&self, branch_a: &str, branch_b: &str) -> Option<BranchComparison> {
        let a = self.get_branch(branch_a)?;
        let b = self.get_branch(branch_b)?;

        Some(BranchComparison {
            branch_a: branch_a.to_string(),
            branch_b: branch_b.to_string(),
            sample_size_a: a.query_count,
            sample_size_b: b.query_count,
            success_rate_a: a.success_rate(),
            success_rate_b: b.success_rate(),
            success_rate_diff: b.success_rate() - a.success_rate(),
            avg_latency_a: a.avg_latency_ms(),
            avg_latency_b: b.avg_latency_ms(),
            latency_diff: b.avg_latency_ms() - a.avg_latency_ms(),
            // Statistical significance would require more complex calculation
            is_significant: false,
            confidence: 0.0,
        })
    }

    /// Get summary statistics
    pub fn summary(&self) -> ExperimentSummary {
        let branch_count = self.branches.len();
        let total_queries = self.total_queries;

        let (best_branch, best_success_rate) = self.branches
            .iter()
            .max_by(|(_, a), (_, b)| {
                a.success_rate().partial_cmp(&b.success_rate()).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(name, metrics)| (Some(name.clone()), metrics.success_rate()))
            .unwrap_or((None, 0.0));

        let avg_latency = if total_queries == 0 {
            0.0
        } else {
            self.branches.values().map(|b| b.total_latency_ms).sum::<f64>() / total_queries as f64
        };

        ExperimentSummary {
            experiment: self.experiment.clone(),
            branch_count,
            total_queries,
            best_branch,
            best_success_rate,
            avg_latency_ms: avg_latency,
            duration: self.started_at.map(|start| {
                chrono::Utc::now().signed_duration_since(start)
            }),
        }
    }
}

/// Comparison between two branches
#[derive(Debug, Clone)]
pub struct BranchComparison {
    /// First branch name
    pub branch_a: String,
    /// Second branch name
    pub branch_b: String,
    /// Sample size for branch A
    pub sample_size_a: u64,
    /// Sample size for branch B
    pub sample_size_b: u64,
    /// Success rate for branch A
    pub success_rate_a: f64,
    /// Success rate for branch B
    pub success_rate_b: f64,
    /// Difference in success rate (B - A)
    pub success_rate_diff: f64,
    /// Average latency for branch A
    pub avg_latency_a: f64,
    /// Average latency for branch B
    pub avg_latency_b: f64,
    /// Difference in latency (B - A)
    pub latency_diff: f64,
    /// Is the difference statistically significant
    pub is_significant: bool,
    /// Confidence level (if significant)
    pub confidence: f64,
}

/// Experiment summary
#[derive(Debug, Clone)]
pub struct ExperimentSummary {
    /// Experiment name
    pub experiment: String,
    /// Number of branches
    pub branch_count: usize,
    /// Total queries
    pub total_queries: u64,
    /// Best performing branch
    pub best_branch: Option<String>,
    /// Best success rate
    pub best_success_rate: f64,
    /// Average latency
    pub avg_latency_ms: f64,
    /// Experiment duration
    pub duration: Option<chrono::Duration>,
}

/// A/B Metrics Manager
pub struct ABMetrics {
    /// Per-experiment metrics
    experiments: Arc<RwLock<HashMap<String, ExperimentMetrics>>>,
}

impl ABMetrics {
    /// Create a new metrics manager
    pub fn new() -> Self {
        Self {
            experiments: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Record a query
    pub async fn record_query(
        &self,
        experiment: &str,
        branch: &str,
        latency_ms: f64,
        success: bool,
    ) {
        let mut experiments = self.experiments.write().await;
        let metrics = experiments
            .entry(experiment.to_string())
            .or_insert_with(|| ExperimentMetrics::new(experiment));

        metrics.record_query(branch, latency_ms, success);
    }

    /// Record a custom event
    pub async fn record_event(
        &self,
        experiment: &str,
        branch: &str,
        event_name: &str,
        value: f64,
    ) {
        let mut experiments = self.experiments.write().await;
        let metrics = experiments
            .entry(experiment.to_string())
            .or_insert_with(|| ExperimentMetrics::new(experiment));

        metrics.record_event(branch, event_name, value);
    }

    /// Get experiment metrics
    pub async fn get_experiment_metrics(&self, experiment: &str) -> Option<ExperimentMetrics> {
        self.experiments.read().await.get(experiment).cloned()
    }

    /// Get branch metrics
    pub async fn get_branch_metrics(
        &self,
        experiment: &str,
        branch: &str,
    ) -> Option<BranchMetrics> {
        self.experiments
            .read()
            .await
            .get(experiment)
            .and_then(|e| e.get_branch(branch).cloned())
    }

    /// Compare branches
    pub async fn compare_branches(
        &self,
        experiment: &str,
        branch_a: &str,
        branch_b: &str,
    ) -> Option<BranchComparison> {
        self.experiments
            .read()
            .await
            .get(experiment)
            .and_then(|e| e.compare(branch_a, branch_b))
    }

    /// Get experiment summary
    pub async fn get_summary(&self, experiment: &str) -> Option<ExperimentSummary> {
        self.experiments
            .read()
            .await
            .get(experiment)
            .map(|e| e.summary())
    }

    /// List all experiments with summaries
    pub async fn all_summaries(&self) -> Vec<ExperimentSummary> {
        self.experiments
            .read()
            .await
            .values()
            .map(|e| e.summary())
            .collect()
    }

    /// Clear metrics for an experiment
    pub async fn clear_experiment(&self, experiment: &str) {
        self.experiments.write().await.remove(experiment);
    }

    /// Clear all metrics
    pub async fn clear_all(&self) {
        self.experiments.write().await.clear();
    }

    /// Get overall statistics
    pub async fn stats(&self) -> MetricsStats {
        let experiments = self.experiments.read().await;
        let total_queries: u64 = experiments.values().map(|e| e.total_queries).sum();
        let total_branches: usize = experiments.values().map(|e| e.branches.len()).sum();

        MetricsStats {
            experiments_tracked: experiments.len(),
            total_branches_tracked: total_branches,
            total_queries_recorded: total_queries,
        }
    }
}

impl Default for ABMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Overall metrics statistics
#[derive(Debug, Clone)]
pub struct MetricsStats {
    /// Number of experiments being tracked
    pub experiments_tracked: usize,
    /// Total branches across all experiments
    pub total_branches_tracked: usize,
    /// Total queries recorded
    pub total_queries_recorded: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_branch_metrics_new() {
        let metrics = BranchMetrics::new("test");
        assert_eq!(metrics.branch, "test");
        assert_eq!(metrics.query_count, 0);
    }

    #[test]
    fn test_branch_metrics_record() {
        let mut metrics = BranchMetrics::new("test");

        metrics.record_query(10.0, true);
        metrics.record_query(20.0, true);
        metrics.record_query(30.0, false);

        assert_eq!(metrics.query_count, 3);
        assert_eq!(metrics.success_count, 2);
        assert_eq!(metrics.failure_count, 1);
        assert_eq!(metrics.min_latency_ms, 10.0);
        assert_eq!(metrics.max_latency_ms, 30.0);
        assert_eq!(metrics.avg_latency_ms(), 20.0);
        assert!((metrics.success_rate() - 0.666).abs() < 0.01);
    }

    #[test]
    fn test_branch_metrics_events() {
        let mut metrics = BranchMetrics::new("test");

        metrics.record_event("conversion", 1.0);
        metrics.record_event("conversion", 1.0);
        metrics.record_event("revenue", 100.0);
        metrics.record_event("revenue", 200.0);

        assert_eq!(metrics.event_average("conversion"), 1.0);
        assert_eq!(metrics.event_average("revenue"), 150.0);
        assert_eq!(metrics.event_average("nonexistent"), 0.0);
    }

    #[test]
    fn test_experiment_metrics() {
        let mut metrics = ExperimentMetrics::new("test_exp");

        metrics.record_query("control", 10.0, true);
        metrics.record_query("control", 15.0, true);
        metrics.record_query("treatment", 8.0, true);
        metrics.record_query("treatment", 12.0, false);

        assert_eq!(metrics.total_queries, 4);
        assert_eq!(metrics.branches.len(), 2);

        let control = metrics.get_branch("control").unwrap();
        assert_eq!(control.query_count, 2);
        assert_eq!(control.success_rate(), 1.0);

        let treatment = metrics.get_branch("treatment").unwrap();
        assert_eq!(treatment.query_count, 2);
        assert_eq!(treatment.success_rate(), 0.5);
    }

    #[test]
    fn test_branch_comparison() {
        let mut metrics = ExperimentMetrics::new("test_exp");

        metrics.record_query("control", 100.0, true);
        metrics.record_query("control", 100.0, false);
        metrics.record_query("treatment", 80.0, true);
        metrics.record_query("treatment", 80.0, true);

        let comparison = metrics.compare("control", "treatment").unwrap();

        assert_eq!(comparison.success_rate_a, 0.5);
        assert_eq!(comparison.success_rate_b, 1.0);
        assert_eq!(comparison.success_rate_diff, 0.5);
        assert_eq!(comparison.avg_latency_a, 100.0);
        assert_eq!(comparison.avg_latency_b, 80.0);
    }

    #[test]
    fn test_experiment_summary() {
        let mut metrics = ExperimentMetrics::new("test_exp");

        metrics.record_query("control", 10.0, true);
        metrics.record_query("treatment", 8.0, true);
        metrics.record_query("treatment", 8.0, true);

        let summary = metrics.summary();

        assert_eq!(summary.experiment, "test_exp");
        assert_eq!(summary.branch_count, 2);
        assert_eq!(summary.total_queries, 3);
        assert_eq!(summary.best_branch, Some("treatment".to_string()));
    }

    #[tokio::test]
    async fn test_ab_metrics_manager() {
        let manager = ABMetrics::new();

        manager.record_query("exp1", "control", 10.0, true).await;
        manager.record_query("exp1", "treatment", 8.0, true).await;
        manager.record_event("exp1", "control", "conversion", 1.0).await;

        let metrics = manager.get_experiment_metrics("exp1").await.unwrap();
        assert_eq!(metrics.total_queries, 2);

        let branch = manager.get_branch_metrics("exp1", "control").await.unwrap();
        assert_eq!(branch.query_count, 1);

        let stats = manager.stats().await;
        assert_eq!(stats.experiments_tracked, 1);
        assert_eq!(stats.total_queries_recorded, 2);
    }

    #[tokio::test]
    async fn test_clear_metrics() {
        let manager = ABMetrics::new();

        manager.record_query("exp1", "control", 10.0, true).await;
        manager.record_query("exp2", "control", 10.0, true).await;

        manager.clear_experiment("exp1").await;
        assert!(manager.get_experiment_metrics("exp1").await.is_none());
        assert!(manager.get_experiment_metrics("exp2").await.is_some());

        manager.clear_all().await;
        assert!(manager.get_experiment_metrics("exp2").await.is_none());
    }
}
