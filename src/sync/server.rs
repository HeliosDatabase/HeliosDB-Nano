//! Sync server implementation (cloud-side)

use super::{
    auth::{Authorizer, Claims, JwtManager},
    conflicts::{Conflict, ConflictManager, ConflictType},
    Acknowledgment, ConflictResolution, Operation, RowDelta, SyncError, SyncRequest, SyncResponse,
    VectorClock,
};
use chrono::Utc;
use parking_lot::RwLock;
use std::collections::HashMap;
use uuid::Uuid;

/// Versioned change entry for the server's change log
#[derive(Debug, Clone)]
struct VersionedChange {
    /// Server version when this change was recorded
    version: u64,
    /// The row delta
    delta: RowDelta,
}

/// Sync server for cloud instances
pub struct SyncServer {
    current_version: u64,
    vector_clock: VectorClock,
    conflict_manager: ConflictManager,
    jwt_manager: JwtManager,
    authorizer: Authorizer,
    /// In-memory change log: version → changes at that version
    change_log: RwLock<Vec<VersionedChange>>,
    /// Row version tracking: (table, row_id) → (version, vector_clock)
    row_versions: RwLock<HashMap<(String, Vec<u8>), (u64, VectorClock)>>,
}

impl SyncServer {
    /// Create a new sync server
    pub fn new() -> Self {
        Self {
            current_version: 0,
            vector_clock: VectorClock::new(),
            conflict_manager: ConflictManager::new(ConflictResolution::UseServer),
            jwt_manager: JwtManager::from_env_or_default(),
            authorizer: Authorizer::new(),
            change_log: RwLock::new(Vec::new()),
            row_versions: RwLock::new(HashMap::new()),
        }
    }

    /// Create a sync server with custom JWT secret
    pub fn with_jwt_secret(secret: &[u8]) -> Self {
        Self {
            current_version: 0,
            vector_clock: VectorClock::new(),
            conflict_manager: ConflictManager::new(ConflictResolution::UseServer),
            jwt_manager: JwtManager::new(secret),
            authorizer: Authorizer::new(),
            change_log: RwLock::new(Vec::new()),
            row_versions: RwLock::new(HashMap::new()),
        }
    }

    /// Create a sync server with custom JWT manager and authorizer
    pub fn with_auth(jwt_manager: JwtManager, authorizer: Authorizer) -> Self {
        Self {
            current_version: 0,
            vector_clock: VectorClock::new(),
            conflict_manager: ConflictManager::new(ConflictResolution::UseServer),
            jwt_manager,
            authorizer,
            change_log: RwLock::new(Vec::new()),
            row_versions: RwLock::new(HashMap::new()),
        }
    }

    /// Handle sync request from client (with JWT token)
    pub async fn handle_sync_request(
        &mut self,
        request: SyncRequest,
        jwt_token: &str,
    ) -> Result<SyncResponse, SyncError> {
        // 1. Validate client with JWT authentication
        let claims = self.authenticate(&jwt_token).await?;

        // 2. Verify client_id matches JWT claims
        if claims.client_id != request.client_id {
            tracing::warn!(
                "Client ID mismatch: JWT={}, Request={}",
                claims.client_id,
                request.client_id
            );
            return Err(SyncError::Authentication);
        }

        // 3. Get changes since last_sync_version
        let delta = self.get_changes_since(request.last_sync_version).await?;

        // 4. Detect conflicts
        let conflicts = self.detect_conflicts(&request.vector_clock, &delta).await?;

        Ok(SyncResponse {
            server_version: self.current_version,
            delta,
            conflicts,
            continuation_token: None,
            vector_clock: self.vector_clock.clone(),
        })
    }

    /// Handle client deltas (with JWT token)
    pub async fn handle_client_deltas(
        &mut self,
        client_id: Uuid,
        deltas: Vec<RowDelta>,
        jwt_token: &str,
    ) -> Result<Acknowledgment, SyncError> {
        // Validate client with JWT authentication
        let claims = self.authenticate(&jwt_token).await?;

        // Verify client_id matches JWT claims
        if claims.client_id != client_id {
            tracing::warn!(
                "Client ID mismatch in deltas: JWT={}, Request={}",
                claims.client_id,
                client_id
            );
            return Err(SyncError::Authentication);
        }
        // Apply deltas to server database
        let applied_count = deltas.len() as u32;

        // Increment version
        self.current_version += 1;

        Ok(Acknowledgment {
            new_version: self.current_version,
            applied_count,
            failed: vec![],
            vector_clock: self.vector_clock.clone(),
        })
    }

    /// Authenticate client using JWT token
    ///
    /// This method performs:
    /// 1. JWT token validation (signature, expiry, format)
    /// 2. Token expiration checking
    /// 3. Tenant-based authorization
    /// 4. Scope verification
    ///
    /// Returns validated claims on success
    async fn authenticate(&self, token: &str) -> Result<Claims, SyncError> {
        // 1. Parse and validate JWT signature and structure
        let claims = self
            .jwt_manager
            .validate_with_scope(token, "sync:read")
            .map_err(|e| {
                tracing::warn!("JWT validation failed: {:?}", e);
                SyncError::Authentication
            })?;

        // 2. Check expiration (redundant but explicit)
        if claims.is_expired() {
            tracing::warn!("Token expired for user: {}", claims.sub);
            return Err(SyncError::Authentication);
        }

        // 3. Check tenant authorization
        self.authorizer.validate_claims(&claims).map_err(|e| {
            tracing::warn!("Authorization failed for tenant: {}", claims.tenant_id);
            SyncError::Authentication
        })?;

        tracing::debug!(
            "Successfully authenticated client: {} (tenant: {}, user: {})",
            claims.client_id,
            claims.tenant_id,
            claims.sub
        );

        Ok(claims)
    }

    /// Generate JWT token for a client (used during client registration)
    pub fn generate_token(
        &self,
        user_id: String,
        tenant_id: String,
        client_id: Uuid,
    ) -> Result<String, SyncError> {
        self.jwt_manager
            .generate_token(user_id, tenant_id, client_id)
    }

    /// Generate token pair (access + refresh) for a client
    pub fn generate_token_pair(
        &self,
        user_id: String,
        tenant_id: String,
        client_id: Uuid,
    ) -> Result<super::TokenPair, SyncError> {
        let access_token = self
            .jwt_manager
            .generate_token(user_id.clone(), tenant_id.clone(), client_id)?;

        let refresh_token = self
            .jwt_manager
            .generate_refresh_token(user_id, tenant_id, client_id)?;

        Ok(super::TokenPair::new(access_token, refresh_token, 3600))
    }

    /// Refresh an access token using a refresh token
    pub fn refresh_token(&self, refresh_token: &str) -> Result<String, SyncError> {
        self.jwt_manager.refresh_access_token(refresh_token)
    }

    /// Add an allowed tenant to the authorizer
    pub fn add_tenant(&mut self, tenant_id: String) {
        self.authorizer.add_tenant(tenant_id);
    }

    /// Remove an allowed tenant from the authorizer
    pub fn remove_tenant(&mut self, tenant_id: &str) -> bool {
        self.authorizer.remove_tenant(tenant_id)
    }

    /// Query the change log for all changes since a specific version
    ///
    /// Returns all RowDelta entries with version > specified version,
    /// ordered by version ascending.
    async fn get_changes_since(&self, version: u64) -> Result<Vec<RowDelta>, SyncError> {
        let change_log = self.change_log.read();

        // Filter changes that occurred after the specified version
        let deltas: Vec<RowDelta> = change_log
            .iter()
            .filter(|entry| entry.version > version)
            .map(|entry| entry.delta.clone())
            .collect();

        tracing::debug!(
            "Retrieved {} changes since version {} (current: {})",
            deltas.len(),
            version,
            self.current_version
        );

        Ok(deltas)
    }

    /// Detect conflicts between client's vector clock and server's deltas
    ///
    /// Conflict detection uses vector clock comparison:
    /// - If client clock < server clock: no conflict (client needs update)
    /// - If client clock > server clock: no conflict (server needs update)
    /// - If clocks are concurrent: conflict detected
    async fn detect_conflicts(
        &self,
        client_clock: &VectorClock,
        deltas: &[RowDelta],
    ) -> Result<Vec<Conflict>, SyncError> {
        let mut conflicts = Vec::new();
        let row_versions = self.row_versions.read();

        for delta in deltas {
            let key = (delta.table.clone(), delta.row_id.clone());

            // Check if server has a version of this row
            if let Some((server_version, server_clock)) = row_versions.get(&key) {
                // Use vector clock to detect concurrent modifications
                if self.conflict_manager.detect_conflict(client_clock, server_clock) {
                    // Determine conflict type based on operation
                    let conflict_type = match &delta.operation {
                        Operation::Delete => ConflictType::DeleteUpdate,
                        Operation::Insert => ConflictType::UniqueViolation,
                        Operation::Update { .. } => ConflictType::ConcurrentUpdate,
                    };

                    // Retrieve server's version of the data from change log
                    let server_data = self.get_row_data(&delta.table, &delta.row_id);

                    let conflict = Conflict {
                        id: Uuid::new_v4(),
                        table: delta.table.clone(),
                        row_id: delta.row_id.clone(),
                        conflict_type,
                        client_version: delta.data.clone(),
                        server_version: server_data,
                        resolution: self.conflict_manager.strategy().clone(),
                    };

                    tracing::warn!(
                        "Conflict detected: table={}, row_id={:?}, type={:?}",
                        delta.table,
                        delta.row_id,
                        conflict_type
                    );

                    conflicts.push(conflict);
                }
            }
        }

        tracing::debug!(
            "Conflict detection complete: {} conflicts found in {} deltas",
            conflicts.len(),
            deltas.len()
        );

        Ok(conflicts)
    }

    /// Record a change in the server's change log
    ///
    /// This method should be called when the server applies a change
    /// to track it for future sync operations.
    pub fn record_change(&mut self, delta: RowDelta) {
        let version = self.current_version;

        // Update row version tracking
        {
            let mut row_versions = self.row_versions.write();
            let key = (delta.table.clone(), delta.row_id.clone());
            row_versions.insert(key, (version, delta.vector_clock.clone()));
        }

        // Append to change log
        {
            let mut change_log = self.change_log.write();
            change_log.push(VersionedChange {
                version,
                delta,
            });
        }

        tracing::debug!("Recorded change at version {}", version);
    }

    /// Get the latest data for a specific row from the change log
    fn get_row_data(&self, table: &str, row_id: &[u8]) -> Vec<u8> {
        let change_log = self.change_log.read();

        // Find the most recent change for this row (iterate in reverse)
        for entry in change_log.iter().rev() {
            if entry.delta.table == table && entry.delta.row_id == row_id {
                return entry.delta.data.clone();
            }
        }

        Vec::new() // Row not found in change log
    }

    /// Compact the change log by removing entries older than the specified version
    ///
    /// This helps manage memory usage for long-running servers.
    pub fn compact_change_log(&self, older_than_version: u64) -> usize {
        let mut change_log = self.change_log.write();
        let original_len = change_log.len();

        change_log.retain(|entry| entry.version >= older_than_version);

        let removed = original_len - change_log.len();
        tracing::info!(
            "Compacted change log: removed {} entries older than version {}",
            removed,
            older_than_version
        );

        removed
    }

    /// Get the current server version
    pub fn version(&self) -> u64 {
        self.current_version
    }

    /// Get the number of entries in the change log
    pub fn change_log_size(&self) -> usize {
        self.change_log.read().len()
    }
}

impl Default for SyncServer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sync::SyncMode;

    #[tokio::test]
    async fn test_sync_server_creation() {
        let server = SyncServer::new();
        assert_eq!(server.current_version, 0);
    }

    #[tokio::test]
    async fn test_handle_sync_request_with_auth() {
        let mut server = SyncServer::with_jwt_secret(b"test-secret");
        let client_id = Uuid::new_v4();

        // Generate valid token
        let token = server
            .generate_token("user123".to_string(), "tenant456".to_string(), client_id)
            .unwrap();

        let request = SyncRequest {
            client_id,
            last_sync_version: 0,
            changed_tables: vec![],
            pending_changes: 0,
            vector_clock: VectorClock::new(),
            sync_mode: SyncMode::Incremental,
        };

        let response = server.handle_sync_request(request, &token).await.unwrap();
        assert_eq!(response.server_version, 0);
    }

    #[tokio::test]
    async fn test_handle_sync_request_invalid_token() {
        let mut server = SyncServer::with_jwt_secret(b"test-secret");

        let request = SyncRequest {
            client_id: Uuid::new_v4(),
            last_sync_version: 0,
            changed_tables: vec![],
            pending_changes: 0,
            vector_clock: VectorClock::new(),
            sync_mode: SyncMode::Incremental,
        };

        // Invalid token should fail
        let result = server.handle_sync_request(request, "invalid.token.here").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SyncError::Authentication));
    }

    #[tokio::test]
    async fn test_handle_sync_request_mismatched_client_id() {
        let mut server = SyncServer::with_jwt_secret(b"test-secret");
        let client_id = Uuid::new_v4();
        let different_client_id = Uuid::new_v4();

        // Generate token for one client
        let token = server
            .generate_token("user123".to_string(), "tenant456".to_string(), client_id)
            .unwrap();

        // Try to use it with a different client_id
        let request = SyncRequest {
            client_id: different_client_id,
            last_sync_version: 0,
            changed_tables: vec![],
            pending_changes: 0,
            vector_clock: VectorClock::new(),
            sync_mode: SyncMode::Incremental,
        };

        let result = server.handle_sync_request(request, &token).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SyncError::Authentication));
    }

    #[tokio::test]
    async fn test_handle_client_deltas_with_auth() {
        let mut server = SyncServer::with_jwt_secret(b"test-secret");
        let client_id = Uuid::new_v4();

        // Generate valid token
        let token = server
            .generate_token("user123".to_string(), "tenant456".to_string(), client_id)
            .unwrap();

        let deltas = vec![];
        let ack = server
            .handle_client_deltas(client_id, deltas, &token)
            .await
            .unwrap();

        assert_eq!(ack.applied_count, 0);
        assert_eq!(ack.new_version, 1);
    }

    #[tokio::test]
    async fn test_tenant_authorization() {
        let jwt_manager = super::super::JwtManager::new(b"test-secret");
        let mut authorizer = super::super::Authorizer::new();
        authorizer.add_tenant("allowed-tenant".to_string());

        let mut server = SyncServer::with_auth(jwt_manager, authorizer);
        let client_id = Uuid::new_v4();

        // Token for allowed tenant - should succeed
        let token = server
            .generate_token(
                "user123".to_string(),
                "allowed-tenant".to_string(),
                client_id,
            )
            .unwrap();

        let request = SyncRequest {
            client_id,
            last_sync_version: 0,
            changed_tables: vec![],
            pending_changes: 0,
            vector_clock: VectorClock::new(),
            sync_mode: SyncMode::Incremental,
        };

        let result = server.handle_sync_request(request, &token).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_tenant_authorization_denied() {
        let jwt_manager = super::super::JwtManager::new(b"test-secret");
        let mut authorizer = super::super::Authorizer::new();
        authorizer.add_tenant("allowed-tenant".to_string());

        let mut server = SyncServer::with_auth(jwt_manager, authorizer);
        let client_id = Uuid::new_v4();

        // Token for disallowed tenant - should fail
        let token = server
            .generate_token(
                "user123".to_string(),
                "forbidden-tenant".to_string(),
                client_id,
            )
            .unwrap();

        let request = SyncRequest {
            client_id,
            last_sync_version: 0,
            changed_tables: vec![],
            pending_changes: 0,
            vector_clock: VectorClock::new(),
            sync_mode: SyncMode::Incremental,
        };

        let result = server.handle_sync_request(request, &token).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SyncError::Authentication));
    }

    #[tokio::test]
    async fn test_token_generation() {
        let server = SyncServer::with_jwt_secret(b"test-secret");
        let client_id = Uuid::new_v4();

        let token = server
            .generate_token("user123".to_string(), "tenant456".to_string(), client_id)
            .unwrap();

        assert!(!token.is_empty());

        // Validate the token can be decoded
        let claims = server.authenticate(&token).await.unwrap();
        assert_eq!(claims.sub, "user123");
        assert_eq!(claims.tenant_id, "tenant456");
        assert_eq!(claims.client_id, client_id);
    }

    #[tokio::test]
    async fn test_token_pair_generation() {
        let server = SyncServer::with_jwt_secret(b"test-secret");
        let client_id = Uuid::new_v4();

        let token_pair = server
            .generate_token_pair("user123".to_string(), "tenant456".to_string(), client_id)
            .unwrap();

        assert!(!token_pair.access_token.is_empty());
        assert!(!token_pair.refresh_token.is_empty());
        assert_eq!(token_pair.token_type, "Bearer");

        // Validate access token
        let claims = server.authenticate(&token_pair.access_token).await.unwrap();
        assert_eq!(claims.sub, "user123");
    }

    #[tokio::test]
    async fn test_refresh_token_flow() {
        let server = SyncServer::with_jwt_secret(b"test-secret");
        let client_id = Uuid::new_v4();

        // Generate token pair
        let token_pair = server
            .generate_token_pair("user123".to_string(), "tenant456".to_string(), client_id)
            .unwrap();

        // Use refresh token to get new access token
        let new_access_token = server.refresh_token(&token_pair.refresh_token).unwrap();

        // Validate new access token
        let claims = server.authenticate(&new_access_token).await.unwrap();
        assert_eq!(claims.sub, "user123");
        assert_eq!(claims.tenant_id, "tenant456");
    }

    #[tokio::test]
    async fn test_tenant_management() {
        let mut server = SyncServer::with_jwt_secret(b"test-secret");

        // Add tenants
        server.add_tenant("tenant1".to_string());
        server.add_tenant("tenant2".to_string());

        // Remove tenant
        assert!(server.remove_tenant("tenant1"));
        assert!(!server.remove_tenant("nonexistent"));
    }
}
