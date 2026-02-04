//! OpenAPI specification serving and documentation UI
//!
//! This module provides:
//! - OpenAPI 3.0 spec serving at /v1/openapi.json and /v1/openapi.yaml
//! - Swagger UI at /v1/docs
//! - ReDoc documentation at /v1/redoc

use axum::{
    body::Body,
    http::{header, StatusCode},
    response::{Html, Response},
    routing::get,
    Router,
};
use crate::api::server::AppState;

/// Create a minimal error response that cannot fail
///
/// This is used as a fallback when building more detailed responses fails.
/// Uses only safe operations that are guaranteed to succeed.
fn minimal_error_response() -> Response {
    // SAFETY: This construction cannot fail because:
    // 1. StatusCode::INTERNAL_SERVER_ERROR is a valid status code
    // 2. Body::empty() cannot fail
    // 3. No headers are being set that could be invalid
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::empty())
        .unwrap_or_else(|_| {
            // Ultimate fallback using default - this branch is unreachable
            // in practice but satisfies the type system
            Response::default()
        })
}

/// The OpenAPI specification as a static string (embedded at compile time)
pub const OPENAPI_YAML: &str = include_str!("openapi.yaml");

/// OpenAPI routes
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/openapi.json", get(openapi_json))
        .route("/openapi.yaml", get(openapi_yaml))
        .route("/docs", get(swagger_ui))
        .route("/docs/", get(swagger_ui))
        .route("/redoc", get(redoc_ui))
        .route("/redoc/", get(redoc_ui))
}

/// Serve OpenAPI spec as JSON
async fn openapi_json() -> Response {
    match serde_yaml::from_str::<serde_json::Value>(OPENAPI_YAML) {
        Ok(spec) => {
            let json = serde_json::to_string_pretty(&spec).unwrap_or_default();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(json))
                .unwrap_or_else(|_| minimal_error_response())
        }
        Err(e) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from(format!("Failed to parse OpenAPI spec: {}", e)))
            .unwrap_or_else(|_| minimal_error_response()),
    }
}

/// Serve OpenAPI spec as YAML
async fn openapi_yaml() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-yaml")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from(OPENAPI_YAML))
        .unwrap_or_else(|_| minimal_error_response())
}

/// Serve Swagger UI
async fn swagger_ui() -> Html<String> {
    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>HeliosDB-Lite API Documentation</title>
    <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5.9.0/swagger-ui.css">
    <style>
        body {{
            margin: 0;
            padding: 0;
        }}
        .swagger-ui .topbar {{
            background-color: #1a1a2e;
        }}
        .swagger-ui .info .title {{
            color: #1a1a2e;
        }}
        .swagger-ui .btn.execute {{
            background-color: #4f46e5;
            border-color: #4f46e5;
        }}
        .swagger-ui .btn.execute:hover {{
            background-color: #4338ca;
            border-color: #4338ca;
        }}
        .helios-header {{
            background: linear-gradient(135deg, #1a1a2e 0%, #16213e 100%);
            color: white;
            padding: 1rem 2rem;
            display: flex;
            align-items: center;
            gap: 1rem;
        }}
        .helios-header h1 {{
            margin: 0;
            font-size: 1.5rem;
            font-weight: 600;
        }}
        .helios-header .version {{
            background: #4f46e5;
            padding: 0.25rem 0.75rem;
            border-radius: 9999px;
            font-size: 0.875rem;
        }}
    </style>
</head>
<body>
    <div class="helios-header">
        <h1>HeliosDB-Lite</h1>
        <span class="version">v3.3.0</span>
    </div>
    <div id="swagger-ui"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5.9.0/swagger-ui-bundle.js"></script>
    <script src="https://unpkg.com/swagger-ui-dist@5.9.0/swagger-ui-standalone-preset.js"></script>
    <script>
        window.onload = function() {{
            SwaggerUIBundle({{
                url: "/v1/openapi.json",
                dom_id: '#swagger-ui',
                deepLinking: true,
                presets: [
                    SwaggerUIBundle.presets.apis,
                    SwaggerUIStandalonePreset
                ],
                plugins: [
                    SwaggerUIBundle.plugins.DownloadUrl
                ],
                layout: "StandaloneLayout",
                validatorUrl: null,
                supportedSubmitMethods: ['get', 'post', 'put', 'delete', 'patch'],
                docExpansion: 'list',
                filter: true,
                showRequestHeaders: true,
                showCommonExtensions: true
            }});
        }};
    </script>
</body>
</html>"#
    );
    Html(html)
}

/// Serve ReDoc documentation
async fn redoc_ui() -> Html<String> {
    let html = format!(
        r###"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>HeliosDB-Lite API Reference</title>
    <link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&display=swap" rel="stylesheet">
    <style>
        body {{
            margin: 0;
            padding: 0;
            font-family: 'Inter', sans-serif;
        }}
        .helios-header {{
            background: linear-gradient(135deg, #1a1a2e 0%, #16213e 100%);
            color: white;
            padding: 1rem 2rem;
            display: flex;
            align-items: center;
            gap: 1rem;
            position: sticky;
            top: 0;
            z-index: 100;
        }}
        .helios-header h1 {{
            margin: 0;
            font-size: 1.5rem;
            font-weight: 600;
        }}
        .helios-header .version {{
            background: #4f46e5;
            padding: 0.25rem 0.75rem;
            border-radius: 9999px;
            font-size: 0.875rem;
        }}
        .helios-header .links {{
            margin-left: auto;
            display: flex;
            gap: 1rem;
        }}
        .helios-header .links a {{
            color: white;
            text-decoration: none;
            opacity: 0.8;
            transition: opacity 0.2s;
        }}
        .helios-header .links a:hover {{
            opacity: 1;
        }}
    </style>
</head>
<body>
    <div class="helios-header">
        <h1>HeliosDB-Lite</h1>
        <span class="version">v3.3.0</span>
        <div class="links">
            <a href="/v1/docs">Swagger UI</a>
            <a href="/v1/openapi.yaml">OpenAPI Spec</a>
            <a href="https://github.com/heliosdb/heliosdb-lite">GitHub</a>
        </div>
    </div>
    <redoc spec-url="/v1/openapi.json"
           hide-download-button="false"
           theme='{{"colors":{{"primary":{{"main":"#4f46e5"}}}}}}'
    ></redoc>
    <script src="https://cdn.redoc.ly/redoc/latest/bundles/redoc.standalone.js"></script>
</body>
</html>"###
    );
    Html(html)
}

// Note: Tests for OpenAPI module moved to integration tests to avoid
// Rust 2021 prefix identifier parsing issues with string literals
