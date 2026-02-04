//! Branch-Based A/B Testing Module
//!
//! Routes users to different branches for live experiments.
//! Leverages HeliosDB-Lite's branching capability for zero-overhead A/B tests.
//!
//! # Features
//!
//! - **User Assignment**: Route users to experiment branches based on configurable rules
//! - **Experiment Management**: Create, pause, and analyze experiments
//! - **Metrics Collection**: Track query counts, latency, and custom metrics per branch
//! - **Gradual Rollouts**: Percentage-based traffic splitting
//!
//! # Example
//!
//! ```ignore
//! // Configure A/B testing
//! let ab = ABTesting::new();
//!
//! ab.create_experiment(Experiment {
//!     name: "new_pricing".to_string(),
//!     branches: vec!["control", "treatment"],
//!     assignment: Assignment::UserIdModulo { divisor: 100, threshold: 50 },
//! });
//!
//! // Route user to assigned branch
//! let branch = ab.get_branch_for_user("user_123")?;
//! ```
//!
//! # Feature Flag
//!
//! Requires `ha-ab-testing` feature to be enabled.

pub mod router;
pub mod experiment;
pub mod metrics;

// Re-exports
pub use router::{ABRouter, Assignment, UserContext};
pub use experiment::{Experiment, ExperimentConfig, ExperimentState};
pub use metrics::{ABMetrics, ExperimentMetrics, BranchMetrics};

use thiserror::Error;
use uuid::Uuid;

/// A/B Testing errors
#[derive(Debug, Error)]
pub enum ABTestingError {
    #[error("Experiment not found: {0}")]
    ExperimentNotFound(String),

    #[error("Branch not found: {0}")]
    BranchNotFound(String),

    #[error("Experiment already exists: {0}")]
    ExperimentExists(String),

    #[error("Invalid assignment rule: {0}")]
    InvalidAssignment(String),

    #[error("Experiment not active: {0}")]
    ExperimentNotActive(String),

    #[error("Configuration error: {0}")]
    Configuration(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, ABTestingError>;

/// A/B Testing manager
pub struct ABTesting {
    /// Router for user assignment
    router: ABRouter,
    /// Metrics collector
    metrics: ABMetrics,
}

impl ABTesting {
    /// Create a new A/B testing manager
    pub fn new() -> Self {
        Self {
            router: ABRouter::new(),
            metrics: ABMetrics::new(),
        }
    }

    /// Create an experiment
    pub async fn create_experiment(&self, experiment: Experiment) -> Result<Uuid> {
        self.router.add_experiment(experiment).await
    }

    /// Get experiment by name
    pub async fn get_experiment(&self, name: &str) -> Option<Experiment> {
        self.router.get_experiment(name).await
    }

    /// List all experiments
    pub async fn list_experiments(&self) -> Vec<Experiment> {
        self.router.list_experiments().await
    }

    /// Start an experiment
    pub async fn start_experiment(&self, name: &str) -> Result<()> {
        self.router.set_experiment_state(name, ExperimentState::Active).await
    }

    /// Pause an experiment
    pub async fn pause_experiment(&self, name: &str) -> Result<()> {
        self.router.set_experiment_state(name, ExperimentState::Paused).await
    }

    /// Complete an experiment
    pub async fn complete_experiment(&self, name: &str, winner: Option<&str>) -> Result<()> {
        self.router.complete_experiment(name, winner).await
    }

    /// Delete an experiment
    pub async fn delete_experiment(&self, name: &str) -> Result<()> {
        self.router.remove_experiment(name).await
    }

    /// Get the branch for a user in an experiment
    pub async fn get_branch(&self, experiment: &str, user_context: &UserContext) -> Result<String> {
        self.router.route_user(experiment, user_context).await
    }

    /// Get the branch for a user (default experiment)
    pub async fn get_default_branch(&self, user_context: &UserContext) -> Option<String> {
        self.router.route_default(user_context).await
    }

    /// Record a query for metrics
    pub async fn record_query(
        &self,
        experiment: &str,
        branch: &str,
        latency_ms: f64,
        success: bool,
    ) {
        self.metrics.record_query(experiment, branch, latency_ms, success).await;
    }

    /// Record a custom event
    pub async fn record_event(
        &self,
        experiment: &str,
        branch: &str,
        event_name: &str,
        value: f64,
    ) {
        self.metrics.record_event(experiment, branch, event_name, value).await;
    }

    /// Get metrics for an experiment
    pub async fn get_metrics(&self, experiment: &str) -> Option<ExperimentMetrics> {
        self.metrics.get_experiment_metrics(experiment).await
    }

    /// Get overall A/B testing statistics
    pub async fn stats(&self) -> ABStats {
        let experiments = self.router.list_experiments().await;
        let active = experiments.iter().filter(|e| e.state == ExperimentState::Active).count();

        ABStats {
            total_experiments: experiments.len(),
            active_experiments: active,
            total_branches: experiments.iter().map(|e| e.branches.len()).sum(),
        }
    }
}

impl Default for ABTesting {
    fn default() -> Self {
        Self::new()
    }
}

/// A/B testing statistics
#[derive(Debug, Clone)]
pub struct ABStats {
    /// Total experiments (all states)
    pub total_experiments: usize,
    /// Active experiments
    pub active_experiments: usize,
    /// Total branches across all experiments
    pub total_branches: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = ABTestingError::ExperimentNotFound("test".to_string());
        assert!(err.to_string().contains("test"));
    }

    #[tokio::test]
    async fn test_ab_testing_lifecycle() {
        let ab = ABTesting::new();

        // Create experiment
        let exp = Experiment::new(
            "test_exp",
            vec!["control".to_string(), "treatment".to_string()],
        );

        let id = ab.create_experiment(exp).await.unwrap();
        assert!(!id.is_nil());

        // Get experiment
        let exp = ab.get_experiment("test_exp").await.unwrap();
        assert_eq!(exp.name, "test_exp");
        assert_eq!(exp.state, ExperimentState::Draft);

        // Start experiment
        ab.start_experiment("test_exp").await.unwrap();
        let exp = ab.get_experiment("test_exp").await.unwrap();
        assert_eq!(exp.state, ExperimentState::Active);

        // Get branch for user
        let ctx = UserContext::new("user_123");
        let branch = ab.get_branch("test_exp", &ctx).await.unwrap();
        assert!(exp.branches.contains(&branch));

        // Record metrics
        ab.record_query("test_exp", &branch, 10.0, true).await;

        // Complete experiment
        ab.complete_experiment("test_exp", Some(&branch)).await.unwrap();
        let exp = ab.get_experiment("test_exp").await.unwrap();
        assert_eq!(exp.state, ExperimentState::Completed);
    }
}
