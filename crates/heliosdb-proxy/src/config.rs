//! Proxy Configuration
//!
//! Configuration management for HeliosDB Proxy.

use crate::{ProxyError, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Proxy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Listen address for client connections
    pub listen_address: String,
    /// Admin API address
    pub admin_address: String,
    /// Enable TR (Transaction Replay)
    pub tr_enabled: bool,
    /// TR mode
    pub tr_mode: TrMode,
    /// Connection pool configuration
    pub pool: PoolConfig,
    /// Load balancer configuration
    pub load_balancer: LoadBalancerConfig,
    /// Health check configuration
    pub health: HealthConfig,
    /// Backend nodes
    pub nodes: Vec<NodeConfig>,
    /// TLS configuration
    pub tls: Option<TlsConfig>,
    /// Write timeout during failover (seconds)
    /// When primary is unavailable, wait this long for a new primary before returning error
    #[serde(default = "default_write_timeout_secs")]
    pub write_timeout_secs: u64,
}

fn default_write_timeout_secs() -> u64 {
    30 // 30 seconds default write timeout during failover
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen_address: "0.0.0.0:5432".to_string(),
            admin_address: "0.0.0.0:9090".to_string(),
            tr_enabled: true,
            tr_mode: TrMode::Session,
            pool: PoolConfig::default(),
            load_balancer: LoadBalancerConfig::default(),
            health: HealthConfig::default(),
            nodes: Vec::new(),
            tls: None,
            write_timeout_secs: default_write_timeout_secs(),
        }
    }
}

impl ProxyConfig {
    /// Get write timeout as Duration
    pub fn write_timeout(&self) -> Duration {
        Duration::from_secs(self.write_timeout_secs)
    }

    /// Load configuration from file
    pub fn from_file(path: &str) -> Result<Self> {
        let path = Path::new(path);

        if !path.exists() {
            return Err(ProxyError::Config(format!(
                "Configuration file not found: {}",
                path.display()
            )));
        }

        let contents = std::fs::read_to_string(path)
            .map_err(|e| ProxyError::Config(format!("Failed to read config: {}", e)))?;

        let config: Self = toml::from_str(&contents)
            .map_err(|e| ProxyError::Config(format!("Failed to parse config: {}", e)))?;

        config.validate()?;

        Ok(config)
    }

    /// Add a node from host:port string
    pub fn add_node(&mut self, host_port: &str, role: &str) -> Result<()> {
        let parts: Vec<&str> = host_port.rsplitn(2, ':').collect();
        if parts.len() != 2 {
            return Err(ProxyError::Config(format!(
                "Invalid host:port format: {}",
                host_port
            )));
        }

        let port: u16 = parts[0].parse()
            .map_err(|_| ProxyError::Config(format!("Invalid port: {}", parts[0])))?;

        let host = parts[1].to_string();

        let role = match role {
            "primary" => NodeRole::Primary,
            "standby" => NodeRole::Standby,
            "replica" => NodeRole::ReadReplica,
            _ => return Err(ProxyError::Config(format!("Unknown role: {}", role))),
        };

        self.nodes.push(NodeConfig {
            host,
            port,
            http_port: default_http_port(),
            role,
            weight: 100,
            enabled: true,
            name: None,
        });

        Ok(())
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<()> {
        // Must have at least one node
        if self.nodes.is_empty() {
            return Err(ProxyError::Config("No backend nodes configured".to_string()));
        }

        // Must have a primary node
        let has_primary = self.nodes.iter().any(|n| n.role == NodeRole::Primary);
        if !has_primary {
            return Err(ProxyError::Config("No primary node configured".to_string()));
        }

        // Validate pool config
        if self.pool.max_connections < self.pool.min_connections {
            return Err(ProxyError::Config(
                "max_connections must be >= min_connections".to_string(),
            ));
        }

        Ok(())
    }

    /// Get primary node
    pub fn primary_node(&self) -> Option<&NodeConfig> {
        self.nodes.iter().find(|n| n.role == NodeRole::Primary && n.enabled)
    }

    /// Get standby nodes
    pub fn standby_nodes(&self) -> Vec<&NodeConfig> {
        self.nodes.iter()
            .filter(|n| n.role == NodeRole::Standby && n.enabled)
            .collect()
    }

    /// Get all enabled nodes
    pub fn enabled_nodes(&self) -> Vec<&NodeConfig> {
        self.nodes.iter().filter(|n| n.enabled).collect()
    }
}

/// TR (Transaction Replay) mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrMode {
    /// No transaction replay
    None,
    /// Re-establish session only
    Session,
    /// Re-execute SELECT queries
    Select,
    /// Full transaction replay
    Transaction,
}

impl Default for TrMode {
    fn default() -> Self {
        TrMode::Session
    }
}

/// Connection pool configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    /// Minimum connections per node
    pub min_connections: usize,
    /// Maximum connections per node
    pub max_connections: usize,
    /// Connection idle timeout (seconds)
    pub idle_timeout_secs: u64,
    /// Maximum connection lifetime (seconds)
    pub max_lifetime_secs: u64,
    /// Connection acquire timeout (seconds)
    pub acquire_timeout_secs: u64,
    /// Test connection before use
    pub test_on_acquire: bool,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min_connections: 2,
            max_connections: 100,
            idle_timeout_secs: 300,
            max_lifetime_secs: 1800,
            acquire_timeout_secs: 30,
            test_on_acquire: true,
        }
    }
}

impl PoolConfig {
    /// Get idle timeout as Duration
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs)
    }

    /// Get max lifetime as Duration
    pub fn max_lifetime(&self) -> Duration {
        Duration::from_secs(self.max_lifetime_secs)
    }

    /// Get acquire timeout as Duration
    pub fn acquire_timeout(&self) -> Duration {
        Duration::from_secs(self.acquire_timeout_secs)
    }
}

/// Load balancer configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadBalancerConfig {
    /// Routing strategy for read queries
    pub read_strategy: Strategy,
    /// Enable read/write splitting
    pub read_write_split: bool,
    /// Latency threshold for unhealthy marking (ms)
    pub latency_threshold_ms: u64,
}

impl Default for LoadBalancerConfig {
    fn default() -> Self {
        Self {
            read_strategy: Strategy::RoundRobin,
            read_write_split: true,
            latency_threshold_ms: 100,
        }
    }
}

/// Load balancing strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Round-robin across nodes
    RoundRobin,
    /// Weighted round-robin
    WeightedRoundRobin,
    /// Route to least loaded node
    LeastConnections,
    /// Route to lowest latency node
    LatencyBased,
    /// Random selection
    Random,
}

/// Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthConfig {
    /// Check interval (seconds)
    pub check_interval_secs: u64,
    /// Check timeout (seconds)
    pub check_timeout_secs: u64,
    /// Failures before marking unhealthy
    pub failure_threshold: u32,
    /// Successes before marking healthy
    pub success_threshold: u32,
    /// Health check query
    pub check_query: String,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            check_interval_secs: 5,
            check_timeout_secs: 3,
            failure_threshold: 3,
            success_threshold: 2,
            check_query: "SELECT 1".to_string(),
        }
    }
}

impl HealthConfig {
    /// Get check interval as Duration
    pub fn check_interval(&self) -> Duration {
        Duration::from_secs(self.check_interval_secs)
    }

    /// Get check timeout as Duration
    pub fn check_timeout(&self) -> Duration {
        Duration::from_secs(self.check_timeout_secs)
    }
}

/// Backend node configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Node host
    pub host: String,
    /// Node port (PostgreSQL protocol)
    pub port: u16,
    /// Node HTTP API port (for SQL API forwarding)
    /// Defaults to 8080 if not specified
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// Node role
    pub role: NodeRole,
    /// Weight for load balancing
    pub weight: u32,
    /// Whether node is enabled
    pub enabled: bool,
    /// Optional node name for logging
    pub name: Option<String>,
}

fn default_http_port() -> u16 {
    8080
}

impl NodeConfig {
    /// Get address string
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Get display name
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.host)
    }
}

/// Node role
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    /// Primary node (accepts writes)
    Primary,
    /// Standby node (can be promoted)
    Standby,
    /// Read replica (read-only, cannot be promoted)
    #[serde(rename = "replica")]
    ReadReplica,
}

/// TLS configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Enable TLS for client connections
    pub enabled: bool,
    /// Path to certificate file
    pub cert_path: String,
    /// Path to private key file
    pub key_path: String,
    /// Path to CA certificate (for client verification)
    pub ca_path: Option<String>,
    /// Require client certificates
    pub require_client_cert: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ProxyConfig::default();
        assert_eq!(config.listen_address, "0.0.0.0:5432");
        assert!(config.tr_enabled);
    }

    #[test]
    fn test_add_node() {
        let mut config = ProxyConfig::default();
        config.add_node("localhost:5432", "primary").unwrap();
        config.add_node("localhost:5433", "standby").unwrap();

        assert_eq!(config.nodes.len(), 2);
        assert!(config.primary_node().is_some());
        assert_eq!(config.standby_nodes().len(), 1);
    }

    #[test]
    fn test_validate_no_nodes() {
        let config = ProxyConfig::default();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_no_primary() {
        let mut config = ProxyConfig::default();
        config.add_node("localhost:5432", "standby").unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_success() {
        let mut config = ProxyConfig::default();
        config.add_node("localhost:5432", "primary").unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_pool_config_durations() {
        let config = PoolConfig::default();
        assert_eq!(config.idle_timeout(), Duration::from_secs(300));
        assert_eq!(config.max_lifetime(), Duration::from_secs(1800));
    }
}
