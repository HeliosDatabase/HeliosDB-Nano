//! Axum server setup for REST API
//!
//! Provides HTTP server with CORS, logging middleware, and route configuration.

use axum::{
    extract::Request,
    http::{
        header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE},
        Method, StatusCode,
    },
    middleware::Next,
    response::Html,
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::{compression::CompressionLayer, cors::CorsLayer, trace::TraceLayer};
use tracing::info;

use super::auth_bridge::AuthBridge;
use super::change_notifier::ChangeNotifier;
use super::middleware::{rate_limit_middleware, AuthMiddleware, RateLimitMiddleware};
use super::oauth::OAuthRegistry;
use super::routes;
use crate::compute::QueryRegistry;
use crate::config::ApiConfig;
use crate::{EmbeddedDatabase, Error, Result};

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    /// Database instance
    pub db: Arc<EmbeddedDatabase>,
    /// Query registry for tracking and cancelling running queries
    pub query_registry: Arc<QueryRegistry>,
    /// Optional BaaS auth bridge (database-persisted authentication)
    pub auth_bridge: Option<Arc<AuthBridge>>,
    /// Optional OAuth2 provider registry (Google, GitHub, etc.)
    pub oauth_registry: Option<Arc<OAuthRegistry>>,
    /// Optional realtime change notifier for WebSocket subscriptions
    pub change_notifier: Option<Arc<ChangeNotifier>>,
}

/// Prefix of the one warning [`ApiServer::from_config`] returns that is EXPECTED in a
/// correct default deployment.
///
/// `from_config` returns `Vec<String>`, which cannot carry a log level, and the two
/// kinds of warning it produces do not deserve the same one: a refused OAuth provider
/// or a failed auth bootstrap means an advertised feature is dead and is an ERROR,
/// while an unconfigured `[api] jwt_secret` is the documented default behaviour and is
/// a WARN. Rather than have callers sniff message text, the ephemeral notice carries
/// this stable prefix and `src/main.rs` matches on it. `from_config`'s own unit test
/// asserts the notice still starts with it, so the two cannot drift apart silently.
pub const EPHEMERAL_JWT_WARNING_PREFIX: &str = "[api] jwt_secret is not configured";

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
    /// use heliosdb_nano::{EmbeddedDatabase, api::ApiServer};
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
        let change_notifier = Arc::new(ChangeNotifier::new(db.clone()));
        Self {
            addr,
            state: AppState {
                db,
                query_registry: Arc::new(QueryRegistry::new()),
                auth_bridge: None,
                oauth_registry: None,
                change_notifier: Some(change_notifier),
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
            state: AppState {
                db,
                query_registry,
                auth_bridge: None,
                oauth_registry: None,
                change_notifier: None,
            },
            auth_middleware: None,
            rate_limit_middleware: None,
        }
    }

    /// Assemble the server the way `heliosdb-nano start` does: auth bridge bootstrapped,
    /// OAuth providers registered from `[[api.oauth_providers]]`.
    ///
    /// Returns the server plus warnings the caller should log. Nothing here logs or
    /// panics, so the exact assembly production runs is reachable from a test.
    ///
    /// # Why this exists
    ///
    /// This is the BaaS layer the README advertises: PostgREST-style `/rest/v1/*`,
    /// `/auth/v1/*`, `/realtime/v1/websocket`, Swagger `/docs` + `/openapi.json`, and
    /// `/version`. Through v4.26.0 the `start` listener served `/` and `/health` and
    /// nothing else, so every one of those endpoints 404'd on the shipped binary.
    ///
    /// v4.27.0 mounted the router — by building it INLINE in `run_http_listener`, while
    /// `tests/baas_http_surface_tests.rs` built its own. Two independent assemblies, so
    /// a test could pass while production wiring was missing, and it duly did: nothing
    /// ever called [`ApiServer::with_oauth_registry`], so `AppState.oauth_registry` was
    /// always `None` and `GET /auth/v1/authorize?provider=google` answered 503
    /// "oauth_not_configured" no matter what the operator put in `config.toml`. This
    /// function is the single seam both sides now go through; adding a third copy would
    /// reopen exactly that gap.
    ///
    /// # JWT signing key
    ///
    /// The key comes from `[api] jwt_secret`, which defaults to a freshly generated
    /// 256-bit CSPRNG value per start. There is deliberately NO constant fallback
    /// anywhere on this path: a guessable signing key lets anyone mint a valid session,
    /// the same shape as the `--auth md5` fail-open fixed in v4.26.0. A random per-start
    /// key means tokens do not survive a restart unless an operator configures one —
    /// that is the correct trade, and the returned warning makes it visible.
    ///
    /// # Failure handling
    ///
    /// Nothing here is fatal, because a database that refuses to start over a mistyped
    /// OAuth provider name is worse than one that starts without that provider. Every
    /// degradation is instead reported:
    ///
    /// * `bootstrap()` creating `_auth_users` / `_auth_refresh_tokens` fails -> warning;
    ///   `/auth/v1/*` will return errors, which is what it did before this was called at
    ///   all (its first real signup died with "Table '_auth_users' does not exist").
    /// * A provider entry is refused -> warning, and it is NOT registered
    ///   (see [`OAuthRegistry::from_config`]).
    /// * No provider registered -> the registry is NOT attached, so `oauth_registry`
    ///   stays `None` and `/auth/v1/authorize` keeps returning its existing 503
    ///   "OAuth is not configured on this server". Attaching an empty registry would
    ///   instead answer 400 "OAuth provider not found: google", which tells an operator
    ///   who configured nothing that their provider is unknown — a worse answer.
    pub fn from_config(addr: SocketAddr, db: Arc<EmbeddedDatabase>, api_config: &ApiConfig) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();

        let auth_bridge = Arc::new(AuthBridge::new(Arc::clone(&db), &api_config.jwt_secret));
        // `bootstrap` is idempotent (CREATE TABLE IF NOT EXISTS). Mounting the auth
        // routes without it just moves the failure from 404 to 500.
        if let Err(e) = auth_bridge.bootstrap() {
            warnings.push(format!(
                "auth schema bootstrap failed: {e}; /auth/v1/* endpoints will return errors until this is resolved"
            ));
        }

        let (registry, oauth_warnings) = OAuthRegistry::from_config(&api_config.oauth_providers);
        warnings.extend(oauth_warnings);

        if api_config.jwt_secret_is_ephemeral {
            warnings.push(format!(
                "{EPHEMERAL_JWT_WARNING_PREFIX}; generated an ephemeral one for this process. \
                 Auth tokens issued now become invalid on restart. \
                 Set [api] jwt_secret to persist them."
            ));
        }

        let mut server = Self::new(addr, db).with_auth_bridge(auth_bridge);
        if registry.has_providers() {
            server = server.with_oauth_registry(Arc::new(registry));
        }

        (server, warnings)
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
    /// use heliosdb_nano::{EmbeddedDatabase, api::{ApiServer, AuthMiddleware}};
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
    /// use heliosdb_nano::{EmbeddedDatabase, api::{ApiServer, RateLimitMiddleware, RateLimitConfig}};
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

    /// Enable the BaaS auth bridge (database-persisted user accounts + JWT).
    pub fn with_auth_bridge(mut self, bridge: Arc<AuthBridge>) -> Self {
        self.state.auth_bridge = Some(bridge);
        self
    }

    /// Enable OAuth2 provider registry (Google, GitHub, etc.).
    pub fn with_oauth_registry(mut self, registry: Arc<OAuthRegistry>) -> Self {
        self.state.oauth_registry = Some(registry);
        self
    }

    /// Build the application router with all routes and middleware, for a caller
    /// that already owns its listener.
    ///
    /// `serve()` binds its own socket. The `heliosdb-nano start` command cannot use
    /// that: it binds the HTTP port up front so a bind failure is reported before
    /// the database opens (see `run_http_listener`). It therefore needs the router
    /// on its own, which is why this is public.
    ///
    /// Until v4.27.0 nothing outside this file called `build_router` at all, so the
    /// whole REST / Auth / Realtime / Swagger surface existed only as a library API
    /// while the README advertised it as a built-in feature of the server.
    pub fn into_router(self) -> Router {
        self.build_router()
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
                    use crate::api::models::ApiError;
                    use axum::http::header;

                    // Extract authentication info from request headers
                    let auth_header = req
                        .headers()
                        .get(header::AUTHORIZATION)
                        .and_then(|h| h.to_str().ok())
                        .and_then(|s| s.strip_prefix("Bearer ").map(String::from));

                    let api_key = req
                        .headers()
                        .get("x-api-key")
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

        // Build PostgREST-compatible REST routes
        let rest_router = routes::rest::routes();

        // Build auth routes (public - no auth middleware required)
        use super::handlers::{auth_handler, oauth_handler};
        let auth_router: Router<AppState> = Router::new()
            .route("/auth/v1/signup", axum::routing::post(auth_handler::signup))
            .route("/auth/v1/token", axum::routing::post(auth_handler::signin))
            .route("/auth/v1/logout", axum::routing::post(auth_handler::logout))
            .route("/auth/v1/refresh", axum::routing::post(auth_handler::refresh))
            .route("/auth/v1/user", axum::routing::get(auth_handler::get_user))
            .route("/auth/v1/authorize", axum::routing::get(oauth_handler::authorize))
            .route("/auth/v1/callback", axum::routing::get(oauth_handler::callback));

        // Build router with public and protected routes
        let router = Router::new()
            .nest("/v1", v1_router)
            .nest("/rest/v1", rest_router)
            .merge(auth_router)
            .route(
                "/realtime/v1/websocket",
                axum::routing::get(super::handlers::ws_handler::ws_upgrade),
            )
            .route("/health", axum::routing::get(health_check))
            .route("/version", axum::routing::get(version_info))
            .route("/docs", axum::routing::get(swagger_ui))
            .route("/openapi.json", axum::routing::get(openapi_json));

        // MCP is deliberately NOT mounted here. The host process mounts it once, with
        // the bearer token and the bind-safety check, via `mcp::attach_mcp_routes`
        // (see `run_http_listener` in src/main.rs). Until v4.31 this router carried its
        // own copy built from `McpState::new(db)` — no auth — and the two mounts
        // collided at build time ("Overlapping method route … POST /mcp"), which
        // killed the HTTP listener of every `mcp-endpoint` build since v4.27.0
        // (sprinter 43b59beba8a9). Exactly one mount, and it must be the
        // authenticated one.

        router.layer(base_middleware).with_state(self.state.clone())
    }

    /// Start the API server
    ///
    /// Runs the server and listens for incoming requests.
    /// This method blocks until the server is shut down.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_nano::{EmbeddedDatabase, api::ApiServer};
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

        info!("Starting HeliosDB Nano REST API server on {}", self.addr);

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
    /// use heliosdb_nano::{EmbeddedDatabase, api::ApiServer};
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

        info!("Starting HeliosDB Nano REST API server on {}", self.addr);

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
async fn health_check() -> axum::Json<serde_json::Value> {
    // `{"status":"ok"}`, NOT the plain string "OK".
    //
    // This is the response the `heliosdb-nano start` listener has always given on
    // /health and what monitoring is written against. When v4.27.0 mounted this
    // router on that listener, this handler took over the path — and returning
    // plain text here silently changed /health's content type and body for every
    // deployment. `daemon_http_listener_serves_health` caught it by failing to
    // decode the response as JSON; keep the shapes identical so the two can never
    // drift apart again.
    axum::Json(serde_json::json!({ "status": "ok" }))
}

/// Version information endpoint
async fn version_info() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "name": "HeliosDB-Nano",
        "version": env!("CARGO_PKG_VERSION"),
        "api_version": "v1",
    }))
}

/// Swagger UI HTML page
async fn swagger_ui() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE html>
<html>
<head>
  <title>HeliosDB Nano API</title>
  <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/swagger-ui-dist/swagger-ui.css">
</head>
<body>
  <div id="swagger-ui"></div>
  <script src="https://cdn.jsdelivr.net/npm/swagger-ui-dist/swagger-ui-bundle.js"></script>
  <script>SwaggerUIBundle({ url: '/openapi.json', dom_id: '#swagger-ui' })</script>
</body>
</html>"#,
    )
}

/// Serve the OpenAPI spec as JSON (converted from the bundled YAML)
async fn openapi_json() -> std::result::Result<axum::Json<serde_json::Value>, StatusCode> {
    let yaml_bytes = include_str!("openapi/openapi.yaml");
    let value: serde_json::Value = serde_yaml::from_str(yaml_bytes).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(axum::Json(value))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_app_state_creation() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let query_registry = Arc::new(QueryRegistry::new());
        let state = AppState {
            db,
            query_registry,
            auth_bridge: None,
            oauth_registry: None,
            change_notifier: None,
        };
        assert!(Arc::strong_count(&state.db) >= 1);
    }

    /// The ephemeral-JWT notice must keep the prefix `src/main.rs` matches on to decide
    /// WARN vs ERROR. If this fails, that notice is being logged as an error.
    #[test]
    fn test_ephemeral_jwt_warning_carries_its_documented_prefix() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let config = ApiConfig::default();
        assert!(config.jwt_secret_is_ephemeral, "Default() means unconfigured");

        let (_server, warnings) = ApiServer::from_config("127.0.0.1:0".parse().unwrap(), db, &config);

        assert!(
            warnings.iter().any(|w| w.starts_with(EPHEMERAL_JWT_WARNING_PREFIX)),
            "no warning carries the documented prefix: {warnings:?}"
        );
    }

    /// A configured key produces no warning at all — the clean-boot case must be quiet,
    /// or operators learn to ignore this list.
    #[test]
    fn test_configured_secret_and_no_providers_warns_about_nothing() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let config = ApiConfig {
            jwt_secret: "operator-chosen-key".to_string(),
            jwt_secret_is_ephemeral: false,
            ..ApiConfig::default()
        };

        let (server, warnings) = ApiServer::from_config("127.0.0.1:0".parse().unwrap(), db, &config);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(server.state.auth_bridge.is_some(), "the auth bridge must be attached");
        assert!(
            server.state.oauth_registry.is_none(),
            "zero configured providers must leave the registry unattached so /auth/v1/authorize \
             keeps answering 503 'not configured' rather than 400 'provider not found'"
        );
    }

    /// A refused provider must not end up in `AppState`.
    #[test]
    fn test_refused_provider_is_not_attached() {
        use crate::config::OAuthProviderConfig;

        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let config = ApiConfig {
            jwt_secret: "operator-chosen-key".to_string(),
            jwt_secret_is_ephemeral: false,
            oauth_providers: vec![OAuthProviderConfig {
                name: "google".to_string(),
                client_id: "test-client-id".to_string(),
                client_secret: "test-client-secret".to_string(),
                redirect_uri: "not a valid url".to_string(),
            }],
            ..ApiConfig::default()
        };

        let (server, warnings) = ApiServer::from_config("127.0.0.1:0".parse().unwrap(), db, &config);

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            server.state.oauth_registry.is_none(),
            "a provider that failed to build must not be served"
        );
    }

    #[tokio::test]
    async fn test_health_check() {
        // `{"status":"ok"}`, not the bare string "OK" this asserted until v4.27.0.
        // Once this router was mounted on the `start` listener, this handler began
        // serving the /health path that has always answered with that JSON object,
        // so the plain-text form was a silent content-type and body change for
        // every deployment's monitoring.
        let json = health_check().await.0;
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn test_version_info() {
        let response = version_info().await;
        let json = response.0;
        assert_eq!(json["name"], "HeliosDB-Nano");
        assert!(json["version"].is_string());
        assert_eq!(json["api_version"], "v1");
    }
}
