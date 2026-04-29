//! HTTP/REST API Server for HeliosDB-Lite v2.3.0 Sync Protocol
//!
//! This module provides a production-ready HTTP server with REST API endpoints
//! for client-server synchronization over HTTP. Built on Axum for high performance
//! and async/await for non-blocking I/O.
//!
//! # Endpoints
//!
//! - POST `/api/v1/sync/register` - Register client for synchronization
//! - POST `/api/v1/sync/pull` - Pull changes from server
//! - POST `/api/v1/sync/push` - Push changes to server
//! - POST `/api/v1/sync/heartbeat` - Client heartbeat
//! - GET `/api/v1/sync/health` - Server health check
//!
//! # Features
//!
//! - CORS support for web clients
//! - gzip compression for large responses
//! - Request logging with tracing
//! - Proper HTTP status codes
//! - JSON serialization/deserialization
//! - JWT authentication support
//! - Error handling with meaningful responses
//!
//! # Example
//!
//! ```rust,ignore
//! use heliosdb_nano::sync::http_server::HttpSyncServer;
//! use std::net::SocketAddr;
//!
//! let server = HttpSyncServer::new(protocol, "0.0.0.0:8080".parse()?);
//! server.serve().await?;
//! ```

use super::{
    auth::{Authorizer, Claims, JwtManager},
    protocol::{SyncMessage, SyncProtocol, PROTOCOL_VERSION},
    SyncError,
};
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use super::Result as SyncResult;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc, time::Instant};
use tower::ServiceBuilder;
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

/// HTTP Sync Server state shared across handlers
#[derive(Clone)]
struct ServerState {
    protocol: Arc<SyncProtocol>,
    jwt_manager: Arc<JwtManager>,
    authorizer: Arc<Authorizer>,
    start_time: Instant,
}

/// HTTP Sync Server for HeliosDB-Lite
pub struct HttpSyncServer {
    state: ServerState,
    bind_addr: SocketAddr,
}

impl HttpSyncServer {
    /// Create a new HTTP sync server
    ///
    /// # Arguments
    ///
    /// * `protocol` - Sync protocol instance for message handling
    /// * `bind_addr` - Socket address to bind to (e.g., "0.0.0.0:8080")
    ///
    /// # Returns
    ///
    /// New `HttpSyncServer` instance
    pub fn new(protocol: Arc<SyncProtocol>, bind_addr: SocketAddr) -> Self {
        Self {
            state: ServerState {
                protocol,
                jwt_manager: Arc::new(JwtManager::from_env_or_default()),
                authorizer: Arc::new(Authorizer::new()),
                start_time: Instant::now(),
            },
            bind_addr,
        }
    }

    /// Create a new HTTP sync server with custom authentication
    ///
    /// # Arguments
    ///
    /// * `protocol` - Sync protocol instance
    /// * `bind_addr` - Socket address to bind to
    /// * `jwt_manager` - JWT manager for authentication
    /// * `authorizer` - Authorizer for tenant validation
    ///
    /// # Returns
    ///
    /// New `HttpSyncServer` instance with custom auth
    pub fn with_auth(
        protocol: Arc<SyncProtocol>,
        bind_addr: SocketAddr,
        jwt_manager: JwtManager,
        authorizer: Authorizer,
    ) -> Self {
        Self {
            state: ServerState {
                protocol,
                jwt_manager: Arc::new(jwt_manager),
                authorizer: Arc::new(authorizer),
                start_time: Instant::now(),
            },
            bind_addr,
        }
    }

    /// Build the router with all endpoints and middleware
    ///
    /// # Returns
    ///
    /// Configured Axum router
    fn router(&self) -> Router {
        // Define routes
        let api_routes = Router::new()
            .route("/register", post(handle_register))
            .route("/pull", post(handle_pull))
            .route("/push", post(handle_push))
            .route("/heartbeat", post(handle_heartbeat))
            .route("/health", get(handle_health));

        // Build main router with versioned API
        Router::new()
            .nest("/api/v1/sync", api_routes)
            .layer(
                ServiceBuilder::new()
                    // Tracing for request logging
                    .layer(TraceLayer::new_for_http())
                    // CORS support for web clients
                    .layer(
                        CorsLayer::new()
                            .allow_origin(Any)
                            .allow_methods(Any)
                            .allow_headers(vec![
                                header::CONTENT_TYPE,
                                header::AUTHORIZATION,
                            ]),
                    )
                    // gzip compression for large responses
                    .layer(CompressionLayer::new())
                    // Request ID middleware
                    .layer(middleware::from_fn(request_id_middleware)),
            )
            .with_state(self.state.clone())
    }

    /// Start serving HTTP requests
    ///
    /// This is a blocking call that runs the server until shutdown.
    ///
    /// # Returns
    ///
    /// Ok on successful shutdown, error on failure
    pub async fn serve(self) -> SyncResult<()> {
        let app = self.router();

        tracing::info!("Starting HTTP sync server on {}", self.bind_addr);

        let listener = tokio::net::TcpListener::bind(&self.bind_addr)
            .await
            .map_err(|e| SyncError::Network(format!("Failed to bind to {}: {}", self.bind_addr, e)))?;

        tracing::info!("HTTP sync server listening on {}", self.bind_addr);

        axum::serve(listener, app)
            .await
            .map_err(|e| SyncError::Network(format!("Server error: {}", e)))?;

        Ok(())
    }
}

/// Request/Response types for API endpoints

#[derive(Debug, Serialize, Deserialize)]
struct RegisterRequest {
    #[serde(flatten)]
    message: SyncMessage,
}

#[derive(Debug, Serialize, Deserialize)]
struct RegisterResponse {
    success: bool,
    client_id: String,
    server_version: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct PullRequest {
    #[serde(flatten)]
    message: SyncMessage,
}

#[derive(Debug, Serialize, Deserialize)]
struct PullResponseWrapper {
    #[serde(flatten)]
    message: SyncMessage,
}

#[derive(Debug, Serialize, Deserialize)]
struct PushRequest {
    #[serde(flatten)]
    message: SyncMessage,
}

#[derive(Debug, Serialize, Deserialize)]
struct PushResponseWrapper {
    #[serde(flatten)]
    message: SyncMessage,
}

#[derive(Debug, Serialize, Deserialize)]
struct HeartbeatRequest {
    #[serde(flatten)]
    message: SyncMessage,
}

#[derive(Debug, Serialize, Deserialize)]
struct HeartbeatResponse {
    success: bool,
    timestamp: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct HealthResponse {
    status: String,
    version: u32,
    uptime_secs: u64,
    registered_clients: usize,
    timestamp: u64,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
    code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<String>,
}

/// Fallback header value for invalid request IDs (should never be used in practice)
const INVALID_REQUEST_ID: HeaderValue = HeaderValue::from_static("invalid");

/// Middleware to add request IDs to all requests
async fn request_id_middleware(req: Request, next: Next) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();

    tracing::debug!(
        request_id = %request_id,
        method = %req.method(),
        uri = %req.uri(),
        "Received request"
    );

    let mut response = next.run(req).await;

    // UUID strings are always valid header values (ASCII alphanumeric + hyphens)
    response.headers_mut().insert(
        "X-Request-ID",
        request_id.parse().unwrap_or(INVALID_REQUEST_ID),
    );

    response
}

/// Extract JWT token from Authorization header
fn extract_token(headers: &HeaderMap) -> SyncResult<String> {
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .ok_or_else(|| SyncError::Authentication)?;

    let auth_str = auth_header
        .to_str()
        .map_err(|_| SyncError::Authentication)?;

    if !auth_str.starts_with("Bearer ") {
        return Err(SyncError::Authentication);
    }

    Ok(auth_str[7..].to_string())
}

/// Validate JWT token and return claims
async fn authenticate(
    state: &ServerState,
    headers: &HeaderMap,
) -> SyncResult<Claims> {
    let token = extract_token(headers)?;

    let claims = state
        .jwt_manager
        .validate_with_scope(&token, "sync:read")
        .map_err(|e| {
            tracing::warn!("JWT validation failed: {:?}", e);
            SyncError::Authentication
        })?;

    if claims.is_expired() {
        tracing::warn!("Token expired for user: {}", claims.sub);
        return Err(SyncError::Authentication);
    }

    state.authorizer.validate_claims(&claims).map_err(|e| {
        tracing::warn!("Authorization failed for tenant: {}", claims.tenant_id);
        SyncError::Authentication
    })?;

    Ok(claims)
}

/// Handler for POST /api/v1/sync/register
///
/// Register a client for synchronization
async fn handle_register(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> std::result::Result<impl IntoResponse, AppError> {
    // Optional authentication - can be made required
    let _claims = authenticate(&state, &headers).await.ok();

    // Validate message type
    if !matches!(req.message, SyncMessage::RegisterClient { .. }) {
        return Err(AppError::BadRequest("Expected RegisterClient message".to_string()));
    }

    // Extract client_id for response
    let client_id = if let SyncMessage::RegisterClient { ref client_id, .. } = req.message {
        client_id.clone()
    } else {
        return Err(AppError::BadRequest("Invalid message structure".to_string()));
    };

    // Handle registration
    state.protocol.handle_register(req.message)
        .map_err(AppError::from)?;

    tracing::info!("Client registered: {}", client_id);

    let response = RegisterResponse {
        success: true,
        client_id,
        server_version: PROTOCOL_VERSION,
    };

    Ok((StatusCode::OK, Json(response)))
}

/// Handler for POST /api/v1/sync/pull
///
/// Pull changes from server
async fn handle_pull(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(req): Json<PullRequest>,
) -> std::result::Result<impl IntoResponse, AppError> {
    // Authenticate request
    let claims = authenticate(&state, &headers).await.map_err(AppError::from)?;

    // Validate message type
    if !matches!(req.message, SyncMessage::PullRequest { .. }) {
        return Err(AppError::BadRequest("Expected PullRequest message".to_string()));
    }

    // Verify client_id matches JWT claims
    if let SyncMessage::PullRequest { ref client_id, .. } = req.message {
        if claims.client_id.to_string() != *client_id {
            tracing::warn!(
                "Client ID mismatch: JWT={}, Request={}",
                claims.client_id,
                client_id
            );
            return Err(AppError::Unauthorized);
        }
    }

    // Handle pull request
    let response_msg = state.protocol.handle_pull_request(req.message)
        .map_err(AppError::from)?;

    let response = PullResponseWrapper {
        message: response_msg,
    };

    Ok((StatusCode::OK, Json(response)))
}

/// Handler for POST /api/v1/sync/push
///
/// Push changes to server
async fn handle_push(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(req): Json<PushRequest>,
) -> std::result::Result<impl IntoResponse, AppError> {
    // Authenticate request
    let claims = authenticate(&state, &headers).await.map_err(AppError::from)?;

    // Validate message type
    if !matches!(req.message, SyncMessage::PushChanges { .. }) {
        return Err(AppError::BadRequest("Expected PushChanges message".to_string()));
    }

    // Verify client_id matches JWT claims
    if let SyncMessage::PushChanges { ref client_id, .. } = req.message {
        if claims.client_id.to_string() != *client_id {
            tracing::warn!(
                "Client ID mismatch: JWT={}, Request={}",
                claims.client_id,
                client_id
            );
            return Err(AppError::Unauthorized);
        }
    }

    // Handle push changes
    let response_msg = state.protocol.handle_push_changes(req.message)
        .map_err(AppError::from)?;

    let response = PushResponseWrapper {
        message: response_msg,
    };

    Ok((StatusCode::OK, Json(response)))
}

/// Handler for POST /api/v1/sync/heartbeat
///
/// Client heartbeat
async fn handle_heartbeat(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(req): Json<HeartbeatRequest>,
) -> std::result::Result<impl IntoResponse, AppError> {
    // Authenticate request
    let claims = authenticate(&state, &headers).await.map_err(AppError::from)?;

    // Validate message type
    if !matches!(req.message, SyncMessage::Heartbeat { .. }) {
        return Err(AppError::BadRequest("Expected Heartbeat message".to_string()));
    }

    // Verify client_id matches JWT claims
    if let SyncMessage::Heartbeat { ref client_id, .. } = req.message {
        if claims.client_id.to_string() != *client_id {
            tracing::warn!(
                "Client ID mismatch: JWT={}, Request={}",
                claims.client_id,
                client_id
            );
            return Err(AppError::Unauthorized);
        }
    }

    // Handle heartbeat
    state.protocol.handle_heartbeat(req.message)
        .map_err(AppError::from)?;

    let response = HeartbeatResponse {
        success: true,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| AppError::InternalError("Time error".to_string()))?
            .as_millis() as u64,
    };

    Ok((StatusCode::OK, Json(response)))
}

/// Handler for GET /api/v1/sync/health
///
/// Server health check
async fn handle_health(
    State(state): State<ServerState>,
) -> std::result::Result<impl IntoResponse, AppError> {
    let uptime = state.start_time.elapsed().as_secs();
    let registered_clients = state.protocol.client_count();

    let response = HealthResponse {
        status: "healthy".to_string(),
        version: PROTOCOL_VERSION,
        uptime_secs: uptime,
        registered_clients,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| AppError::InternalError("Time error".to_string()))?
            .as_millis() as u64,
    };

    Ok((StatusCode::OK, Json(response)))
}

/// Application error type for HTTP responses
#[derive(Debug)]
enum AppError {
    BadRequest(String),
    Unauthorized,
    InternalError(String),
    SyncError(SyncError),
}

impl From<SyncError> for AppError {
    fn from(err: SyncError) -> Self {
        AppError::SyncError(err)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_code, message, details) = match self {
            AppError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                msg,
                None,
            ),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "Authentication failed".to_string(),
                None,
            ),
            AppError::InternalError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "Internal server error".to_string(),
                Some(msg),
            ),
            AppError::SyncError(e) => match e {
                SyncError::Authentication => (
                    StatusCode::UNAUTHORIZED,
                    "AUTH_FAILED",
                    "Authentication failed".to_string(),
                    None,
                ),
                SyncError::InvalidMessage(msg) => (
                    StatusCode::BAD_REQUEST,
                    "INVALID_MESSAGE",
                    msg,
                    None,
                ),
                SyncError::Network(msg) => (
                    StatusCode::BAD_GATEWAY,
                    "NETWORK_ERROR",
                    "Network error".to_string(),
                    Some(msg),
                ),
                SyncError::Storage(msg) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "STORAGE_ERROR",
                    "Storage error".to_string(),
                    Some(msg),
                ),
                SyncError::Serialization(msg) => (
                    StatusCode::BAD_REQUEST,
                    "SERIALIZATION_ERROR",
                    "Serialization error".to_string(),
                    Some(msg),
                ),
                SyncError::ConflictResolution(msg) => (
                    StatusCode::CONFLICT,
                    "CONFLICT_ERROR",
                    "Conflict resolution failed".to_string(),
                    Some(msg),
                ),
                SyncError::QueueFull => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "QUEUE_FULL",
                    "Queue full".to_string(),
                    None,
                ),
            },
        };

        let error_response = ErrorResponse {
            error: message,
            code: error_code.to_string(),
            details,
        };

        tracing::warn!(
            status = %status,
            code = error_code,
            "Request failed"
        );

        (status, Json(error_response)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{
        protocol::{ChangeLog as ChangeLogTrait, ConflictDetector},
        vector_clock::VectorClock,
        protocol::ChangeEntry,
    };
    use parking_lot::RwLock;
    use std::collections::HashMap;

    // Mock implementations for testing
    struct MockChangeLog {
        changes: RwLock<Vec<ChangeEntry>>,
    }

    impl MockChangeLog {
        fn new() -> Self {
            Self {
                changes: RwLock::new(Vec::new()),
            }
        }
    }

    impl ChangeLogTrait for MockChangeLog {
        fn get_changes_since(&self, lsn: u64, limit: usize) -> super::Result<Vec<ChangeEntry>> {
            let changes = self.changes.read();
            Ok(changes
                .iter()
                .filter(|c| c.lsn > lsn)
                .take(limit)
                .cloned()
                .collect())
        }

        fn current_lsn(&self) -> super::Result<u64> {
            let changes = self.changes.read();
            Ok(changes.last().map(|c| c.lsn).unwrap_or(0))
        }

        fn append_changes(&self, changes: &[ChangeEntry]) -> super::Result<Vec<u64>> {
            let mut log = self.changes.write();
            let lsns: Vec<u64> = changes.iter().map(|c| c.lsn).collect();
            log.extend_from_slice(changes);
            Ok(lsns)
        }
    }

    struct MockConflictDetector;

    impl ConflictDetector for MockConflictDetector {
        fn detect_conflicts(
            &self,
            _local_clock: &VectorClock,
            _remote_changes: &[ChangeEntry],
        ) -> super::Result<Vec<crate::sync::protocol::ConflictReport>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn test_http_server_creation() {
        let change_log = Arc::new(MockChangeLog::new());
        let conflict_detector = Arc::new(MockConflictDetector);
        let protocol = Arc::new(SyncProtocol::new(change_log, conflict_detector));

        let bind_addr: SocketAddr = "127.0.0.1:8080".parse().expect("Valid address");
        let server = HttpSyncServer::new(protocol, bind_addr);

        assert_eq!(server.bind_addr, bind_addr);
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let change_log = Arc::new(MockChangeLog::new());
        let conflict_detector = Arc::new(MockConflictDetector);
        let protocol = Arc::new(SyncProtocol::new(change_log, conflict_detector));

        let state = ServerState {
            protocol,
            jwt_manager: Arc::new(JwtManager::from_env_or_default()),
            authorizer: Arc::new(Authorizer::new()),
            start_time: Instant::now(),
        };

        let result = handle_health(State(state)).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_extract_token_valid() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer test-token-123".parse().expect("Valid header"),
        );

        let token = extract_token(&headers).expect("Should extract token");
        assert_eq!(token, "test-token-123");
    }

    #[test]
    fn test_extract_token_missing() {
        let headers = HeaderMap::new();
        let result = extract_token(&headers);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_token_invalid_format() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "InvalidFormat token".parse().expect("Valid header"),
        );

        let result = extract_token(&headers);
        assert!(result.is_err());
    }
}
