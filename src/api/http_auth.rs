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
//! * `X-HeliosDB-Authorization: Basic <base64(user:password)>` — an explicit
//!   outer-auth header for BaaS JWT requests. When present it is authoritative;
//!   an invalid value never falls back to the ordinary `Authorization` header,
//!   which remains available for the downstream app JWT.
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
use tokio::sync::Semaphore;

use crate::protocol::postgres::{AuthManager, AuthMethod, InMemoryPasswordStore, PasswordStore, SharedPasswordStore};

const OUTER_AUTHORIZATION_HEADER: &str = "x-heliosdb-authorization";

/// Failed authentications tolerated for ONE peer in one failure window before the
/// policy refuses further attempts from that peer. Ten is small enough to blunt
/// online guessing and large enough not to bite a human who mistypes twice.
pub const MAX_FAILURES: u32 = 10;

/// Default window over which failures are counted.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// Default upper bound on the number of distinct peers tracked at once. Without it,
/// a peer rotating source addresses could grow the limiter map without bound. When
/// full, the entry with the oldest window is evicted; entries are also dropped once
/// their window expires, so the steady-state size is bounded by the attacker's rate.
pub const MAX_TRACKED_PEERS: usize = 4096;

/// Default upper bound on credential verifications running at the same time ACROSS
/// all peers. Password verification (PBKDF2 for SCRAM, an MD5 recomputation for MD5)
/// is deliberately CPU-bound to blunt offline and online guessing, so an unbounded
/// number of concurrent verifications is a trivial CPU-exhaustion vector. Admission
/// is a separate axis from per-peer failure counting: a legitimate burst of VALID
/// requests must not be mistaken for an attack, but the verifier still needs a hard
/// ceiling. A permit is held for the duration of one verification and released by
/// RAII, so a panic in the comparison cannot leak a slot.
pub const MAX_IN_FLIGHT_VERIFICATIONS: usize = 64;

/// Bounded, operator-tunable knobs for the HTTP credential limiter.
///
/// Every field maps to an `[api]` key (see [`crate::config::ApiConfig`]). The
/// defaults are the compile-time constants above; zero is never a valid value and
/// [`HttpRateLimits::validate`] rejects it, because a zero window/count would
/// either disable the limiter silently or lock every peer out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpRateLimits {
    /// Failed credential attempts tolerated for one peer inside `failure_window`.
    pub max_failures: u32,
    /// Sliding window over which failures are counted.
    pub failure_window: Duration,
    /// Cap on the number of distinct peers tracked at once.
    pub max_tracked_peers: usize,
    /// Cap on simultaneous credential verifications across all peers.
    pub max_in_flight: usize,
}

impl Default for HttpRateLimits {
    fn default() -> Self {
        Self {
            max_failures: MAX_FAILURES,
            failure_window: FAILURE_WINDOW,
            max_tracked_peers: MAX_TRACKED_PEERS,
            max_in_flight: MAX_IN_FLIGHT_VERIFICATIONS,
        }
    }
}

impl HttpRateLimits {
    /// Reject the configurations that would make the limiter meaningless or
    /// universally fatal. Zero is the only value that is always wrong: a zero
    /// failure count locks every peer out after the first attempt, a zero window
    /// makes the expiry arithmetic nonsensical, a zero peer cap evicts every entry
    /// immediately, and a zero concurrency cap refuses every verification.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_failures == 0 {
            return Err("[api] http_auth_max_failures must be greater than 0".to_string());
        }
        if self.failure_window.is_zero() {
            return Err("[api] http_auth_failure_window_secs must be greater than 0".to_string());
        }
        if self.max_tracked_peers == 0 {
            return Err("[api] http_auth_max_tracked_peers must be greater than 0".to_string());
        }
        if self.max_in_flight == 0 {
            return Err("[api] http_auth_max_in_flight must be greater than 0".to_string());
        }
        if self.max_in_flight > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(format!(
                "[api] http_auth_max_in_flight must be <= {}",
                tokio::sync::Semaphore::MAX_PERMITS
            ));
        }
        Ok(())
    }

    /// Seconds to advertise in `Retry-After` when a peer is over the limit. Derived
    /// from the CONFIGURED window, never a hard-coded constant, so an operator who
    /// shortens the window is not told to wait longer than that window.
    pub fn retry_after_secs(&self) -> u64 {
        self.failure_window.as_secs().max(1)
    }
}

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
    /// Operator-tunable bounds (peer failures, tracked peers, in-flight verifications).
    limits: HttpRateLimits,
    /// Bounded per-peer failure state.
    peers: Mutex<PeerFailures>,
    /// Separate bounded admission for credential verification. Holding a permit
    /// across the comparison bounds CPU an unauthenticated peer can occupy; the
    /// permit is RAII so it is released on normal return and on unwind.
    in_flight: Arc<Semaphore>,
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
            .field("limits", &self.limits)
            .finish()
    }
}

impl HttpAuthPolicy {
    /// Shared builder. Collapses a trust verifier to `None` (no HTTP credential)
    /// and materialises the in-flight semaphore from the configured bound.
    fn build(
        verifier: Option<Arc<AuthManager>>,
        bearer_token: Option<Arc<str>>,
        allow_insecure_http: bool,
        limits: HttpRateLimits,
    ) -> Self {
        let verifier = verifier.filter(|manager| !matches!(manager.method(), AuthMethod::Trust));
        Self {
            verifier,
            bearer_token,
            allow_insecure_http,
            in_flight: Arc::new(Semaphore::new(limits.max_in_flight)),
            peers: Mutex::new(PeerFailures::default()),
            limits,
        }
    }

    /// The trust-mode policy: no route authentication. Used for loopback/dev binds
    /// and by tests that exercise the router without credentials.
    pub fn disabled() -> Self {
        Self::build(None, None, false, HttpRateLimits::default())
    }

    /// Build the policy around the live wire [`AuthManager`] with the default limits.
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
        Self::build(verifier, bearer_token, allow_insecure_http, HttpRateLimits::default())
    }

    /// As [`HttpAuthPolicy::from_auth_manager`], but with operator-configured limits.
    ///
    /// Fails (rather than silently clamping) when a limit is zero, so a typo in
    /// `[api]` cannot quietly disable the limiter. Callers map the error to a
    /// startup configuration failure.
    pub fn from_auth_manager_with_limits(
        verifier: Option<Arc<AuthManager>>,
        bearer_token: Option<Arc<str>>,
        allow_insecure_http: bool,
        limits: HttpRateLimits,
    ) -> Result<Self, String> {
        limits.validate()?;
        Ok(Self::build(verifier, bearer_token, allow_insecure_http, limits))
    }

    /// Convenience constructor for tests and embedders: build a policy from a single
    /// wire password, seeding the same default users the daemon seeds.
    ///
    /// The wire listener always resolves to a real [`AuthManager`]; prefer
    /// [`HttpAuthPolicy::from_auth_manager`] so the two share one user set.
    pub fn from_wire(password: Option<String>, bearer_token: Option<Arc<str>>, allow_insecure_http: bool) -> Self {
        Self::build(
            Self::verifier_from_password(password),
            bearer_token,
            allow_insecure_http,
            HttpRateLimits::default(),
        )
    }

    /// As [`HttpAuthPolicy::from_wire`], but with operator-configured limits.
    pub fn from_wire_with_limits(
        password: Option<String>,
        bearer_token: Option<Arc<str>>,
        allow_insecure_http: bool,
        limits: HttpRateLimits,
    ) -> Result<Self, String> {
        limits.validate()?;
        Ok(Self::build(
            Self::verifier_from_password(password),
            bearer_token,
            allow_insecure_http,
            limits,
        ))
    }

    fn verifier_from_password(password: Option<String>) -> Option<Arc<AuthManager>> {
        password.map(|password| {
            let mut store = InMemoryPasswordStore::new();
            let _ = store.add_user("postgres", &password);
            let _ = store.add_user("helios", &password);
            Arc::new(AuthManager::with_password_store(
                AuthMethod::ScramSha256,
                SharedPasswordStore::new(store),
            ))
        })
    }

    /// The effective limiter limits (operator config or defaults).
    pub fn limits(&self) -> HttpRateLimits {
        self.limits
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
    /// Order is deliberate:
    /// 1. a peer already over its failure budget is refused without spending a
    ///    verification (the cheapest possible rejection);
    /// 2. a bounded in-flight permit is acquired for the comparison, so concurrent
    ///    unauthenticated verifications cannot exhaust CPU;
    /// 3. the credential is checked;
    /// 4. ONLY a failed check is recorded against the peer — a valid request never
    ///    counts toward the failure budget, and it does NOT clear failures already
    ///    recorded in the window (so a valid request cannot restore an attacker's
    ///    budget mid-window).
    fn verify_peer(&self, peer: PeerKey, headers: &HeaderMap, path: &str) -> Result<(), HttpAuthRejection> {
        // Trust mode has no credential to check; the layer still answers health and
        // preflight, but everything else is passed through.
        if !self.is_required() {
            return Ok(());
        }

        if self.peer_over_limit(peer) {
            return Err(HttpAuthRejection::TooManyAttempts {
                retry_after_secs: self.limits.retry_after_secs(),
            });
        }

        // Bounded in-flight verification admission. RAII: the permit drops at the
        // end of this scope (including on panic), releasing exactly one slot.
        let _permit = match self.in_flight.try_acquire() {
            Ok(permit) => permit,
            // The verifier is saturated. Fail closed with the same 429 shape rather
            // than run an unbounded number of CPU-bound comparisons.
            Err(_) => {
                return Err(HttpAuthRejection::TooManyAttempts {
                    retry_after_secs: self.limits.retry_after_secs(),
                })
            }
        };

        if self.credentials_ok(headers, path) {
            Ok(())
        } else {
            self.record_failure(peer);
            Err(HttpAuthRejection::Unauthorized)
        }
    }

    /// Whether the peer is currently over its failure budget (lock poisoned => fail
    /// closed by treating the peer as over the limit).
    fn peer_over_limit(&self, peer: PeerKey) -> bool {
        match self.peers.lock() {
            Ok(mut peers) => peers.over_limit(peer, &self.limits),
            Err(_) => true,
        }
    }

    /// Record ONE failed credential attempt for the peer. Only called after a
    /// comparison actually failed, so valid traffic never inflates the counter.
    fn record_failure(&self, peer: PeerKey) {
        if let Ok(mut peers) = self.peers.lock() {
            peers.record_failure(peer, &self.limits);
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
        // BaaS auth uses Authorization: Bearer for the application JWT. A
        // dedicated outer header lets both credentials travel on one request.
        // Presence is authoritative, including malformed values: never fall
        // back to a weaker or unrelated header after an explicit failure.
        let raw_value = headers
            .get(OUTER_AUTHORIZATION_HEADER)
            .or_else(|| headers.get(header::AUTHORIZATION));
        let Some(raw) = raw_value.and_then(|v| v.to_str().ok()) else {
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
    /// Too many failed attempts inside the configured failure window (or the
    /// verifier is saturated). `retry_after_secs` is the configured window, not a
    /// hard-coded constant.
    TooManyAttempts { retry_after_secs: u64 },
}

impl HttpAuthRejection {
    fn status(self) -> StatusCode {
        match self {
            HttpAuthRejection::Unauthorized => StatusCode::UNAUTHORIZED,
            HttpAuthRejection::TooManyAttempts { .. } => StatusCode::TOO_MANY_REQUESTS,
        }
    }
}

impl IntoResponse for HttpAuthRejection {
    fn into_response(self) -> Response {
        let message = match self {
            HttpAuthRejection::Unauthorized => "authentication required",
            HttpAuthRejection::TooManyAttempts { .. } => "too many failed authentication attempts; try again later",
        };
        let body = axum::Json(serde_json::json!({
            "error": "Unauthorized",
            "message": message,
        }));
        let mut response = (self.status(), body).into_response();
        match self {
            HttpAuthRejection::Unauthorized => {
                response.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Basic realm=\"heliosdb-nano\""),
                );
            }
            HttpAuthRejection::TooManyAttempts { retry_after_secs } => {
                // `Retry-After` is derived from the configured failure window, so a
                // client is never told to wait longer than the operator configured.
                if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                    response.headers_mut().insert(header::RETRY_AFTER, value);
                }
            }
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
        HeaderValue::from_static("authorization, x-heliosdb-authorization, content-type"),
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
    /// Drop every window that has fully expired. Called under the lock before any
    /// read or write so the map cannot grow with stale entries.
    fn purge(&mut self, window: Duration, now: Instant) {
        self.peers
            .retain(|_, state| now.duration_since(state.window_start) < window);
    }

    /// Whether the peer has spent its failure budget inside the current window.
    /// This never mutates state, so a legitimate request cannot be counted as a
    /// failure just by being checked.
    fn over_limit(&mut self, peer: PeerKey, limits: &HttpRateLimits) -> bool {
        let now = Instant::now();
        self.purge(limits.failure_window, now);
        self.peers
            .get(&peer)
            .is_some_and(|state| state.over_limit(limits.max_failures, limits.failure_window, now))
    }

    /// Record exactly one failed credential comparison for the peer, bounding the
    /// number of tracked peers first.
    fn record_failure(&mut self, peer: PeerKey, limits: &HttpRateLimits) {
        let now = Instant::now();
        self.purge(limits.failure_window, now);

        if self.peers.len() >= limits.max_tracked_peers && !self.peers.contains_key(&peer) {
            if let Some(oldest) = self
                .peers
                .iter()
                .min_by_key(|(_, state)| state.window_start)
                .map(|(key, _)| *key)
            {
                self.peers.remove(&oldest);
            }
        }

        self.peers
            .entry(peer)
            .or_default()
            .record_failure(limits.max_failures, limits.failure_window, now);
    }
}

/// Bounded failure counter: at most `max_failures` inside `window`.
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
    fn over_limit(&self, max_failures: u32, window: Duration, now: Instant) -> bool {
        self.count >= max_failures && now.duration_since(self.window_start) < window
    }

    fn record_failure(&mut self, _max_failures: u32, window: Duration, now: Instant) {
        if now.duration_since(self.window_start) >= window {
            self.window_start = now;
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
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
            Err(HttpAuthRejection::TooManyAttempts {
                retry_after_secs: FAILURE_WINDOW.as_secs()
            }),
            "the {MAX_FAILURES}th failure must arm the limiter"
        );
    }

    /// The limiter must count FAILURES, not attempts: a burst of valid requests
    /// followed by one wrong password must still be a 401, not a 429. Before this,
    /// every attempt (success or failure) was recorded, so a busy legitimate client
    /// could lock itself out.
    #[test]
    fn valid_attempts_are_not_counted_as_failures() {
        let policy = required_policy();
        let right = {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {}", basic("postgres", "correct-horse"))).unwrap(),
            );
            headers
        };
        let wrong = {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {}", basic("postgres", "wrong"))).unwrap(),
            );
            headers
        };

        for _ in 0..(MAX_FAILURES * 4) {
            assert!(policy.verify_peer(PeerKey::Unknown, &right, "/version").is_ok());
        }
        assert_eq!(
            policy.verify_peer(PeerKey::Unknown, &wrong, "/version"),
            Err(HttpAuthRejection::Unauthorized),
            "a failure after many valid requests must not have armed the limiter"
        );
    }

    /// A freshly-armed peer reports the CONFIGURED window in `Retry-After`, not the
    /// compile-time default. An operator who shortens the window must be obeyed.
    #[test]
    fn retry_after_reflects_the_configured_window() {
        let limits = HttpRateLimits {
            max_failures: 1,
            failure_window: Duration::from_secs(7),
            ..HttpRateLimits::default()
        };
        let policy = Arc::new(
            HttpAuthPolicy::from_wire_with_limits(Some("correct-horse".to_string()), None, false, limits).unwrap(),
        );
        let wrong = {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {}", basic("postgres", "wrong"))).unwrap(),
            );
            headers
        };
        assert_eq!(policy.verify(&wrong, "/version"), Err(HttpAuthRejection::Unauthorized));
        assert_eq!(
            policy.verify(&wrong, "/version"),
            Err(HttpAuthRejection::TooManyAttempts { retry_after_secs: 7 })
        );
    }

    /// Zero limits are invalid and must be rejected at construction (the daemon
    /// turns this into a startup config error instead of silently disarming).
    #[test]
    fn zero_rate_limit_knobs_are_rejected() {
        for limits in [
            HttpRateLimits {
                max_failures: 0,
                ..HttpRateLimits::default()
            },
            HttpRateLimits {
                failure_window: Duration::ZERO,
                ..HttpRateLimits::default()
            },
            HttpRateLimits {
                max_tracked_peers: 0,
                ..HttpRateLimits::default()
            },
            HttpRateLimits {
                max_in_flight: 0,
                ..HttpRateLimits::default()
            },
        ] {
            assert!(limits.validate().is_err(), "zero must be rejected: {limits:?}");
            assert!(
                HttpAuthPolicy::from_wire_with_limits(Some("correct-horse".to_string()), None, false, limits).is_err()
            );
        }
    }

    /// The tracked-peer map is bounded: feeding more distinct peers than the cap
    /// never grows it past the cap.
    #[test]
    fn tracked_peers_are_bounded() {
        let limits = HttpRateLimits {
            max_tracked_peers: 8,
            ..HttpRateLimits::default()
        };
        let mut failures = PeerFailures::default();
        let wrong_limits = limits;
        for octet in 0u8..64 {
            let peer = PeerKey::Addr(IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, octet)));
            failures.record_failure(peer, &wrong_limits);
        }
        assert!(
            failures.peers.len() <= limits.max_tracked_peers,
            "peer map must stay bounded, got {}",
            failures.peers.len()
        );
    }

    /// Concurrent valid credentials must all pass: the in-flight verifier bound is
    /// generous enough for a legitimate burst and must not reject valid traffic.
    #[test]
    fn concurrent_valid_credentials_are_controlled_not_blocked() {
        let policy = required_policy();
        let mut handles = Vec::new();
        for _ in 0..16 {
            let policy = Arc::clone(&policy);
            handles.push(std::thread::spawn(move || {
                let mut headers = HeaderMap::new();
                headers.insert(
                    header::AUTHORIZATION,
                    HeaderValue::from_str(&format!("Basic {}", basic("postgres", "correct-horse"))).unwrap(),
                );
                policy.verify_peer(PeerKey::Unknown, &headers, "/version")
            }));
        }
        for handle in handles {
            assert_eq!(handle.join().expect("thread"), Ok(()));
        }
    }

    #[test]
    fn failure_limiter_is_per_peer_and_success_does_not_erase_failures() {
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
        // A success must not erase completed failed attempts from the same
        // window; otherwise a valid request could restore an attacker's budget.
        assert!(policy.verify_peer(attacker, &right, "/version").is_ok());
        for _ in 0..7 {
            assert_eq!(
                policy.verify_peer(attacker, &wrong, "/version"),
                Err(HttpAuthRejection::Unauthorized)
            );
        }
        assert!(matches!(
            policy.verify_peer(attacker, &wrong, "/version"),
            Err(HttpAuthRejection::TooManyAttempts { .. })
        ));
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

    #[tokio::test]
    async fn dedicated_outer_header_composes_with_baas_bearer_and_is_authoritative() {
        let app = attach_http_auth(
            Router::new().route("/auth/v1/user", get(|| async { "user" })),
            required_policy(),
        );
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/auth/v1/user")
                    .header(header::AUTHORIZATION, "Bearer app-jwt")
                    .header(
                        OUTER_AUTHORIZATION_HEADER,
                        format!("Basic {}", basic("postgres", "correct-horse")),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/auth/v1/user")
                    .header(
                        header::AUTHORIZATION,
                        format!("Basic {}", basic("postgres", "correct-horse")),
                    )
                    .header(OUTER_AUTHORIZATION_HEADER, "Basic malformed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
