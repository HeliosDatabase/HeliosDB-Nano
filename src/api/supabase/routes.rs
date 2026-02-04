//! Supabase-Compatible API Routes
//!
//! Route definitions for Supabase-compatible REST, Auth, Storage, and Realtime APIs.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Supabase API router configuration
#[derive(Debug, Clone)]
pub struct SupabaseRouter {
    /// Project reference (like Supabase project ID)
    pub project_ref: String,
    /// API key (anon key)
    pub anon_key: Option<String>,
    /// Service role key
    pub service_role_key: Option<String>,
    /// JWT secret
    pub jwt_secret: String,
    /// Enable PostgREST API
    pub enable_rest: bool,
    /// Enable Auth API
    pub enable_auth: bool,
    /// Enable Storage API
    pub enable_storage: bool,
    /// Enable Realtime API
    pub enable_realtime: bool,
}

impl Default for SupabaseRouter {
    fn default() -> Self {
        Self {
            project_ref: "local".to_string(),
            anon_key: None,
            service_role_key: None,
            jwt_secret: "your-super-secret-jwt-key".to_string(),
            enable_rest: true,
            enable_auth: true,
            enable_storage: true,
            enable_realtime: true,
        }
    }
}

/// Route handler result
#[derive(Debug, Clone, Serialize)]
pub struct RouteResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: serde_json::Value,
}

impl RouteResponse {
    pub fn ok(body: serde_json::Value) -> Self {
        Self {
            status: 200,
            headers: default_headers(),
            body,
        }
    }

    pub fn created(body: serde_json::Value) -> Self {
        Self {
            status: 201,
            headers: default_headers(),
            body,
        }
    }

    pub fn no_content() -> Self {
        Self {
            status: 204,
            headers: default_headers(),
            body: serde_json::Value::Null,
        }
    }

    pub fn error(status: u16, error: &str, message: &str) -> Self {
        Self {
            status,
            headers: default_headers(),
            body: serde_json::json!({
                "error": error,
                "message": message
            }),
        }
    }

    pub fn not_found(message: &str) -> Self {
        Self::error(404, "Not found", message)
    }

    pub fn unauthorized() -> Self {
        Self::error(401, "Unauthorized", "Invalid or missing API key")
    }

    pub fn bad_request(message: &str) -> Self {
        Self::error(400, "Bad request", message)
    }

    pub fn with_header(mut self, key: &str, value: &str) -> Self {
        self.headers.insert(key.to_string(), value.to_string());
        self
    }
}

fn default_headers() -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("X-Powered-By".to_string(), "HeliosDB-Lite".to_string());
    headers
}

/// Request context
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Option<serde_json::Value>,
    pub user_id: Option<String>,
    pub role: String,
}

impl RequestContext {
    pub fn header(&self, key: &str) -> Option<&String> {
        self.headers.get(key)
            .or_else(|| self.headers.get(&key.to_lowercase()))
    }

    pub fn api_key(&self) -> Option<&String> {
        self.header("apikey")
            .or_else(|| self.header("Authorization").map(|h| h.strip_prefix("Bearer ").unwrap_or(h)).map(|_| &String::new()))
    }

    pub fn content_type(&self) -> Option<&String> {
        self.header("Content-Type")
            .or_else(|| self.header("content-type"))
    }

    pub fn prefer(&self) -> PreferHeader {
        PreferHeader::parse(self.header("Prefer").map(|s| s.as_str()).unwrap_or(""))
    }
}

/// Parsed Prefer header
#[derive(Debug, Clone, Default)]
pub struct PreferHeader {
    pub return_type: Option<ReturnType>,
    pub count: Option<CountType>,
    pub resolution: Option<ConflictResolution>,
}

#[derive(Debug, Clone)]
pub enum ReturnType {
    Representation,
    Minimal,
    Headers,
}

#[derive(Debug, Clone)]
pub enum CountType {
    Exact,
    Planned,
    Estimated,
}

#[derive(Debug, Clone)]
pub enum ConflictResolution {
    MergeDuplicates,
    IgnoreDuplicates,
}

impl PreferHeader {
    pub fn parse(header: &str) -> Self {
        let mut prefer = Self::default();

        for part in header.split(',') {
            let part = part.trim();
            match part {
                "return=representation" => prefer.return_type = Some(ReturnType::Representation),
                "return=minimal" => prefer.return_type = Some(ReturnType::Minimal),
                "return=headers-only" => prefer.return_type = Some(ReturnType::Headers),
                "count=exact" => prefer.count = Some(CountType::Exact),
                "count=planned" => prefer.count = Some(CountType::Planned),
                "count=estimated" => prefer.count = Some(CountType::Estimated),
                "resolution=merge-duplicates" => prefer.resolution = Some(ConflictResolution::MergeDuplicates),
                "resolution=ignore-duplicates" => prefer.resolution = Some(ConflictResolution::IgnoreDuplicates),
                _ => {}
            }
        }

        prefer
    }
}

/// Route definitions
pub struct Routes;

impl Routes {
    /// All Supabase-compatible route patterns
    pub fn all() -> Vec<RoutePattern> {
        let mut routes = Vec::new();

        // PostgREST routes
        routes.extend(Self::rest_routes());

        // Auth routes
        routes.extend(Self::auth_routes());

        // Storage routes
        routes.extend(Self::storage_routes());

        // Realtime routes
        routes.extend(Self::realtime_routes());

        routes
    }

    /// PostgREST API routes
    pub fn rest_routes() -> Vec<RoutePattern> {
        vec![
            RoutePattern::new("GET", "/rest/v1/{table}", "postgrest.select"),
            RoutePattern::new("POST", "/rest/v1/{table}", "postgrest.insert"),
            RoutePattern::new("PATCH", "/rest/v1/{table}", "postgrest.update"),
            RoutePattern::new("DELETE", "/rest/v1/{table}", "postgrest.delete"),
            RoutePattern::new("POST", "/rest/v1/rpc/{function}", "postgrest.rpc"),
            RoutePattern::new("GET", "/rest/v1/", "postgrest.root"),
        ]
    }

    /// Auth API routes
    pub fn auth_routes() -> Vec<RoutePattern> {
        vec![
            RoutePattern::new("POST", "/auth/v1/signup", "auth.signup"),
            RoutePattern::new("POST", "/auth/v1/token", "auth.token"),
            RoutePattern::new("POST", "/auth/v1/logout", "auth.logout"),
            RoutePattern::new("GET", "/auth/v1/user", "auth.user"),
            RoutePattern::new("PUT", "/auth/v1/user", "auth.update_user"),
            RoutePattern::new("POST", "/auth/v1/recover", "auth.recover"),
            RoutePattern::new("POST", "/auth/v1/verify", "auth.verify"),
            RoutePattern::new("POST", "/auth/v1/otp", "auth.otp"),
            RoutePattern::new("GET", "/auth/v1/authorize", "auth.authorize"),
            RoutePattern::new("POST", "/auth/v1/token?grant_type=refresh_token", "auth.refresh"),
            RoutePattern::new("GET", "/auth/v1/settings", "auth.settings"),
            RoutePattern::new("GET", "/auth/v1/health", "auth.health"),
        ]
    }

    /// Storage API routes
    pub fn storage_routes() -> Vec<RoutePattern> {
        vec![
            // Buckets
            RoutePattern::new("GET", "/storage/v1/bucket", "storage.list_buckets"),
            RoutePattern::new("POST", "/storage/v1/bucket", "storage.create_bucket"),
            RoutePattern::new("GET", "/storage/v1/bucket/{id}", "storage.get_bucket"),
            RoutePattern::new("PUT", "/storage/v1/bucket/{id}", "storage.update_bucket"),
            RoutePattern::new("DELETE", "/storage/v1/bucket/{id}", "storage.delete_bucket"),
            RoutePattern::new("POST", "/storage/v1/bucket/{id}/empty", "storage.empty_bucket"),

            // Objects
            RoutePattern::new("POST", "/storage/v1/object/{bucket}/{path:*}", "storage.upload"),
            RoutePattern::new("PUT", "/storage/v1/object/{bucket}/{path:*}", "storage.update"),
            RoutePattern::new("GET", "/storage/v1/object/{bucket}/{path:*}", "storage.download"),
            RoutePattern::new("DELETE", "/storage/v1/object/{bucket}", "storage.delete"),
            RoutePattern::new("POST", "/storage/v1/object/list/{bucket}", "storage.list"),
            RoutePattern::new("POST", "/storage/v1/object/move", "storage.move"),
            RoutePattern::new("POST", "/storage/v1/object/copy", "storage.copy"),

            // Signed URLs
            RoutePattern::new("POST", "/storage/v1/object/sign/{bucket}/{path:*}", "storage.sign"),
            RoutePattern::new("GET", "/storage/v1/object/sign/{bucket}/{path:*}", "storage.get_signed"),
            RoutePattern::new("POST", "/storage/v1/object/upload/sign/{bucket}/{path:*}", "storage.upload_sign"),

            // Public URLs
            RoutePattern::new("GET", "/storage/v1/object/public/{bucket}/{path:*}", "storage.public"),

            // Render (image transformations)
            RoutePattern::new("GET", "/storage/v1/render/image/{bucket}/{path:*}", "storage.render"),
        ]
    }

    /// Realtime API routes
    pub fn realtime_routes() -> Vec<RoutePattern> {
        vec![
            RoutePattern::new("GET", "/realtime/v1/websocket", "realtime.websocket"),
            RoutePattern::new("GET", "/realtime/v1/health", "realtime.health"),
        ]
    }
}

/// Route pattern
#[derive(Debug, Clone)]
pub struct RoutePattern {
    pub method: String,
    pub pattern: String,
    pub handler: String,
}

impl RoutePattern {
    pub fn new(method: &str, pattern: &str, handler: &str) -> Self {
        Self {
            method: method.to_string(),
            pattern: pattern.to_string(),
            handler: handler.to_string(),
        }
    }

    /// Match path against pattern and extract params
    pub fn matches(&self, method: &str, path: &str) -> Option<HashMap<String, String>> {
        if self.method != method && self.method != "*" {
            return None;
        }

        let pattern_parts: Vec<&str> = self.pattern.split('/').filter(|s| !s.is_empty()).collect();
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        // Handle wildcard paths
        let has_wildcard = pattern_parts.iter().any(|p| p.ends_with(":*}"));

        if !has_wildcard && pattern_parts.len() != path_parts.len() {
            return None;
        }

        let mut params = HashMap::new();
        let mut path_idx = 0;

        for pattern_part in &pattern_parts {
            if path_idx >= path_parts.len() {
                return None;
            }

            if pattern_part.starts_with('{') && pattern_part.ends_with('}') {
                let param_name = &pattern_part[1..pattern_part.len()-1];

                if param_name.ends_with(":*") {
                    // Wildcard - capture rest of path
                    let name = &param_name[..param_name.len()-2];
                    let value = path_parts[path_idx..].join("/");
                    params.insert(name.to_string(), value);
                    return Some(params);
                } else {
                    params.insert(param_name.to_string(), path_parts[path_idx].to_string());
                }
            } else if *pattern_part != path_parts[path_idx] {
                return None;
            }

            path_idx += 1;
        }

        Some(params)
    }
}

/// Supabase client configuration (for SDK generation)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupabaseClientConfig {
    pub url: String,
    pub anon_key: String,
    pub service_role_key: Option<String>,
}

/// Generate JavaScript client initialization code
pub fn generate_js_client(config: &SupabaseClientConfig) -> String {
    format!(
        r#"import {{ createClient }} from '@supabase/supabase-js'

const supabaseUrl = '{}'
const supabaseKey = '{}'

export const supabase = createClient(supabaseUrl, supabaseKey)

// Alternative: HeliosDB-Lite native client
import {{ HeliosDB }} from '@heliosdb/client'

export const helios = new HeliosDB({{
  url: supabaseUrl,
  apiKey: supabaseKey,
  // Additional HeliosDB features:
  branches: true,
  vectorSearch: true,
  agentMemory: true
}})
"#,
        config.url,
        config.anon_key
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_route_matching() {
        let route = RoutePattern::new("GET", "/rest/v1/{table}", "postgrest.select");

        let params = route.matches("GET", "/rest/v1/users");
        assert!(params.is_some());
        assert_eq!(params.unwrap().get("table"), Some(&"users".to_string()));

        assert!(route.matches("POST", "/rest/v1/users").is_none());
        assert!(route.matches("GET", "/rest/v1/").is_none());
    }

    #[test]
    fn test_wildcard_route() {
        let route = RoutePattern::new("GET", "/storage/v1/object/{bucket}/{path:*}", "storage.download");

        let params = route.matches("GET", "/storage/v1/object/mybucket/path/to/file.txt");
        assert!(params.is_some());
        let params = params.unwrap();
        assert_eq!(params.get("bucket"), Some(&"mybucket".to_string()));
        assert_eq!(params.get("path"), Some(&"path/to/file.txt".to_string()));
    }

    #[test]
    fn test_prefer_header() {
        let prefer = PreferHeader::parse("return=representation, count=exact");
        assert!(matches!(prefer.return_type, Some(ReturnType::Representation)));
        assert!(matches!(prefer.count, Some(CountType::Exact)));
    }
}
