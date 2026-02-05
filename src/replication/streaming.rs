//! WAL Streaming Server & Client
//!
//! Integrates the transport layer with WAL replication.
//! Implements hybrid streaming (batch catch-up + real-time) and sync modes.
//!
//! # Architecture
//!
//! ```text
//! PRIMARY                                    STANDBY
//! ┌─────────────────────┐                   ┌─────────────────────┐
//! │ StreamingServer     │                   │ StreamingClient     │
//! │ ├─ ReplicationServer│◄──TCP Stream──────│ ├─ ReplicationConn  │
//! │ ├─ WalReplicator    │                   │ ├─ WalApplicator    │
//! │ └─ SyncModeHandler  │                   │ └─ AckSender        │
//! └─────────────────────┘                   └─────────────────────┘
//! ```
//!
//! # Sync Modes
//!
//! - **Async**: Primary doesn't wait for ACKs
//! - **SemiSync**: Primary waits for transport ACK (received, not applied)
//! - **Sync**: Primary waits for apply ACK (WAL has been applied)

use super::config::{FailoverConfig, WalStreamingConfig};
use super::ha_state::{ha_state, StandbyInfo, StandbyState as HAStandbyState, SyncMode as HASyncMode};
use super::transport::{
    AckPayload, AckType, Capabilities, HandshakeRequest, HandshakeResponse,
    HeartbeatPayload, HealthStatus, Message, MessageType, NodeRole, ReplicationConnection,
    SyncModeConfig, WalBatchPayload, WalEntryPayload, WalEntryType as TransportWalEntryType,
    WalRequestPayload, HEARTBEAT_INTERVAL,
};
use super::wal_replicator::{Lsn, WalEntry, WalEntryType};
use super::wal_store::{BatchRequest, WalStore};
use super::{ReplicationError, Result};
use bytes::Bytes;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use uuid::Uuid;

// =============================================================================
// STREAMING SERVER (PRIMARY SIDE)
// =============================================================================

/// Streaming server configuration
#[derive(Debug, Clone)]
pub struct StreamingServerConfig {
    /// Listen address for replication connections
    pub listen_addr: SocketAddr,
    /// WAL streaming configuration
    pub wal_config: WalStreamingConfig,
    /// Default sync mode
    pub sync_mode: SyncModeConfig,
    /// Failover configuration
    pub failover_config: FailoverConfig,
    /// Maximum standbys
    pub max_standbys: usize,
    /// Heartbeat interval
    pub heartbeat_interval: Duration,
}

impl Default for StreamingServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:5433".parse().unwrap(),
            wal_config: WalStreamingConfig::default(),
            sync_mode: SyncModeConfig::Async,
            failover_config: FailoverConfig::default(),
            max_standbys: 10,
            heartbeat_interval: HEARTBEAT_INTERVAL,
        }
    }
}

/// Connected standby information
struct ConnectedStandby {
    /// Node ID
    node_id: Uuid,
    /// Remote address
    addr: SocketAddr,
    /// Negotiated sync mode
    sync_mode: SyncModeConfig,
    /// Last acknowledged LSN
    ack_lsn: Lsn,
    /// Last ACK type
    ack_type: AckType,
    /// Last heartbeat time
    last_heartbeat: Instant,
    /// Health status
    health: HealthStatus,
    /// Message sender
    msg_tx: mpsc::Sender<Message>,
    /// Pending ACK waiters (sequence -> sender)
    pending_acks: HashMap<u64, oneshot::Sender<AckPayload>>,
}

/// Streaming server for primary node
pub struct StreamingServer {
    /// Server configuration
    config: StreamingServerConfig,
    /// This node's ID
    node_id: Uuid,
    /// Current write LSN
    current_lsn: Arc<AtomicU64>,
    /// Fencing token (incremented on each primary election)
    fencing_token: Arc<AtomicU64>,
    /// Current term/epoch
    term: Arc<AtomicU64>,
    /// Is this node the primary
    is_primary: Arc<AtomicBool>,
    /// Connected standbys
    standbys: Arc<RwLock<HashMap<Uuid, ConnectedStandby>>>,
    /// WAL broadcast channel
    wal_broadcast: broadcast::Sender<WalEntry>,
    /// Shutdown signal
    shutdown_tx: broadcast::Sender<()>,
    /// WAL store for batch catch-up
    wal_store: Arc<WalStore>,
}

impl StreamingServer {
    /// Create a new streaming server
    pub fn new(config: StreamingServerConfig, node_id: Uuid, wal_store: Arc<WalStore>) -> Self {
        let (wal_broadcast, _) = broadcast::channel(config.wal_config.batch_size);
        let (shutdown_tx, _) = broadcast::channel(1);

        // Register broadcast sender with global HA state for storage engine access
        ha_state().set_wal_broadcast(wal_broadcast.clone());

        Self {
            config,
            node_id,
            current_lsn: Arc::new(AtomicU64::new(0)),
            fencing_token: Arc::new(AtomicU64::new(1)),
            term: Arc::new(AtomicU64::new(1)),
            is_primary: Arc::new(AtomicBool::new(true)),
            standbys: Arc::new(RwLock::new(HashMap::new())),
            wal_broadcast,
            shutdown_tx,
            wal_store,
        }
    }

    /// Start the streaming server
    pub async fn start(&self) -> Result<()> {
        let listener = TcpListener::bind(&self.config.listen_addr)
            .await
            .map_err(|e| ReplicationError::Network(format!("Bind failed: {}", e)))?;

        tracing::info!(
            "Streaming server listening on {} (node: {})",
            self.config.listen_addr,
            self.node_id
        );

        let mut shutdown_rx = self.shutdown_tx.subscribe();

        // Spawn heartbeat task
        let heartbeat_handle = self.spawn_heartbeat_task();

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, addr)) => {
                            if !self.is_primary.load(Ordering::SeqCst) {
                                tracing::warn!("Rejecting connection from {} - not primary", addr);
                                continue;
                            }

                            let standbys_count = self.standbys.read().await.len();
                            if standbys_count >= self.config.max_standbys {
                                tracing::warn!("Rejecting connection from {} - max standbys reached", addr);
                                continue;
                            }

                            let conn = ReplicationConnection::from_stream(stream, addr);
                            self.handle_new_connection(conn).await;
                        }
                        Err(e) => {
                            tracing::error!("Accept error: {}", e);
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::info!("Streaming server shutting down");
                    break;
                }
            }
        }

        heartbeat_handle.abort();
        Ok(())
    }

    /// Handle a new standby connection
    async fn handle_new_connection(&self, conn: ReplicationConnection) {
        let node_id = self.node_id;
        let fencing_token = self.fencing_token.load(Ordering::SeqCst);
        let current_lsn = self.current_lsn.load(Ordering::SeqCst);
        let standbys = self.standbys.clone();
        let wal_rx = self.wal_broadcast.subscribe();
        let shutdown_rx = self.shutdown_tx.subscribe();
        let wal_store = self.wal_store.clone();

        tokio::spawn(async move {
            if let Err(e) = Self::connection_loop(
                conn,
                node_id,
                fencing_token,
                current_lsn,
                standbys,
                wal_rx,
                shutdown_rx,
                wal_store,
            )
            .await
            {
                tracing::error!("Connection error: {}", e);
            }
        });
    }

    /// Main connection handling loop
    async fn connection_loop(
        mut conn: ReplicationConnection,
        server_node_id: Uuid,
        fencing_token: u64,
        current_lsn: Lsn,
        standbys: Arc<RwLock<HashMap<Uuid, ConnectedStandby>>>,
        mut wal_rx: broadcast::Receiver<WalEntry>,
        mut shutdown_rx: broadcast::Receiver<()>,
        wal_store: Arc<WalStore>,
    ) -> Result<()> {
        let addr = conn.remote_addr();
        tracing::info!("New connection from {}", addr);

        // Wait for handshake
        let msg = conn.recv().await?;
        if msg.header.msg_type != MessageType::HandshakeRequest {
            return Err(ReplicationError::Transport("Expected HandshakeRequest".to_string()));
        }

        let request: HandshakeRequest = bincode::deserialize(&msg.payload)
            .map_err(|e| ReplicationError::Transport(format!("Deserialize failed: {}", e)))?;

        tracing::info!(
            "Handshake from {:?} node {} at {}",
            request.role,
            request.node_id,
            addr
        );

        // Build response - respect the standby's requested sync mode
        // Standby announces its desired mode; primary honors it
        let negotiated_sync = request.sync_mode;

        let response = HandshakeResponse {
            accepted: true,
            server_node_id,
            sync_mode: negotiated_sync,
            primary_lsn: current_lsn,
            slot_name: request.slot_name.clone(),
            fencing_token,
            capabilities: Capabilities::all(),
            error: None,
        };

        let response_payload = bincode::serialize(&response)
            .map_err(|e| ReplicationError::Transport(format!("Serialize failed: {}", e)))?;

        conn.send(MessageType::HandshakeResponse, Bytes::from(response_payload))
            .await?;

        // Create message channel for this standby
        let (msg_tx, mut msg_rx) = mpsc::channel::<Message>(1000);

        // Register standby
        let standby = ConnectedStandby {
            node_id: request.node_id,
            addr,
            sync_mode: negotiated_sync,
            ack_lsn: request.current_lsn.unwrap_or(0),
            ack_type: AckType::Received,
            last_heartbeat: Instant::now(),
            health: HealthStatus::Healthy,
            msg_tx: msg_tx.clone(),
            pending_acks: HashMap::new(),
        };

        standbys.write().await.insert(request.node_id, standby);
        tracing::info!("Standby {} registered", request.node_id);

        // Register with global HA state for system views
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let ha_sync_mode = match negotiated_sync {
            SyncModeConfig::Async => HASyncMode::Async,
            SyncModeConfig::SemiSync { .. } => HASyncMode::SemiSync,
            SyncModeConfig::Sync { .. } => HASyncMode::Sync,
        };

        ha_state().register_standby(StandbyInfo {
            node_id: request.node_id,
            address: addr.to_string(),
            connected_at: now,
            last_heartbeat: now,
            sync_mode: ha_sync_mode,
            current_lsn: request.current_lsn.unwrap_or(0),
            flush_lsn: request.current_lsn.unwrap_or(0),
            apply_lsn: request.current_lsn.unwrap_or(0),
            lag_bytes: current_lsn.saturating_sub(request.current_lsn.unwrap_or(0)),
            lag_ms: 0,
            state: HAStandbyState::Connecting,
        });

        // Check if standby needs catch-up
        let standby_lsn = request.current_lsn.unwrap_or(0);
        if standby_lsn < current_lsn {
            tracing::info!(
                "Standby {} needs catch-up: {} -> {}",
                request.node_id,
                standby_lsn,
                current_lsn
            );

            // Send WAL batches for catch-up
            Self::send_catchup_batches(&mut conn, &wal_store, standby_lsn, current_lsn).await?;
        }

        // Main streaming loop
        let standby_node_id = request.node_id;
        loop {
            tokio::select! {
                // Receive WAL entry from broadcast
                wal_result = wal_rx.recv() => {
                    match wal_result {
                        Ok(entry) => {
                            tracing::info!("StreamingServer: Forwarding WAL entry LSN={} to standby", entry.lsn);
                            // Convert to transport format
                            let payload = Self::wal_entry_to_payload(&entry);
                            let payload_bytes = bincode::serialize(&payload)
                                .map_err(|e| ReplicationError::Transport(format!("Serialize failed: {}", e)))?;

                            conn.send(MessageType::WalEntry, Bytes::from(payload_bytes)).await?;
                            tracing::info!("StreamingServer: Sent WAL entry LSN={} to standby", entry.lsn);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Standby {} lagged {} entries", standby_node_id, n);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }

                // Receive message from standby
                msg_result = conn.recv() => {
                    match msg_result {
                        Ok(msg) => {
                            Self::handle_standby_message(
                                &standbys,
                                standby_node_id,
                                msg,
                            ).await?;
                        }
                        Err(e) => {
                            tracing::warn!("Connection error from {}: {}", standby_node_id, e);
                            break;
                        }
                    }
                }

                // Send queued messages
                Some(msg) = msg_rx.recv() => {
                    conn.send_message(&msg).await?;
                }

                // Shutdown signal
                _ = shutdown_rx.recv() => {
                    break;
                }
            }
        }

        // Cleanup
        standbys.write().await.remove(&standby_node_id);
        ha_state().remove_standby(standby_node_id);
        tracing::info!("Standby {} disconnected", standby_node_id);

        Ok(())
    }

    /// Handle message from standby
    async fn handle_standby_message(
        standbys: &Arc<RwLock<HashMap<Uuid, ConnectedStandby>>>,
        standby_id: Uuid,
        msg: Message,
    ) -> Result<()> {
        tracing::debug!("StreamingServer: Received message from standby {}: type={:?}", standby_id, msg.header.msg_type);
        match msg.header.msg_type {
            MessageType::Ack => {
                let ack: AckPayload = bincode::deserialize(&msg.payload)
                    .map_err(|e| ReplicationError::Transport(format!("Deserialize failed: {}", e)))?;
                tracing::info!("StreamingServer: Received ACK from standby {} for LSN={} type={:?}", standby_id, ack.lsn, ack.ack_type);

                let mut standbys = standbys.write().await;
                if let Some(standby) = standbys.get_mut(&standby_id) {
                    standby.ack_lsn = ack.lsn;
                    standby.ack_type = ack.ack_type;
                    standby.last_heartbeat = Instant::now();

                    // Wake up pending ACK waiter if any
                    if let Some(sender) = standby.pending_acks.remove(&ack.sequence) {
                        let _ = sender.send(ack.clone());
                    }
                }
                drop(standbys); // Release lock before updating global state

                // Update global HA state
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;

                ha_state().update_standby(standby_id, |info| {
                    info.current_lsn = ack.lsn;
                    info.flush_lsn = ack.lsn;
                    if matches!(ack.ack_type, AckType::Applied | AckType::Checkpointed) {
                        info.apply_lsn = ack.lsn;
                    }
                    info.last_heartbeat = now;
                    info.state = HAStandbyState::Streaming;
                });
            }
            MessageType::Heartbeat => {
                let heartbeat: HeartbeatPayload = bincode::deserialize(&msg.payload)
                    .map_err(|e| ReplicationError::Transport(format!("Deserialize failed: {}", e)))?;

                let mut standbys = standbys.write().await;
                if let Some(standby) = standbys.get_mut(&standby_id) {
                    standby.ack_lsn = heartbeat.apply_lsn.unwrap_or(heartbeat.flush_lsn);
                    standby.last_heartbeat = Instant::now();
                    standby.health = heartbeat.health;
                }
                drop(standbys); // Release lock before updating global state

                // Update global HA state
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;

                ha_state().update_standby(standby_id, |info| {
                    info.current_lsn = heartbeat.current_lsn;
                    info.flush_lsn = heartbeat.flush_lsn;
                    info.apply_lsn = heartbeat.apply_lsn.unwrap_or(heartbeat.flush_lsn);
                    info.lag_bytes = heartbeat.lag_bytes;
                    info.last_heartbeat = now;
                    info.state = HAStandbyState::Streaming;
                });
            }
            MessageType::WalRequest => {
                let request: WalRequestPayload = bincode::deserialize(&msg.payload)
                    .map_err(|e| ReplicationError::Transport(format!("Deserialize failed: {}", e)))?;

                tracing::info!(
                    "WAL request from {}: {} -> {:?}",
                    standby_id,
                    request.from_lsn,
                    request.to_lsn
                );

                // Queue a catch-up response via standby's message channel
                // Note: Actual batch sending is handled elsewhere since we don't have
                // direct access to wal_store here. The standby should use the
                // dedicated catch-up protocol on connection.
            }
            _ => {
                tracing::warn!("Unexpected message type from standby: {:?}", msg.header.msg_type);
            }
        }

        Ok(())
    }

    /// Send catch-up batches to standby
    async fn send_catchup_batches(
        conn: &mut ReplicationConnection,
        wal_store: &WalStore,
        from_lsn: Lsn,
        to_lsn: Lsn,
    ) -> Result<()> {
        const BATCH_SIZE: usize = 1000;
        const MAX_BATCH_BYTES: usize = 10 * 1024 * 1024; // 10 MB

        let mut current_from = from_lsn;
        let mut batch_num = 0u32;

        loop {
            let request = BatchRequest {
                from_lsn: current_from,
                to_lsn: Some(to_lsn),
                max_entries: BATCH_SIZE,
                max_bytes: MAX_BATCH_BYTES,
            };

            let batch = wal_store.get_batch(request).await?;

            if batch.entries.is_empty() {
                break;
            }

            batch_num += 1;
            let is_final = !batch.has_more;

            tracing::debug!(
                "Sending catch-up batch {}: {} entries ({} -> {}), final={}",
                batch_num,
                batch.entries.len(),
                batch.start_lsn,
                batch.end_lsn,
                is_final
            );

            // Convert entries to transport format
            let entry_payloads: Vec<WalEntryPayload> = batch
                .entries
                .iter()
                .map(Self::wal_entry_to_payload)
                .collect();

            let batch_payload = WalBatchPayload {
                start_lsn: batch.start_lsn,
                end_lsn: batch.end_lsn,
                entry_count: batch.entries.len() as u32,
                entries: entry_payloads,
                is_final,
            };

            let payload_bytes = bincode::serialize(&batch_payload)
                .map_err(|e| ReplicationError::Transport(format!("Serialize failed: {}", e)))?;

            conn.send(MessageType::WalBatch, Bytes::from(payload_bytes)).await?;

            if is_final {
                break;
            }

            current_from = batch.end_lsn;
        }

        tracing::info!(
            "Catch-up complete: sent {} batches, {} -> {}",
            batch_num,
            from_lsn,
            to_lsn
        );

        Ok(())
    }

    /// Convert WAL entry to transport payload
    fn wal_entry_to_payload(entry: &WalEntry) -> WalEntryPayload {
        let entry_type = match entry.entry_type {
            WalEntryType::Insert => TransportWalEntryType::Insert,
            WalEntryType::Update => TransportWalEntryType::Update,
            WalEntryType::Delete => TransportWalEntryType::Delete,
            WalEntryType::TxBegin => TransportWalEntryType::TxBegin,
            WalEntryType::TxCommit => TransportWalEntryType::TxCommit,
            WalEntryType::TxRollback => TransportWalEntryType::TxAbort,
            WalEntryType::Checkpoint => TransportWalEntryType::Checkpoint,
            WalEntryType::SchemaChange => TransportWalEntryType::SchemaChange,
            WalEntryType::BranchOp => TransportWalEntryType::BranchOp,
        };

        WalEntryPayload {
            lsn: entry.lsn,
            tx_id: entry.tx_id, // Pass through transaction ID
            entry_type,
            data: entry.data.clone(),
            timestamp_us: chrono::Utc::now().timestamp_micros() as u64,
            checksum: entry.checksum,
        }
    }

    /// Spawn heartbeat monitoring task
    fn spawn_heartbeat_task(&self) -> tokio::task::JoinHandle<()> {
        let standbys = self.standbys.clone();
        let interval = self.config.heartbeat_interval;
        let node_id = self.node_id;
        let current_lsn = self.current_lsn.clone();

        tokio::spawn(async move {
            let mut interval_timer = tokio::time::interval(interval);
            loop {
                interval_timer.tick().await;

                let heartbeat = HeartbeatPayload {
                    node_id,
                    role: NodeRole::Primary,
                    current_lsn: current_lsn.load(Ordering::SeqCst),
                    flush_lsn: current_lsn.load(Ordering::SeqCst),
                    apply_lsn: None,
                    timestamp_ms: chrono::Utc::now().timestamp_millis() as u64,
                    lag_bytes: 0,
                    health: HealthStatus::Healthy,
                };

                let payload = match bincode::serialize(&heartbeat) {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                let standbys = standbys.read().await;
                for (_, standby) in standbys.iter() {
                    let msg = Message::new(
                        MessageType::Heartbeat,
                        Bytes::from(payload.clone()),
                        0,
                    );
                    let _ = standby.msg_tx.send(msg).await;
                }
            }
        })
    }

    /// Broadcast a WAL entry to all standbys
    pub fn broadcast(&self, entry: WalEntry) -> Result<()> {
        self.current_lsn.store(entry.lsn, Ordering::SeqCst);
        self.wal_broadcast
            .send(entry)
            .map_err(|e| ReplicationError::WalStreaming(e.to_string()))?;
        Ok(())
    }

    /// Wait for ACK from standbys based on sync mode
    pub async fn wait_for_ack(&self, lsn: Lsn) -> Result<()> {
        match self.config.sync_mode {
            SyncModeConfig::Async => {
                // No waiting in async mode
                Ok(())
            }
            SyncModeConfig::SemiSync { min_acks, timeout_ms } => {
                self.wait_for_acks(lsn, min_acks as usize, AckType::Received, timeout_ms).await
            }
            SyncModeConfig::Sync { min_applied, timeout_ms } => {
                self.wait_for_acks(lsn, min_applied as usize, AckType::Applied, timeout_ms).await
            }
        }
    }

    /// Wait for specific number of ACKs of a given type
    async fn wait_for_acks(
        &self,
        lsn: Lsn,
        min_acks: usize,
        ack_type: AckType,
        timeout_ms: u32,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);

        loop {
            if Instant::now() >= deadline {
                return Err(ReplicationError::Timeout(format!(
                    "Timeout waiting for {} {:?} ACKs for LSN {}",
                    min_acks, ack_type, lsn
                )));
            }

            let standbys = self.standbys.read().await;
            let ack_count = standbys
                .values()
                .filter(|s| s.ack_lsn >= lsn && Self::ack_type_satisfies(&s.ack_type, &ack_type))
                .count();

            if ack_count >= min_acks {
                return Ok(());
            }

            drop(standbys);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Check if an ACK type satisfies the required type
    fn ack_type_satisfies(actual: &AckType, required: &AckType) -> bool {
        match (actual, required) {
            (AckType::Applied, _) | (AckType::Checkpointed, _) => true,
            (AckType::Written, AckType::Written | AckType::Received) => true,
            (AckType::Received, AckType::Received) => true,
            _ => false,
        }
    }

    /// Shutdown the server
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    /// Get connected standby count
    pub async fn standby_count(&self) -> usize {
        self.standbys.read().await.len()
    }

    /// Get standby states
    pub async fn standby_states(&self) -> Vec<(Uuid, Lsn, HealthStatus)> {
        self.standbys
            .read()
            .await
            .iter()
            .map(|(id, s)| (*id, s.ack_lsn, s.health))
            .collect()
    }
}

// =============================================================================
// STREAMING CLIENT (STANDBY SIDE)
// =============================================================================

/// Streaming client configuration
#[derive(Debug, Clone)]
pub struct StreamingClientConfig {
    /// This node's ID
    pub node_id: Uuid,
    /// Primary host:port
    pub primary_addr: SocketAddr,
    /// Sync mode to request
    pub sync_mode: SyncModeConfig,
    /// Connection timeout
    pub connect_timeout: Duration,
    /// Reconnect delay
    pub reconnect_delay: Duration,
    /// Max reconnect attempts
    pub max_reconnect_attempts: u32,
}

/// Streaming client state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingClientState {
    Disconnected,
    Connecting,
    Handshaking,
    CatchingUp,
    Streaming,
    Reconnecting,
    Error,
}

/// Streaming client for standby nodes
pub struct StreamingClient {
    /// Configuration
    config: StreamingClientConfig,
    /// Client state
    state: Arc<RwLock<StreamingClientState>>,
    /// Applied LSN (WAL entries that have been fully applied)
    applied_lsn: Arc<AtomicU64>,
    /// Flush LSN (WAL entries that have been flushed to disk but may not be applied)
    flush_lsn: Arc<AtomicU64>,
    /// Primary's LSN (from heartbeats)
    primary_lsn: Arc<AtomicU64>,
    /// Fencing token from primary
    fencing_token: Arc<AtomicU64>,
    /// Entry receiver (for external consumption)
    entry_tx: mpsc::Sender<WalEntry>,
    /// Shutdown signal
    shutdown_tx: broadcast::Sender<()>,
}

impl StreamingClient {
    /// Create a new streaming client
    pub fn new(config: StreamingClientConfig) -> (Self, mpsc::Receiver<WalEntry>) {
        let (entry_tx, entry_rx) = mpsc::channel(10000);
        let (shutdown_tx, _) = broadcast::channel(1);

        let client = Self {
            config,
            state: Arc::new(RwLock::new(StreamingClientState::Disconnected)),
            applied_lsn: Arc::new(AtomicU64::new(0)),
            flush_lsn: Arc::new(AtomicU64::new(0)),
            primary_lsn: Arc::new(AtomicU64::new(0)),
            fencing_token: Arc::new(AtomicU64::new(0)),
            entry_tx,
            shutdown_tx,
        };

        (client, entry_rx)
    }

    /// Start the streaming client
    ///
    /// Automatically reconnects to the primary with exponential backoff.
    /// Set `max_reconnect_attempts` to 0 for unlimited reconnection attempts.
    pub async fn start(&self) -> Result<()> {
        let mut reconnect_attempts: u32 = 0;
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let base_delay = self.config.reconnect_delay;
        let max_delay = Duration::from_secs(60); // Cap at 60 seconds

        loop {
            *self.state.write().await = StreamingClientState::Connecting;

            // Track state before attempting connection
            let was_streaming_before = *self.state.read().await == StreamingClientState::Streaming;

            match self.connect_and_stream().await {
                Ok(()) => {
                    // Clean shutdown
                    break;
                }
                Err(e) => {
                    // Check if we successfully entered streaming state before the error
                    // If we did, reset the counter since the reconnection worked
                    let current_state = *self.state.read().await;
                    let was_streaming = current_state == StreamingClientState::Streaming
                        || current_state == StreamingClientState::CatchingUp;

                    if was_streaming || was_streaming_before {
                        tracing::info!(
                            "Connection lost after successful streaming - resetting reconnect counter"
                        );
                        reconnect_attempts = 0;
                    }

                    reconnect_attempts += 1;

                    // Check if we've exceeded max attempts (0 = unlimited)
                    let unlimited = self.config.max_reconnect_attempts == 0;
                    if !unlimited && reconnect_attempts >= self.config.max_reconnect_attempts {
                        tracing::error!(
                            "Streaming error (attempt {}/{}): {} - giving up",
                            reconnect_attempts,
                            self.config.max_reconnect_attempts,
                            e
                        );
                        *self.state.write().await = StreamingClientState::Error;
                        return Err(e);
                    }

                    // Calculate exponential backoff delay
                    // delay = base_delay * 2^(attempts-1), capped at max_delay
                    let backoff_multiplier = 2u32.saturating_pow(reconnect_attempts.saturating_sub(1).min(6));
                    let delay = std::cmp::min(
                        base_delay.saturating_mul(backoff_multiplier),
                        max_delay,
                    );

                    if unlimited {
                        tracing::warn!(
                            "Streaming error (attempt {}): {} - reconnecting in {:?}",
                            reconnect_attempts,
                            e,
                            delay
                        );
                    } else {
                        tracing::warn!(
                            "Streaming error (attempt {}/{}): {} - reconnecting in {:?}",
                            reconnect_attempts,
                            self.config.max_reconnect_attempts,
                            e,
                            delay
                        );
                    }

                    *self.state.write().await = StreamingClientState::Reconnecting;

                    // Wait before reconnecting with exponential backoff
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = shutdown_rx.recv() => {
                            tracing::info!("Streaming client shutdown requested during reconnect");
                            break;
                        }
                    }
                }
            }
        }

        *self.state.write().await = StreamingClientState::Disconnected;
        Ok(())
    }

    /// Connect to primary and start streaming
    async fn connect_and_stream(&self) -> Result<()> {
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        // Connect to primary
        let mut conn = ReplicationConnection::connect(
            self.config.primary_addr,
            self.config.connect_timeout,
        )
        .await?;

        // Send handshake
        *self.state.write().await = StreamingClientState::Handshaking;

        let current_lsn = self.applied_lsn.load(Ordering::SeqCst);
        let request = HandshakeRequest {
            node_id: self.config.node_id,
            role: NodeRole::Standby,
            sync_mode: self.config.sync_mode,
            current_lsn: Some(current_lsn),
            slot_name: None, // TODO: Support replication slots
            capabilities: Capabilities::all(),
        };

        let response = conn.handshake_client(request).await?;

        if !response.accepted {
            return Err(ReplicationError::Transport(format!(
                "Handshake rejected: {}",
                response.error.unwrap_or_default()
            )));
        }

        self.fencing_token.store(response.fencing_token, Ordering::SeqCst);
        self.primary_lsn.store(response.primary_lsn, Ordering::SeqCst);

        tracing::info!(
            "Connected to primary (node: {}, LSN: {}, fencing: {})",
            response.server_node_id,
            response.primary_lsn,
            response.fencing_token
        );

        // Check if we need catch-up
        if current_lsn < response.primary_lsn {
            *self.state.write().await = StreamingClientState::CatchingUp;
            tracing::info!("Starting catch-up: {} -> {}", current_lsn, response.primary_lsn);

            // Request WAL batch for catch-up
            let wal_request = WalRequestPayload {
                from_lsn: current_lsn,
                to_lsn: Some(response.primary_lsn),
                max_entries: 1000,
                max_bytes: 10 * 1024 * 1024, // 10 MB
            };

            let payload = bincode::serialize(&wal_request)
                .map_err(|e| ReplicationError::Transport(format!("Serialize failed: {}", e)))?;

            conn.send(MessageType::WalRequest, Bytes::from(payload)).await?;

            // Note: Catch-up batches will be received in the main loop as WalBatch messages
            // The server sends them proactively after handshake, so this request is optional
        }

        *self.state.write().await = StreamingClientState::Streaming;

        // Main streaming loop
        let mut heartbeat_interval = tokio::time::interval(HEARTBEAT_INTERVAL);

        loop {
            tokio::select! {
                // Receive messages from primary
                msg_result = conn.recv() => {
                    match msg_result {
                        Ok(msg) => {
                            self.handle_primary_message(&mut conn, msg).await?;
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                }

                // Send heartbeat
                _ = heartbeat_interval.tick() => {
                    self.send_heartbeat(&mut conn).await?;
                }

                // Shutdown signal
                _ = shutdown_rx.recv() => {
                    conn.close().await?;
                    break;
                }
            }
        }

        Ok(())
    }

    /// Handle message from primary
    async fn handle_primary_message(
        &self,
        conn: &mut ReplicationConnection,
        msg: Message,
    ) -> Result<()> {
        match msg.header.msg_type {
            MessageType::WalEntry => {
                tracing::info!("StreamingClient: Received WalEntry message, payload_len={}", msg.payload.len());
                let payload: WalEntryPayload = bincode::deserialize(&msg.payload)
                    .map_err(|e| ReplicationError::Transport(format!("Deserialize failed: {}", e)))?;

                let entry = Self::payload_to_wal_entry(&payload);
                let lsn = entry.lsn;
                tracing::info!("StreamingClient: Processing WAL entry LSN={}", lsn);

                // Send to applicator
                self.entry_tx
                    .send(entry)
                    .await
                    .map_err(|e| ReplicationError::WalStreaming(e.to_string()))?;
                tracing::info!("StreamingClient: Sent WAL entry LSN={} to applicator", lsn);

                // Send ACK
                self.send_ack(conn, lsn, AckType::Received, msg.header.sequence).await?;
            }
            MessageType::WalBatch => {
                let payload: WalBatchPayload = bincode::deserialize(&msg.payload)
                    .map_err(|e| ReplicationError::Transport(format!("Deserialize failed: {}", e)))?;

                tracing::info!(
                    "Received WAL batch: {} entries, {} -> {}",
                    payload.entry_count,
                    payload.start_lsn,
                    payload.end_lsn
                );

                for entry_payload in &payload.entries {
                    let entry = Self::payload_to_wal_entry(entry_payload);
                    self.entry_tx
                        .send(entry)
                        .await
                        .map_err(|e| ReplicationError::WalStreaming(e.to_string()))?;
                }

                // ACK the batch
                self.send_ack(conn, payload.end_lsn, AckType::Received, msg.header.sequence).await?;

                if payload.is_final {
                    *self.state.write().await = StreamingClientState::Streaming;
                }
            }
            MessageType::Heartbeat => {
                let payload: HeartbeatPayload = bincode::deserialize(&msg.payload)
                    .map_err(|e| ReplicationError::Transport(format!("Deserialize failed: {}", e)))?;

                self.primary_lsn.store(payload.current_lsn, Ordering::SeqCst);
            }
            _ => {
                tracing::warn!("Unexpected message type from primary: {:?}", msg.header.msg_type);
            }
        }

        Ok(())
    }

    /// Convert transport payload to WAL entry
    fn payload_to_wal_entry(payload: &WalEntryPayload) -> WalEntry {
        let entry_type = match payload.entry_type {
            TransportWalEntryType::Insert => WalEntryType::Insert,
            TransportWalEntryType::Update => WalEntryType::Update,
            TransportWalEntryType::Delete => WalEntryType::Delete,
            TransportWalEntryType::TxBegin => WalEntryType::TxBegin,
            TransportWalEntryType::TxCommit => WalEntryType::TxCommit,
            TransportWalEntryType::TxAbort => WalEntryType::TxRollback,
            TransportWalEntryType::Checkpoint => WalEntryType::Checkpoint,
            TransportWalEntryType::SchemaChange => WalEntryType::SchemaChange,
            TransportWalEntryType::BranchOp => WalEntryType::BranchOp,
        };

        WalEntry {
            lsn: payload.lsn,
            tx_id: payload.tx_id, // Extract transaction ID from payload
            entry_type,
            data: payload.data.clone(),
            checksum: payload.checksum,
        }
    }

    /// Send ACK to primary
    async fn send_ack(
        &self,
        conn: &mut ReplicationConnection,
        lsn: Lsn,
        ack_type: AckType,
        sequence: u64,
    ) -> Result<()> {
        let ack = AckPayload {
            lsn,
            ack_type,
            node_id: self.config.node_id,
            timestamp_ms: chrono::Utc::now().timestamp_millis() as u64,
            sequence,
        };

        eprintln!("DEBUG: send_ack called for LSN={} type={:?}", lsn, ack_type);
        tracing::info!("StreamingClient: Sending ACK for LSN={} type={:?} seq={}", lsn, ack_type, sequence);
        let result = conn.send_ack(ack).await;
        eprintln!("DEBUG: send_ack result for LSN={}: {:?}", lsn, result.is_ok());
        match &result {
            Ok(_) => tracing::info!("StreamingClient: ACK sent successfully for LSN={}", lsn),
            Err(e) => tracing::error!("StreamingClient: Failed to send ACK for LSN={}: {}", lsn, e),
        }
        result
    }

    /// Send heartbeat to primary
    async fn send_heartbeat(&self, conn: &mut ReplicationConnection) -> Result<()> {
        let applied = self.applied_lsn.load(Ordering::SeqCst);
        let flushed = self.flush_lsn.load(Ordering::SeqCst);
        let primary = self.primary_lsn.load(Ordering::SeqCst);

        let heartbeat = HeartbeatPayload {
            node_id: self.config.node_id,
            role: NodeRole::Standby,
            current_lsn: flushed.max(applied), // Current position is max of flush and apply
            flush_lsn: flushed,                 // Separately tracked flush position
            apply_lsn: Some(applied),
            timestamp_ms: chrono::Utc::now().timestamp_millis() as u64,
            lag_bytes: primary.saturating_sub(flushed),
            health: HealthStatus::Healthy,
        };

        let payload = bincode::serialize(&heartbeat)
            .map_err(|e| ReplicationError::Transport(format!("Serialize failed: {}", e)))?;

        conn.send(MessageType::Heartbeat, Bytes::from(payload)).await?;
        Ok(())
    }

    /// Report that an entry has been applied
    pub fn report_applied(&self, lsn: Lsn) {
        self.applied_lsn.fetch_max(lsn, Ordering::SeqCst);
    }

    /// Report that an entry has been flushed to disk (but not yet applied)
    pub fn report_flushed(&self, lsn: Lsn) {
        self.flush_lsn.fetch_max(lsn, Ordering::SeqCst);
    }

    /// Get flush LSN
    pub fn flush_lsn(&self) -> Lsn {
        self.flush_lsn.load(Ordering::SeqCst)
    }

    /// Get current state
    pub async fn state(&self) -> StreamingClientState {
        *self.state.read().await
    }

    /// Get applied LSN
    pub fn applied_lsn(&self) -> Lsn {
        self.applied_lsn.load(Ordering::SeqCst)
    }

    /// Get replication lag
    pub fn lag_bytes(&self) -> u64 {
        let primary = self.primary_lsn.load(Ordering::SeqCst);
        let applied = self.applied_lsn.load(Ordering::SeqCst);
        primary.saturating_sub(applied)
    }

    /// Shutdown the client
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_streaming_server_config_default() {
        let config = StreamingServerConfig::default();
        assert_eq!(config.max_standbys, 10);
        matches!(config.sync_mode, SyncModeConfig::Async);
    }

    #[test]
    fn test_wal_entry_to_payload_conversion() {
        let entry = WalEntry {
            lsn: 100,
            tx_id: Some(42),
            entry_type: WalEntryType::Insert,
            data: vec![1, 2, 3],
            checksum: 0xDEADBEEF,
        };

        let payload = StreamingServer::wal_entry_to_payload(&entry);
        assert_eq!(payload.lsn, 100);
        assert_eq!(payload.checksum, 0xDEADBEEF);
        matches!(payload.entry_type, TransportWalEntryType::Insert);
    }

    #[test]
    fn test_payload_to_wal_entry_conversion() {
        let payload = WalEntryPayload {
            lsn: 200,
            tx_id: Some(42),
            entry_type: TransportWalEntryType::Update,
            data: vec![4, 5, 6],
            timestamp_us: 12345678,
            checksum: 0xBEEFCAFE,
        };

        let entry = StreamingClient::payload_to_wal_entry(&payload);
        assert_eq!(entry.lsn, 200);
        assert_eq!(entry.checksum, 0xBEEFCAFE);
        matches!(entry.entry_type, WalEntryType::Update);
    }

    #[test]
    fn test_ack_type_satisfies() {
        // Applied satisfies all
        assert!(StreamingServer::ack_type_satisfies(&AckType::Applied, &AckType::Received));
        assert!(StreamingServer::ack_type_satisfies(&AckType::Applied, &AckType::Written));
        assert!(StreamingServer::ack_type_satisfies(&AckType::Applied, &AckType::Applied));

        // Written satisfies received and written
        assert!(StreamingServer::ack_type_satisfies(&AckType::Written, &AckType::Received));
        assert!(StreamingServer::ack_type_satisfies(&AckType::Written, &AckType::Written));
        assert!(!StreamingServer::ack_type_satisfies(&AckType::Written, &AckType::Applied));

        // Received only satisfies received
        assert!(StreamingServer::ack_type_satisfies(&AckType::Received, &AckType::Received));
        assert!(!StreamingServer::ack_type_satisfies(&AckType::Received, &AckType::Written));
    }

    #[test]
    fn test_streaming_client_creation() {
        let config = StreamingClientConfig {
            node_id: Uuid::new_v4(),
            primary_addr: "127.0.0.1:5433".parse().unwrap(),
            sync_mode: SyncModeConfig::SemiSync {
                min_acks: 1,
                timeout_ms: 5000,
            },
            connect_timeout: Duration::from_secs(10),
            reconnect_delay: Duration::from_secs(5),
            max_reconnect_attempts: 3,
        };

        let (client, _rx) = StreamingClient::new(config);
        assert_eq!(client.applied_lsn(), 0);
        assert_eq!(client.lag_bytes(), 0);
    }
}
