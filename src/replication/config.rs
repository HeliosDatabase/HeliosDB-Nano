//! HA Replication Configuration Types
//!
//! This module defines all configuration structures for the HA system:
//! - Replication roles (Primary, Standby, MultiPrimary)
//! - Sync modes (Sync, Async)
//! - Conflict resolution strategies
//! - Node and cluster configuration

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

// =============================================================================
// CORE ENUMS
// =============================================================================

/// Replication role of a node
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReplicationRole {
    /// Primary node - accepts writes, streams WAL to standbys
    Primary,
    /// Standby node - read-only, receives WAL from primary
    Standby,
    /// Multi-primary node - accepts writes, syncs with peers
    MultiPrimary,
    /// Shard coordinator - routes queries to shards
    Coordinator,
    /// Shard node - holds partition of data
    Shard,
}

/// Synchronization mode for replication
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMode {
    /// Synchronous - wait for replica acknowledgment
    Sync,
    /// Asynchronous - don't wait for replica acknowledgment
    Async,
    /// Semi-synchronous - wait for at least N replicas
    SemiSync { min_replicas: u32 },
}

impl Default for SyncMode {
    fn default() -> Self {
        Self::Async
    }
}

/// Conflict resolution strategy for multi-primary replication
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictStrategy {
    /// Last writer wins based on timestamp
    LastWriterWins,
    /// First writer wins - reject later updates
    FirstWriterWins,
    /// Use vector clock precedence
    VectorClockPrecedence,
    /// Custom resolution via user-defined function
    Custom,
    /// Queue for manual review
    ManualReview,
}

impl Default for ConflictStrategy {
    fn default() -> Self {
        Self::LastWriterWins
    }
}

/// Node health status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeHealth {
    /// Node is healthy and accepting connections
    Healthy,
    /// Node is lagging but operational
    Lagging,
    /// Node is unreachable
    Unreachable,
    /// Node is in recovery
    Recovering,
    /// Node has failed
    Failed,
}

/// TR (Transaction Replay) mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrMode {
    /// No replay - return error on failure
    None,
    /// Re-establish session only, lose transaction state
    Session,
    /// Re-execute SELECT queries after failover
    Select,
    /// Full transaction replay (Oracle TAC equivalent)
    Transaction,
}

impl Default for TrMode {
    fn default() -> Self {
        Self::Session
    }
}

// =============================================================================
// TIER 1: WARM STANDBY CONFIGURATION
// =============================================================================

/// Configuration for Tier 1 Warm Standby replication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WarmStandbyConfig {
    /// This node's role
    pub role: ReplicationRole,

    /// This node's unique identifier
    pub node_id: Uuid,

    /// This node's bind address for replication
    pub bind_addr: SocketAddr,

    /// Primary configuration (for standbys)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<PrimaryConfig>,

    /// Standby configurations (for primary)
    #[serde(default)]
    pub standbys: Vec<StandbyConfig>,

    /// WAL streaming settings
    #[serde(default)]
    pub wal_streaming: WalStreamingConfig,

    /// Failover settings
    #[serde(default)]
    pub failover: FailoverConfig,
}

/// Configuration for connecting to a primary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrimaryConfig {
    /// Primary host address
    pub host: String,
    /// Primary port
    pub port: u16,
    /// Connection timeout
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: Duration,
    /// Use TLS for connection
    #[serde(default)]
    pub use_tls: bool,
}

/// Configuration for a standby node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandbyConfig {
    /// Standby node ID
    pub node_id: Uuid,
    /// Standby host address
    pub host: String,
    /// Standby port
    pub port: u16,
    /// Synchronization mode for this standby
    #[serde(default)]
    pub sync_mode: SyncMode,
    /// Priority for failover (lower = higher priority)
    #[serde(default = "default_priority")]
    pub priority: u32,
}

/// WAL streaming configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalStreamingConfig {
    /// Maximum WAL segment size before rotation
    #[serde(default = "default_wal_segment_size")]
    pub segment_size: usize,

    /// Number of WAL segments to retain
    #[serde(default = "default_wal_retention")]
    pub retention_segments: u32,

    /// Streaming buffer size
    #[serde(default = "default_buffer_size")]
    pub buffer_size: usize,

    /// Batch size for WAL entries
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// Enable compression for WAL streaming
    #[serde(default = "default_true")]
    pub enable_compression: bool,

    /// Checkpoint interval
    #[serde(default = "default_checkpoint_interval")]
    pub checkpoint_interval: Duration,
}

impl Default for WalStreamingConfig {
    fn default() -> Self {
        Self {
            segment_size: default_wal_segment_size(),
            retention_segments: default_wal_retention(),
            buffer_size: default_buffer_size(),
            batch_size: default_batch_size(),
            enable_compression: true,
            checkpoint_interval: default_checkpoint_interval(),
        }
    }
}

/// Failover configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailoverConfig {
    /// Enable automatic failover
    #[serde(default)]
    pub auto_failover: bool,

    /// Health check interval
    #[serde(default = "default_health_check_interval")]
    pub health_check_interval: Duration,

    /// Number of failed health checks before failover
    #[serde(default = "default_failover_threshold")]
    pub failover_threshold: u32,

    /// Maximum allowed replication lag before failover (in LSN)
    #[serde(default = "default_max_lag")]
    pub max_replication_lag: u64,

    /// Timeout for failover completion
    #[serde(default = "default_failover_timeout")]
    pub failover_timeout: Duration,

    /// Require manual confirmation for failover
    #[serde(default)]
    pub require_confirmation: bool,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            auto_failover: false,
            health_check_interval: default_health_check_interval(),
            failover_threshold: default_failover_threshold(),
            max_replication_lag: default_max_lag(),
            failover_timeout: default_failover_timeout(),
            require_confirmation: false,
        }
    }
}

// =============================================================================
// TIER 2: MULTI-PRIMARY CONFIGURATION
// =============================================================================

/// Configuration for Tier 2 Multi-Primary replication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiPrimaryConfig {
    /// This node's unique identifier
    pub node_id: Uuid,

    /// This node's region identifier
    pub region_id: String,

    /// This node's bind address
    pub bind_addr: SocketAddr,

    /// Peer nodes in the cluster
    pub peers: Vec<PeerConfig>,

    /// Conflict resolution strategy
    #[serde(default)]
    pub conflict_strategy: ConflictStrategy,

    /// Sync settings
    #[serde(default)]
    pub sync: MultiPrimarySyncConfig,

    /// Branch replication settings
    #[serde(default)]
    pub branch_sync: BranchSyncConfig,
}

/// Configuration for a peer node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    /// Peer node ID
    pub node_id: Uuid,
    /// Peer region ID
    pub region_id: String,
    /// Peer host address
    pub host: String,
    /// Peer port
    pub port: u16,
    /// Use TLS for connection
    #[serde(default)]
    pub use_tls: bool,
}

/// Multi-primary sync configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiPrimarySyncConfig {
    /// Sync interval for periodic synchronization
    #[serde(default = "default_sync_interval")]
    pub sync_interval: Duration,

    /// Maximum batch size for sync
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// Enable vector clock tracking
    #[serde(default = "default_true")]
    pub vector_clocks: bool,

    /// Maximum convergence time before alerting
    #[serde(default = "default_convergence_timeout")]
    pub convergence_timeout: Duration,
}

impl Default for MultiPrimarySyncConfig {
    fn default() -> Self {
        Self {
            sync_interval: default_sync_interval(),
            batch_size: default_batch_size(),
            vector_clocks: true,
            convergence_timeout: default_convergence_timeout(),
        }
    }
}

/// Branch synchronization configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchSyncConfig {
    /// Branches to sync (empty = all branches)
    #[serde(default)]
    pub branches: Vec<String>,

    /// Exclude branches from sync
    #[serde(default)]
    pub exclude_branches: Vec<String>,

    /// Sync mode for branches
    #[serde(default)]
    pub sync_mode: SyncMode,
}

impl Default for BranchSyncConfig {
    fn default() -> Self {
        Self {
            branches: Vec::new(),
            exclude_branches: Vec::new(),
            sync_mode: SyncMode::Async,
        }
    }
}

// =============================================================================
// TIER 3: SHARDING CONFIGURATION
// =============================================================================

/// Configuration for Tier 3 Sharding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardingConfig {
    /// This node's role in the cluster
    pub role: ReplicationRole,

    /// This node's unique identifier
    pub node_id: Uuid,

    /// This node's bind address
    pub bind_addr: SocketAddr,

    /// Shard ID (for shard nodes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard_id: Option<u32>,

    /// Hash ring configuration
    #[serde(default)]
    pub hash_ring: HashRingConfig,

    /// Shard nodes (for coordinators)
    #[serde(default)]
    pub shards: Vec<ShardNodeConfig>,

    /// Cross-shard query settings
    #[serde(default)]
    pub cross_shard: CrossShardConfig,
}

/// Hash ring configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashRingConfig {
    /// Number of virtual nodes per physical node
    #[serde(default = "default_virtual_nodes")]
    pub virtual_nodes: u32,

    /// Hash function to use
    #[serde(default)]
    pub hash_function: HashFunction,

    /// Replication factor (copies per key)
    #[serde(default = "default_replication_factor")]
    pub replication_factor: u32,
}

impl Default for HashRingConfig {
    fn default() -> Self {
        Self {
            virtual_nodes: default_virtual_nodes(),
            hash_function: HashFunction::default(),
            replication_factor: default_replication_factor(),
        }
    }
}

/// Hash function for consistent hashing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HashFunction {
    /// xxHash (fast, good distribution)
    XxHash,
    /// MurmurHash3
    Murmur3,
    /// Blake3 (cryptographic, slower)
    Blake3,
}

impl Default for HashFunction {
    fn default() -> Self {
        Self::XxHash
    }
}

/// Configuration for a shard node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardNodeConfig {
    /// Shard ID
    pub shard_id: u32,
    /// Node ID
    pub node_id: Uuid,
    /// Host address
    pub host: String,
    /// Port
    pub port: u16,
    /// Key range start (inclusive)
    pub key_range_start: u64,
    /// Key range end (exclusive)
    pub key_range_end: u64,
}

/// Cross-shard query configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossShardConfig {
    /// Enable cross-shard queries
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Query timeout for cross-shard queries
    #[serde(default = "default_query_timeout")]
    pub query_timeout: Duration,

    /// Maximum parallel shard queries
    #[serde(default = "default_max_parallel")]
    pub max_parallel_queries: u32,

    /// Enable 2PC for cross-shard transactions
    #[serde(default)]
    pub enable_2pc: bool,
}

impl Default for CrossShardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            query_timeout: default_query_timeout(),
            max_parallel_queries: default_max_parallel(),
            enable_2pc: false,
        }
    }
}

// =============================================================================
// BRANCH-TO-SERVER REPLICATION
// =============================================================================

/// Configuration for branch-to-server replication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchReplicationConfig {
    /// Replication targets
    pub targets: Vec<BranchTarget>,
}

/// Configuration for a branch replication target
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchTarget {
    /// Branch name to replicate
    pub branch: String,

    /// Remote server host
    pub remote_host: String,

    /// Remote server port
    pub remote_port: u16,

    /// Authentication method
    pub auth: AuthMethod,

    /// Sync mode
    #[serde(default)]
    pub sync_mode: SyncMode,

    /// Maximum allowed lag (for async mode)
    #[serde(default = "default_max_lag_ms")]
    pub max_lag_ms: u64,
}

/// Authentication method for replication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthMethod {
    /// Pre-shared key exchange (for trusted environments)
    SecurePairing {
        /// Pairing key
        pairing_key: String,
    },
    /// Mutual TLS with client certificates
    Tls {
        /// Path to client certificate
        cert_path: PathBuf,
        /// Path to client key
        key_path: PathBuf,
        /// Path to CA certificate
        ca_path: PathBuf,
    },
    /// API token authentication
    Token {
        /// API token
        token: String,
    },
    /// OAuth2 authentication (future)
    OAuth2 {
        /// Client ID
        client_id: String,
        /// Client secret
        client_secret: String,
        /// Token endpoint
        token_endpoint: String,
    },
}

// =============================================================================
// HA-DEDUP CONFIGURATION
// =============================================================================

/// Configuration for HA-Dedup content-addressed deduplication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HaDedupConfig {
    /// Enable content-addressed deduplication
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Minimum blob size for deduplication (bytes)
    #[serde(default = "default_min_dedup_size")]
    pub min_blob_size: usize,

    /// Hash algorithm for content addressing
    #[serde(default)]
    pub hash_algorithm: ContentHashAlgorithm,

    /// Enable on-demand content fetching
    #[serde(default = "default_true")]
    pub lazy_fetch: bool,

    /// Content fetch timeout
    #[serde(default = "default_fetch_timeout")]
    pub fetch_timeout: Duration,
}

impl Default for HaDedupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_blob_size: default_min_dedup_size(),
            hash_algorithm: ContentHashAlgorithm::default(),
            lazy_fetch: true,
            fetch_timeout: default_fetch_timeout(),
        }
    }
}

/// Hash algorithm for content addressing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentHashAlgorithm {
    /// Blake3 (fast, cryptographic)
    Blake3,
    /// SHA-256
    Sha256,
}

impl Default for ContentHashAlgorithm {
    fn default() -> Self {
        Self::Blake3
    }
}

// =============================================================================
// HELIOS PROXY / TR CONFIGURATION
// =============================================================================

/// Configuration for HeliosProxy connection routing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Proxy bind address
    pub bind_addr: SocketAddr,

    /// Backend nodes
    pub backends: Vec<BackendConfig>,

    /// Connection pool settings
    #[serde(default)]
    pub connection_pool: ConnectionPoolConfig,

    /// Load balancer settings
    #[serde(default)]
    pub load_balancer: LoadBalancerConfig,

    /// Health check settings
    #[serde(default)]
    pub health_check: HealthCheckConfig,

    /// TR settings
    #[serde(default)]
    pub tr: TrConfig,
}

/// Backend node configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    /// Backend node ID
    pub node_id: Uuid,
    /// Host address
    pub host: String,
    /// Port
    pub port: u16,
    /// Role (primary or standby)
    pub role: ReplicationRole,
    /// Weight for load balancing
    #[serde(default = "default_weight")]
    pub weight: u32,
}

/// Connection pool configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionPoolConfig {
    /// Minimum connections per backend
    #[serde(default = "default_min_connections")]
    pub min_connections: u32,

    /// Maximum connections per backend
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,

    /// Connection idle timeout
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: Duration,

    /// Connection acquire timeout
    #[serde(default = "default_acquire_timeout")]
    pub acquire_timeout: Duration,
}

impl Default for ConnectionPoolConfig {
    fn default() -> Self {
        Self {
            min_connections: default_min_connections(),
            max_connections: default_max_connections(),
            idle_timeout: default_idle_timeout(),
            acquire_timeout: default_acquire_timeout(),
        }
    }
}

/// Load balancer configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadBalancerConfig {
    /// Load balancing strategy
    #[serde(default)]
    pub strategy: LoadBalanceStrategy,

    /// Enable read/write splitting
    #[serde(default = "default_true")]
    pub read_write_split: bool,

    /// Stick reads to the same backend within a session
    #[serde(default)]
    pub session_affinity: bool,
}

impl Default for LoadBalancerConfig {
    fn default() -> Self {
        Self {
            strategy: LoadBalanceStrategy::default(),
            read_write_split: true,
            session_affinity: false,
        }
    }
}

/// Load balancing strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoadBalanceStrategy {
    /// Round-robin across healthy backends
    RoundRobin,
    /// Least connections
    LeastConnections,
    /// Random selection
    Random,
    /// Weighted round-robin
    WeightedRoundRobin,
    /// Latency-based routing
    LatencyBased,
}

impl Default for LoadBalanceStrategy {
    fn default() -> Self {
        Self::RoundRobin
    }
}

/// Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    /// Health check interval
    #[serde(default = "default_health_check_interval")]
    pub interval: Duration,

    /// Health check timeout
    #[serde(default = "default_health_check_timeout")]
    pub timeout: Duration,

    /// Number of failures before marking unhealthy
    #[serde(default = "default_unhealthy_threshold")]
    pub unhealthy_threshold: u32,

    /// Number of successes before marking healthy
    #[serde(default = "default_healthy_threshold")]
    pub healthy_threshold: u32,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            interval: default_health_check_interval(),
            timeout: default_health_check_timeout(),
            unhealthy_threshold: default_unhealthy_threshold(),
            healthy_threshold: default_healthy_threshold(),
        }
    }
}

/// TR (Transaction Replay) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrConfig {
    /// Enable TR
    #[serde(default)]
    pub enabled: bool,

    /// Default TR mode for sessions
    #[serde(default)]
    pub default_mode: TrMode,

    /// Transaction journal retention
    #[serde(default = "default_journal_retention")]
    pub journal_retention: Duration,

    /// Maximum journal size per transaction (bytes)
    #[serde(default = "default_max_journal_size")]
    pub max_journal_size: usize,

    /// Enable cursor restoration
    #[serde(default = "default_true")]
    pub restore_cursors: bool,

    /// Enable session state migration
    #[serde(default = "default_true")]
    pub migrate_session_state: bool,
}

impl Default for TrConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_mode: TrMode::default(),
            journal_retention: default_journal_retention(),
            max_journal_size: default_max_journal_size(),
            restore_cursors: true,
            migrate_session_state: true,
        }
    }
}

// =============================================================================
// A/B TESTING CONFIGURATION
// =============================================================================

/// Configuration for branch-based A/B testing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbTestingConfig {
    /// Enable A/B testing router
    #[serde(default)]
    pub enabled: bool,

    /// Active experiments
    pub experiments: Vec<ExperimentConfig>,

    /// Default branch for unassigned users
    #[serde(default = "default_branch")]
    pub default_branch: String,
}

/// Configuration for an A/B experiment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentConfig {
    /// Experiment name
    pub name: String,

    /// Experiment branches (variant names)
    pub branches: Vec<String>,

    /// Assignment strategy
    pub assignment: AssignmentStrategy,

    /// Experiment enabled
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Experiment metadata
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// User assignment strategy for A/B testing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AssignmentStrategy {
    /// Assign based on user_id % divisor
    UserIdModulo {
        /// Divisor for modulo
        divisor: u32,
        /// Threshold for control group (< threshold = control)
        threshold: u32,
    },
    /// Assign based on percentage
    Percentage {
        /// Percentages for each branch (must sum to 100)
        percentages: Vec<u32>,
    },
    /// Assign based on user attribute
    Attribute {
        /// Attribute name
        attribute: String,
        /// Mapping of attribute values to branches
        mapping: HashMap<String, String>,
    },
    /// Random assignment with seed
    Random {
        /// Random seed
        seed: u64,
    },
}

// =============================================================================
// DEFAULT VALUE FUNCTIONS
// =============================================================================

fn default_connect_timeout() -> Duration {
    Duration::from_secs(10)
}

fn default_priority() -> u32 {
    100
}

fn default_wal_segment_size() -> usize {
    16 * 1024 * 1024 // 16 MB
}

fn default_wal_retention() -> u32 {
    64 // 64 segments = 1 GB
}

fn default_buffer_size() -> usize {
    1024 * 1024 // 1 MB
}

fn default_batch_size() -> usize {
    1000
}

fn default_true() -> bool {
    true
}

fn default_checkpoint_interval() -> Duration {
    Duration::from_secs(300) // 5 minutes
}

fn default_health_check_interval() -> Duration {
    Duration::from_secs(5)
}

fn default_failover_threshold() -> u32 {
    3
}

fn default_max_lag() -> u64 {
    10_000_000 // 10 MB of WAL
}

fn default_failover_timeout() -> Duration {
    Duration::from_secs(30)
}

fn default_sync_interval() -> Duration {
    Duration::from_secs(5)
}

fn default_convergence_timeout() -> Duration {
    Duration::from_secs(60)
}

fn default_virtual_nodes() -> u32 {
    150
}

fn default_replication_factor() -> u32 {
    3
}

fn default_query_timeout() -> Duration {
    Duration::from_secs(30)
}

fn default_max_parallel() -> u32 {
    10
}

fn default_max_lag_ms() -> u64 {
    5000 // 5 seconds
}

fn default_min_dedup_size() -> usize {
    4096 // 4 KB
}

fn default_fetch_timeout() -> Duration {
    Duration::from_secs(30)
}

fn default_weight() -> u32 {
    100
}

fn default_min_connections() -> u32 {
    2
}

fn default_max_connections() -> u32 {
    100
}

fn default_idle_timeout() -> Duration {
    Duration::from_secs(600) // 10 minutes
}

fn default_acquire_timeout() -> Duration {
    Duration::from_secs(30)
}

fn default_health_check_timeout() -> Duration {
    Duration::from_secs(5)
}

fn default_unhealthy_threshold() -> u32 {
    3
}

fn default_healthy_threshold() -> u32 {
    2
}

fn default_journal_retention() -> Duration {
    Duration::from_secs(3600) // 1 hour
}

fn default_max_journal_size() -> usize {
    100 * 1024 * 1024 // 100 MB
}

fn default_branch() -> String {
    "main".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_mode_default() {
        assert_eq!(SyncMode::default(), SyncMode::Async);
    }

    #[test]
    fn test_conflict_strategy_default() {
        assert_eq!(ConflictStrategy::default(), ConflictStrategy::LastWriterWins);
    }

    #[test]
    fn test_tr_mode_default() {
        assert_eq!(TrMode::default(), TrMode::Session);
    }

    #[test]
    fn test_wal_streaming_config_default() {
        let config = WalStreamingConfig::default();
        assert_eq!(config.segment_size, 16 * 1024 * 1024);
        assert!(config.enable_compression);
    }

    #[test]
    fn test_failover_config_default() {
        let config = FailoverConfig::default();
        assert!(!config.auto_failover);
        assert_eq!(config.failover_threshold, 3);
    }
}
