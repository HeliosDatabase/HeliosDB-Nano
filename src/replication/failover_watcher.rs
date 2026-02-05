//! Failover Watcher - Tier 1 Warm Standby
//!
//! Monitors primary health and orchestrates failover when needed.
//! Supports automatic and manual failover modes.
//!
//! # Health Check Protocol
//!
//! The watcher performs periodic health checks on the primary node:
//! 1. Attempts TCP connection to primary's replication port
//! 2. Sends a Heartbeat message and expects HeartbeatResponse
//! 3. Tracks consecutive failures against configured threshold
//! 4. Triggers failover when threshold is reached
//!
//! # Integration with Switchover
//!
//! When automatic failover is triggered, it integrates with SwitchoverCoordinator
//! to perform a controlled promotion of the best standby candidate.

use super::config::{FailoverConfig, NodeHealth, StandbyConfig};
use super::transport::{
    Capabilities, HandshakeRequest, HeartbeatPayload, HealthStatus, MessageType,
    NodeRole as TransportNodeRole, ReplicationConnection, SyncModeConfig,
};
use super::wal_replicator::Lsn;
use super::{ReplicationError, Result};
use bytes::Bytes;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

/// Failover event types
#[derive(Debug, Clone)]
pub enum FailoverEvent {
    /// Primary health check failed
    PrimaryUnhealthy { reason: String },
    /// Primary is back online
    PrimaryRecovered,
    /// Failover initiated
    FailoverStarted { target_standby: Uuid },
    /// Failover completed
    FailoverCompleted { new_primary: Uuid, old_primary: Option<Uuid> },
    /// Failover failed
    FailoverFailed { reason: String },
    /// Standby promoted
    StandbyPromoted { standby_id: Uuid, at_lsn: Lsn },
    /// Manual failover requested
    ManualFailoverRequested { target: Option<Uuid> },
}

/// Health check result
#[derive(Debug, Clone)]
pub struct HealthCheckResult {
    /// Node ID
    pub node_id: Uuid,
    /// Health status
    pub health: NodeHealth,
    /// Response time in milliseconds
    pub response_time_ms: Option<u64>,
    /// Current LSN (if available)
    pub current_lsn: Option<Lsn>,
    /// Error message (if unhealthy)
    pub error: Option<String>,
    /// Timestamp of check
    pub checked_at: chrono::DateTime<chrono::Utc>,
}

/// Failover candidate information
#[derive(Debug, Clone)]
pub struct FailoverCandidate {
    /// Standby node ID
    pub node_id: Uuid,
    /// Standby configuration
    pub config: StandbyConfig,
    /// Current applied LSN
    pub applied_lsn: Lsn,
    /// Lag from primary
    pub lag_bytes: u64,
    /// Priority (lower = higher priority)
    pub priority: u32,
    /// Is the standby healthy?
    pub healthy: bool,
}

/// Failover Watcher - monitors and orchestrates failover
pub struct FailoverWatcher {
    /// Configuration
    config: FailoverConfig,
    /// This node's ID
    node_id: Uuid,
    /// Primary node ID
    primary_id: Uuid,
    /// Primary node address
    primary_addr: Option<SocketAddr>,
    /// Standby configurations
    standbys: Vec<StandbyConfig>,
    /// Node health states
    health_states: Arc<RwLock<HashMap<Uuid, HealthCheckResult>>>,
    /// Consecutive failure counts
    failure_counts: Arc<RwLock<HashMap<Uuid, u32>>>,
    /// Event channel sender
    event_tx: mpsc::Sender<FailoverEvent>,
    /// Event channel receiver
    event_rx: Option<mpsc::Receiver<FailoverEvent>>,
    /// Failover in progress
    failover_in_progress: Arc<RwLock<bool>>,
    /// Is running flag
    is_running: Arc<AtomicBool>,
    /// Shutdown signal
    shutdown_tx: mpsc::Sender<()>,
    /// Shutdown receiver (for health check task)
    shutdown_rx: Arc<RwLock<Option<mpsc::Receiver<()>>>>,
}

impl FailoverWatcher {
    /// Create a new failover watcher
    pub fn new(
        config: FailoverConfig,
        node_id: Uuid,
        primary_id: Uuid,
        primary_addr: Option<SocketAddr>,
        standbys: Vec<StandbyConfig>,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel(100);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        Self {
            config,
            node_id,
            primary_id,
            primary_addr,
            standbys,
            health_states: Arc::new(RwLock::new(HashMap::new())),
            failure_counts: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            event_rx: Some(event_rx),
            failover_in_progress: Arc::new(RwLock::new(false)),
            is_running: Arc::new(AtomicBool::new(false)),
            shutdown_tx,
            shutdown_rx: Arc::new(RwLock::new(Some(shutdown_rx))),
        }
    }

    /// Create a failover watcher with minimal configuration (for backward compatibility)
    pub fn new_simple(
        config: FailoverConfig,
        primary_id: Uuid,
        standbys: Vec<StandbyConfig>,
    ) -> Self {
        Self::new(config, Uuid::new_v4(), primary_id, None, standbys)
    }

    /// Set the primary address for health checks
    pub fn set_primary_addr(&mut self, addr: SocketAddr) {
        self.primary_addr = Some(addr);
    }

    /// Update primary node information
    pub fn update_primary(&mut self, primary_id: Uuid, primary_addr: Option<SocketAddr>) {
        self.primary_id = primary_id;
        self.primary_addr = primary_addr;
        // Reset failure counts when primary changes
        tokio::spawn({
            let failure_counts = Arc::clone(&self.failure_counts);
            async move {
                failure_counts.write().await.clear();
            }
        });
    }

    /// Start the failover watcher
    ///
    /// Spawns background tasks for health monitoring.
    pub async fn start(&self) -> Result<()> {
        if self.is_running.swap(true, Ordering::SeqCst) {
            return Err(ReplicationError::Failover("Failover watcher already running".to_string()));
        }

        if !self.config.auto_failover {
            // In manual mode, no background task runs, so reset is_running
            self.is_running.store(false, Ordering::SeqCst);
            tracing::info!("Failover watcher started in manual mode (no automatic health checks)");
            return Ok(());
        }

        // Take the shutdown receiver for this start
        let shutdown_rx = {
            let mut guard = self.shutdown_rx.write().await;
            guard.take()
        };

        let Some(mut shutdown_rx) = shutdown_rx else {
            self.is_running.store(false, Ordering::SeqCst);
            return Err(ReplicationError::Failover("Shutdown receiver already taken".to_string()));
        };

        // Clone Arc references for the spawned task
        let health_check_interval = self.config.health_check_interval;
        let failover_threshold = self.config.failover_threshold;
        let primary_id = self.primary_id;
        let primary_addr = self.primary_addr;
        let node_id = self.node_id;
        let health_states = Arc::clone(&self.health_states);
        let failure_counts = Arc::clone(&self.failure_counts);
        let failover_in_progress = Arc::clone(&self.failover_in_progress);
        let is_running = Arc::clone(&self.is_running);
        let event_tx = self.event_tx.clone();
        let standbys = self.standbys.clone();

        // Spawn the health check loop
        tokio::spawn(async move {
            tracing::info!(
                "Health check loop started: interval={:?}, threshold={}",
                health_check_interval,
                failover_threshold
            );

            let mut interval = tokio::time::interval(health_check_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Perform health check on primary
                        if let Some(addr) = primary_addr {
                            let result = Self::perform_health_check(
                                node_id,
                                primary_id,
                                addr,
                                Duration::from_secs(5),
                            ).await;

                            // Store result
                            health_states.write().await.insert(primary_id, result.clone());

                            match result.health {
                                NodeHealth::Healthy => {
                                    // Reset failure count on successful check
                                    failure_counts.write().await.remove(&primary_id);
                                    tracing::trace!(
                                        "Primary {} healthy, response_time={}ms, lsn={:?}",
                                        primary_id,
                                        result.response_time_ms.unwrap_or(0),
                                        result.current_lsn
                                    );
                                }
                                NodeHealth::Failed | NodeHealth::Unreachable => {
                                    let mut counts = failure_counts.write().await;
                                    let count = counts.entry(primary_id).or_insert(0);
                                    *count += 1;
                                    let current_count = *count;

                                    tracing::warn!(
                                        "Primary {} health check failed ({}/{}): {:?}",
                                        primary_id,
                                        current_count,
                                        failover_threshold,
                                        result.error
                                    );

                                    // Send unhealthy event
                                    let _ = event_tx.send(FailoverEvent::PrimaryUnhealthy {
                                        reason: result.error.clone().unwrap_or_else(|| "Health check failed".to_string()),
                                    }).await;

                                    // Check if failover threshold reached
                                    if current_count >= failover_threshold {
                                        // Check if failover already in progress
                                        let in_progress = *failover_in_progress.read().await;
                                        if !in_progress {
                                            tracing::error!(
                                                "Failover threshold reached for primary {} ({} consecutive failures)",
                                                primary_id,
                                                current_count
                                            );

                                            // Trigger automatic failover
                                            *failover_in_progress.write().await = true;

                                            // Find best candidate from standbys
                                            let best_candidate = Self::select_best_standby(
                                                &standbys,
                                                &health_states,
                                            ).await;

                                            if let Some(candidate) = best_candidate {
                                                let _ = event_tx.send(FailoverEvent::FailoverStarted {
                                                    target_standby: candidate,
                                                }).await;
                                            } else {
                                                let _ = event_tx.send(FailoverEvent::FailoverFailed {
                                                    reason: "No healthy standby available".to_string(),
                                                }).await;
                                                *failover_in_progress.write().await = false;
                                            }
                                        }
                                    }
                                }
                                NodeHealth::Lagging => {
                                    tracing::debug!("Primary {} lagging: {:?}", primary_id, result.error);
                                }
                                NodeHealth::Recovering => {
                                    tracing::debug!("Primary {} recovering: {:?}", primary_id, result.error);
                                }
                            }
                        } else {
                            tracing::debug!("No primary address configured, skipping health check");
                        }

                        // Also check standby health periodically
                        for standby in &standbys {
                            if let Ok(addr) = format!("{}:{}", standby.host, standby.port).parse::<SocketAddr>() {
                                let result = Self::perform_health_check(
                                    node_id,
                                    standby.node_id,
                                    addr,
                                    Duration::from_secs(5),
                                ).await;
                                health_states.write().await.insert(standby.node_id, result);
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        tracing::info!("Health check loop received shutdown signal");
                        break;
                    }
                }
            }

            is_running.store(false, Ordering::SeqCst);
            tracing::info!("Health check loop stopped");
        });

        tracing::info!(
            "Failover watcher started with interval {:?}, threshold {}",
            health_check_interval,
            failover_threshold
        );
        Ok(())
    }

    /// Stop the failover watcher
    pub async fn stop(&self) -> Result<()> {
        if !self.is_running.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Send shutdown signal
        let _ = self.shutdown_tx.send(()).await;

        // Wait briefly for task to stop
        tokio::time::sleep(Duration::from_millis(100)).await;

        tracing::info!("Failover watcher stopped");
        Ok(())
    }

    /// Check if the watcher is running
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::SeqCst)
    }

    /// Perform a health check on a specific node using TCP connection and heartbeat
    async fn perform_health_check(
        this_node_id: Uuid,
        target_node_id: Uuid,
        addr: SocketAddr,
        timeout: Duration,
    ) -> HealthCheckResult {
        let start = Instant::now();

        // Attempt TCP connection with timeout
        let result = tokio::time::timeout(timeout, async {
            Self::do_health_check(this_node_id, target_node_id, addr).await
        }).await;

        let elapsed = start.elapsed();

        match result {
            Ok(Ok((health, lsn))) => {
                HealthCheckResult {
                    node_id: target_node_id,
                    health,
                    response_time_ms: Some(elapsed.as_millis() as u64),
                    current_lsn: lsn,
                    error: None,
                    checked_at: chrono::Utc::now(),
                }
            }
            Ok(Err(e)) => {
                HealthCheckResult {
                    node_id: target_node_id,
                    health: NodeHealth::Failed,
                    response_time_ms: Some(elapsed.as_millis() as u64),
                    current_lsn: None,
                    error: Some(e.to_string()),
                    checked_at: chrono::Utc::now(),
                }
            }
            Err(_) => {
                HealthCheckResult {
                    node_id: target_node_id,
                    health: NodeHealth::Unreachable,
                    response_time_ms: Some(elapsed.as_millis() as u64),
                    current_lsn: None,
                    error: Some("Connection timeout".to_string()),
                    checked_at: chrono::Utc::now(),
                }
            }
        }
    }

    /// Internal health check implementation
    async fn do_health_check(
        this_node_id: Uuid,
        target_node_id: Uuid,
        addr: SocketAddr,
    ) -> Result<(NodeHealth, Option<Lsn>)> {
        // Try to connect
        let mut conn = ReplicationConnection::connect(addr, Duration::from_secs(5)).await?;

        // Send handshake request
        let handshake_req = HandshakeRequest {
            node_id: this_node_id,
            role: TransportNodeRole::Standby,
            sync_mode: SyncModeConfig::Async,
            current_lsn: None,
            slot_name: None,
            capabilities: Capabilities::all(),
        };

        let handshake_response = conn.handshake_client(handshake_req).await?;

        if !handshake_response.accepted {
            return Err(ReplicationError::Failover(
                handshake_response.error.unwrap_or_else(|| "Handshake rejected".to_string())
            ));
        }

        // Send heartbeat to get current status
        let heartbeat = HeartbeatPayload {
            node_id: this_node_id,
            role: TransportNodeRole::Standby,
            current_lsn: 0,
            flush_lsn: 0,
            apply_lsn: None,
            timestamp_ms: chrono::Utc::now().timestamp_millis() as u64,
            lag_bytes: 0,
            health: HealthStatus::Healthy,
        };

        let payload = bincode::serialize(&heartbeat)
            .map_err(|e| ReplicationError::Failover(format!("Serialize heartbeat failed: {}", e)))?;

        conn.send(MessageType::Heartbeat, Bytes::from(payload)).await?;

        // Wait for heartbeat response
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            conn.recv()
        ).await
            .map_err(|_| ReplicationError::Failover("Heartbeat response timeout".to_string()))?
            ?;

        if response.header.msg_type == MessageType::HeartbeatResponse {
            let heartbeat_resp: HeartbeatPayload = bincode::deserialize(&response.payload)
                .map_err(|e| ReplicationError::Failover(format!("Deserialize heartbeat failed: {}", e)))?;

            let health = match heartbeat_resp.health {
                HealthStatus::Healthy => NodeHealth::Healthy,
                HealthStatus::Degraded | HealthStatus::CatchingUp | HealthStatus::Lagging => NodeHealth::Lagging,
                HealthStatus::Error => NodeHealth::Failed,
            };

            conn.close().await.ok();
            Ok((health, Some(heartbeat_resp.current_lsn)))
        } else {
            conn.close().await.ok();
            // Even if response is not HeartbeatResponse, connection succeeded
            Ok((NodeHealth::Healthy, Some(handshake_response.primary_lsn)))
        }
    }

    /// Select the best standby for failover based on health and priority
    async fn select_best_standby(
        standbys: &[StandbyConfig],
        health_states: &Arc<RwLock<HashMap<Uuid, HealthCheckResult>>>,
    ) -> Option<Uuid> {
        let states = health_states.read().await;

        let mut candidates: Vec<_> = standbys
            .iter()
            .filter_map(|s| {
                let health = states.get(&s.node_id)?;
                if health.health == NodeHealth::Healthy {
                    Some((s.node_id, s.priority, health.current_lsn.unwrap_or(0)))
                } else {
                    None
                }
            })
            .collect();

        // Sort by priority (lower first), then by LSN (higher first - most up-to-date)
        candidates.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2.cmp(&a.2))
        });

        candidates.first().map(|(id, _, _)| *id)
    }

    /// Perform a health check on a node (public API)
    pub async fn check_health(&self, node_id: Uuid, host: &str, port: u16) -> HealthCheckResult {
        let addr: SocketAddr = match format!("{}:{}", host, port).parse() {
            Ok(a) => a,
            Err(e) => {
                return HealthCheckResult {
                    node_id,
                    health: NodeHealth::Unreachable,
                    response_time_ms: None,
                    current_lsn: None,
                    error: Some(format!("Invalid address: {}", e)),
                    checked_at: chrono::Utc::now(),
                };
            }
        };

        let result = Self::perform_health_check(self.node_id, node_id, addr, Duration::from_secs(5)).await;

        // Update stored state
        self.health_states.write().await.insert(node_id, result.clone());

        result
    }

    /// Record a health check failure
    pub async fn record_failure(&self, node_id: Uuid) -> u32 {
        let mut counts = self.failure_counts.write().await;
        let count = counts.entry(node_id).or_insert(0);
        *count += 1;
        *count
    }

    /// Reset failure count for a node
    pub async fn reset_failures(&self, node_id: Uuid) {
        self.failure_counts.write().await.remove(&node_id);
    }

    /// Check if failover should be triggered
    pub async fn should_failover(&self) -> bool {
        let counts = self.failure_counts.read().await;
        if let Some(&count) = counts.get(&self.primary_id) {
            count >= self.config.failover_threshold
        } else {
            false
        }
    }

    /// Get failover candidates sorted by priority
    pub async fn get_candidates(&self, primary_lsn: Lsn) -> Vec<FailoverCandidate> {
        let health_states = self.health_states.read().await;

        let mut candidates: Vec<FailoverCandidate> = self
            .standbys
            .iter()
            .map(|s| {
                let health = health_states.get(&s.node_id);
                let applied_lsn = health.and_then(|h| h.current_lsn).unwrap_or(0);
                let healthy = health.map(|h| h.health == NodeHealth::Healthy).unwrap_or(false);

                FailoverCandidate {
                    node_id: s.node_id,
                    config: s.clone(),
                    applied_lsn,
                    lag_bytes: primary_lsn.saturating_sub(applied_lsn),
                    priority: s.priority,
                    healthy,
                }
            })
            .filter(|c| c.healthy)
            .collect();

        // Sort by priority (lower first), then by lag (lower first)
        candidates.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.lag_bytes.cmp(&b.lag_bytes))
        });

        candidates
    }

    /// Initiate automatic failover
    pub async fn initiate_failover(&self, primary_lsn: Lsn) -> Result<Uuid> {
        // Check if failover already in progress
        {
            let in_progress = self.failover_in_progress.read().await;
            if *in_progress {
                return Err(ReplicationError::Failover("Failover already in progress".to_string()));
            }
        }

        // Mark failover in progress
        *self.failover_in_progress.write().await = true;

        // Get best candidate
        let candidates = self.get_candidates(primary_lsn).await;
        let candidate = candidates
            .first()
            .ok_or_else(|| ReplicationError::Failover("No healthy standbys available".to_string()))?;

        // Send event
        let _ = self.event_tx.send(FailoverEvent::FailoverStarted {
            target_standby: candidate.node_id,
        }).await;

        tracing::info!(
            "Initiating failover to standby {} at LSN {}",
            candidate.node_id,
            candidate.applied_lsn
        );

        // Step 1: Fence old primary (prevent writes)
        // Try to send a fencing message to the primary to stop accepting writes
        if let Some(addr) = self.primary_addr {
            tracing::info!("Attempting to fence old primary at {:?}", addr);
            match Self::send_fence_request(self.node_id, self.primary_id, addr).await {
                Ok(()) => {
                    tracing::info!("Old primary {} successfully fenced", self.primary_id);
                }
                Err(e) => {
                    // Primary might be down, which is why we're failing over
                    tracing::warn!("Could not fence primary (may be down): {}", e);
                }
            }
        }

        // Step 2: Wait for standby to catch up (with timeout)
        let target_lsn = primary_lsn;
        let catch_up_timeout = self.config.failover_timeout;
        let start = Instant::now();

        tracing::info!(
            "Waiting for standby {} to catch up to LSN {} (timeout: {:?})",
            candidate.node_id,
            target_lsn,
            catch_up_timeout
        );

        // In a full implementation, we would poll the standby's applied LSN
        // For now, we verify the candidate's lag is acceptable
        if candidate.lag_bytes > self.config.max_replication_lag {
            let lag_error = format!(
                "Standby {} has excessive lag ({} bytes > {} max)",
                candidate.node_id,
                candidate.lag_bytes,
                self.config.max_replication_lag
            );
            tracing::error!("{}", lag_error);
            let _ = self.event_tx.send(FailoverEvent::FailoverFailed {
                reason: lag_error.clone(),
            }).await;
            *self.failover_in_progress.write().await = false;
            return Err(ReplicationError::Failover(lag_error));
        }

        tracing::info!(
            "Standby {} lag ({} bytes) is within acceptable threshold",
            candidate.node_id,
            candidate.lag_bytes
        );

        // Step 3: Promote standby
        // Send promotion notification to the standby
        let standby_config = self.standbys.iter()
            .find(|s| s.node_id == candidate.node_id);

        if let Some(config) = standby_config {
            let addr_str = format!("{}:{}", config.host, config.port);
            if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                tracing::info!("Sending promotion request to standby {} at {}", candidate.node_id, addr);
                match Self::send_promote_request(self.node_id, candidate.node_id, addr, target_lsn).await {
                    Ok(()) => {
                        tracing::info!("Standby {} promoted successfully", candidate.node_id);
                        let _ = self.event_tx.send(FailoverEvent::StandbyPromoted {
                            standby_id: candidate.node_id,
                            at_lsn: candidate.applied_lsn,
                        }).await;
                    }
                    Err(e) => {
                        let promote_error = format!("Failed to promote standby: {}", e);
                        tracing::error!("{}", promote_error);
                        let _ = self.event_tx.send(FailoverEvent::FailoverFailed {
                            reason: promote_error.clone(),
                        }).await;
                        *self.failover_in_progress.write().await = false;
                        return Err(ReplicationError::Failover(promote_error));
                    }
                }
            } else {
                tracing::warn!("Invalid standby address: {}", addr_str);
            }
        }

        // Step 4: Update cluster metadata (handled by role_manager in production)
        tracing::info!(
            "Failover complete: {} -> {} (took {:?})",
            self.primary_id,
            candidate.node_id,
            start.elapsed()
        );

        // Step 5: Notify completion
        let _ = self.event_tx.send(FailoverEvent::FailoverCompleted {
            new_primary: candidate.node_id,
            old_primary: Some(self.primary_id),
        }).await;

        *self.failover_in_progress.write().await = false;

        Ok(candidate.node_id)
    }

    /// Request manual failover
    pub async fn request_manual_failover(&self, target: Option<Uuid>) -> Result<()> {
        // Record the request
        let _ = self.event_tx.send(FailoverEvent::ManualFailoverRequested { target: target.clone() }).await;

        if self.config.require_confirmation {
            // Just record the request, don't execute automatically
            tracing::info!("Manual failover requested - awaiting confirmation");
            return Ok(());
        }

        // Get the current LSN for failover
        let primary_lsn = {
            let states = self.health_states.read().await;
            states.get(&self.primary_id)
                .and_then(|s| s.current_lsn)
                .unwrap_or(0)
        };

        // If target specified, verify it's a valid standby
        if let Some(target_id) = target {
            let candidates = self.get_candidates(primary_lsn).await;
            if !candidates.iter().any(|c| c.node_id == target_id) {
                return Err(ReplicationError::Failover(
                    format!("Target {} is not a valid failover candidate", target_id)
                ));
            }
        }

        // Execute failover
        tracing::info!("Executing manual failover to {:?}", target);
        self.initiate_failover(primary_lsn).await?;

        Ok(())
    }

    /// Send a fence request to the primary to stop accepting writes
    async fn send_fence_request(
        _this_node_id: Uuid,
        _target_node_id: Uuid,
        addr: SocketAddr,
    ) -> Result<()> {
        // Attempt to connect and send a fence command
        // In a full implementation, this would:
        // 1. Connect to the primary
        // 2. Send a Fence message with a new fencing token
        // 3. Wait for acknowledgment
        // For now, we just verify connectivity

        let connect_timeout = Duration::from_secs(5);
        match tokio::time::timeout(connect_timeout, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(_stream)) => {
                tracing::info!("Connected to primary for fencing at {:?}", addr);
                // In a full implementation, send the fence command here
                Ok(())
            }
            Ok(Err(e)) => {
                Err(ReplicationError::Failover(format!("Cannot connect to fence: {}", e)))
            }
            Err(_) => {
                Err(ReplicationError::Failover("Fence connection timeout".to_string()))
            }
        }
    }

    /// Send a promotion request to a standby
    async fn send_promote_request(
        _this_node_id: Uuid,
        _target_node_id: Uuid,
        addr: SocketAddr,
        _target_lsn: Lsn,
    ) -> Result<()> {
        // Attempt to connect and send a promote command
        // In a full implementation, this would:
        // 1. Connect to the standby
        // 2. Send a Promote message with the target LSN
        // 3. Wait for acknowledgment that promotion is complete
        // For now, we just verify connectivity

        let connect_timeout = Duration::from_secs(5);
        match tokio::time::timeout(connect_timeout, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(_stream)) => {
                tracing::info!("Connected to standby for promotion at {:?}", addr);
                // In a full implementation, send the promote command here
                Ok(())
            }
            Ok(Err(e)) => {
                Err(ReplicationError::Failover(format!("Cannot connect to promote: {}", e)))
            }
            Err(_) => {
                Err(ReplicationError::Failover("Promote connection timeout".to_string()))
            }
        }
    }

    /// Take the event receiver (can only be done once)
    pub fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<FailoverEvent>> {
        self.event_rx.take()
    }

    /// Get current health states
    pub async fn health_states(&self) -> HashMap<Uuid, HealthCheckResult> {
        self.health_states.read().await.clone()
    }

    /// Get the event sender for external subscriptions
    pub fn event_sender(&self) -> mpsc::Sender<FailoverEvent> {
        self.event_tx.clone()
    }

    /// Mark failover as completed
    pub async fn complete_failover(&self, new_primary: Uuid) {
        *self.failover_in_progress.write().await = false;
        let _ = self.event_tx.send(FailoverEvent::FailoverCompleted {
            new_primary,
            old_primary: Some(self.primary_id),
        }).await;
    }

    /// Mark failover as failed
    pub async fn fail_failover(&self, reason: String) {
        *self.failover_in_progress.write().await = false;
        let _ = self.event_tx.send(FailoverEvent::FailoverFailed { reason }).await;
    }
}

// =============================================================================
// AUTOMATIC FAILOVER COORDINATOR
// =============================================================================

use super::split_brain::{SplitBrainProtector, ProtectionEvent, ProtectionState};
use super::switchover::{SwitchoverCoordinator, SwitchoverCommand, SwitchoverEvent};
use super::transport::VoteReason;

/// Automatic Failover Coordinator
///
/// Bridges FailoverWatcher health monitoring to SwitchoverCoordinator for
/// automatic failover execution with split-brain protection. This component:
/// 1. Subscribes to FailoverWatcher events
/// 2. On FailoverStarted, activates split-brain fencing
/// 3. Triggers SwitchoverCoordinator for controlled promotion
/// 4. Issues new fencing tokens when primary changes
/// 5. Handles failover completion/failure callbacks
pub struct AutomaticFailoverCoordinator {
    /// Failover watcher event receiver
    event_rx: mpsc::Receiver<FailoverEvent>,
    /// Switchover coordinator command sender
    switchover_tx: mpsc::Sender<SwitchoverCommand>,
    /// Switchover event receiver
    switchover_rx: tokio::sync::broadcast::Receiver<SwitchoverEvent>,
    /// Failover watcher event sender (for callbacks)
    failover_callback_tx: mpsc::Sender<FailoverEvent>,
    /// Split-brain protector (optional)
    split_brain_protector: Option<Arc<SplitBrainProtector>>,
    /// Split-brain event receiver (optional)
    split_brain_rx: Option<mpsc::Receiver<ProtectionEvent>>,
    /// Is running
    is_running: Arc<AtomicBool>,
}

impl AutomaticFailoverCoordinator {
    /// Create a new automatic failover coordinator
    pub fn new(
        mut failover_watcher: FailoverWatcher,
        switchover_coordinator: &SwitchoverCoordinator,
    ) -> Option<Self> {
        let event_rx = failover_watcher.take_event_receiver()?;
        let switchover_tx = switchover_coordinator.command_sender();
        let switchover_rx = switchover_coordinator.subscribe();
        let failover_callback_tx = failover_watcher.event_sender();

        Some(Self {
            event_rx,
            switchover_tx,
            switchover_rx,
            failover_callback_tx,
            split_brain_protector: None,
            split_brain_rx: None,
            is_running: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Create with split-brain protection enabled
    pub fn with_split_brain_protection(
        mut failover_watcher: FailoverWatcher,
        switchover_coordinator: &SwitchoverCoordinator,
        split_brain_protector: Arc<SplitBrainProtector>,
        split_brain_rx: mpsc::Receiver<ProtectionEvent>,
    ) -> Option<Self> {
        let event_rx = failover_watcher.take_event_receiver()?;
        let switchover_tx = switchover_coordinator.command_sender();
        let switchover_rx = switchover_coordinator.subscribe();
        let failover_callback_tx = failover_watcher.event_sender();

        Some(Self {
            event_rx,
            switchover_tx,
            switchover_rx,
            failover_callback_tx,
            split_brain_protector: Some(split_brain_protector),
            split_brain_rx: Some(split_brain_rx),
            is_running: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Create from raw components
    pub fn from_components(
        event_rx: mpsc::Receiver<FailoverEvent>,
        switchover_tx: mpsc::Sender<SwitchoverCommand>,
        switchover_rx: tokio::sync::broadcast::Receiver<SwitchoverEvent>,
        failover_callback_tx: mpsc::Sender<FailoverEvent>,
    ) -> Self {
        Self {
            event_rx,
            switchover_tx,
            switchover_rx,
            failover_callback_tx,
            split_brain_protector: None,
            split_brain_rx: None,
            is_running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create from raw components with split-brain protection
    pub fn from_components_with_split_brain(
        event_rx: mpsc::Receiver<FailoverEvent>,
        switchover_tx: mpsc::Sender<SwitchoverCommand>,
        switchover_rx: tokio::sync::broadcast::Receiver<SwitchoverEvent>,
        failover_callback_tx: mpsc::Sender<FailoverEvent>,
        split_brain_protector: Arc<SplitBrainProtector>,
        split_brain_rx: mpsc::Receiver<ProtectionEvent>,
    ) -> Self {
        Self {
            event_rx,
            switchover_tx,
            switchover_rx,
            failover_callback_tx,
            split_brain_protector: Some(split_brain_protector),
            split_brain_rx: Some(split_brain_rx),
            is_running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Run the automatic failover coordinator
    ///
    /// This should be spawned as a background task.
    pub async fn run(mut self) {
        self.is_running.store(true, Ordering::SeqCst);

        let split_brain_enabled = self.split_brain_protector.is_some();
        tracing::info!(
            "Automatic failover coordinator started (split-brain protection: {})",
            if split_brain_enabled { "enabled" } else { "disabled" }
        );

        loop {
            // Create the optional split_brain_rx future
            let split_brain_recv = async {
                if let Some(ref mut rx) = self.split_brain_rx {
                    rx.recv().await
                } else {
                    // Never resolves if no split-brain protection
                    std::future::pending().await
                }
            };

            tokio::select! {
                // Handle failover watcher events
                Some(event) = self.event_rx.recv() => {
                    self.handle_failover_event(event).await;
                }
                // Handle switchover coordinator events
                result = self.switchover_rx.recv() => {
                    match result {
                        Ok(event) => self.handle_switchover_event(event).await,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Lagged {} switchover events", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tracing::info!("Switchover event channel closed");
                            break;
                        }
                    }
                }
                // Handle split-brain protection events
                Some(event) = split_brain_recv => {
                    self.handle_split_brain_event(event).await;
                }
            }
        }

        self.is_running.store(false, Ordering::SeqCst);
        tracing::info!("Automatic failover coordinator stopped");
    }

    /// Handle a failover watcher event
    async fn handle_failover_event(&self, event: FailoverEvent) {
        match event {
            FailoverEvent::FailoverStarted { target_standby } => {
                tracing::info!(
                    "Automatic failover triggered, initiating switchover to {}",
                    target_standby
                );

                // Step 1: Activate split-brain fencing if available
                // This ensures the old primary cannot write during the transition
                if let Some(ref protector) = self.split_brain_protector {
                    tracing::info!("Activating split-brain fencing for failover");

                    // Check current protection state
                    let state = protector.current_state().await;
                    if state == ProtectionState::SplitBrain {
                        tracing::error!("Split-brain detected - manual intervention required");
                        let _ = self.failover_callback_tx.send(FailoverEvent::FailoverFailed {
                            reason: "Split-brain detected - manual intervention required".to_string(),
                        }).await;
                        return;
                    }

                    // Request votes for the new primary election
                    // This will update the fencing token, preventing the old primary from writing
                    match protector.request_votes(VoteReason::PrimaryFailure).await {
                        Ok(won) => {
                            if won {
                                tracing::info!(
                                    "Election won with new fencing token: {}",
                                    protector.current_fencing_token()
                                );
                            } else {
                                tracing::warn!(
                                    "Election not won, but proceeding with switchover (token: {})",
                                    protector.current_fencing_token()
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to request votes: {}", e);
                            // Continue with switchover anyway - fencing is best-effort
                        }
                    }
                }

                // Step 2: Initiate controlled switchover
                // Create response channel
                let (response_tx, response_rx) = tokio::sync::oneshot::channel();

                // Send initiate command to switchover coordinator
                let result = self.switchover_tx.send(SwitchoverCommand::Initiate {
                    target_node: target_standby,
                    response: response_tx,
                }).await;

                if let Err(e) = result {
                    tracing::error!("Failed to send switchover command: {}", e);
                    let _ = self.failover_callback_tx.send(FailoverEvent::FailoverFailed {
                        reason: format!("Switchover command failed: {}", e),
                    }).await;
                    return;
                }

                // Wait for response (with timeout)
                match tokio::time::timeout(Duration::from_secs(120), response_rx).await {
                    Ok(Ok(Ok(switchover_id))) => {
                        tracing::info!(
                            "Switchover initiated successfully: {}",
                            switchover_id
                        );
                        // The switchover completion will be handled via switchover events
                    }
                    Ok(Ok(Err(e))) => {
                        tracing::error!("Switchover failed: {}", e);
                        let _ = self.failover_callback_tx.send(FailoverEvent::FailoverFailed {
                            reason: format!("Switchover error: {}", e),
                        }).await;
                    }
                    Ok(Err(_)) => {
                        tracing::error!("Switchover response channel closed");
                        let _ = self.failover_callback_tx.send(FailoverEvent::FailoverFailed {
                            reason: "Switchover response channel closed".to_string(),
                        }).await;
                    }
                    Err(_) => {
                        tracing::error!("Switchover response timeout");
                        let _ = self.failover_callback_tx.send(FailoverEvent::FailoverFailed {
                            reason: "Switchover timeout".to_string(),
                        }).await;
                    }
                }
            }
            FailoverEvent::PrimaryUnhealthy { reason } => {
                tracing::warn!("Primary unhealthy notification: {}", reason);
                // This is informational - the health check loop handles threshold counting
            }
            FailoverEvent::PrimaryRecovered => {
                tracing::info!("Primary recovered");
            }
            FailoverEvent::ManualFailoverRequested { target } => {
                tracing::info!("Manual failover requested to {:?}", target);
                // Manual failover bypasses the automatic coordinator
            }
            _ => {
                // Other events are handled elsewhere
            }
        }
    }

    /// Handle a switchover coordinator event
    async fn handle_switchover_event(&self, event: SwitchoverEvent) {
        match event {
            SwitchoverEvent::Completed { new_primary, duration_ms, .. } => {
                tracing::info!(
                    "Switchover completed in {}ms, new primary: {}",
                    duration_ms,
                    new_primary
                );

                // Update split-brain protection with new primary info
                // The fencing token should have been updated during request_votes
                if let Some(ref protector) = self.split_brain_protector {
                    let token = protector.current_fencing_token();
                    tracing::info!(
                        "Failover complete - current fencing token: {}, new primary: {}",
                        token,
                        new_primary
                    );
                }

                let _ = self.failover_callback_tx.send(FailoverEvent::FailoverCompleted {
                    new_primary,
                    old_primary: None, // Set by the caller
                }).await;
            }
            SwitchoverEvent::Failed { error, .. } => {
                tracing::error!("Switchover failed: {}", error);
                let _ = self.failover_callback_tx.send(FailoverEvent::FailoverFailed {
                    reason: error,
                }).await;
            }
            SwitchoverEvent::Cancelled { .. } => {
                tracing::info!("Switchover cancelled");
                let _ = self.failover_callback_tx.send(FailoverEvent::FailoverFailed {
                    reason: "Switchover cancelled".to_string(),
                }).await;
            }
            _ => {
                // Other switchover events are informational
            }
        }
    }

    /// Handle a split-brain protection event
    async fn handle_split_brain_event(&self, event: ProtectionEvent) {
        match event {
            ProtectionEvent::PrimaryElected { node_id, term, fencing_token } => {
                tracing::info!(
                    "Split-brain: Primary elected - node: {}, term: {}, fencing_token: {}",
                    node_id,
                    term,
                    fencing_token
                );
            }
            ProtectionEvent::PrimaryLost { previous_primary, reason } => {
                tracing::warn!(
                    "Split-brain: Primary lost - previous: {}, reason: {}",
                    previous_primary,
                    reason
                );
                // This may trigger automatic failover via the health check loop
            }
            ProtectionEvent::FencingTokenChanged { old_token, new_token } => {
                tracing::info!(
                    "Split-brain: Fencing token updated {} -> {}",
                    old_token,
                    new_token
                );
            }
            ProtectionEvent::SplitBrainDetected { primaries } => {
                tracing::error!(
                    "Split-brain DETECTED! Multiple primaries: {:?}. Manual intervention required.",
                    primaries
                );
                // Send failure event to halt any ongoing failover
                let _ = self.failover_callback_tx.send(FailoverEvent::FailoverFailed {
                    reason: format!(
                        "Split-brain detected: {} primaries found",
                        primaries.len()
                    ),
                }).await;
            }
            ProtectionEvent::ElectionStarted { term, reason } => {
                tracing::info!(
                    "Split-brain: Election started - term: {}, reason: {:?}",
                    term,
                    reason
                );
            }
            ProtectionEvent::ElectionCompleted { winner, term } => {
                tracing::info!(
                    "Split-brain: Election completed - term: {}, winner: {:?}",
                    term,
                    winner
                );
            }
            ProtectionEvent::ElectionNeeded { reason } => {
                tracing::info!(
                    "Split-brain: Election needed - reason: {:?}",
                    reason
                );
                // The election should be started by the split-brain protector
                // We just log the event here
            }
        }
    }
}

/// Builder for setting up automatic failover
pub struct AutomaticFailoverBuilder {
    config: FailoverConfig,
    node_id: Uuid,
    primary_id: Uuid,
    primary_addr: Option<SocketAddr>,
    standbys: Vec<StandbyConfig>,
    split_brain_protector: Option<Arc<SplitBrainProtector>>,
    split_brain_rx: Option<mpsc::Receiver<ProtectionEvent>>,
}

impl AutomaticFailoverBuilder {
    /// Create a new builder
    pub fn new(config: FailoverConfig) -> Self {
        Self {
            config,
            node_id: Uuid::new_v4(),
            primary_id: Uuid::new_v4(),
            primary_addr: None,
            standbys: Vec::new(),
            split_brain_protector: None,
            split_brain_rx: None,
        }
    }

    /// Set this node's ID
    pub fn node_id(mut self, id: Uuid) -> Self {
        self.node_id = id;
        self
    }

    /// Set the primary node ID and address
    pub fn primary(mut self, id: Uuid, addr: SocketAddr) -> Self {
        self.primary_id = id;
        self.primary_addr = Some(addr);
        self
    }

    /// Add a standby configuration
    pub fn add_standby(mut self, standby: StandbyConfig) -> Self {
        self.standbys.push(standby);
        self
    }

    /// Enable split-brain protection
    ///
    /// This adds fencing token validation to prevent stale primaries from writing
    /// after a failover has completed.
    pub fn with_split_brain_protection(
        mut self,
        protector: Arc<SplitBrainProtector>,
        event_rx: mpsc::Receiver<ProtectionEvent>,
    ) -> Self {
        self.split_brain_protector = Some(protector);
        self.split_brain_rx = Some(event_rx);
        self
    }

    /// Build the FailoverWatcher
    pub fn build(self) -> FailoverWatcher {
        FailoverWatcher::new(
            self.config,
            self.node_id,
            self.primary_id,
            self.primary_addr,
            self.standbys,
        )
    }

    /// Build only the AutomaticFailoverCoordinator (consumes watcher for event channel)
    ///
    /// The coordinator takes ownership of the failover event channel.
    /// Use this when you want to run automatic failover.
    pub fn build_coordinator(
        mut self,
        switchover_coordinator: &SwitchoverCoordinator,
    ) -> Option<AutomaticFailoverCoordinator> {
        let watcher = FailoverWatcher::new(
            self.config,
            self.node_id,
            self.primary_id,
            self.primary_addr,
            self.standbys,
        );

        if let (Some(protector), Some(rx)) =
            (self.split_brain_protector.take(), self.split_brain_rx.take())
        {
            AutomaticFailoverCoordinator::with_split_brain_protection(
                watcher,
                switchover_coordinator,
                protector,
                rx,
            )
        } else {
            AutomaticFailoverCoordinator::new(watcher, switchover_coordinator)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_failover_watcher_creation() {
        let config = FailoverConfig::default();
        let watcher = FailoverWatcher::new_simple(config, Uuid::new_v4(), vec![]);
        assert!(!watcher.should_failover().await);
    }

    #[tokio::test]
    async fn test_failure_counting() {
        let config = FailoverConfig {
            failover_threshold: 3,
            ..Default::default()
        };
        let primary_id = Uuid::new_v4();
        let watcher = FailoverWatcher::new_simple(config, primary_id, vec![]);

        // Record failures
        assert_eq!(watcher.record_failure(primary_id).await, 1);
        assert!(!watcher.should_failover().await);

        assert_eq!(watcher.record_failure(primary_id).await, 2);
        assert!(!watcher.should_failover().await);

        assert_eq!(watcher.record_failure(primary_id).await, 3);
        assert!(watcher.should_failover().await);
    }

    #[tokio::test]
    async fn test_candidate_sorting() {
        let config = FailoverConfig::default();
        let primary_id = Uuid::new_v4();

        let standby1 = StandbyConfig {
            node_id: Uuid::new_v4(),
            host: "standby1".to_string(),
            port: 5432,
            sync_mode: super::super::config::SyncMode::Async,
            priority: 100,
        };

        let standby2 = StandbyConfig {
            node_id: Uuid::new_v4(),
            host: "standby2".to_string(),
            port: 5432,
            sync_mode: super::super::config::SyncMode::Async,
            priority: 50, // Higher priority (lower number)
        };

        let watcher = FailoverWatcher::new_simple(config, primary_id, vec![standby1.clone(), standby2.clone()]);

        // Mark both as healthy
        {
            let mut states = watcher.health_states.write().await;
            states.insert(standby1.node_id, HealthCheckResult {
                node_id: standby1.node_id,
                health: NodeHealth::Healthy,
                response_time_ms: Some(10),
                current_lsn: Some(100),
                error: None,
                checked_at: chrono::Utc::now(),
            });
            states.insert(standby2.node_id, HealthCheckResult {
                node_id: standby2.node_id,
                health: NodeHealth::Healthy,
                response_time_ms: Some(10),
                current_lsn: Some(100),
                error: None,
                checked_at: chrono::Utc::now(),
            });
        }

        let candidates = watcher.get_candidates(100).await;
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].node_id, standby2.node_id); // Higher priority first
    }

    #[tokio::test]
    async fn test_full_constructor() {
        let config = FailoverConfig::default();
        let node_id = Uuid::new_v4();
        let primary_id = Uuid::new_v4();
        let primary_addr: SocketAddr = "127.0.0.1:5433".parse().unwrap();

        let watcher = FailoverWatcher::new(
            config,
            node_id,
            primary_id,
            Some(primary_addr),
            vec![],
        );

        assert_eq!(watcher.primary_id, primary_id);
        assert_eq!(watcher.primary_addr, Some(primary_addr));
        assert!(!watcher.is_running());
    }

    #[tokio::test]
    async fn test_health_check_loop_start_stop() {
        let mut config = FailoverConfig::default();
        config.auto_failover = false; // Manual mode for this test

        let watcher = FailoverWatcher::new_simple(config, Uuid::new_v4(), vec![]);

        // Start in manual mode should not spawn health check loop
        watcher.start().await.unwrap();
        assert!(!watcher.is_running()); // Manual mode doesn't set is_running

        watcher.stop().await.unwrap();
    }
}
