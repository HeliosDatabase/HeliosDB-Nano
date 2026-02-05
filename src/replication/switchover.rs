//! Switchover Coordinator - Orchestrates controlled switchover operations
//!
//! Implements a 5-phase protocol for zero-downtime primary switchover:
//! 1. Preparation - Verify preconditions
//! 2. Synchronization - Drain primary, sync standbys to target LSN
//! 3. Role Change - Demote old primary, promote new primary
//! 4. Reconfiguration - Standbys reconnect to new primary
//! 5. Resumption - Resume normal operations

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::RwLock;
use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;

use crate::{Result, Error};
use super::role_manager::{RoleManager, NodeRole, RoleChangeReason, SwitchoverPhase};
use super::ha_state::HAStateRegistry;

/// Switchover configuration
#[derive(Debug, Clone)]
pub struct SwitchoverConfig {
    /// Maximum time to wait for standbys to sync (default: 30s)
    pub sync_timeout: Duration,
    /// Maximum time for the entire switchover (default: 60s)
    pub total_timeout: Duration,
    /// Minimum number of standbys that must be synced before proceeding
    pub min_synced_standbys: usize,
    /// Whether to allow switchover if not all standbys are synced
    pub allow_partial_sync: bool,
    /// Drain timeout - max time to wait for in-flight transactions (default: 10s)
    pub drain_timeout: Duration,
    /// Health check interval during switchover (default: 100ms)
    pub health_check_interval: Duration,
}

impl Default for SwitchoverConfig {
    fn default() -> Self {
        Self {
            sync_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(60),
            min_synced_standbys: 1,
            allow_partial_sync: false,
            drain_timeout: Duration::from_secs(10),
            health_check_interval: Duration::from_millis(100),
        }
    }
}

/// Standby node status during switchover
#[derive(Debug, Clone)]
pub struct StandbyStatus {
    pub node_id: Uuid,
    pub current_lsn: u64,
    pub target_lsn: u64,
    pub is_synced: bool,
    pub last_seen: Instant,
    pub replication_lag_ms: u64,
}

/// Switchover precondition check result
#[derive(Debug, Clone)]
pub struct SwitchoverCheck {
    pub can_proceed: bool,
    pub target_healthy: bool,
    pub target_lsn: u64,
    pub primary_lsn: u64,
    pub lag_bytes: u64,
    pub synced_standbys: Vec<Uuid>,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
}

/// Command for the switchover coordinator
#[derive(Debug)]
pub enum SwitchoverCommand {
    /// Initiate switchover to target node
    Initiate {
        target_node: Uuid,
        response: oneshot::Sender<Result<Uuid>>,
    },
    /// Cancel ongoing switchover
    Cancel {
        response: oneshot::Sender<Result<()>>,
    },
    /// Check switchover readiness
    Check {
        target_node: Uuid,
        response: oneshot::Sender<Result<SwitchoverCheck>>,
    },
    /// Report standby LSN progress
    StandbyProgress {
        node_id: Uuid,
        lsn: u64,
    },
    /// Standby ready notification
    StandbyReady {
        node_id: Uuid,
    },
    /// Shutdown coordinator
    Shutdown,
}

/// Event broadcast during switchover
#[derive(Debug, Clone)]
pub enum SwitchoverEvent {
    /// Switchover started
    Started {
        switchover_id: Uuid,
        source: Uuid,
        target: Uuid,
    },
    /// Phase changed
    PhaseChanged {
        switchover_id: Uuid,
        phase: SwitchoverPhase,
    },
    /// Prepare to follow new primary (sent to standbys)
    PrepareNewPrimary {
        switchover_id: Uuid,
        new_primary: Uuid,
        new_primary_addr: String,
    },
    /// Switchover completed
    Completed {
        switchover_id: Uuid,
        new_primary: Uuid,
        duration_ms: u64,
    },
    /// Switchover failed
    Failed {
        switchover_id: Uuid,
        error: String,
    },
    /// Switchover cancelled
    Cancelled {
        switchover_id: Uuid,
    },
}

/// Switchover Coordinator
pub struct SwitchoverCoordinator {
    /// This node's ID
    node_id: Uuid,
    /// Role manager reference
    role_manager: Arc<RoleManager>,
    /// HA state registry
    ha_registry: Arc<HAStateRegistry>,
    /// Configuration
    config: SwitchoverConfig,
    /// Command channel sender
    command_tx: mpsc::Sender<SwitchoverCommand>,
    /// Event broadcaster
    event_tx: broadcast::Sender<SwitchoverEvent>,
    /// Node addresses (node_id -> address)
    node_addresses: Arc<RwLock<HashMap<Uuid, String>>>,
    /// In-flight transaction counter (for drain phase)
    in_flight_transactions: Arc<std::sync::atomic::AtomicU64>,
    /// Write block flag (set during drain phase)
    writes_blocked: Arc<std::sync::atomic::AtomicBool>,
}

impl SwitchoverCoordinator {
    /// Create a new switchover coordinator
    pub fn new(
        node_id: Uuid,
        role_manager: Arc<RoleManager>,
        ha_registry: Arc<HAStateRegistry>,
        config: SwitchoverConfig,
    ) -> (Self, mpsc::Receiver<SwitchoverCommand>) {
        let (command_tx, command_rx) = mpsc::channel(64);
        let (event_tx, _) = broadcast::channel(64);

        let coordinator = Self {
            node_id,
            role_manager,
            ha_registry,
            config,
            command_tx,
            event_tx,
            node_addresses: Arc::new(RwLock::new(HashMap::new())),
            in_flight_transactions: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            writes_blocked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        (coordinator, command_rx)
    }

    /// Get command sender for external use
    pub fn command_sender(&self) -> mpsc::Sender<SwitchoverCommand> {
        self.command_tx.clone()
    }

    /// Subscribe to switchover events
    pub fn subscribe(&self) -> broadcast::Receiver<SwitchoverEvent> {
        self.event_tx.subscribe()
    }

    /// Register a node's address
    pub fn register_node_address(&self, node_id: Uuid, address: String) {
        self.node_addresses.write().insert(node_id, address);
    }

    /// Check if writes are blocked (during switchover drain phase)
    pub fn are_writes_blocked(&self) -> bool {
        self.writes_blocked.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Increment in-flight transaction counter
    pub fn begin_transaction(&self) -> Result<TransactionGuard> {
        if self.writes_blocked.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::ha("Writes blocked during switchover"));
        }
        self.in_flight_transactions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(TransactionGuard {
            counter: Arc::clone(&self.in_flight_transactions),
        })
    }

    /// Check switchover readiness without initiating
    pub async fn check_switchover(&self, target_node: Uuid) -> Result<SwitchoverCheck> {
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(SwitchoverCommand::Check {
                target_node,
                response: response_tx,
            })
            .await
            .map_err(|_| Error::ha("Coordinator channel closed"))?;

        response_rx.await.map_err(|_| Error::ha("Response channel closed"))?
    }

    /// Initiate a controlled switchover
    pub async fn initiate_switchover(&self, target_node: Uuid) -> Result<Uuid> {
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(SwitchoverCommand::Initiate {
                target_node,
                response: response_tx,
            })
            .await
            .map_err(|_| Error::ha("Coordinator channel closed"))?;

        response_rx.await.map_err(|_| Error::ha("Response channel closed"))?
    }

    /// Cancel ongoing switchover
    pub async fn cancel_switchover(&self) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(SwitchoverCommand::Cancel {
                response: response_tx,
            })
            .await
            .map_err(|_| Error::ha("Coordinator channel closed"))?;

        response_rx.await.map_err(|_| Error::ha("Response channel closed"))?
    }

    /// Run the coordinator event loop (call on primary node)
    pub async fn run(&self, mut command_rx: mpsc::Receiver<SwitchoverCommand>) {
        tracing::info!("Switchover coordinator started on node {}", self.node_id);

        while let Some(cmd) = command_rx.recv().await {
            match cmd {
                SwitchoverCommand::Initiate { target_node, response } => {
                    let result = self.handle_initiate(target_node).await;
                    let _ = response.send(result);
                }
                SwitchoverCommand::Cancel { response } => {
                    let result = self.handle_cancel().await;
                    let _ = response.send(result);
                }
                SwitchoverCommand::Check { target_node, response } => {
                    let result = self.handle_check(target_node).await;
                    let _ = response.send(result);
                }
                SwitchoverCommand::StandbyProgress { node_id, lsn } => {
                    self.handle_standby_progress(node_id, lsn).await;
                }
                SwitchoverCommand::StandbyReady { node_id } => {
                    self.handle_standby_ready(node_id).await;
                }
                SwitchoverCommand::Shutdown => {
                    tracing::info!("Switchover coordinator shutting down");
                    break;
                }
            }
        }
    }

    /// Handle switchover initiation
    async fn handle_initiate(&self, target_node: Uuid) -> Result<Uuid> {
        // Verify we're the primary
        if !self.role_manager.is_primary() {
            return Err(Error::ha("Only primary can initiate switchover"));
        }

        // Check preconditions
        let check = self.handle_check(target_node).await?;
        if !check.can_proceed {
            return Err(Error::ha(format!(
                "Switchover blocked: {}",
                check.blockers.join(", ")
            )));
        }

        // Begin switchover
        let switchover_id = self.role_manager.begin_switchover(target_node)?;
        let start_time = Instant::now();

        // Broadcast start event
        let _ = self.event_tx.send(SwitchoverEvent::Started {
            switchover_id,
            source: self.node_id,
            target: target_node,
        });

        // Execute switchover phases
        let result = self.execute_switchover(switchover_id, target_node, check.primary_lsn).await;

        match result {
            Ok(()) => {
                let duration = start_time.elapsed();
                let _ = self.event_tx.send(SwitchoverEvent::Completed {
                    switchover_id,
                    new_primary: target_node,
                    duration_ms: duration.as_millis() as u64,
                });
                tracing::info!(
                    "Switchover {} completed in {}ms",
                    switchover_id,
                    duration.as_millis()
                );
                Ok(switchover_id)
            }
            Err(e) => {
                let _ = self.event_tx.send(SwitchoverEvent::Failed {
                    switchover_id,
                    error: e.to_string(),
                });
                // Rollback
                self.rollback_switchover().await;
                Err(e)
            }
        }
    }

    /// Execute the 5-phase switchover protocol
    async fn execute_switchover(
        &self,
        switchover_id: Uuid,
        target_node: Uuid,
        primary_lsn: u64,
    ) -> Result<()> {
        let timeout = Instant::now() + self.config.total_timeout;

        // Phase 1: Preparation (already done in check)
        self.advance_phase(SwitchoverPhase::Preparation)?;
        let _ = self.event_tx.send(SwitchoverEvent::PhaseChanged {
            switchover_id,
            phase: SwitchoverPhase::Preparation,
        });

        // Phase 2: Synchronization
        self.advance_phase(SwitchoverPhase::Synchronization)?;
        let _ = self.event_tx.send(SwitchoverEvent::PhaseChanged {
            switchover_id,
            phase: SwitchoverPhase::Synchronization,
        });

        // Block new writes
        self.writes_blocked.store(true, std::sync::atomic::Ordering::SeqCst);
        tracing::info!("Writes blocked for switchover synchronization");

        // Enter draining state
        self.role_manager.change_role(NodeRole::Draining, RoleChangeReason::Switchover)?;

        // Wait for in-flight transactions to complete
        let drain_deadline = Instant::now() + self.config.drain_timeout;
        while self.in_flight_transactions.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            if Instant::now() > drain_deadline {
                return Err(Error::ha("Drain timeout: in-flight transactions not completing"));
            }
            if Instant::now() > timeout {
                return Err(Error::ha("Switchover timeout during drain phase"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tracing::info!("Primary drained, no in-flight transactions");

        // Get final LSN after drain
        let final_lsn = self.ha_registry.get_lsn();
        self.role_manager.set_switchover_target_lsn(final_lsn)?;

        // Wait for target standby to catch up
        let sync_deadline = Instant::now() + self.config.sync_timeout;
        loop {
            // Check target standby LSN
            let standby_info = self.ha_registry.get_standbys()
                .into_iter()
                .find(|s| s.node_id == target_node);
            if let Some(info) = standby_info {
                if info.apply_lsn >= final_lsn {
                    tracing::info!(
                        "Target standby {} caught up to LSN {}",
                        target_node,
                        final_lsn
                    );
                    break;
                }
            }

            if Instant::now() > sync_deadline {
                return Err(Error::ha("Sync timeout: target standby not caught up"));
            }
            if Instant::now() > timeout {
                return Err(Error::ha("Switchover timeout during sync phase"));
            }
            tokio::time::sleep(self.config.health_check_interval).await;
        }

        // Phase 3: Role Change
        self.advance_phase(SwitchoverPhase::RoleChange)?;
        let _ = self.event_tx.send(SwitchoverEvent::PhaseChanged {
            switchover_id,
            phase: SwitchoverPhase::RoleChange,
        });

        // Demote this node to standby
        self.role_manager.change_role(
            NodeRole::TransitioningToStandby,
            RoleChangeReason::Switchover,
        )?;
        self.role_manager.demote_to_standby(RoleChangeReason::Switchover)?;

        // Notify target to promote (via HA registry broadcast)
        // The target node should be listening for this event
        tracing::info!("Signaling target {} to promote", target_node);

        // Phase 4: Reconfiguration
        self.advance_phase(SwitchoverPhase::Reconfiguration)?;
        let _ = self.event_tx.send(SwitchoverEvent::PhaseChanged {
            switchover_id,
            phase: SwitchoverPhase::Reconfiguration,
        });

        // Notify all standbys of new primary
        let target_addr = self.node_addresses.read().get(&target_node).cloned()
            .unwrap_or_else(|| format!("{}:5433", target_node)); // Default format

        let _ = self.event_tx.send(SwitchoverEvent::PrepareNewPrimary {
            switchover_id,
            new_primary: target_node,
            new_primary_addr: target_addr,
        });

        // Update our primary tracking
        self.role_manager.set_current_primary(Some(target_node));

        // Phase 5: Resumption
        self.advance_phase(SwitchoverPhase::Resumption)?;
        let _ = self.event_tx.send(SwitchoverEvent::PhaseChanged {
            switchover_id,
            phase: SwitchoverPhase::Resumption,
        });

        // Unblock writes (though this node is now standby)
        self.writes_blocked.store(false, std::sync::atomic::Ordering::SeqCst);

        // Mark completed
        self.advance_phase(SwitchoverPhase::Completed)?;

        Ok(())
    }

    /// Handle switchover cancellation
    async fn handle_cancel(&self) -> Result<()> {
        self.role_manager.cancel_switchover()?;
        self.rollback_switchover().await;

        if let Some(state) = self.role_manager.switchover_state() {
            let _ = self.event_tx.send(SwitchoverEvent::Cancelled {
                switchover_id: state.switchover_id,
            });
        }

        Ok(())
    }

    /// Rollback a failed or cancelled switchover
    async fn rollback_switchover(&self) {
        tracing::warn!("Rolling back switchover");

        // Unblock writes
        self.writes_blocked.store(false, std::sync::atomic::Ordering::SeqCst);

        // Restore primary role if we were demoted
        let current_role = self.role_manager.role();
        if matches!(
            current_role,
            NodeRole::Draining | NodeRole::TransitioningToStandby
        ) {
            if let Err(e) = self.role_manager.change_role(NodeRole::Primary, RoleChangeReason::Switchover) {
                tracing::error!("Failed to rollback to primary: {}", e);
            }
        }

        self.role_manager.set_current_primary(Some(self.node_id));
    }

    /// Check switchover preconditions
    async fn handle_check(&self, target_node: Uuid) -> Result<SwitchoverCheck> {
        let mut check = SwitchoverCheck {
            can_proceed: true,
            target_healthy: false,
            target_lsn: 0,
            primary_lsn: self.ha_registry.get_lsn(),
            lag_bytes: 0,
            synced_standbys: vec![],
            warnings: vec![],
            blockers: vec![],
        };

        // Check if we're primary
        if !self.role_manager.is_primary() {
            check.can_proceed = false;
            check.blockers.push("This node is not the primary".to_string());
        }

        // Check if switchover already in progress
        if self.role_manager.is_switchover_in_progress() {
            check.can_proceed = false;
            check.blockers.push("Switchover already in progress".to_string());
        }

        // Check target node health
        let standby_info = self.ha_registry.get_standbys()
            .into_iter()
            .find(|s| s.node_id == target_node);
        if let Some(info) = standby_info {
            check.target_healthy = true;
            check.target_lsn = info.apply_lsn;
            check.lag_bytes = check.primary_lsn.saturating_sub(check.target_lsn);

            if info.apply_lsn < check.primary_lsn {
                let lag = check.primary_lsn - info.apply_lsn;
                check.warnings.push(format!(
                    "Target standby is {} LSN behind (will sync during switchover)",
                    lag
                ));
            }
        } else {
            check.can_proceed = false;
            check.blockers.push(format!(
                "Target node {} not found or not healthy",
                target_node
            ));
        }

        // Check other standbys
        for info in self.ha_registry.get_standbys() {
            if info.apply_lsn >= check.primary_lsn.saturating_sub(100) {
                // Within 100 LSN is considered synced
                check.synced_standbys.push(info.node_id);
            }
        }

        if check.synced_standbys.len() < self.config.min_synced_standbys {
            if self.config.allow_partial_sync {
                check.warnings.push(format!(
                    "Only {} standbys synced (minimum: {})",
                    check.synced_standbys.len(),
                    self.config.min_synced_standbys
                ));
            } else {
                check.can_proceed = false;
                check.blockers.push(format!(
                    "Insufficient synced standbys: {} (need {})",
                    check.synced_standbys.len(),
                    self.config.min_synced_standbys
                ));
            }
        }

        Ok(check)
    }

    /// Handle standby LSN progress report
    async fn handle_standby_progress(&self, node_id: Uuid, lsn: u64) {
        // This is used during switchover to track sync progress
        if let Some(state) = self.role_manager.switchover_state() {
            if node_id == state.target_node {
                if let Some(target_lsn) = state.target_lsn {
                    if lsn >= target_lsn {
                        tracing::info!(
                            "Target standby {} reached target LSN {}",
                            node_id,
                            target_lsn
                        );
                    }
                }
            }
        }
    }

    /// Handle standby ready notification
    async fn handle_standby_ready(&self, node_id: Uuid) {
        tracing::info!("Standby {} reports ready", node_id);
    }

    fn advance_phase(&self, phase: SwitchoverPhase) -> Result<()> {
        self.role_manager.advance_switchover_phase(phase)
    }
}

/// Guard for tracking in-flight transactions
pub struct TransactionGuard {
    counter: Arc<std::sync::atomic::AtomicU64>,
}

impl Drop for TransactionGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Reconnection target for WAL replicator
#[derive(Debug, Clone)]
pub struct ReconnectTarget {
    /// New primary node ID
    pub node_id: Uuid,
    /// New primary address (host:port)
    pub address: String,
}

/// Switchover handler for standby nodes (receives switchover events)
pub struct StandbySwitchoverHandler {
    node_id: Uuid,
    role_manager: Arc<RoleManager>,
    ha_registry: Arc<HAStateRegistry>,
    /// Channel to receive switchover events from primary
    event_rx: broadcast::Receiver<SwitchoverEvent>,
    /// Channel to signal WAL replicator reconnection
    reconnect_tx: Option<mpsc::Sender<ReconnectTarget>>,
}

impl StandbySwitchoverHandler {
    pub fn new(
        node_id: Uuid,
        role_manager: Arc<RoleManager>,
        ha_registry: Arc<HAStateRegistry>,
        event_rx: broadcast::Receiver<SwitchoverEvent>,
    ) -> Self {
        Self {
            node_id,
            role_manager,
            ha_registry,
            event_rx,
            reconnect_tx: None,
        }
    }

    /// Create with a reconnect channel for WAL replicator
    pub fn with_reconnect_channel(
        node_id: Uuid,
        role_manager: Arc<RoleManager>,
        ha_registry: Arc<HAStateRegistry>,
        event_rx: broadcast::Receiver<SwitchoverEvent>,
        reconnect_tx: mpsc::Sender<ReconnectTarget>,
    ) -> Self {
        Self {
            node_id,
            role_manager,
            ha_registry,
            event_rx,
            reconnect_tx: Some(reconnect_tx),
        }
    }

    /// Run the standby switchover handler
    pub async fn run(mut self) {
        tracing::info!("Standby switchover handler started on node {}", self.node_id);

        loop {
            match self.event_rx.recv().await {
                Ok(event) => self.handle_event(event).await,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Standby switchover handler lagged {} events", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("Switchover event channel closed");
                    break;
                }
            }
        }
    }

    async fn handle_event(&self, event: SwitchoverEvent) {
        match event {
            SwitchoverEvent::Started { switchover_id, source, target } => {
                tracing::info!(
                    "Switchover {} started: {} -> {}",
                    switchover_id,
                    source,
                    target
                );

                // If we're the target, prepare for promotion
                if target == self.node_id {
                    tracing::info!("This node is the switchover target - preparing for promotion");
                    if let Err(e) = self.role_manager.change_role(
                        NodeRole::CatchingUp,
                        RoleChangeReason::Switchover,
                    ) {
                        tracing::error!("Failed to enter catching up state: {}", e);
                    }
                }
            }
            SwitchoverEvent::PrepareNewPrimary { switchover_id, new_primary, new_primary_addr } => {
                tracing::info!(
                    "Switchover {}: new primary is {} at {}",
                    switchover_id,
                    new_primary,
                    new_primary_addr
                );

                if new_primary == self.node_id {
                    // We are becoming the new primary
                    if let Err(e) = self.role_manager.promote_to_primary(RoleChangeReason::Switchover) {
                        tracing::error!("Failed to promote to primary: {}", e);
                    } else {
                        tracing::info!("Successfully promoted to primary");
                    }
                } else {
                    // Reconfigure to follow new primary
                    self.role_manager.set_current_primary(Some(new_primary));
                    tracing::info!("Reconfigured to follow new primary {}", new_primary);

                    // Signal WAL replicator to reconnect to new primary
                    if let Some(ref tx) = self.reconnect_tx {
                        let target = ReconnectTarget {
                            node_id: new_primary,
                            address: new_primary_addr.clone(),
                        };
                        if let Err(e) = tx.try_send(target) {
                            tracing::error!("Failed to signal WAL replicator reconnection: {}", e);
                        } else {
                            tracing::info!("Signaled WAL replicator to reconnect to {}", new_primary_addr);
                        }
                    }
                }
            }
            SwitchoverEvent::Completed { switchover_id, new_primary, duration_ms } => {
                tracing::info!(
                    "Switchover {} completed in {}ms, new primary: {}",
                    switchover_id,
                    duration_ms,
                    new_primary
                );
            }
            SwitchoverEvent::Failed { switchover_id, error } => {
                tracing::error!("Switchover {} failed: {}", switchover_id, error);
                // Revert any transitional state
                if self.role_manager.role().is_transitioning() {
                    let _ = self.role_manager.change_role(
                        NodeRole::Standby,
                        RoleChangeReason::Switchover,
                    );
                }
            }
            SwitchoverEvent::Cancelled { switchover_id } => {
                tracing::info!("Switchover {} cancelled", switchover_id);
                // Revert any transitional state
                if self.role_manager.role().is_transitioning() {
                    let _ = self.role_manager.change_role(
                        NodeRole::Standby,
                        RoleChangeReason::Switchover,
                    );
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_switchover_check() {
        // Test would require mocking HA registry
        // This is a placeholder for integration tests
    }
}
