//! Protocol Integration Tests
//!
//! End-to-end tests for protocol server integration
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_lite::{Config, storage::StorageEngine};
use heliosdb_lite::protocols::{
    ServerManager,
    ServerManagerConfig,
    oracle::OracleServerConfig,
};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_server_manager_creation() {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());

    let oracle_config = OracleServerConfig {
        listen_addr: "127.0.0.1".to_string(),
        port: 15210, // Use non-standard port for testing
        max_connections: 10,
    };

    let server_config = ServerManagerConfig::oracle_only(oracle_config);
    let manager = ServerManager::new(storage, server_config);

    // Check health
    let health = manager.health_check();
    assert!(health.oracle_enabled);
    assert!(!health.postgres_enabled);
    assert!(health.is_healthy());
}

#[tokio::test]
async fn test_server_manager_config() {
    let oracle_config = OracleServerConfig {
        listen_addr: "0.0.0.0".to_string(),
        port: 1521,
        max_connections: 100,
    };

    let config = ServerManagerConfig::oracle_only(oracle_config.clone());
    assert!(config.enable_oracle);
    assert!(!config.enable_postgres);
    assert_eq!(config.oracle_config.port, 1521);
    assert_eq!(config.oracle_config.max_connections, 100);
}

#[tokio::test]
async fn test_dual_protocol_config() {
    let oracle_config = OracleServerConfig::default();

    let config = ServerManagerConfig::dual_protocol(oracle_config, 5432);
    assert!(config.enable_oracle);
    assert!(config.enable_postgres); // Marked as enabled (even though not implemented)
    assert_eq!(config.postgres_port, 5432);
}

#[tokio::test]
async fn test_server_startup_shutdown() {
    let config = Config::in_memory();
    let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());

    let oracle_config = OracleServerConfig {
        listen_addr: "127.0.0.1".to_string(),
        port: 15211, // Different port to avoid conflicts
        max_connections: 10,
    };

    let server_config = ServerManagerConfig::oracle_only(oracle_config);
    let manager = ServerManager::new(storage, server_config);

    // Start server in background
    let server_task = tokio::spawn(async move {
        manager.start().await
    });

    // Give server time to start
    sleep(Duration::from_millis(100)).await;

    // Server is running - we can't easily test without a real client
    // For now, just verify the task is still running
    assert!(!server_task.is_finished());

    // TODO: Add actual client connection test when Oracle client is implemented
    // For now, we abort the server task
    server_task.abort();
}

#[test]
fn test_oracle_server_config() {
    let config = OracleServerConfig::default();
    assert_eq!(config.listen_addr, "127.0.0.1");
    assert_eq!(config.port, 1521);
    assert_eq!(config.max_connections, 100);
}

#[test]
fn test_oracle_server_custom_config() {
    let config = OracleServerConfig {
        listen_addr: "0.0.0.0".to_string(),
        port: 1522,
        max_connections: 50,
    };
    assert_eq!(config.listen_addr, "0.0.0.0");
    assert_eq!(config.port, 1522);
    assert_eq!(config.max_connections, 50);
}

// Integration test: Verify all protocol components compile together
#[test]
fn test_protocol_module_exports() {
    // This test ensures all exports are available
    use heliosdb_lite::protocols::{
        // Adapters
        StorageAdapter,
        QueryExecutorAdapter,
        PubSubAdapter,
        LiteStorageAdapter,
        LiteQueryExecutorAdapter,
        PubSubManager,
        ConnectionPool,
        PoolConfig,
        // Oracle
        OracleServer,
        OracleTranslator,
        OracleProtocolHandler,
        // Server Manager
        ServerManager,
        ServerManagerConfig,
        ServerHealth,
    };

    // Just ensuring they compile
    let _: Option<Box<dyn StorageAdapter>> = None;
    let _: Option<Box<dyn QueryExecutorAdapter>> = None;
    let _: Option<Box<dyn PubSubAdapter>> = None;
}

#[test]
fn test_storage_adapter_integration() {
    use heliosdb_lite::protocols::{LiteStorageAdapter, StorageAdapter};

    let config = Config::in_memory();
    let engine = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
    let adapter = LiteStorageAdapter::new(engine);

    // Test basic operations
    adapter.put(b"test_key", b"test_value").unwrap();
    let value = adapter.get(b"test_key").unwrap();
    assert_eq!(value, Some(b"test_value".to_vec()));

    adapter.delete(b"test_key").unwrap();
    let value = adapter.get(b"test_key").unwrap();
    assert_eq!(value, None);
}

#[test]
fn test_query_executor_adapter_integration() {
    use heliosdb_lite::protocols::{LiteQueryExecutorAdapter, QueryExecutorAdapter};

    let config = Config::in_memory();
    let engine = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
    let adapter = LiteQueryExecutorAdapter::new(engine);

    // Test table creation
    let result = adapter.execute_query("CREATE TABLE test_table (id INT, name TEXT)");
    assert!(result.is_ok());

    // Test insert
    let result = adapter.execute_query("INSERT INTO test_table (id, name) VALUES (1, 'Alice')");
    assert!(result.is_ok());

    // Test select
    let result = adapter.execute_query("SELECT * FROM test_table");
    assert!(result.is_ok());
    let rows = result.unwrap();
    assert_eq!(rows.rows.len(), 1);
}

#[test]
fn test_connection_pool_integration() {
    use heliosdb_lite::protocols::{ConnectionPool, PoolConfig};
    use std::time::Duration;

    let pool_config = PoolConfig {
        min_size: 2,
        max_size: 10,
        connection_timeout: Duration::from_secs(5),
        db_path: None,  // In-memory
        db_config: Config::in_memory(),
    };

    let pool = ConnectionPool::new(pool_config).unwrap();

    // Get connection
    let conn = pool.get();
    assert!(conn.is_ok());

    // Check pool stats
    let stats = pool.stats();
    assert!(stats.total_connections >= 1);
}
