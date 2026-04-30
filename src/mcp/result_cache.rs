//! Server-side LRU cache for repeated MCP `tools/call` requests.
//!
//! Closes `FEATURE_REQUEST_tool_result_caching.md`. Within an
//! agentic-coding session the same `helios_lsp_definition` /
//! `helios_graphrag_search` / `heliosdb_hybrid_search` is often called
//! several times in a row (refactor-then-verify loops, Read-then-confirm
//! patterns). Each call re-parses, re-traverses, re-ranks. This cache
//! makes subsequent identical calls ~1 ms.
//!
//! ## Design
//!
//! - **Per-process LRU** keyed by `(tool_name, canonicalised_args)`,
//!   bounded at [`CACHE_CAPACITY`] entries.
//! - **TTL** of [`CACHE_TTL`]. Entries past their TTL are dropped on
//!   read.
//! - **Generation counter** bumped on every call to a tool listed in
//!   [`writes`]; cached entries from a prior generation are
//!   unreachable and evict via LRU.
//! - **Read-only allow-list** — only tools in [`read_only`] are
//!   considered cacheable; everything else (including `heliosdb_query`
//!   which can be SELECT or DML) bypasses the cache entirely.
//! - **Argument canonicalisation** — JSON object keys sorted
//!   recursively; `_meta` / `_meta.progressToken` keys dropped so that
//!   the same logical call deduplicates regardless of progress
//!   token.
//! - **Errors are not cached** — a transient error shouldn't
//!   poison-pill the cache; the next call retries.
//!
//! ## What this is NOT
//!
//! - Not cross-process. RAM-only.
//! - Not persistent across restarts.
//! - Not a replacement for the engine's plan / parse / row caches —
//!   complements them at the MCP-tool boundary.

use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lru::LruCache;
use once_cell::sync::Lazy;
use serde_json::Value as JsonValue;

use super::tools::ToolOutcome;

/// Default LRU capacity. Tunable via env `HELIOS_MCP_CACHE_CAPACITY`
/// at process start; mutating after the cache is built is not
/// supported in v1.
pub const CACHE_CAPACITY: usize = 256;

/// Default TTL. Past this, entries are dropped on read regardless of
/// generation. Five minutes is the same TTL the rest of the engine
/// uses for its plan / parse / row caches.
pub const CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone)]
struct CacheEntry {
    outcome: ToolOutcome,
    inserted_at: Instant,
    generation: u64,
}

struct Cache {
    lru: LruCache<String, CacheEntry>,
    generation: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
}

static CACHE: Lazy<Mutex<Cache>> = Lazy::new(|| {
    let cap = std::env::var("HELIOS_MCP_CACHE_CAPACITY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(CACHE_CAPACITY);
    let cap = NonZeroUsize::new(cap.max(1))
        .unwrap_or_else(|| NonZeroUsize::new(CACHE_CAPACITY).expect("CACHE_CAPACITY > 0"));
    Mutex::new(Cache {
        lru: LruCache::new(cap),
        generation: 0,
        hits: 0,
        misses: 0,
        evictions: 0,
    })
});

/// Tools whose results are safe to cache. Read-only against
/// `_hdb_code_*`, `_hdb_graph_*`, `pg_catalog`, etc.
pub fn read_only(tool_name: &str) -> bool {
    matches!(
        tool_name,
        // Engine-DB read tools
        "heliosdb_schema"
            | "heliosdb_list_tables"
            | "heliosdb_branch_list"
            | "heliosdb_search"
            | "heliosdb_time_travel"
            | "heliosdb_hybrid_search"
            | "heliosdb_graph_traverse"
            | "heliosdb_graph_path"
            // LSP-shaped tools — all reads against `_hdb_code_*`
            | "helios_lsp_definition"
            | "helios_lsp_references"
            | "helios_lsp_call_hierarchy"
            | "helios_lsp_hover"
            | "helios_lsp_document_symbols"
            | "helios_lsp_rename_preview"
            | "helios_lsp_references_diff"
            | "helios_lsp_body_diff"
            | "helios_ast_diff"
            // Cross-modal read
            | "helios_graphrag_search"
    )
}

/// Tools that mutate engine state. A call to any of these bumps the
/// generation counter, making all prior cache entries unreachable.
/// `heliosdb_query` is intentionally **not** here — it could be a
/// SELECT or a DML. Treated as never-cache, never-invalidate (the
/// safe default; a DML through `heliosdb_query` will still write
/// through to the engine, and the next read against affected tables
/// just won't get a cache hit because that read tool wasn't called
/// during this generation).
pub fn writes(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "heliosdb_create_table"
            | "heliosdb_insert"
            | "heliosdb_branch_create"
            | "heliosdb_branch_merge"
            | "heliosdb_graph_add_edge"
            | "heliosdb_embed_and_store"
            | "heliosdb_bm25_index"
            | "helios_lsp_rename_apply"
    )
}

/// Recursively sort JSON object keys + drop `_meta` so identical
/// calls dedupe regardless of progress-token noise.
fn canonicalise(v: &JsonValue) -> JsonValue {
    match v {
        JsonValue::Object(map) => {
            let mut entries: Vec<(&String, &JsonValue)> = map
                .iter()
                .filter(|(k, _)| !k.starts_with("_meta"))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let new_map: serde_json::Map<String, JsonValue> = entries
                .into_iter()
                .map(|(k, v)| (k.clone(), canonicalise(v)))
                .collect();
            JsonValue::Object(new_map)
        }
        JsonValue::Array(arr) => JsonValue::Array(arr.iter().map(canonicalise).collect()),
        other => other.clone(),
    }
}

fn cache_key(tool_name: &str, args: &JsonValue) -> String {
    format!("{tool_name}::{}", canonicalise(args))
}

/// Look up `(tool_name, args)` in the cache. Returns `None` for
/// non-read-only tools, on miss, on TTL expiry, or on
/// generation-mismatch.
pub fn try_get(tool_name: &str, args: &JsonValue) -> Option<ToolOutcome> {
    if !read_only(tool_name) {
        return None;
    }
    let key = cache_key(tool_name, args);
    let mut guard = CACHE.lock().ok()?;
    let current_gen = guard.generation;
    let entry = match guard.lru.get(&key) {
        Some(e) => e.clone(),
        None => {
            guard.misses += 1;
            return None;
        }
    };
    if entry.inserted_at.elapsed() > CACHE_TTL {
        guard.lru.pop(&key);
        guard.misses += 1;
        guard.evictions += 1;
        return None;
    }
    if entry.generation != current_gen {
        guard.lru.pop(&key);
        guard.misses += 1;
        guard.evictions += 1;
        return None;
    }
    guard.hits += 1;
    Some(entry.outcome)
}

/// Insert `(tool_name, args, outcome)` into the cache. No-op for
/// non-read-only tools or for error outcomes (transient errors
/// shouldn't poison-pill the cache).
pub fn insert(tool_name: &str, args: &JsonValue, outcome: &ToolOutcome) {
    if !read_only(tool_name) || outcome.is_error {
        return;
    }
    let key = cache_key(tool_name, args);
    let mut guard = match CACHE.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let generation = guard.generation;
    guard.lru.put(
        key,
        CacheEntry {
            outcome: outcome.clone(),
            inserted_at: Instant::now(),
            generation,
        },
    );
}

/// Bump the generation counter. Called automatically when a
/// [`writes`] tool is invoked. All prior entries become
/// unreachable; LRU evicts on the next pass.
pub fn invalidate_for_writes() {
    if let Ok(mut guard) = CACHE.lock() {
        guard.generation = guard.generation.wrapping_add(1);
    }
}

/// Snapshot of cache statistics. Surfaced via `helios/info`.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct CacheStats {
    pub size: usize,
    pub capacity: usize,
    pub generation: u64,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

pub fn stats() -> CacheStats {
    match CACHE.lock() {
        Ok(g) => CacheStats {
            size: g.lru.len(),
            capacity: g.lru.cap().get(),
            generation: g.generation,
            hits: g.hits,
            misses: g.misses,
            evictions: g.evictions,
        },
        Err(_) => CacheStats {
            size: 0,
            capacity: 0,
            generation: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
        },
    }
}

/// Test-only: clear the cache (state lives in `static CACHE`).
#[doc(hidden)]
pub fn _clear_for_tests() {
    if let Ok(mut g) = CACHE.lock() {
        g.lru.clear();
        g.generation = 0;
        g.hits = 0;
        g.misses = 0;
        g.evictions = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok_outcome(v: JsonValue) -> ToolOutcome {
        ToolOutcome::ok(v)
    }

    #[test]
    fn read_only_classification() {
        assert!(read_only("helios_lsp_definition"));
        assert!(read_only("helios_graphrag_search"));
        assert!(read_only("heliosdb_hybrid_search"));
        assert!(!read_only("heliosdb_query")); // never-cache
        assert!(!read_only("helios_lsp_rename_apply"));
        assert!(!read_only("totally_unknown_tool"));
    }

    #[test]
    fn write_classification() {
        assert!(writes("helios_lsp_rename_apply"));
        assert!(writes("heliosdb_branch_create"));
        assert!(!writes("helios_lsp_definition"));
    }

    #[test]
    fn canonicalisation_sorts_keys_and_drops_meta() {
        let a = json!({ "z": 1, "a": 2, "_meta": { "progressToken": "x" } });
        let b = json!({ "a": 2, "z": 1 });
        assert_eq!(canonicalise(&a), canonicalise(&b));
    }

    #[test]
    fn miss_then_hit_then_invalidate() {
        _clear_for_tests();
        let args = json!({ "name": "foo" });
        // Miss
        assert!(try_get("helios_lsp_definition", &args).is_none());
        // Insert
        insert(
            "helios_lsp_definition",
            &args,
            &ok_outcome(json!({ "rows": [] })),
        );
        // Hit
        let hit = try_get("helios_lsp_definition", &args).expect("expected cache hit");
        assert_eq!(hit.payload, json!({ "rows": [] }));
        // Invalidate via write
        invalidate_for_writes();
        assert!(try_get("helios_lsp_definition", &args).is_none());
    }

    #[test]
    fn errors_not_cached() {
        _clear_for_tests();
        let args = json!({ "name": "foo" });
        insert(
            "helios_lsp_definition",
            &args,
            &ToolOutcome::err("boom"),
        );
        assert!(try_get("helios_lsp_definition", &args).is_none());
    }

    #[test]
    fn non_readonly_tools_bypass_cache() {
        _clear_for_tests();
        let args = json!({});
        insert("heliosdb_insert", &args, &ok_outcome(json!({})));
        assert!(try_get("heliosdb_insert", &args).is_none());
    }
}
