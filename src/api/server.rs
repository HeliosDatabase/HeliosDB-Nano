//! Axum server setup for REST API
//!
//! Provides HTTP server with CORS, logging middleware, and route configuration.

use axum::{
    Router,
    extract::Request,
    middleware::Next,
    http::{
        header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE},
        Method, StatusCode,
    },
};
use tower::ServiceBuilder;
use tower_http::{
    cors::CorsLayer,
    trace::TraceLayer,
    compression::CompressionLayer,
};
use std::sync::Arc;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;

use crate::{EmbeddedDatabase, Result, Error};
use crate::compute::QueryRegistry;
use super::routes;
use super::middleware::{AuthMiddleware, RateLimitMiddleware, rate_limit_middleware};

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    /// Database instance
    pub db: Arc<EmbeddedDatabase>,
    /// Query registry for tracking and cancelling running queries
    pub query_registry: Arc<QueryRegistry>,
}

/// REST API Server
pub struct ApiServer {
    /// Server address
    addr: SocketAddr,
    /// Application state
    state: AppState,
    /// Authentication middleware
    auth_middleware: Option<Arc<AuthMiddleware>>,
    /// Rate limiting middleware
    rate_limit_middleware: Option<Arc<RateLimitMiddleware>>,
}

impl ApiServer {
    /// Create a new API server
    ///
    /// # Arguments
    ///
    /// * `addr` - Socket address to bind to (e.g., "127.0.0.1:8080")
    /// * `db` - Database instance to expose via API
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::{EmbeddedDatabase, api::ApiServer};
    /// use std::sync::Arc;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    /// let addr = "127.0.0.1:8080".parse()?;
    /// let server = ApiServer::new(addr, db);
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(addr: SocketAddr, db: Arc<EmbeddedDatabase>) -> Self {
        Self {
            addr,
            state: AppState {
                db,
                query_registry: Arc::new(QueryRegistry::new()),
            },
            auth_middleware: None,
            rate_limit_middleware: None,
        }
    }

    /// Create a new API server with a custom query registry
    pub fn with_query_registry(
        addr: SocketAddr,
        db: Arc<EmbeddedDatabase>,
        query_registry: Arc<QueryRegistry>,
    ) -> Self {
        Self {
            addr,
            state: AppState { db, query_registry },
            auth_middleware: None,
            rate_limit_middleware: None,
        }
    }

    /// Enable authentication middleware
    ///
    /// # Arguments
    ///
    /// * `auth` - Authentication middleware instance
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::{EmbeddedDatabase, api::{ApiServer, AuthMiddleware}};
    /// use std::sync::Arc;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    /// let addr = "127.0.0.1:8080".parse()?;
    /// let auth = AuthMiddleware::from_env_or_default();
    /// let server = ApiServer::new(addr, db)
    ///     .with_auth(auth);
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_auth(mut self, auth: AuthMiddleware) -> Self {
        self.auth_middleware = Some(Arc::new(auth));
        self
    }

    /// Enable rate limiting middleware
    ///
    /// # Arguments
    ///
    /// * `rate_limiter` - Rate limiting middleware instance
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::{EmbeddedDatabase, api::{ApiServer, RateLimitMiddleware, RateLimitConfig}};
    /// use std::sync::Arc;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    /// let addr = "127.0.0.1:8080".parse()?;
    /// let rate_limiter = RateLimitMiddleware::new(RateLimitConfig::authenticated());
    /// let server = ApiServer::new(addr, db)
    ///     .with_rate_limiting(rate_limiter);
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_rate_limiting(mut self, rate_limiter: RateLimitMiddleware) -> Self {
        self.rate_limit_middleware = Some(Arc::new(rate_limiter));
        self
    }

    /// Build the application router with all routes and middleware
    fn build_router(&self) -> Router {
        // Create CORS layer
        let cors = CorsLayer::new()
            .allow_origin(tower_http::cors::Any)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::DELETE,
                Method::PATCH,
                Method::OPTIONS,
            ])
            .allow_headers([ACCEPT, AUTHORIZATION, CONTENT_TYPE]);

        // Create base middleware stack (applied to all routes)
        let base_middleware = ServiceBuilder::new()
            .layer(TraceLayer::new_for_http())
            .layer(CompressionLayer::new())
            .layer(cors);

        // Build protected v1 routes (require authentication)
        let v1_router = routes::v1_routes();

        // Apply authentication middleware if configured
        let v1_router = if let Some(auth) = &self.auth_middleware {
            let auth_clone = auth.clone();
            v1_router.layer(axum::middleware::from_fn(move |mut req: Request, next: Next| {
                let auth = auth_clone.clone();
                async move {
                    use axum::http::header;
                    use crate::api::models::ApiError;

                    // Extract authentication info from request headers
                    let auth_header = req.headers().get(header::AUTHORIZATION)
                        .and_then(|h| h.to_str().ok())
                        .and_then(|s| s.strip_prefix("Bearer ").map(String::from));

                    let api_key = req.headers().get("x-api-key")
                        .and_then(|h| h.to_str().ok())
                        .map(String::from);

                    // Authenticate using extracted credentials
                    let user_ctx = if let Some(token) = auth_header {
                        auth.authenticate_jwt(&token).await
                    } else if let Some(key) = api_key {
                        auth.authenticate_api_key(&key).await
                    } else {
                        Err(ApiError::unauthorized("Missing or invalid authentication credentials"))
                    };

                    match user_ctx {
                        Ok(ctx) => {
                            // Attach user context to request extensions
                            req.extensions_mut().insert(ctx);
                            // Continue to next middleware/handler
                            Ok(next.run(req).await)
                        }
                        Err(err) => Err(err),
                    }
                }
            }))
        } else {
            v1_router
        };

        // Apply rate limiting middleware if configured
        let v1_router = if let Some(rate_limiter) = &self.rate_limit_middleware {
            let limiter = rate_limiter.clone();
            v1_router.layer(axum::middleware::from_fn(move |req, next| {
                let limiter = limiter.clone();
                rate_limit_middleware(limiter, req, next)
            }))
        } else {
            v1_router
        };

        // Build router with public and protected routes
        Router::new()
            .nest("/v1", v1_router)
            .route("/health", axum::routing::get(health_check))
            .route("/version", axum::routing::get(version_info))
            .layer(base_middleware)
            .with_state(self.state.clone())
    }

    /// Start the API server
    ///
    /// Runs the server and listens for incoming requests.
    /// This method blocks until the server is shut down.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::{EmbeddedDatabase, api::ApiServer};
    /// use std::sync::Arc;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    /// let addr = "127.0.0.1:8080".parse()?;
    /// let server = ApiServer::new(addr, db);
    /// server.serve().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn serve(self) -> Result<()> {
        let app = self.build_router();

        info!("Starting HeliosDB-Lite REST API server on {}", self.addr);

        let listener = TcpListener::bind(self.addr)
            .await
            .map_err(|e| Error::network(format!("Failed to bind to {}: {}", self.addr, e)))?;

        info!("API server listening on {}", self.addr);

        axum::serve(listener, app)
            .await
            .map_err(|e| Error::network(format!("Server error: {}", e)))?;

        Ok(())
    }

    /// Start the API server with graceful shutdown
    ///
    /// Runs the server and listens for incoming requests.
    /// The server will shut down gracefully when the provided signal future completes.
    ///
    /// # Arguments
    ///
    /// * `shutdown_signal` - Future that completes when shutdown should begin
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::{EmbeddedDatabase, api::ApiServer};
    /// use std::sync::Arc;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    /// let addr = "127.0.0.1:8080".parse()?;
    /// let server = ApiServer::new(addr, db);
    ///
    /// // Shutdown on Ctrl+C
    /// server.serve_with_shutdown(async {
    ///     tokio::signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
    ///     println!("Shutting down gracefully...");
    /// }).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn serve_with_shutdown<F>(self, shutdown_signal: F) -> Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let app = self.build_router();

        info!("Starting HeliosDB-Lite REST API server on {}", self.addr);

        let listener = TcpListener::bind(self.addr)
            .await
            .map_err(|e| Error::network(format!("Failed to bind to {}: {}", self.addr, e)))?;

        info!("API server listening on {}", self.addr);

        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal)
            .await
            .map_err(|e| Error::network(format!("Server error: {}", e)))?;

        info!("API server shut down gracefully");

        Ok(())
    }
}

/// Health check endpoint
async fn health_check() -> (StatusCode, &'static str) {
    (StatusCode::OK, "OK")
}

/// Version information endpoint
async fn version_info() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "name": "HeliosDB-Lite",
        "version": env!("CARGO_PKG_VERSION"),
        "api_version": "v1",
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_app_state_creation() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let query_registry = Arc::new(QueryRegistry::new());
        let state = AppState { db, query_registry };
        assert!(Arc::strong_count(&state.db) >= 1);
    }

    #[tokio::test]
    async fn test_health_check() {
        let (status, body) = health_check().await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "OK");
    }

    #[tokio::test]
    async fn test_version_info() {
        let response = version_info().await;
        let json = response.0;
        assert_eq!(json["name"], "HeliosDB-Lite");
        assert!(json["version"].is_string());
        assert_eq!(json["api_version"], "v1");
    }
}
