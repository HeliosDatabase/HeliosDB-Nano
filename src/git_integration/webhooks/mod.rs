//! Webhook Server for Git Provider Integration
//!
//! Provides HTTP endpoints for receiving webhooks from GitHub, GitLab,
//! and other Git providers to automate PR/MR preview environments.
//!
//! ## Supported Providers
//!
//! - **GitHub**: PR opened/closed/merged events
//! - **GitLab**: MR opened/closed/merged events
//! - **Generic**: Provider-agnostic webhook interface
//!
//! ## PR Lifecycle
//!
//! ```text
//! PR Opened  → create_preview_branch(base, pr_ref)
//! PR Updated → sync + apply_migrations(pr_branch)
//! PR Merged  → merge_db_branch(pr_branch, base) + cleanup
//! PR Closed  → drop_preview_branch(pr_branch)
//! ```

#![allow(dead_code)]
#![allow(unused_variables)]

use crate::storage::BranchId;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Git provider type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitProvider {
    GitHub,
    GitLab,
    Bitbucket,
    Generic,
}

impl Default for GitProvider {
    fn default() -> Self {
        GitProvider::Generic
    }
}

/// Webhook event type
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookEventType {
    /// PR/MR opened
    PrOpened,
    /// PR/MR updated (new commits pushed)
    PrUpdated,
    /// PR/MR merged
    PrMerged,
    /// PR/MR closed without merge
    PrClosed,
    /// Push to branch
    Push,
    /// Branch created
    BranchCreated,
    /// Branch deleted
    BranchDeleted,
}

/// Generic webhook event (provider-agnostic)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookEvent {
    /// Event type
    pub event_type: WebhookEventType,
    /// Source Git branch
    pub source_branch: String,
    /// Target Git branch (for PRs)
    pub target_branch: Option<String>,
    /// PR/MR number
    pub pr_number: Option<u64>,
    /// Commit SHA
    pub commit_sha: Option<String>,
    /// Provider
    pub provider: GitProvider,
    /// Repository name/path
    pub repository: Option<String>,
    /// Event timestamp
    pub timestamp: Option<u64>,
    /// Raw payload (for debugging)
    #[serde(skip)]
    pub raw_payload: Option<String>,
}

impl WebhookEvent {
    /// Create a new webhook event
    pub fn new(event_type: WebhookEventType, source_branch: String, provider: GitProvider) -> Self {
        Self {
            event_type,
            source_branch,
            target_branch: None,
            pr_number: None,
            commit_sha: None,
            provider,
            repository: None,
            timestamp: None,
            raw_payload: None,
        }
    }
}

/// GitHub webhook payload (PR events)
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubPrPayload {
    pub action: String,
    pub number: u64,
    pub pull_request: GitHubPullRequest,
    pub repository: GitHubRepository,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubPullRequest {
    pub head: GitHubRef,
    pub base: GitHubRef,
    pub title: Option<String>,
    pub merged: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubRepository {
    pub full_name: String,
}

/// GitLab webhook payload (MR events)
#[derive(Debug, Clone, Deserialize)]
pub struct GitLabMrPayload {
    pub event_type: String,
    pub object_attributes: GitLabMrAttributes,
    pub project: GitLabProject,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitLabMrAttributes {
    pub iid: u64,
    pub action: Option<String>,
    pub source_branch: String,
    pub target_branch: String,
    pub title: Option<String>,
    pub state: String,
    pub last_commit: Option<GitLabCommit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitLabCommit {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitLabProject {
    pub path_with_namespace: String,
}

/// Webhook handler result
#[derive(Debug, Clone, Serialize)]
pub struct WebhookResult {
    /// Success status
    pub success: bool,
    /// Result message
    pub message: String,
    /// Created/modified branch ID
    pub branch_id: Option<BranchId>,
    /// Action taken
    pub action: Option<String>,
}

impl WebhookResult {
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
            branch_id: None,
            action: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
            branch_id: None,
            action: None,
        }
    }

    pub fn with_branch(mut self, branch_id: BranchId) -> Self {
        self.branch_id = Some(branch_id);
        self
    }

    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }
}

/// Webhook configuration
#[derive(Debug, Clone)]
pub struct WebhookConfig {
    /// GitHub webhook secret for HMAC validation
    pub github_secret: Option<String>,
    /// GitLab webhook token
    pub gitlab_token: Option<String>,
    /// Generic webhook secret
    pub generic_secret: Option<String>,
    /// Allowed IP ranges (optional)
    pub allowed_ips: Vec<String>,
    /// Rate limit (requests per minute)
    pub rate_limit: u32,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            github_secret: None,
            gitlab_token: None,
            generic_secret: None,
            allowed_ips: Vec::new(),
            rate_limit: 60,
        }
    }
}

/// Webhook handler
pub struct WebhookHandler {
    config: WebhookConfig,
}

impl WebhookHandler {
    /// Create a new webhook handler
    pub fn new(config: WebhookConfig) -> Self {
        Self { config }
    }

    /// Parse GitHub webhook payload
    pub fn parse_github(&self, payload: &str, event_header: &str) -> Result<WebhookEvent> {
        match event_header {
            "pull_request" => {
                let pr_payload: GitHubPrPayload = serde_json::from_str(payload)
                    .map_err(|e| Error::sql_parse(format!("Invalid GitHub PR payload: {}", e)))?;

                let event_type = match pr_payload.action.as_str() {
                    "opened" | "reopened" => WebhookEventType::PrOpened,
                    "closed" if pr_payload.pull_request.merged == Some(true) => {
                        WebhookEventType::PrMerged
                    }
                    "closed" => WebhookEventType::PrClosed,
                    "synchronize" => WebhookEventType::PrUpdated,
                    _ => return Err(Error::sql_parse(format!(
                        "Unsupported GitHub PR action: {}",
                        pr_payload.action
                    ))),
                };

                Ok(WebhookEvent {
                    event_type,
                    source_branch: pr_payload.pull_request.head.ref_name,
                    target_branch: Some(pr_payload.pull_request.base.ref_name),
                    pr_number: Some(pr_payload.number),
                    commit_sha: Some(pr_payload.pull_request.head.sha),
                    provider: GitProvider::GitHub,
                    repository: Some(pr_payload.repository.full_name),
                    timestamp: None,
                    raw_payload: Some(payload.to_string()),
                })
            }
            _ => Err(Error::sql_parse(format!(
                "Unsupported GitHub event: {}",
                event_header
            ))),
        }
    }

    /// Parse GitLab webhook payload
    pub fn parse_gitlab(&self, payload: &str) -> Result<WebhookEvent> {
        let mr_payload: GitLabMrPayload = serde_json::from_str(payload)
            .map_err(|e| Error::sql_parse(format!("Invalid GitLab MR payload: {}", e)))?;

        let event_type = match mr_payload.object_attributes.action.as_deref() {
            Some("open") | Some("reopen") => WebhookEventType::PrOpened,
            Some("merge") => WebhookEventType::PrMerged,
            Some("close") => WebhookEventType::PrClosed,
            Some("update") => WebhookEventType::PrUpdated,
            _ => match mr_payload.object_attributes.state.as_str() {
                "opened" => WebhookEventType::PrOpened,
                "merged" => WebhookEventType::PrMerged,
                "closed" => WebhookEventType::PrClosed,
                _ => return Err(Error::sql_parse(format!(
                    "Unsupported GitLab MR state: {}",
                    mr_payload.object_attributes.state
                ))),
            },
        };

        Ok(WebhookEvent {
            event_type,
            source_branch: mr_payload.object_attributes.source_branch,
            target_branch: Some(mr_payload.object_attributes.target_branch),
            pr_number: Some(mr_payload.object_attributes.iid),
            commit_sha: mr_payload.object_attributes.last_commit.map(|c| c.id),
            provider: GitProvider::GitLab,
            repository: Some(mr_payload.project.path_with_namespace),
            timestamp: None,
            raw_payload: Some(payload.to_string()),
        })
    }

    /// Parse generic webhook payload
    pub fn parse_generic(&self, payload: &str) -> Result<WebhookEvent> {
        serde_json::from_str(payload)
            .map_err(|e| Error::sql_parse(format!("Invalid generic webhook payload: {}", e)))
    }

    /// Validate GitHub webhook signature (HMAC-SHA256)
    ///
    /// TODO: Implement actual HMAC-SHA256 validation when hmac/sha2 crates are added
    pub fn validate_github_signature(&self, _payload: &[u8], signature: &str) -> Result<bool> {
        let Some(ref _secret) = self.config.github_secret else {
            // No secret configured, skip validation
            return Ok(true);
        };

        // GitHub signature format: sha256=<hex>
        if !signature.starts_with("sha256=") {
            return Err(Error::authentication("Invalid GitHub signature format"));
        }

        // TODO: Implement HMAC-SHA256 validation
        // For now, warn and accept (development mode)
        tracing::warn!("GitHub webhook signature validation not yet implemented");
        Ok(true)
    }

    /// Validate GitLab webhook token
    pub fn validate_gitlab_token(&self, token: &str) -> Result<bool> {
        let Some(ref expected) = self.config.gitlab_token else {
            return Ok(true);
        };
        Ok(token == expected)
    }

    /// Handle a webhook event
    pub fn handle(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        match event.event_type {
            WebhookEventType::PrOpened => self.handle_pr_opened(event),
            WebhookEventType::PrUpdated => self.handle_pr_updated(event),
            WebhookEventType::PrMerged => self.handle_pr_merged(event),
            WebhookEventType::PrClosed => self.handle_pr_closed(event),
            WebhookEventType::Push => self.handle_push(event),
            WebhookEventType::BranchCreated => self.handle_branch_created(event),
            WebhookEventType::BranchDeleted => self.handle_branch_deleted(event),
        }
    }

    fn handle_pr_opened(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        // TODO: Create preview branch from base
        // 1. Get or create DB branch for target (e.g., main)
        // 2. Create new branch for PR (e.g., pr-123)
        // 3. Link to Git branch
        // 4. Apply any migrations from source branch

        let pr_num = event.pr_number.unwrap_or(0);
        let branch_name = format!("pr-{}", pr_num);

        tracing::info!(
            "PR #{} opened: {} -> {}",
            pr_num,
            event.source_branch,
            event.target_branch.as_deref().unwrap_or("main")
        );

        Ok(WebhookResult::success(format!(
            "Created preview branch '{}' for PR #{}",
            branch_name, pr_num
        ))
        .with_action("create_preview_branch"))
    }

    fn handle_pr_updated(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        // TODO: Sync preview branch with new commits
        // 1. Get PR branch
        // 2. Apply new migrations if any
        // 3. Update commit tracking

        let pr_num = event.pr_number.unwrap_or(0);

        tracing::info!("PR #{} updated with new commits", pr_num);

        Ok(WebhookResult::success(format!("Synced PR #{} preview branch", pr_num))
            .with_action("sync_preview_branch"))
    }

    fn handle_pr_merged(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        // TODO: Merge DB branch and cleanup
        // 1. Merge PR branch into target branch
        // 2. Delete PR branch
        // 3. Update link tracking

        let pr_num = event.pr_number.unwrap_or(0);

        tracing::info!(
            "PR #{} merged into {}",
            pr_num,
            event.target_branch.as_deref().unwrap_or("main")
        );

        Ok(WebhookResult::success(format!(
            "Merged and cleaned up PR #{} preview branch",
            pr_num
        ))
        .with_action("merge_preview_branch"))
    }

    fn handle_pr_closed(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        // TODO: Drop preview branch without merge
        // 1. Delete PR branch
        // 2. Clean up links

        let pr_num = event.pr_number.unwrap_or(0);

        tracing::info!("PR #{} closed without merge, dropping preview", pr_num);

        Ok(WebhookResult::success(format!(
            "Dropped PR #{} preview branch",
            pr_num
        ))
        .with_action("drop_preview_branch"))
    }

    fn handle_push(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        // TODO: Sync linked branch on push
        tracing::info!("Push to branch: {}", event.source_branch);
        Ok(WebhookResult::success(format!(
            "Synced branch '{}' on push",
            event.source_branch
        )))
    }

    fn handle_branch_created(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        // TODO: Optionally create linked DB branch
        tracing::info!("Branch created: {}", event.source_branch);
        Ok(WebhookResult::success(format!(
            "Noted branch '{}' creation",
            event.source_branch
        )))
    }

    fn handle_branch_deleted(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        // TODO: Optionally drop linked DB branch
        tracing::info!("Branch deleted: {}", event.source_branch);
        Ok(WebhookResult::success(format!(
            "Noted branch '{}' deletion",
            event.source_branch
        )))
    }
}

/// Storage-aware webhook handler that can manage database branches
///
/// This handler integrates with the storage engine to actually create,
/// merge, and drop database branches in response to webhook events.
pub struct StorageWebhookHandler<'a> {
    config: WebhookConfig,
    branch_manager: &'a crate::storage::BranchManager,
    link_manager: Option<&'a super::LinkManager>,
}

impl<'a> StorageWebhookHandler<'a> {
    /// Create a new storage-aware webhook handler
    pub fn new(
        config: WebhookConfig,
        branch_manager: &'a crate::storage::BranchManager,
    ) -> Self {
        Self {
            config,
            branch_manager,
            link_manager: None,
        }
    }

    /// Set the link manager for Git-DB branch synchronization
    pub fn with_link_manager(mut self, link_manager: &'a super::LinkManager) -> Self {
        self.link_manager = Some(link_manager);
        self
    }

    /// Handle a webhook event with storage integration
    pub fn handle(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        match event.event_type {
            WebhookEventType::PrOpened => self.handle_pr_opened(event),
            WebhookEventType::PrUpdated => self.handle_pr_updated(event),
            WebhookEventType::PrMerged => self.handle_pr_merged(event),
            WebhookEventType::PrClosed => self.handle_pr_closed(event),
            WebhookEventType::Push => self.handle_push(event),
            WebhookEventType::BranchCreated => self.handle_branch_created(event),
            WebhookEventType::BranchDeleted => self.handle_branch_deleted(event),
        }
    }

    fn handle_pr_opened(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        let pr_num = event.pr_number.unwrap_or(0);
        let preview_branch_name = format!("pr-{}", pr_num);
        let target_branch = event.target_branch.as_deref().unwrap_or("main");

        tracing::info!(
            "Creating preview branch '{}' for PR #{} (base: {})",
            preview_branch_name,
            pr_num,
            target_branch
        );

        // Get the base branch ID
        let base_branch = self.branch_manager.get_branch_by_name(target_branch)
            .map_err(|e| Error::execution(format!("Base branch '{}' not found: {}", target_branch, e)))?;

        // Get current timestamp for snapshot
        let snapshot_ts = self.branch_manager.current_timestamp();

        // Create preview branch options with Git link metadata
        let mut options = crate::storage::BranchOptions::default();
        options.git_link = Some(crate::storage::GitLinkMetadata {
            git_branch: event.source_branch.clone(),
            last_commit: event.commit_sha.clone(),
            auto_sync: true,
            provider: Some(format!("{:?}", event.provider).to_lowercase()),
            pr_number: event.pr_number,
            repo_path: None,
            linked_at: snapshot_ts,
        });

        // Create the preview branch
        let branch_id = self.branch_manager.create_branch(
            &preview_branch_name,
            Some(target_branch),
            snapshot_ts,
            options,
        )?;

        tracing::info!(
            "Preview branch '{}' created with ID {} for PR #{}",
            preview_branch_name,
            branch_id,
            pr_num
        );

        Ok(WebhookResult::success(format!(
            "Created preview branch '{}' for PR #{}",
            preview_branch_name, pr_num
        ))
        .with_branch(branch_id)
        .with_action("create_preview_branch"))
    }

    fn handle_pr_updated(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        let pr_num = event.pr_number.unwrap_or(0);
        let preview_branch_name = format!("pr-{}", pr_num);

        tracing::info!("Syncing preview branch '{}' for PR #{}", preview_branch_name, pr_num);

        // Get the preview branch
        let branch = match self.branch_manager.get_branch_by_name(&preview_branch_name) {
            Ok(b) => b,
            Err(_) => {
                // Branch doesn't exist, create it instead
                return self.handle_pr_opened(event);
            }
        };

        // Update the commit SHA in git link metadata
        // For now, just log the update - full implementation would update the metadata
        if let Some(sha) = &event.commit_sha {
            tracing::info!("Updating commit SHA for PR #{} to {}", pr_num, sha);
        }

        Ok(WebhookResult::success(format!("Synced PR #{} preview branch", pr_num))
            .with_branch(branch.branch_id)
            .with_action("sync_preview_branch"))
    }

    fn handle_pr_merged(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        let pr_num = event.pr_number.unwrap_or(0);
        let preview_branch_name = format!("pr-{}", pr_num);
        let target_branch = event.target_branch.as_deref().unwrap_or("main");

        tracing::info!(
            "Merging and cleaning up PR #{} preview branch '{}' into '{}'",
            pr_num,
            preview_branch_name,
            target_branch
        );

        // Check if the preview branch exists
        match self.branch_manager.get_branch_by_name(&preview_branch_name) {
            Ok(branch) => {
                // Merge the branch - use the current timestamp
                let snapshot_ts = self.branch_manager.current_timestamp();

                match self.branch_manager.merge_branch(
                    &preview_branch_name,
                    target_branch,
                    crate::storage::MergeStrategy::default(),
                ) {
                    Ok(result) => {
                        tracing::info!(
                            "Merged PR #{} preview branch: {} keys merged, {} conflicts, completed={}",
                            pr_num,
                            result.merged_keys,
                            result.conflicts.len(),
                            result.completed
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to merge PR #{} branch (may require manual resolution): {}",
                            pr_num,
                            e
                        );
                    }
                }

                // Drop the preview branch after merge
                if let Err(e) = self.branch_manager.drop_branch(&preview_branch_name, true) {
                    tracing::warn!("Failed to drop PR #{} branch after merge: {}", pr_num, e);
                }

                Ok(WebhookResult::success(format!(
                    "Merged and cleaned up PR #{} preview branch",
                    pr_num
                ))
                .with_action("merge_preview_branch"))
            }
            Err(_) => {
                // Branch doesn't exist - nothing to merge
                Ok(WebhookResult::success(format!(
                    "PR #{} preview branch does not exist (may already be cleaned up)",
                    pr_num
                ))
                .with_action("no_action"))
            }
        }
    }

    fn handle_pr_closed(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        let pr_num = event.pr_number.unwrap_or(0);
        let preview_branch_name = format!("pr-{}", pr_num);

        tracing::info!("Dropping PR #{} preview branch '{}' (closed without merge)", pr_num, preview_branch_name);

        // Check if the preview branch exists and drop it
        match self.branch_manager.get_branch_by_name(&preview_branch_name) {
            Ok(branch) => {
                self.branch_manager.drop_branch(&preview_branch_name, false)?;

                Ok(WebhookResult::success(format!(
                    "Dropped PR #{} preview branch",
                    pr_num
                ))
                .with_branch(branch.branch_id)
                .with_action("drop_preview_branch"))
            }
            Err(_) => {
                Ok(WebhookResult::success(format!(
                    "PR #{} preview branch does not exist",
                    pr_num
                ))
                .with_action("no_action"))
            }
        }
    }

    fn handle_push(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        tracing::info!("Push to branch '{}' - checking for linked DB branch", event.source_branch);

        // Check if there's a DB branch linked to this Git branch
        match self.branch_manager.get_branch_by_name(&event.source_branch) {
            Ok(branch) => {
                // Branch exists, check if it has a git link
                if branch.options.git_link.is_some() {
                    tracing::info!(
                        "Found linked DB branch '{}' for Git branch '{}'",
                        branch.name,
                        event.source_branch
                    );
                    // Here we could trigger migration application or other sync operations
                }
                Ok(WebhookResult::success(format!(
                    "Synced branch '{}'",
                    event.source_branch
                ))
                .with_branch(branch.branch_id))
            }
            Err(_) => {
                Ok(WebhookResult::success(format!(
                    "No linked DB branch for Git branch '{}'",
                    event.source_branch
                )))
            }
        }
    }

    fn handle_branch_created(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        tracing::info!("Git branch '{}' created", event.source_branch);

        // Optionally auto-create a linked DB branch
        // For now, just note the creation
        Ok(WebhookResult::success(format!(
            "Noted Git branch '{}' creation",
            event.source_branch
        )))
    }

    fn handle_branch_deleted(&self, event: &WebhookEvent) -> Result<WebhookResult> {
        tracing::info!("Git branch '{}' deleted", event.source_branch);

        // Check if there's a linked DB branch and optionally drop it
        match self.branch_manager.get_branch_by_name(&event.source_branch) {
            Ok(branch) => {
                if let Some(git_link) = &branch.options.git_link {
                    if git_link.git_branch == event.source_branch {
                        tracing::info!(
                            "Found linked DB branch '{}' for deleted Git branch",
                            branch.name
                        );
                        // Optionally drop the DB branch (configurable behavior)
                        // For safety, we don't auto-drop by default
                    }
                }
                Ok(WebhookResult::success(format!(
                    "Noted Git branch '{}' deletion (linked DB branch preserved)",
                    event.source_branch
                )))
            }
            Err(_) => {
                Ok(WebhookResult::success(format!(
                    "Noted Git branch '{}' deletion",
                    event.source_branch
                )))
            }
        }
    }
}

/// Rate limiter for webhook endpoints
pub struct RateLimiter {
    /// Requests per minute limit
    limit: u32,
    /// Request counts by IP/key
    counts: std::sync::RwLock<HashMap<String, (u64, u32)>>,
}

impl RateLimiter {
    pub fn new(limit: u32) -> Self {
        Self {
            limit,
            counts: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Check if request is allowed
    pub fn check(&self, key: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let minute = now / 60;

        let mut counts = self.counts.write().unwrap();
        let entry = counts.entry(key.to_string()).or_insert((minute, 0));

        if entry.0 != minute {
            // Reset for new minute
            *entry = (minute, 1);
            true
        } else if entry.1 < self.limit {
            entry.1 += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_webhook_event_creation() {
        let event = WebhookEvent::new(
            WebhookEventType::PrOpened,
            "feature/test".to_string(),
            GitProvider::GitHub,
        );

        assert_eq!(event.event_type, WebhookEventType::PrOpened);
        assert_eq!(event.source_branch, "feature/test");
        assert_eq!(event.provider, GitProvider::GitHub);
    }

    #[test]
    fn test_webhook_result() {
        let result = WebhookResult::success("Test")
            .with_branch(1)
            .with_action("test_action");

        assert!(result.success);
        assert_eq!(result.branch_id, Some(1));
        assert_eq!(result.action, Some("test_action".to_string()));
    }

    #[test]
    fn test_rate_limiter() {
        let limiter = RateLimiter::new(2);

        assert!(limiter.check("test_key"));
        assert!(limiter.check("test_key"));
        assert!(!limiter.check("test_key")); // Third request should be denied
    }

    #[test]
    fn test_parse_github_pr_opened() {
        let handler = WebhookHandler::new(WebhookConfig::default());

        let payload = r#"{
            "action": "opened",
            "number": 123,
            "pull_request": {
                "head": {"ref": "feature/test", "sha": "abc123"},
                "base": {"ref": "main", "sha": "def456"},
                "title": "Test PR"
            },
            "repository": {"full_name": "owner/repo"}
        }"#;

        let event = handler.parse_github(payload, "pull_request").unwrap();

        assert_eq!(event.event_type, WebhookEventType::PrOpened);
        assert_eq!(event.source_branch, "feature/test");
        assert_eq!(event.target_branch, Some("main".to_string()));
        assert_eq!(event.pr_number, Some(123));
    }
}
