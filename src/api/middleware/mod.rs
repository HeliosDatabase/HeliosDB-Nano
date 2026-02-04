//! Middleware for REST API
//!
//! Provides authentication, authorization, and rate limiting middleware.

pub mod auth;
pub mod rate_limit;

// Re-exports
pub use auth::{AuthMiddleware, UserContext, AuthMethod, auth_middleware};
pub use rate_limit::{RateLimitMiddleware, RateLimitConfig, rate_limit_middleware};
