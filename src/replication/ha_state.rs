//! HA State Registry
//!
//! Provides global state tracking for High Availability monitoring.
//! This module maintains information about:
//! - Current node's HA configuration and role
//! - Connected standbys (when running as primary)
//! - Primary connection status (when running as standby)
//! - Replication lag and performance metrics

use std::sync::RwLock;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use tokio::sync::broadcast;
use super::wal_replicator::{WalEntry, WalEntryType, Lsn};
use crate::storage::WalOperation;

/// Global HA state registry instance
static HA_STATE: once_cell::sync::Lazy<HAStateRegistry> =
    once_cell::sync::Lazy::new(HAStateRegistry::new);

/// Get the global HA state registry
pub fn ha_state() -> &'static HAStateRegistry {
    &HA_STATE
}

/// HA Node Role
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HARole {
    Standalone,
    Primary,
    Standby,
    Observer,
}

impl HARole {
    pub fn as_str(&self) -> &'static str {
        match self {
            HARole::Standalone => "standalone",
            HARole::Primary => "primary",
            HARole::Standby => "standby",
            HARole::Observer => "observer",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "primary" => HARole::Primary,
            "standby" => HARole::Standby,
            "observer" => HARole::Observer,
            _ => HARole::Standalone,
        }
    }
}

/// Sync mode for replication
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    Async,
    SemiSync,
    Sync,
}

impl SyncMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SyncMode::Async => "async",
            SyncMode::SemiSync => "semi-sync",
            SyncMode::Sync => "sync",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "semi-sync" | "semisync" => SyncMode::SemiSync,
            "sync" => SyncMode::Sync,
            _ => SyncMode::Async,
        }
    }
}

/// Node configuration
#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub node_id: Uuid,
    pub role: HARole,
    pub listen_addr: String,
    pub port: u16,
    pub replication_port: u16,
    pub sync_mode: SyncMode,
    pub primary_host: Option<String>,
    pub started_at: i64,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            node_id: Uuid::new_v4(),
            role: HARole::Standalone,
            listen_addr: "127.0.0.1".to_string(),
            port: 5432,
            replication_port: 5433,
            sync_mode: SyncMode::Async,
            primary_host: None,
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        }
    }
}

/// Connected standby information
#[derive(Debug, Clone)]
pub struct StandbyInfo {
    pub node_id: Uuid,
    pub address: String,
    pub connected_at: i64,
    pub last_heartbeat: i64,
    pub sync_mode: SyncMode,
    pub current_lsn: u64,
    pub flush_lsn: u64,
    pub apply_lsn: u64,
    pub lag_bytes: u64,
    pub lag_ms: u64,
    pub state: StandbyState,
}

/// Standby connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandbyState {
    Connecting,
    Streaming,
    CatchingUp,
    Synced,
    Disconnected,
}

impl StandbyState {
    pub fn as_str(&self) -> &'static str {
        match self {
            StandbyState::Connecting => "connecting",
            StandbyState::Streaming => "streaming",
            StandbyState::CatchingUp => "catching_up",
            StandbyState::Synced => "synced",
            StandbyState::Disconnected => "disconnected",
        }
    }
}

/// Primary connection information (for standbys)
#[derive(Debug, Clone)]
pub struct PrimaryInfo {
    pub node_id: Uuid,
    pub address: String,
    pub connected_at: i64,
    pub last_heartbeat: i64,
    pub primary_lsn: u64,
    pub local_lsn: u64,
    pub lag_bytes: u64,
    pub lag_ms: u64,
    pub fencing_token: u64,
    pub state: PrimaryConnectionState,
}

/// Primary connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimaryConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Streaming,
    Error,
}

impl PrimaryConnectionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PrimaryConnectionState::Disconnected => "disconnected",
            PrimaryConnectionState::Connecting => "connecting",
            PrimaryConnectionState::Connected => "connected",
            PrimaryConnectionState::Streaming => "streaming",
            PrimaryConnectionState::Error => "error",
        }
    }
}

/// Replication metrics
#[derive(Debug, Clone, Default)]
pub struct ReplicationMetrics {
    pub wal_writes: u64,
    pub wal_bytes_written: u64,
    pub records_replicated: u64,
    pub bytes_replicated: u64,
    pub heartbeats_sent: u64,
    pub heartbeats_received: u64,
    pub reconnect_count: u64,
    pub last_wal_write: Option<i64>,
    pub last_replication: Option<i64>,
}

/// HA State Registry
pub struct HAStateRegistry {
    config: RwLock<NodeConfig>,
    standbys: RwLock<HashMap<Uuid, StandbyInfo>>,
    primary: RwLock<Option<PrimaryInfo>>,
    metrics: RwLock<ReplicationMetrics>,
    current_lsn: RwLock<u64>,
    is_read_only: RwLock<bool>,
    /// WAL broadcast sender for replication (primary only)
    wal_broadcast_tx: RwLock<Option<broadcast::Sender<WalEntry>>>,
}

impl HAStateRegistry {
    /// Create a new HA state registry
    pub fn new() -> Self {
        Self {
            config: RwLock::new(NodeConfig::default()),
            standbys: RwLock::new(HashMap::new()),
            primary: RwLock::new(None),
            metrics: RwLock::new(ReplicationMetrics::default()),
            current_lsn: RwLock::new(0),
            is_read_only: RwLock::new(false),
            wal_broadcast_tx: RwLock::new(None),
        }
    }

    // ========== Configuration ==========

    /// Set node configuration
    pub fn set_config(&self, config: NodeConfig) {
        // Standbys should be read-only by default
        let read_only = config.role == HARole::Standby;

        if let Ok(mut cfg) = self.config.write() {
            *cfg = config;
        }
        if let Ok(mut ro) = self.is_read_only.write() {
            *ro = read_only;
        }
    }

    /// Get node configuration
    pub fn get_config(&self) -> Option<NodeConfig> {
        self.config.read().ok().map(|c| c.clone())
    }

    /// Get current role
    pub fn get_role(&self) -> HARole {
        self.config.read().map(|c| c.role).unwrap_or(HARole::Standalone)
    }

    /// Check if node is read-only (standbys are read-only by default)
    pub fn is_read_only(&self) -> bool {
        self.is_read_only.read().map(|r| *r).unwrap_or(false)
    }

    /// Set read-only mode explicitly
    pub fn set_read_only(&self, read_only: bool) {
        if let Ok(mut ro) = self.is_read_only.write() {
            *ro = read_only;
        }
    }

    // ========== Standby Management (Primary Role) ==========

    /// Register a connected standby
    pub fn register_standby(&self, info: StandbyInfo) {
        if let Ok(mut standbys) = self.standbys.write() {
            standbys.insert(info.node_id, info);
        }
    }

    /// Update standby status
    pub fn update_standby(&self, node_id: Uuid, update: impl FnOnce(&mut StandbyInfo)) {
        if let Ok(mut standbys) = self.standbys.write() {
            if let Some(standby) = standbys.get_mut(&node_id) {
                update(standby);
            }
        }
    }

    /// Remove a standby
    pub fn remove_standby(&self, node_id: Uuid) {
        if let Ok(mut standbys) = self.standbys.write() {
            standbys.remove(&node_id);
        }
    }

    /// Get all standbys
    pub fn get_standbys(&self) -> Vec<StandbyInfo> {
        self.standbys.read()
            .map(|s| s.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Get standby count
    pub fn standby_count(&self) -> usize {
        self.standbys.read().map(|s| s.len()).unwrap_or(0)
    }

    // ========== Primary Connection (Standby Role) ==========

    /// Set primary connection info
    pub fn set_primary(&self, info: PrimaryInfo) {
        if let Ok(mut primary) = self.primary.write() {
            *primary = Some(info);
        }
    }

    /// Update primary connection
    pub fn update_primary(&self, update: impl FnOnce(&mut PrimaryInfo)) {
        if let Ok(mut primary) = self.primary.write() {
            if let Some(ref mut p) = *primary {
                update(p);
            }
        }
    }

    /// Clear primary connection
    pub fn clear_primary(&self) {
        if let Ok(mut primary) = self.primary.write() {
            *primary = None;
        }
    }

    /// Get primary connection info
    pub fn get_primary(&self) -> Option<PrimaryInfo> {
        self.primary.read().ok().and_then(|p| p.clone())
    }

    // ========== LSN Management ==========

    /// Set current LSN
    pub fn set_lsn(&self, lsn: u64) {
        if let Ok(mut current) = self.current_lsn.write() {
            *current = lsn;
        }
    }

    /// Get current LSN
    pub fn get_lsn(&self) -> u64 {
        self.current_lsn.read().map(|l| *l).unwrap_or(0)
    }

    /// Increment LSN and return new value
    pub fn increment_lsn(&self) -> u64 {
        if let Ok(mut current) = self.current_lsn.write() {
            *current += 1;
            *current
        } else {
            0
        }
    }

    // ========== Metrics ==========

    /// Get replication metrics
    pub fn get_metrics(&self) -> ReplicationMetrics {
        self.metrics.read()
            .map(|m| m.clone())
            .unwrap_or_default()
    }

    /// Update metrics
    pub fn update_metrics(&self, update: impl FnOnce(&mut ReplicationMetrics)) {
        if let Ok(mut metrics) = self.metrics.write() {
            update(&mut metrics);
        }
    }

    /// Record WAL write
    pub fn record_wal_write(&self, bytes: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.update_metrics(|m| {
            m.wal_writes += 1;
            m.wal_bytes_written += bytes;
            m.last_wal_write = Some(now);
        });
    }

    /// Record replication
    pub fn record_replication(&self, records: u64, bytes: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.update_metrics(|m| {
            m.records_replicated += records;
            m.bytes_replicated += bytes;
            m.last_replication = Some(now);
        });
    }

    /// Record heartbeat sent
    pub fn record_heartbeat_sent(&self) {
        self.update_metrics(|m| m.heartbeats_sent += 1);
    }

    /// Record heartbeat received
    pub fn record_heartbeat_received(&self) {
        self.update_metrics(|m| m.heartbeats_received += 1);
    }

    /// Record reconnect
    pub fn record_reconnect(&self) {
        self.update_metrics(|m| m.reconnect_count += 1);
    }

    // ========== WAL Broadcasting (Primary Role) ==========

    /// Set the WAL broadcast sender (called by StreamingServer)
    pub fn set_wal_broadcast(&self, tx: broadcast::Sender<WalEntry>) {
        if let Ok(mut wal_tx) = self.wal_broadcast_tx.write() {
            *wal_tx = Some(tx);
        }
    }

    /// Clear the WAL broadcast sender
    pub fn clear_wal_broadcast(&self) {
        if let Ok(mut wal_tx) = self.wal_broadcast_tx.write() {
            *wal_tx = None;
        }
    }

    /// Check if WAL broadcasting is enabled
    pub fn has_wal_broadcast(&self) -> bool {
        self.wal_broadcast_tx.read()
            .map(|tx| tx.is_some())
            .unwrap_or(false)
    }

    /// Broadcast a WAL entry to standbys (called by storage engine)
    /// Returns the LSN if broadcast was successful, None if no broadcast channel
    pub fn broadcast_wal_entry(&self, entry: WalEntry) -> Option<Lsn> {
        if let Ok(tx_guard) = self.wal_broadcast_tx.read() {
            if let Some(ref tx) = *tx_guard {
                let lsn = entry.lsn;
                let data_len = entry.data.len() as u64;

                // Broadcast to all subscribers
                match tx.send(entry) {
                    Ok(receiver_count) => {
                        tracing::info!("broadcast_wal_entry: LSN={} sent to {} receivers, data_len={}", lsn, receiver_count, data_len);
                        // Update metrics
                        self.record_replication(1, data_len);
                        return Some(lsn);
                    }
                    Err(e) => {
                        tracing::warn!("broadcast_wal_entry: LSN={} send failed: {}", lsn, e);
                    }
                }
            } else {
                tracing::warn!("broadcast_wal_entry: No broadcast channel configured");
            }
        } else {
            tracing::warn!("broadcast_wal_entry: Failed to acquire read lock");
        }
        None
    }

    /// Wait for synchronous replication based on configured sync mode
    /// Returns Ok(()) if sync mode is Async or when required ACKs received
    /// Returns Err if timeout waiting for standbys
    pub fn wait_for_sync(&self, lsn: u64, timeout_ms: u64) -> std::result::Result<(), String> {
        let config = match self.get_config() {
            Some(c) => c,
            None => return Ok(()), // No config, treat as async
        };

        match config.sync_mode {
            SyncMode::Async => Ok(()), // No waiting needed
            SyncMode::SemiSync | SyncMode::Sync => {
                // Wait for at least one standby to reach this LSN
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

                loop {
                    if std::time::Instant::now() >= deadline {
                        return Err(format!("Timeout waiting for standby ACK for LSN {}", lsn));
                    }

                    // Check if any standby has reached this LSN
                    // "Faster-safe" approach: wait for flush_lsn (data received by standby)
                    // for both SemiSync and Sync modes. This is faster because we don't wait
                    // for the standby to apply the entry, but still safe because the data
                    // has reached the standby's WAL buffer and will survive primary crash.
                    let standbys = self.get_standbys();
                    tracing::debug!("wait_for_sync: Checking {} standbys for LSN={}", standbys.len(), lsn);
                    for s in &standbys {
                        tracing::debug!("wait_for_sync: Standby {} flush_lsn={} apply_lsn={}", s.node_id, s.flush_lsn, s.apply_lsn);
                    }
                    let acked = standbys.iter().any(|s| s.flush_lsn >= lsn);

                    if acked {
                        tracing::info!("wait_for_sync: LSN {} acked by standby", lsn);
                        return Ok(());
                    }

                    // Short sleep before retry
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
    }

    /// Broadcast a WAL operation from storage engine
    /// Converts WalOperation to replication WalEntry and broadcasts
    pub fn broadcast_wal_operation(&self, lsn: u64, operation: &WalOperation) -> Option<Lsn> {
        let role = self.get_role();
        tracing::info!("broadcast_wal_operation: LSN={}, role={:?}, op={:?}", lsn, role, std::mem::discriminant(operation));

        // Only broadcast if we're the primary
        if role != HARole::Primary {
            tracing::info!("broadcast_wal_operation: Skipping - not primary (role={:?})", role);
            return None;
        }

        // Convert storage WalOperation to replication WalEntry
        let (entry_type, data) = match operation {
            // DML operations
            WalOperation::Insert { .. } => (WalEntryType::Insert, serialize_operation(operation)),
            WalOperation::Update { .. } => (WalEntryType::Update, serialize_operation(operation)),
            WalOperation::Delete { .. } => (WalEntryType::Delete, serialize_operation(operation)),
            WalOperation::Truncate { .. } => (WalEntryType::Delete, serialize_operation(operation)),

            // Table DDL
            WalOperation::CreateTable { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),
            WalOperation::DropTable { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),
            WalOperation::AlterColumnStorage { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),

            // Index operations
            WalOperation::CreateIndex { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),
            WalOperation::DropIndex { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),

            // Trigger operations
            WalOperation::CreateTrigger { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),
            WalOperation::DropTrigger { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),

            // Function/Procedure operations
            WalOperation::CreateFunction { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),
            WalOperation::DropFunction { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),
            WalOperation::CreateProcedure { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),
            WalOperation::DropProcedure { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),

            // Materialized view operations
            WalOperation::CreateMaterializedView { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),
            WalOperation::DropMaterializedView { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),
            WalOperation::RefreshMaterializedView { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),

            // Constraint operations
            WalOperation::AddConstraint { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),
            WalOperation::DropConstraint { .. } => (WalEntryType::SchemaChange, serialize_operation(operation)),

            // Transaction control
            WalOperation::Begin { .. } => (WalEntryType::TxBegin, serialize_operation(operation)),
            WalOperation::Commit { .. } => (WalEntryType::TxCommit, serialize_operation(operation)),
            WalOperation::Abort { .. } => (WalEntryType::TxRollback, serialize_operation(operation)),

            // HA replication operations
            WalOperation::UpdateCounter { .. } => (WalEntryType::Update, serialize_operation(operation)),
        };

        let checksum = crc32fast::hash(&data);
        let entry = WalEntry {
            lsn,
            entry_type,
            data,
            checksum,
        };

        self.broadcast_wal_entry(entry)
    }
}

/// Serialize a WalOperation to bytes for replication
fn serialize_operation(operation: &WalOperation) -> Vec<u8> {
    match bincode::serialize(operation) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("Failed to serialize WAL operation: {}", e);
            Vec::new()
        }
    }
}

impl Default for HAStateRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ha_state_registry() {
        let registry = HAStateRegistry::new();

        // Test default config
        let config = registry.get_config().unwrap();
        assert_eq!(config.role, HARole::Standalone);

        // Test setting config
        registry.set_config(NodeConfig {
            role: HARole::Primary,
            ..Default::default()
        });
        assert_eq!(registry.get_role(), HARole::Primary);

        // Test LSN
        assert_eq!(registry.get_lsn(), 0);
        registry.set_lsn(100);
        assert_eq!(registry.get_lsn(), 100);
        assert_eq!(registry.increment_lsn(), 101);
    }

    #[test]
    fn test_standby_management() {
        let registry = HAStateRegistry::new();
        let standby_id = Uuid::new_v4();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        registry.register_standby(StandbyInfo {
            node_id: standby_id,
            address: "192.168.1.10:5433".to_string(),
            connected_at: now,
            last_heartbeat: now,
            sync_mode: SyncMode::SemiSync,
            current_lsn: 100,
            flush_lsn: 100,
            apply_lsn: 100,
            lag_bytes: 0,
            lag_ms: 0,
            state: StandbyState::Streaming,
        });

        assert_eq!(registry.standby_count(), 1);

        let standbys = registry.get_standbys();
        assert_eq!(standbys.len(), 1);
        assert_eq!(standbys[0].node_id, standby_id);
    }

    #[test]
    fn test_read_only_mode() {
        let registry = HAStateRegistry::new();

        // Standalone is not read-only by default
        registry.set_config(NodeConfig {
            role: HARole::Standalone,
            ..Default::default()
        });
        assert!(!registry.is_read_only());

        // Standby is read-only by default
        registry.set_config(NodeConfig {
            role: HARole::Standby,
            ..Default::default()
        });
        assert!(registry.is_read_only());

        // Primary is not read-only
        registry.set_config(NodeConfig {
            role: HARole::Primary,
            ..Default::default()
        });
        assert!(!registry.is_read_only());
    }
}
