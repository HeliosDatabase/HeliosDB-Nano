#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! Feature truth, REST: no route may be mounted that only pretends.
//!
//! The routes are read from the source of `src/api/routes/*.rs`, so a new route is in
//! this test the moment it is mounted. Each one is asked once, and its handler is read.
//! A route is a stub when
//!
//! - it answers 501, or its answer says "not implemented" (asked), or
//! - its handler's own text says the work is not done (read): the markers below.
//!
//! Every stub must be in `tests/feature_truth/rest_routes.txt` under `[known_stubs]`
//! with the package that owns it and how it shows, and every entry there must still be
//! a stub: a package that finishes a route removes its line, and a route that starts to
//! pretend fails this test. `[not_stubs]` holds handlers whose 501 refuses one option
//! or a feature that is switched off.

#[path = "feature_truth_support/lists.rs"]
mod lists;

use axum::body::Body;
use axum::http::Request;
use heliosdb_nano::api::server::AppState;
use heliosdb_nano::EmbeddedDatabase;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;

/// Text in an answer that says the route did nothing.
const ANSWER_MARKERS: &[&str] = &["not yet implemented", "not implemented", "not yet fully implemented"];

/// Text in a handler that says the handler is not finished.
const SOURCE_MARKERS: &[&str] = &[
    "NOT_IMPLEMENTED",
    "not yet implemented",
    "not implemented",
    "not yet fully implemented",
    "doesn't exist yet",
    "Placeholder",
    "placeholder",
    "For now",
    "In a real implementation",
    "In production",
    "mock provider",
];

#[derive(Debug, Clone)]
struct Route {
    file: String,
    method: String,
    path: String,
    /// `module::function`, or `function` for a handler in the routes file itself.
    handler: String,
}

impl Route {
    fn key(&self) -> String {
        format!("{} {} {}", self.method, self.file, self.path)
    }
}

fn source(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The text between a `(` at `open` and its matching `)`.
fn balanced(text: &str, open: usize) -> &str {
    let mut depth = 0usize;
    for (i, c) in text[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &text[open + 1..open + i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced parenthesis at byte {open}");
}

/// Every `.route("path", method(handler)…)` of one routes file, outside its tests.
fn routes_of(file: &str) -> Vec<Route> {
    let text = source(&format!("src/api/routes/{file}.rs"));
    let text = text.split("#[cfg(test)]").next().unwrap();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(".route(") {
        let open = from + at + ".route".len();
        let inner = balanced(text, open);
        from = open + inner.len();
        let first_quote = inner.find('"').expect("a route path");
        let path_end = inner[first_quote + 1..].find('"').unwrap() + first_quote + 1;
        let path = &inner[first_quote + 1..path_end];
        let rest = &inner[path_end + 1..];
        for method in ["get", "post", "put", "patch", "delete"] {
            let mut search = 0;
            while let Some(hit) = rest[search..].find(&format!("{method}(")) {
                let start = search + hit;
                search = start + method.len();
                let before = rest[..start].chars().next_back();
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let handler = balanced(rest, start + method.len()).trim();
                let handler = handler.rsplit("handlers::").next().unwrap().trim().to_string();
                out.push(Route {
                    file: file.to_string(),
                    method: method.to_uppercase(),
                    path: path.to_string(),
                    handler,
                });
            }
        }
    }
    out
}

fn route_files() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api/routes");
    let mut files: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter_map(|n| n.strip_suffix(".rs").map(str::to_string))
        .filter(|n| n != "mod")
        .collect();
    files.sort();
    files
}

fn router_of(file: &str) -> axum::Router<AppState> {
    use heliosdb_nano::api::routes;
    match file {
        "agents" => routes::agents::routes(),
        "branches" => routes::branches::routes(),
        "cancellation" => routes::cancellation::routes(),
        "chat" => routes::chat::routes(),
        "data" => routes::data::routes(),
        "documents" => routes::documents::routes(),
        "query" => routes::query::routes(),
        "rest" => routes::rest::routes(),
        "schema" => routes::schema::routes(),
        "vectors" => routes::vectors::routes(),
        "webhooks" => routes::webhooks::routes(),
        other => panic!("src/api/routes/{other}.rs is new: add it to router_of in this test"),
    }
}

/// The body of `fn name` in `text`: from the brace after its parameter list to the
/// matching brace.
fn function_body<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let at = text.find(&format!("fn {name}("))?;
    let open = at + format!("fn {name}").len();
    let params = balanced(text, open);
    let after = open + params.len() + 2;
    let brace = after + text[after..].find('{')?;
    let mut depth = 0usize;
    for (i, c) in text[brace..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[brace..=brace + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The marker a handler's own text carries, if any.
fn source_marker(route: &Route) -> Option<&'static str> {
    let (text, name) = match route.handler.split_once("::") {
        Some((module, name)) => (source(&format!("src/api/handlers/{module}.rs")), name.to_string()),
        None => (
            source(&format!("src/api/routes/{}.rs", route.file)),
            route.handler.clone(),
        ),
    };
    let body = function_body(&text, &name)
        .unwrap_or_else(|| panic!("handler {} of {} not found in its source", route.handler, route.key()));
    SOURCE_MARKERS.iter().copied().find(|m| body.contains(m))
}

fn state() -> AppState {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE ft_items (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("INSERT INTO ft_items VALUES (1, 'one')").unwrap();
    AppState {
        db,
        query_registry: Arc::new(heliosdb_nano::compute::QueryRegistry::new()),
        auth_bridge: None,
        oauth_registry: None,
        change_notifier: None,
    }
}

/// A small request for a route: path parameters filled in, and a body where one is taken.
fn request_for(route: &Route) -> Request<Body> {
    let uri: String = route
        .path
        .split('/')
        .map(|segment| match segment {
            ":name" => "main",
            ":table" => "ft_items",
            ":function" => "version",
            s if s.starts_with(':') => "x1",
            s => s,
        })
        .collect::<Vec<_>>()
        .join("/");
    let body = match (route.file.as_str(), route.method.as_str(), route.path.as_str()) {
        ("query", "POST", _) => r#"{"sql":"SELECT 1"}"#,
        ("schema", "POST", "/compare") => r#"{"source":"CREATE TABLE a (x INT)","target":"CREATE TABLE b (y TEXT)"}"#,
        (_, "GET" | "DELETE", _) => "",
        _ => "{}",
    };
    let mut builder = Request::builder().method(route.method.as_str()).uri(uri);
    if !body.is_empty() {
        builder = builder.header("content-type", "application/json");
    }
    builder.body(Body::from(body)).unwrap()
}

/// How a route shows as a stub, if it does: `answers 501`, `answers "<marker>"`, or
/// `handler says "<marker>"`.
async fn stub_evidence(route: &Route) -> Option<String> {
    let router: axum::Router<()> = router_of(&route.file).with_state(state());
    let response = router.oneshot(request_for(route)).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 22).await.unwrap();
    let text = String::from_utf8_lossy(&body).to_lowercase();
    if status == axum::http::StatusCode::NOT_IMPLEMENTED {
        return Some("answers 501".to_string());
    }
    if let Some(marker) = ANSWER_MARKERS.iter().find(|m| text.contains(**m)) {
        return Some(format!("answers \"{marker}\""));
    }
    source_marker(route).map(|marker| format!("handler says \"{marker}\""))
}

#[test]
fn the_route_reader_finds_every_form() {
    let data = routes_of("data");
    let keys: BTreeSet<String> = data.iter().map(Route::key).collect();
    for expected in [
        "GET data /:name/tables",
        "GET data /:name/tables/:table/data",
        "POST data /:name/tables/:table/data",
        "PUT data /:name/tables/:table/data",
        "DELETE data /:name/tables/:table/data",
    ] {
        assert!(keys.contains(expected), "{expected} not found in {keys:?}");
    }
    assert_eq!(data.len(), 5);
    assert!(routes_of("rest")
        .iter()
        .any(|r| r.method == "PATCH" && r.handler == "rest_handler::rest_update"));
    assert!(routes_of("webhooks")
        .iter()
        .any(|r| r.method == "GET" && r.handler == "health_check"));
    let all: usize = route_files().iter().map(|f| routes_of(f).len()).sum();
    assert!(all >= 70, "only {all} routes found");
    let text = "async fn a(State(AppState { db, .. }): State<AppState>) -> X { if x { 1 } else { 2 } }\nfn b() {}";
    assert_eq!(function_body(text, "a"), Some("{ if x { 1 } else { 2 } }"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_mounted_route_does_something_or_is_a_known_stub() {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    let mut all: BTreeSet<String> = BTreeSet::new();
    for file in route_files() {
        for route in routes_of(&file) {
            all.insert(route.key());
            if let Some(evidence) = stub_evidence(&route).await {
                found.insert(route.key(), evidence);
            }
        }
    }
    let file = lists::load("rest_routes", 1);
    let known: BTreeMap<&str, (&str, &str)> = lists::section(&file, "known_stubs")
        .iter()
        .map(|e| (e.key.as_str(), (e.field(1), e.field(0))))
        .collect();
    let excused: BTreeMap<&str, &str> = lists::section(&file, "not_stubs")
        .iter()
        .map(|e| (e.key.as_str(), e.field(0)))
        .collect();

    let mut problems = Vec::new();
    for (key, evidence) in &found {
        match (known.get(key.as_str()), excused.get(key.as_str())) {
            (Some(_), Some(_)) => problems.push(format!("{key}: in both lists")),
            (Some((how, _)), None) if how != evidence => {
                problems.push(format!("{key}: listed as `{how}`, it now shows as `{evidence}`"))
            }
            (Some(_), None) => {}
            (None, Some(_)) if evidence.starts_with("handler says") || evidence == "answers 501" => {}
            (None, _) => problems.push(format!("new stub: {key} | <owner> | {evidence}")),
        }
    }
    for (key, (_, owner)) in &known {
        if !all.contains(*key) {
            problems.push(format!("{key}: listed for {owner}, no such route is mounted"));
        } else if !found.contains_key(*key) {
            problems.push(format!(
                "{key}: no longer a stub: remove its line from rest_routes.txt (owner {owner})"
            ));
        }
    }
    for key in excused.keys() {
        if !all.contains(*key) {
            problems.push(format!("{key}: in [not_stubs], no such route is mounted"));
        } else if !found.contains_key(*key) {
            problems.push(format!("{key}: in [not_stubs] and shows nothing: remove it"));
        }
    }
    assert!(
        problems.is_empty(),
        "{} routes, {} stubs listed, {} problems:\n{}",
        all.len(),
        known.len(),
        problems.len(),
        problems.join("\n")
    );
}
