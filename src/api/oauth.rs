//! OAuth2 provider registry for Google and GitHub authentication.
//!
//! Implements the Authorization Code + PKCE flow:
//! 1. `get_authorize_url()` builds the redirect URL and stores the PKCE verifier.
//! 2. `exchange_code()` exchanges the authorization code for tokens, then fetches
//!    the provider's userinfo endpoint to obtain the user's email/name/avatar.
//!
//! The registry is designed to be wrapped in `Arc` and shared across handlers.
//!
//! # HTTP transport
//!
//! `oauth2` 5 speaks `http::Request<Vec<u8>>` / `http::Response<Vec<u8>>` natively
//! and implements its own `AsyncHttpClient` trait for `reqwest::Client`, so the
//! token endpoint is driven by the very same `reqwest` client that fetches the
//! provider's userinfo endpoint — one connection pool, one TLS stack.
//!
//! This file used to carry a hand-rolled adapter (`oauth2_http_adapter`) that
//! re-encoded every method, header, status code and body between `http` 0.2 (which
//! `oauth2` 4 was built on) and `http` 1.x (which `reqwest` 0.12 uses), because
//! `oauth2` 4's `request_async` wanted a closure rather than a client. That adapter
//! is gone: it was pure translation overhead, it silently dropped any response
//! header whose name or value failed to round-trip, and the `oauth2` 4 dependency
//! that forced it also dragged in a duplicate `hyper` 0.14 / `h2` 0.3 subtree
//! carrying RUSTSEC-2026-0258.

use oauth2::basic::{BasicClient, BasicErrorResponse};
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet, HttpClientError,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RequestTokenError, Scope, TokenResponse, TokenUrl,
};
use parking_lot::RwLock;
use serde::Deserialize;

use crate::config::OAuthProviderConfig;
use std::collections::HashMap;
use std::error::Error as StdError;
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

/// The fully-parameterised `BasicClient` this registry stores.
///
/// `oauth2` 5 tracks in the type system which endpoints have been configured, so
/// that a flow can no longer be started against a client that is missing the
/// endpoint it needs — an error that used to surface only at runtime.
/// `authorize_url()` exists only once the authorization endpoint is set, and
/// `exchange_code()` only once the token endpoint is set. `register_google` and
/// `register_github` set both, hence `EndpointSet` in the first and last slots.
///
/// The parameter order is `<HasAuthUrl, HasDeviceAuthUrl, HasIntrospectionUrl,
/// HasRevocationUrl, HasTokenUrl>`; the three middle endpoints are unused by this
/// crate and stay `EndpointNotSet`, which statically forbids calling the device-code,
/// introspection and revocation flows on these clients.
pub type ConfiguredOAuthClient = BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

/// A registered OAuth provider with its client, scopes, and userinfo URL.
pub struct OAuthProvider {
    pub name: String,
    pub client: ConfiguredOAuthClient,
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

    /// Build a registry from the `[[api.oauth_providers]]` entries of a parsed config.
    ///
    /// Returns the registry plus one human-readable warning for every entry that was
    /// REFUSED. The caller ([`crate::api::ApiServer::from_config`]) logs those warnings;
    /// nothing here writes to the log itself, so this stays a pure, unit-testable
    /// function.
    ///
    /// # Why warnings instead of an error
    ///
    /// One misspelled provider name must not stop the server from booting, and it must
    /// not be swallowed either: an operator who writes `name = "gooogle"` and gets a
    /// silently empty registry sees exactly the 503 this seam exists to remove, with
    /// nothing in the log to explain it. Every refusal therefore produces a line.
    ///
    /// # Fail closed
    ///
    /// A refused entry is never served. `register_*` validates every URL before it
    /// inserts, so a failure cannot leave a half-built provider behind — but a *later*
    /// duplicate entry that fails to build could otherwise leave the *earlier* entry
    /// registered under the same name, and the server would then serve a configuration
    /// the operator has since replaced. The explicit `remove` below closes that: once
    /// any entry for a provider is refused, that provider is not served at all.
    ///
    /// # Secrets
    ///
    /// Warning strings quote the provider name and a bounded prefix of the `client_id`
    /// (public by construction — it is handed to the user's browser in the authorize
    /// redirect). The `client_secret` is read only to check that it is non-empty and is
    /// never formatted into a message.
    pub fn from_config(providers: &[OAuthProviderConfig]) -> (Self, Vec<String>) {
        let mut registry = Self::new();
        let mut warnings = Vec::new();
        // Canonical names already accepted, in config order.
        let mut seen: Vec<String> = Vec::new();

        for entry in providers {
            // Provider names match case-insensitively: `Google`, `GOOGLE` and `google`
            // are the same provider. This TOML is written by hand.
            let canonical = entry.name.trim().to_ascii_lowercase();

            // 1. An unrecognised name is a refusal with a message, never a silent skip.
            if !matches!(canonical.as_str(), "google" | "github") {
                warnings.push(format!(
                    "unknown OAuth provider '{}'; supported: {SUPPORTED_PROVIDERS}",
                    entry.name
                ));
                continue;
            }

            // 2. A second entry for the same provider overwrites the first. That is
            //    allowed (last one wins), but it is never what the operator meant.
            if seen.iter().any(|name| name == &canonical) {
                warnings.push(format!(
                    "duplicate [[api.oauth_providers]] entry for '{canonical}'; the last entry in \
                     the config wins and the earlier one is ignored"
                ));
            } else {
                seen.push(canonical.clone());
            }

            // 3. Blank credentials build a client that fails only at the provider, with
            //    an opaque error, after the user has already been redirected away.
            if let Some(field) = blank_required_field(entry) {
                warnings.push(format!(
                    "OAuth provider '{canonical}' is REFUSED and will not be served: {field} is empty"
                ));
                registry.providers.remove(&canonical);
                continue;
            }

            // 4. Build. `register_*` validates the redirect URI (and the fixed endpoint
            //    URLs) before inserting anything.
            let built = match canonical.as_str() {
                "google" => registry.register_google(&entry.client_id, &entry.client_secret, &entry.redirect_uri),
                "github" => registry.register_github(&entry.client_id, &entry.client_secret, &entry.redirect_uri),
                // Unreachable: step 1 rejected every other name. Written as an error
                // rather than a panic so that a future edit to that guard degrades to a
                // refusal instead of taking the process down.
                other => Err(OAuthError::ConfigError(format!("unsupported provider '{other}'"))),
            };

            if let Err(e) = built {
                warnings.push(format!(
                    "OAuth provider '{canonical}' (client_id '{}') is REFUSED and will not be served: {e}",
                    short_client_id(&entry.client_id)
                ));
                registry.providers.remove(&canonical);
            }
        }

        (registry, warnings)
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

        let client = BasicClient::new(ClientId::new(client_id.to_string()))
            .set_client_secret(ClientSecret::new(client_secret.to_string()))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
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
        let token_url = TokenUrl::new("https://github.com/login/oauth/access_token".to_string())
            .map_err(|e| OAuthError::ConfigError(format!("Invalid GitHub token URL: {e}")))?;
        let redirect = RedirectUrl::new(redirect_uri.to_string())
            .map_err(|e| OAuthError::ConfigError(format!("Invalid redirect URI: {e}")))?;

        let client = BasicClient::new(ClientId::new(client_id.to_string()))
            .set_client_secret(ClientSecret::new(client_secret.to_string()))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
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

        let mut auth_req = prov.client.authorize_url(CsrfToken::new_random);

        for scope in &prov.scopes {
            auth_req = auth_req.add_scope(Scope::new(scope.clone()));
        }

        let (auth_url, csrf_state) = auth_req.set_pkce_challenge(pkce_challenge).url();

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
    pub async fn exchange_code(&self, code: &str, state: &str) -> Result<OAuthUserInfo, OAuthError> {
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

        // 2. Exchange code for tokens.
        //
        // `Policy::none()` is not a nicety: following redirects from the token
        // endpoint turns this call into an SSRF primitive (the provider, or anyone
        // who can spoof it, could bounce us at an internal address with the client
        // secret attached). `oauth2` 5 makes the same recommendation, and the same
        // client is reused for the userinfo fetch below, exactly as before.
        let http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| OAuthError::TokenExchange(format!("Failed to build HTTP client: {e}")))?;

        // `reqwest::Client` implements `oauth2::AsyncHttpClient`, so it is passed
        // straight in; no adapter closure and no `http` 0.2 <-> 1.x translation.
        let token_result = prov
            .client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .set_pkce_verifier(pending.verifier)
            .request_async(&http_client)
            .await
            .map_err(|e| OAuthError::TokenExchange(token_exchange_message(&e)))?;

        let access_token = token_result.access_token().secret().clone();

        // 3. Fetch userinfo
        let userinfo = fetch_userinfo(&http_client, &prov.userinfo_url, &access_token, provider_name).await?;

        Ok(userinfo)
    }

    /// Return the provider name associated with a pending state, if any.
    ///
    /// This is useful for the callback handler to know which provider initiated
    /// the flow without requiring a separate query parameter.
    pub fn provider_for_state(&self, state: &str) -> Option<String> {
        self.pending_flows.read().get(state).map(|f| f.provider.clone())
    }

    /// Returns `true` if there is at least one registered provider.
    pub fn has_providers(&self) -> bool {
        !self.providers.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Config -> registry helpers
// ---------------------------------------------------------------------------

/// The provider names [`OAuthRegistry::from_config`] accepts, for its error messages.
///
/// Kept next to the `matches!` guard that enforces it so the two cannot drift.
const SUPPORTED_PROVIDERS: &str = "google, github";

/// Upper bound, in characters, on how much of a `client_id` a refusal warning quotes.
///
/// The `client_id` is not a secret — it travels to the provider in the authorize
/// redirect, in plain sight of the user's browser — but it is operator-supplied text
/// that ends up in the server log, so the line is bounded rather than echoing it
/// wholesale. Long enough that a Google (`....apps.googleusercontent.com`) or GitHub
/// (`Iv1....`) client id stays recognisable at a glance.
const MAX_LOGGED_CLIENT_ID_CHARS: usize = 48;

/// Quote a `client_id` for a log line, truncated to [`MAX_LOGGED_CLIENT_ID_CHARS`].
///
/// Truncation lands on a character boundary (`char_indices`), so a multi-byte client
/// id cannot panic here.
fn short_client_id(client_id: &str) -> String {
    match client_id.char_indices().nth(MAX_LOGGED_CLIENT_ID_CHARS) {
        Some((byte_idx, _)) => format!("{}...", client_id.get(..byte_idx).unwrap_or_default()),
        None => client_id.to_string(),
    }
}

/// Name of the first required credential field that is blank, if any.
///
/// `client_secret` is inspected here and nowhere else outside the `oauth2` client it is
/// handed to: this function returns the field's NAME, never its value.
fn blank_required_field(entry: &OAuthProviderConfig) -> Option<&'static str> {
    if entry.client_id.trim().is_empty() {
        Some("client_id")
    } else if entry.client_secret.trim().is_empty() {
        Some("client_secret")
    } else if entry.redirect_uri.trim().is_empty() {
        Some("redirect_uri")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Token-endpoint error mapping
// ---------------------------------------------------------------------------

/// The concrete error `CodeTokenRequest::request_async` yields when the
/// `AsyncHttpClient` is a `reqwest::Client`.
///
/// Naming it keeps [`token_exchange_message`] a plain, unit-testable function
/// instead of a generic one that can only be exercised through a live HTTP call.
type TokenExchangeError = RequestTokenError<HttpClientError<reqwest::Error>, BasicErrorResponse>;

/// Upper bound on how far [`error_chain`] walks a `source()` chain.
///
/// A well-behaved chain terminates on its own; the cap only stops a buggy
/// third-party `source()` implementation from making this loop forever while
/// formatting an error.
const MAX_ERROR_CHAIN_DEPTH: usize = 8;

/// Flatten an error and its `source()` chain into one `": "`-joined message.
///
/// Necessary because `oauth2` 5's `thiserror` messages deliberately omit the
/// source: `HttpClientError::Reqwest` renders as the bare string `"client error"`
/// and `HttpClientError::Io` as `"I/O error"`. Formatting only the outermost error
/// would throw away the one thing an operator needs — *why* the request failed.
fn error_chain(err: &dyn StdError) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(inner) = source {
        if parts.len() >= MAX_ERROR_CHAIN_DEPTH {
            break;
        }
        parts.push(inner.to_string());
        source = inner.source();
    }
    parts.join(": ")
}

/// Render a token-endpoint failure as a message that keeps the provider's own words.
///
/// The previous code formatted the error with `{e}` alone. That was adequate under
/// `oauth2` 4 but is not under 5, whose `Display` impls for the transport and parse
/// variants are stubs (see [`error_chain`]); mapping every variant explicitly keeps
/// `OAuthError::TokenExchange` as specific as it was, and more specific for
/// transport failures.
fn token_exchange_message(err: &TokenExchangeError) -> String {
    match err {
        // The provider answered with a structured RFC 6749 §5.2 error.
        // `StandardErrorResponse`'s Display renders it as
        // `invalid_grant: <description> (see <uri>)` — the provider's own message.
        RequestTokenError::ServerResponse(response) => response.to_string(),
        // Transport failure (DNS, TLS, connection refused, timeout, ...).
        RequestTokenError::Request(inner) => {
            format!("HTTP request to token endpoint failed: {}", error_chain(inner))
        }
        // Malformed response body. The body itself is deliberately NOT included:
        // a 200 that fails to deserialize can still contain the access token, and
        // this string is surfaced to the HTTP client through `ApiError`.
        RequestTokenError::Parse(parse_err, _body) => {
            format!("Malformed token response: {}", error_chain(parse_err))
        }
        // e.g. "server returned empty error response" / "unexpected response
        // Content-Type: ...". Already a complete sentence from the oauth2 crate.
        RequestTokenError::Other(message) => message.clone(),
    }
}

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
        other => Err(OAuthError::UserInfoFetch(format!("Unknown provider: {other}"))),
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
async fn fetch_github_primary_email(client: &reqwest::Client, access_token: &str) -> Result<String, OAuthError> {
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
        registry.register_google("id", "secret", "http://localhost/cb").unwrap();

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

    // -----------------------------------------------------------------------
    // oauth2 5 typestate + error-mapping seam
    // -----------------------------------------------------------------------

    /// A registered client must statically satisfy the token endpoint typestate.
    ///
    /// `exchange_code()` only exists on a `Client` whose `HasTokenUrl` parameter is
    /// `EndpointSet`, so this stops compiling if `ConfiguredOAuthClient` or the
    /// builder chain in `register_*` ever loses `set_token_uri`.
    #[test]
    fn test_registered_client_supports_code_exchange() {
        let mut registry = OAuthRegistry::new();
        registry.register_google("id", "secret", "http://localhost/cb").unwrap();

        let provider = registry.providers.get("google").unwrap();
        // Nothing is sent; that this compiles and builds a request is the assertion.
        let _request = provider
            .client
            .exchange_code(AuthorizationCode::new("dummy-code".to_string()))
            .set_pkce_verifier(PkceCodeVerifier::new("dummy-verifier".to_string()));
    }

    #[test]
    fn test_token_exchange_message_keeps_provider_error() {
        use oauth2::basic::BasicErrorResponseType;

        let response = BasicErrorResponse::new(
            BasicErrorResponseType::InvalidGrant,
            Some("authorization code expired".to_string()),
            None,
        );
        let message = token_exchange_message(&RequestTokenError::ServerResponse(response));

        assert!(message.contains("invalid_grant"), "lost the error code: {message}");
        assert!(
            message.contains("authorization code expired"),
            "lost the description: {message}"
        );
    }

    #[test]
    fn test_token_exchange_message_transport_error_is_not_generic() {
        // `HttpClientError::Io` renders as the stub "I/O error"; the source walk in
        // `error_chain` is what keeps the real cause in the message.
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let err: TokenExchangeError = RequestTokenError::Request(HttpClientError::Io(io_err));
        let message = token_exchange_message(&err);

        assert!(message.contains("token endpoint"), "lost context: {message}");
        assert!(
            message.contains("connection refused"),
            "collapsed to generic: {message}"
        );
    }

    #[test]
    fn test_token_exchange_message_other_is_passed_through() {
        let err: TokenExchangeError = RequestTokenError::Other("server returned empty error response".to_string());
        assert_eq!(token_exchange_message(&err), "server returned empty error response");
    }

    #[test]
    fn test_error_chain_joins_sources() {
        let leaf = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        let wrapped = HttpClientError::<reqwest::Error>::Io(leaf);
        let chained = error_chain(&wrapped);

        assert!(chained.starts_with("I/O error"), "{chained}");
        assert!(chained.contains("timed out"), "{chained}");
        // Two links, so exactly one separator.
        assert_eq!(chained.matches(": ").count(), 1, "{chained}");
    }

    // -----------------------------------------------------------------------
    // from_config: the config -> registry seam (sprinter e7b6cea1d3bc)
    // -----------------------------------------------------------------------

    fn provider(name: &str, redirect: &str) -> OAuthProviderConfig {
        OAuthProviderConfig {
            name: name.to_string(),
            client_id: "test-client-id".to_string(),
            client_secret: "test-client-secret".to_string(),
            redirect_uri: redirect.to_string(),
        }
    }

    /// The happy path: a well-formed entry registers and says nothing.
    #[test]
    fn test_from_config_registers_google_without_warnings() {
        let (registry, warnings) =
            OAuthRegistry::from_config(&[provider("google", "http://localhost:8080/auth/v1/callback")]);

        assert!(registry.providers.contains_key("google"));
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    /// TOML is hand-written; `Google` and `google` are the same provider.
    #[test]
    fn test_from_config_matches_provider_names_case_insensitively() {
        let (registry, warnings) =
            OAuthRegistry::from_config(&[provider("GitHub", "http://localhost:8080/auth/v1/callback")]);

        assert!(registry.providers.contains_key("github"));
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    /// An unknown name must WARN, not vanish — a silent skip is indistinguishable
    /// from "OAuth is not configured", which is the bug this seam exists to fix.
    #[test]
    fn test_from_config_unknown_provider_warns_and_is_not_registered() {
        let (registry, warnings) =
            OAuthRegistry::from_config(&[provider("gooogle", "http://localhost:8080/auth/v1/callback")]);

        assert!(!registry.has_providers(), "an unknown provider must not be served");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = warnings.first().expect("one warning");
        assert!(warning.contains("gooogle"), "{warning}");
        assert!(
            warning.contains("google, github"),
            "must list what IS supported: {warning}"
        );
    }

    /// A build failure fails CLOSED: the provider is not in the registry at all.
    #[test]
    fn test_from_config_build_failure_is_refused_not_registered() {
        let (registry, warnings) = OAuthRegistry::from_config(&[provider("google", "not a valid url")]);

        assert!(
            !registry.has_providers(),
            "a provider that failed to build must NOT be served"
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = warnings.first().expect("one warning");
        assert!(warning.contains("google"), "{warning}");
        assert!(warning.contains("REFUSED"), "{warning}");
    }

    /// A later entry that fails must not leave an earlier, good one serving a config
    /// the operator has already replaced.
    #[test]
    fn test_from_config_failed_duplicate_unregisters_the_earlier_entry() {
        let (registry, warnings) = OAuthRegistry::from_config(&[
            provider("google", "http://localhost:8080/auth/v1/callback"),
            provider("google", "not a valid url"),
        ]);

        assert!(
            !registry.has_providers(),
            "the refused second entry must take the first one down with it"
        );
        assert!(
            warnings.iter().any(|w| w.contains("duplicate")),
            "the duplicate must be reported: {warnings:?}"
        );
        assert!(warnings.iter().any(|w| w.contains("REFUSED")), "{warnings:?}");
    }

    /// Blank credentials would redirect the user to a provider that rejects them.
    #[test]
    fn test_from_config_blank_credentials_are_refused() {
        let mut entry = provider("google", "http://localhost:8080/auth/v1/callback");
        entry.client_secret = "   ".to_string();

        let (registry, warnings) = OAuthRegistry::from_config(&[entry]);

        assert!(!registry.has_providers());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = warnings.first().expect("one warning");
        assert!(warning.contains("client_secret"), "must name the field: {warning}");
        assert!(warning.contains("is empty"), "{warning}");
    }

    /// No entries at all is not an error and produces no noise; the caller uses
    /// `has_providers()` to decide whether to attach the registry.
    #[test]
    fn test_from_config_empty_is_silent_and_has_no_providers() {
        let (registry, warnings) = OAuthRegistry::from_config(&[]);
        assert!(!registry.has_providers());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn test_from_config_registers_both_providers() {
        let (registry, warnings) = OAuthRegistry::from_config(&[
            provider("google", "http://localhost:8080/auth/v1/callback"),
            provider("github", "http://localhost:8080/auth/v1/callback"),
        ]);

        assert_eq!(registry.providers.len(), 2, "{warnings:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// No warning may ever carry the client secret. This is the assertion that keeps a
    /// future "helpful" error message from writing a credential into the server log.
    #[test]
    fn test_from_config_warnings_never_contain_the_client_secret() {
        let secret = "GOCSPX-super-secret-value";
        let entries = vec![
            OAuthProviderConfig {
                name: "nope".to_string(),
                client_id: "test-client-id".to_string(),
                client_secret: secret.to_string(),
                redirect_uri: "http://localhost:8080/auth/v1/callback".to_string(),
            },
            OAuthProviderConfig {
                name: "google".to_string(),
                client_id: "test-client-id".to_string(),
                client_secret: secret.to_string(),
                redirect_uri: "not a valid url".to_string(),
            },
        ];

        let (_registry, warnings) = OAuthRegistry::from_config(&entries);

        assert_eq!(warnings.len(), 2, "{warnings:?}");
        for warning in &warnings {
            assert!(
                !warning.contains(secret),
                "a refusal warning leaked the client_secret: {warning}"
            );
        }
    }

    /// A long client id is truncated rather than echoed wholesale into the log.
    #[test]
    fn test_short_client_id_truncates_on_a_char_boundary() {
        let long = "\u{e9}".repeat(MAX_LOGGED_CLIENT_ID_CHARS * 2);
        let shortened = short_client_id(&long);

        assert!(shortened.ends_with("..."), "{shortened}");
        assert_eq!(
            shortened.chars().count(),
            MAX_LOGGED_CLIENT_ID_CHARS + 3,
            "expected {MAX_LOGGED_CLIENT_ID_CHARS} chars plus the ellipsis: {shortened}"
        );

        let short = "Iv1.abc123";
        assert_eq!(short_client_id(short), short, "a short id is quoted verbatim");
    }

    #[test]
    fn test_token_exchange_error_maps_to_token_exchange_variant() {
        // Guards the granularity requirement end-to-end: whatever the token endpoint
        // does, callers keep seeing `OAuthError::TokenExchange` carrying the detail.
        let err: TokenExchangeError = RequestTokenError::Other("boom".to_string());
        let mapped = OAuthError::TokenExchange(token_exchange_message(&err));

        assert!(matches!(mapped, OAuthError::TokenExchange(_)));
        assert!(mapped.to_string().contains("boom"));
    }
}
