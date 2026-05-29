//! REST API module for HeliosDB-Lite
//!
//! Provides HTTP REST API endpoints for database operations.

pub mod auth_bridge;
pub mod change_notifier;
pub mod handlers;
pub mod jwt;
pub mod middleware;
pub mod models;
pub mod oauth;
pub mod openapi;
pub mod rest_executor;
pub mod routes;
pub mod server;

// Re-exports for convenience
pub use middleware::{AuthMiddleware, RateLimitConfig, RateLimitMiddleware, UserContext};
pub use models::error::ApiError;
pub use openapi::OPENAPI_YAML;
pub use server::ApiServer;
