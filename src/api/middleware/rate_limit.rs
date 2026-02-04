//! Rate limiting middleware for REST API
//!
//! Implements token bucket algorithm for rate limiting based on IP address or API key.

use axum::{
    extract::{ConnectInfo, Request},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};

use crate::api::models::ApiError;
use super::auth::UserContext;

/// Rate limit configuration
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum number of requests allowed
    pub max_requests: u32,

    /// Time window for rate limiting
    pub window_duration: Duration,

    /// Burst capacity (how many requests can be made instantly)
    pub burst_capacity: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_requests: 100,
            window_duration: Duration::from_secs(60),
            burst_capacity: 10,
        }
    }
}

impl RateLimitConfig {
    /// Create a new rate limit configuration
    pub fn new(max_requests: u32, window_secs: u64, burst_capacity: u32) -> Self {
        Self {
            max_requests,
            window_duration: Duration::from_secs(window_secs),
            burst_capacity,
        }
    }

    /// Create a configuration for public endpoints (more restrictive)
    pub fn public() -> Self {
        Self {
            max_requests: 30,
            window_duration: Duration::from_secs(60),
            burst_capacity: 5,
        }
    }

    /// Create a configuration for authenticated endpoints
    pub fn authenticated() -> Self {
        Self {
            max_requests: 100,
            window_duration: Duration::from_secs(60),
            burst_capacity: 10,
        }
    }

    /// Create a configuration for heavy operations
    pub fn heavy_operations() -> Self {
        Self {
            max_requests: 10,
            window_duration: Duration::from_secs(60),
            burst_capacity: 2,
        }
    }
}

/// Rate limit information returned in response headers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitInfo {
    /// Maximum requests allowed in window
    pub limit: u32,

    /// Remaining requests in current window
    pub remaining: u32,

    /// Time when the rate limit resets (Unix timestamp)
    pub reset_at: u64,

    /// Retry after seconds (only present when rate limited)
    pub retry_after: Option<u64>,
}

/// Token bucket for rate limiting
#[derive(Debug, Clone)]
struct TokenBucket {
    /// Current number of tokens
    tokens: f64,

    /// Maximum tokens (capacity)
    capacity: f64,

    /// Token refill rate (tokens per second)
    refill_rate: f64,

    /// Last refill timestamp
    last_refill: Instant,

    /// Window start time for tracking
    window_start: Instant,
}

impl TokenBucket {
    /// Create a new token bucket
    fn new(capacity: f64, refill_rate: f64) -> Self {
        let now = Instant::now();
        Self {
            tokens: capacity,
            capacity,
            refill_rate,
            last_refill: now,
            window_start: now,
        }
    }

    /// Refill tokens based on elapsed time
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();

        // Calculate tokens to add
        let tokens_to_add = elapsed * self.refill_rate;

        // Add tokens but don't exceed capacity
        self.tokens = (self.tokens + tokens_to_add).min(self.capacity);
        self.last_refill = now;
    }

    /// Try to consume a token
    fn try_consume(&mut self) -> bool {
        self.refill();

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Get remaining tokens
    fn remaining(&mut self) -> u32 {
        self.refill();
        self.tokens.floor() as u32
    }

    /// Get seconds until next token
    fn time_until_token(&mut self) -> f64 {
        self.refill();
        if self.tokens >= 1.0 {
            0.0
        } else {
            (1.0 - self.tokens) / self.refill_rate
        }
    }

    /// Reset the window
    fn reset_window(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= Duration::from_secs(60) {
            self.window_start = now;
        }
    }
}

/// Rate limiting middleware
#[derive(Clone)]
pub struct RateLimitMiddleware {
    config: RateLimitConfig,
    buckets: Arc<Mutex<HashMap<String, TokenBucket>>>,
}

impl RateLimitMiddleware {
    /// Create new rate limiting middleware
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            buckets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create with default configuration
    pub fn default_config() -> Self {
        Self::new(RateLimitConfig::default())
    }

    /// Get rate limit key from request
    fn get_rate_limit_key(&self, req: &Request) -> String {
        // Try to get user context first (from auth middleware)
        if let Some(user_ctx) = req.extensions().get::<UserContext>() {
            return format!("user:{}", user_ctx.user_id);
        }

        // Fall back to IP address
        if let Some(ConnectInfo(addr)) = req.extensions().get::<ConnectInfo<SocketAddr>>() {
            return format!("ip:{}", addr.ip());
        }

        // Last resort: use a default key
        "unknown".to_string()
    }

    /// Check rate limit for a key
    fn check_rate_limit(&self, key: &str) -> Result<RateLimitInfo, RateLimitInfo> {
        // Recover from poisoned lock gracefully instead of panicking
        let mut buckets = match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // Log poison and recover the lock to prevent cascading failures
                eprintln!("WARNING: Rate limiter lock poisoned, recovering...");
                poisoned.into_inner()
            }
        };

        // Calculate refill rate: tokens per second to refill max_requests in window
        let refill_rate = self.config.max_requests as f64
            / self.config.window_duration.as_secs_f64();

        // Get or create bucket
        let bucket = buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket::new(self.config.burst_capacity as f64, refill_rate));

        // Reset window if needed
        bucket.reset_window();

        // Try to consume a token
        let allowed = bucket.try_consume();
        let remaining = bucket.remaining();
        let reset_at = (bucket.window_start + Duration::from_secs(60))
            .duration_since(Instant::now())
            .as_secs();

        let info = RateLimitInfo {
            limit: self.config.max_requests,
            remaining,
            reset_at,
            retry_after: if allowed {
                None
            } else {
                Some(bucket.time_until_token().ceil() as u64)
            },
        };

        if allowed {
            Ok(info)
        } else {
            Err(info)
        }
    }

    /// Clean up old buckets (should be called periodically)
    pub fn cleanup_old_buckets(&self) {
        // Recover from poisoned lock gracefully instead of panicking
        let mut buckets = match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                eprintln!("WARNING: Rate limiter lock poisoned during cleanup, recovering...");
                poisoned.into_inner()
            }
        };
        let now = Instant::now();

        buckets.retain(|_, bucket| {
            // Keep buckets that have been active in the last hour
            now.duration_since(bucket.last_refill) < Duration::from_secs(3600)
        });
    }
}

/// Rate limiting middleware handler
pub async fn rate_limit_middleware(
    rate_limiter: Arc<RateLimitMiddleware>,
    req: Request,
    next: Next,
) -> Result<Response, Response> {
    let key = rate_limiter.get_rate_limit_key(&req);

    match rate_limiter.check_rate_limit(&key) {
        Ok(info) => {
            // Rate limit check passed
            let mut response = next.run(req).await;

            // Add rate limit headers (gracefully handle parse errors)
            let headers = response.headers_mut();
            if let Ok(value) = info.limit.to_string().parse() {
                headers.insert("X-RateLimit-Limit", value);
            }
            if let Ok(value) = info.remaining.to_string().parse() {
                headers.insert("X-RateLimit-Remaining", value);
            }
            if let Ok(value) = info.reset_at.to_string().parse() {
                headers.insert("X-RateLimit-Reset", value);
            }

            Ok(response)
        }
        Err(info) => {
            // Rate limit exceeded
            let retry_after = info.retry_after.unwrap_or(60);

            let error = ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "RateLimitExceeded",
                "Rate limit exceeded",
            );

            let mut response = error.into_response();

            // Add rate limit headers (gracefully handle parse errors)
            let headers = response.headers_mut();
            if let Ok(value) = info.limit.to_string().parse() {
                headers.insert("X-RateLimit-Limit", value);
            }
            if let Ok(value) = "0".parse() {
                headers.insert("X-RateLimit-Remaining", value);
            }
            if let Ok(value) = info.reset_at.to_string().parse() {
                headers.insert("X-RateLimit-Reset", value);
            }
            if let Ok(value) = retry_after.to_string().parse() {
                headers.insert("Retry-After", value);
            }

            Err(response)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limit_config() {
        let config = RateLimitConfig::default();
        assert_eq!(config.max_requests, 100);
        assert_eq!(config.window_duration, Duration::from_secs(60));

        let public = RateLimitConfig::public();
        assert_eq!(public.max_requests, 30);

        let auth = RateLimitConfig::authenticated();
        assert_eq!(auth.max_requests, 100);

        let heavy = RateLimitConfig::heavy_operations();
        assert_eq!(heavy.max_requests, 10);
    }

    #[test]
    fn test_token_bucket_creation() {
        let bucket = TokenBucket::new(10.0, 1.0);
        assert_eq!(bucket.capacity, 10.0);
        assert_eq!(bucket.tokens, 10.0);
        assert_eq!(bucket.refill_rate, 1.0);
    }

    #[test]
    fn test_token_bucket_consume() {
        let mut bucket = TokenBucket::new(10.0, 1.0);

        // Should be able to consume up to capacity
        for _ in 0..10 {
            assert!(bucket.try_consume());
        }

        // Should fail when empty
        assert!(!bucket.try_consume());
    }

    #[test]
    fn test_token_bucket_remaining() {
        let mut bucket = TokenBucket::new(10.0, 1.0);

        assert_eq!(bucket.remaining(), 10);

        bucket.try_consume();
        assert_eq!(bucket.remaining(), 9);

        for _ in 0..9 {
            bucket.try_consume();
        }
        assert_eq!(bucket.remaining(), 0);
    }

    #[test]
    fn test_token_bucket_refill() {
        let mut bucket = TokenBucket::new(10.0, 10.0); // 10 tokens per second

        // Consume all tokens
        for _ in 0..10 {
            assert!(bucket.try_consume());
        }
        assert!(!bucket.try_consume());

        // Simulate time passing (0.5 seconds = 5 tokens)
        bucket.last_refill = Instant::now() - Duration::from_millis(500);
        bucket.refill();

        // Should have ~5 tokens now
        assert!(bucket.remaining() >= 4 && bucket.remaining() <= 5);
    }

    #[test]
    fn test_rate_limit_middleware_creation() {
        let config = RateLimitConfig::default();
        let middleware = RateLimitMiddleware::new(config);

        assert_eq!(middleware.config.max_requests, 100);
    }

    #[test]
    fn test_rate_limit_check() {
        let config = RateLimitConfig::new(10, 60, 5);
        let middleware = RateLimitMiddleware::new(config);

        // First 5 requests should succeed (burst capacity)
        for i in 0..5 {
            let result = middleware.check_rate_limit("test-key");
            assert!(result.is_ok(), "Request {} should succeed", i + 1);

            let info = result.unwrap();
            assert_eq!(info.limit, 10);
            assert!(info.retry_after.is_none());
        }

        // 6th request should fail (burst exhausted, need to wait for refill)
        let result = middleware.check_rate_limit("test-key");
        assert!(result.is_err(), "Request 6 should be rate limited");

        let info = result.unwrap_err();
        assert_eq!(info.remaining, 0);
        assert!(info.retry_after.is_some());
    }

    #[test]
    fn test_rate_limit_different_keys() {
        let config = RateLimitConfig::new(2, 60, 2);
        let middleware = RateLimitMiddleware::new(config);

        // Different keys should have separate limits
        assert!(middleware.check_rate_limit("key1").is_ok());
        assert!(middleware.check_rate_limit("key2").is_ok());
        assert!(middleware.check_rate_limit("key1").is_ok());
        assert!(middleware.check_rate_limit("key2").is_ok());

        // Both should be exhausted now
        assert!(middleware.check_rate_limit("key1").is_err());
        assert!(middleware.check_rate_limit("key2").is_err());
    }

    #[test]
    fn test_cleanup_old_buckets() {
        let config = RateLimitConfig::default();
        let middleware = RateLimitMiddleware::new(config);

        // Add some buckets
        middleware.check_rate_limit("key1").ok();
        middleware.check_rate_limit("key2").ok();
        middleware.check_rate_limit("key3").ok();

        // Verify they exist
        {
            let buckets = middleware.buckets.lock().unwrap();
            assert_eq!(buckets.len(), 3);
        }

        // Cleanup shouldn't remove recent buckets
        middleware.cleanup_old_buckets();

        {
            let buckets = middleware.buckets.lock().unwrap();
            assert_eq!(buckets.len(), 3);
        }
    }

    #[test]
    fn test_rate_limit_info() {
        let info = RateLimitInfo {
            limit: 100,
            remaining: 50,
            reset_at: 1234567890,
            retry_after: None,
        };

        assert_eq!(info.limit, 100);
        assert_eq!(info.remaining, 50);
        assert_eq!(info.reset_at, 1234567890);
        assert!(info.retry_after.is_none());

        let limited_info = RateLimitInfo {
            limit: 100,
            remaining: 0,
            reset_at: 1234567890,
            retry_after: Some(30),
        };

        assert_eq!(limited_info.remaining, 0);
        assert_eq!(limited_info.retry_after, Some(30));
    }
}
