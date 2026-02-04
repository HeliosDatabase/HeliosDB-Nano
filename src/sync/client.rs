//! Sync Client for HeliosDB-Lite v2.3.0
//!
//! Client-side coordinator for automatic synchronization with server using the
//! modern SyncMessage protocol with vector clocks, idempotency, and retry logic.
//!
//! # Features
//!
//! - Automatic pull/push synchronization
//! - Exponential backoff retry logic
//! - Concurrent sync prevention
//! - Background sync loop with configurable interval
//! - Heartbeat mechanism for server health monitoring
//! - Comprehensive error handling
//! - Authentication with JWT token refresh
//!
//! # Example
//!
//! ```ignore
//! use heliosdb_lite::sync::SyncClient;
//! use std::sync::Arc;
//!
//! let client = SyncClient::new(
//!     "client-1".to_string(),
//!     "http://localhost:8080".to_string(),
//!     storage,
//!     config,
//! )?;
//!
//! // Register with server
//! client.register().await?;
//!
//! // Start background sync
//! let handle = client.start_background_sync().await?;
//! ```

use super::{
    auth::{JwtManager, TokenPair},
    ChangeLogImpl as ChangeLog,
    delta_applicator::DeltaApplicator,
    protocol::{ChangeEntry, ChangeOperation, SyncMessage, PROTOCOL_VERSION},
    vector_clock::VectorClock,
    Result, SyncError,
};
use crate::storage::StorageEngine;
use parking_lot::RwLock;
use reqwest::{Client as HttpClient, StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Client synchronization configuration
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Pull interval in seconds (default: 30s)
    pub pull_interval_secs: u64,
    /// Push immediately after local commit (default: true)
    pub push_on_commit: bool,
    /// Maximum changes per request (default: 1000)
    pub batch_size: usize,
    /// Retry attempts for failures (default: 3)
    pub retry_attempts: u32,
    /// Initial retry delay in milliseconds (default: 1000ms)
    pub retry_initial_delay_ms: u64,
    /// Maximum retry delay in milliseconds (default: 30000ms)
    pub retry_max_delay_ms: u64,
    /// Connection timeout in seconds (default: 30s)
    pub connection_timeout_secs: u64,
    /// Request timeout in seconds (default: 60s)
    pub request_timeout_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            pull_interval_secs: 30,
            push_on_commit: true,
            batch_size: 1000,
            retry_attempts: 3,
            retry_initial_delay_ms: 1000,
            retry_max_delay_ms: 30000,
            connection_timeout_secs: 30,
            request_timeout_secs: 60,
        }
    }
}

/// Client state tracking
#[derive(Debug, Clone)]
struct ClientState {
    /// Last known server LSN
    last_known_lsn: u64,
    /// Last pull timestamp
    last_pull_time: SystemTime,
    /// Last push timestamp
    last_push_time: SystemTime,
    /// Whether a sync is currently in progress
    is_syncing: bool,
    /// Client's vector clock
    vector_clock: VectorClock,
    /// Background sync task handle (if running)
    bg_task_running: bool,
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            last_known_lsn: 0,
            last_pull_time: SystemTime::UNIX_EPOCH,
            last_push_time: SystemTime::UNIX_EPOCH,
            is_syncing: false,
            vector_clock: VectorClock::new(),
            bg_task_running: false,
        }
    }
}

/// Result of a pull operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullResult {
    /// Number of changes applied
    pub applied: usize,
    /// Number of conflicts detected
    pub conflicts: usize,
    /// Whether more changes are available
    pub has_more: bool,
    /// Server LSN after pull
    pub server_lsn: u64,
}

/// Result of a push operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushResult {
    /// Number of changes pushed
    pub pushed: usize,
    /// Number of conflicts detected
    pub conflicts: Vec<String>,
    /// Server LSN after push
    pub server_lsn: u64,
}

/// Combined sync result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    /// Pull result
    pub pull: PullResult,
    /// Push result
    pub push: PushResult,
    /// Total duration in milliseconds
    pub duration_ms: u64,
}

/// Sync client coordinator
pub struct SyncClient {
    /// Unique client identifier
    client_id: String,
    /// Server URL (e.g., http://localhost:8080)
    server_url: String,
    /// Local storage engine
    local_storage: Arc<StorageEngine>,
    /// Change log for tracking local changes
    change_log: Arc<ChangeLog>,
    /// Delta applicator for applying remote changes
    delta_applicator: Arc<DeltaApplicator>,
    /// HTTP client for communication
    http_client: HttpClient,
    /// Sync configuration
    config: SyncConfig,
    /// Client state (protected by RwLock)
    state: Arc<RwLock<ClientState>>,
    /// JWT manager for authentication
    jwt_manager: JwtManager,
    /// Access token (protected by RwLock)
    access_token: Arc<RwLock<Option<String>>>,
    /// Refresh token (protected by RwLock)
    refresh_token: Arc<RwLock<Option<String>>>,
    /// Node ID for vector clock
    node_id: Uuid,
}

impl SyncClient {
    /// Create a new sync client
    ///
    /// # Arguments
    ///
    /// * `client_id` - Unique identifier for this client
    /// * `server_url` - Server URL (e.g., http://localhost:8080)
    /// * `local_storage` - Local storage engine
    /// * `config` - Sync configuration
    ///
    /// # Returns
    ///
    /// New SyncClient instance
    ///
    /// # Errors
    ///
    /// Returns an error if initialization fails
    pub fn new(
        client_id: String,
        server_url: String,
        local_storage: Arc<StorageEngine>,
        config: SyncConfig,
    ) -> Result<Self> {
        // Initialize change log
        let change_log = Arc::new(
            ChangeLog::new(Arc::clone(&local_storage.db))
                .map_err(|e| SyncError::Storage(e.to_string()))?,
        );

        // Initialize delta applicator
        let conflict_detector = Arc::new(crate::sync::conflict::ConflictDetector::default());
        let delta_applicator = Arc::new(
            DeltaApplicator::new(Arc::clone(&local_storage), conflict_detector)
                .map_err(|e| SyncError::Storage(e.to_string()))?,
        );

        // Create HTTP client with timeouts
        let http_client = HttpClient::builder()
            .connect_timeout(Duration::from_secs(config.connection_timeout_secs))
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|e| SyncError::Network(e.to_string()))?;

        let node_id = Uuid::new_v4();

        info!(
            "Sync client initialized: client_id={}, server={}, node_id={}",
            client_id, server_url, node_id
        );

        Ok(Self {
            client_id,
            server_url,
            local_storage,
            change_log,
            delta_applicator,
            http_client,
            config,
            state: Arc::new(RwLock::new(ClientState::default())),
            jwt_manager: JwtManager::from_env_or_default(),
            access_token: Arc::new(RwLock::new(None)),
            refresh_token: Arc::new(RwLock::new(None)),
            node_id,
        })
    }

    /// Set authentication tokens
    pub fn set_tokens(&self, token_pair: TokenPair) {
        *self.access_token.write() = Some(token_pair.access_token);
        *self.refresh_token.write() = Some(token_pair.refresh_token);
        info!("Authentication tokens updated for client {}", self.client_id);
    }

    /// Register client with server
    ///
    /// Must be called before any sync operations.
    ///
    /// # Returns
    ///
    /// Ok on successful registration
    ///
    /// # Errors
    ///
    /// Returns an error if registration fails
    pub async fn register(&self) -> Result<()> {
        info!("Registering client {} with server", self.client_id);

        let state = self.state.read();
        let message = SyncMessage::RegisterClient {
            version: PROTOCOL_VERSION,
            client_id: self.client_id.clone(),
            last_known_lsn: state.last_known_lsn,
            vector_clock: state.vector_clock.clone(),
            metadata: std::collections::HashMap::new(),
        };
        drop(state);

        // Send registration request with retry
        self.send_message_with_retry("/api/v1/sync/register", &message)
            .await?;

        info!("Client {} registered successfully", self.client_id);
        Ok(())
    }

    /// Pull changes from server
    ///
    /// Fetches and applies changes from the server since the last known LSN.
    ///
    /// # Returns
    ///
    /// PullResult with statistics
    ///
    /// # Errors
    ///
    /// Returns an error if pull fails
    pub async fn pull(&self) -> Result<PullResult> {
        debug!("Starting pull operation for client {}", self.client_id);

        // Prevent concurrent pulls
        {
            let mut state = self.state.write();
            if state.is_syncing {
                return Err(SyncError::InvalidMessage(
                    "Sync already in progress".to_string(),
                ));
            }
            state.is_syncing = true;
        }

        // Ensure we release the lock when done
        let result = self.pull_internal().await;

        // Clear sync flag
        {
            let mut state = self.state.write();
            state.is_syncing = false;
        }

        result
    }

    /// Internal pull implementation
    async fn pull_internal(&self) -> Result<PullResult> {
        let message_id = Uuid::new_v4();
        let state = self.state.read();
        let since_lsn = state.last_known_lsn;
        drop(state);

        let request = SyncMessage::PullRequest {
            message_id,
            client_id: self.client_id.clone(),
            since_lsn,
            max_entries: self.config.batch_size,
            continuation_token: None,
        };

        // Send pull request with retry
        let response = self
            .send_message_with_retry("/api/v1/sync/pull", &request)
            .await?;

        // Parse response
        match response {
            SyncMessage::PullResponse {
                changes,
                server_lsn,
                has_more,
                vector_clock,
                ..
            } => {
                debug!(
                    "Received {} changes from server (LSN: {}, has_more: {})",
                    changes.len(),
                    server_lsn,
                    has_more
                );

                // Apply changes
                let apply_result = self
                    .delta_applicator
                    .apply_batch(
                        changes
                            .into_iter()
                            .map(|c| self.convert_protocol_change_to_delta(c))
                            .collect(),
                    )
                    .map_err(|e| SyncError::Storage(e.to_string()))?;

                // Update state
                {
                    let mut state = self.state.write();
                    state.last_known_lsn = server_lsn;
                    state.last_pull_time = SystemTime::now();
                    state.vector_clock.merge(&vector_clock);
                }

                Ok(PullResult {
                    applied: apply_result.applied.len(),
                    conflicts: apply_result.conflicts.len(),
                    has_more,
                    server_lsn,
                })
            }
            SyncMessage::SyncError { message, .. } => {
                Err(SyncError::InvalidMessage(format!("Server error: {}", message)))
            }
            _ => Err(SyncError::InvalidMessage(
                "Invalid response to PullRequest".to_string(),
            )),
        }
    }

    /// Push local changes to server
    ///
    /// Sends local changes that have not yet been synchronized.
    ///
    /// # Returns
    ///
    /// PushResult with statistics
    ///
    /// # Errors
    ///
    /// Returns an error if push fails
    pub async fn push(&self) -> Result<PushResult> {
        debug!("Starting push operation for client {}", self.client_id);

        let state = self.state.read();
        let since_lsn = state.last_known_lsn;
        let vector_clock = state.vector_clock.clone();
        drop(state);

        // Get local changes since last sync
        let local_changes = self
            .change_log
            .query_since_lsn(since_lsn, Some(self.config.batch_size))
            .map_err(|e| SyncError::Storage(e.to_string()))?;

        if local_changes.is_empty() {
            debug!("No local changes to push");
            return Ok(PushResult {
                pushed: 0,
                conflicts: vec![],
                server_lsn: since_lsn,
            });
        }

        debug!("Pushing {} local changes to server", local_changes.len());

        // Convert to protocol changes
        let protocol_changes: Vec<ChangeEntry> = local_changes
            .into_iter()
            .map(|c| self.convert_change_log_to_protocol(c))
            .collect();

        let message_id = Uuid::new_v4();
        let request = SyncMessage::PushChanges {
            message_id,
            client_id: self.client_id.clone(),
            changes: protocol_changes,
            vector_clock,
        };

        // Send push request with retry
        let response = self
            .send_message_with_retry("/api/v1/sync/push", &request)
            .await?;

        // Parse response
        match response {
            SyncMessage::PushAck {
                accepted_lsns,
                conflicts,
                server_lsn,
                vector_clock,
                ..
            } => {
                debug!(
                    "Push acknowledged: {} accepted, {} conflicts",
                    accepted_lsns.len(),
                    conflicts.len()
                );

                // Update state
                {
                    let mut state = self.state.write();
                    state.last_known_lsn = server_lsn;
                    state.last_push_time = SystemTime::now();
                    state.vector_clock.merge(&vector_clock);
                }

                Ok(PushResult {
                    pushed: accepted_lsns.len(),
                    conflicts: conflicts
                        .into_iter()
                        .map(|c| format!("{:?}", c.conflict_type))
                        .collect(),
                    server_lsn,
                })
            }
            SyncMessage::SyncError { message, .. } => {
                Err(SyncError::InvalidMessage(format!("Server error: {}", message)))
            }
            _ => Err(SyncError::InvalidMessage(
                "Invalid response to PushChanges".to_string(),
            )),
        }
    }

    /// Perform full synchronization (pull + push)
    ///
    /// # Returns
    ///
    /// SyncResult with combined statistics
    ///
    /// # Errors
    ///
    /// Returns an error if sync fails
    pub async fn sync(&self) -> Result<SyncResult> {
        let start = SystemTime::now();
        info!("Starting full sync for client {}", self.client_id);

        // Pull first
        let pull = self.pull().await?;

        // Then push
        let push = self.push().await?;

        let duration = start
            .elapsed()
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        info!(
            "Sync complete: pulled {} changes, pushed {} changes in {}ms",
            pull.applied, push.pushed, duration
        );

        Ok(SyncResult {
            pull,
            push,
            duration_ms: duration,
        })
    }

    /// Start background sync loop
    ///
    /// Spawns a tokio task that periodically pulls and pushes changes.
    ///
    /// # Returns
    ///
    /// JoinHandle for the background task
    ///
    /// # Errors
    ///
    /// Returns an error if task spawning fails
    pub async fn start_background_sync(&self) -> Result<JoinHandle<()>> {
        {
            let mut state = self.state.write();
            if state.bg_task_running {
                return Err(SyncError::InvalidMessage(
                    "Background sync already running".to_string(),
                ));
            }
            state.bg_task_running = true;
        }

        let client_clone = self.clone_for_background();
        let pull_interval = Duration::from_secs(self.config.pull_interval_secs);

        info!(
            "Starting background sync with interval of {}s",
            self.config.pull_interval_secs
        );

        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(pull_interval);

            loop {
                interval.tick().await;

                // Perform sync
                if let Err(e) = client_clone.sync().await {
                    error!("Background sync failed: {}", e);
                }

                // Send heartbeat
                if let Err(e) = client_clone.send_heartbeat().await {
                    error!("Heartbeat failed: {}", e);
                }
            }
        });

        Ok(handle)
    }

    /// Stop background sync
    ///
    /// Note: This only marks the state; the JoinHandle must be aborted separately.
    pub async fn stop_background_sync(&self) -> Result<()> {
        let mut state = self.state.write();
        state.bg_task_running = false;
        info!("Background sync stopped for client {}", self.client_id);
        Ok(())
    }

    /// Send heartbeat to server
    ///
    /// # Returns
    ///
    /// Ok on successful heartbeat
    ///
    /// # Errors
    ///
    /// Returns an error if heartbeat fails
    async fn send_heartbeat(&self) -> Result<()> {
        let state = self.state.read();
        let current_lsn = state.last_known_lsn;
        drop(state);

        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let heartbeat = SyncMessage::Heartbeat {
            client_id: self.client_id.clone(),
            timestamp,
            current_lsn,
        };

        debug!("Sending heartbeat to server");

        // Send without retry (heartbeats are optional)
        self.send_message("/api/v1/sync/heartbeat", &heartbeat)
            .await?;

        Ok(())
    }

    /// Send message with exponential backoff retry
    async fn send_message_with_retry(
        &self,
        endpoint: &str,
        message: &SyncMessage,
    ) -> Result<SyncMessage> {
        let mut retry_delay = Duration::from_millis(self.config.retry_initial_delay_ms);
        let max_delay = Duration::from_millis(self.config.retry_max_delay_ms);

        for attempt in 0..self.config.retry_attempts {
            match self.send_message(endpoint, message).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    if attempt + 1 < self.config.retry_attempts {
                        warn!(
                            "Request failed (attempt {}/{}): {}. Retrying in {:?}",
                            attempt + 1,
                            self.config.retry_attempts,
                            e,
                            retry_delay
                        );

                        tokio::time::sleep(retry_delay).await;

                        // Exponential backoff
                        retry_delay = std::cmp::min(retry_delay * 2, max_delay);
                    } else {
                        error!("Request failed after {} attempts: {}", self.config.retry_attempts, e);
                        return Err(e);
                    }
                }
            }
        }

        Err(SyncError::Network("All retry attempts failed".to_string()))
    }

    /// Send message to server (single attempt)
    async fn send_message(&self, endpoint: &str, message: &SyncMessage) -> Result<SyncMessage> {
        let url = format!("{}{}", self.server_url, endpoint);

        // Get auth token if available
        let token = self.access_token.read().clone();

        let mut request_builder = self.http_client.post(&url).json(message);

        // Add authentication header if token available
        if let Some(token) = token {
            request_builder = request_builder.header("Authorization", format!("Bearer {}", token));
        }

        let response = request_builder
            .send()
            .await
            .map_err(|e| SyncError::Network(e.to_string()))?;

        // Check status code
        let status = response.status();
        if !status.is_success() {
            if status == StatusCode::UNAUTHORIZED {
                // Try to refresh token
                if self.try_refresh_token().await.is_ok() {
                    // Retry with new token
                    return self.send_message(endpoint, message).await;
                }
                return Err(SyncError::Authentication);
            }

            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(SyncError::Network(format!(
                "HTTP {}: {}",
                status, error_text
            )));
        }

        // Parse response
        let sync_response: SyncMessage = response
            .json()
            .await
            .map_err(|e| SyncError::Serialization(e.to_string()))?;

        Ok(sync_response)
    }

    /// Try to refresh access token using refresh token
    async fn try_refresh_token(&self) -> Result<()> {
        let refresh_token = self.refresh_token.read();
        let refresh_token_str = refresh_token
            .as_ref()
            .ok_or(SyncError::Authentication)?
            .clone();
        drop(refresh_token);

        debug!("Attempting to refresh access token");

        // Validate and refresh token using JWT manager
        let new_access_token = self
            .jwt_manager
            .refresh_access_token(&refresh_token_str)
            .map_err(|_| SyncError::Authentication)?;

        *self.access_token.write() = Some(new_access_token);

        info!("Access token refreshed successfully");
        Ok(())
    }

    /// Convert protocol ChangeEntry to delta applicator format
    fn convert_protocol_change_to_delta(
        &self,
        change: ChangeEntry,
    ) -> crate::sync::delta_applicator::ChangeEntry {
        crate::sync::delta_applicator::ChangeEntry {
            lsn: change.lsn,
            table: change.table,
            operation: match change.operation {
                ChangeOperation::Insert => crate::sync::message::Operation::Insert,
                ChangeOperation::Update => {
                    crate::sync::message::Operation::Update { columns: vec![] }
                }
                ChangeOperation::Delete => crate::sync::message::Operation::Delete,
            },
            row_id: change.key,
            data: change.data,
            vector_clock: change.vector_clock,
            checksum: change.checksum,
            node_id: self.node_id,
        }
    }

    /// Convert change log entry to protocol format
    fn convert_change_log_to_protocol(
        &self,
        change: crate::sync::change_log::ChangeEntry,
    ) -> ChangeEntry {
        use crate::sync::change_log::ChangeType;

        let (operation, table, key, data) = match change.change_type {
            ChangeType::Insert { table, row_id, data } => {
                (ChangeOperation::Insert, table, row_id.to_be_bytes().to_vec(), data)
            }
            ChangeType::Update {
                table,
                row_id,
                new_data,
                ..
            } => {
                (ChangeOperation::Update, table, row_id.to_be_bytes().to_vec(), new_data)
            }
            ChangeType::Delete { table, row_id, .. } => {
                (ChangeOperation::Delete, table, row_id.to_be_bytes().to_vec(), vec![])
            }
            _ => {
                // DDL operations not supported in sync yet
                return ChangeEntry {
                    lsn: change.lsn,
                    table: "unsupported".to_string(),
                    operation: ChangeOperation::Insert,
                    key: vec![],
                    data: vec![],
                    vector_clock: change.vector_clock,
                    timestamp: chrono::DateTime::from_timestamp_millis(change.timestamp as i64)
                        .unwrap_or_default(),
                    checksum: 0,
                    compressed: false,
                };
            }
        };

        let mut entry = ChangeEntry {
            lsn: change.lsn,
            table,
            operation,
            key,
            data,
            vector_clock: change.vector_clock,
            timestamp: chrono::DateTime::from_timestamp_millis(change.timestamp as i64)
                .unwrap_or_default(),
            checksum: 0,
            compressed: false,
        };

        entry.checksum = entry.calculate_checksum();
        entry
    }

    /// Clone for background task (contains only Arc-wrapped data)
    fn clone_for_background(&self) -> Self {
        Self {
            client_id: self.client_id.clone(),
            server_url: self.server_url.clone(),
            local_storage: Arc::clone(&self.local_storage),
            change_log: Arc::clone(&self.change_log),
            delta_applicator: Arc::clone(&self.delta_applicator),
            http_client: self.http_client.clone(),
            config: self.config.clone(),
            state: Arc::clone(&self.state),
            jwt_manager: self.jwt_manager.clone(),
            access_token: Arc::clone(&self.access_token),
            refresh_token: Arc::clone(&self.refresh_token),
            node_id: self.node_id,
        }
    }

    /// Get client status
    pub fn status(&self) -> ClientStatus {
        let state = self.state.read();
        ClientStatus {
            client_id: self.client_id.clone(),
            last_known_lsn: state.last_known_lsn,
            is_syncing: state.is_syncing,
            bg_task_running: state.bg_task_running,
            last_pull_time: state.last_pull_time,
            last_push_time: state.last_push_time,
        }
    }
}

/// Client status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientStatus {
    pub client_id: String,
    pub last_known_lsn: u64,
    pub is_syncing: bool,
    pub bg_task_running: bool,
    pub last_pull_time: SystemTime,
    pub last_push_time: SystemTime,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    fn create_test_client() -> SyncClient {
        let config = Config::default();
        let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());

        SyncClient::new(
            "test-client".to_string(),
            "http://localhost:8080".to_string(),
            storage,
            SyncConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn test_client_creation() {
        let client = create_test_client();
        assert_eq!(client.client_id, "test-client");
        assert_eq!(client.server_url, "http://localhost:8080");
    }

    #[test]
    fn test_config_defaults() {
        let config = SyncConfig::default();
        assert_eq!(config.pull_interval_secs, 30);
        assert_eq!(config.batch_size, 1000);
        assert_eq!(config.retry_attempts, 3);
        assert!(config.push_on_commit);
    }

    #[test]
    fn test_client_status() {
        let client = create_test_client();
        let status = client.status();

        assert_eq!(status.client_id, "test-client");
        assert_eq!(status.last_known_lsn, 0);
        assert!(!status.is_syncing);
        assert!(!status.bg_task_running);
    }

    #[test]
    fn test_set_tokens() {
        let client = create_test_client();

        let token_pair = TokenPair::new(
            "access-token".to_string(),
            "refresh-token".to_string(),
            3600,
        );

        client.set_tokens(token_pair);

        let access = client.access_token.read();
        assert!(access.is_some());
        assert_eq!(access.as_ref().unwrap(), "access-token");
    }

    #[tokio::test]
    async fn test_concurrent_sync_prevention() {
        let client = Arc::new(create_test_client());

        // Set syncing flag
        {
            let mut state = client.state.write();
            state.is_syncing = true;
        }

        // Try to pull (should fail)
        let result = client.pull().await;
        assert!(result.is_err());

        match result {
            Err(SyncError::InvalidMessage(msg)) => {
                assert!(msg.contains("already in progress"));
            }
            _ => panic!("Expected InvalidMessage error"),
        }
    }

    #[tokio::test]
    async fn test_background_sync_already_running() {
        let client = create_test_client();

        // Mark as running
        {
            let mut state = client.state.write();
            state.bg_task_running = true;
        }

        // Try to start again
        let result = client.start_background_sync().await;
        assert!(result.is_err());
    }
}
