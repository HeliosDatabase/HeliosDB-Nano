//! OAuth2 provider registry for Google and GitHub authentication.
//!
//! Implements the Authorization Code + PKCE flow:
//! 1. `get_authorize_url()` builds the redirect URL and stores the PKCE verifier.
//! 2. `exchange_code()` exchanges the authorization code for tokens, then fetches
//!    the provider's userinfo endpoint to obtain the user's email/name/avatar.
//!
//! The registry is designed to be wrapped in `Arc` and shared across handlers.

use oauth2::{
    basic::BasicClient, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use parking_lot::RwLock;
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during the OAuth flow.
#[derive(Debug)]
pub enum OAuthError {
    /// The requested provider name is not registered.
    ProviderNotFound(String),
    /// The `state` parameter does not match any pending flow.
    InvalidState,
    /// Token exchange with the provider failed.
    TokenExchange(String),
    /// Fetching user information from the provider failed.
    UserInfoFetch(String),
    /// Provider configuration is invalid.
    ConfigError(String),
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProviderNotFound(name) => write!(f, "OAuth provider not found: {name}"),
            Self::InvalidState => write!(f, "Invalid or expired OAuth state parameter"),
            Self::TokenExchange(msg) => write!(f, "OAuth token exchange failed: {msg}"),
            Self::UserInfoFetch(msg) => write!(f, "Failed to fetch user info: {msg}"),
            Self::ConfigError(msg) => write!(f, "OAuth configuration error: {msg}"),
        }
    }
}

impl std::error::Error for OAuthError {}

// ---------------------------------------------------------------------------
// User info returned from providers
// ---------------------------------------------------------------------------

/// Normalized user information extracted from an OAuth provider's userinfo endpoint.
#[derive(Debug, Clone)]
pub struct OAuthUserInfo {
    /// User's email address.
    pub email: String,
    /// Display name (if available).
    pub name: Option<String>,
    /// Avatar / profile picture URL (if available).
    pub avatar_url: Option<String>,
    /// Provider name (e.g. `"google"`, `"github"`).
    pub provider: String,
    /// The unique user ID on the provider's side.
    pub provider_id: String,
}

// ---------------------------------------------------------------------------
// Internal: per-provider config
// ---------------------------------------------------------------------------

/// A registered OAuth provider with its client, scopes, and userinfo URL.
pub struct OAuthProvider {
    pub name: String,
    pub client: BasicClient,
    pub scopes: Vec<String>,
    pub userinfo_url: String,
}

// ---------------------------------------------------------------------------
// Pending flow entry (state -> verifier + provider name)
// ---------------------------------------------------------------------------

struct PendingFlow {
    verifier: PkceCodeVerifier,
    provider: String,
}

// ---------------------------------------------------------------------------
// OAuth registry
// ---------------------------------------------------------------------------

/// Thread-safe registry of OAuth providers and their pending PKCE flows.
pub struct OAuthRegistry {
    providers: HashMap<String, OAuthProvider>,
    /// PKCE verifiers keyed by the `state` string.
    pending_flows: RwLock<HashMap<String, PendingFlow>>,
}

impl OAuthRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            pending_flows: RwLock::new(HashMap::new()),
        }
    }

    /// Register Google as an OAuth provider.
    ///
    /// # Endpoints
    /// - Auth:  `https://accounts.google.com/o/oauth2/v2/auth`
    /// - Token: `https://oauth2.googleapis.com/token`
    /// - Userinfo: `https://www.googleapis.com/oauth2/v3/userinfo`
    pub fn register_google(
        &mut self,
        client_id: &str,
        client_secret: &str,
        redirect_uri: &str,
    ) -> Result<(), OAuthError> {
        let auth_url = AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".to_string())
            .map_err(|e| OAuthError::ConfigError(format!("Invalid Google auth URL: {e}")))?;
        let token_url = TokenUrl::new("https://oauth2.googleapis.com/token".to_string())
            .map_err(|e| OAuthError::ConfigError(format!("Invalid Google token URL: {e}")))?;
        let redirect = RedirectUrl::new(redirect_uri.to_string())
            .map_err(|e| OAuthError::ConfigError(format!("Invalid redirect URI: {e}")))?;

        let client = BasicClient::new(
            ClientId::new(client_id.to_string()),
            Some(ClientSecret::new(client_secret.to_string())),
            auth_url,
            Some(token_url),
        )
        .set_redirect_uri(redirect);

        self.providers.insert(
            "google".to_string(),
            OAuthProvider {
                name: "google".to_string(),
                client,
                scopes: vec!["email".to_string(), "profile".to_string()],
                userinfo_url: "https://www.googleapis.com/oauth2/v3/userinfo".to_string(),
            },
        );
        Ok(())
    }

    /// Register GitHub as an OAuth provider.
    ///
    /// # Endpoints
    /// - Auth:  `https://github.com/login/oauth/authorize`
    /// - Token: `https://github.com/login/oauth/access_token`
    /// - Userinfo: `https://api.github.com/user`
    pub fn register_github(
        &mut self,
        client_id: &str,
        client_secret: &str,
        redirect_uri: &str,
    ) -> Result<(), OAuthError> {
        let auth_url = AuthUrl::new("https://github.com/login/oauth/authorize".to_string())
            .map_err(|e| OAuthError::ConfigError(format!("Invalid GitHub auth URL: {e}")))?;
        let token_url =
            TokenUrl::new("https://github.com/login/oauth/access_token".to_string())
                .map_err(|e| OAuthError::ConfigError(format!("Invalid GitHub token URL: {e}")))?;
        let redirect = RedirectUrl::new(redirect_uri.to_string())
            .map_err(|e| OAuthError::ConfigError(format!("Invalid redirect URI: {e}")))?;

        let client = BasicClient::new(
            ClientId::new(client_id.to_string()),
            Some(ClientSecret::new(client_secret.to_string())),
            auth_url,
            Some(token_url),
        )
        .set_redirect_uri(redirect);

        self.providers.insert(
            "github".to_string(),
            OAuthProvider {
                name: "github".to_string(),
                client,
                scopes: vec!["read:user".to_string(), "user:email".to_string()],
                userinfo_url: "https://api.github.com/user".to_string(),
            },
        );
        Ok(())
    }

    /// Build the authorization redirect URL for a given provider.
    ///
    /// Returns `(redirect_url, state)`. The caller should redirect the user's
    /// browser to `redirect_url`. The `state` value is stored internally and
    /// matched during `exchange_code`.
    pub fn get_authorize_url(&self, provider: &str) -> Result<(String, String), OAuthError> {
        let prov = self
            .providers
            .get(provider)
            .ok_or_else(|| OAuthError::ProviderNotFound(provider.to_string()))?;

        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let mut auth_req = prov
            .client
            .authorize_url(CsrfToken::new_random);

        for scope in &prov.scopes {
            auth_req = auth_req.add_scope(Scope::new(scope.clone()));
        }

        let (auth_url, csrf_state) = auth_req
            .set_pkce_challenge(pkce_challenge)
            .url();

        let state_str = csrf_state.secret().clone();

        // Store the PKCE verifier for later exchange
        self.pending_flows.write().insert(
            state_str.clone(),
            PendingFlow {
                verifier: pkce_verifier,
                provider: provider.to_string(),
            },
        );

        Ok((auth_url.to_string(), state_str))
    }

    /// Exchange an authorization code for user information.
    ///
    /// 1. Retrieves the PKCE verifier associated with `state`.
    /// 2. Exchanges `code` for an access token via the provider's token endpoint.
    /// 3. Fetches the provider's userinfo endpoint with that token.
    /// 4. Parses the response into [`OAuthUserInfo`].
    pub async fn exchange_code(
        &self,
        code: &str,
        state: &str,
    ) -> Result<OAuthUserInfo, OAuthError> {
        // 1. Pop the pending flow
        let pending = self
            .pending_flows
            .write()
            .remove(state)
            .ok_or(OAuthError::InvalidState)?;

        let provider_name = &pending.provider;
        let prov = self
            .providers
            .get(provider_name)
            .ok_or_else(|| OAuthError::ProviderNotFound(provider_name.clone()))?;

        // 2. Exchange code for tokens
        let http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| OAuthError::TokenExchange(format!("Failed to build HTTP client: {e}")))?;

        let client_for_token = http_client.clone();
        let token_result = prov
            .client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .set_pkce_verifier(pending.verifier)
            .request_async(|req: oauth2::HttpRequest| async move {
                oauth2_http_adapter(&client_for_token, req).await
            })
            .await
            .map_err(|e| OAuthError::TokenExchange(format!("{e}")))?;

        let access_token = token_result.access_token().secret().clone();

        // 3. Fetch userinfo
        let userinfo = fetch_userinfo(
            &http_client,
            &prov.userinfo_url,
            &access_token,
            provider_name,
        )
        .await?;

        Ok(userinfo)
    }

    /// Return the provider name associated with a pending state, if any.
    ///
    /// This is useful for the callback handler to know which provider initiated
    /// the flow without requiring a separate query parameter.
    pub fn provider_for_state(&self, state: &str) -> Option<String> {
        self.pending_flows
            .read()
            .get(state)
            .map(|f| f.provider.clone())
    }

    /// Returns `true` if there is at least one registered provider.
    pub fn has_providers(&self) -> bool {
        !self.providers.is_empty()
    }
}

// ---------------------------------------------------------------------------
// oauth2 crate <-> reqwest HTTP adapter
// ---------------------------------------------------------------------------

/// Adapter that converts an [`oauth2::HttpRequest`] into a `reqwest` request,
/// executes it, and converts the response back into [`oauth2::HttpResponse`].
///
/// The `oauth2` v4 crate expects `request_async` to receive a closure of type
/// `FnOnce(HttpRequest) -> Future<Output = Result<HttpResponse, E>>`.
///
/// Because `oauth2` v4 depends on `http` 0.2 while `reqwest` 0.12 depends on
/// `http` 1.x, we convert between the two crate versions manually.
async fn oauth2_http_adapter(
    client: &reqwest::Client,
    req: oauth2::HttpRequest,
) -> Result<oauth2::HttpResponse, OAuthAdapterError> {
    // Convert method (http 0.2 -> string -> reqwest/http 1.x)
    let method_str = req.method.as_str();
    let rw_method: reqwest::Method = reqwest::Method::from_bytes(method_str.as_bytes())
        .map_err(|e| OAuthAdapterError(format!("Invalid HTTP method: {e}")))?;

    let mut builder = client.request(rw_method, req.url.as_str());

    // Convert headers (http 0.2 HeaderMap -> raw bytes -> reqwest/http 1.x)
    for (name, value) in &req.headers {
        let name_str = name.as_str();
        let value_bytes = value.as_bytes();
        let rw_name = reqwest::header::HeaderName::from_bytes(name_str.as_bytes())
            .map_err(|e| OAuthAdapterError(format!("Invalid header name: {e}")))?;
        let rw_value = reqwest::header::HeaderValue::from_bytes(value_bytes)
            .map_err(|e| OAuthAdapterError(format!("Invalid header value: {e}")))?;
        builder = builder.header(rw_name, rw_value);
    }

    builder = builder.body(req.body);

    let resp = builder
        .send()
        .await
        .map_err(|e| OAuthAdapterError(format!("HTTP request failed: {e}")))?;

    // Convert response back (reqwest/http 1.x -> http 0.2)
    let status_u16 = resp.status().as_u16();
    let oauth2_status = oauth2::http::StatusCode::from_u16(status_u16)
        .map_err(|e| OAuthAdapterError(format!("Invalid status code: {e}")))?;

    let rw_headers = resp.headers().clone();
    let body = resp
        .bytes()
        .await
        .map_err(|e| OAuthAdapterError(format!("Failed to read response body: {e}")))?
        .to_vec();

    // Convert headers back
    let mut oauth2_headers = oauth2::http::HeaderMap::new();
    for (name, value) in &rw_headers {
        if let (Ok(n), Ok(v)) = (
            oauth2::http::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            oauth2::http::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            oauth2_headers.insert(n, v);
        }
    }

    Ok(oauth2::HttpResponse {
        status_code: oauth2_status,
        headers: oauth2_headers,
        body,
    })
}

/// Simple error type for the HTTP adapter used in `request_async`.
#[derive(Debug)]
struct OAuthAdapterError(String);

impl fmt::Display for OAuthAdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for OAuthAdapterError {}

// ---------------------------------------------------------------------------
// Userinfo fetch + parsing (provider-specific)
// ---------------------------------------------------------------------------

/// Fetch and parse user information from a provider's userinfo endpoint.
async fn fetch_userinfo(
    client: &reqwest::Client,
    userinfo_url: &str,
    access_token: &str,
    provider: &str,
) -> Result<OAuthUserInfo, OAuthError> {
    let resp = client
        .get(userinfo_url)
        .bearer_auth(access_token)
        .header("Accept", "application/json")
        .header("User-Agent", "HeliosDB-Nano/1.0")
        .send()
        .await
        .map_err(|e| OAuthError::UserInfoFetch(format!("Request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(OAuthError::UserInfoFetch(format!(
            "Provider returned HTTP {status}: {body}"
        )));
    }

    match provider {
        "google" => parse_google_userinfo(resp).await,
        "github" => parse_github_userinfo(resp, client, access_token).await,
        other => Err(OAuthError::UserInfoFetch(format!(
            "Unknown provider: {other}"
        ))),
    }
}

/// Parse Google's `/oauth2/v3/userinfo` response.
#[derive(Deserialize)]
struct GoogleUserInfo {
    sub: String,
    email: Option<String>,
    name: Option<String>,
    picture: Option<String>,
}

async fn parse_google_userinfo(resp: reqwest::Response) -> Result<OAuthUserInfo, OAuthError> {
    let info: GoogleUserInfo = resp
        .json()
        .await
        .map_err(|e| OAuthError::UserInfoFetch(format!("Failed to parse Google response: {e}")))?;

    let email = info
        .email
        .ok_or_else(|| OAuthError::UserInfoFetch("Google response missing email".to_string()))?;

    Ok(OAuthUserInfo {
        email,
        name: info.name,
        avatar_url: info.picture,
        provider: "google".to_string(),
        provider_id: info.sub,
    })
}

/// Parse GitHub's `/user` response.
///
/// GitHub may not include the email in the `/user` response if the user has
/// their email set to private. In that case we make a second request to
/// `https://api.github.com/user/emails` to find the primary verified email.
#[derive(Deserialize)]
struct GitHubUserInfo {
    id: u64,
    email: Option<String>,
    name: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct GitHubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

async fn parse_github_userinfo(
    resp: reqwest::Response,
    client: &reqwest::Client,
    access_token: &str,
) -> Result<OAuthUserInfo, OAuthError> {
    let info: GitHubUserInfo = resp
        .json()
        .await
        .map_err(|e| OAuthError::UserInfoFetch(format!("Failed to parse GitHub response: {e}")))?;

    // Try the email from /user first; fall back to /user/emails
    let email = if let Some(ref e) = info.email {
        e.clone()
    } else {
        fetch_github_primary_email(client, access_token).await?
    };

    Ok(OAuthUserInfo {
        email,
        name: info.name,
        avatar_url: info.avatar_url,
        provider: "github".to_string(),
        provider_id: info.id.to_string(),
    })
}

/// Fetch the primary verified email from GitHub's `/user/emails` endpoint.
async fn fetch_github_primary_email(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<String, OAuthError> {
    let resp = client
        .get("https://api.github.com/user/emails")
        .bearer_auth(access_token)
        .header("Accept", "application/json")
        .header("User-Agent", "HeliosDB-Nano/1.0")
        .send()
        .await
        .map_err(|e| OAuthError::UserInfoFetch(format!("GitHub /user/emails request failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(OAuthError::UserInfoFetch(
            "GitHub /user/emails returned non-200".to_string(),
        ));
    }

    let emails: Vec<GitHubEmail> = resp
        .json()
        .await
        .map_err(|e| OAuthError::UserInfoFetch(format!("Failed to parse GitHub emails: {e}")))?;

    // Prefer primary + verified, then any verified, then any email
    emails
        .iter()
        .find(|e| e.primary && e.verified)
        .or_else(|| emails.iter().find(|e| e.verified))
        .or_else(|| emails.first())
        .map(|e| e.email.clone())
        .ok_or_else(|| OAuthError::UserInfoFetch("No email found on GitHub account".to_string()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_new_is_empty() {
        let registry = OAuthRegistry::new();
        assert!(!registry.has_providers());
    }

    #[test]
    fn test_register_google() {
        let mut registry = OAuthRegistry::new();
        registry
            .register_google("client-id", "client-secret", "http://localhost:8080/callback")
            .unwrap();
        assert!(registry.has_providers());
        assert!(registry.providers.contains_key("google"));
    }

    #[test]
    fn test_register_github() {
        let mut registry = OAuthRegistry::new();
        registry
            .register_github("client-id", "client-secret", "http://localhost:8080/callback")
            .unwrap();
        assert!(registry.has_providers());
        assert!(registry.providers.contains_key("github"));
    }

    #[test]
    fn test_get_authorize_url_unknown_provider() {
        let registry = OAuthRegistry::new();
        let result = registry.get_authorize_url("unknown");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), OAuthError::ProviderNotFound(_)));
    }

    #[test]
    fn test_get_authorize_url_google() {
        let mut registry = OAuthRegistry::new();
        registry
            .register_google("test-id", "test-secret", "http://localhost/callback")
            .unwrap();

        let (url, state) = registry.get_authorize_url("google").unwrap();
        assert!(url.contains("accounts.google.com"));
        assert!(url.contains("test-id"));
        assert!(!state.is_empty());

        // Verify the state was stored
        assert!(registry.provider_for_state(&state).is_some());
        assert_eq!(registry.provider_for_state(&state).unwrap(), "google");
    }

    #[test]
    fn test_get_authorize_url_github() {
        let mut registry = OAuthRegistry::new();
        registry
            .register_github("gh-id", "gh-secret", "http://localhost/callback")
            .unwrap();

        let (url, state) = registry.get_authorize_url("github").unwrap();
        assert!(url.contains("github.com"));
        assert!(url.contains("gh-id"));
        assert!(!state.is_empty());
    }

    #[test]
    fn test_invalid_state_returns_none() {
        let registry = OAuthRegistry::new();
        assert!(registry.provider_for_state("nonexistent").is_none());
    }

    #[test]
    fn test_oauth_error_display() {
        let err = OAuthError::ProviderNotFound("foo".to_string());
        assert!(err.to_string().contains("foo"));

        let err = OAuthError::InvalidState;
        assert!(err.to_string().contains("Invalid"));

        let err = OAuthError::TokenExchange("timeout".to_string());
        assert!(err.to_string().contains("timeout"));

        let err = OAuthError::UserInfoFetch("parse error".to_string());
        assert!(err.to_string().contains("parse error"));

        let err = OAuthError::ConfigError("bad uri".to_string());
        assert!(err.to_string().contains("bad uri"));
    }

    #[test]
    fn test_multiple_providers() {
        let mut registry = OAuthRegistry::new();
        registry
            .register_google("g-id", "g-secret", "http://localhost/callback")
            .unwrap();
        registry
            .register_github("gh-id", "gh-secret", "http://localhost/callback")
            .unwrap();
        assert_eq!(registry.providers.len(), 2);
    }

    #[test]
    fn test_authorize_url_scopes() {
        let mut registry = OAuthRegistry::new();
        registry
            .register_google("id", "secret", "http://localhost/cb")
            .unwrap();

        let (url, _) = registry.get_authorize_url("google").unwrap();
        // Scopes should be present in the URL
        assert!(url.contains("scope="));
    }

    #[test]
    fn test_register_invalid_redirect_uri() {
        let mut registry = OAuthRegistry::new();
        let result = registry.register_google("id", "secret", "not a valid url");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), OAuthError::ConfigError(_)));
    }
}
