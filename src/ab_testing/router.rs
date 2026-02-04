//! A/B Router - User-to-Branch Routing
//!
//! Routes users to experiment branches based on configurable assignment rules.

use super::experiment::{Experiment, ExperimentState};
use super::{ABTestingError, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// User context for routing decisions
#[derive(Debug, Clone)]
pub struct UserContext {
    /// User identifier
    pub user_id: String,
    /// User groups/segments
    pub groups: Vec<String>,
    /// Custom attributes
    pub attributes: HashMap<String, String>,
    /// Session ID (for sticky sessions)
    pub session_id: Option<String>,
    /// Request timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl UserContext {
    /// Create a new user context
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
            groups: Vec::new(),
            attributes: HashMap::new(),
            session_id: None,
            timestamp: chrono::Utc::now(),
        }
    }

    /// Add groups
    pub fn with_groups(mut self, groups: Vec<String>) -> Self {
        self.groups = groups;
        self
    }

    /// Add an attribute
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// Set session ID
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Get a hash for consistent assignment
    pub fn hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.user_id.hash(&mut hasher);
        hasher.finish()
    }
}

/// Assignment strategy
#[derive(Debug, Clone)]
pub enum Assignment {
    /// Assign based on user ID modulo
    UserIdModulo {
        /// Divisor for modulo operation
        divisor: u64,
    },
    /// Assign based on user ID hash
    UserIdHash,
    /// Assign based on attribute value
    Attribute {
        /// Attribute name
        name: String,
        /// Value to branch mapping
        mapping: HashMap<String, String>,
    },
    /// Assign based on group membership
    Group {
        /// Group to branch mapping
        mapping: HashMap<String, String>,
    },
    /// Assign based on percentage (random with seed)
    Percentage,
    /// Always assign to specific branch
    Fixed {
        /// Branch name
        branch: String,
    },
    /// Round-robin assignment (for testing)
    RoundRobin,
}

impl Default for Assignment {
    fn default() -> Self {
        Assignment::UserIdHash
    }
}

/// Sticky assignment cache entry
#[derive(Debug, Clone)]
struct StickyEntry {
    /// Assigned branch
    branch: String,
    /// Assignment timestamp
    assigned_at: chrono::DateTime<chrono::Utc>,
}

/// A/B Router
pub struct ABRouter {
    /// Experiments by name
    experiments: Arc<RwLock<HashMap<String, Experiment>>>,
    /// Assignment strategy per experiment
    assignments: Arc<RwLock<HashMap<String, Assignment>>>,
    /// Sticky session cache: (experiment, user_id) -> branch
    sticky_cache: Arc<RwLock<HashMap<(String, String), StickyEntry>>>,
    /// Default experiment (for routing without explicit experiment)
    default_experiment: Arc<RwLock<Option<String>>>,
    /// Round-robin counter (for testing)
    rr_counter: Arc<RwLock<HashMap<String, usize>>>,
}

impl ABRouter {
    /// Create a new router
    pub fn new() -> Self {
        Self {
            experiments: Arc::new(RwLock::new(HashMap::new())),
            assignments: Arc::new(RwLock::new(HashMap::new())),
            sticky_cache: Arc::new(RwLock::new(HashMap::new())),
            default_experiment: Arc::new(RwLock::new(None)),
            rr_counter: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add an experiment
    pub async fn add_experiment(&self, experiment: Experiment) -> Result<Uuid> {
        let name = experiment.name.clone();
        let id = experiment.id;

        {
            let experiments = self.experiments.read().await;
            if experiments.contains_key(&name) {
                return Err(ABTestingError::ExperimentExists(name));
            }
        }

        self.experiments.write().await.insert(name.clone(), experiment);
        self.assignments.write().await.insert(name.clone(), Assignment::default());

        tracing::info!("Added experiment: {}", name);
        Ok(id)
    }

    /// Remove an experiment
    pub async fn remove_experiment(&self, name: &str) -> Result<()> {
        self.experiments.write().await.remove(name);
        self.assignments.write().await.remove(name);

        // Clear sticky cache for this experiment
        let mut cache = self.sticky_cache.write().await;
        cache.retain(|(exp, _), _| exp != name);

        tracing::info!("Removed experiment: {}", name);
        Ok(())
    }

    /// Get an experiment
    pub async fn get_experiment(&self, name: &str) -> Option<Experiment> {
        self.experiments.read().await.get(name).cloned()
    }

    /// List all experiments
    pub async fn list_experiments(&self) -> Vec<Experiment> {
        self.experiments.read().await.values().cloned().collect()
    }

    /// Set experiment state
    pub async fn set_experiment_state(&self, name: &str, state: ExperimentState) -> Result<()> {
        let mut experiments = self.experiments.write().await;
        let exp = experiments.get_mut(name).ok_or_else(|| {
            ABTestingError::ExperimentNotFound(name.to_string())
        })?;

        match state {
            ExperimentState::Active => exp.start()?,
            ExperimentState::Paused => exp.pause()?,
            ExperimentState::Archived => exp.archive(),
            _ => {
                return Err(ABTestingError::Configuration(format!(
                    "Cannot directly set state to {:?}",
                    state
                )));
            }
        }

        Ok(())
    }

    /// Complete an experiment
    pub async fn complete_experiment(&self, name: &str, winner: Option<&str>) -> Result<()> {
        let mut experiments = self.experiments.write().await;
        let exp = experiments.get_mut(name).ok_or_else(|| {
            ABTestingError::ExperimentNotFound(name.to_string())
        })?;

        exp.complete(winner.map(String::from))
    }

    /// Set assignment strategy for an experiment
    pub async fn set_assignment(&self, name: &str, assignment: Assignment) -> Result<()> {
        if !self.experiments.read().await.contains_key(name) {
            return Err(ABTestingError::ExperimentNotFound(name.to_string()));
        }

        self.assignments.write().await.insert(name.to_string(), assignment);
        Ok(())
    }

    /// Set default experiment
    pub async fn set_default_experiment(&self, name: Option<&str>) -> Result<()> {
        if let Some(n) = name {
            if !self.experiments.read().await.contains_key(n) {
                return Err(ABTestingError::ExperimentNotFound(n.to_string()));
            }
        }

        *self.default_experiment.write().await = name.map(String::from);
        Ok(())
    }

    /// Route a user to a branch
    pub async fn route_user(&self, experiment: &str, context: &UserContext) -> Result<String> {
        let experiments = self.experiments.read().await;
        let exp = experiments.get(experiment).ok_or_else(|| {
            ABTestingError::ExperimentNotFound(experiment.to_string())
        })?;

        // Check if experiment accepts traffic
        if !exp.accepts_traffic() {
            return Err(ABTestingError::ExperimentNotActive(experiment.to_string()));
        }

        // Check exclusions
        if self.is_user_excluded(exp, context) {
            // Return control branch for excluded users
            return exp.control_branch()
                .cloned()
                .ok_or_else(|| ABTestingError::BranchNotFound("control".to_string()));
        }

        // If paused, return control
        if exp.state == ExperimentState::Paused {
            return exp.control_branch()
                .cloned()
                .ok_or_else(|| ABTestingError::BranchNotFound("control".to_string()));
        }

        // Check sticky cache
        if exp.config.sticky_sessions {
            let cache = self.sticky_cache.read().await;
            let key = (experiment.to_string(), context.user_id.clone());
            if let Some(entry) = cache.get(&key) {
                return Ok(entry.branch.clone());
            }
        }

        // Get assignment strategy
        let assignments = self.assignments.read().await;
        let assignment = assignments.get(experiment)
            .cloned()
            .unwrap_or_default();
        drop(assignments);

        // Compute branch
        let branch = self.compute_assignment(exp, context, &assignment).await?;

        // Store in sticky cache
        if exp.config.sticky_sessions {
            let key = (experiment.to_string(), context.user_id.clone());
            let entry = StickyEntry {
                branch: branch.clone(),
                assigned_at: chrono::Utc::now(),
            };
            self.sticky_cache.write().await.insert(key, entry);
        }

        Ok(branch)
    }

    /// Route using default experiment
    pub async fn route_default(&self, context: &UserContext) -> Option<String> {
        let default_exp = self.default_experiment.read().await.clone()?;
        self.route_user(&default_exp, context).await.ok()
    }

    /// Check if user is excluded from experiment
    fn is_user_excluded(&self, exp: &Experiment, context: &UserContext) -> bool {
        // Check excluded groups
        if !exp.config.excluded_groups.is_empty() {
            for group in &context.groups {
                if exp.config.excluded_groups.contains(group) {
                    return true;
                }
            }
        }

        // Check included groups (if specified, user must be in one)
        if !exp.config.included_groups.is_empty() {
            let in_included = context.groups.iter()
                .any(|g| exp.config.included_groups.contains(g));
            if !in_included {
                return true;
            }
        }

        false
    }

    /// Compute branch assignment
    async fn compute_assignment(
        &self,
        exp: &Experiment,
        context: &UserContext,
        assignment: &Assignment,
    ) -> Result<String> {
        match assignment {
            Assignment::UserIdModulo { divisor } => {
                self.assign_by_modulo(exp, context, *divisor)
            }
            Assignment::UserIdHash => {
                self.assign_by_hash(exp, context)
            }
            Assignment::Attribute { name, mapping } => {
                self.assign_by_attribute(exp, context, name, mapping)
            }
            Assignment::Group { mapping } => {
                self.assign_by_group(exp, context, mapping)
            }
            Assignment::Percentage => {
                self.assign_by_percentage(exp, context)
            }
            Assignment::Fixed { branch } => {
                if exp.branches.contains(branch) {
                    Ok(branch.clone())
                } else {
                    Err(ABTestingError::BranchNotFound(branch.clone()))
                }
            }
            Assignment::RoundRobin => {
                self.assign_round_robin(exp).await
            }
        }
    }

    fn assign_by_modulo(&self, exp: &Experiment, context: &UserContext, divisor: u64) -> Result<String> {
        let hash = context.hash();
        let bucket = hash % divisor;

        // Map bucket to branch based on allocation
        let mut cumulative = 0u64;
        for branch in &exp.branches {
            let alloc = exp.get_allocation(branch) as u64;
            cumulative += alloc * divisor / 100;
            if bucket < cumulative {
                return Ok(branch.clone());
            }
        }

        // Fallback to last branch
        exp.branches.last()
            .cloned()
            .ok_or_else(|| ABTestingError::Internal("No branches".to_string()))
    }

    fn assign_by_hash(&self, exp: &Experiment, context: &UserContext) -> Result<String> {
        let hash = context.hash();
        let bucket = (hash % 100) as u32;

        let mut cumulative = 0u32;
        for branch in &exp.branches {
            cumulative += exp.get_allocation(branch);
            if bucket < cumulative {
                return Ok(branch.clone());
            }
        }

        exp.branches.last()
            .cloned()
            .ok_or_else(|| ABTestingError::Internal("No branches".to_string()))
    }

    fn assign_by_attribute(
        &self,
        exp: &Experiment,
        context: &UserContext,
        attr_name: &str,
        mapping: &HashMap<String, String>,
    ) -> Result<String> {
        if let Some(value) = context.attributes.get(attr_name) {
            if let Some(branch) = mapping.get(value) {
                if exp.branches.contains(branch) {
                    return Ok(branch.clone());
                }
            }
        }

        // Default to control
        exp.control_branch()
            .cloned()
            .ok_or_else(|| ABTestingError::BranchNotFound("control".to_string()))
    }

    fn assign_by_group(
        &self,
        exp: &Experiment,
        context: &UserContext,
        mapping: &HashMap<String, String>,
    ) -> Result<String> {
        for group in &context.groups {
            if let Some(branch) = mapping.get(group) {
                if exp.branches.contains(branch) {
                    return Ok(branch.clone());
                }
            }
        }

        // Default to control
        exp.control_branch()
            .cloned()
            .ok_or_else(|| ABTestingError::BranchNotFound("control".to_string()))
    }

    fn assign_by_percentage(&self, exp: &Experiment, context: &UserContext) -> Result<String> {
        // Same as hash but with different semantics
        self.assign_by_hash(exp, context)
    }

    async fn assign_round_robin(&self, exp: &Experiment) -> Result<String> {
        let mut counters = self.rr_counter.write().await;
        let counter = counters.entry(exp.name.clone()).or_insert(0);
        let branch = &exp.branches[*counter % exp.branches.len()];
        *counter += 1;
        Ok(branch.clone())
    }

    /// Clear sticky cache for an experiment
    pub async fn clear_sticky_cache(&self, experiment: &str) {
        let mut cache = self.sticky_cache.write().await;
        cache.retain(|(exp, _), _| exp != experiment);
    }

    /// Clear sticky cache for a user
    pub async fn clear_user_cache(&self, user_id: &str) {
        let mut cache = self.sticky_cache.write().await;
        cache.retain(|(_, uid), _| uid != user_id);
    }
}

impl Default for ABRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_context() {
        let ctx = UserContext::new("user123")
            .with_groups(vec!["beta".to_string()])
            .with_attribute("region", "US")
            .with_session("session456");

        assert_eq!(ctx.user_id, "user123");
        assert_eq!(ctx.groups, vec!["beta"]);
        assert_eq!(ctx.attributes.get("region"), Some(&"US".to_string()));
        assert_eq!(ctx.session_id, Some("session456".to_string()));
    }

    #[test]
    fn test_user_context_hash() {
        let ctx1 = UserContext::new("user123");
        let ctx2 = UserContext::new("user123");
        let ctx3 = UserContext::new("user456");

        // Same user = same hash
        assert_eq!(ctx1.hash(), ctx2.hash());
        // Different user = different hash (usually)
        assert_ne!(ctx1.hash(), ctx3.hash());
    }

    #[tokio::test]
    async fn test_router_add_experiment() {
        let router = ABRouter::new();

        let exp = Experiment::new(
            "test_exp",
            vec!["control".to_string(), "treatment".to_string()],
        );

        let id = router.add_experiment(exp).await.unwrap();
        assert!(!id.is_nil());

        let exp = router.get_experiment("test_exp").await.unwrap();
        assert_eq!(exp.name, "test_exp");
    }

    #[tokio::test]
    async fn test_router_duplicate_experiment() {
        let router = ABRouter::new();

        let exp1 = Experiment::new("test", vec!["a".to_string()]);
        let exp2 = Experiment::new("test", vec!["b".to_string()]);

        router.add_experiment(exp1).await.unwrap();
        let result = router.add_experiment(exp2).await;

        assert!(matches!(result, Err(ABTestingError::ExperimentExists(_))));
    }

    #[tokio::test]
    async fn test_route_user() {
        let router = ABRouter::new();

        let mut exp = Experiment::new(
            "test_exp",
            vec!["control".to_string(), "treatment".to_string()],
        );
        exp.start().unwrap();

        router.add_experiment(exp).await.unwrap();

        let ctx = UserContext::new("user123");
        let branch = router.route_user("test_exp", &ctx).await.unwrap();

        assert!(["control", "treatment"].contains(&branch.as_str()));
    }

    #[tokio::test]
    async fn test_sticky_sessions() {
        let router = ABRouter::new();

        let mut exp = Experiment::new(
            "test_exp",
            vec!["control".to_string(), "treatment".to_string()],
        );
        exp.start().unwrap();

        router.add_experiment(exp).await.unwrap();

        let ctx = UserContext::new("user123");

        // Route multiple times
        let branch1 = router.route_user("test_exp", &ctx).await.unwrap();
        let branch2 = router.route_user("test_exp", &ctx).await.unwrap();
        let branch3 = router.route_user("test_exp", &ctx).await.unwrap();

        // Should always get same branch (sticky)
        assert_eq!(branch1, branch2);
        assert_eq!(branch2, branch3);
    }

    #[tokio::test]
    async fn test_round_robin() {
        let router = ABRouter::new();

        let mut exp = Experiment::new(
            "test_exp",
            vec!["a".to_string(), "b".to_string()],
        );
        exp.start().unwrap();

        router.add_experiment(exp).await.unwrap();
        router.set_assignment("test_exp", Assignment::RoundRobin).await.unwrap();

        // Clear any sticky cache
        router.clear_sticky_cache("test_exp").await;

        // With round robin, branches should alternate
        let mut branches = Vec::new();
        for i in 0..4 {
            let ctx = UserContext::new(format!("user{}", i));
            router.clear_user_cache(&ctx.user_id).await;
            let branch = router.route_user("test_exp", &ctx).await.unwrap();
            branches.push(branch);
        }

        // Should alternate
        assert_eq!(branches[0], branches[2]);
        assert_eq!(branches[1], branches[3]);
        assert_ne!(branches[0], branches[1]);
    }

    #[tokio::test]
    async fn test_paused_experiment() {
        let router = ABRouter::new();

        let mut exp = Experiment::new(
            "test_exp",
            vec!["control".to_string(), "treatment".to_string()],
        );
        exp.start().unwrap();

        router.add_experiment(exp).await.unwrap();
        router.set_experiment_state("test_exp", ExperimentState::Paused).await.unwrap();

        let ctx = UserContext::new("user123");
        let branch = router.route_user("test_exp", &ctx).await.unwrap();

        // Paused experiments route to control
        assert_eq!(branch, "control");
    }
}
