//! High Availability Replication Module
//!
//! Tier 1 HA for HeliosDB-Nano: a primary and its physical standbys.
//!
//! A standby is a 1:1 physical replica of its primary (plan
//! `docs/plans/PHYSICAL_STANDBY_REPLICATION.md`): it applies the primary's
//! RocksDB write groups verbatim and serves read-only SQL from them. Spec P2.1
//! put the whole conversation on ONE connection per standby, protocol
//! `NPHYS002` on the primary's `[replication] replication_port`
//! ([`physical_wire`]): the bootstrap copy, the physical WAL, the standby's
//! identity and history, its heartbeats and the acknowledgements synchronous
//! commits wait on ([`ha_state::HAStateRegistry::wait_for_sequence`]). Spec
//! P2.2 put that connection on TLS for remote standbys ([`physical_tls`]) and
//! made an interrupted bootstrap copy resumable. The native v3 channel, the
//! logical standby (WAL applicator, logical catch-up, its on-disk segment
//! store) and the observer, failover-watcher and split-brain machinery that
//! only spoke v3 are gone; fencing and promotion return on the physical
//! protocol (plan phase P5).
//!
//! # Feature Flags
//!
//! - `ha-tier1`: Warm standby replication

pub mod config;

// HA State Registry (available for all HA features)
#[cfg(feature = "ha-tier1")]
pub mod ha_state;
#[cfg(feature = "ha-tier1")]
pub use ha_state::{
    ha_state, HARole, HAStateRegistry, NodeConfig, PrimaryInfo, StandbyHandle, StandbyInfo, StandbyRegistration,
    SyncMode, SyncTimeoutAction,
};

// Tier 1: Warm Standby (ha-tier1)
#[cfg(feature = "ha-tier1")]
pub mod checkpoint_transfer;
#[cfg(feature = "ha-tier1")]
pub mod history;
#[cfg(feature = "ha-tier1")]
pub mod physical;
#[cfg(feature = "ha-tier1")]
pub mod physical_applier;
#[cfg(feature = "ha-tier1")]
pub mod physical_receiver;
#[cfg(feature = "ha-tier1")]
pub mod physical_source;
#[cfg(feature = "ha-tier1")]
pub mod physical_tls;
#[cfg(feature = "ha-tier1")]
pub mod physical_vector;
#[cfg(feature = "ha-tier1")]
pub mod physical_wire;
// Spec P1.3: the posture lives at the crate root (`crate::standby_posture`),
// compiled in every build because the SQL layer asks it whether this node is
// in recovery; re-exported here beside the receiver it is derived from.
pub use crate::standby_posture;
#[cfg(feature = "ha-tier1")]
pub mod token;

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
pub use token::read_replication_token;

use thiserror::Error;

/// Replication errors
#[derive(Debug, Error)]
pub enum ReplicationError {
    #[error("Replication history mismatch: receiver expects {expected}, primary has {actual}; preserve this receiver and prepare a new dataset; online resync is not supported")]
    HistoryMismatch { expected: uuid::Uuid, actual: uuid::Uuid },

    #[error("Replication history metadata: {0}")]
    HistoryMetadata(String),

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
