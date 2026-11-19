//! v03.7: webhook endpoints refuse unsigned requests when a secret is set, and
//! compare secrets in constant time. The router is the real one
//! (`api::routes::webhooks`), mounted on an in-memory database that can
//! branch, so a request that is wrongly accepted shows up as a new branch.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use heliosdb_nano::{
    api::{routes::webhooks, server::AppState},
    compute::QueryRegistry,
    git_integration::webhooks::{ct_eq_secret, WebhookConfig},
    EmbeddedDatabase,
};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    let mut block = [0u8; 64];
    block[..key.len()].copy_from_slice(key);
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner);
    hex::encode(outer.finalize())
}

fn app(config: WebhookConfig) -> (Router, Arc<EmbeddedDatabase>) {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let state = AppState {
        db: Arc::clone(&db),
        query_registry: Arc::new(QueryRegistry::new()),
        auth_bridge: None,
        oauth_registry: None,
        change_notifier: None,
    };
    (webhooks::routes_with_config(config).with_state(state), db)
}

fn config(github: Option<&str>, gitlab: Option<&str>, generic: Option<&str>) -> WebhookConfig {
    WebhookConfig {
        github_secret: github.map(str::to_string),
        gitlab_token: gitlab.map(str::to_string),
        generic_secret: generic.map(str::to_string),
        ..WebhookConfig::default()
    }
}

fn branches(db: &EmbeddedDatabase) -> Vec<String> {
    db.storage
        .list_branches()
        .unwrap()
        .into_iter()
        .map(|branch| branch.name)
        .collect()
}

fn has_preview(db: &EmbeddedDatabase) -> bool {
    branches(db).iter().any(|name| name.starts_with("pr-"))
}

const GITHUB_BODY: &str = r#"{"action":"opened","number":7,"pull_request":{"head":{"ref":"feat","sha":"abc"},"base":{"ref":"main","sha":"def"}},"repository":{"full_name":"o/r"}}"#;
const GITLAB_BODY: &str = r#"{"event_type":"merge_request","object_attributes":{"iid":8,"action":"open","source_branch":"feat","target_branch":"main","state":"opened"},"project":{"path_with_namespace":"o/r"}}"#;
const GENERIC_BODY: &str = r#"{"event_type":"pr_opened","source_branch":"feat","target_branch":"main","pr_number":9}"#;

async fn post(app: &Router, path: &str, headers: &[(&str, &str)], body: &str) -> StatusCode {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn github_secret_set_requires_a_valid_signature() {
    let secret = "s3cret";
    let (router, db) = app(config(Some(secret), None, None));
    let event = ("X-GitHub-Event", "pull_request");

    // header missing
    assert_eq!(
        post(&router, "/github", &[event], GITHUB_BODY).await,
        StatusCode::UNAUTHORIZED
    );
    assert!(!has_preview(&db), "an unsigned request created a branch");
    // wrong signature
    let wrong = format!("sha256={}", hmac_sha256_hex(b"other", GITHUB_BODY.as_bytes()));
    assert_eq!(
        post(
            &router,
            "/github",
            &[event, ("X-Hub-Signature-256", &wrong)],
            GITHUB_BODY
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    // no sha256= prefix
    let bare = hmac_sha256_hex(secret.as_bytes(), GITHUB_BODY.as_bytes());
    assert_eq!(
        post(
            &router,
            "/github",
            &[event, ("X-Hub-Signature-256", &bare)],
            GITHUB_BODY
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    assert!(!has_preview(&db));
    // valid
    let good = format!("sha256={bare}");
    assert_eq!(
        post(
            &router,
            "/github",
            &[event, ("X-Hub-Signature-256", &good)],
            GITHUB_BODY
        )
        .await,
        StatusCode::OK
    );
    assert!(
        has_preview(&db),
        "a signed request must be processed: {:?}",
        branches(&db)
    );
}

#[tokio::test]
async fn gitlab_token_set_requires_the_token() {
    let (router, db) = app(config(None, Some("tok"), None));
    assert_eq!(
        post(&router, "/gitlab", &[], GITLAB_BODY).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(&router, "/gitlab", &[("X-Gitlab-Token", "to")], GITLAB_BODY).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(&router, "/gitlab", &[("X-Gitlab-Token", "tokk")], GITLAB_BODY).await,
        StatusCode::UNAUTHORIZED
    );
    assert!(!has_preview(&db));
    assert_eq!(
        post(&router, "/gitlab", &[("X-Gitlab-Token", "tok")], GITLAB_BODY).await,
        StatusCode::OK
    );
    assert!(has_preview(&db));
}

#[tokio::test]
async fn generic_secret_set_requires_the_bearer() {
    let (router, db) = app(config(None, None, Some("gen")));
    assert_eq!(
        post(&router, "/generic", &[], GENERIC_BODY).await,
        StatusCode::UNAUTHORIZED
    );
    // a prefix of the secret, a longer value, and the wrong scheme
    for value in ["Bearer ge", "Bearer genx", "gen", "Basic gen"] {
        assert_eq!(
            post(&router, "/generic", &[("Authorization", value)], GENERIC_BODY).await,
            StatusCode::UNAUTHORIZED,
            "{value}"
        );
    }
    assert!(!has_preview(&db));
    assert_eq!(
        post(&router, "/generic", &[("Authorization", "Bearer gen")], GENERIC_BODY).await,
        StatusCode::OK
    );
    assert!(has_preview(&db));
}

#[tokio::test]
async fn no_secret_keeps_todays_behaviour() {
    // Owner decision pending (see the item report): with no secret configured
    // a request is still accepted, as before.
    let (router, db) = app(config(None, None, None));
    assert_eq!(post(&router, "/generic", &[], GENERIC_BODY).await, StatusCode::OK);
    assert!(has_preview(&db));
}

#[tokio::test]
async fn empty_secret_counts_as_unset() {
    let from_empty = WebhookConfig::from_values(Some(String::new()), Some(String::new()), Some(String::new()));
    assert!(from_empty.github_secret.is_none());
    assert!(from_empty.gitlab_token.is_none());
    assert!(from_empty.generic_secret.is_none());
    let kept = WebhookConfig::from_values(Some("a".into()), None, None);
    assert_eq!(kept.github_secret.as_deref(), Some("a"));
}

#[test]
fn ct_eq_secret_equal_different_and_empty() {
    assert!(ct_eq_secret("abc", "abc"));
    assert!(!ct_eq_secret("abc", "abd"));
    assert!(!ct_eq_secret("abc", "ab"));
    assert!(!ct_eq_secret("ab", "abc"));
    assert!(ct_eq_secret("", ""));
    assert!(!ct_eq_secret("", "a"));
}
