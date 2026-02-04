//! Test Utilities for Sync Protocol Testing
//!
//! This module provides comprehensive test utilities for integration testing,
//! unit testing, and benchmarking the sync protocol components.
//!
//! # Features
//!
//! - Mock servers and clients for testing
//! - Test data generators
//! - Async test helpers
//! - Database setup utilities
//! - Assertion helpers for sync state

use super::{
    ChangeLog, ChangeLogEntry, ChangeLogImpl, ChangeType, ConflictChangeEntry,
    ConflictChangeOperation, ConflictDetector, ConflictResolutionV2 as ConflictResolution,
    RowDelta, SyncClient, SyncConfig, SyncServer, VectorClock,
};
use crate::types::Schema;
use rocksdb::{Options, DB};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;
use uuid::Uuid;

// ============================================================================
// Test Server
// ============================================================================

/// Test sync server with automatic cleanup and helper methods
pub struct TestSyncServer {
    server: Arc<Mutex<SyncServer>>,
    jwt_secret: Vec<u8>,
    port: u16,
}

impl TestSyncServer {
    /// Create a new test sync server with default configuration
    pub fn new() -> Self {
        let jwt_secret = b"test-sync-secret-key-for-testing-12345678".to_vec();
        let server = SyncServer::with_jwt_secret(&jwt_secret);

        Self {
            server: Arc::new(Mutex::new(server)),
            jwt_secret,
            port: 8080,
        }
    }

    /// Create a test server with custom JWT secret
    pub fn with_jwt_secret(secret: &[u8]) -> Self {
        let server = SyncServer::with_jwt_secret(secret);

        Self {
            server: Arc::new(Mutex::new(server)),
            jwt_secret: secret.to_vec(),
            port: 8080,
        }
    }

    /// Create a test server with tenant authorization
    pub fn with_tenants(tenants: Vec<String>) -> Self {
        let jwt_secret = b"test-sync-secret-key-for-testing-12345678".to_vec();
        let jwt_manager = super::JwtManager::new(&jwt_secret);
        let mut authorizer = super::Authorizer::new();

        for tenant in tenants {
            authorizer.add_tenant(tenant);
        }

        let server = SyncServer::with_auth(jwt_manager, authorizer);

        Self {
            server: Arc::new(Mutex::new(server)),
            jwt_secret,
            port: 8080,
        }
    }

    /// Set the server port (for display/logging purposes)
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Get the server port
    pub fn get_port(&self) -> u16 {
        self.port
    }

    /// Register a client and generate authentication tokens
    pub async fn register_client(
        &self,
        user_id: &str,
        tenant_id: &str,
        client_id: Uuid,
    ) -> super::TokenPair {
        let server = self.server.lock().await;
        server
            .generate_token_pair(user_id.to_string(), tenant_id.to_string(), client_id)
            .expect("Failed to generate token pair")
    }

    /// Generate a single access token
    pub async fn generate_token(
        &self,
        user_id: &str,
        tenant_id: &str,
        client_id: Uuid,
    ) -> String {
        let server = self.server.lock().await;
        server
            .generate_token(user_id.to_string(), tenant_id.to_string(), client_id)
            .expect("Failed to generate token")
    }

    /// Add a tenant to the authorizer
    pub async fn add_tenant(&self, tenant_id: String) {
        let mut server = self.server.lock().await;
        server.add_tenant(tenant_id);
    }

    /// Remove a tenant from the authorizer
    pub async fn remove_tenant(&self, tenant_id: &str) -> bool {
        let mut server = self.server.lock().await;
        server.remove_tenant(tenant_id)
    }

    /// Handle a sync request from a client
    pub async fn handle_sync_request(
        &self,
        request: super::SyncRequest,
        token: &str,
    ) -> Result<super::SyncResponse, super::SyncError> {
        let mut server = self.server.lock().await;
        server.handle_sync_request(request, token).await
    }

    /// Handle client deltas
    pub async fn handle_client_deltas(
        &self,
        client_id: Uuid,
        deltas: Vec<RowDelta>,
        token: &str,
    ) -> Result<super::Acknowledgment, super::SyncError> {
        let mut server = self.server.lock().await;
        server.handle_client_deltas(client_id, deltas, token).await
    }

    /// Get the server's JWT secret (for testing token validation)
    pub fn jwt_secret(&self) -> &[u8] {
        &self.jwt_secret
    }
}

impl Default for TestSyncServer {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Test Client
// ============================================================================

/// Test sync client with helper methods for testing
pub struct TestSyncClient {
    client: SyncClient,
    client_id: Uuid,
    server_url: String,
}

impl TestSyncClient {
    /// Create a new test client with default configuration
    pub fn new(client_id: Uuid, server_url: &str) -> Self {
        let config = SyncConfig {
            server_url: server_url.to_string(),
            client_id,
            sync_interval: std::time::Duration::from_secs(30),
            retry_interval: std::time::Duration::from_secs(5),
            max_batch_size: 1000,
            enable_compression: true,
            enable_e2e_encryption: false,
        };

        let client = SyncClient::new(config).expect("Failed to create sync client");

        Self {
            client,
            client_id,
            server_url: server_url.to_string(),
        }
    }

    /// Create a test client with custom configuration
    pub fn with_config(config: SyncConfig) -> Self {
        let client_id = config.client_id;
        let server_url = config.server_url.clone();
        let client = SyncClient::new(config).expect("Failed to create sync client");

        Self {
            client,
            client_id,
            server_url,
        }
    }

    /// Get the client ID
    pub fn client_id(&self) -> Uuid {
        self.client_id
    }

    /// Get the server URL
    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    /// Set authentication tokens
    pub fn set_tokens(&mut self, tokens: super::TokenPair) {
        self.client.set_tokens(tokens);
    }

    /// Set individual auth tokens
    pub fn set_auth_tokens(&mut self, access_token: String, refresh_token: String) {
        self.client.set_auth_tokens(access_token, refresh_token);
    }

    /// Check if authenticated
    pub fn is_authenticated(&self) -> bool {
        self.client.is_authenticated()
    }

    /// Enqueue a change for synchronization
    pub fn enqueue_change(&mut self, delta: RowDelta) -> Result<(), super::SyncError> {
        self.client.enqueue_change(delta)
    }

    /// Get current sync status
    pub fn status(&self) -> super::client::SyncStatus {
        self.client.status()
    }

    /// Get the internal client (for advanced testing)
    pub fn inner(&self) -> &SyncClient {
        &self.client
    }

    /// Get mutable access to the internal client
    pub fn inner_mut(&mut self) -> &mut SyncClient {
        &mut self.client
    }
}

// ============================================================================
// Test Data Generators
// ============================================================================

/// Create a test row delta with specified parameters
pub fn create_test_delta(table: &str, row_id: u64, data: Vec<u8>) -> RowDelta {
    use chrono::Utc;

    let mut delta = RowDelta {
        table: table.to_string(),
        operation: super::Operation::Insert,
        row_id: vec![row_id as u8],
        data,
        vector_clock: VectorClock::new(),
        timestamp: Utc::now(),
        checksum: 0,
    };

    delta.checksum = delta.calculate_checksum();
    delta
}

/// Create a test update delta
pub fn create_update_delta(table: &str, row_id: u64, data: Vec<u8>) -> RowDelta {
    use chrono::Utc;

    let mut delta = RowDelta {
        table: table.to_string(),
        operation: super::Operation::Update {
            columns: vec!["data".to_string()],
        },
        row_id: vec![row_id as u8],
        data,
        vector_clock: VectorClock::new(),
        timestamp: Utc::now(),
        checksum: 0,
    };

    delta.checksum = delta.calculate_checksum();
    delta
}

/// Create a test delete delta
pub fn create_delete_delta(table: &str, row_id: u64) -> RowDelta {
    use chrono::Utc;

    let mut delta = RowDelta {
        table: table.to_string(),
        operation: super::Operation::Delete,
        row_id: vec![row_id as u8],
        data: vec![],
        vector_clock: VectorClock::new(),
        timestamp: Utc::now(),
        checksum: 0,
    };

    delta.checksum = delta.calculate_checksum();
    delta
}

/// Create a test change entry for conflict detection
pub fn create_change_entry(
    node_id: Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    operation: ConflictChangeOperation,
) -> ConflictChangeEntry {
    let mut vc = VectorClock::new();
    vc.increment(node_id);

    ConflictChangeEntry {
        data: vec![1, 2, 3, 4, 5],
        timestamp,
        node_id,
        vector_clock: vc,
        operation,
    }
}

/// Create a test change entry with custom data
pub fn create_change_entry_with_data(
    node_id: Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    operation: ConflictChangeOperation,
    data: Vec<u8>,
) -> ConflictChangeEntry {
    let mut vc = VectorClock::new();
    vc.increment(node_id);

    ConflictChangeEntry {
        data,
        timestamp,
        node_id,
        vector_clock: vc,
        operation,
    }
}

/// Create a test change log entry
pub fn create_change_log_entry(
    lsn: u64,
    transaction_id: u64,
    change_type: ChangeType,
) -> ChangeLogEntry {
    ChangeLogEntry::new(lsn, transaction_id, change_type, VectorClock::new())
}

/// Generate multiple test deltas
pub fn generate_test_deltas(table: &str, count: usize, data_size: usize) -> Vec<RowDelta> {
    (0..count)
        .map(|i| create_test_delta(table, i as u64, vec![i as u8; data_size]))
        .collect()
}

// ============================================================================
// Database Setup Utilities
// ============================================================================

/// Create a test RocksDB instance with automatic cleanup
pub fn create_test_db() -> (Arc<DB>, TempDir) {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let mut opts = Options::default();
    opts.create_if_missing(true);
    let db = DB::open(&opts, temp_dir.path()).expect("Failed to open database");
    (Arc::new(db), temp_dir)
}

/// Create a test change log with database
pub fn create_test_change_log() -> (ChangeLogImpl, Arc<DB>, TempDir) {
    let (db, temp_dir) = create_test_db();
    let change_log = ChangeLogImpl::new(Arc::clone(&db)).expect("Failed to create change log");
    (change_log, db, temp_dir)
}

/// Populate a change log with test data
pub fn populate_change_log(
    change_log: &ChangeLogImpl,
    table: &str,
    count: usize,
    data_size: usize,
) -> Vec<u64> {
    let mut lsns = Vec::new();

    for i in 0..count {
        let change = ChangeType::Insert {
            table: table.to_string(),
            row_id: i as u64,
            data: vec![i as u8; data_size],
        };

        let lsn = change_log
            .append(i as u64, change, VectorClock::new())
            .expect("Failed to append change");

        lsns.push(lsn);
    }

    lsns
}

// ============================================================================
// Assertion Helpers
// ============================================================================

/// Assert that two vector clocks are equal
pub fn assert_vector_clocks_equal(vc1: &VectorClock, vc2: &VectorClock) {
    assert!(
        !vc1.is_concurrent(vc2),
        "Vector clocks should not be concurrent"
    );
    assert!(
        !vc1.happens_before(vc2),
        "vc1 should not happen before vc2"
    );
    assert!(
        !vc2.happens_before(vc1),
        "vc2 should not happen before vc1"
    );
}

/// Assert that vc1 happens before vc2
pub fn assert_happens_before(vc1: &VectorClock, vc2: &VectorClock) {
    assert!(
        vc1.happens_before(vc2),
        "vc1 should happen before vc2"
    );
    assert!(
        !vc2.happens_before(vc1),
        "vc2 should not happen before vc1"
    );
}

/// Assert that two vector clocks are concurrent
pub fn assert_concurrent(vc1: &VectorClock, vc2: &VectorClock) {
    assert!(
        vc1.is_concurrent(vc2),
        "Vector clocks should be concurrent"
    );
}

/// Assert that a delta has a valid checksum
pub fn assert_valid_checksum(delta: &RowDelta) {
    assert!(
        delta.verify_checksum(),
        "Delta checksum should be valid"
    );
}

/// Assert that a client has pending changes
pub fn assert_pending_changes(client: &TestSyncClient, expected: usize) {
    let status = client.status();
    assert_eq!(
        status.pending_changes, expected,
        "Expected {} pending changes, found {}",
        expected, status.pending_changes
    );
}

// ============================================================================
// Async Test Helpers
// ============================================================================

/// Run a test server for the duration of the async block
pub async fn with_test_server<F, Fut>(f: F)
where
    F: FnOnce(TestSyncServer) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let server = TestSyncServer::new();
    f(server).await;
}

/// Run a test server and client for the duration of the async block
pub async fn with_test_server_and_client<F, Fut>(f: F)
where
    F: FnOnce(TestSyncServer, TestSyncClient) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let server = TestSyncServer::new();
    let client_id = Uuid::new_v4();
    let client = TestSyncClient::new(client_id, "http://localhost:8080");

    f(server, client).await;
}

/// Setup a complete test environment with server, multiple clients, and authentication
pub async fn setup_test_environment(
    num_clients: usize,
) -> (TestSyncServer, Vec<(TestSyncClient, super::TokenPair)>) {
    let server = TestSyncServer::new();
    let mut clients = Vec::new();

    for i in 0..num_clients {
        let client_id = Uuid::new_v4();
        let mut client = TestSyncClient::new(client_id, "http://localhost:8080");
        let tokens = server
            .register_client(&format!("user{}", i), "tenant1", client_id)
            .await;
        client.set_tokens(tokens.clone());
        clients.push((client, tokens));
    }

    (server, clients)
}

// ============================================================================
// Conflict Detection Helpers
// ============================================================================

/// Create a conflict detector for testing
pub fn create_test_conflict_detector(
    resolution: ConflictResolution,
) -> ConflictDetector {
    ConflictDetector::new(resolution, Uuid::new_v4())
}

/// Create two concurrent change entries for conflict testing
pub fn create_concurrent_changes(
) -> (ConflictChangeEntry, ConflictChangeEntry, Uuid, Uuid) {
    let node1 = Uuid::new_v4();
    let node2 = Uuid::new_v4();

    let now = chrono::Utc::now();

    let mut local = create_change_entry(node1, now, ConflictChangeOperation::Update);
    let mut remote = create_change_entry(node2, now, ConflictChangeOperation::Update);

    // Make them concurrent
    local.vector_clock.increment(node1);
    remote.vector_clock.increment(node2);

    (local, remote, node1, node2)
}

/// Create a causally ordered pair of changes
pub fn create_causal_changes(
) -> (ConflictChangeEntry, ConflictChangeEntry, Uuid, Uuid) {
    let node1 = Uuid::new_v4();
    let node2 = Uuid::new_v4();

    let now = chrono::Utc::now();

    let local = create_change_entry(node1, now, ConflictChangeOperation::Update);
    let mut remote = create_change_entry(node2, now, ConflictChangeOperation::Update);

    // Make remote causally after local
    remote.vector_clock.merge(&local.vector_clock);
    remote.vector_clock.increment(node2);

    (local, remote, node1, node2)
}

// ============================================================================
// Performance Testing Helpers
// ============================================================================

/// Measure the time to execute an async operation
pub async fn measure_async<F, Fut, T>(f: F) -> (T, std::time::Duration)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let start = std::time::Instant::now();
    let result = f().await;
    let duration = start.elapsed();
    (result, duration)
}

/// Measure the time to execute a sync operation
pub fn measure_sync<F, T>(f: F) -> (T, std::time::Duration)
where
    F: FnOnce() -> T,
{
    let start = std::time::Instant::now();
    let result = f();
    let duration = start.elapsed();
    (result, duration)
}

// ============================================================================
// Test Module
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_test_delta() {
        let delta = create_test_delta("users", 1, vec![1, 2, 3]);
        assert_eq!(delta.table, "users");
        assert!(delta.verify_checksum());
    }

    #[test]
    fn test_generate_test_deltas() {
        let deltas = generate_test_deltas("users", 10, 100);
        assert_eq!(deltas.len(), 10);
        for delta in deltas {
            assert!(delta.verify_checksum());
        }
    }

    #[test]
    fn test_create_test_db() {
        let (db, _temp_dir) = create_test_db();
        assert!(db.get(b"test").is_ok());
    }

    #[test]
    fn test_create_test_change_log() {
        let (change_log, _db, _temp_dir) = create_test_change_log();
        assert_eq!(change_log.get_latest_lsn(), 0);
    }

    #[test]
    fn test_populate_change_log() {
        let (change_log, _db, _temp_dir) = create_test_change_log();
        let lsns = populate_change_log(&change_log, "users", 10, 100);

        assert_eq!(lsns.len(), 10);
        assert_eq!(change_log.get_latest_lsn(), 10);
    }

    #[test]
    fn test_create_concurrent_changes() {
        let (local, remote, _node1, _node2) = create_concurrent_changes();
        assert!(local.vector_clock.is_concurrent(&remote.vector_clock));
    }

    #[test]
    fn test_create_causal_changes() {
        let (local, remote, _node1, _node2) = create_causal_changes();
        assert!(local.vector_clock.happens_before(&remote.vector_clock));
    }

    #[tokio::test]
    async fn test_test_server_creation() {
        let server = TestSyncServer::new();
        assert_eq!(server.get_port(), 8080);
    }

    #[tokio::test]
    async fn test_test_client_creation() {
        let client_id = Uuid::new_v4();
        let client = TestSyncClient::new(client_id, "http://localhost:8080");
        assert_eq!(client.client_id(), client_id);
        assert_eq!(client.server_url(), "http://localhost:8080");
    }

    #[tokio::test]
    async fn test_setup_test_environment() {
        let (server, clients) = setup_test_environment(3).await;
        assert_eq!(clients.len(), 3);

        for (client, _tokens) in clients {
            assert!(client.is_authenticated());
        }
    }

    #[test]
    fn test_measure_sync() {
        let (result, duration) = measure_sync(|| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            42
        });

        assert_eq!(result, 42);
        assert!(duration.as_millis() >= 10);
    }
}
