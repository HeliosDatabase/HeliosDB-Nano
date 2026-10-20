//! REST API module for HeliosDB-Lite
//!
//! Provides HTTP REST API endpoints for database operations.

pub mod auth_bridge;
pub mod change_notifier;
pub mod handlers;
pub mod http_auth;
pub mod jwt;
pub mod middleware;
pub mod models;
pub mod oauth;
pub mod openapi;
pub mod rest_executor;
pub mod routes;
pub mod server;

// Re-exports for convenience
pub use http_auth::{
    attach_http_auth, attach_http_auth_with_replication, http_bind_safety_check, resolve_http_listen, HttpAuthPolicy,
    HttpAuthRejection, HttpRateLimits, ReplicationStatus, FAILURE_WINDOW, HTTP_DEFAULT_LISTEN, MAX_FAILURES,
    MAX_IN_FLIGHT_VERIFICATIONS, MAX_TRACKED_PEERS,
};
pub use middleware::{AuthMiddleware, RateLimitConfig, RateLimitMiddleware, UserContext};
pub use models::error::ApiError;
pub use openapi::OPENAPI_YAML;
pub use server::{ApiServer, EPHEMERAL_JWT_WARNING_PREFIX};
