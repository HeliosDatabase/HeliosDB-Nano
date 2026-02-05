//! Split-Brain Protection Module
//!
//! Implements quorum-based primary election and fencing to prevent split-brain scenarios
//! in multi-node HeliosDB-Lite clusters.
//!
//! # Architecture
//!
//! ```text
//!                    ┌─────────────────────┐
//!                    │   OBSERVER NODE     │
//!                    │  - Votes on primary │
//!                    │  - No data writes   │
//!                    │  - Witness only     │
//!                    └──────────┬──────────┘
//!                               │
//!            Vote Request/Response
//!                               │
//!      ┌────────────────────────┼────────────────────────┐
//!      │                        │                        │
//!      ▼                        ▼                        ▼
//! ┌─────────┐              ┌─────────┐              ┌─────────┐
//! │ Primary │◄────────────►│ Standby │◄────────────►│ Standby │
//! │ (R/W)   │   Fencing    │ (R/O)   │   Fencing    │ (R/O)   │
//! └─────────┘    Token     └─────────┘    Token     └─────────┘
//! ```
//!
//! # Fencing Tokens
//!
//! Every write operation must include a valid fencing token. If a node receives
//! a write with a lower fencing token than its current known token, it rejects
//! the write. This prevents stale primaries from corrupting data after failover.
//!
//! # Quorum Rules
//!
//! - 3 nodes (1P + 2S): quorum = 2 (can lose 1 node)
//! - 5 nodes (1P + 4S or 1P + 2S + 2O): quorum = 3 (can lose 2 nodes)
//!
//! With observers (witness nodes that don't hold data):
//! - 2 nodes + 1 observer: quorum = 2 (can lose 1 node)
//! - 2 nodes + 2 observers: quorum = 2 (can lose 2 nodes)

use super::transport::{
    FencingTokenPayload, Message, MessageType, NodeRole, ReplicationConnection,
    VoteReason, VoteRequestPayload, VoteResponsePayload,
};
use super::{ReplicationError, Result};
use bytes::Bytes;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, RwLock};
use uuid::Uuid;

/// Observer/Witness node configuration
#[derive(Debug, Clone)]
pub struct ObserverConfig {
    /// Observer node addresses
    pub observers: Vec<SocketAddr>,
    /// Quorum size (including self)
    pub quorum_size: usize,
    /// Vote timeout
    pub vote_timeout: Duration,
    /// Heartbeat interval
    pub heartbeat_interval: Duration,
    /// Election timeout (how long before assuming primary failed)
    pub election_timeout: Duration,
}

impl Default for ObserverConfig {
    fn default() -> Self {
        Self {
            observers: Vec::new(),
            quorum_size: 2,
            vote_timeout: Duration::from_secs(5),
            heartbeat_interval: Duration::from_secs(1),
            election_timeout: Duration::from_secs(10),
        }
    }
}

/// Split-brain protection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectionState {
    /// Normal operation, primary is known
    Normal,
    /// Election in progress
    Election,
    /// Fenced - waiting for new primary
    Fenced,
    /// Split-brain detected - manual intervention required
    SplitBrain,
}

/// Vote state during election
#[derive(Debug, Clone)]
struct VoteState {
    /// Current term
    term: u64,
    /// Candidate we voted for
    voted_for: Option<Uuid>,
    /// Votes received (when candidate)
    votes_received: HashSet<Uuid>,
    /// Election start time
    election_start: Instant,
}

/// Known node in the cluster
#[derive(Debug, Clone)]
pub struct ClusterNode {
    /// Node ID
    pub node_id: Uuid,
    /// Node role
    pub role: NodeRole,
    /// Socket address
    pub addr: SocketAddr,
    /// Last known LSN
    pub last_lsn: u64,
    /// Last heartbeat time
    pub last_heartbeat: Instant,
    /// Is node healthy
    pub is_healthy: bool,
    /// Fencing token
    pub fencing_token: u64,
}

/// Split-brain protection coordinator
pub struct SplitBrainProtector {
    /// This node's ID
    node_id: Uuid,
    /// This node's role
    role: Arc<RwLock<NodeRole>>,
    /// Current protection state
    state: Arc<RwLock<ProtectionState>>,
    /// Configuration
    config: ObserverConfig,
    /// Current term/epoch
    term: Arc<AtomicU64>,
    /// Current fencing token
    fencing_token: Arc<AtomicU64>,
    /// Known primary node
    known_primary: Arc<RwLock<Option<Uuid>>>,
    /// Vote state
    vote_state: Arc<RwLock<VoteState>>,
    /// Cluster nodes
    cluster_nodes: Arc<RwLock<HashMap<Uuid, ClusterNode>>>,
    /// Observer connections
    observer_connections: Arc<RwLock<HashMap<SocketAddr, mpsc::Sender<Message>>>>,
    /// Is running
    is_running: Arc<AtomicBool>,
    /// Shutdown signal
    shutdown_tx: broadcast::Sender<()>,
    /// Event channel for state changes
    event_tx: mpsc::Sender<ProtectionEvent>,
    /// Current LSN (from WAL store)
    current_lsn: Arc<AtomicU64>,
}

/// Events from the protection system
#[derive(Debug, Clone)]
pub enum ProtectionEvent {
    /// Primary elected
    PrimaryElected { node_id: Uuid, term: u64, fencing_token: u64 },
    /// Primary lost
    PrimaryLost { previous_primary: Uuid, reason: String },
    /// Fencing token changed
    FencingTokenChanged { old_token: u64, new_token: u64 },
    /// Split-brain detected
    SplitBrainDetected { primaries: Vec<Uuid> },
    /// Election started
    ElectionStarted { term: u64, reason: VoteReason },
    /// Election completed
    ElectionCompleted { winner: Option<Uuid>, term: u64 },
    /// Election needed (signal to coordinator to start election)
    ElectionNeeded { reason: VoteReason },
}

impl SplitBrainProtector {
    /// Create a new split-brain protector
    pub fn new(
        node_id: Uuid,
        role: NodeRole,
        config: ObserverConfig,
    ) -> (Self, mpsc::Receiver<ProtectionEvent>) {
        let (shutdown_tx, _) = broadcast::channel(1);
        let (event_tx, event_rx) = mpsc::channel(100);

        let protector = Self {
            node_id,
            role: Arc::new(RwLock::new(role)),
            state: Arc::new(RwLock::new(ProtectionState::Normal)),
            config,
            term: Arc::new(AtomicU64::new(1)),
            fencing_token: Arc::new(AtomicU64::new(1)),
            known_primary: Arc::new(RwLock::new(None)),
            vote_state: Arc::new(RwLock::new(VoteState {
                term: 1,
                voted_for: None,
                votes_received: HashSet::new(),
                election_start: Instant::now(),
            })),
            cluster_nodes: Arc::new(RwLock::new(HashMap::new())),
            observer_connections: Arc::new(RwLock::new(HashMap::new())),
            is_running: Arc::new(AtomicBool::new(false)),
            shutdown_tx,
            event_tx,
            current_lsn: Arc::new(AtomicU64::new(0)),
        };

        (protector, event_rx)
    }

    /// Start the protection system
    pub async fn start(&self) -> Result<()> {
        self.is_running.store(true, Ordering::SeqCst);
        tracing::info!("Split-brain protection started for node {}", self.node_id);

        // Connect to observers
        for addr in &self.config.observers {
            if let Err(e) = self.connect_to_observer(*addr).await {
                tracing::warn!("Failed to connect to observer {}: {}", addr, e);
            }
        }

        // Start heartbeat and election monitoring
        let heartbeat_handle = self.spawn_heartbeat_task();
        let election_handle = self.spawn_election_monitor();

        let mut shutdown_rx = self.shutdown_tx.subscribe();
        shutdown_rx.recv().await.ok();

        heartbeat_handle.abort();
        election_handle.abort();

        Ok(())
    }

    /// Connect to an observer node
    async fn connect_to_observer(&self, addr: SocketAddr) -> Result<()> {
        let conn = ReplicationConnection::connect(addr, Duration::from_secs(5)).await?;
        let (msg_tx, mut msg_rx) = mpsc::channel::<Message>(100);

        self.observer_connections.write().await.insert(addr, msg_tx);
        tracing::info!("Connected to observer at {}", addr);

        // Start connection handler task
        let vote_state = self.vote_state.clone();
        let term = self.term.clone();
        let fencing_token = self.fencing_token.clone();
        let event_tx = self.event_tx.clone();
        let is_running = self.is_running.clone();

        tokio::spawn(async move {
            while is_running.load(Ordering::SeqCst) {
                // Receive responses from the observer
                match tokio::time::timeout(Duration::from_secs(30), msg_rx.recv()).await {
                    Ok(Some(msg)) => {
                        match msg.header.msg_type {
                            MessageType::VoteResponse => {
                                if let Ok(response) = bincode::deserialize::<VoteResponsePayload>(&msg.payload) {
                                    if response.vote_granted {
                                        let mut state = vote_state.write().await;
                                        if response.term == state.term {
                                            state.votes_received.insert(response.voter_id);
                                            tracing::info!(
                                                "Received vote from {} for term {} (total: {})",
                                                response.voter_id,
                                                response.term,
                                                state.votes_received.len()
                                            );
                                        }
                                    }
                                }
                            }
                            MessageType::FencingToken => {
                                if let Ok(payload) = bincode::deserialize::<FencingTokenPayload>(&msg.payload) {
                                    let current_token = fencing_token.load(Ordering::SeqCst);
                                    if payload.token > current_token {
                                        fencing_token.store(payload.token, Ordering::SeqCst);
                                        tracing::info!(
                                            "Updated fencing token: {} -> {}",
                                            current_token,
                                            payload.token
                                        );
                                    }
                                }
                            }
                            _ => {
                                tracing::trace!("Received message type: {:?}", msg.header.msg_type);
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::warn!("Observer connection closed");
                        break;
                    }
                    Err(_) => {
                        // Timeout - keep waiting
                        continue;
                    }
                }
            }
            tracing::debug!("Connection handler for {:?} stopped", addr);
        });

        Ok(())
    }

    /// Spawn heartbeat task
    fn spawn_heartbeat_task(&self) -> tokio::task::JoinHandle<()> {
        let node_id = self.node_id;
        let term = self.term.clone();
        let fencing_token = self.fencing_token.clone();
        let role = self.role.clone();
        let observer_connections = self.observer_connections.clone();
        let interval = self.config.heartbeat_interval;
        let is_running = self.is_running.clone();
        let current_lsn = self.current_lsn.clone();

        tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);

            while is_running.load(Ordering::SeqCst) {
                timer.tick().await;

                let current_role = *role.read().await;
                let lsn = current_lsn.load(Ordering::SeqCst);
                let heartbeat = super::transport::HeartbeatPayload {
                    node_id,
                    role: current_role,
                    current_lsn: lsn,
                    flush_lsn: lsn,
                    apply_lsn: None,
                    timestamp_ms: chrono::Utc::now().timestamp_millis() as u64,
                    lag_bytes: 0,
                    health: super::transport::HealthStatus::Healthy,
                };

                let payload = match bincode::serialize(&heartbeat) {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                let msg = Message::new(MessageType::Heartbeat, Bytes::from(payload), 0);

                // Send to all observers
                let connections = observer_connections.read().await;
                for (addr, tx) in connections.iter() {
                    if tx.send(msg.clone()).await.is_err() {
                        tracing::warn!("Failed to send heartbeat to {}", addr);
                    }
                }
            }
        })
    }

    /// Spawn election monitoring task
    fn spawn_election_monitor(&self) -> tokio::task::JoinHandle<()> {
        let node_id = self.node_id;
        let role = self.role.clone();
        let known_primary = self.known_primary.clone();
        let cluster_nodes = self.cluster_nodes.clone();
        let state = self.state.clone();
        let event_tx = self.event_tx.clone();
        let election_timeout = self.config.election_timeout;
        let is_running = self.is_running.clone();

        tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(1));

            while is_running.load(Ordering::SeqCst) {
                timer.tick().await;

                // Only standbys monitor for primary failure
                if *role.read().await != NodeRole::Standby {
                    continue;
                }

                // Check if primary is alive
                let nodes = cluster_nodes.read().await;
                let primary_id = known_primary.read().await.clone();

                if let Some(primary_id) = primary_id {
                    if let Some(primary) = nodes.get(&primary_id) {
                        if primary.last_heartbeat.elapsed() > election_timeout {
                            drop(nodes);

                            tracing::warn!(
                                "Primary {} heartbeat timeout, considering election",
                                primary_id
                            );

                            *state.write().await = ProtectionState::Election;
                            let _ = event_tx.send(ProtectionEvent::PrimaryLost {
                                previous_primary: primary_id,
                                reason: "Heartbeat timeout".to_string(),
                            }).await;

                            // Signal that an election should start
                            // The actual election is coordinated through request_votes()
                            let _ = event_tx.send(ProtectionEvent::ElectionNeeded {
                                reason: VoteReason::PrimaryFailure,
                            }).await;
                        }
                    }
                }
            }
        })
    }

    /// Request votes for primary election
    pub async fn request_votes(&self, reason: VoteReason) -> Result<bool> {
        // Increment term
        let new_term = self.term.fetch_add(1, Ordering::SeqCst) + 1;

        tracing::info!(
            "Node {} requesting votes for term {} (reason: {:?})",
            self.node_id,
            new_term,
            reason
        );

        // Update vote state
        {
            let mut vote_state = self.vote_state.write().await;
            vote_state.term = new_term;
            vote_state.voted_for = Some(self.node_id); // Vote for self
            vote_state.votes_received.clear();
            vote_state.votes_received.insert(self.node_id);
            vote_state.election_start = Instant::now();
        }

        *self.state.write().await = ProtectionState::Election;

        let _ = self.event_tx.send(ProtectionEvent::ElectionStarted {
            term: new_term,
            reason,
        }).await;

        // Create vote request
        let vote_request = VoteRequestPayload {
            candidate_id: self.node_id,
            term: new_term,
            last_lsn: self.current_lsn.load(Ordering::SeqCst),
            previous_primary: *self.known_primary.read().await,
            reason,
        };

        let payload = bincode::serialize(&vote_request)
            .map_err(|e| ReplicationError::Internal(e.to_string()))?;

        let msg = Message::new(MessageType::VoteRequest, Bytes::from(payload), 0);

        // Send vote requests to all observers
        let connections = self.observer_connections.read().await;
        let observer_count = connections.len();

        for (addr, tx) in connections.iter() {
            if let Err(e) = tx.send(msg.clone()).await {
                tracing::warn!("Failed to send vote request to {}: {}", addr, e);
            }
        }
        drop(connections);

        // Wait for votes with timeout
        let vote_timeout = self.config.vote_timeout;
        let start = Instant::now();

        loop {
            // Check if we have enough votes
            let vote_state = self.vote_state.read().await;
            let votes_received = vote_state.votes_received.len();

            if votes_received >= self.config.quorum_size {
                tracing::info!(
                    "Quorum reached: {} votes (needed: {})",
                    votes_received,
                    self.config.quorum_size
                );
                break;
            }

            // Check for timeout
            if start.elapsed() >= vote_timeout {
                tracing::warn!(
                    "Vote timeout after {:?}, received {} of {} required votes",
                    start.elapsed(),
                    votes_received,
                    self.config.quorum_size
                );
                break;
            }

            drop(vote_state);

            // Brief pause before checking again
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Final vote count
        let votes_received = self.vote_state.read().await.votes_received.len();

        // Check if we won
        let won = votes_received >= self.config.quorum_size;

        if won {
            self.become_primary(new_term).await?;
        }

        let _ = self.event_tx.send(ProtectionEvent::ElectionCompleted {
            winner: if won { Some(self.node_id) } else { None },
            term: new_term,
        }).await;

        Ok(won)
    }

    /// Become the primary
    async fn become_primary(&self, term: u64) -> Result<()> {
        // Generate new fencing token
        let new_token = self.fencing_token.fetch_add(1, Ordering::SeqCst) + 1;

        tracing::info!(
            "Node {} becoming primary (term: {}, fencing_token: {})",
            self.node_id,
            term,
            new_token
        );

        *self.role.write().await = NodeRole::Primary;
        *self.known_primary.write().await = Some(self.node_id);
        *self.state.write().await = ProtectionState::Normal;

        // Broadcast new fencing token
        self.broadcast_fencing_token(new_token).await?;

        let _ = self.event_tx.send(ProtectionEvent::PrimaryElected {
            node_id: self.node_id,
            term,
            fencing_token: new_token,
        }).await;

        Ok(())
    }

    /// Broadcast new fencing token to all nodes
    async fn broadcast_fencing_token(&self, token: u64) -> Result<()> {
        let payload = FencingTokenPayload {
            token,
            issuer_id: self.node_id,
            term: self.term.load(Ordering::SeqCst),
            timestamp_ms: chrono::Utc::now().timestamp_millis() as u64,
        };

        let payload_bytes = bincode::serialize(&payload)
            .map_err(|e| ReplicationError::Internal(e.to_string()))?;

        let msg = Message::new(MessageType::FencingToken, Bytes::from(payload_bytes), 0);

        let connections = self.observer_connections.read().await;
        for (_, tx) in connections.iter() {
            let _ = tx.send(msg.clone()).await;
        }

        Ok(())
    }

    /// Update the current LSN (called by WAL store on writes)
    pub fn update_current_lsn(&self, lsn: u64) {
        self.current_lsn.store(lsn, Ordering::SeqCst);
    }

    /// Get the current LSN
    pub fn get_current_lsn(&self) -> u64 {
        self.current_lsn.load(Ordering::SeqCst)
    }

    /// Handle incoming vote request
    pub async fn handle_vote_request(&self, request: VoteRequestPayload) -> VoteResponsePayload {
        let current_term = self.term.load(Ordering::SeqCst);
        let mut vote_state = self.vote_state.write().await;

        // If request term is lower, reject
        if request.term < current_term {
            return VoteResponsePayload {
                voter_id: self.node_id,
                vote_granted: false,
                term: current_term,
                fencing_token: self.fencing_token.load(Ordering::SeqCst),
                known_primary: *self.known_primary.read().await,
                rejection_reason: Some("Stale term".to_string()),
            };
        }

        // If we already voted for someone else in this term, reject
        if vote_state.term == request.term {
            if let Some(voted_for) = vote_state.voted_for {
                if voted_for != request.candidate_id {
                    return VoteResponsePayload {
                        voter_id: self.node_id,
                        vote_granted: false,
                        term: current_term,
                        fencing_token: self.fencing_token.load(Ordering::SeqCst),
                        known_primary: *self.known_primary.read().await,
                        rejection_reason: Some(format!("Already voted for {}", voted_for)),
                    };
                }
            }
        }

        // Update term if higher
        if request.term > current_term {
            self.term.store(request.term, Ordering::SeqCst);
            vote_state.term = request.term;
            vote_state.voted_for = None;
        }

        // Grant vote
        vote_state.voted_for = Some(request.candidate_id);

        tracing::info!(
            "Node {} granted vote to {} for term {}",
            self.node_id,
            request.candidate_id,
            request.term
        );

        VoteResponsePayload {
            voter_id: self.node_id,
            vote_granted: true,
            term: request.term,
            fencing_token: self.fencing_token.load(Ordering::SeqCst),
            known_primary: *self.known_primary.read().await,
            rejection_reason: None,
        }
    }

    /// Handle incoming vote response
    pub async fn handle_vote_response(&self, response: VoteResponsePayload) -> Result<bool> {
        let mut vote_state = self.vote_state.write().await;

        // Ignore if not in election or response is for wrong term
        if *self.state.read().await != ProtectionState::Election {
            return Ok(false);
        }

        if response.term != vote_state.term {
            return Ok(false);
        }

        if response.vote_granted {
            vote_state.votes_received.insert(response.voter_id);

            tracing::info!(
                "Node {} received vote from {} ({}/{})",
                self.node_id,
                response.voter_id,
                vote_state.votes_received.len(),
                self.config.quorum_size
            );

            if vote_state.votes_received.len() >= self.config.quorum_size {
                drop(vote_state);
                self.become_primary(self.term.load(Ordering::SeqCst)).await?;
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Handle incoming fencing token
    pub async fn handle_fencing_token(&self, payload: FencingTokenPayload) {
        let current_token = self.fencing_token.load(Ordering::SeqCst);

        if payload.token > current_token {
            let old_token = self.fencing_token.swap(payload.token, Ordering::SeqCst);

            tracing::info!(
                "Fencing token updated: {} -> {} (issuer: {})",
                old_token,
                payload.token,
                payload.issuer_id
            );

            // Update known primary
            *self.known_primary.write().await = Some(payload.issuer_id);

            // If we thought we were primary, step down
            if *self.role.read().await == NodeRole::Primary && payload.issuer_id != self.node_id {
                tracing::warn!(
                    "Stepping down: received higher fencing token from {}",
                    payload.issuer_id
                );
                *self.role.write().await = NodeRole::Standby;
            }

            let _ = self.event_tx.send(ProtectionEvent::FencingTokenChanged {
                old_token,
                new_token: payload.token,
            }).await;
        }
    }

    /// Validate a fencing token for a write operation
    pub fn validate_fencing_token(&self, token: u64) -> bool {
        let current = self.fencing_token.load(Ordering::SeqCst);
        token >= current
    }

    /// Get current fencing token
    pub fn current_fencing_token(&self) -> u64 {
        self.fencing_token.load(Ordering::SeqCst)
    }

    /// Get current term
    pub fn current_term(&self) -> u64 {
        self.term.load(Ordering::SeqCst)
    }

    /// Get current protection state
    pub async fn current_state(&self) -> ProtectionState {
        *self.state.read().await
    }

    /// Get known primary
    pub async fn known_primary(&self) -> Option<Uuid> {
        *self.known_primary.read().await
    }

    /// Get this node's role
    pub async fn role(&self) -> NodeRole {
        *self.role.read().await
    }

    /// Register a cluster node
    pub async fn register_node(&self, node: ClusterNode) {
        self.cluster_nodes.write().await.insert(node.node_id, node);
    }

    /// Update node heartbeat
    pub async fn update_node_heartbeat(&self, node_id: Uuid, lsn: u64) {
        if let Some(node) = self.cluster_nodes.write().await.get_mut(&node_id) {
            node.last_heartbeat = Instant::now();
            node.last_lsn = lsn;
            node.is_healthy = true;
        }
    }

    /// Shutdown the protector
    pub fn shutdown(&self) {
        self.is_running.store(false, Ordering::SeqCst);
        let _ = self.shutdown_tx.send(());
    }
}

// =============================================================================
// OBSERVER NODE
// =============================================================================

/// Observer/Witness node implementation
///
/// Observers participate in voting but don't store data.
/// They're useful for achieving quorum in 2-node setups.
pub struct ObserverNode {
    /// Node ID
    node_id: Uuid,
    /// Listen address
    listen_addr: SocketAddr,
    /// Current term
    term: Arc<AtomicU64>,
    /// Current fencing token
    fencing_token: Arc<AtomicU64>,
    /// Known primary
    known_primary: Arc<RwLock<Option<Uuid>>>,
    /// Vote state
    vote_state: Arc<RwLock<VoteState>>,
    /// Known nodes
    known_nodes: Arc<RwLock<HashMap<Uuid, (SocketAddr, Instant)>>>,
    /// Shutdown signal
    shutdown_tx: broadcast::Sender<()>,
}

impl ObserverNode {
    /// Create a new observer node
    pub fn new(node_id: Uuid, listen_addr: SocketAddr) -> Self {
        let (shutdown_tx, _) = broadcast::channel(1);

        Self {
            node_id,
            listen_addr,
            term: Arc::new(AtomicU64::new(1)),
            fencing_token: Arc::new(AtomicU64::new(0)),
            known_primary: Arc::new(RwLock::new(None)),
            vote_state: Arc::new(RwLock::new(VoteState {
                term: 1,
                voted_for: None,
                votes_received: HashSet::new(),
                election_start: Instant::now(),
            })),
            known_nodes: Arc::new(RwLock::new(HashMap::new())),
            shutdown_tx,
        }
    }

    /// Start the observer node
    pub async fn start(&self) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(&self.listen_addr)
            .await
            .map_err(|e| ReplicationError::Network(format!("Bind failed: {}", e)))?;

        tracing::info!(
            "Observer node {} listening on {}",
            self.node_id,
            self.listen_addr
        );

        let mut shutdown_rx = self.shutdown_tx.subscribe();

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, addr)) => {
                            let conn = ReplicationConnection::from_stream(stream, addr);
                            self.handle_connection(conn).await;
                        }
                        Err(e) => {
                            tracing::error!("Accept error: {}", e);
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::info!("Observer node shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Handle an incoming connection
    async fn handle_connection(&self, mut conn: ReplicationConnection) {
        let term = self.term.clone();
        let fencing_token = self.fencing_token.clone();
        let known_primary = self.known_primary.clone();
        let vote_state = self.vote_state.clone();
        let known_nodes = self.known_nodes.clone();
        let node_id = self.node_id;

        tokio::spawn(async move {
            loop {
                match conn.recv().await {
                    Ok(msg) => {
                        match msg.header.msg_type {
                            MessageType::VoteRequest => {
                                let request: VoteRequestPayload = match bincode::deserialize(&msg.payload) {
                                    Ok(r) => r,
                                    Err(_) => continue,
                                };

                                let response = Self::handle_vote_request_static(
                                    node_id,
                                    &term,
                                    &fencing_token,
                                    &known_primary,
                                    &vote_state,
                                    request,
                                ).await;

                                let payload = match bincode::serialize(&response) {
                                    Ok(p) => p,
                                    Err(_) => continue,
                                };

                                let _ = conn.send(MessageType::VoteResponse, Bytes::from(payload)).await;
                            }
                            MessageType::Heartbeat => {
                                let heartbeat: super::transport::HeartbeatPayload =
                                    match bincode::deserialize(&msg.payload) {
                                        Ok(h) => h,
                                        Err(_) => continue,
                                    };

                                // Update known nodes
                                known_nodes.write().await.insert(
                                    heartbeat.node_id,
                                    (conn.remote_addr(), Instant::now()),
                                );

                                // Track primary
                                if heartbeat.role == NodeRole::Primary {
                                    *known_primary.write().await = Some(heartbeat.node_id);
                                }
                            }
                            MessageType::FencingToken => {
                                let payload: FencingTokenPayload =
                                    match bincode::deserialize(&msg.payload) {
                                        Ok(p) => p,
                                        Err(_) => continue,
                                    };

                                let current = fencing_token.load(Ordering::SeqCst);
                                if payload.token > current {
                                    fencing_token.store(payload.token, Ordering::SeqCst);
                                    *known_primary.write().await = Some(payload.issuer_id);
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    /// Handle vote request (static version for spawned task)
    async fn handle_vote_request_static(
        node_id: Uuid,
        term: &AtomicU64,
        fencing_token: &AtomicU64,
        known_primary: &RwLock<Option<Uuid>>,
        vote_state: &RwLock<VoteState>,
        request: VoteRequestPayload,
    ) -> VoteResponsePayload {
        let current_term = term.load(Ordering::SeqCst);
        let mut state = vote_state.write().await;

        // Reject stale term
        if request.term < current_term {
            return VoteResponsePayload {
                voter_id: node_id,
                vote_granted: false,
                term: current_term,
                fencing_token: fencing_token.load(Ordering::SeqCst),
                known_primary: *known_primary.read().await,
                rejection_reason: Some("Stale term".to_string()),
            };
        }

        // Already voted in this term
        if state.term == request.term {
            if let Some(voted_for) = state.voted_for {
                if voted_for != request.candidate_id {
                    return VoteResponsePayload {
                        voter_id: node_id,
                        vote_granted: false,
                        term: current_term,
                        fencing_token: fencing_token.load(Ordering::SeqCst),
                        known_primary: *known_primary.read().await,
                        rejection_reason: Some(format!("Already voted for {}", voted_for)),
                    };
                }
            }
        }

        // Update term
        if request.term > current_term {
            term.store(request.term, Ordering::SeqCst);
            state.term = request.term;
            state.voted_for = None;
        }

        // Grant vote
        state.voted_for = Some(request.candidate_id);

        tracing::info!(
            "Observer {} granted vote to {} for term {}",
            node_id,
            request.candidate_id,
            request.term
        );

        VoteResponsePayload {
            voter_id: node_id,
            vote_granted: true,
            term: request.term,
            fencing_token: fencing_token.load(Ordering::SeqCst),
            known_primary: *known_primary.read().await,
            rejection_reason: None,
        }
    }

    /// Shutdown the observer
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
    fn test_protection_state() {
        assert_eq!(ProtectionState::Normal, ProtectionState::Normal);
        assert_ne!(ProtectionState::Normal, ProtectionState::Election);
    }

    #[test]
    fn test_observer_config_default() {
        let config = ObserverConfig::default();
        assert_eq!(config.quorum_size, 2);
        assert!(config.observers.is_empty());
    }

    #[tokio::test]
    async fn test_split_brain_protector_creation() {
        let node_id = Uuid::new_v4();
        let config = ObserverConfig::default();
        let (protector, _rx) = SplitBrainProtector::new(node_id, NodeRole::Primary, config);

        assert_eq!(protector.current_term(), 1);
        assert_eq!(protector.current_fencing_token(), 1);
        assert_eq!(protector.role().await, NodeRole::Primary);
    }

    #[tokio::test]
    async fn test_vote_request_handling() {
        let node_id = Uuid::new_v4();
        let config = ObserverConfig::default();
        let (protector, _rx) = SplitBrainProtector::new(node_id, NodeRole::Standby, config);

        let candidate_id = Uuid::new_v4();
        let request = VoteRequestPayload {
            candidate_id,
            term: 2,
            last_lsn: 100,
            previous_primary: None,
            reason: VoteReason::PrimaryFailure,
        };

        let response = protector.handle_vote_request(request).await;
        assert!(response.vote_granted);
        assert_eq!(response.term, 2);
    }

    #[tokio::test]
    async fn test_fencing_token_validation() {
        let node_id = Uuid::new_v4();
        let config = ObserverConfig::default();
        let (protector, _rx) = SplitBrainProtector::new(node_id, NodeRole::Primary, config);

        // Initial token is 1
        assert!(protector.validate_fencing_token(1));
        assert!(protector.validate_fencing_token(2));
        assert!(!protector.validate_fencing_token(0));
    }

    #[tokio::test]
    async fn test_fencing_token_update() {
        let node_id = Uuid::new_v4();
        let config = ObserverConfig::default();
        let (protector, mut rx) = SplitBrainProtector::new(node_id, NodeRole::Standby, config);

        let issuer_id = Uuid::new_v4();
        let payload = FencingTokenPayload {
            token: 5,
            issuer_id,
            term: 2,
            timestamp_ms: 0,
        };

        protector.handle_fencing_token(payload).await;

        assert_eq!(protector.current_fencing_token(), 5);
        assert_eq!(protector.known_primary().await, Some(issuer_id));

        // Check event was sent
        if let Some(event) = rx.recv().await {
            match event {
                ProtectionEvent::FencingTokenChanged { old_token, new_token } => {
                    assert_eq!(old_token, 1);
                    assert_eq!(new_token, 5);
                }
                _ => panic!("Expected FencingTokenChanged event"),
            }
        }
    }

    #[test]
    fn test_observer_node_creation() {
        let node_id = Uuid::new_v4();
        let addr: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let observer = ObserverNode::new(node_id, addr);

        assert_eq!(observer.term.load(Ordering::SeqCst), 1);
    }
}
