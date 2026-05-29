//! Middleware for REST API
//!
//! Provides authentication, authorization, and rate limiting middleware.

pub mod auth;
pub mod rate_limit;

// Re-exports
pub use auth::{auth_middleware, AuthMethod, AuthMiddleware, UserContext};
pub use rate_limit::{rate_limit_middleware, RateLimitConfig, RateLimitMiddleware};
