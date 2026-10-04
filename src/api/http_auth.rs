//! Wire-credential authentication for the daemon HTTP API.
//!
//! P0-1: before this module the listener that `run_http_listener` assembles served
//! the whole BaaS surface (`/v1/*`, `/rest/v1/*`, `/auth/v1/*`, `/realtime/v1`,
//! `/version`, `/docs`, …) with NO authentication, even when the server was started
//! with `--auth scram-sha-256 --password …`. Anyone who could reach the HTTP port
//! could run SQL through `POST /v1/branches/main/query`. This module closes that.
//!
//! The policy is deliberately small and has one rule: **if the wire listener has a
//! credential** (any `--auth` other than `trust`), then every route on the daemon
//! HTTP listener requires it, except the side-effect-free health endpoint and CORS
//! preflight. The operator presents it either as
//!
//! * `Authorization: Basic <base64(user:password)>` — verified by the SAME
//!   [`AuthManager`] the PostgreSQL wire uses (`AuthManager::verify_basic`), so the
//!   username is checked, unknown users fail closed, and SCRAM / cleartext / MD5
//!   semantics are not re-implemented here. No credential comparison lives in this
//!   file: the bearer-token check uses `subtle`'s constant-time primitive.
//! * `Authorization: Bearer <token>` — where `<token>` is the MCP bearer token
//!   (`--mcp-token`), accepted only on MCP routes.
//!
//! The policy is a *stop-gap P0 fix*. Basic auth is only safe over loopback or TLS,
//! so [`http_bind_safety_check`] refuses EVERY non-loopback plaintext bind:
//! unauthenticated trust mode unless `--allow-insecure-http` is passed, and
//! authenticated HTTP Basic unconditionally (that override only covers trust mode).
//! The HTTP listener never terminates TLS, so a wire-protocol `--tls-cert` does not
//! make it HTTPS.
//!
//! `/health` and `OPTIONS` are answered by this layer itself — a fixed, status-only
//! body and a fixed preflight — instead of being forwarded to a downstream handler.
//! That keeps the daemon's health contract free of version/branch data no matter
//! what `ApiServer` later chooses to mount.
//!
//! MCP is intentionally *not* special-cased. The layer wraps the entire assembled
//! router (BaaS + MCP) and runs first, so a request to `/mcp` must satisfy the wire
//! policy before MCP's own [`crate::mcp::McpAuth`] sees it.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{ConnectInfo, Request},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Router,
};
use base64::Engine as _;
use subtle::ConstantTimeEq;

use crate::protocol::postgres::{AuthManager, AuthMethod, InMemoryPasswordStore, PasswordStore, SharedPasswordStore};

/// Failed authentications tolerated for ONE peer in one [`FAILURE_WINDOW`] before
/// the policy refuses further attempts from that peer. Ten is small enough to blunt
/// online guessing and large enough not to bite a human who mistypes twice.
pub const MAX_FAILURES: u32 = 10;

/// Window over which failures are counted.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// Upper bound on the number of distinct peers tracked at once. Without it, a peer
/// rotating source addresses could grow the limiter map without bound. When full,
/// the entry with the oldest window is evicted; entries are also dropped once their
/// window expires, so the steady-state size is bounded by the attacker's rate.
pub const MAX_TRACKED_PEERS: usize = 4096;

/// Default HTTP listener bind address.
///
/// Deliberately loopback and NOT the database listener's `--listen`: starting the
/// database on `0.0.0.0` must never silently publish the HTTP surface on every
/// interface. Operators opt out explicitly with `--http-listen`.
pub const HTTP_DEFAULT_LISTEN: &str = "127.0.0.1";

/// Resolve the HTTP listener bind address from an explicit `--http-listen` value,
/// defaulting to [`HTTP_DEFAULT_LISTEN`] when none was given.
pub fn resolve_http_listen(explicit: Option<&str>) -> String {
    explicit.unwrap_or(HTTP_DEFAULT_LISTEN).to_string()
}

/// HTTP API authentication policy.
///
/// Built once at startup and shared (behind an `Arc`) by every request. The
/// credential verifier is the live PostgreSQL [`AuthManager`], so HTTP and the wire
/// protocol can never disagree about who a valid user is.
pub struct HttpAuthPolicy {
    /// `Some` exactly when the wire listener is password-protected. The verifier is
    /// the same object the PostgreSQL listener uses; `None` in trust mode.
    verifier: Option<Arc<AuthManager>>,
    /// Optional bearer token (the MCP `--mcp-token`), accepted only on `/mcp*`.
    bearer_token: Option<Arc<str>>,
    /// `--allow-insecure-http`: permit a non-loopback plaintext bind in trust mode.
    allow_insecure_http: bool,
    /// Bounded per-peer failure state.
    peers: Mutex<PeerFailures>,
}

impl std::fmt::Debug for HttpAuthPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpAuthPolicy")
            .field("required", &self.is_required())
            .field(
                "verifier",
                &if self.verifier.is_some() {
                    "<AuthManager>"
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
    /// and by tests that exercise the router without credentials.
    pub fn disabled() -> Self {
        Self {
            verifier: None,
            bearer_token: None,
            allow_insecure_http: false,
            peers: Mutex::new(PeerFailures::default()),
        }
    }

    /// Build the policy around the live wire [`AuthManager`].
    ///
    /// A verifier whose method is [`AuthMethod::Trust`] carries no credential and is
    /// collapsed to `None`, so a trust-mode daemon does not accidentally require
    /// Basic auth (the default `postgres`/`admin` users exist but are not a wire
    /// secret).
    pub fn from_auth_manager(
        verifier: Option<Arc<AuthManager>>,
        bearer_token: Option<Arc<str>>,
        allow_insecure_http: bool,
    ) -> Self {
        let verifier = verifier.filter(|manager| !matches!(manager.method(), AuthMethod::Trust));
        Self {
            verifier,
            bearer_token,
            allow_insecure_http,
            peers: Mutex::new(PeerFailures::default()),
        }
    }

    /// Convenience constructor for tests and embedders: build a policy from a single
    /// wire password, seeding the same default users the daemon seeds.
    ///
    /// The wire listener always resolves to a real [`AuthManager`]; prefer
    /// [`HttpAuthPolicy::from_auth_manager`] so the two share one user set.
    pub fn from_wire(password: Option<String>, bearer_token: Option<Arc<str>>, allow_insecure_http: bool) -> Self {
        let verifier = password.map(|password| {
            let mut store = InMemoryPasswordStore::new();
            let _ = store.add_user("postgres", &password);
            let _ = store.add_user("helios", &password);
            Arc::new(AuthManager::with_password_store(
                AuthMethod::ScramSha256,
                SharedPasswordStore::new(store),
            ))
        });
        Self::from_auth_manager(verifier, bearer_token, allow_insecure_http)
    }

    /// Whether every non-health route must authenticate.
    pub fn is_required(&self) -> bool {
        self.verifier.is_some()
    }

    /// Whether the operator explicitly opted into a plaintext non-loopback bind.
    pub fn allow_insecure_http(&self) -> bool {
        self.allow_insecure_http
    }

    /// Validate a request's `Authorization` header for the anonymous test-only peer.
    #[cfg(test)]
    fn verify(&self, headers: &HeaderMap, path: &str) -> Result<(), HttpAuthRejection> {
        self.verify_peer(PeerKey::Unknown, headers, path)
    }

    /// Validate a request's `Authorization` header for a specific peer.
    ///
    /// Admission (the rate-limit check + the attempt record) happens in one critical
    /// section before the credential comparison; a success resets only this peer.
    fn verify_peer(&self, peer: PeerKey, headers: &HeaderMap, path: &str) -> Result<(), HttpAuthRejection> {
        // Trust mode has no credential to check; the layer still answers health and
        // preflight, but everything else is passed through.
        if !self.is_required() {
            return Ok(());
        }

        if !self.admit(peer) {
            return Err(HttpAuthRejection::TooManyAttempts);
        }

        if self.credentials_ok(headers, path) {
            self.reset(peer);
            Ok(())
        } else {
            Err(HttpAuthRejection::Unauthorized)
        }
    }

    /// Atomically check the peer's window and record this attempt. `false` means the
    /// peer is over the limit (or the lock is poisoned — fail closed).
    fn admit(&self, peer: PeerKey) -> bool {
        match self.peers.lock() {
            Ok(mut peers) => peers.admit(peer),
            Err(_) => false,
        }
    }

    /// Reset only the named peer's window after a successful authentication.
    fn reset(&self, peer: PeerKey) {
        if let Ok(mut peers) = self.peers.lock() {
            peers.reset(peer);
        }
    }

    /// Credential check across the Basic and Bearer mechanisms.
    ///
    /// Basic delegates to the wire [`AuthManager`] (`verify_basic`) so the username
    /// is significant, unknown users fail closed, and no comparison is hand-rolled.
    /// Bearer uses `subtle`'s constant-time slice comparison.
    fn credentials_ok(&self, headers: &HeaderMap, path: &str) -> bool {
        let Some(verifier) = &self.verifier else {
            return false;
        };
        let Some(raw) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
            return false;
        };

        if let Some(token) = strip_scheme(raw, "Bearer") {
            // The MCP token is scoped to MCP routes. It must never become a
            // database-wide credential for REST/BaaS endpoints.
            if path != "/mcp" && !path.starts_with("/mcp/") {
                return false;
            }
            return match &self.bearer_token {
                Some(expected) => constant_time_bytes_eq(token.as_bytes(), expected.as_bytes()),
                None => false,
            };
        }

        if let Some(encoded) = strip_scheme(raw, "Basic") {
            let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
                return false;
            };
            let Some(colon) = decoded.iter().position(|b| *b == b':') else {
                return false;
            };
            let (username, password) = decoded.split_at(colon);
            let password = password.get(1..).unwrap_or(&[]);
            let (Ok(username), Ok(password)) = (std::str::from_utf8(username), std::str::from_utf8(password)) else {
                return false;
            };
            if username.is_empty() {
                return false;
            }
            return verifier.verify_basic(username, password);
        }

        false
    }
}

/// Compare two byte strings in constant time.
///
/// Uses the `subtle` crate's audited `ConstantTimeEq`; the slice implementation
/// folds the length difference into the result instead of returning early.
fn constant_time_bytes_eq(a: &[u8], b: &[u8]) -> bool {
    bool::from(a.ct_eq(b))
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

/// The fixed, status-only health body. Never forwards to whatever `/health` handler
/// a downstream router happens to mount, so version/branch data cannot leak here.
fn fixed_health_response() -> Response {
    (StatusCode::OK, axum::Json(serde_json::json!({ "status": "ok" }))).into_response()
}

/// The fixed, side-effect-free CORS preflight response. Never forwards to a handler.
fn fixed_preflight_response() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::ALLOW,
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type"),
    );
    headers.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    response
}

/// Wrap an already-assembled router with the wire-credential policy.
///
/// The layer is installed even in trust mode so that `/health` and `OPTIONS` are
/// always answered by this outer layer (fixed bodies) and never forwarded; in trust
/// mode every other route passes straight through.
pub fn attach_http_auth<S>(router: Router<S>, policy: Arc<HttpAuthPolicy>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(axum::middleware::from_fn(move |req: Request, next: Next| {
        let policy = Arc::clone(&policy);
        async move { guard(policy, req, next).await }
    }))
}

async fn guard(policy: Arc<HttpAuthPolicy>, req: Request, next: Next) -> Response {
    // Requirement P0-5: preflight never reaches a handler and carries no side effect.
    if req.method() == Method::OPTIONS {
        return fixed_preflight_response();
    }

    let path = req.uri().path();

    // Requirement P0-2: health is answered here, not by `ApiServer::health_check`.
    // `/` is the daemon's health alias.
    if req.method() == Method::GET && (path == "/health" || path == "/") {
        return fixed_health_response();
    }

    if !policy.is_required() {
        return next.run(req).await;
    }

    let peer = peer_key(&req);
    match policy.verify_peer(peer, req.headers(), path) {
        Ok(()) => next.run(req).await,
        Err(rejection) => rejection.into_response(),
    }
}

/// Extract the peer key from the connection info attached by
/// `into_make_service_with_connect_info`. Missing info (e.g. a router exercised with
/// `tower::oneshot` in a unit test) collapses to a single shared bucket.
fn peer_key(req: &Request) -> PeerKey {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(PeerKey::Unknown, |connect_info| PeerKey::Addr(connect_info.0.ip()))
}

/// Limit key. Address is the peer IP (port excluded, so rotating source ports cannot
/// evade the limiter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PeerKey {
    Addr(IpAddr),
    Unknown,
}

/// Bounded per-peer failure state.
#[derive(Debug, Default)]
struct PeerFailures {
    peers: HashMap<PeerKey, FailureWindow>,
}

impl PeerFailures {
    /// Atomically check the peer's window and record this attempt.
    fn admit(&mut self, peer: PeerKey) -> bool {
        let now = Instant::now();
        self.peers
            .retain(|_, window| now.duration_since(window.window_start) < FAILURE_WINDOW);

        if self.peers.len() >= MAX_TRACKED_PEERS && !self.peers.contains_key(&peer) {
            if let Some(oldest) = self
                .peers
                .iter()
                .min_by_key(|(_, window)| window.window_start)
                .map(|(key, _)| *key)
            {
                self.peers.remove(&oldest);
            }
        }

        let window = self.peers.entry(peer).or_default();
        if window.over_limit() {
            return false;
        }
        window.record_failure();
        true
    }

    /// Reset only the named peer.
    fn reset(&mut self, peer: PeerKey) {
        if let Some(window) = self.peers.get_mut(&peer) {
            window.reset();
        }
    }
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

/// Refuse a non-loopback plaintext HTTP bind.
///
/// * loopback (`127.0.0.0/8`, `::1`) is always allowed;
/// * an actual HTTPS listener (`https_terminated`) is always allowed;
/// * a non-loopback bind that authenticates every request is refused **always** —
///   Basic credentials over plaintext would cross the network in the clear, and
///   `--allow-insecure-http` does not cover authenticated traffic;
/// * a non-loopback trust-mode bind is refused unless `--allow-insecure-http` was
///   passed (the explicit P0 override for an unauthenticated dev/exposure case).
pub fn http_bind_safety_check(addr: SocketAddr, policy: &HttpAuthPolicy, https_terminated: bool) -> Result<(), String> {
    if is_loopback(addr.ip()) || https_terminated {
        return Ok(());
    }

    if policy.is_required() {
        return Err(format!(
            "refusing to expose authenticated HTTP over plaintext on non-loopback address {addr}: \
             HTTP credentials would cross the network in the clear. --allow-insecure-http does not \
             apply to authenticated HTTP; bind --http-listen 127.0.0.1 or terminate TLS in front of \
             the HTTP API."
        ));
    }

    if policy.allow_insecure_http() {
        return Ok(());
    }

    Err(format!(
        "refusing to bind the HTTP API on non-loopback address {addr}: the wire protocol is \
         in trust mode, so every HTTP route (including SQL execution) would be unauthenticated. \
         Start with --auth scram-sha-256 and a password, bind --http-listen 127.0.0.1, or pass \
         --allow-insecure-http to accept the exposure."
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

    fn basic(username: &str, password: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    }

    fn required_policy() -> Arc<HttpAuthPolicy> {
        Arc::new(HttpAuthPolicy::from_wire(
            Some("correct-horse".to_string()),
            Some(Arc::from("mcp-token-xyz")),
            false,
        ))
    }

    #[test]
    fn default_http_listen_is_loopback_unless_explicit() {
        assert_eq!(resolve_http_listen(None), "127.0.0.1");
        assert_eq!(resolve_http_listen(Some("0.0.0.0")), "0.0.0.0");
        assert_eq!(resolve_http_listen(Some("127.0.0.1")), "127.0.0.1");
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
            HeaderValue::from_str(&format!("Basic {}", basic("postgres", "wrong"))).unwrap(),
        );
        assert_eq!(
            policy.verify(&headers, "/version"),
            Err(HttpAuthRejection::Unauthorized)
        );
    }

    #[test]
    fn basic_auth_is_username_aware() {
        let policy = required_policy();

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {}", basic("postgres", "correct-horse"))).unwrap(),
        );
        assert!(policy.verify(&headers, "/version").is_ok());

        // Right password, unknown user must fail closed.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {}", basic("nobody", "correct-horse"))).unwrap(),
        );
        assert_eq!(
            policy.verify(&headers, "/version"),
            Err(HttpAuthRejection::Unauthorized)
        );

        // Empty username must fail closed.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {}", basic("", "correct-horse"))).unwrap(),
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
            HeaderValue::from_str(&format!("Basic {}", basic("postgres", "correct-horse"))).unwrap(),
        );
        assert!(policy.verify(&headers, "/version").is_ok());

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer mcp-token-xyz"));
        assert!(policy.verify(&headers, "/mcp").is_ok());
        assert!(
            policy.verify(&headers, "/version").is_err(),
            "the MCP bearer token must not authenticate BaaS routes"
        );
    }

    #[test]
    fn required_policy_locks_out_after_bounded_failures() {
        let policy = required_policy();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {}", basic("postgres", "wrong"))).unwrap(),
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
    fn failure_limiter_is_per_peer_and_success_resets_only_that_peer() {
        let policy = required_policy();
        let attacker: PeerKey = PeerKey::Addr("203.0.113.7".parse().unwrap());
        let legit: PeerKey = PeerKey::Addr("127.0.0.1".parse().unwrap());
        let wrong = {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {}", basic("postgres", "wrong"))).unwrap(),
            );
            headers
        };
        let right = {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {}", basic("postgres", "correct-horse"))).unwrap(),
            );
            headers
        };

        for _ in 0..3 {
            assert!(policy.verify_peer(attacker, &wrong, "/version").is_err());
        }
        // The legitimate peer is untouched by the attacker's failures.
        assert!(policy.verify_peer(legit, &right, "/version").is_ok());
        // A success resets only the attacker's own window.
        assert!(policy.verify_peer(attacker, &right, "/version").is_ok());
        // Still well under the limit after the reset.
        assert_eq!(
            policy.verify_peer(attacker, &wrong, "/version"),
            Err(HttpAuthRejection::Unauthorized)
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
    fn non_loopback_trust_mode_bind_is_refused_but_override_allowed() {
        let policy = HttpAuthPolicy::disabled();
        let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let err = http_bind_safety_check(addr, &policy, false).unwrap_err();
        assert!(err.contains("trust mode"), "got: {err}");

        let override_policy = Arc::new(HttpAuthPolicy::from_wire(None, None, true));
        assert!(http_bind_safety_check(addr, &override_policy, false).is_ok());
    }

    #[test]
    fn authenticated_non_loopback_plaintext_is_refused_even_with_allow_insecure() {
        let policy = Arc::new(HttpAuthPolicy::from_wire(Some("correct-horse".to_string()), None, true));
        let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let err = http_bind_safety_check(addr, &policy, false).unwrap_err();
        assert!(err.contains("plaintext"), "got: {err}");

        // The override does not apply; an actual HTTPS listener would be allowed.
        assert!(http_bind_safety_check(addr, &policy, true).is_ok());
    }

    #[tokio::test]
    async fn middleware_health_is_fixed_and_other_routes_are_not() {
        let app = attach_http_auth(
            Router::new()
                .route(
                    "/health",
                    get(|| async { axum::Json(serde_json::json!({"status": "leak", "version": "x"})) }),
                )
                .route("/version", get(|| async { "v" })),
            required_policy(),
        );

        let health = app
            .clone()
            .oneshot(HttpRequest::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(health.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "ok");
        assert!(
            body.get("version").is_none(),
            "health must not forward version data: {body}"
        );

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
                    .header(
                        header::AUTHORIZATION,
                        format!("Basic {}", basic("postgres", "correct-horse")),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(version.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn options_returns_fixed_preflight_without_hitting_the_handler() {
        let app = attach_http_auth(
            Router::new().route("/version", axum::routing::any(|| async { "handler-ran" })),
            required_policy(),
        );
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::OPTIONS)
                    .uri("/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(resp.headers().contains_key(header::ACCESS_CONTROL_ALLOW_METHODS));
    }
}
