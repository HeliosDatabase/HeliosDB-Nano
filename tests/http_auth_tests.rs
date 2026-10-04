//! P0-1 regression suite: the daemon HTTP surface must require the wire credential.
//!
//! Before this, `POST /v1/branches/main/query` (and every other route assembled by
//! `run_http_listener`) executed SQL with no authentication even when the server was
//! started with `--auth scram-sha-256 --password …`. This file builds the SAME
//! composition the daemon builds — `ApiServer::from_config` (BaaS), the authenticated
//! MCP mount, then `attach_http_auth` — and asserts the policy:
//!
//! * every non-health route is 401 without credentials, with `WWW-Authenticate`;
//! * `/health` (and the `/` alias) is reachable without credentials;
//! * the wire password and the MCP bearer token are both accepted;
//! * the exact branch-query bypass is closed.
//!
//! It uses `tower::oneshot` rather than a real socket so it exercises the router the
//! daemon serves without binding a port.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use heliosdb_nano::api::{attach_http_auth, http_bind_safety_check, ApiServer, HttpAuthPolicy};
use heliosdb_nano::config::ApiConfig;
use heliosdb_nano::EmbeddedDatabase;
use std::sync::Arc;
use tower::ServiceExt;

const PASSWORD: &str = "s3cret-wire-password";
const MCP_TOKEN: &str = "mcp-bearer-token";

/// The router `run_http_listener` assembles, with a required policy. MCP is mounted
/// only when the feature is on, exactly as in `src/main.rs`.
fn protected_router() -> axum::Router {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (server, _warnings) = ApiServer::from_config("127.0.0.1:0".parse().unwrap(), db, &ApiConfig::default());
    let app = server.into_router().route("/", axum::routing::get(|| async { "ok" }));

    #[cfg(feature = "mcp-endpoint")]
    let app = {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
        let state = heliosdb_nano::mcp::McpState::new(db)
            .with_auth(heliosdb_nano::mcp::McpAuth::BearerToken(Arc::from(MCP_TOKEN)));
        heliosdb_nano::mcp::attach_mcp_routes(app, state)
    };

    let policy = Arc::new(HttpAuthPolicy::from_wire(
        Some(PASSWORD.to_string()),
        Some(Arc::from(MCP_TOKEN)),
        false,
    ));
    attach_http_auth(app, policy)
}

fn basic(password: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(format!("postgres:{password}"))
}

async fn status(router: &axum::Router, method: &str, path: &str, body: Option<&str>, auth: Option<&str>) -> StatusCode {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(auth) = auth {
        req = req.header(header::AUTHORIZATION, auth);
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    router.clone().oneshot(req).await.expect("router response").status()
}

/// Every documented daemon route, including the exact bypass in the brief.
const ROUTES: &[(&str, &str, Option<&str>)] = &[
    ("POST", "/v1/branches/main/query", Some(r#"{"sql":"SELECT 1"}"#)),
    ("POST", "/v1/branches/main/execute", Some(r#"{"sql":"SELECT 1"}"#)),
    ("GET", "/v1/branches/", None),
    ("GET", "/rest/v1/users", None),
    ("POST", "/auth/v1/signup", Some(r#"{"email":"a@b.c","password":"x"}"#)),
    ("POST", "/auth/v1/token", Some(r#"{"email":"a@b.c","password":"x"}"#)),
    ("GET", "/version", None),
    ("GET", "/docs", None),
    ("GET", "/openapi.json", None),
];

#[tokio::test]
async fn every_route_is_rejected_without_credentials() {
    let router = protected_router();
    for (method, path, body) in ROUTES {
        let status = status(&router, method, path, *body, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} must be 401 without credentials, got {status}"
        );
    }
}

#[tokio::test]
async fn unauthorized_response_carries_www_authenticate() {
    let req = Request::builder()
        .method("GET")
        .uri("/version")
        .body(Body::empty())
        .unwrap();
    let resp = protected_router().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        resp.headers().contains_key(header::WWW_AUTHENTICATE),
        "a 401 must carry WWW-Authenticate"
    );
}

#[tokio::test]
async fn health_is_exempt_but_only_health() {
    let router = protected_router();
    assert_eq!(
        status(&router, "GET", "/health", None, None).await,
        StatusCode::OK,
        "/health is the side-effect-free exception"
    );
    assert_eq!(
        status(&router, "GET", "/", None, None).await,
        StatusCode::OK,
        "/ is the health alias on the daemon listener"
    );
    assert_eq!(
        status(&router, "GET", "/version", None, None).await,
        StatusCode::UNAUTHORIZED,
        "/version leaks a version and is NOT exempt"
    );
}

#[tokio::test]
async fn the_exact_branch_query_bypass_is_closed() {
    let router = protected_router();
    let path = "/v1/branches/main/query";
    let body = Some(r#"{"sql":"SELECT 1"}"#);

    assert_eq!(
        status(&router, "POST", path, body, None).await,
        StatusCode::UNAUTHORIZED,
        "the P0 bypass must be 401 without credentials"
    );

    // A wrong password is rejected.
    assert_eq!(
        status(&router, "POST", path, body, Some(&format!("Basic {}", basic("wrong")))).await,
        StatusCode::UNAUTHORIZED
    );

    // The correct password reaches the handler (which may itself return a 4xx/5xx
    // for the in-memory branch, but MUST NOT be the auth layer's 401).
    let authed = status(&router, "POST", path, body, Some(&format!("Basic {}", basic(PASSWORD)))).await;
    assert_ne!(
        authed,
        StatusCode::UNAUTHORIZED,
        "the wire password must pass the auth layer"
    );
}

#[tokio::test]
async fn safe_routes_return_200_with_the_wire_password() {
    let router = protected_router();
    let auth = format!("Basic {}", basic(PASSWORD));
    for path in ["/version", "/docs", "/openapi.json"] {
        assert_eq!(
            status(&router, "GET", path, None, Some(&auth)).await,
            StatusCode::OK,
            "GET {path} must return 200 with the wire password"
        );
    }
}

#[tokio::test]
async fn the_mcp_bearer_token_is_scoped_to_mcp_routes() {
    let router = protected_router();
    assert_eq!(
        status(&router, "GET", "/version", None, Some(&format!("Bearer {MCP_TOKEN}"))).await,
        StatusCode::UNAUTHORIZED,
        "the MCP bearer token must not authenticate BaaS routes"
    );
}

#[cfg(feature = "mcp-endpoint")]
#[tokio::test]
async fn mcp_remains_authenticated_under_the_wire_policy() {
    let router = protected_router();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;

    assert_eq!(
        status(&router, "POST", "/mcp", Some(body), None).await,
        StatusCode::UNAUTHORIZED,
        "MCP without any credential is rejected by the wire layer"
    );
    assert_eq!(
        status(
            &router,
            "POST",
            "/mcp",
            Some(body),
            Some(&format!("Basic {}", basic(PASSWORD)))
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "Basic alone does not satisfy MCP's own bearer check"
    );
    assert_eq!(
        status(
            &router,
            "POST",
            "/mcp",
            Some(body),
            Some(&format!("Bearer {MCP_TOKEN}"))
        )
        .await,
        StatusCode::OK,
        "the shared bearer token satisfies both layers"
    );
}

// ---------------------------------------------------------------------------
// Trust mode: no route auth, but the listener must stay loopback by default.
// ---------------------------------------------------------------------------

#[test]
fn trust_mode_loopback_is_allowed_and_non_loopback_is_refused() {
    let policy = HttpAuthPolicy::disabled();
    assert!(!policy.is_required());

    let loopback = "127.0.0.1:8080".parse().unwrap();
    assert!(http_bind_safety_check(loopback, &policy, false).is_ok());

    let public = "0.0.0.0:8080".parse().unwrap();
    assert!(
        http_bind_safety_check(public, &policy, false).is_err(),
        "trust mode on a non-loopback address must be refused without --allow-insecure-http"
    );
}
