//! Wire-credential authentication for the daemon HTTP API.
//!
//! P0-1: before this module the listener that `run_http_listener` assembles served
//! the whole BaaS surface (`/v1/*`, `/rest/v1/*`, `/auth/v1/*`, `/realtime/v1`,
//! `/version`, `/docs`, …) with NO authentication, even when the server was started
//! with `--auth scram-sha-256 --password …`. Anyone who could reach the HTTP port
//! could run SQL through `POST /v1/branches/main/query`. This module closes that.
//!
//! The policy is deliberately small and has one rule: **if the wire protocol has a
//! password** (any `--auth` other than `trust`, or `--password`/`--password-file`/
//! `HELIOSDB_PASSWORD` set), then every route on the daemon HTTP listener requires
//! the same secret, with a side-effect-free health exemption. The operator presents
//! it either as
//!
//! * `Authorization: Basic <base64(user:password)>` — verified against the wire
//!   password with a constant-time comparison, or
//! * `Authorization: Bearer <token>` — where `<token>` is the MCP bearer token
//!   (`--mcp-token`), so the MCP endpoint and the BaaS routes share one credential
//!   instead of each inventing its own.
//!
//! The policy is a *stop-gap P0 fix*: Basic auth is only safe over loopback or TLS,
//! so [`http_bind_safety_check`] refuses a non-loopback plaintext bind unless the
//! operator passes `--allow-insecure-http`. It also refuses a non-loopback trust-mode
//! bind, which is the "unauthenticated SQL on :8080" case from the brief.
//!
//! MCP is intentionally *not* special-cased here. The layer wraps the entire
//! assembled router (BaaS + MCP) and runs first, so a request to `/mcp` must satisfy
//! the wire policy before MCP's own [`crate::mcp::McpAuth`] sees it. Neither path can
//! reach an unauthenticated handler.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::Request,
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Router,
};
use base64::Engine as _;
use zeroize::Zeroizing;

/// Failed authentications tolerated in one [`FAILURE_WINDOW`] before the policy
/// refuses further attempts. Ten is small enough to blunt online guessing and large
/// enough not to bite a human who mistypes a password a couple of times.
pub const MAX_FAILURES: u32 = 10;

/// Window over which failures are counted.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// HTTP API authentication policy.
///
/// Built once at startup and shared (behind an `Arc`) by every request. The wire
/// password is kept in a [`Zeroizing`] buffer so it is wiped when the policy drops.
pub struct HttpAuthPolicy {
    /// `Some` when the wire protocol is password-protected; `None` in trust mode.
    /// Presence alone is what makes authentication mandatory — no separate flag.
    password: Option<Zeroizing<String>>,
    /// Optional bearer token (the MCP `--mcp-token`), accepted in addition to Basic.
    bearer_token: Option<Arc<str>>,
    /// `--allow-insecure-http`: permit a non-loopback plaintext bind.
    allow_insecure_http: bool,
    /// Bounded failure counter shared by all requests.
    failures: Mutex<FailureWindow>,
}

impl std::fmt::Debug for HttpAuthPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpAuthPolicy")
            .field("required", &self.is_required())
            .field(
                "password",
                &if self.password.is_some() {
                    "<redacted>"
                } else {
                    "<none>"
                },
            )
            .field("bearer_token", &self.bearer_token.as_ref().map(|_| "<redacted>"))
            .field("allow_insecure_http", &self.allow_insecure_http)
            .finish()
    }
}

impl HttpAuthPolicy {
    /// The trust-mode policy: no route authentication. Used for loopback/dev binds
    /// and by tests that exercise the router without the security layer.
    pub fn disabled() -> Self {
        Self {
            password: None,
            bearer_token: None,
            allow_insecure_http: false,
            failures: Mutex::new(FailureWindow::default()),
        }
    }

    /// Build the policy from the resolved wire credentials.
    ///
    /// `password` is `Some` iff the wire protocol has authentication configured
    /// (the caller already enforced that a non-`trust` method always gets one), and
    /// it is the single source of truth for whether HTTP auth is required.
    pub fn from_wire(password: Option<String>, bearer_token: Option<Arc<str>>, allow_insecure_http: bool) -> Self {
        Self {
            password: password.map(Zeroizing::new),
            bearer_token,
            allow_insecure_http,
            failures: Mutex::new(FailureWindow::default()),
        }
    }

    /// Whether every non-health route must authenticate.
    pub fn is_required(&self) -> bool {
        self.password.is_some()
    }

    /// Whether the operator explicitly opted into a plaintext non-loopback bind.
    pub fn allow_insecure_http(&self) -> bool {
        self.allow_insecure_http
    }

    /// Validate a request's `Authorization` header.
    fn verify(&self, headers: &HeaderMap, path: &str) -> Result<(), HttpAuthRejection> {
        // Trust mode has no credential to check. `attach_http_auth` does not install
        // the layer in that case; this early return keeps the policy meaningful on
        // its own (and in tests).
        if !self.is_required() {
            return Ok(());
        }

        if self.failures.lock().map(|f| f.over_limit()).unwrap_or(true) {
            // Poisoned lock is treated as "locked": fail closed.
            return Err(HttpAuthRejection::TooManyAttempts);
        }

        if self.credentials_ok(headers, path) {
            if let Ok(mut failures) = self.failures.lock() {
                failures.reset();
            }
            return Ok(());
        }

        if let Ok(mut failures) = self.failures.lock() {
            failures.record_failure();
        }
        Err(HttpAuthRejection::Unauthorized)
    }

    /// Constant-time credential check across the Basic and Bearer mechanisms.
    fn credentials_ok(&self, headers: &HeaderMap, path: &str) -> bool {
        let Some(raw) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
            return false;
        };

        if let Some(token) = strip_scheme(raw, "Bearer") {
            // The MCP token is scoped to MCP routes. It must never become a
            // database-wide credential for REST/BaaS endpoints.
            if path != "/mcp" && !path.starts_with("/mcp/") {
                return false;
            }
            if let Some(expected) = &self.bearer_token {
                return constant_time_eq(token.as_bytes(), expected.as_bytes());
            }
            return false;
        }

        if let Some(encoded) = strip_scheme(raw, "Basic") {
            let Some(expected) = &self.password else {
                return false;
            };
            let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
                return false;
            };
            // `user:password` — the username is not secret and the wire protocol
            // seeds every default user with the same password, so only the password
            // is compared, and it is compared in constant time.
            let Some(colon) = decoded.iter().position(|b| *b == b':') else {
                return false;
            };
            let password = decoded.get(colon + 1..).unwrap_or(&[]);
            return constant_time_eq(password, expected.as_bytes());
        }

        false
    }
}

/// Reject reason, rendered as a 401 (with `WWW-Authenticate`) or a 429.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpAuthRejection {
    /// No credentials, or credentials that did not match.
    Unauthorized,
    /// Too many failed attempts inside [`FAILURE_WINDOW`].
    TooManyAttempts,
}

impl HttpAuthRejection {
    fn status(self) -> StatusCode {
        match self {
            HttpAuthRejection::Unauthorized => StatusCode::UNAUTHORIZED,
            HttpAuthRejection::TooManyAttempts => StatusCode::TOO_MANY_REQUESTS,
        }
    }
}

impl IntoResponse for HttpAuthRejection {
    fn into_response(self) -> Response {
        let message = match self {
            HttpAuthRejection::Unauthorized => "authentication required",
            HttpAuthRejection::TooManyAttempts => "too many failed authentication attempts; try again later",
        };
        let body = axum::Json(serde_json::json!({
            "error": "Unauthorized",
            "message": message,
        }));
        let mut response = (self.status(), body).into_response();
        if self == HttpAuthRejection::Unauthorized {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"heliosdb-nano\""),
            );
        } else {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
        }
        response
    }
}

/// Split `"<Scheme> <credentials>"`, matching the scheme case-insensitively.
///
/// Unknown schemes fall through to `None` so the request is rejected rather than
/// silently treated as authenticated.
fn strip_scheme<'a>(raw: &'a str, scheme: &str) -> Option<&'a str> {
    let (name, rest) = raw.split_once(' ')?;
    if name.eq_ignore_ascii_case(scheme) {
        Some(rest)
    } else {
        None
    }
}

/// Compare two byte strings without an early exit, so timing does not reveal how
/// many leading bytes matched. The length is folded into the difference instead of
/// being returned early (a length leak is not a secret leak here, but this is both
/// shorter and stricter).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let len = a.len().max(b.len());
    for i in 0..len {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

/// Bounded failure counter: at most [`MAX_FAILURES`] inside [`FAILURE_WINDOW`].
#[derive(Debug)]
struct FailureWindow {
    count: u32,
    window_start: Instant,
}

impl Default for FailureWindow {
    fn default() -> Self {
        Self {
            count: 0,
            window_start: Instant::now(),
        }
    }
}

impl FailureWindow {
    fn over_limit(&self) -> bool {
        self.count >= MAX_FAILURES && self.window_start.elapsed() < FAILURE_WINDOW
    }

    fn record_failure(&mut self) {
        if self.window_start.elapsed() >= FAILURE_WINDOW {
            self.window_start = Instant::now();
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
    }

    fn reset(&mut self) {
        self.count = 0;
        self.window_start = Instant::now();
    }
}

/// Return `true` for the only routes that may skip authentication: the
/// side-effect-free health endpoint(s) and CORS preflight.
///
/// `/` is the daemon's health alias added in `run_http_listener` and `/health` is
/// mounted by `ApiServer`. Both answer a static `{"status":"ok"}` and expose nothing
/// version- or data-sensitive. `OPTIONS` is exempt so the CORS preflight can complete
/// (preflights never carry credentials); the actual request still authenticates.
fn is_health_exempt(method: &Method, path: &str) -> bool {
    if method == Method::OPTIONS {
        return true;
    }
    method == Method::GET && (path == "/health" || path == "/")
}

/// Wrap an already-assembled router with the wire-credential policy.
///
/// A disabled policy is a no-op, so trust-mode/dev callers keep their old surface.
/// The layer is applied last, outermost, so it runs before every route (MCP
/// included) and before the fallback.
pub fn attach_http_auth<S>(router: Router<S>, policy: Arc<HttpAuthPolicy>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    if !policy.is_required() {
        return router;
    }
    router.layer(axum::middleware::from_fn(move |req: Request, next: Next| {
        let policy = Arc::clone(&policy);
        async move { guard(policy, req, next).await }
    }))
}

async fn guard(policy: Arc<HttpAuthPolicy>, req: Request, next: Next) -> Response {
    if is_health_exempt(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    match policy.verify(req.headers(), req.uri().path()) {
        Ok(()) => next.run(req).await,
        Err(rejection) => rejection.into_response(),
    }
}

/// Refuse a non-loopback plaintext HTTP bind that would expose unauthenticated SQL
/// or Basic credentials to the network.
///
/// * loopback (`127.0.0.0/8`, `::1`) is always allowed;
/// * `--allow-insecure-http` is an explicit operator override and is always allowed;
/// * otherwise a trust-mode bind is refused (the P0 case), and a password-protected
///   bind is refused too because the listener serves plaintext — configuring TLS on
///   the *wire* listener does not make this one HTTPS.
pub fn http_bind_safety_check(addr: SocketAddr, policy: &HttpAuthPolicy, tls_configured: bool) -> Result<(), String> {
    if is_loopback(addr.ip()) || policy.allow_insecure_http {
        return Ok(());
    }

    if !policy.is_required() {
        return Err(format!(
            "refusing to bind the HTTP API on non-loopback address {addr}: the wire protocol is \
             in trust mode, so every HTTP route (including SQL execution) would be unauthenticated. \
             Start with --auth scram-sha-256 and a password, bind --http-listen 127.0.0.1, or pass \
             --allow-insecure-http to accept the exposure."
        ));
    }

    let tls_note = if tls_configured {
        " (TLS is configured for the wire protocol, but the HTTP API listener is still plaintext)"
    } else {
        ""
    };
    Err(format!(
        "refusing to expose HTTP authentication over plaintext on non-loopback address {addr}{tls_note}: \
         credentials would cross the network in the clear. Bind --http-listen 127.0.0.1, terminate TLS \
         in front of the HTTP API, or pass --allow-insecure-http to accept the exposure."
    ))
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use base64::Engine as _;
    use tower::ServiceExt;

    fn basic(password: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(format!("postgres:{password}"))
    }

    fn required_policy() -> Arc<HttpAuthPolicy> {
        Arc::new(HttpAuthPolicy::from_wire(
            Some("correct-horse".to_string()),
            Some(Arc::from("mcp-token-xyz")),
            false,
        ))
    }

    #[test]
    fn constant_time_eq_matches_only_equal_slices() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreu"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn disabled_policy_is_not_required_and_accepts_anything() {
        let policy = HttpAuthPolicy::disabled();
        assert!(!policy.is_required());
        assert!(policy.verify(&HeaderMap::new(), "/version").is_ok());
    }

    #[test]
    fn required_policy_rejects_missing_and_wrong_credentials() {
        let policy = required_policy();
        assert!(policy.is_required());
        assert_eq!(
            policy.verify(&HeaderMap::new(), "/version"),
            Err(HttpAuthRejection::Unauthorized)
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {}", basic("wrong"))).unwrap(),
        );
        assert_eq!(
            policy.verify(&headers, "/version"),
            Err(HttpAuthRejection::Unauthorized)
        );
    }

    #[test]
    fn required_policy_accepts_the_wire_password_and_the_mcp_token() {
        let policy = required_policy();

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {}", basic("correct-horse"))).unwrap(),
        );
        assert!(policy.verify(&headers, "/version").is_ok());

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer mcp-token-xyz"));
        assert!(policy.verify(&headers, "/mcp").is_ok());
    }

    #[test]
    fn required_policy_locks_out_after_bounded_failures() {
        let policy = required_policy();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {}", basic("wrong"))).unwrap(),
        );

        for _ in 0..MAX_FAILURES {
            assert_eq!(
                policy.verify(&headers, "/version"),
                Err(HttpAuthRejection::Unauthorized)
            );
        }
        assert_eq!(
            policy.verify(&headers, "/version"),
            Err(HttpAuthRejection::TooManyAttempts),
            "the {MAX_FAILURES}th failure must arm the limiter"
        );
    }

    #[test]
    fn loopback_binds_are_always_allowed() {
        let policy = required_policy();
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert!(http_bind_safety_check(addr, &policy, false).is_ok());
        let addr6: SocketAddr = "[::1]:8080".parse().unwrap();
        assert!(http_bind_safety_check(addr6, &policy, false).is_ok());
    }

    #[test]
    fn non_loopback_trust_mode_bind_is_refused() {
        let policy = HttpAuthPolicy::disabled();
        let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let err = http_bind_safety_check(addr, &policy, false).unwrap_err();
        assert!(err.contains("trust mode"), "got: {err}");
    }

    #[test]
    fn non_loopback_plaintext_auth_bind_is_refused() {
        let policy = required_policy();
        let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let err = http_bind_safety_check(addr, &policy, true).unwrap_err();
        assert!(err.contains("plaintext"), "got: {err}");
    }

    #[test]
    fn allow_insecure_http_permits_non_loopback() {
        let policy = Arc::new(HttpAuthPolicy::from_wire(Some("correct-horse".to_string()), None, true));
        let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        assert!(http_bind_safety_check(addr, &policy, false).is_ok());
    }

    #[tokio::test]
    async fn middleware_health_is_exempt_but_other_routes_are_not() {
        let app = attach_http_auth(
            Router::new()
                .route("/health", get(|| async { "ok" }))
                .route("/version", get(|| async { "v" })),
            required_policy(),
        );

        let health = app
            .clone()
            .oneshot(HttpRequest::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        let version = app
            .clone()
            .oneshot(HttpRequest::builder().uri("/version").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(version.status(), StatusCode::UNAUTHORIZED);
        assert!(
            version.headers().contains_key(header::WWW_AUTHENTICATE),
            "401 must carry WWW-Authenticate"
        );

        let version = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/version")
                    .header(header::AUTHORIZATION, format!("Basic {}", basic("correct-horse")))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(version.status(), StatusCode::OK);
    }
}
