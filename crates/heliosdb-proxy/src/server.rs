//! Proxy Server Implementation
//!
//! Main server that accepts client connections and routes them to backends.
//! Implements PostgreSQL wire protocol forwarding with TWR (Transparent Write Routing).

use crate::admin::{AdminServer, AdminState, ConfigSnapshot, NodeSnapshot};
use crate::config::{NodeConfig, NodeRole, ProxyConfig, TrMode};
use crate::protocol::{
    AuthRequest, ErrorResponse, Message, MessageType, ParseMessage, ProtocolCodec, QueryMessage,
    StartupMessage, TransactionStatus,
};
use crate::{ProxyError, Result};
use bytes::{BufMut, BytesMut};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, RwLock, Semaphore};
use uuid::Uuid;

/// Proxy server
pub struct ProxyServer {
    config: ProxyConfig,
    state: Arc<ServerState>,
    shutdown_tx: broadcast::Sender<()>,
}

/// Server runtime state
struct ServerState {
    /// Active client sessions
    sessions: RwLock<HashMap<Uuid, Arc<ClientSession>>>,
    /// Connection pools per node
    pools: RwLock<HashMap<String, NodePool>>,
    /// Node health status
    health: RwLock<HashMap<String, NodeHealth>>,
    /// Metrics
    metrics: ServerMetrics,
    /// Load balancer state
    lb_state: RwLock<LoadBalancerState>,
}

/// Per-node connection pool
struct NodePool {
    /// Node configuration
    config: NodeConfig,
    /// Available connections
    connections: RwLock<Vec<BackendConnection>>,
    /// Connection limit semaphore
    semaphore: Semaphore,
    /// Active connection count
    active_count: AtomicU64,
}

/// Backend connection
struct BackendConnection {
    /// Connection ID
    id: Uuid,
    /// TCP stream (wrapped for protocol handling)
    stream: Option<TcpStream>,
    /// Creation time
    created_at: chrono::DateTime<chrono::Utc>,
    /// Last used time
    last_used: chrono::DateTime<chrono::Utc>,
    /// Whether connection is healthy
    healthy: bool,
}

/// Node health status
#[derive(Debug, Clone)]
pub struct NodeHealth {
    /// Node address
    pub address: String,
    /// Whether node is healthy
    pub healthy: bool,
    /// Last check time
    pub last_check: chrono::DateTime<chrono::Utc>,
    /// Consecutive failures
    pub failure_count: u32,
    /// Last error message
    pub last_error: Option<String>,
    /// Average latency (ms)
    pub latency_ms: f64,
    /// Replication lag (if applicable)
    pub replication_lag_bytes: Option<u64>,
}

/// Server metrics
#[derive(Default)]
struct ServerMetrics {
    /// Total connections accepted
    connections_accepted: AtomicU64,
    /// Total connections closed
    connections_closed: AtomicU64,
    /// Total queries processed
    queries_processed: AtomicU64,
    /// Total bytes received from clients
    bytes_received: AtomicU64,
    /// Total bytes sent to clients
    bytes_sent: AtomicU64,
    /// Failover count
    failovers: AtomicU64,
}

/// Load balancer state
struct LoadBalancerState {
    /// Round-robin counter
    rr_counter: u64,
    /// Node weights for weighted round-robin
    weights: HashMap<String, u32>,
    /// Current weight counter
    weight_counter: HashMap<String, u32>,
}

/// Client session
pub struct ClientSession {
    /// Session ID
    pub id: Uuid,
    /// Client address
    pub client_addr: SocketAddr,
    /// Current backend node
    pub current_node: RwLock<Option<String>>,
    /// Transaction state
    pub tx_state: RwLock<TransactionState>,
    /// Session variables
    pub variables: RwLock<HashMap<String, String>>,
    /// Created at
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// TR mode for this session
    pub tr_mode: TrMode,
}

/// Transaction state
#[derive(Debug, Clone, Default)]
pub struct TransactionState {
    /// Whether in a transaction
    pub in_transaction: bool,
    /// Transaction ID
    pub tx_id: Option<Uuid>,
    /// Statements executed in current transaction
    pub statements: Vec<StatementLog>,
    /// Read-only transaction
    pub read_only: bool,
    /// Savepoints
    pub savepoints: Vec<String>,
}

/// Logged statement for TR replay
#[derive(Debug, Clone)]
pub struct StatementLog {
    /// Statement SQL
    pub sql: String,
    /// Parameters
    pub params: Vec<String>,
    /// Result checksum
    pub result_checksum: Option<u64>,
    /// Execution time
    pub executed_at: chrono::DateTime<chrono::Utc>,
}

impl ProxyServer {
    /// Create a new proxy server
    pub fn new(config: ProxyConfig) -> Result<Self> {
        let (shutdown_tx, _) = broadcast::channel(1);

        // Initialize pools for each node
        let mut pools = HashMap::new();
        for node in &config.nodes {
            let pool = NodePool {
                config: node.clone(),
                connections: RwLock::new(Vec::new()),
                semaphore: Semaphore::new(config.pool.max_connections),
                active_count: AtomicU64::new(0),
            };
            pools.insert(node.address(), pool);
        }

        // Initialize health status
        let mut health = HashMap::new();
        for node in &config.nodes {
            health.insert(
                node.address(),
                NodeHealth {
                    address: node.address(),
                    healthy: true, // Assume healthy until proven otherwise
                    last_check: chrono::Utc::now(),
                    failure_count: 0,
                    last_error: None,
                    latency_ms: 0.0,
                    replication_lag_bytes: None,
                },
            );
        }

        // Initialize load balancer state
        let mut weights = HashMap::new();
        let mut weight_counter = HashMap::new();
        for node in &config.nodes {
            weights.insert(node.address(), node.weight);
            weight_counter.insert(node.address(), node.weight);
        }

        let state = Arc::new(ServerState {
            sessions: RwLock::new(HashMap::new()),
            pools: RwLock::new(pools),
            health: RwLock::new(health),
            metrics: ServerMetrics::default(),
            lb_state: RwLock::new(LoadBalancerState {
                rr_counter: 0,
                weights,
                weight_counter,
            }),
        });

        Ok(Self {
            config,
            state,
            shutdown_tx,
        })
    }

    /// Run the proxy server
    pub async fn run(&self) -> Result<()> {
        let listener = TcpListener::bind(&self.config.listen_address)
            .await
            .map_err(|e| ProxyError::Network(format!("Failed to bind: {}", e)))?;

        tracing::info!("Proxy listening on {}", self.config.listen_address);

        // Start background tasks
        let health_task = self.spawn_health_checker();
        let pool_task = self.spawn_pool_manager();

        // Start admin server
        let admin_task = self.spawn_admin_server();

        let mut shutdown_rx = self.shutdown_tx.subscribe();

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, addr)) => {
                            self.state.metrics.connections_accepted.fetch_add(1, Ordering::Relaxed);
                            let state = self.state.clone();
                            let config = self.config.clone();
                            let shutdown_tx = self.shutdown_tx.clone();

                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_client(stream, addr, state, config, shutdown_tx).await {
                                    tracing::error!("Client handler error: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Accept error: {}", e);
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::info!("Shutdown signal received");
                    break;
                }
            }
        }

        // Wait for background tasks
        health_task.abort();
        pool_task.abort();
        admin_task.abort();

        Ok(())
    }

    /// Spawn admin API server
    fn spawn_admin_server(&self) -> tokio::task::JoinHandle<()> {
        let config = self.config.clone();
        let state = self.state.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        tokio::spawn(async move {
            // Create admin state
            let admin_state = Arc::new(AdminState::new());

            // Initialize config snapshot
            {
                let mut snapshot = admin_state.config_snapshot.write().await;
                *snapshot = ConfigSnapshot {
                    listen_address: config.listen_address.clone(),
                    admin_address: config.admin_address.clone(),
                    tr_enabled: config.tr_enabled,
                    tr_mode: format!("{:?}", config.tr_mode),
                    pool_min_connections: config.pool.min_connections,
                    pool_max_connections: config.pool.max_connections,
                    nodes: config.nodes.iter().map(|n| NodeSnapshot {
                        address: n.address(),
                        role: format!("{:?}", n.role),
                        weight: n.weight,
                        enabled: n.enabled,
                    }).collect(),
                };
            }

            // Set proxy config for SQL routing
            admin_state.set_proxy_config(config.clone()).await;

            // Create admin server
            let admin_server = AdminServer::new(config.admin_address.clone(), admin_state.clone());

            // Spawn state sync task
            let admin_state_sync = admin_state.clone();
            let server_state = state.clone();
            let sync_task = tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                loop {
                    interval.tick().await;

                    // Sync health status
                    {
                        let health = server_state.health.read().await;
                        let mut admin_health = admin_state_sync.node_health.write().await;
                        *admin_health = health.clone();
                    }

                    // Sync metrics
                    {
                        let metrics = ServerMetricsSnapshot {
                            connections_accepted: server_state.metrics.connections_accepted.load(Ordering::Relaxed),
                            connections_closed: server_state.metrics.connections_closed.load(Ordering::Relaxed),
                            queries_processed: server_state.metrics.queries_processed.load(Ordering::Relaxed),
                            bytes_received: server_state.metrics.bytes_received.load(Ordering::Relaxed),
                            bytes_sent: server_state.metrics.bytes_sent.load(Ordering::Relaxed),
                            failovers: server_state.metrics.failovers.load(Ordering::Relaxed),
                        };
                        let mut admin_metrics = admin_state_sync.metrics.write().await;
                        *admin_metrics = metrics;
                    }

                    // Sync session count
                    {
                        let sessions = server_state.sessions.read().await;
                        let mut admin_sessions = admin_state_sync.active_sessions.write().await;
                        *admin_sessions = sessions.len() as u64;
                    }
                }
            });

            // Run admin server
            tokio::select! {
                result = admin_server.run() => {
                    if let Err(e) = result {
                        tracing::error!("Admin server error: {}", e);
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::info!("Admin server shutting down");
                }
            }

            sync_task.abort();
        })
    }

    /// Handle a client connection
    async fn handle_client(
        mut stream: TcpStream,
        addr: SocketAddr,
        state: Arc<ServerState>,
        config: ProxyConfig,
        _shutdown_tx: broadcast::Sender<()>,
    ) -> Result<()> {
        tracing::debug!("New client connection from {}", addr);

        // Create session
        let session = Arc::new(ClientSession {
            id: Uuid::new_v4(),
            client_addr: addr,
            current_node: RwLock::new(None),
            tx_state: RwLock::new(TransactionState::default()),
            variables: RwLock::new(HashMap::new()),
            created_at: chrono::Utc::now(),
            tr_mode: config.tr_mode,
        });

        // Register session
        {
            let mut sessions = state.sessions.write().await;
            sessions.insert(session.id, session.clone());
        }

        // Main client loop
        let result = Self::client_loop(&mut stream, &session, &state, &config).await;

        // Cleanup session
        {
            let mut sessions = state.sessions.write().await;
            sessions.remove(&session.id);
        }

        state
            .metrics
            .connections_closed
            .fetch_add(1, Ordering::Relaxed);

        result
    }

    /// Main client processing loop with full PostgreSQL protocol handling
    async fn client_loop(
        stream: &mut TcpStream,
        session: &Arc<ClientSession>,
        state: &Arc<ServerState>,
        config: &ProxyConfig,
    ) -> Result<()> {
        let codec = ProtocolCodec::new();
        let mut buffer = BytesMut::with_capacity(8192);
        let mut backend_stream: Option<TcpStream> = None;
        let mut backend_node: Option<String> = None;

        // Handle startup phase
        let startup_result =
            Self::handle_startup(stream, &mut buffer, &codec, session, state, config).await;

        match startup_result {
            Ok((Some(stream_conn), node_addr)) => {
                backend_stream = Some(stream_conn);
                backend_node = Some(node_addr);
            }
            Ok((None, _)) => {
                // SSL rejected or cancel request, connection should close
                return Ok(());
            }
            Err(e) => {
                tracing::error!("Startup failed: {}", e);
                // Send error to client
                let err_msg = Self::create_error_response("08006", &format!("Startup failed: {}", e));
                let _ = stream.write_all(&err_msg).await;
                return Err(e);
            }
        }

        // Main query loop
        loop {
            // Read from client
            let mut read_buf = vec![0u8; 8192];
            let n = stream
                .read(&mut read_buf)
                .await
                .map_err(|e| ProxyError::Network(format!("Read error: {}", e)))?;

            if n == 0 {
                // Client disconnected
                break;
            }

            buffer.extend_from_slice(&read_buf[..n]);
            state.metrics.bytes_received.fetch_add(n as u64, Ordering::Relaxed);

            // Process all complete messages in buffer
            while let Some(msg) = codec.decode_message(&mut buffer)? {
                // Handle Terminate message
                if msg.msg_type == MessageType::Terminate {
                    return Ok(());
                }

                // Route and process the message
                let (response, new_backend, new_node) = Self::route_and_forward(
                    &msg,
                    backend_stream.take(),
                    backend_node.take(),
                    session,
                    state,
                    config,
                )
                .await?;

                backend_stream = new_backend;
                backend_node = new_node;

                // Send response to client
                if !response.is_empty() {
                    stream
                        .write_all(&response)
                        .await
                        .map_err(|e| ProxyError::Network(format!("Write error: {}", e)))?;

                    state
                        .metrics
                        .bytes_sent
                        .fetch_add(response.len() as u64, Ordering::Relaxed);
                }

                state.metrics.queries_processed.fetch_add(1, Ordering::Relaxed);
            }
        }

        Ok(())
    }

    /// Handle PostgreSQL startup phase (SSL, authentication)
    async fn handle_startup(
        client_stream: &mut TcpStream,
        buffer: &mut BytesMut,
        codec: &ProtocolCodec,
        session: &Arc<ClientSession>,
        state: &Arc<ServerState>,
        config: &ProxyConfig,
    ) -> Result<(Option<TcpStream>, String)> {
        // Read startup message
        let mut read_buf = vec![0u8; 1024];
        let n = client_stream
            .read(&mut read_buf)
            .await
            .map_err(|e| ProxyError::Network(format!("Startup read error: {}", e)))?;

        if n == 0 {
            return Ok((None, String::new()));
        }

        buffer.extend_from_slice(&read_buf[..n]);

        // Parse startup message
        let startup_msg = codec.decode_startup(buffer)?;

        match startup_msg {
            Some(StartupMessage::SSLRequest) => {
                // Reject SSL (send 'N')
                client_stream
                    .write_all(&[b'N'])
                    .await
                    .map_err(|e| ProxyError::Network(format!("SSL reject error: {}", e)))?;

                // Read actual startup message
                buffer.clear();
                let n = client_stream
                    .read(&mut read_buf)
                    .await
                    .map_err(|e| ProxyError::Network(format!("Post-SSL read error: {}", e)))?;

                if n == 0 {
                    return Ok((None, String::new()));
                }

                buffer.extend_from_slice(&read_buf[..n]);

                // Parse the real startup message
                return Self::process_startup(
                    client_stream,
                    buffer,
                    codec,
                    session,
                    state,
                    config,
                )
                .await;
            }
            Some(StartupMessage::CancelRequest { .. }) => {
                // Cancel requests are handled separately, just close connection
                return Ok((None, String::new()));
            }
            Some(StartupMessage::Startup { params, .. }) => {
                // Connect to backend and forward startup
                return Self::connect_and_authenticate(
                    client_stream,
                    &params,
                    session,
                    state,
                    config,
                )
                .await;
            }
            None => {
                return Err(ProxyError::Protocol("Incomplete startup message".to_string()));
            }
        }
    }

    /// Process startup message after SSL negotiation
    async fn process_startup(
        client_stream: &mut TcpStream,
        buffer: &mut BytesMut,
        codec: &ProtocolCodec,
        session: &Arc<ClientSession>,
        state: &Arc<ServerState>,
        config: &ProxyConfig,
    ) -> Result<(Option<TcpStream>, String)> {
        let startup_msg = codec.decode_startup(buffer)?;

        match startup_msg {
            Some(StartupMessage::Startup { params, .. }) => {
                Self::connect_and_authenticate(client_stream, &params, session, state, config).await
            }
            _ => Err(ProxyError::Protocol("Expected startup message".to_string())),
        }
    }

    /// Connect to backend and handle authentication
    async fn connect_and_authenticate(
        client_stream: &mut TcpStream,
        params: &HashMap<String, String>,
        session: &Arc<ClientSession>,
        state: &Arc<ServerState>,
        config: &ProxyConfig,
    ) -> Result<(Option<TcpStream>, String)> {
        // Select initial backend node (primary for now)
        let node_addr = Self::select_node(session, state, config).await?;

        // Connect to backend
        let mut backend = tokio::time::timeout(
            config.pool.acquire_timeout(),
            TcpStream::connect(&node_addr),
        )
        .await
        .map_err(|_| ProxyError::Connection(format!("Connection timeout to {}", node_addr)))?
        .map_err(|e| ProxyError::Connection(format!("Failed to connect to {}: {}", node_addr, e)))?;

        // Build and send startup message to backend
        let startup_bytes = Self::build_startup_message(params);
        backend
            .write_all(&startup_bytes)
            .await
            .map_err(|e| ProxyError::Network(format!("Backend startup write error: {}", e)))?;

        // Forward authentication messages between client and backend
        Self::proxy_authentication(client_stream, &mut backend).await?;

        // Store session variables
        {
            let mut vars = session.variables.write().await;
            for (k, v) in params {
                vars.insert(k.clone(), v.clone());
            }
        }

        Ok((Some(backend), node_addr))
    }

    /// Build PostgreSQL startup message
    fn build_startup_message(params: &HashMap<String, String>) -> Vec<u8> {
        let mut payload = BytesMut::new();

        // Protocol version 3.0
        payload.put_u32(196608);

        // Parameters
        for (key, value) in params {
            payload.extend_from_slice(key.as_bytes());
            payload.put_u8(0);
            payload.extend_from_slice(value.as_bytes());
            payload.put_u8(0);
        }
        payload.put_u8(0); // Terminator

        // Build complete message with length prefix
        let mut msg = BytesMut::new();
        msg.put_u32((payload.len() + 4) as u32);
        msg.extend_from_slice(&payload);

        msg.to_vec()
    }

    /// Proxy authentication messages between client and backend
    async fn proxy_authentication(
        client_stream: &mut TcpStream,
        backend_stream: &mut TcpStream,
    ) -> Result<()> {
        let codec = ProtocolCodec::new();
        let mut backend_buffer = BytesMut::with_capacity(4096);
        let mut client_buffer = BytesMut::with_capacity(4096);

        loop {
            // Read from backend
            let mut read_buf = vec![0u8; 4096];
            let n = backend_stream
                .read(&mut read_buf)
                .await
                .map_err(|e| ProxyError::Network(format!("Backend auth read error: {}", e)))?;

            if n == 0 {
                return Err(ProxyError::Connection("Backend closed during auth".to_string()));
            }

            backend_buffer.extend_from_slice(&read_buf[..n]);

            // Forward all data to client
            client_stream
                .write_all(&read_buf[..n])
                .await
                .map_err(|e| ProxyError::Network(format!("Client auth write error: {}", e)))?;

            // Check for authentication complete or error
            while let Some(msg) = codec.decode_message(&mut backend_buffer.clone())? {
                match msg.msg_type {
                    MessageType::AuthRequest => {
                        // Check if auth OK
                        if msg.payload.len() >= 4 {
                            let auth_type =
                                i32::from_be_bytes([msg.payload[0], msg.payload[1], msg.payload[2], msg.payload[3]]);
                            if auth_type == 0 {
                                // AuthenticationOk - continue to read ReadyForQuery
                            }
                        }
                    }
                    MessageType::ReadyForQuery => {
                        // Authentication complete
                        return Ok(());
                    }
                    MessageType::ErrorResponse => {
                        // Authentication failed - error already sent to client
                        return Err(ProxyError::Auth("Authentication failed".to_string()));
                    }
                    _ => {
                        // Continue forwarding
                    }
                }
                // Advance the actual buffer
                let _ = codec.decode_message(&mut backend_buffer)?;
            }

            // If backend requires password, forward client's response
            // Read password from client if needed
            let n = tokio::time::timeout(Duration::from_millis(100), client_stream.read(&mut read_buf))
                .await;

            if let Ok(Ok(n)) = n {
                if n > 0 {
                    client_buffer.extend_from_slice(&read_buf[..n]);
                    backend_stream
                        .write_all(&read_buf[..n])
                        .await
                        .map_err(|e| ProxyError::Network(format!("Backend password write error: {}", e)))?;
                }
            }
        }
    }

    /// Route message and forward to appropriate backend
    async fn route_and_forward(
        msg: &Message,
        mut backend_stream: Option<TcpStream>,
        current_node: Option<String>,
        session: &Arc<ClientSession>,
        state: &Arc<ServerState>,
        config: &ProxyConfig,
    ) -> Result<(Vec<u8>, Option<TcpStream>, Option<String>)> {
        // Determine if this is a write operation
        let is_write = Self::is_write_message(msg);

        // Sticky session mode: stay on same backend if we have one and it's healthy
        // Only switch if:
        // 1. No current backend
        // 2. Write query and current backend is not primary
        // 3. Current backend is unhealthy
        let need_switch = if let Some(ref current) = current_node {
            let health = state.health.read().await;
            let current_healthy = health.get(current).map(|h| h.healthy).unwrap_or(false);

            if !current_healthy {
                true
            } else if is_write {
                // Check if current is primary
                let is_primary = config.nodes.iter()
                    .find(|n| n.address() == *current)
                    .map(|n| n.role == NodeRole::Primary)
                    .unwrap_or(false);
                !is_primary
            } else {
                false
            }
        } else {
            true
        };

        let target_node = if need_switch {
            if is_write {
                Self::select_primary_with_timeout(session, state, config).await?
            } else {
                Self::select_read_node(session, state, config).await?
            }
        } else {
            current_node.clone().unwrap()
        };

        let mut backend = if need_switch {
            // Close old connection if any
            drop(backend_stream);

            // Connect to new backend
            let new_backend = tokio::time::timeout(
                config.pool.acquire_timeout(),
                TcpStream::connect(&target_node),
            )
            .await
            .map_err(|_| ProxyError::Connection(format!("Connection timeout to {}", target_node)))?
            .map_err(|e| {
                ProxyError::Connection(format!("Failed to connect to {}: {}", target_node, e))
            })?;

            // Re-authenticate to new backend (silently, without forwarding to client)
            let params = session.variables.read().await.clone();
            let startup = Self::build_startup_message(&params);
            let mut backend = new_backend;
            backend
                .write_all(&startup)
                .await
                .map_err(|e| ProxyError::Network(format!("Backend startup error: {}", e)))?;

            // Complete authentication by reading until ReadyForQuery
            Self::complete_backend_auth(&mut backend).await?;

            tracing::debug!(
                "Switched backend from {:?} to {} for {} query",
                current_node,
                target_node,
                if is_write { "write" } else { "read" }
            );

            backend
        } else {
            backend_stream.unwrap()
        };

        // Forward the message to backend
        let encoded = msg.encode();
        backend
            .write_all(&encoded)
            .await
            .map_err(|e| ProxyError::Network(format!("Backend write error: {}", e)))?;

        // Read response from backend
        let mut response = Vec::new();
        let mut response_buffer = BytesMut::with_capacity(8192);
        let codec = ProtocolCodec::new();

        loop {
            let mut read_buf = vec![0u8; 8192];
            let n = tokio::time::timeout(Duration::from_secs(30), backend.read(&mut read_buf))
                .await
                .map_err(|_| ProxyError::Network("Backend read timeout".to_string()))?
                .map_err(|e| ProxyError::Network(format!("Backend read error: {}", e)))?;

            if n == 0 {
                break;
            }

            response.extend_from_slice(&read_buf[..n]);
            response_buffer.extend_from_slice(&read_buf[..n]);

            // Check if we've received ReadyForQuery (end of response)
            while let Some(resp_msg) = codec.decode_message(&mut response_buffer.clone())? {
                if resp_msg.msg_type == MessageType::ReadyForQuery {
                    // Update transaction state
                    if !resp_msg.payload.is_empty() {
                        let status = TransactionStatus::from_byte(resp_msg.payload[0]);
                        let mut tx_state = session.tx_state.write().await;
                        tx_state.in_transaction = status != TransactionStatus::Idle;
                    }
                    return Ok((response, Some(backend), Some(target_node)));
                }
                let _ = codec.decode_message(&mut response_buffer)?;
            }
        }

        Ok((response, Some(backend), Some(target_node)))
    }

    /// Check if a message is a write operation
    fn is_write_message(msg: &Message) -> bool {
        match msg.msg_type {
            MessageType::Query => {
                // Parse query and check if it's a write
                if let Ok(query_msg) = QueryMessage::parse(msg.payload.clone()) {
                    Self::is_write_query(&query_msg.query)
                } else {
                    false
                }
            }
            MessageType::Parse => {
                // Parse prepared statement
                if let Ok(parse_msg) = ParseMessage::parse(msg.payload.clone()) {
                    Self::is_write_query(&parse_msg.query)
                } else {
                    false
                }
            }
            // Execute, Bind, etc. maintain the current connection
            _ => false,
        }
    }

    /// Check if SQL query is a write operation
    fn is_write_query(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();

        // Write operations
        if upper.starts_with("INSERT")
            || upper.starts_with("UPDATE")
            || upper.starts_with("DELETE")
            || upper.starts_with("CREATE")
            || upper.starts_with("DROP")
            || upper.starts_with("ALTER")
            || upper.starts_with("TRUNCATE")
            || upper.starts_with("GRANT")
            || upper.starts_with("REVOKE")
            || upper.starts_with("VACUUM")
            || upper.starts_with("REINDEX")
            || upper.starts_with("CLUSTER")
        {
            return true;
        }

        // Transaction control goes to current node
        if upper.starts_with("BEGIN")
            || upper.starts_with("START")
            || upper.starts_with("COMMIT")
            || upper.starts_with("ROLLBACK")
            || upper.starts_with("SAVEPOINT")
            || upper.starts_with("RELEASE")
        {
            return true;
        }

        // SET commands go to primary to maintain session state
        if upper.starts_with("SET") && !upper.starts_with("SET TRANSACTION READ ONLY") {
            return true;
        }

        false
    }

    /// Select primary node with write timeout during failover
    async fn select_primary_with_timeout(
        session: &Arc<ClientSession>,
        state: &Arc<ServerState>,
        config: &ProxyConfig,
    ) -> Result<String> {
        let timeout = config.write_timeout();
        let start = std::time::Instant::now();
        let check_interval = Duration::from_millis(500);

        loop {
            // Try to find healthy primary
            let health = state.health.read().await;
            let primary = config
                .nodes
                .iter()
                .find(|n| n.role == NodeRole::Primary && n.enabled);

            if let Some(primary_node) = primary {
                if let Some(node_health) = health.get(&primary_node.address()) {
                    if node_health.healthy {
                        // Update session's current node
                        let mut current = session.current_node.write().await;
                        *current = Some(primary_node.address());
                        return Ok(primary_node.address());
                    }
                }
            }
            drop(health);

            // Check if timeout exceeded
            if start.elapsed() >= timeout {
                state.metrics.failovers.fetch_add(1, Ordering::Relaxed);
                return Err(ProxyError::NoHealthyNodes);
            }

            tracing::warn!(
                "Primary unavailable, waiting for failover... ({:.1}s elapsed, {:.1}s timeout)",
                start.elapsed().as_secs_f64(),
                timeout.as_secs_f64()
            );

            // Wait before retry
            tokio::time::sleep(check_interval).await;
        }
    }

    /// Select node for read operations with load balancing
    async fn select_read_node(
        session: &Arc<ClientSession>,
        state: &Arc<ServerState>,
        config: &ProxyConfig,
    ) -> Result<String> {
        // If in transaction, stick to current node
        {
            let tx_state = session.tx_state.read().await;
            if tx_state.in_transaction {
                if let Some(node) = session.current_node.read().await.clone() {
                    return Ok(node);
                }
            }
        }

        // Get healthy nodes (prefer standbys for reads)
        let health = state.health.read().await;
        let healthy_standbys: Vec<&NodeConfig> = config
            .nodes
            .iter()
            .filter(|n| {
                n.enabled
                    && (n.role == NodeRole::Standby || n.role == NodeRole::ReadReplica)
                    && health
                        .get(&n.address())
                        .map(|h| h.healthy)
                        .unwrap_or(false)
            })
            .collect();

        if !healthy_standbys.is_empty() {
            // Round-robin across healthy standbys
            let mut lb_state = state.lb_state.write().await;
            let index = lb_state.rr_counter as usize % healthy_standbys.len();
            lb_state.rr_counter = lb_state.rr_counter.wrapping_add(1);
            let node_addr = healthy_standbys[index].address();

            let mut current = session.current_node.write().await;
            *current = Some(node_addr.clone());
            return Ok(node_addr);
        }

        // Fall back to primary if no healthy standbys
        Self::select_node(session, state, config).await
    }

    /// Complete backend authentication by reading until ReadyForQuery
    /// This is used when switching backends - we don't forward auth to client
    async fn complete_backend_auth(backend: &mut TcpStream) -> Result<()> {
        let codec = ProtocolCodec::new();
        let mut buffer = BytesMut::with_capacity(4096);
        let timeout = Duration::from_secs(10);
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                return Err(ProxyError::Auth("Backend authentication timeout".to_string()));
            }

            let mut read_buf = vec![0u8; 4096];
            let n = tokio::time::timeout(Duration::from_secs(5), backend.read(&mut read_buf))
                .await
                .map_err(|_| ProxyError::Auth("Read timeout during backend auth".to_string()))?
                .map_err(|e| ProxyError::Network(format!("Backend auth read error: {}", e)))?;

            if n == 0 {
                return Err(ProxyError::Connection("Backend closed during auth".to_string()));
            }

            buffer.extend_from_slice(&read_buf[..n]);

            // Check for complete messages
            loop {
                if buffer.len() < 5 {
                    break;
                }

                // Parse message
                let mut temp_buffer = buffer.clone();
                match codec.decode_message(&mut temp_buffer)? {
                    Some(msg) => {
                        match msg.msg_type {
                            MessageType::ReadyForQuery => {
                                // Authentication complete
                                return Ok(());
                            }
                            MessageType::ErrorResponse => {
                                let err = ErrorResponse::parse(msg.payload)
                                    .map(|e| e.message().unwrap_or("Unknown error").to_string())
                                    .unwrap_or_else(|_| "Parse error".to_string());
                                return Err(ProxyError::Auth(err));
                            }
                            _ => {
                                // Continue reading (AuthRequest, ParameterStatus, BackendKeyData, etc.)
                            }
                        }
                        // Consume the message from actual buffer
                        let _ = codec.decode_message(&mut buffer)?;
                    }
                    None => {
                        // Need more data
                        break;
                    }
                }
            }
        }
    }

    /// Create PostgreSQL error response message
    fn create_error_response(code: &str, message: &str) -> Vec<u8> {
        let mut fields = HashMap::new();
        fields.insert('S', "ERROR".to_string());
        fields.insert('V', "ERROR".to_string());
        fields.insert('C', code.to_string());
        fields.insert('M', message.to_string());

        let err = ErrorResponse { fields };
        err.encode().encode().to_vec()
    }

    /// Select a backend node for the request
    /// Select a backend node for initial connection
    /// Prefers primary but falls back to standbys for read connections
    async fn select_node(
        session: &Arc<ClientSession>,
        state: &Arc<ServerState>,
        config: &ProxyConfig,
    ) -> Result<String> {
        // If in a transaction, stick to the current node
        {
            let tx_state = session.tx_state.read().await;
            if tx_state.in_transaction {
                if let Some(node) = session.current_node.read().await.clone() {
                    return Ok(node);
                }
            }
        }

        // Get healthy nodes
        let health = state.health.read().await;
        let healthy_nodes: Vec<&NodeConfig> = config
            .nodes
            .iter()
            .filter(|n| {
                n.enabled
                    && health
                        .get(&n.address())
                        .map(|h| h.healthy)
                        .unwrap_or(false)
            })
            .collect();

        if healthy_nodes.is_empty() {
            return Err(ProxyError::NoHealthyNodes);
        }

        // Try to find healthy primary first
        if let Some(primary) = healthy_nodes.iter().find(|n| n.role == NodeRole::Primary) {
            let node_addr = primary.address();
            let mut current = session.current_node.write().await;
            *current = Some(node_addr.clone());
            return Ok(node_addr);
        }

        // Fall back to standby if primary is unavailable
        // (Initial connection will work, writes will use write timeout to wait for primary)
        if let Some(standby) = healthy_nodes.iter().find(|n| n.role == NodeRole::Standby) {
            tracing::warn!("Primary unavailable, connecting to standby for initial session");
            let node_addr = standby.address();
            let mut current = session.current_node.write().await;
            *current = Some(node_addr.clone());
            return Ok(node_addr);
        }

        // No nodes available
        Err(ProxyError::NoHealthyNodes)
    }

    /// Get a connection from the pool
    async fn get_connection(
        node_addr: &str,
        state: &Arc<ServerState>,
        config: &ProxyConfig,
    ) -> Result<BackendConnection> {
        let pools = state.pools.read().await;
        let pool = pools
            .get(node_addr)
            .ok_or_else(|| ProxyError::Pool(format!("No pool for node: {}", node_addr)))?;

        // Try to get existing connection
        {
            let mut conns = pool.connections.write().await;
            if let Some(conn) = conns.pop() {
                if conn.healthy {
                    pool.active_count.fetch_add(1, Ordering::Relaxed);
                    return Ok(conn);
                }
            }
        }

        // Acquire permit for new connection
        let _permit = pool
            .semaphore
            .acquire()
            .await
            .map_err(|_| ProxyError::Pool("Failed to acquire connection permit".to_string()))?;

        // Create new connection
        let stream = tokio::time::timeout(
            config.pool.acquire_timeout(),
            TcpStream::connect(node_addr),
        )
        .await
        .map_err(|_| ProxyError::Connection(format!("Connection timeout to {}", node_addr)))?
        .map_err(|e| ProxyError::Connection(format!("Failed to connect to {}: {}", node_addr, e)))?;

        let conn = BackendConnection {
            id: Uuid::new_v4(),
            stream: Some(stream),
            created_at: chrono::Utc::now(),
            last_used: chrono::Utc::now(),
            healthy: true,
        };

        pool.active_count.fetch_add(1, Ordering::Relaxed);
        Ok(conn)
    }

    /// Spawn health checker background task
    fn spawn_health_checker(&self) -> tokio::task::JoinHandle<()> {
        let state = self.state.clone();
        let config = self.config.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(config.health.check_interval_secs));

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        Self::check_all_nodes(&state, &config).await;
                    }
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                }
            }
        })
    }

    /// Check health of all nodes
    async fn check_all_nodes(state: &Arc<ServerState>, config: &ProxyConfig) {
        for node in &config.nodes {
            let result = Self::check_node_health(node, config).await;
            let mut health = state.health.write().await;

            if let Some(node_health) = health.get_mut(&node.address()) {
                match result {
                    Ok(latency) => {
                        node_health.healthy = true;
                        node_health.failure_count = 0;
                        node_health.latency_ms = latency;
                        node_health.last_error = None;
                    }
                    Err(e) => {
                        node_health.failure_count += 1;
                        node_health.last_error = Some(e.to_string());

                        if node_health.failure_count >= config.health.failure_threshold {
                            node_health.healthy = false;
                            tracing::warn!(
                                "Node {} marked unhealthy after {} failures",
                                node.address(),
                                node_health.failure_count
                            );
                        }
                    }
                }
                node_health.last_check = chrono::Utc::now();
            }
        }
    }

    /// Check health of a single node
    async fn check_node_health(node: &NodeConfig, config: &ProxyConfig) -> Result<f64> {
        let start = std::time::Instant::now();

        let timeout = std::time::Duration::from_secs(config.health.check_timeout_secs);
        let _stream = tokio::time::timeout(timeout, TcpStream::connect(node.address()))
            .await
            .map_err(|_| ProxyError::HealthCheck(format!("Timeout connecting to {}", node.address())))?
            .map_err(|e| {
                ProxyError::HealthCheck(format!("Failed to connect to {}: {}", node.address(), e))
            })?;

        // In a real implementation, we would execute the health check query here
        let latency = start.elapsed().as_secs_f64() * 1000.0;
        Ok(latency)
    }

    /// Spawn pool manager background task
    fn spawn_pool_manager(&self) -> tokio::task::JoinHandle<()> {
        let state = self.state.clone();
        let config = self.config.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        Self::cleanup_pools(&state, &config).await;
                    }
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                }
            }
        })
    }

    /// Cleanup idle connections from pools
    async fn cleanup_pools(state: &Arc<ServerState>, config: &ProxyConfig) {
        let pools = state.pools.read().await;
        let now = chrono::Utc::now();
        let idle_timeout = chrono::Duration::seconds(config.pool.idle_timeout_secs as i64);

        for pool in pools.values() {
            let mut conns = pool.connections.write().await;
            conns.retain(|conn| now - conn.last_used < idle_timeout);
        }
    }

    /// Shutdown the server
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    /// Get server metrics
    pub fn metrics(&self) -> ServerMetricsSnapshot {
        ServerMetricsSnapshot {
            connections_accepted: self.state.metrics.connections_accepted.load(Ordering::Relaxed),
            connections_closed: self.state.metrics.connections_closed.load(Ordering::Relaxed),
            queries_processed: self.state.metrics.queries_processed.load(Ordering::Relaxed),
            bytes_received: self.state.metrics.bytes_received.load(Ordering::Relaxed),
            bytes_sent: self.state.metrics.bytes_sent.load(Ordering::Relaxed),
            failovers: self.state.metrics.failovers.load(Ordering::Relaxed),
        }
    }
}

/// Metrics snapshot for external consumption
#[derive(Debug, Clone)]
pub struct ServerMetricsSnapshot {
    pub connections_accepted: u64,
    pub connections_closed: u64,
    pub queries_processed: u64,
    pub bytes_received: u64,
    pub bytes_sent: u64,
    pub failovers: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HealthConfig, LoadBalancerConfig, PoolConfig};

    fn test_config() -> ProxyConfig {
        let mut config = ProxyConfig::default();
        config.listen_address = "127.0.0.1:0".to_string();
        config
            .add_node("127.0.0.1:5432", "primary")
            .unwrap();
        config
    }

    #[test]
    fn test_server_creation() {
        let config = test_config();
        let server = ProxyServer::new(config);
        assert!(server.is_ok());
    }

    #[test]
    fn test_initial_metrics() {
        let config = test_config();
        let server = ProxyServer::new(config).unwrap();
        let metrics = server.metrics();
        assert_eq!(metrics.connections_accepted, 0);
        assert_eq!(metrics.queries_processed, 0);
    }

    #[tokio::test]
    async fn test_session_creation() {
        let config = test_config();
        let server = ProxyServer::new(config).unwrap();

        let sessions = server.state.sessions.read().await;
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn test_node_health_initialization() {
        let config = test_config();
        let server = ProxyServer::new(config).unwrap();

        let health = server.state.health.read().await;
        assert!(!health.is_empty());

        for node_health in health.values() {
            assert!(node_health.healthy);
            assert_eq!(node_health.failure_count, 0);
        }
    }
}
