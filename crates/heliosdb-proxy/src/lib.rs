//! HeliosDB Proxy - Standalone Connection Router
//!
//! A standalone proxy for HeliosDB-Lite providing:
//! - Connection pooling
//! - Load balancing (read/write splitting)
//! - Health monitoring
//! - Transaction Replay (TR)
//!
//! # Deployment Options
//!
//! - **Standalone binary**: Run as a separate process
//! - **Kubernetes sidecar**: Deploy alongside your application
//! - **Embedded library**: Use as a library in your application
//!
//! # Quick Start
//!
//! ```bash
//! # Start with config file
//! heliosdb-proxy --config /etc/heliosdb/proxy.toml
//!
//! # Start with command line options
//! heliosdb-proxy \
//!   --listen 0.0.0.0:5432 \
//!   --primary db-primary:5432 \
//!   --standby db-standby-1:5432 \
//!   --standby db-standby-2:5432
//! ```
//!
//! # Configuration Example
//!
//! ```toml
//! [proxy]
//! listen_address = "0.0.0.0:5432"
//! admin_address = "0.0.0.0:9090"
//!
//! [pool]
//! min_connections = 5
//! max_connections = 100
//! idle_timeout_secs = 300
//!
//! [load_balancer]
//! strategy = "round_robin"  # or "least_connections", "latency_based"
//! read_write_split = true
//!
//! [health]
//! check_interval_secs = 5
//! failure_threshold = 3
//!
//! [[nodes]]
//! host = "db-primary"
//! port = 5432
//! role = "primary"
//!
//! [[nodes]]
//! host = "db-standby-1"
//! port = 5432
//! role = "standby"
//! ```

pub mod config;
pub mod server;
pub mod protocol;
pub mod admin;

use thiserror::Error;

/// Proxy error types
#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Connection error: {0}")]
    Connection(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Pool error: {0}")]
    Pool(String),

    #[error("Health check error: {0}")]
    HealthCheck(String),

    #[error("Failover error: {0}")]
    Failover(String),

    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("No healthy nodes available")]
    NoHealthyNodes,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, ProxyError>;

/// Proxy version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default listen port
pub const DEFAULT_PORT: u16 = 5432;

/// Default admin port
pub const DEFAULT_ADMIN_PORT: u16 = 9090;
