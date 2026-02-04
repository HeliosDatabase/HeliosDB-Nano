//! REST API module for HeliosDB-Lite
//!
//! Provides HTTP REST API endpoints for database operations.

pub mod server;
pub mod routes;
pub mod models;
pub mod handlers;
pub mod middleware;
pub mod jwt;
pub mod openapi;

// Re-exports for convenience
pub use server::ApiServer;
pub use models::error::ApiError;
pub use middleware::{AuthMiddleware, UserContext, RateLimitMiddleware, RateLimitConfig};
pub use openapi::OPENAPI_YAML;
