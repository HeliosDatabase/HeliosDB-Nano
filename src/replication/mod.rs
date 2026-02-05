//! High Availability Replication Module
//!
//! Implements Tier 1 HA for HeliosDB-Nano:
//! - Tier 1: Warm Standby (Active-Passive WAL streaming)
//!
//! # Feature Flags
//!
//! - `ha-tier1`: Warm standby replication

pub mod config;

// HA State Registry (available for all HA features)
#[cfg(feature = "ha-tier1")]
pub mod ha_state;
#[cfg(feature = "ha-tier1")]
pub use ha_state::{ha_state, HAStateRegistry, HARole, SyncMode, NodeConfig, StandbyInfo, PrimaryInfo};

// Tier 1: Warm Standby (ha-tier1)
#[cfg(feature = "ha-tier1")]
pub mod wal_replicator;
#[cfg(feature = "ha-tier1")]
pub mod wal_applicator;
#[cfg(feature = "ha-tier1")]
pub mod failover_watcher;
#[cfg(feature = "ha-tier1")]
pub mod lsn_manager;
#[cfg(feature = "ha-tier1")]
pub mod transport;
#[cfg(feature = "ha-tier1")]
pub mod streaming;
#[cfg(feature = "ha-tier1")]
pub mod wal_store;
#[cfg(feature = "ha-tier1")]
pub mod split_brain;
#[cfg(feature = "ha-tier1")]
pub mod logical_replication;
#[cfg(feature = "ha-tier1")]
pub mod query_forwarder;

// Controlled Switchover (ha-tier1)
#[cfg(feature = "ha-tier1")]
pub mod role_manager;
#[cfg(feature = "ha-tier1")]
pub mod switchover;
#[cfg(feature = "ha-tier1")]
pub mod topology;
#[cfg(feature = "ha-tier1")]
pub use role_manager::{RoleManager, NodeRole, RoleChangeEvent, RoleChangeReason, SwitchoverPhase};
#[cfg(feature = "ha-tier1")]
pub use switchover::{SwitchoverCoordinator, SwitchoverConfig, SwitchoverEvent, SwitchoverCheck};
#[cfg(feature = "ha-tier1")]
pub use topology::{TopologyManager, NodeInfo, TopologyEvent, ClusterSummary, topology_manager};

// Re-exports for convenience
pub use config::*;

#[cfg(feature = "ha-tier1")]
pub use wal_replicator::{WalReplicator, Lsn};
#[cfg(feature = "ha-tier1")]
pub use wal_applicator::WalApplicator;
#[cfg(feature = "ha-tier1")]
pub use failover_watcher::{
    FailoverWatcher, FailoverEvent, HealthCheckResult, FailoverCandidate,
    AutomaticFailoverCoordinator, AutomaticFailoverBuilder,
};
#[cfg(feature = "ha-tier1")]
pub use lsn_manager::LsnManager;
#[cfg(feature = "ha-tier1")]
pub use transport::{
    ReplicationConnection, ReplicationServer, SyncModeConfig,
    AckType, HealthStatus, MessageType, Capabilities,
    NodeRole as WireNodeRole, // Renamed to avoid conflict with role_manager::NodeRole
};
#[cfg(feature = "ha-tier1")]
pub use streaming::{
    StreamingServer, StreamingServerConfig, StreamingClient, StreamingClientConfig,
    StreamingClientState,
};
#[cfg(feature = "ha-tier1")]
pub use wal_store::{
    WalStore, WalStoreConfig, BatchRequest, BatchResult, BatchStreamState,
};
#[cfg(feature = "ha-tier1")]
pub use split_brain::{
    SplitBrainProtector, ObserverNode, ObserverConfig, ProtectionState, ProtectionEvent,
    ClusterNode,
};
#[cfg(feature = "ha-tier1")]
pub use logical_replication::{
    LogicalReplicationPipeline, LogicalReplicationConfig, TableFilter, RowFilter,
    ColumnMapping, ColumnTransform, ChangeEvent, ChangeOperation, ChangeRow, FieldValue,
};
#[cfg(feature = "ha-tier1")]
pub use query_forwarder::{
    QueryForwarder, ForwardedResult, ForwarderError, ColumnInfo,
    init_query_forwarder, query_forwarder,
};

use thiserror::Error;

/// Replication errors
#[derive(Debug, Error)]
pub enum ReplicationError {
    #[error("WAL streaming error: {0}")]
    WalStreaming(String),

    #[error("Failover error: {0}")]
    Failover(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Authentication error: {0}")]
    Authentication(String),

    #[error("Configuration error: {0}")]
    Configuration(String),

    #[error("LSN tracking error: {0}")]
    LsnTracking(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Transport error: {0}")]
    Transport(String),
}

pub type Result<T> = std::result::Result<T, ReplicationError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = ReplicationError::WalStreaming("test error".to_string());
        assert!(err.to_string().contains("WAL streaming"));
    }
}
