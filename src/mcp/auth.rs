//! Authentication + authorisation for the MCP HTTP endpoint.
//!
//! Two checks layered together:
//!
//! 1. **JWT verification** via the existing [`crate::api::jwt::JwtManager`].
//!    Tokens are read from the standard `Authorization: Bearer …`
//!    header.
//! 2. **Scope authorisation**: MCP methods are split into
//!    [`Scope::Read`] (`initialize`, `tools/list`, `resources/list`,
//!    `resources/read`, `ping`) and [`Scope::Write`] (`tools/call`,
//!    plus anything that mutates state). The token must carry
//!    `mcp:read` for read-only methods and `mcp:write` for writes.
//!
//! The auth layer is opt-in. [`McpAuth::Disabled`] short-circuits all
//! checks — appropriate for stdio servers and Unix-domain sockets
//! that already have OS-level peer authentication. For TCP listeners
//! the [`bind_safety_check`] helper refuses non-loopback binds
//! unless an authenticator is configured.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
};

use crate::api::jwt::{Claims, JwtManager};
use crate::Error;

#[derive(Clone)]
pub enum McpAuth {
    /// No auth required — caller has guaranteed the transport is
    /// already authenticated (stdio child process, peer-cred Unix
    /// socket, etc.).
    Disabled,
    /// JWT-based: every request must carry a valid Bearer token with
    /// the appropriate `mcp:*` scope.
    Jwt(Arc<JwtManager>),
}

impl std::fmt::Debug for McpAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpAuth::Disabled => write!(f, "McpAuth::Disabled"),
            McpAuth::Jwt(_) => write!(f, "McpAuth::Jwt(<manager>)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Read,
    Write,
}

impl Scope {
    pub fn token(self) -> &'static str {
        match self {
            Scope::Read => "mcp:read",
            Scope::Write => "mcp:write",
        }
    }

    /// Map a JSON-RPC method to the scope it requires. `tools/call`
    /// counts as Write because tools may mutate state — finer-grained
    /// per-tool ACL is a follow-up.
    pub fn for_method(method: &str) -> Self {
        match method {
            "initialize"
            | "ping"
            | "tools/list"
            | "resources/list"
            | "resources/read" => Scope::Read,
            _ => Scope::Write,
        }
    }
}

impl McpAuth {
    /// Verify a request: extract bearer token, validate JWT, check
    /// scope. Returns the claims on success so handlers can use them
    /// for downstream identity checks.
    pub fn check(&self, header_value: Option<&str>, scope: Scope) -> Result<Option<Claims>, AuthError> {
        match self {
            McpAuth::Disabled => Ok(None),
            McpAuth::Jwt(mgr) => {
                let token = header_value
                    .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
                    .ok_or(AuthError::MissingToken)?;
                let claims = mgr
                    .validate_token(token)
                    .map_err(AuthError::InvalidToken)?;
                let needed = scope.token();
                if !claims.scopes.iter().any(|s| s == needed) {
                    return Err(AuthError::InsufficientScope(needed));
                }
                Ok(Some(claims))
            }
        }
    }
}

#[derive(Debug)]
pub enum AuthError {
    MissingToken,
    InvalidToken(Error),
    InsufficientScope(&'static str),
}

impl AuthError {
    pub fn status(&self) -> StatusCode {
        match self {
            AuthError::MissingToken | AuthError::InvalidToken(_) => StatusCode::UNAUTHORIZED,
            AuthError::InsufficientScope(_) => StatusCode::FORBIDDEN,
        }
    }
    pub fn message(&self) -> String {
        match self {
            AuthError::MissingToken => "missing bearer token".to_string(),
            AuthError::InvalidToken(e) => format!("invalid token: {e}"),
            AuthError::InsufficientScope(s) => format!("missing scope: {s}"),
        }
    }
}

// ----------------------------------------------------------------------
// Public-bind safety
// ----------------------------------------------------------------------

/// Refuse to bind a TCP listener on a non-loopback address unless an
/// authenticator is configured. Stops the common footgun of running
/// `--mcp-bind 0.0.0.0` against a default-built nano with no auth set
/// up.
pub fn bind_safety_check(addr: SocketAddr, auth: &McpAuth) -> Result<(), String> {
    let is_loopback = match addr.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    };
    if is_loopback {
        return Ok(());
    }
    match auth {
        McpAuth::Disabled => Err(format!(
            "refusing to bind MCP endpoint on non-loopback address {addr}: \
             authentication is disabled. Configure McpAuth::Jwt(...) or bind \
             to 127.0.0.1 / [::1] / a Unix socket instead."
        )),
        McpAuth::Jwt(_) => Ok(()),
    }
}

// ----------------------------------------------------------------------
// Axum middleware
// ----------------------------------------------------------------------

/// Middleware that runs JWT + scope validation on every MCP HTTP /
/// WebSocket request. The middleware deliberately doesn't decode the
/// JSON-RPC body — scope policy is keyed on the route, with `tools/call`
/// counted as Write across the board. Per-method scope refinement
/// for the unified `POST /mcp` endpoint runs inside the handler via
/// [`McpAuth::check`] once the body is parsed.
pub async fn require_read_scope(
    State(auth): State<McpAuth>,
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    enforce(&auth, Scope::Read, req, next).await
}

pub async fn require_write_scope(
    State(auth): State<McpAuth>,
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    enforce(&auth, Scope::Write, req, next).await
}

async fn enforce(
    auth: &McpAuth,
    scope: Scope,
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    match auth.check(header, scope) {
        Ok(_) => Ok(next.run(req).await),
        Err(e) => Err((e.status(), e.message())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use uuid::Uuid;

    #[test]
    fn disabled_check_passes() {
        let auth = McpAuth::Disabled;
        assert!(auth.check(None, Scope::Write).unwrap().is_none());
    }

    fn jwt_with_scopes(scopes: &[&str]) -> (Arc<JwtManager>, String) {
        let mgr = Arc::new(JwtManager::new(b"test-secret"));
        let token = mgr
            .generate_token("u".into(), "t".into(), Uuid::new_v4())
            .unwrap();
        // Decode + re-encode with custom scopes.
        let mut claims = mgr.validate_token(&token).unwrap();
        claims.scopes = scopes.iter().map(|s| (*s).to_string()).collect();
        // Re-encode using the same key.
        use jsonwebtoken::{encode, EncodingKey, Header};
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap();
        (mgr, token)
    }

    #[test]
    fn jwt_read_scope_passes_for_read() {
        let (mgr, token) = jwt_with_scopes(&["mcp:read"]);
        let auth = McpAuth::Jwt(mgr);
        let header = format!("Bearer {token}");
        assert!(auth.check(Some(&header), Scope::Read).is_ok());
    }

    #[test]
    fn jwt_read_scope_fails_for_write() {
        let (mgr, token) = jwt_with_scopes(&["mcp:read"]);
        let auth = McpAuth::Jwt(mgr);
        let header = format!("Bearer {token}");
        let err = auth.check(Some(&header), Scope::Write).unwrap_err();
        match err {
            AuthError::InsufficientScope("mcp:write") => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn jwt_missing_token_errors() {
        let auth = McpAuth::Jwt(Arc::new(JwtManager::new(b"test-secret")));
        let err = auth.check(None, Scope::Read).unwrap_err();
        assert!(matches!(err, AuthError::MissingToken));
    }

    #[test]
    fn jwt_invalid_token_errors() {
        let auth = McpAuth::Jwt(Arc::new(JwtManager::new(b"test-secret")));
        let err = auth.check(Some("Bearer not-a-token"), Scope::Read).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn loopback_bind_always_ok() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9000);
        assert!(bind_safety_check(addr, &McpAuth::Disabled).is_ok());
    }

    #[test]
    fn public_bind_disabled_auth_refused() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9000);
        let err = bind_safety_check(addr, &McpAuth::Disabled).unwrap_err();
        assert!(err.contains("non-loopback"), "got: {err}");
    }

    #[test]
    fn public_bind_jwt_ok() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9000);
        let auth = McpAuth::Jwt(Arc::new(JwtManager::new(b"strong-secret")));
        assert!(bind_safety_check(addr, &auth).is_ok());
    }

    #[test]
    fn scope_for_method_routing() {
        assert_eq!(Scope::for_method("initialize"), Scope::Read);
        assert_eq!(Scope::for_method("tools/list"), Scope::Read);
        assert_eq!(Scope::for_method("tools/call"), Scope::Write);
        assert_eq!(Scope::for_method("custom/whatever"), Scope::Write);
    }
}
