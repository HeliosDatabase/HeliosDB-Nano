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
pub use ha_state::{ha_state, HARole, HAStateRegistry, NodeConfig, PrimaryInfo, StandbyInfo, SyncMode};

// Tier 1: Warm Standby (ha-tier1)
#[cfg(feature = "ha-tier1")]
pub mod checkpoint_transfer;
#[cfg(feature = "ha-tier1")]
pub mod failover_watcher;
#[cfg(feature = "ha-tier1")]
pub mod history;
#[cfg(feature = "ha-tier1")]
pub mod logical_replication;
#[cfg(feature = "ha-tier1")]
pub mod lsn_manager;
#[cfg(feature = "ha-tier1")]
pub mod physical;
#[cfg(feature = "ha-tier1")]
pub mod physical_applier;
#[cfg(feature = "ha-tier1")]
pub mod physical_receiver;
#[cfg(feature = "ha-tier1")]
pub mod physical_source;
#[cfg(feature = "ha-tier1")]
pub mod physical_vector;
#[cfg(feature = "ha-tier1")]
pub mod physical_wire;
#[cfg(feature = "ha-tier1")]
pub mod query_forwarder;
#[cfg(feature = "ha-tier1")]
pub mod split_brain;
// Spec P1.3: the posture lives at the crate root (`crate::standby_posture`),
// compiled in every build because the SQL layer asks it whether this node is
// in recovery; re-exported here beside the receiver it is derived from.
pub use crate::standby_posture;
#[cfg(feature = "ha-tier1")]
pub mod streaming;
#[cfg(feature = "ha-tier1")]
pub mod token;
#[cfg(feature = "ha-tier1")]
pub mod transport;
#[cfg(feature = "ha-tier1")]
pub mod wal_applicator;
#[cfg(feature = "ha-tier1")]
pub mod wal_replicator;
#[cfg(feature = "ha-tier1")]
pub mod wal_store;

// Controlled Switchover (ha-tier1)
#[cfg(feature = "ha-tier1")]
pub mod role_manager;
#[cfg(feature = "ha-tier1")]
pub mod switchover;
#[cfg(feature = "ha-tier1")]
pub mod topology;
#[cfg(feature = "ha-tier1")]
pub use role_manager::{NodeRole, RoleChangeEvent, RoleChangeReason, RoleManager, SwitchoverPhase};
#[cfg(feature = "ha-tier1")]
pub use switchover::{SwitchoverCheck, SwitchoverConfig, SwitchoverCoordinator, SwitchoverEvent};
#[cfg(feature = "ha-tier1")]
pub use topology::{topology_manager, ClusterSummary, NodeInfo, TopologyEvent, TopologyManager};

// Re-exports for convenience
pub use config::*;

#[cfg(feature = "ha-tier1")]
pub use failover_watcher::{
    AutomaticFailoverBuilder, AutomaticFailoverCoordinator, FailoverCandidate, FailoverEvent, FailoverWatcher,
    HealthCheckResult,
};
#[cfg(feature = "ha-tier1")]
pub use logical_replication::{
    ChangeEvent, ChangeOperation, ChangeRow, ColumnMapping, ColumnTransform, FieldValue, LogicalReplicationConfig,
    LogicalReplicationPipeline, RowFilter, TableFilter,
};
#[cfg(feature = "ha-tier1")]
pub use lsn_manager::LsnManager;
#[cfg(feature = "ha-tier1")]
pub use query_forwarder::{
    init_query_forwarder, query_forwarder, ColumnInfo, ForwardedResult, ForwarderError, QueryForwarder,
};
#[cfg(feature = "ha-tier1")]
pub use split_brain::{
    ClusterNode, ObserverConfig, ObserverNode, ProtectionEvent, ProtectionState, SplitBrainProtector,
};
#[cfg(feature = "ha-tier1")]
pub use streaming::{
    StreamingClient, StreamingClientConfig, StreamingClientState, StreamingClientStatus, StreamingServer,
    StreamingServerConfig,
};
#[cfg(feature = "ha-tier1")]
pub use token::read_replication_token;
#[cfg(feature = "ha-tier1")]
pub use transport::{
    AckType,
    Capabilities,
    HealthStatus,
    MessageType,
    NodeRole as WireNodeRole, // Renamed to avoid conflict with role_manager::NodeRole
    ReplicationConnection,
    ReplicationServer,
    SyncModeConfig,
};
#[cfg(feature = "ha-tier1")]
pub use wal_applicator::WalApplicator;
#[cfg(feature = "ha-tier1")]
pub use wal_replicator::{Lsn, WalReplicator};
#[cfg(feature = "ha-tier1")]
pub use wal_store::{BatchRequest, BatchResult, BatchStreamState, WalStore, WalStoreConfig};

use thiserror::Error;

/// Replication errors
#[derive(Debug, Error)]
pub enum ReplicationError {
    #[error("Replication history mismatch: receiver expects {expected}, primary has {actual}; preserve this receiver and prepare a new dataset; online resync is not supported")]
    HistoryMismatch { expected: uuid::Uuid, actual: uuid::Uuid },

    #[error("Replication history metadata: {0}")]
    HistoryMetadata(String),

    #[error("Unsupported native replication protocol version {actual}; required version {expected} includes mandatory history identity")]
    ProtocolVersion { expected: u16, actual: u16 },

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

#[cfg(feature = "ha-tier1")]
impl From<history::HistoryError> for ReplicationError {
    fn from(error: history::HistoryError) -> Self {
        match error {
            history::HistoryError::Mismatch { expected, actual } => Self::HistoryMismatch { expected, actual },
            other => Self::HistoryMetadata(other.to_string()),
        }
    }
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
