//! Experiment Configuration - A/B Testing
//!
//! Defines experiments, variants, and their lifecycle.

use super::{ABTestingError, Result};
use std::collections::HashMap;
use uuid::Uuid;

/// Experiment state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExperimentState {
    /// Experiment is being configured
    Draft,
    /// Experiment is active and routing users
    Active,
    /// Experiment is paused (users go to control)
    Paused,
    /// Experiment is completed
    Completed,
    /// Experiment was archived
    Archived,
}

/// Experiment definition
#[derive(Debug, Clone)]
pub struct Experiment {
    /// Unique experiment ID
    pub id: Uuid,
    /// Experiment name (unique)
    pub name: String,
    /// Description
    pub description: Option<String>,
    /// Branch names (first is typically control)
    pub branches: Vec<String>,
    /// Traffic allocation per branch (must sum to 100)
    pub allocation: HashMap<String, u32>,
    /// Current state
    pub state: ExperimentState,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Started timestamp
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Completed timestamp
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Winning branch (if completed)
    pub winner: Option<String>,
    /// Configuration
    pub config: ExperimentConfig,
    /// Tags for organization
    pub tags: Vec<String>,
    /// Owner/creator
    pub owner: Option<String>,
}

impl Experiment {
    /// Create a new experiment with equal allocation
    pub fn new(name: impl Into<String>, branches: Vec<String>) -> Self {
        let name = name.into();
        let branch_count = branches.len();
        let base_allocation = 100 / branch_count as u32;
        let remainder = 100 % branch_count as u32;

        let mut allocation = HashMap::new();
        for (i, branch) in branches.iter().enumerate() {
            let extra = if i == 0 { remainder } else { 0 };
            allocation.insert(branch.clone(), base_allocation + extra);
        }

        Self {
            id: Uuid::new_v4(),
            name,
            description: None,
            branches,
            allocation,
            state: ExperimentState::Draft,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            winner: None,
            config: ExperimentConfig::default(),
            tags: Vec::new(),
            owner: None,
        }
    }

    /// Set custom allocation
    pub fn with_allocation(mut self, allocation: HashMap<String, u32>) -> Result<Self> {
        // Validate allocation
        let total: u32 = allocation.values().sum();
        if total != 100 {
            return Err(ABTestingError::Configuration(format!(
                "Allocation must sum to 100, got {}",
                total
            )));
        }

        for branch in allocation.keys() {
            if !self.branches.contains(branch) {
                return Err(ABTestingError::BranchNotFound(branch.clone()));
            }
        }

        self.allocation = allocation;
        Ok(self)
    }

    /// Set description
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Set tags
    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    /// Set owner
    pub fn with_owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = Some(owner.into());
        self
    }

    /// Set configuration
    pub fn with_config(mut self, config: ExperimentConfig) -> Self {
        self.config = config;
        self
    }

    /// Get the control branch (first branch)
    pub fn control_branch(&self) -> Option<&String> {
        self.branches.first()
    }

    /// Get allocation for a branch
    pub fn get_allocation(&self, branch: &str) -> u32 {
        *self.allocation.get(branch).unwrap_or(&0)
    }

    /// Check if experiment is active
    pub fn is_active(&self) -> bool {
        self.state == ExperimentState::Active
    }

    /// Check if experiment is accepting traffic
    pub fn accepts_traffic(&self) -> bool {
        matches!(self.state, ExperimentState::Active | ExperimentState::Paused)
    }

    /// Start the experiment
    pub fn start(&mut self) -> Result<()> {
        if self.state != ExperimentState::Draft && self.state != ExperimentState::Paused {
            return Err(ABTestingError::Configuration(format!(
                "Cannot start experiment in {:?} state",
                self.state
            )));
        }

        self.state = ExperimentState::Active;
        self.started_at = Some(chrono::Utc::now());
        Ok(())
    }

    /// Pause the experiment
    pub fn pause(&mut self) -> Result<()> {
        if self.state != ExperimentState::Active {
            return Err(ABTestingError::Configuration(format!(
                "Cannot pause experiment in {:?} state",
                self.state
            )));
        }

        self.state = ExperimentState::Paused;
        Ok(())
    }

    /// Complete the experiment
    pub fn complete(&mut self, winner: Option<String>) -> Result<()> {
        if let Some(ref w) = winner {
            if !self.branches.contains(w) {
                return Err(ABTestingError::BranchNotFound(w.clone()));
            }
        }

        self.state = ExperimentState::Completed;
        self.completed_at = Some(chrono::Utc::now());
        self.winner = winner;
        Ok(())
    }

    /// Archive the experiment
    pub fn archive(&mut self) {
        self.state = ExperimentState::Archived;
    }

    /// Get experiment duration (if started)
    pub fn duration(&self) -> Option<chrono::Duration> {
        self.started_at.map(|start| {
            let end = self.completed_at.unwrap_or_else(chrono::Utc::now);
            end.signed_duration_since(start)
        })
    }
}

/// Experiment configuration
#[derive(Debug, Clone)]
pub struct ExperimentConfig {
    /// Minimum sample size before results are significant
    pub min_sample_size: u64,
    /// Confidence level for statistical significance (e.g., 0.95)
    pub confidence_level: f64,
    /// Allow users to be reassigned on experiment changes
    pub allow_reassignment: bool,
    /// Sticky sessions (same user always gets same branch)
    pub sticky_sessions: bool,
    /// Exclude specific user groups
    pub excluded_groups: Vec<String>,
    /// Only include specific user groups
    pub included_groups: Vec<String>,
    /// Auto-complete when statistical significance reached
    pub auto_complete: bool,
    /// Maximum duration before auto-complete
    pub max_duration_hours: Option<u32>,
}

impl Default for ExperimentConfig {
    fn default() -> Self {
        Self {
            min_sample_size: 100,
            confidence_level: 0.95,
            allow_reassignment: false,
            sticky_sessions: true,
            excluded_groups: Vec::new(),
            included_groups: Vec::new(),
            auto_complete: false,
            max_duration_hours: None,
        }
    }
}

impl ExperimentConfig {
    /// Set minimum sample size
    pub fn with_min_sample_size(mut self, size: u64) -> Self {
        self.min_sample_size = size;
        self
    }

    /// Set confidence level
    pub fn with_confidence_level(mut self, level: f64) -> Self {
        self.confidence_level = level;
        self
    }

    /// Enable sticky sessions
    pub fn with_sticky_sessions(mut self, sticky: bool) -> Self {
        self.sticky_sessions = sticky;
        self
    }

    /// Set excluded groups
    pub fn with_excluded_groups(mut self, groups: Vec<String>) -> Self {
        self.excluded_groups = groups;
        self
    }

    /// Set included groups
    pub fn with_included_groups(mut self, groups: Vec<String>) -> Self {
        self.included_groups = groups;
        self
    }

    /// Enable auto-complete
    pub fn with_auto_complete(mut self, auto: bool) -> Self {
        self.auto_complete = auto;
        self
    }

    /// Set maximum duration
    pub fn with_max_duration(mut self, hours: u32) -> Self {
        self.max_duration_hours = Some(hours);
        self
    }
}

/// Experiment variant (branch) information
#[derive(Debug, Clone)]
pub struct Variant {
    /// Branch name
    pub name: String,
    /// Traffic allocation percentage
    pub allocation: u32,
    /// Description
    pub description: Option<String>,
    /// Is this the control variant
    pub is_control: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_experiment_new() {
        let exp = Experiment::new(
            "test",
            vec!["control".to_string(), "treatment".to_string()],
        );

        assert_eq!(exp.name, "test");
        assert_eq!(exp.branches.len(), 2);
        assert_eq!(exp.state, ExperimentState::Draft);

        // Check allocation (50/50)
        assert_eq!(exp.get_allocation("control"), 50);
        assert_eq!(exp.get_allocation("treatment"), 50);
    }

    #[test]
    fn test_experiment_uneven_branches() {
        let exp = Experiment::new(
            "test",
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        );

        // 100 / 3 = 33 each, with remainder on first
        let total: u32 = exp.allocation.values().sum();
        assert_eq!(total, 100);
    }

    #[test]
    fn test_custom_allocation() {
        let exp = Experiment::new(
            "test",
            vec!["control".to_string(), "treatment".to_string()],
        );

        let mut alloc = HashMap::new();
        alloc.insert("control".to_string(), 80);
        alloc.insert("treatment".to_string(), 20);

        let exp = exp.with_allocation(alloc).unwrap();
        assert_eq!(exp.get_allocation("control"), 80);
        assert_eq!(exp.get_allocation("treatment"), 20);
    }

    #[test]
    fn test_invalid_allocation() {
        let exp = Experiment::new(
            "test",
            vec!["control".to_string(), "treatment".to_string()],
        );

        let mut alloc = HashMap::new();
        alloc.insert("control".to_string(), 60);
        alloc.insert("treatment".to_string(), 60);

        let result = exp.with_allocation(alloc);
        assert!(result.is_err());
    }

    #[test]
    fn test_experiment_lifecycle() {
        let mut exp = Experiment::new(
            "test",
            vec!["control".to_string(), "treatment".to_string()],
        );

        // Start
        exp.start().unwrap();
        assert_eq!(exp.state, ExperimentState::Active);
        assert!(exp.started_at.is_some());

        // Pause
        exp.pause().unwrap();
        assert_eq!(exp.state, ExperimentState::Paused);

        // Resume
        exp.start().unwrap();
        assert_eq!(exp.state, ExperimentState::Active);

        // Complete
        exp.complete(Some("treatment".to_string())).unwrap();
        assert_eq!(exp.state, ExperimentState::Completed);
        assert_eq!(exp.winner, Some("treatment".to_string()));
    }

    #[test]
    fn test_control_branch() {
        let exp = Experiment::new(
            "test",
            vec!["control".to_string(), "treatment".to_string()],
        );

        assert_eq!(exp.control_branch(), Some(&"control".to_string()));
    }

    #[test]
    fn test_experiment_config() {
        let config = ExperimentConfig::default()
            .with_min_sample_size(1000)
            .with_confidence_level(0.99)
            .with_sticky_sessions(true)
            .with_auto_complete(true);

        assert_eq!(config.min_sample_size, 1000);
        assert_eq!(config.confidence_level, 0.99);
        assert!(config.sticky_sessions);
        assert!(config.auto_complete);
    }
}
