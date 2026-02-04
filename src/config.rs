//! Configuration for HeliosDB Lite
//!
//! This module provides comprehensive configuration options for the database.
//! Configuration can be loaded from TOML files or constructed programmatically.
//!
//! # Configuration Sections
//!
//! - [`StorageConfig`] - Database path, WAL, compression, caching
//! - [`EncryptionConfig`] - Encryption at rest with AES-256-GCM
//! - [`ServerConfig`] - Network settings, TLS, connection limits
//! - [`SessionConfig`] - Session timeouts and per-user limits
//! - [`LockConfig`] - Lock acquisition timeouts, deadlock detection
//! - [`DumpConfig`] - Backup scheduling and compression
//! - [`VectorConfig`] - Vector index settings (HNSW, PQ)
//!
//! # Example: TOML Configuration
//!
//! ```toml
//! [storage]
//! path = "./data"
//! memory_only = false
//! wal_enabled = true
//! compression = "Zstd"
//!
//! [server]
//! listen_addr = "0.0.0.0"
//! port = 5432
//! max_connections = 100
//!
//! [session]
//! timeout_secs = 3600
//! max_sessions_per_user = 10
//! ```
//!
//! # Example: Programmatic Configuration
//!
//! ```rust
//! use heliosdb_lite::Config;
//!
//! // In-memory database
//! let config = Config::in_memory();
//!
//! // Load from file
//! // let config = Config::from_file("heliosdb.toml")?;
//! ```

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Database configuration
///
/// The main configuration struct for HeliosDB Lite. All configuration
/// sections use sensible defaults and can be customized individually.
///
/// # Loading Configuration
///
/// ```rust,no_run
/// use heliosdb_lite::Config;
///
/// // Default configuration with file-based storage
/// let config = Config::default();
///
/// // In-memory database (for testing)
/// let config = Config::in_memory();
///
/// // Load from TOML file
/// let config = Config::from_file("heliosdb.toml")?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Validation
///
/// Call [`validate()`](Config::validate) to check all configuration values
/// before using the configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Storage configuration
    #[serde(default)]
    pub storage: StorageConfig,
    /// Encryption configuration
    #[serde(default)]
    pub encryption: EncryptionConfig,
    /// Server configuration
    #[serde(default)]
    pub server: ServerConfig,
    /// Performance configuration
    #[serde(default)]
    pub performance: PerformanceConfig,
    /// Audit configuration
    #[serde(default)]
    pub audit: crate::audit::AuditConfig,
    /// Optimizer configuration (v2.1)
    #[serde(default)]
    pub optimizer: OptimizerConfig,
    /// Authentication configuration (v2.1)
    #[serde(default)]
    pub authentication: AuthenticationConfig,
    /// Compression configuration (v2.1)
    #[serde(default)]
    pub compression: CompressionConfig,
    /// Materialized view configuration (v2.1)
    #[serde(default)]
    pub materialized_views: MaterializedViewConfig,
    /// Vector index configuration (v2.1)
    #[serde(default)]
    pub vector: VectorConfig,
    /// Sync configuration (v2.3 - Experimental)
    #[serde(default)]
    pub sync: SyncConfig,
    /// Session configuration (v3.1.0)
    #[serde(default)]
    pub session: SessionConfig,
    /// Lock configuration (v3.1.0)
    #[serde(default)]
    pub locks: LockConfig,
    /// Dump configuration (v3.1.0)
    #[serde(default)]
    pub dump: DumpConfig,
    /// Resource quota configuration (v3.1.0)
    #[serde(default)]
    pub resource_quotas: ResourceQuotaConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            storage: StorageConfig::default(),
            encryption: EncryptionConfig::default(),
            server: ServerConfig::default(),
            performance: PerformanceConfig::default(),
            audit: crate::audit::AuditConfig::default(),
            optimizer: OptimizerConfig::default(),
            authentication: AuthenticationConfig::default(),
            compression: CompressionConfig::default(),
            materialized_views: MaterializedViewConfig::default(),
            vector: VectorConfig::default(),
            sync: SyncConfig::default(),
            session: SessionConfig::default(),
            locks: LockConfig::default(),
            dump: DumpConfig::default(),
            resource_quotas: ResourceQuotaConfig::default(),
        }
    }
}

impl Config {
    /// Create in-memory configuration
    pub fn in_memory() -> Self {
        Self {
            storage: StorageConfig {
                path: None,
                memory_only: true,
                ..Default::default()
            },
            audit: crate::audit::AuditConfig::default(),
            ..Default::default()
        }
    }

    /// Load configuration from file
    pub fn from_file(path: impl AsRef<std::path::Path>) -> crate::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)
            .map_err(|e| crate::Error::config(format!("Failed to parse config: {}", e)))?;
        Ok(config)
    }

    /// Save configuration to file
    pub fn save_to_file(&self, path: impl AsRef<std::path::Path>) -> crate::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| crate::Error::config(format!("Failed to serialize config: {}", e)))?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Validate all configuration sections
    pub fn validate(&self) -> crate::Result<()> {
        self.session.validate()?;
        self.locks.validate()?;
        self.dump.validate()?;
        self.resource_quotas.validate()?;
        Ok(())
    }
}

/// Storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Database path (None for in-memory)
    pub path: Option<PathBuf>,
    /// Memory-only mode
    pub memory_only: bool,
    /// Write-ahead log enabled
    pub wal_enabled: bool,
    /// WAL synchronization mode (sync, async, or group_commit)
    pub wal_sync_mode: WalSyncModeConfig,
    /// Maximum memory for cache (bytes)
    pub cache_size: usize,
    /// Compression enabled
    pub compression: CompressionType,
    /// Automatic time-travel versioning enabled (default: true)
    ///
    /// When enabled, all insert/update operations automatically create
    /// versioned snapshots for time-travel queries. This enables AS OF
    /// TIMESTAMP/TRANSACTION/SCN queries with zero configuration.
    ///
    /// Set to false to disable automatic versioning and reduce write overhead
    /// for workloads that don't require time-travel queries.
    pub time_travel_enabled: bool,
    /// Query timeout in milliseconds (None for unlimited)
    ///
    /// If set, queries that exceed this duration will be automatically
    /// terminated to prevent resource exhaustion. Applies to the entire
    /// query execution from start to finish.
    ///
    /// Default: None (unlimited)
    /// Recommended: 30000 (30 seconds) for production environments
    pub query_timeout_ms: Option<u64>,
    /// Statement timeout in milliseconds (None for unlimited)
    ///
    /// If set, individual statement operations (e.g., a single scan or join)
    /// that exceed this duration will be terminated. This provides finer-grained
    /// timeout control than query_timeout_ms.
    ///
    /// Default: None (unlimited)
    /// Note: Currently not implemented, reserved for future use
    pub statement_timeout_ms: Option<u64>,
    /// Transaction isolation level
    ///
    /// Controls the visibility of concurrent transaction changes.
    /// Default: ReadCommitted (standard PostgreSQL default)
    pub transaction_isolation: TransactionIsolation,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: Some(PathBuf::from("./heliosdb-data")),
            memory_only: false,
            wal_enabled: true,
            wal_sync_mode: WalSyncModeConfig::Sync, // Safest default
            cache_size: 512 * 1024 * 1024, // 512 MB
            compression: CompressionType::Zstd,
            time_travel_enabled: true, // Enable by default for zero-config transparency
            query_timeout_ms: None, // Unlimited by default
            statement_timeout_ms: None, // Unlimited by default
            transaction_isolation: TransactionIsolation::ReadCommitted, // PostgreSQL default
        }
    }
}

/// WAL synchronization mode configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalSyncModeConfig {
    /// Synchronous - fsync on every write (safest, slowest)
    Sync,
    /// Asynchronous - OS-managed flush (faster, less safe)
    Async,
    /// Group commit - batch multiple operations (balanced)
    GroupCommit,
}

/// Compression type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionType {
    /// No compression
    None,
    /// Zstandard compression (recommended)
    Zstd,
    /// LZ4 compression (faster, lower ratio)
    Lz4,
}

/// Transaction isolation level
///
/// Controls how transactions see concurrent changes from other transactions.
/// HeliosDB-Lite implements snapshot isolation by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransactionIsolation {
    /// Read Uncommitted (not recommended)
    ///
    /// Transactions can see uncommitted changes from other transactions (dirty reads).
    /// Not fully supported - maps to ReadCommitted for safety.
    ReadUncommitted,
    /// Read Committed (PostgreSQL default)
    ///
    /// Transactions only see committed changes from other transactions.
    /// Each statement sees a fresh snapshot at statement start.
    ReadCommitted,
    /// Repeatable Read
    ///
    /// Transactions see a consistent snapshot from transaction start.
    /// No dirty reads, non-repeatable reads, or phantom reads.
    RepeatableRead,
    /// Serializable (strictest)
    ///
    /// Transactions execute as if they run serially.
    /// Prevents all anomalies but may cause serialization failures.
    Serializable,
}

/// Encryption configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EncryptionConfig {
    /// Encryption enabled
    pub enabled: bool,
    /// Encryption algorithm
    pub algorithm: EncryptionAlgorithm,
    /// Key source
    pub key_source: KeySource,
    /// Key rotation interval (days)
    pub rotation_interval_days: u32,
    /// Zero-Knowledge Encryption mode (v3.5)
    #[serde(default)]
    pub zke: ZkeEncryptionConfig,
}

impl Default for EncryptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            algorithm: EncryptionAlgorithm::Aes256Gcm,
            key_source: KeySource::Environment("HELIOSDB_ENCRYPTION_KEY".to_string()),
            rotation_interval_days: 90,
            zke: ZkeEncryptionConfig::default(),
        }
    }
}

/// Zero-Knowledge Encryption configuration (v3.5)
///
/// ZKE ensures encryption keys never leave the client. The server only
/// ever sees encrypted data and cannot decrypt without client-provided keys.
///
/// # Security Properties
///
/// - **Client-Side Encryption**: All data encrypted before transmission
/// - **Per-Request Keys**: Keys provided with each request, never stored
/// - **Key Hash Validation**: Server validates key via SHA-256 hash
/// - **Nonce-Based Replay Protection**: Each request has unique nonce
///
/// # Example Configuration
///
/// ```toml
/// [encryption.zke]
/// enabled = true
/// mode = "per_request"
/// require_key_hash = true
/// replay_protection = true
/// nonce_window_secs = 300
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZkeEncryptionConfig {
    /// Enable Zero-Knowledge Encryption mode
    pub enabled: bool,
    /// ZKE mode: "full", "hybrid", or "per_request"
    pub mode: ZkeMode,
    /// Require key hash validation on every request
    pub require_key_hash: bool,
    /// Enable nonce-based replay protection
    pub replay_protection: bool,
    /// Nonce validity window in seconds (default: 300 = 5 minutes)
    pub nonce_window_secs: u64,
    /// Maximum cached nonces for replay protection (default: 10000)
    pub max_cached_nonces: usize,
}

impl Default for ZkeEncryptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: ZkeMode::PerRequest,
            require_key_hash: true,
            replay_protection: true,
            nonce_window_secs: 300,
            max_cached_nonces: 10000,
        }
    }
}

/// Zero-Knowledge Encryption mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZkeMode {
    /// Full zero-knowledge: client encrypts all data
    /// - No server-side search capabilities
    /// - Maximum privacy
    Full,
    /// Hybrid: metadata visible, row data encrypted
    /// - Table/column names unencrypted
    /// - Basic filtering possible
    Hybrid,
    /// Per-request decryption with client-provided key
    /// - Full SQL capabilities
    /// - Key zeroed after each request
    PerRequest,
}

/// Encryption algorithm
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EncryptionAlgorithm {
    /// AES-256-GCM (recommended)
    Aes256Gcm,
}

/// Encryption key source
///
/// Specifies where the encryption key is retrieved from. For security,
/// keys should never be stored in configuration files directly.
///
/// # Security Recommendations
///
/// - **Production**: Use `Kms` with a cloud provider for key management
/// - **Development**: Use `Environment` with a secure secret manager
/// - **Testing**: Use `File` with proper file permissions (chmod 600)
///
/// # Examples
///
/// ```toml
/// # Environment variable (recommended for development)
/// [encryption]
/// key_source = { Environment = "HELIOSDB_KEY" }
///
/// # AWS KMS (recommended for production)
/// [encryption]
/// key_source = { Kms = { provider = "aws", key_id = "arn:aws:kms:..." } }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeySource {
    /// Read key from environment variable
    Environment(String),
    /// Read key from file (ensure proper file permissions)
    File(PathBuf),
    /// Use cloud Key Management Service
    Kms {
        /// Cloud provider: "aws", "azure", or "gcp"
        provider: String,
        /// Key identifier (ARN for AWS, key URI for others)
        key_id: String,
    },
}

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Listen address
    pub listen_addr: String,
    /// PostgreSQL protocol port
    pub port: u16,
    /// Oracle TNS protocol port (optional, disabled if None)
    pub oracle_port: Option<u16>,
    /// Maximum connections
    pub max_connections: usize,
    /// TLS enabled
    pub tls_enabled: bool,
    /// TLS certificate path
    pub tls_cert_path: Option<PathBuf>,
    /// TLS key path
    pub tls_key_path: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1".to_string(),
            port: 5432,
            oracle_port: Some(1521), // Enable Oracle protocol by default
            max_connections: 100,
            tls_enabled: false,
            tls_cert_path: None,
            tls_key_path: None,
        }
    }
}

/// Performance configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PerformanceConfig {
    /// Worker threads
    pub worker_threads: usize,
    /// Query timeout (seconds)
    pub query_timeout_secs: u64,
    /// Enable SIMD
    pub simd_enabled: bool,
    /// Parallel query execution
    pub parallel_query: bool,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            worker_threads: num_cpus::get(),
            query_timeout_secs: 300,
            simd_enabled: true,
            parallel_query: true,
        }
    }
}

// Add num_cpus to dependencies
// For now use a simple fallback
mod num_cpus {
    pub fn get() -> usize {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    }
}

/// Optimizer configuration (v2.1)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OptimizerConfig {
    /// Enable query optimizer
    pub enabled: bool,
    /// Enable sequential scan
    pub enable_seqscan: bool,
    /// Enable index scan
    pub enable_indexscan: bool,
    /// Enable hash join
    pub enable_hashjoin: bool,
    /// Enable merge join
    pub enable_mergejoin: bool,
    /// Enable nested loop join
    pub enable_nestloop: bool,
    /// Cost model parameters
    pub seq_page_cost: f64,
    pub random_page_cost: f64,
    pub cpu_tuple_cost: f64,
    pub cpu_index_tuple_cost: f64,
}

impl Default for OptimizerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            enable_seqscan: true,
            enable_indexscan: true,
            enable_hashjoin: true,
            enable_mergejoin: true,
            enable_nestloop: true,
            seq_page_cost: 1.0,
            random_page_cost: 4.0,
            cpu_tuple_cost: 0.01,
            cpu_index_tuple_cost: 0.005,
        }
    }
}

/// Authentication configuration (v2.1)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthenticationConfig {
    /// Enable authentication
    pub enabled: bool,
    /// Authentication method
    pub method: AuthMethod,
    /// JWT secret key (for JWT auth)
    pub jwt_secret: Option<String>,
    /// JWT expiration time (seconds)
    pub jwt_expiration_secs: u64,
    /// Password hash algorithm
    pub password_hash_algorithm: PasswordHashAlgorithm,
    /// Users file path (for file-based auth)
    pub users_file: Option<PathBuf>,
}

impl Default for AuthenticationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            method: AuthMethod::Trust,
            jwt_secret: None,
            jwt_expiration_secs: 86400, // 24 hours
            password_hash_algorithm: PasswordHashAlgorithm::Argon2,
            users_file: None,
        }
    }
}

/// Authentication method
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    /// No authentication (dev mode)
    Trust,
    /// Password authentication
    Password,
    /// JWT authentication
    Jwt,
    /// LDAP authentication
    Ldap,
}

/// Password hash algorithm
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasswordHashAlgorithm {
    /// Argon2 (recommended)
    Argon2,
    /// BCrypt
    Bcrypt,
    /// PBKDF2
    Pbkdf2,
}

/// Compression configuration (v2.1)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompressionConfig {
    /// Default compression type
    pub default_type: CompressionType,
    /// Compression level (1-22 for zstd)
    pub level: i32,
    /// Enable ALP compression for numeric columns
    pub enable_alp: bool,
    /// Enable FSST compression for string columns
    pub enable_fsst: bool,
    /// Minimum data size to trigger compression (bytes)
    pub min_size_bytes: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            default_type: CompressionType::Zstd,
            level: 3,
            enable_alp: true,
            enable_fsst: true,
            min_size_bytes: 1024, // 1KB
        }
    }
}

/// Materialized view configuration (v2.3 - Incremental MVs)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MaterializedViewConfig {
    /// Enable auto-refresh by default
    pub auto_refresh_default: bool,
    /// Default max CPU percentage for refresh
    pub default_max_cpu_percent: u8,
    /// Refresh check interval (seconds)
    pub refresh_check_interval_secs: u64,
    /// Maximum concurrent refreshes
    pub max_concurrent_refreshes: usize,
    /// Enable incremental refresh feature (v2.3.0)
    pub enable_incremental: bool,
    /// Enable delta tracking for base tables (v2.3.0)
    pub enable_delta_tracking: bool,
    /// Enable CPU-aware scheduling (v2.3.0)
    pub enable_scheduler: bool,
    /// Delta retention period in hours (purge old deltas)
    pub delta_retention_hours: u64,
}

impl Default for MaterializedViewConfig {
    fn default() -> Self {
        Self {
            auto_refresh_default: false,
            default_max_cpu_percent: 15,
            refresh_check_interval_secs: 60,
            max_concurrent_refreshes: 2,
            enable_incremental: false,  // Disabled by default, opt-in
            enable_delta_tracking: false,  // Disabled by default
            enable_scheduler: false,  // Disabled by default
            delta_retention_hours: 168,  // 7 days
        }
    }
}

/// Vector index configuration (v2.1)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VectorConfig {
    /// Default vector index type
    pub default_index_type: VectorIndexType,
    /// HNSW ef_construction parameter
    pub hnsw_ef_construction: usize,
    /// HNSW M parameter (connections per layer)
    pub hnsw_m: usize,
    /// Enable product quantization
    pub enable_pq: bool,
    /// PQ subvector count
    pub pq_subvectors: usize,
    /// PQ bits per subvector
    pub pq_bits: usize,
}

impl Default for VectorConfig {
    fn default() -> Self {
        Self {
            default_index_type: VectorIndexType::Hnsw,
            hnsw_ef_construction: 200,
            hnsw_m: 16,
            enable_pq: true,
            pq_subvectors: 8,
            pq_bits: 8,
        }
    }
}

/// Vector index type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VectorIndexType {
    /// Flat (brute force, exact)
    Flat,
    /// HNSW (approximate, fast)
    Hnsw,
    /// IVF (inverted file)
    Ivf,
}

/// Sync configuration (v2.3 - Experimental)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// Enable sync protocol
    pub enabled: bool,
    /// Node ID for this instance (generated if None)
    pub node_id: Option<String>,
    /// Sync server URL (for clients)
    pub server_url: Option<String>,
    /// Client ID (for authentication)
    pub client_id: Option<String>,
    /// Sync interval in seconds
    pub sync_interval_secs: u64,
    /// Enable change log capture
    pub change_log_enabled: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            node_id: None,
            server_url: None,
            client_id: None,
            sync_interval_secs: 30,
            change_log_enabled: true,
        }
    }
}

/// Session configuration (v3.1.0)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Session timeout in seconds (default: 3600 = 1 hour)
    pub timeout_secs: u64,
    /// Maximum sessions per user (default: 10)
    pub max_sessions_per_user: u32,
    /// Session cleanup interval in seconds (default: 300 = 5 minutes)
    pub cleanup_interval_secs: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 3600,
            max_sessions_per_user: 10,
            cleanup_interval_secs: 300,
        }
    }
}

impl SessionConfig {
    /// Validate configuration values
    pub fn validate(&self) -> crate::Result<()> {
        if self.timeout_secs < 1 {
            return Err(crate::Error::config(
                "session.timeout_secs must be at least 1 second",
            ));
        }
        if self.max_sessions_per_user < 1 {
            return Err(crate::Error::config(
                "session.max_sessions_per_user must be at least 1",
            ));
        }
        if self.cleanup_interval_secs < 1 {
            return Err(crate::Error::config(
                "session.cleanup_interval_secs must be at least 1 second",
            ));
        }
        Ok(())
    }
}

/// Lock configuration (v3.1.0)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LockConfig {
    /// Lock acquisition timeout in milliseconds (default: 30000 = 30 seconds)
    pub timeout_ms: u32,
    /// Deadlock detection interval in milliseconds (default: 100)
    pub deadlock_check_interval_ms: u32,
    /// Maximum number of concurrent lock holders (default: 10000)
    pub max_lock_holders: u32,
}

impl Default for LockConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 30000,
            deadlock_check_interval_ms: 100,
            max_lock_holders: 10000,
        }
    }
}

impl LockConfig {
    /// Validate configuration values
    pub fn validate(&self) -> crate::Result<()> {
        if self.timeout_ms < 100 {
            return Err(crate::Error::config(
                "locks.timeout_ms must be at least 100 milliseconds",
            ));
        }
        if self.deadlock_check_interval_ms < 10 {
            return Err(crate::Error::config(
                "locks.deadlock_check_interval_ms must be at least 10 milliseconds",
            ));
        }
        if self.max_lock_holders < 1 {
            return Err(crate::Error::config(
                "locks.max_lock_holders must be at least 1",
            ));
        }
        Ok(())
    }
}

/// Dump configuration (v3.1.0)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DumpConfig {
    /// Enable automatic dumps on a schedule (default: false)
    pub auto_dump_enabled: bool,
    /// Cron-style schedule for automatic dumps (e.g., "0 */6 * * *")
    pub schedule: String,
    /// Compression algorithm: "zstd", "gzip", "none" (default: "zstd")
    pub compression: String,
    /// Maximum size of a single dump file in MB before rolling (default: 10000)
    pub max_dump_size_mb: u64,
    /// Number of old dumps to keep (0 = keep all, default: 10)
    pub keep_dumps: usize,
    /// Directory to store dumps (default: ".dumps")
    pub dump_dir: String,
}

impl Default for DumpConfig {
    fn default() -> Self {
        Self {
            auto_dump_enabled: false,
            schedule: String::new(),
            compression: "zstd".to_string(),
            max_dump_size_mb: 10000,
            keep_dumps: 10,
            dump_dir: ".dumps".to_string(),
        }
    }
}

impl DumpConfig {
    /// Validate configuration values
    pub fn validate(&self) -> crate::Result<()> {
        // Validate compression type
        match self.compression.as_str() {
            "zstd" | "gzip" | "none" => {}
            _ => {
                return Err(crate::Error::config(format!(
                    "dump.compression must be 'zstd', 'gzip', or 'none', got '{}'",
                    self.compression
                )));
            }
        }

        // Validate max dump size
        if self.max_dump_size_mb < 1 {
            return Err(crate::Error::config(
                "dump.max_dump_size_mb must be at least 1 MB",
            ));
        }

        // Validate cron schedule if auto_dump_enabled
        if self.auto_dump_enabled && !self.schedule.is_empty() {
            Self::validate_cron_schedule(&self.schedule)?;
        }

        Ok(())
    }

    /// Validate cron schedule format (basic validation)
    fn validate_cron_schedule(schedule: &str) -> crate::Result<()> {
        let parts: Vec<&str> = schedule.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(crate::Error::config(format!(
                "Invalid cron schedule format '{}'. Expected 5 fields: minute hour day month weekday",
                schedule
            )));
        }
        Ok(())
    }
}

/// Resource quota configuration (v3.1.0)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceQuotaConfig {
    /// Memory limit per user in MB (default: 1024)
    pub memory_limit_per_user_mb: u64,
    /// Maximum concurrent queries per user (default: 100)
    pub max_concurrent_queries: u32,
    /// Query execution timeout in seconds (default: 300 = 5 minutes)
    pub query_timeout_secs: u64,
}

impl Default for ResourceQuotaConfig {
    fn default() -> Self {
        Self {
            memory_limit_per_user_mb: 1024,
            max_concurrent_queries: 100,
            query_timeout_secs: 300,
        }
    }
}

impl ResourceQuotaConfig {
    /// Validate configuration values
    pub fn validate(&self) -> crate::Result<()> {
        if self.memory_limit_per_user_mb < 1 {
            return Err(crate::Error::config(
                "resource_quotas.memory_limit_per_user_mb must be at least 1 MB",
            ));
        }
        if self.max_concurrent_queries < 1 {
            return Err(crate::Error::config(
                "resource_quotas.max_concurrent_queries must be at least 1",
            ));
        }
        if self.query_timeout_secs < 1 {
            return Err(crate::Error::config(
                "resource_quotas.query_timeout_secs must be at least 1 second",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_config_default() {
        let config = SessionConfig::default();
        assert_eq!(config.timeout_secs, 3600);
        assert_eq!(config.max_sessions_per_user, 10);
        assert_eq!(config.cleanup_interval_secs, 300);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_session_config_validation() {
        let mut config = SessionConfig::default();

        // Valid config
        assert!(config.validate().is_ok());

        // Invalid timeout
        config.timeout_secs = 0;
        assert!(config.validate().is_err());
        config.timeout_secs = 3600;

        // Invalid max sessions
        config.max_sessions_per_user = 0;
        assert!(config.validate().is_err());
        config.max_sessions_per_user = 10;

        // Invalid cleanup interval
        config.cleanup_interval_secs = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_lock_config_default() {
        let config = LockConfig::default();
        assert_eq!(config.timeout_ms, 30000);
        assert_eq!(config.deadlock_check_interval_ms, 100);
        assert_eq!(config.max_lock_holders, 10000);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_lock_config_validation() {
        let mut config = LockConfig::default();

        // Valid config
        assert!(config.validate().is_ok());

        // Invalid timeout (too low)
        config.timeout_ms = 50;
        assert!(config.validate().is_err());
        config.timeout_ms = 30000;

        // Invalid deadlock check interval
        config.deadlock_check_interval_ms = 5;
        assert!(config.validate().is_err());
        config.deadlock_check_interval_ms = 100;

        // Invalid max lock holders
        config.max_lock_holders = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_dump_config_default() {
        let config = DumpConfig::default();
        assert!(!config.auto_dump_enabled);
        assert_eq!(config.schedule, "");
        assert_eq!(config.compression, "zstd");
        assert_eq!(config.max_dump_size_mb, 10000);
        assert_eq!(config.keep_dumps, 10);
        assert_eq!(config.dump_dir, ".dumps");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_dump_config_validation() {
        let mut config = DumpConfig::default();

        // Valid config
        assert!(config.validate().is_ok());

        // Invalid compression type
        config.compression = "invalid".to_string();
        assert!(config.validate().is_err());
        config.compression = "zstd".to_string();

        // Valid compression types
        config.compression = "gzip".to_string();
        assert!(config.validate().is_ok());
        config.compression = "none".to_string();
        assert!(config.validate().is_ok());
        config.compression = "zstd".to_string();

        // Invalid max dump size
        config.max_dump_size_mb = 0;
        assert!(config.validate().is_err());
        config.max_dump_size_mb = 10000;

        // Valid cron schedule
        config.auto_dump_enabled = true;
        config.schedule = "0 */6 * * *".to_string();
        assert!(config.validate().is_ok());

        // Invalid cron schedule
        config.schedule = "invalid".to_string();
        assert!(config.validate().is_err());

        // Empty schedule with auto_dump_enabled is OK
        config.schedule = "".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_resource_quota_config_default() {
        let config = ResourceQuotaConfig::default();
        assert_eq!(config.memory_limit_per_user_mb, 1024);
        assert_eq!(config.max_concurrent_queries, 100);
        assert_eq!(config.query_timeout_secs, 300);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_resource_quota_config_validation() {
        let mut config = ResourceQuotaConfig::default();

        // Valid config
        assert!(config.validate().is_ok());

        // Invalid memory limit
        config.memory_limit_per_user_mb = 0;
        assert!(config.validate().is_err());
        config.memory_limit_per_user_mb = 1024;

        // Invalid max concurrent queries
        config.max_concurrent_queries = 0;
        assert!(config.validate().is_err());
        config.max_concurrent_queries = 100;

        // Invalid query timeout
        config.query_timeout_secs = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_with_new_sections() {
        let config = Config::default();

        // Verify new sections are initialized
        assert_eq!(config.session.timeout_secs, 3600);
        assert_eq!(config.locks.timeout_ms, 30000);
        assert_eq!(config.dump.compression, "zstd");
        assert_eq!(config.resource_quotas.memory_limit_per_user_mb, 1024);

        // Validate entire config
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_serialization() {
        let config = Config::default();

        // Serialize to TOML
        let toml_str = toml::to_string(&config).expect("Failed to serialize config");

        // Should contain new sections
        assert!(toml_str.contains("[session]"));
        assert!(toml_str.contains("[locks]"));
        assert!(toml_str.contains("[dump]"));
        assert!(toml_str.contains("[resource_quotas]"));
    }

    #[test]
    fn test_config_deserialization() {
        let toml_str = r#"
            [storage]
            memory_only = true
            wal_enabled = false
            wal_sync_mode = "sync"
            cache_size = 536870912
            compression = "Zstd"
            time_travel_enabled = true
            transaction_isolation = "READ_COMMITTED"

            [session]
            timeout_secs = 7200
            max_sessions_per_user = 20
            cleanup_interval_secs = 600

            [locks]
            timeout_ms = 60000
            deadlock_check_interval_ms = 200
            max_lock_holders = 20000

            [dump]
            auto_dump_enabled = true
            schedule = "0 2 * * *"
            compression = "gzip"
            max_dump_size_mb = 5000
            keep_dumps = 5
            dump_dir = "/var/dumps"

            [resource_quotas]
            memory_limit_per_user_mb = 2048
            max_concurrent_queries = 200
            query_timeout_secs = 600
        "#;

        let config: Config = toml::from_str(toml_str).expect("Failed to deserialize config");

        // Verify session config
        assert_eq!(config.session.timeout_secs, 7200);
        assert_eq!(config.session.max_sessions_per_user, 20);
        assert_eq!(config.session.cleanup_interval_secs, 600);

        // Verify lock config
        assert_eq!(config.locks.timeout_ms, 60000);
        assert_eq!(config.locks.deadlock_check_interval_ms, 200);
        assert_eq!(config.locks.max_lock_holders, 20000);

        // Verify dump config
        assert!(config.dump.auto_dump_enabled);
        assert_eq!(config.dump.schedule, "0 2 * * *");
        assert_eq!(config.dump.compression, "gzip");
        assert_eq!(config.dump.max_dump_size_mb, 5000);
        assert_eq!(config.dump.keep_dumps, 5);
        assert_eq!(config.dump.dump_dir, "/var/dumps");

        // Verify resource quota config
        assert_eq!(config.resource_quotas.memory_limit_per_user_mb, 2048);
        assert_eq!(config.resource_quotas.max_concurrent_queries, 200);
        assert_eq!(config.resource_quotas.query_timeout_secs, 600);

        // Validate entire config
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_with_missing_sections() {
        // Config with only storage section (old format)
        let toml_str = r#"
            [storage]
            memory_only = true
            wal_enabled = false
            wal_sync_mode = "sync"
            cache_size = 536870912
            compression = "Zstd"
            time_travel_enabled = true
            transaction_isolation = "READ_COMMITTED"
        "#;

        let config: Config = toml::from_str(toml_str).expect("Failed to deserialize config");

        // New sections should have default values
        assert_eq!(config.session.timeout_secs, 3600);
        assert_eq!(config.locks.timeout_ms, 30000);
        assert_eq!(config.dump.compression, "zstd");
        assert_eq!(config.resource_quotas.memory_limit_per_user_mb, 1024);

        // Should still validate
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_cron_schedule_validation() {
        // Valid cron schedules
        assert!(DumpConfig::validate_cron_schedule("0 */6 * * *").is_ok());
        assert!(DumpConfig::validate_cron_schedule("0 2 * * *").is_ok());
        assert!(DumpConfig::validate_cron_schedule("*/15 * * * *").is_ok());
        assert!(DumpConfig::validate_cron_schedule("0 0 1 * *").is_ok());
        assert!(DumpConfig::validate_cron_schedule("30 3 * * 0").is_ok());

        // Invalid cron schedules
        assert!(DumpConfig::validate_cron_schedule("invalid").is_err());
        assert!(DumpConfig::validate_cron_schedule("0 * * *").is_err());  // Only 4 fields
        assert!(DumpConfig::validate_cron_schedule("0 * * * * *").is_err());  // 6 fields
        assert!(DumpConfig::validate_cron_schedule("").is_err());  // Empty
    }
}
