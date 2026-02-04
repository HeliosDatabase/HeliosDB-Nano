//! Admin API
//!
//! REST API for proxy management, monitoring, and configuration.
//! Includes HTTP SQL API for transparent write routing (TWR) and load balancing.

use crate::config::{NodeConfig, NodeRole, ProxyConfig};
use crate::server::{NodeHealth, ServerMetricsSnapshot};
use crate::{ProxyError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, RwLock};

/// Admin API server
pub struct AdminServer {
    /// Listen address
    listen_address: String,
    /// Shared state with proxy
    state: Arc<AdminState>,
    /// Shutdown channel
    shutdown_tx: broadcast::Sender<()>,
}

/// Shared admin state
pub struct AdminState {
    /// Node health status
    pub node_health: RwLock<HashMap<String, NodeHealth>>,
    /// Server metrics
    pub metrics: RwLock<ServerMetricsSnapshot>,
    /// Active sessions count
    pub active_sessions: RwLock<u64>,
    /// Configuration (read-only)
    pub config_snapshot: RwLock<ConfigSnapshot>,
    /// Full proxy config (for SQL routing)
    pub proxy_config: RwLock<Option<ProxyConfig>>,
    /// Round-robin counter for read load balancing
    read_lb_counter: AtomicUsize,
    /// Registered command handlers
    commands: RwLock<HashMap<String, CommandHandler>>,
}


/// Command handler type
type CommandHandler = Arc<dyn Fn(&[&str]) -> Result<String> + Send + Sync>;

/// Configuration snapshot for admin API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub listen_address: String,
    pub admin_address: String,
    pub tr_enabled: bool,
    pub tr_mode: String,
    pub pool_min_connections: usize,
    pub pool_max_connections: usize,
    pub nodes: Vec<NodeSnapshot>,
}

/// Node configuration snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSnapshot {
    pub address: String,
    pub role: String,
    pub weight: u32,
    pub enabled: bool,
}

impl AdminServer {
    /// Create a new admin server
    pub fn new(listen_address: String, state: Arc<AdminState>) -> Self {
        let (shutdown_tx, _) = broadcast::channel(1);

        Self {
            listen_address,
            state,
            shutdown_tx,
        }
    }

    /// Run the admin server
    pub async fn run(&self) -> Result<()> {
        let listener = TcpListener::bind(&self.listen_address)
            .await
            .map_err(|e| ProxyError::Network(format!("Failed to bind admin: {}", e)))?;

        tracing::info!("Admin API listening on {}", self.listen_address);

        let mut shutdown_rx = self.shutdown_tx.subscribe();

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, addr)) => {
                            let state = self.state.clone();
                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_connection(stream, addr, state).await {
                                    tracing::error!("Admin connection error: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Admin accept error: {}", e);
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::info!("Admin server shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Handle an admin connection
    async fn handle_connection(
        mut stream: TcpStream,
        addr: SocketAddr,
        state: Arc<AdminState>,
    ) -> Result<()> {
        tracing::debug!("Admin connection from {}", addr);

        let (reader, mut writer) = stream.split();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();

        // Read HTTP request headers
        let mut headers = Vec::new();
        let mut content_length: usize = 0;

        loop {
            line.clear();
            let bytes_read = reader
                .read_line(&mut line)
                .await
                .map_err(|e| ProxyError::Network(format!("Read error: {}", e)))?;

            if bytes_read == 0 || line == "\r\n" {
                break;
            }

            // Parse Content-Length header
            let trimmed = line.trim();
            if trimmed.to_lowercase().starts_with("content-length:") {
                if let Some(len_str) = trimmed.split(':').nth(1) {
                    content_length = len_str.trim().parse().unwrap_or(0);
                }
            }
            headers.push(trimmed.to_string());
        }

        if headers.is_empty() {
            return Ok(());
        }

        // Parse request line
        let request_line = &headers[0];
        let parts: Vec<&str> = request_line.split_whitespace().collect();

        if parts.len() < 2 {
            Self::send_response(&mut writer, 400, "Bad Request", "Invalid request line").await?;
            return Ok(());
        }

        let method = parts[0];
        let path = parts[1];

        // Read request body for POST/PUT requests
        let body = if content_length > 0 && (method == "POST" || method == "PUT") {
            let mut body_buf = vec![0u8; content_length];
            reader.read_exact(&mut body_buf).await
                .map_err(|e| ProxyError::Network(format!("Body read error: {}", e)))?;
            Some(String::from_utf8_lossy(&body_buf).to_string())
        } else {
            None
        };

        // Route request
        let response = Self::route_request(method, path, body.as_deref(), &state).await;

        match response {
            Ok((status, body)) => {
                Self::send_json_response(&mut writer, status, &body).await?;
            }
            Err(e) => {
                let error = ErrorResponse {
                    error: e.to_string(),
                };
                Self::send_json_response(&mut writer, 500, &error).await?;
            }
        }

        Ok(())
    }

    /// Route a request to the appropriate handler
    async fn route_request(
        method: &str,
        path: &str,
        body: Option<&str>,
        state: &Arc<AdminState>,
    ) -> Result<(u16, serde_json::Value)> {
        match (method, path) {
            // SQL API - Execute SQL with TWR (Transparent Write Routing)
            ("POST", "/api/sql") => {
                Self::handle_sql_request(body, state).await
            }

            // Health endpoints
            ("GET", "/health") => {
                let health = HealthResponse { status: "ok" };
                Ok((200, serde_json::to_value(health)?))
            }
            ("GET", "/health/ready") => {
                let ready = Self::check_readiness(state).await;
                let response = ReadinessResponse {
                    ready,
                    message: if ready {
                        "Proxy is ready"
                    } else {
                        "Proxy is not ready"
                    },
                };
                let status = if ready { 200 } else { 503 };
                Ok((status, serde_json::to_value(response)?))
            }
            ("GET", "/health/live") => {
                let response = LivenessResponse { alive: true };
                Ok((200, serde_json::to_value(response)?))
            }

            // Metrics
            ("GET", "/metrics") => {
                let metrics = state.metrics.read().await.clone();
                Ok((200, serde_json::to_value(MetricsResponse::from(metrics))?))
            }
            ("GET", "/metrics/prometheus") => {
                let metrics = state.metrics.read().await.clone();
                let prometheus = Self::format_prometheus_metrics(&metrics);
                Ok((200, serde_json::json!({ "text": prometheus })))
            }

            // Node management
            ("GET", "/nodes") => {
                let health = state.node_health.read().await;
                let nodes: Vec<NodeHealthResponse> = health
                    .values()
                    .map(|h| NodeHealthResponse::from(h.clone()))
                    .collect();
                Ok((200, serde_json::to_value(nodes)?))
            }
            ("GET", path) if path.starts_with("/nodes/") => {
                let node_addr = path.trim_start_matches("/nodes/");
                let health = state.node_health.read().await;
                match health.get(node_addr) {
                    Some(h) => Ok((200, serde_json::to_value(NodeHealthResponse::from(h.clone()))?)),
                    None => Ok((404, serde_json::json!({ "error": "Node not found" }))),
                }
            }
            ("POST", path) if path.starts_with("/nodes/") && path.ends_with("/enable") => {
                let node_addr = path
                    .trim_start_matches("/nodes/")
                    .trim_end_matches("/enable");
                Self::set_node_enabled(state, node_addr, true).await?;
                Ok((200, serde_json::json!({ "status": "enabled" })))
            }
            ("POST", path) if path.starts_with("/nodes/") && path.ends_with("/disable") => {
                let node_addr = path
                    .trim_start_matches("/nodes/")
                    .trim_end_matches("/disable");
                Self::set_node_enabled(state, node_addr, false).await?;
                Ok((200, serde_json::json!({ "status": "disabled" })))
            }

            // Configuration
            ("GET", "/config") => {
                let config = state.config_snapshot.read().await.clone();
                Ok((200, serde_json::to_value(config)?))
            }

            // Sessions
            ("GET", "/sessions") => {
                let count = *state.active_sessions.read().await;
                let response = SessionsResponse {
                    active_sessions: count,
                };
                Ok((200, serde_json::to_value(response)?))
            }

            // Pools
            ("GET", "/pools") => {
                let pools = Self::get_pool_stats(state).await;
                Ok((200, serde_json::to_value(pools)?))
            }

            // Version
            ("GET", "/version") => {
                let response = VersionResponse {
                    version: crate::VERSION.to_string(),
                    build_time: env!("CARGO_PKG_VERSION").to_string(),
                };
                Ok((200, serde_json::to_value(response)?))
            }

            // Not found
            _ => Ok((404, serde_json::json!({ "error": "Not found" }))),
        }
    }

    /// Handle SQL execution request with TWR (Transparent Write Routing)
    async fn handle_sql_request(
        body: Option<&str>,
        state: &Arc<AdminState>,
    ) -> Result<(u16, serde_json::Value)> {
        // Parse request body
        let body = body.ok_or_else(|| ProxyError::Internal("Missing request body".to_string()))?;
        let request: SqlRequest = serde_json::from_str(body)
            .map_err(|e| ProxyError::Internal(format!("Invalid JSON: {}", e)))?;

        let sql = request.query.trim();
        if sql.is_empty() {
            return Ok((400, serde_json::json!({ "error": "Empty query" })));
        }

        // Classify query as read or write
        let is_write = Self::is_write_query(sql);
        let query_type = if is_write { "write" } else { "read" };

        // Get proxy config
        let proxy_config = state.proxy_config.read().await;
        let config = proxy_config.as_ref()
            .ok_or_else(|| ProxyError::Internal("Proxy config not initialized".to_string()))?;

        // Get node health
        let health = state.node_health.read().await;

        // Select target node based on query type
        let target_node = if is_write {
            // Write queries always go to primary
            Self::select_primary_node(config, &health)?
        } else {
            // Read queries can go to any healthy node with load balancing
            Self::select_read_node(config, &health, state)?
        };

        let target_address = format!("{}:{}", target_node.host, target_node.port);
        // Use HTTP port from node config (defaults to 8080)
        let http_port = target_node.http_port;
        let http_url = format!("http://{}:{}/api/sql", target_node.host, http_port);

        tracing::debug!(
            "Routing {} query to {} ({})",
            query_type,
            target_address,
            match target_node.role {
                NodeRole::Primary => "primary",
                NodeRole::Standby => "standby",
                NodeRole::ReadReplica => "replica",
            }
        );

        // Forward request to backend node
        let result = Self::forward_sql_request(&http_url, sql).await?;

        // Return result with routing metadata
        let response = SqlResponse {
            query_type: query_type.to_string(),
            routed_to: target_address,
            node_role: format!("{:?}", target_node.role).to_lowercase(),
            result,
        };

        Ok((200, serde_json::to_value(response)?))
    }

    /// Determine if a query is a write operation
    fn is_write_query(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();

        // Write operations
        if upper.starts_with("INSERT")
            || upper.starts_with("UPDATE")
            || upper.starts_with("DELETE")
            || upper.starts_with("CREATE")
            || upper.starts_with("ALTER")
            || upper.starts_with("DROP")
            || upper.starts_with("TRUNCATE")
            || upper.starts_with("GRANT")
            || upper.starts_with("REVOKE")
            || upper.starts_with("VACUUM")
            || upper.starts_with("REINDEX")
            || upper.starts_with("MERGE")
            || upper.starts_with("UPSERT")
        {
            return true;
        }

        // Transaction control that might contain writes
        if upper.starts_with("BEGIN")
            || upper.starts_with("COMMIT")
            || upper.starts_with("ROLLBACK")
            || upper.starts_with("SAVEPOINT")
        {
            // Transaction control goes to primary for safety
            return true;
        }

        // Read operations
        false
    }

    /// Select primary node for write queries
    fn select_primary_node<'a>(
        config: &'a ProxyConfig,
        health: &HashMap<String, NodeHealth>,
    ) -> Result<&'a NodeConfig> {
        config.nodes.iter()
            .find(|n| {
                n.role == NodeRole::Primary
                    && n.enabled
                    && health.get(&n.address()).map(|h| h.healthy).unwrap_or(false)
            })
            .ok_or_else(|| ProxyError::Internal("No healthy primary node available".to_string()))
    }

    /// Select node for read queries with load balancing
    fn select_read_node<'a>(
        config: &'a ProxyConfig,
        health: &HashMap<String, NodeHealth>,
        state: &AdminState,
    ) -> Result<&'a NodeConfig> {
        // Get all healthy nodes (primary, standby, or replica)
        let healthy_nodes: Vec<&NodeConfig> = config.nodes.iter()
            .filter(|n| n.enabled && health.get(&n.address()).map(|h| h.healthy).unwrap_or(false))
            .collect();

        if healthy_nodes.is_empty() {
            return Err(ProxyError::Internal("No healthy nodes available".to_string()));
        }

        // If read/write splitting is enabled and there are standbys, prefer them
        if config.load_balancer.read_write_split {
            let read_nodes: Vec<&NodeConfig> = healthy_nodes.iter()
                .filter(|n| n.role == NodeRole::Standby || n.role == NodeRole::ReadReplica)
                .copied()
                .collect();

            if !read_nodes.is_empty() {
                // Round-robin across read nodes
                let counter = state.read_lb_counter.fetch_add(1, Ordering::Relaxed);
                let index = counter % read_nodes.len();
                return Ok(read_nodes[index]);
            }
        }

        // Fall back to round-robin across all healthy nodes
        let counter = state.read_lb_counter.fetch_add(1, Ordering::Relaxed);
        let index = counter % healthy_nodes.len();
        Ok(healthy_nodes[index])
    }

    /// Forward SQL request to backend node's HTTP API
    async fn forward_sql_request(url: &str, sql: &str) -> Result<serde_json::Value> {
        // Build HTTP request
        let request_body = serde_json::json!({ "query": sql });
        let body_bytes = serde_json::to_vec(&request_body)
            .map_err(|e| ProxyError::Internal(format!("JSON serialization error: {}", e)))?;

        // Parse URL
        let url_parts: Vec<&str> = url.trim_start_matches("http://").splitn(2, '/').collect();
        if url_parts.is_empty() {
            return Err(ProxyError::Internal("Invalid URL".to_string()));
        }

        let host_port = url_parts[0];
        let path = if url_parts.len() > 1 {
            format!("/{}", url_parts[1])
        } else {
            "/".to_string()
        };

        // Connect to backend
        let stream = TcpStream::connect(host_port).await
            .map_err(|e| ProxyError::Network(format!("Failed to connect to {}: {}", host_port, e)))?;

        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Send HTTP request
        let request = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            path,
            host_port,
            body_bytes.len()
        );

        writer.write_all(request.as_bytes()).await
            .map_err(|e| ProxyError::Network(format!("Write error: {}", e)))?;
        writer.write_all(&body_bytes).await
            .map_err(|e| ProxyError::Network(format!("Write body error: {}", e)))?;

        // Read response headers
        let mut response_headers = Vec::new();
        let mut line = String::new();
        let mut content_length: usize = 0;

        loop {
            line.clear();
            let bytes_read = reader.read_line(&mut line).await
                .map_err(|e| ProxyError::Network(format!("Response read error: {}", e)))?;

            if bytes_read == 0 || line == "\r\n" {
                break;
            }

            let trimmed = line.trim();
            if trimmed.to_lowercase().starts_with("content-length:") {
                if let Some(len_str) = trimmed.split(':').nth(1) {
                    content_length = len_str.trim().parse().unwrap_or(0);
                }
            }
            response_headers.push(trimmed.to_string());
        }

        // Read response body
        let mut body_buf = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body_buf).await
                .map_err(|e| ProxyError::Network(format!("Response body read error: {}", e)))?;
        }

        let response_body = String::from_utf8_lossy(&body_buf);

        // Parse JSON response
        serde_json::from_str(&response_body)
            .map_err(|e| ProxyError::Internal(format!("Invalid JSON response: {} - body: {}", e, response_body)))
    }

    /// Check if proxy is ready to accept connections
    async fn check_readiness(state: &Arc<AdminState>) -> bool {
        let health = state.node_health.read().await;

        // Need at least one healthy primary
        health.values().any(|h| h.healthy)
    }

    /// Set node enabled status
    async fn set_node_enabled(state: &Arc<AdminState>, node_addr: &str, enabled: bool) -> Result<()> {
        let mut health = state.node_health.write().await;

        if let Some(node_health) = health.get_mut(node_addr) {
            node_health.healthy = enabled;
            Ok(())
        } else {
            Err(ProxyError::Config(format!("Node not found: {}", node_addr)))
        }
    }

    /// Get pool statistics
    async fn get_pool_stats(_state: &Arc<AdminState>) -> Vec<PoolStatsResponse> {
        // Placeholder - in real implementation would query pool state
        Vec::new()
    }

    /// Format metrics as Prometheus text format
    fn format_prometheus_metrics(metrics: &ServerMetricsSnapshot) -> String {
        let mut output = String::new();

        output.push_str("# HELP heliosdb_proxy_connections_total Total connections accepted\n");
        output.push_str("# TYPE heliosdb_proxy_connections_total counter\n");
        output.push_str(&format!(
            "heliosdb_proxy_connections_total {}\n",
            metrics.connections_accepted
        ));

        output.push_str("# HELP heliosdb_proxy_connections_closed Total connections closed\n");
        output.push_str("# TYPE heliosdb_proxy_connections_closed counter\n");
        output.push_str(&format!(
            "heliosdb_proxy_connections_closed {}\n",
            metrics.connections_closed
        ));

        output.push_str("# HELP heliosdb_proxy_queries_total Total queries processed\n");
        output.push_str("# TYPE heliosdb_proxy_queries_total counter\n");
        output.push_str(&format!(
            "heliosdb_proxy_queries_total {}\n",
            metrics.queries_processed
        ));

        output.push_str("# HELP heliosdb_proxy_bytes_received_total Total bytes received\n");
        output.push_str("# TYPE heliosdb_proxy_bytes_received_total counter\n");
        output.push_str(&format!(
            "heliosdb_proxy_bytes_received_total {}\n",
            metrics.bytes_received
        ));

        output.push_str("# HELP heliosdb_proxy_bytes_sent_total Total bytes sent\n");
        output.push_str("# TYPE heliosdb_proxy_bytes_sent_total counter\n");
        output.push_str(&format!(
            "heliosdb_proxy_bytes_sent_total {}\n",
            metrics.bytes_sent
        ));

        output.push_str("# HELP heliosdb_proxy_failovers_total Total failovers\n");
        output.push_str("# TYPE heliosdb_proxy_failovers_total counter\n");
        output.push_str(&format!(
            "heliosdb_proxy_failovers_total {}\n",
            metrics.failovers
        ));

        output
    }

    /// Send HTTP response
    async fn send_response(
        writer: &mut tokio::net::tcp::WriteHalf<'_>,
        status: u16,
        status_text: &str,
        body: &str,
    ) -> Result<()> {
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            status_text,
            body.len(),
            body
        );

        writer
            .write_all(response.as_bytes())
            .await
            .map_err(|e| ProxyError::Network(format!("Write error: {}", e)))?;

        Ok(())
    }

    /// Send JSON HTTP response
    async fn send_json_response<T: Serialize>(
        writer: &mut tokio::net::tcp::WriteHalf<'_>,
        status: u16,
        body: &T,
    ) -> Result<()> {
        let json = serde_json::to_string(body)
            .map_err(|e| ProxyError::Internal(format!("JSON error: {}", e)))?;

        let status_text = match status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Unknown",
        };

        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            status_text,
            json.len(),
            json
        );

        writer
            .write_all(response.as_bytes())
            .await
            .map_err(|e| ProxyError::Network(format!("Write error: {}", e)))?;

        Ok(())
    }

    /// Shutdown the admin server
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }
}

impl AdminState {
    /// Create new admin state
    pub fn new() -> Self {
        Self {
            node_health: RwLock::new(HashMap::new()),
            metrics: RwLock::new(ServerMetricsSnapshot {
                connections_accepted: 0,
                connections_closed: 0,
                queries_processed: 0,
                bytes_received: 0,
                bytes_sent: 0,
                failovers: 0,
            }),
            active_sessions: RwLock::new(0),
            config_snapshot: RwLock::new(ConfigSnapshot {
                listen_address: String::new(),
                admin_address: String::new(),
                tr_enabled: false,
                tr_mode: String::new(),
                pool_min_connections: 0,
                pool_max_connections: 0,
                nodes: Vec::new(),
            }),
            proxy_config: RwLock::new(None),
            read_lb_counter: AtomicUsize::new(0),
            commands: RwLock::new(HashMap::new()),
        }
    }

    /// Set the proxy configuration for SQL routing
    pub async fn set_proxy_config(&self, config: ProxyConfig) {
        let mut proxy_config = self.proxy_config.write().await;
        *proxy_config = Some(config);
    }

    /// Register a command handler
    pub async fn register_command<F>(&self, name: &str, handler: F)
    where
        F: Fn(&[&str]) -> Result<String> + Send + Sync + 'static,
    {
        let mut commands = self.commands.write().await;
        commands.insert(name.to_string(), Arc::new(handler));
    }

    /// Execute a command
    pub async fn execute_command(&self, name: &str, args: &[&str]) -> Result<String> {
        let commands = self.commands.read().await;
        match commands.get(name) {
            Some(handler) => handler(args),
            None => Err(ProxyError::Internal(format!("Unknown command: {}", name))),
        }
    }
}

impl Default for AdminState {
    fn default() -> Self {
        Self::new()
    }
}

// Request and Response types

/// SQL execution request
#[derive(Debug, Deserialize)]
struct SqlRequest {
    /// SQL query to execute
    query: String,
    /// Optional parameters (for prepared statements - future use)
    #[serde(default)]
    params: Vec<serde_json::Value>,
}

/// SQL execution response
#[derive(Debug, Serialize)]
struct SqlResponse {
    /// Query type (read/write)
    query_type: String,
    /// Node the query was routed to
    routed_to: String,
    /// Role of the target node
    node_role: String,
    /// Query result from backend
    result: serde_json::Value,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct ReadinessResponse {
    ready: bool,
    message: &'static str,
}

#[derive(Serialize)]
struct LivenessResponse {
    alive: bool,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Serialize)]
struct MetricsResponse {
    connections_accepted: u64,
    connections_closed: u64,
    connections_active: u64,
    queries_processed: u64,
    bytes_received: u64,
    bytes_sent: u64,
    failovers: u64,
}

impl From<ServerMetricsSnapshot> for MetricsResponse {
    fn from(m: ServerMetricsSnapshot) -> Self {
        Self {
            connections_accepted: m.connections_accepted,
            connections_closed: m.connections_closed,
            connections_active: m.connections_accepted.saturating_sub(m.connections_closed),
            queries_processed: m.queries_processed,
            bytes_received: m.bytes_received,
            bytes_sent: m.bytes_sent,
            failovers: m.failovers,
        }
    }
}

#[derive(Serialize)]
struct NodeHealthResponse {
    address: String,
    healthy: bool,
    last_check: String,
    failure_count: u32,
    last_error: Option<String>,
    latency_ms: f64,
    replication_lag_bytes: Option<u64>,
}

impl From<NodeHealth> for NodeHealthResponse {
    fn from(h: NodeHealth) -> Self {
        Self {
            address: h.address,
            healthy: h.healthy,
            last_check: h.last_check.to_rfc3339(),
            failure_count: h.failure_count,
            last_error: h.last_error,
            latency_ms: h.latency_ms,
            replication_lag_bytes: h.replication_lag_bytes,
        }
    }
}

#[derive(Serialize)]
struct SessionsResponse {
    active_sessions: u64,
}

#[derive(Serialize)]
struct PoolStatsResponse {
    node: String,
    active_connections: u64,
    idle_connections: u64,
    pending_requests: u64,
    total_connections_created: u64,
    total_connections_closed: u64,
}

#[derive(Serialize)]
struct VersionResponse {
    version: String,
    build_time: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_admin_state_creation() {
        let state = AdminState::new();
        let sessions = state.active_sessions.read().await;
        assert_eq!(*sessions, 0);
    }

    #[tokio::test]
    async fn test_readiness_check_no_nodes() {
        let state = Arc::new(AdminState::new());
        let ready = AdminServer::check_readiness(&state).await;
        assert!(!ready);
    }

    #[tokio::test]
    async fn test_readiness_check_with_healthy_node() {
        let state = Arc::new(AdminState::new());

        {
            let mut health = state.node_health.write().await;
            health.insert(
                "localhost:5432".to_string(),
                NodeHealth {
                    address: "localhost:5432".to_string(),
                    healthy: true,
                    last_check: chrono::Utc::now(),
                    failure_count: 0,
                    last_error: None,
                    latency_ms: 1.0,
                    replication_lag_bytes: None,
                },
            );
        }

        let ready = AdminServer::check_readiness(&state).await;
        assert!(ready);
    }

    #[tokio::test]
    async fn test_command_registration() {
        let state = AdminState::new();

        state
            .register_command("test", |args| {
                Ok(format!("Test command with {} args", args.len()))
            })
            .await;

        let result = state.execute_command("test", &["arg1", "arg2"]).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Test command with 2 args");
    }

    #[tokio::test]
    async fn test_unknown_command() {
        let state = AdminState::new();
        let result = state.execute_command("unknown", &[]).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_prometheus_metrics_format() {
        let metrics = ServerMetricsSnapshot {
            connections_accepted: 100,
            connections_closed: 50,
            queries_processed: 1000,
            bytes_received: 50000,
            bytes_sent: 100000,
            failovers: 2,
        };

        let output = AdminServer::format_prometheus_metrics(&metrics);
        assert!(output.contains("heliosdb_proxy_connections_total 100"));
        assert!(output.contains("heliosdb_proxy_queries_total 1000"));
        assert!(output.contains("heliosdb_proxy_failovers_total 2"));
    }

    #[test]
    fn test_metrics_response_active_connections() {
        let snapshot = ServerMetricsSnapshot {
            connections_accepted: 100,
            connections_closed: 30,
            queries_processed: 500,
            bytes_received: 10000,
            bytes_sent: 20000,
            failovers: 1,
        };

        let response = MetricsResponse::from(snapshot);
        assert_eq!(response.connections_active, 70);
    }
}
