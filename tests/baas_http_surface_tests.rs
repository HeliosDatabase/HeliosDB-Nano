//! The BaaS HTTP surface the README advertises is actually mounted (task #115).
//!
//! WHAT WAS BROKEN. `README.md` line 7 sells a "built-in BaaS layer (Auth, REST API,
//! Realtime)" and line 393 shows a working `curl -X POST .../auth/v1/signup`. On the shipped
//! binary every one of those endpoints returned **404**: `run_http_listener` in
//! `src/main.rs` mounted `/` and `/health` and nothing else, so `ApiServer`'s router — REST,
//! Auth, Realtime, Swagger, `/version` — existed only as a library API that no caller ever
//! reached. Verified by running the binary and probing, not by reading code.
//!
//! Three further no-caller defects sat behind it, each of which alone would have kept the
//! feature broken after mounting:
//!   * `AuthBridge::new` / `ApiServer::with_auth_bridge` — never called, so `state.auth_bridge`
//!     was always `None` and every `/auth/v1/*` handler returned 503 "auth_not_enabled".
//!   * `AuthBridge::bootstrap` — creates `_auth_users` / `_auth_refresh_tokens`, called only by
//!     its own two unit tests, so the first real signup died with
//!     "Table '_auth_users' does not exist".
//!   * FOUR hardcoded fallback JWT secrets (`heliosdb-jwt-secret-change-in-production`,
//!     `your-super-secret-jwt-key`, and `default-secret-change-in-production` twice). Harmless
//!     while nothing signed tokens; a forgeable-token hazard the moment the layer went live.
//!     All four are now a per-process CSPRNG value — there is no shipped signing key.
//!
//! A FOURTH one outlived that fix, and this file now guards it too. `OAuthRegistry` and the
//! whole Authorization Code + PKCE flow in `src/api/oauth.rs` were complete, and `src/config.rs`
//! parsed `[[api.oauth_providers]]` into `ApiConfig::oauth_providers` — but nothing ever called
//! `ApiServer::with_oauth_registry`, so `AppState.oauth_registry` was always `None` and
//! `GET /auth/v1/authorize?provider=google` answered **503 "oauth_not_configured"** no matter
//! what the operator wrote in `config.toml`, while the README and the website advertised OAuth
//! sign-in. It survived #115 because that fix left TWO assemblies standing: this file built its
//! own router and `run_http_listener` built a second one inline in `src/main.rs`, so a green
//! suite here proved nothing about the shipped binary. Both sides now go through
//! `ApiServer::from_config`, which is the seam the tests below exercise.
//!
//! WHAT THIS FILE GUARDS. That the routes are MOUNTED. A regression here is invisible to every
//! other suite: the library-level handler tests kept passing throughout, because the handlers
//! were always fine — it was the wiring that did not exist. Asserting "not 404" is the whole
//! point; asserting handler behaviour is what the existing suites already do.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use heliosdb_nano::api::ApiServer;
use heliosdb_nano::config::{ApiConfig, OAuthProviderConfig};
use heliosdb_nano::EmbeddedDatabase;
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceExt;

/// Obviously fake credentials. Nothing here talks to a provider; every assertion is about
/// the redirect this server builds from the operator's config.
const CLIENT_ID: &str = "test-client-id";
const CLIENT_SECRET: &str = "test-client-secret";
const REDIRECT_URI: &str = "http://localhost:8080/auth/v1/callback";

/// An `ApiConfig` with a fixed key and one `[[api.oauth_providers]]` entry per name.
///
/// `jwt_secret_is_ephemeral` is false so a correctly configured server produces an EMPTY
/// warning list — see `a_good_config_produces_no_warnings`.
fn api_config(provider_names: &[&str]) -> ApiConfig {
    ApiConfig {
        // A per-test key. The point of the assertions here is the ROUTE, not the crypto.
        jwt_secret: heliosdb_nano::config::generate_jwt_secret(),
        jwt_secret_is_ephemeral: false,
        anon_key: None,
        service_role_key: None,
        oauth_providers: provider_names
            .iter()
            .map(|name| OAuthProviderConfig {
                name: (*name).to_string(),
                client_id: CLIENT_ID.to_string(),
                client_secret: CLIENT_SECRET.to_string(),
                redirect_uri: REDIRECT_URI.to_string(),
            })
            .collect(),
    }
}

/// Boot a router through the SAME assembly `heliosdb-nano start` uses.
///
/// This helper used to build its own `ApiServer::new(...).with_auth_bridge(...)` chain. That
/// second, independent assembly is precisely why the missing OAuth wiring stayed invisible:
/// this file was green while the binary served 503. There is one assembly now, and the tests
/// go through it.
fn router_from(config: &ApiConfig) -> (axum::Router, Vec<String>) {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (server, warnings) = ApiServer::from_config("127.0.0.1:0".parse().unwrap(), db, config);
    (server.into_router(), warnings)
}

fn router() -> axum::Router {
    router_from(&api_config(&[])).0
}

/// `GET /auth/v1/authorize?provider=...`, returning the status and the `Location` header.
async fn authorize(router: axum::Router, provider: &str) -> (StatusCode, Option<String>) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/auth/v1/authorize?provider={provider}"))
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.expect("router response");
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    (resp.status(), location)
}

/// Parse a URL's query string into a map of DECODED key/value pairs.
///
/// Asserting on the parsed query rather than on substrings of the URL: parameter order is
/// the `oauth2` crate's business, and `redirect_uri` arrives percent-encoded, so a
/// `contains()` check would either be order-dependent or quietly match nothing.
fn parse_query(url: &str) -> HashMap<String, String> {
    let query = url.split_once('?').map_or("", |(_, q)| q);
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

/// Minimal `application/x-www-form-urlencoded` decoder (`%XX` plus `+` for space).
///
/// Deliberately hand-rolled: pulling `url` or `percent-encoding` in as a dev-dependency to
/// read four query parameters is not worth the dependency surface.
fn percent_decode(input: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match c {
            '+' => out.push(b' '),
            '%' => match (chars.next(), chars.next()) {
                (Some(hi), Some(lo)) => {
                    let hex: String = [hi, lo].iter().collect();
                    match u8::from_str_radix(&hex, 16) {
                        Ok(byte) => out.push(byte),
                        Err(_) => {
                            out.push(b'%');
                            out.extend_from_slice(hex.as_bytes());
                        }
                    }
                }
                _ => out.push(b'%'),
            },
            other => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Assert the shape every provider's authorize redirect must have.
fn assert_authorize_redirect(status: StatusCode, location: Option<String>, expected_endpoint: &str) {
    assert_eq!(
        status,
        StatusCode::TEMPORARY_REDIRECT,
        "a configured provider must redirect (307). 503 here means the OAuth registry is not \
         wired into AppState — the state every build was in before sprinter e7b6cea1d3bc; 400 \
         means an EMPTY registry was attached instead of none."
    );

    let location = location.expect("a 307 must carry a Location header");
    assert!(
        location.starts_with(expected_endpoint),
        "expected a redirect to {expected_endpoint}, got {location}"
    );

    let query = parse_query(&location);
    assert_eq!(
        query.get("client_id").map(String::as_str),
        Some(CLIENT_ID),
        "the configured client_id must reach the provider: {location}"
    );
    assert_eq!(
        query.get("redirect_uri").map(String::as_str),
        Some(REDIRECT_URI),
        "the configured redirect_uri must reach the provider: {location}"
    );
    assert_eq!(
        query.get("response_type").map(String::as_str),
        Some("code"),
        "this is the Authorization Code flow: {location}"
    );
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256"),
        "PKCE must use S256, never `plain`: {location}"
    );
    assert!(
        query.get("code_challenge").is_some_and(|v| !v.is_empty()),
        "no PKCE challenge in {location}"
    );
    assert!(
        query.get("state").is_some_and(|v| !v.is_empty()),
        "no CSRF state in {location}"
    );
}

async fn status_of(method: &str, path: &str, body: Option<&str>) -> StatusCode {
    let req = Request::builder().method(method).uri(path);
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    router().oneshot(req).await.expect("router response").status()
}

/// The core regression assertion for #115: every documented endpoint is REACHABLE.
///
/// 404 here means the route is not mounted — which is exactly the state the shipped binary
/// was in through v4.26.0 while the README advertised all of it.
#[tokio::test]
async fn every_documented_baas_endpoint_is_mounted() {
    let cases: &[(&str, &str, Option<&str>)] = &[
        ("GET", "/health", None),
        ("GET", "/version", None),
        ("GET", "/docs", None),
        ("GET", "/openapi.json", None),
        (
            "POST",
            "/auth/v1/signup",
            Some(r#"{"email":"a@b.c","password":"correct-horse-battery"}"#),
        ),
        (
            "POST",
            "/auth/v1/token",
            Some(r#"{"email":"a@b.c","password":"correct-horse-battery"}"#),
        ),
        ("GET", "/auth/v1/user", None),
        ("POST", "/auth/v1/logout", Some(r#"{"refresh_token":"x"}"#)),
        ("POST", "/auth/v1/refresh", Some(r#"{"refresh_token":"x"}"#)),
        ("GET", "/realtime/v1/websocket", None),
    ];

    for (method, path, body) in cases {
        let status = status_of(method, path, *body).await;
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {path} is NOT MOUNTED (404). The README advertises this endpoint; \
             if it is being removed, remove the claim in the same commit."
        );
        assert_ne!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {path} returned 503 — the auth bridge is not wired into AppState, \
             which is the state that made every /auth/v1/* call fail before #115."
        );
    }
}

/// `/health` must return `{"status":"ok"}` as JSON, not the plain string "OK".
///
/// The `start` listener has always answered /health in that shape and monitoring is
/// written against it. When the ApiServer router was mounted on that listener its own
/// handler took the path over — and it returned plain text, silently changing the content
/// type and body for every deployment. That is a contract break disguised as a routing
/// change, so assert the SHAPE, not just the status code.
#[tokio::test]
async fn health_returns_the_json_shape_monitoring_expects() {
    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = router().oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("/health must be JSON, got {:?}: {e}", String::from_utf8_lossy(&bytes)));
    assert_eq!(json["status"], "ok", "/health must report {{\"status\":\"ok\"}}");
}

/// Endpoints that must be plainly successful, not merely reachable.
#[tokio::test]
async fn informational_endpoints_return_200() {
    for path in ["/health", "/version", "/docs", "/openapi.json"] {
        assert_eq!(
            status_of("GET", path, None).await,
            StatusCode::OK,
            "GET {path} must return 200"
        );
    }
}

/// The README's own signup example, end to end: it must create a user and mint a session.
#[tokio::test]
async fn the_readme_signup_example_succeeds() {
    let status = status_of(
        "POST",
        "/auth/v1/signup",
        Some(r#"{"email":"user@example.com","password":"correct-horse-battery"}"#),
    )
    .await;
    assert!(
        status.is_success(),
        "the signup example printed in README.md returned {status}. A 500 here usually means \
         AuthBridge::bootstrap did not run and `_auth_users` does not exist."
    );
}

/// A wrong password must NOT authenticate. Without this, the success test above could be
/// satisfied by a server that accepts anything — the exact trap that let the `--auth md5`
/// fail-open ship (GH#19).
#[tokio::test]
async fn a_wrong_password_is_rejected_by_the_token_endpoint() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let bridge = Arc::new(heliosdb_nano::api::auth_bridge::AuthBridge::new(
        Arc::clone(&db),
        &heliosdb_nano::config::generate_jwt_secret(),
    ));
    bridge.bootstrap().expect("bootstrap");
    bridge
        .sign_up("user@example.com", "correct-horse-battery")
        .expect("signup");

    assert!(
        bridge.sign_in("user@example.com", "wrong-password-xyz").is_err(),
        "a WRONG password must be rejected"
    );
    assert!(
        bridge.sign_in("user@example.com", "correct-horse-battery").is_ok(),
        "the CORRECT password must still authenticate"
    );
}

// ===========================================================================
// No shipped default signing key
// ===========================================================================

/// Every generated secret must be unique and full-length. The previous generator produced
/// 128 bits from `RandomState` (a HashDoS seed, not a CSPRNG) and wrote it out TWICE as
/// `{h1}{h2}{h1}{h2}` — 64 hex chars whose second half merely repeats the first. That was
/// tolerable while it signed nothing; it signs auth tokens now.
#[test]
fn generated_jwt_secrets_are_unique_and_not_self_repeating() {
    let a = heliosdb_nano::config::generate_jwt_secret();
    let b = heliosdb_nano::config::generate_jwt_secret();

    assert_eq!(a.len(), 64, "expected 256 bits as 64 hex chars, got {}", a.len());
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "must be hex: {a}");
    assert_ne!(a, b, "two generated secrets must differ");

    let (first, second) = a.split_at(32);
    assert_ne!(
        first, second,
        "the two halves of the secret are identical — the generator is padding entropy by \
         repeating itself, so this is a 128-bit key wearing a 256-bit costume"
    );
}

/// `jwt_secret_is_ephemeral` must reflect whether the OPERATOR configured a key, because the
/// server's warning (and the decision not to fall back to a constant) hangs off it.
#[test]
fn ephemeral_flag_tracks_whether_the_operator_configured_a_secret() {
    use heliosdb_nano::config::Config;

    let unset = Config::from_toml_str("[storage]\ncache_size_mb = 64\n").expect("parse");
    assert!(
        unset.api.jwt_secret_is_ephemeral,
        "no [api] jwt_secret in the file => the key was generated => ephemeral"
    );
    assert_eq!(unset.api.jwt_secret.len(), 64, "a key must still be generated");

    let set = Config::from_toml_str("[api]\njwt_secret = \"operator-chosen-key\"\n").expect("parse");
    assert!(
        !set.api.jwt_secret_is_ephemeral,
        "an explicitly configured [api] jwt_secret is NOT ephemeral"
    );
    assert_eq!(
        set.api.jwt_secret, "operator-chosen-key",
        "the configured key must be used"
    );

    // `[api]` present but no jwt_secret still counts as unconfigured.
    let partial = Config::from_toml_str("[api]\nanon_key = \"x\"\n").expect("parse");
    assert!(
        partial.api.jwt_secret_is_ephemeral,
        "[api] without jwt_secret must still be treated as unconfigured"
    );
}

// ===========================================================================
// OAuth providers configured in config.toml are actually served
// (sprinter e7b6cea1d3bc)
// ===========================================================================

/// THE regression assertion: `[[api.oauth_providers]] name = "google"` produces a real
/// redirect to Google, not 503 "oauth_not_configured".
#[tokio::test]
async fn a_configured_google_provider_is_served() {
    let (router, warnings) = router_from(&api_config(&["google"]));
    assert!(warnings.is_empty(), "a valid provider must not warn: {warnings:?}");

    let (status, location) = authorize(router, "google").await;
    assert_authorize_redirect(status, location, "https://accounts.google.com/o/oauth2/v2/auth");
}

/// The same for GitHub — the other provider `OAuthRegistry` supports.
#[tokio::test]
async fn a_configured_github_provider_is_served() {
    let (router, warnings) = router_from(&api_config(&["github"]));
    assert!(warnings.is_empty(), "a valid provider must not warn: {warnings:?}");

    let (status, location) = authorize(router, "github").await;
    assert_authorize_redirect(status, location, "https://github.com/login/oauth/authorize");
}

/// Two providers in one config: both must be served. A registry that only ever kept the
/// last entry would pass each single-provider test above and still be broken here.
#[tokio::test]
async fn both_providers_can_be_configured_at_once() {
    let (router, warnings) = router_from(&api_config(&["google", "github"]));
    assert!(warnings.is_empty(), "{warnings:?}");

    let (status, location) = authorize(router.clone(), "google").await;
    assert_authorize_redirect(status, location, "https://accounts.google.com/o/oauth2/v2/auth");

    let (status, location) = authorize(router, "github").await;
    assert_authorize_redirect(status, location, "https://github.com/login/oauth/authorize");
}

/// A misspelled provider name must WARN and must not be served.
///
/// The warning is the whole point: a silent skip is indistinguishable from "OAuth is not
/// configured", which is the failure mode this work exists to remove.
#[tokio::test]
async fn an_unknown_provider_name_warns_and_is_not_served() {
    let (router, warnings) = router_from(&api_config(&["gooogle"]));

    assert_eq!(warnings.len(), 1, "a refused entry must be reported once: {warnings:?}");
    let warning = warnings.first().expect("one warning");
    assert!(
        warning.contains("gooogle"),
        "the warning must name the entry: {warning}"
    );
    assert!(
        warning.contains("google, github"),
        "the warning must say what IS supported: {warning}"
    );
    assert!(
        !warning.contains(CLIENT_SECRET),
        "a warning must never carry the client_secret: {warning}"
    );

    let (status, location) = authorize(router, "gooogle").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a refused provider must not be served"
    );
    assert!(location.is_none(), "there must be no redirect: {location:?}");
}

/// THE CONTROL. With no providers configured, `/auth/v1/authorize` must still be MOUNTED
/// (not 404) and must answer the pre-existing 503 "not configured".
///
/// Without this, every test above could be satisfied by a server that redirects to Google
/// unconditionally, and the 503 path — the correct answer for an operator who configured no
/// OAuth at all — would be untested.
#[tokio::test]
async fn with_no_providers_configured_authorize_is_mounted_and_returns_503() {
    let (router, warnings) = router_from(&api_config(&[]));
    assert!(warnings.is_empty(), "{warnings:?}");

    let (status, location) = authorize(router, "google").await;
    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "/auth/v1/authorize must be mounted even when OAuth is unconfigured"
    );
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "with zero providers the registry must be left unattached, so this stays the existing \
         503 'oauth_not_configured' rather than a confusing 400 'provider not found'"
    );
    assert!(location.is_none(), "there must be no redirect: {location:?}");
}

/// A correct configuration must boot SILENTLY. Warnings an operator sees on every clean
/// start are warnings they learn to ignore.
#[tokio::test]
async fn a_good_config_produces_no_warnings() {
    let (_router, warnings) = router_from(&api_config(&["google", "github"]));
    assert!(
        warnings.is_empty(),
        "a valid config must produce no warnings, got: {warnings:?}"
    );
}

/// The ephemeral-key notice is the ONE warning a default deployment is expected to see, and
/// `src/main.rs` logs it at WARN rather than ERROR by matching this prefix.
#[tokio::test]
async fn an_unconfigured_jwt_secret_warns_with_the_documented_prefix() {
    let config = ApiConfig {
        jwt_secret_is_ephemeral: true,
        ..api_config(&["google"])
    };
    let (_router, warnings) = router_from(&config);

    assert!(
        warnings
            .iter()
            .any(|w| w.starts_with(heliosdb_nano::api::EPHEMERAL_JWT_WARNING_PREFIX)),
        "{warnings:?}"
    );
    for warning in &warnings {
        assert!(
            !warning.contains(&config.jwt_secret),
            "a warning must never carry the signing key: {warning}"
        );
    }
}
