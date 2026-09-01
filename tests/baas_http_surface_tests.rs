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
//! WHAT THIS FILE GUARDS. That the routes are MOUNTED. A regression here is invisible to every
//! other suite: the library-level handler tests kept passing throughout, because the handlers
//! were always fine — it was the wiring that did not exist. Asserting "not 404" is the whole
//! point; asserting handler behaviour is what the existing suites already do.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use heliosdb_nano::api::ApiServer;
use heliosdb_nano::EmbeddedDatabase;
use std::sync::Arc;
use tower::ServiceExt;

fn router() -> axum::Router {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let bridge = Arc::new(heliosdb_nano::api::auth_bridge::AuthBridge::new(
        Arc::clone(&db),
        // A per-test key. The point of the assertion below is the ROUTE, not the crypto.
        &heliosdb_nano::config::generate_jwt_secret(),
    ));
    bridge.bootstrap().expect("auth schema bootstrap");
    ApiServer::new("127.0.0.1:0".parse().unwrap(), db)
        .with_auth_bridge(bridge)
        .into_router()
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
