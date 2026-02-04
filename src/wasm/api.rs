//! WASM HTTP API
//!
//! HTTP-like API layer for WASM environments that can work with
//! fetch() in browsers and edge workers.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// HTTP Method
#[derive(Debug, Clone, PartialEq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

impl From<&str> for Method {
    fn from(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "PATCH" => Method::Patch,
            _ => Method::Get,
        }
    }
}

/// HTTP Request representation
#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Option<Vec<u8>>,
}

impl Request {
    pub fn new(method: Method, path: &str) -> Self {
        Self {
            method,
            path: path.to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        }
    }

    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = Some(body);
        self
    }

    pub fn with_header(mut self, key: &str, value: &str) -> Self {
        self.headers.insert(key.to_string(), value.to_string());
        self
    }

    pub fn with_query(mut self, key: &str, value: &str) -> Self {
        self.query.insert(key.to_string(), value.to_string());
        self
    }

    /// Parse JSON body
    pub fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, ApiError> {
        let body = self.body.as_ref().ok_or(ApiError::BadRequest("Missing body".into()))?;
        serde_json::from_slice(body).map_err(|e| ApiError::BadRequest(e.to_string()))
    }
}

/// HTTP Response representation
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        Self {
            status,
            headers,
            body: Vec::new(),
        }
    }

    pub fn ok() -> Self {
        Self::new(200)
    }

    pub fn created() -> Self {
        Self::new(201)
    }

    pub fn no_content() -> Self {
        Self::new(204)
    }

    pub fn bad_request() -> Self {
        Self::new(400)
    }

    pub fn not_found() -> Self {
        Self::new(404)
    }

    pub fn internal_error() -> Self {
        Self::new(500)
    }

    pub fn with_json<T: Serialize>(mut self, data: &T) -> Self {
        self.body = serde_json::to_vec(data).unwrap_or_default();
        self
    }

    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn with_header(mut self, key: &str, value: &str) -> Self {
        self.headers.insert(key.to_string(), value.to_string());
        self
    }
}

/// API Error types
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Unauthorized")]
    Unauthorized,
}

impl From<ApiError> for Response {
    fn from(error: ApiError) -> Self {
        let (status, message) = match &error {
            ApiError::BadRequest(msg) => (400, msg.clone()),
            ApiError::NotFound(msg) => (404, msg.clone()),
            ApiError::Internal(msg) => (500, msg.clone()),
            ApiError::Unauthorized => (401, "Unauthorized".to_string()),
        };

        Response::new(status).with_json(&serde_json::json!({
            "error": message
        }))
    }
}

/// Router for handling HTTP-like requests in WASM
pub struct Router {
    routes: Vec<Route>,
}

struct Route {
    method: Method,
    pattern: String,
    handler: Box<dyn Fn(Request, RouteParams) -> Response + Send + Sync>,
}

/// Route parameters extracted from path
pub type RouteParams = HashMap<String, String>;

impl Router {
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    /// Add a GET route
    pub fn get<F>(mut self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request, RouteParams) -> Response + Send + Sync + 'static,
    {
        self.routes.push(Route {
            method: Method::Get,
            pattern: pattern.to_string(),
            handler: Box::new(handler),
        });
        self
    }

    /// Add a POST route
    pub fn post<F>(mut self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request, RouteParams) -> Response + Send + Sync + 'static,
    {
        self.routes.push(Route {
            method: Method::Post,
            pattern: pattern.to_string(),
            handler: Box::new(handler),
        });
        self
    }

    /// Add a PUT route
    pub fn put<F>(mut self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request, RouteParams) -> Response + Send + Sync + 'static,
    {
        self.routes.push(Route {
            method: Method::Put,
            pattern: pattern.to_string(),
            handler: Box::new(handler),
        });
        self
    }

    /// Add a DELETE route
    pub fn delete<F>(mut self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request, RouteParams) -> Response + Send + Sync + 'static,
    {
        self.routes.push(Route {
            method: Method::Delete,
            pattern: pattern.to_string(),
            handler: Box::new(handler),
        });
        self
    }

    /// Handle incoming request
    pub fn handle(&self, request: Request) -> Response {
        for route in &self.routes {
            if route.method == request.method {
                if let Some(params) = self.match_pattern(&route.pattern, &request.path) {
                    return (route.handler)(request, params);
                }
            }
        }

        Response::not_found().with_json(&serde_json::json!({
            "error": "Route not found"
        }))
    }

    /// Match path against pattern and extract params
    fn match_pattern(&self, pattern: &str, path: &str) -> Option<RouteParams> {
        let pattern_parts: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if pattern_parts.len() != path_parts.len() {
            return None;
        }

        let mut params = RouteParams::new();

        for (pattern_part, path_part) in pattern_parts.iter().zip(path_parts.iter()) {
            if pattern_part.starts_with(':') {
                let param_name = &pattern_part[1..];
                params.insert(param_name.to_string(), path_part.to_string());
            } else if pattern_part != path_part {
                return None;
            }
        }

        Some(params)
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the HeliosDB API router
pub fn build_api_router() -> Router {
    Router::new()
        // Health check
        .get("/health", |_, _| {
            Response::ok().with_json(&serde_json::json!({
                "status": "healthy",
                "version": env!("CARGO_PKG_VERSION")
            }))
        })
        // Query endpoint
        .post("/v1/branches/:branch/query", |req, params| {
            let branch = params.get("branch").cloned().unwrap_or_else(|| "main".to_string());

            #[derive(Deserialize)]
            struct QueryBody {
                sql: String,
                #[serde(default)]
                params: Vec<serde_json::Value>,
            }

            match req.json::<QueryBody>() {
                Ok(body) => {
                    // Execute query (would use WasmRuntime)
                    Response::ok().with_json(&serde_json::json!({
                        "branch": branch,
                        "sql": body.sql,
                        "rows": [],
                        "columns": []
                    }))
                }
                Err(e) => e.into(),
            }
        })
        // List tables
        .get("/v1/branches/:branch/tables", |_, params| {
            let branch = params.get("branch").cloned().unwrap_or_else(|| "main".to_string());
            Response::ok().with_json(&serde_json::json!({
                "branch": branch,
                "tables": []
            }))
        })
        // Insert data
        .post("/v1/branches/:branch/tables/:table/data", |req, params| {
            let branch = params.get("branch").cloned().unwrap_or_else(|| "main".to_string());
            let table = params.get("table").cloned().unwrap_or_default();

            #[derive(Deserialize)]
            struct InsertBody {
                rows: Vec<serde_json::Value>,
            }

            match req.json::<InsertBody>() {
                Ok(body) => {
                    Response::created().with_json(&serde_json::json!({
                        "branch": branch,
                        "table": table,
                        "inserted": body.rows.len()
                    }))
                }
                Err(e) => e.into(),
            }
        })
        // Vector stores
        .get("/v1/vectors/stores", |_, _| {
            Response::ok().with_json(&serde_json::json!({
                "stores": []
            }))
        })
        .post("/v1/vectors/stores", |req, _| {
            #[derive(Deserialize)]
            struct CreateStoreBody {
                name: String,
                #[serde(default = "default_dimensions")]
                dimensions: usize,
                #[serde(default = "default_metric")]
                metric: String,
            }

            fn default_dimensions() -> usize { 1536 }
            fn default_metric() -> String { "cosine".to_string() }

            match req.json::<CreateStoreBody>() {
                Ok(body) => {
                    Response::created().with_json(&serde_json::json!({
                        "name": body.name,
                        "dimensions": body.dimensions,
                        "metric": body.metric
                    }))
                }
                Err(e) => e.into(),
            }
        })
        // Vector search
        .post("/v1/vectors/stores/:store/search", |req, params| {
            let store = params.get("store").cloned().unwrap_or_default();

            #[derive(Deserialize)]
            struct SearchBody {
                vector: Vec<f32>,
                #[serde(default = "default_top_k")]
                top_k: usize,
            }

            fn default_top_k() -> usize { 10 }

            match req.json::<SearchBody>() {
                Ok(_body) => {
                    Response::ok().with_json(&serde_json::json!({
                        "store": store,
                        "results": []
                    }))
                }
                Err(e) => e.into(),
            }
        })
        // Text search
        .post("/v1/vectors/stores/:store/search/text", |req, params| {
            let store = params.get("store").cloned().unwrap_or_default();

            #[derive(Deserialize)]
            struct TextSearchBody {
                text: String,
                #[serde(default = "default_top_k")]
                top_k: usize,
            }

            fn default_top_k() -> usize { 10 }

            match req.json::<TextSearchBody>() {
                Ok(_body) => {
                    Response::ok().with_json(&serde_json::json!({
                        "store": store,
                        "results": []
                    }))
                }
                Err(e) => e.into(),
            }
        })
        // Agent memory
        .post("/v1/agents/memory/:session/add", |req, params| {
            let session = params.get("session").cloned().unwrap_or_default();

            #[derive(Deserialize)]
            struct AddMessageBody {
                role: String,
                content: String,
            }

            match req.json::<AddMessageBody>() {
                Ok(body) => {
                    Response::created().with_json(&serde_json::json!({
                        "session_id": session,
                        "role": body.role,
                        "content": body.content
                    }))
                }
                Err(e) => e.into(),
            }
        })
        .get("/v1/agents/memory/:session/messages", |_, params| {
            let session = params.get("session").cloned().unwrap_or_default();
            Response::ok().with_json(&serde_json::json!({
                "session_id": session,
                "messages": []
            }))
        })
        // Branches
        .get("/v1/branches", |_, _| {
            Response::ok().with_json(&serde_json::json!({
                "branches": [
                    {"name": "main", "default": true}
                ]
            }))
        })
        .post("/v1/branches", |req, _| {
            #[derive(Deserialize)]
            struct CreateBranchBody {
                name: String,
                #[serde(default = "default_branch")]
                from_branch: String,
            }

            fn default_branch() -> String { "main".to_string() }

            match req.json::<CreateBranchBody>() {
                Ok(body) => {
                    Response::created().with_json(&serde_json::json!({
                        "name": body.name,
                        "from_branch": body.from_branch
                    }))
                }
                Err(e) => e.into(),
            }
        })
}

/// Handle fetch event (for Cloudflare Workers, etc.)
pub fn handle_fetch(method: &str, url: &str, body: Option<&[u8]>, headers: HashMap<String, String>) -> Response {
    let router = build_api_router();

    // Parse URL to extract path and query
    let (path, query) = parse_url(url);

    let mut request = Request::new(Method::from(method), &path);
    request.headers = headers;
    request.query = query;
    request.body = body.map(|b| b.to_vec());

    router.handle(request)
}

/// Parse URL into path and query parameters
fn parse_url(url: &str) -> (String, HashMap<String, String>) {
    let mut query = HashMap::new();

    // Strip protocol and host if present
    let path_start = url.find("://")
        .map(|i| url[i + 3..].find('/').map(|j| i + 3 + j).unwrap_or(url.len()))
        .unwrap_or(0);

    let path_and_query = &url[path_start..];

    if let Some(q_pos) = path_and_query.find('?') {
        let path = path_and_query[..q_pos].to_string();
        let query_str = &path_and_query[q_pos + 1..];

        for pair in query_str.split('&') {
            if let Some(eq_pos) = pair.find('=') {
                let key = &pair[..eq_pos];
                let value = &pair[eq_pos + 1..];
                query.insert(key.to_string(), value.to_string());
            }
        }

        (path, query)
    } else {
        (path_and_query.to_string(), query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_url() {
        let (path, query) = parse_url("/v1/branches/main/query?limit=10");
        assert_eq!(path, "/v1/branches/main/query");
        assert_eq!(query.get("limit"), Some(&"10".to_string()));
    }

    #[test]
    fn test_route_matching() {
        let router = Router::new()
            .get("/v1/branches/:branch/tables", |_, params| {
                Response::ok().with_json(&serde_json::json!({
                    "branch": params.get("branch")
                }))
            });

        let request = Request::new(Method::Get, "/v1/branches/main/tables");
        let response = router.handle(request);
        assert_eq!(response.status, 200);
    }
}
