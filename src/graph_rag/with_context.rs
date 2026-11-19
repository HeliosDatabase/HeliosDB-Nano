//! `WITH CONTEXT` SQL clause — pre-parser surface for the graph-rag
//! seed → expand → rerank pipeline (FR 4 §4.4).
//!
//! Syntax accepted (case-insensitive):
//!
//! ```text
//! SELECT ... FROM ... WHERE ...
//! WITH CONTEXT (
//!     HOPS <n>,
//!     [ EDGES <kind1>|<kind2>|... ],
//!     [ DIRECTION in|out|both ],
//!     [ RERANK BY <expr> ],     -- accepted but unused in this MVP
//!     [ EXPAND_LIMIT <k> ],
//!     [ LIMIT <k> ]
//! );
//! ```
//!
//! The pre-parser strips the clause before Nano's planner sees the
//! SQL, remembers it in a [`WithContextOptions`], and the caller's
//! entry point (`EmbeddedDatabase::query`) dispatches to
//! [`graph_rag_expand_with_context`] to run the seed query and
//! expand.

use super::search::{expand_hops, graph_rag_search, load_nodes, Direction, GraphRagHit, GraphRagOptions};
use crate::{EmbeddedDatabase, Error, Result, Value};

/// Parsed form of `WITH CONTEXT (...)`. Pure data — detached from
/// the SQL string so execution paths can't accidentally re-parse.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WithContextOptions {
    pub hops: u32,
    pub edges: Vec<String>,
    pub direction: Option<Direction>,
    /// Text of the RERANK BY expression as written by the user.  MVP
    /// records it but does not evaluate — reranking is seed-distance
    /// based in phase 3.
    pub rerank_by: Option<String>,
    pub expand_limit: Option<usize>,
    pub limit: Option<usize>,
}

/// Detect a trailing `WITH CONTEXT ( ... )` clause on `sql`.  Returns
/// `(stripped_sql, opts)` on match or `None` when the SQL has no
/// clause.
pub fn detect_with_context(sql: &str) -> Option<(String, WithContextOptions)> {
    let s = sql.trim().trim_end_matches(';');
    let lower = s.to_ascii_lowercase();
    // Look for last occurrence of `with context` that is at top level
    // (not inside a paren or string).  Because WITH CONTEXT attaches
    // to the outermost SELECT, the straightforward last-match is
    // usually correct.
    let idx = find_toplevel(&lower, s, "with context")?;
    // Require `(` immediately after (whitespace allowed).
    let after = &s[idx + "with context".len()..];
    let trimmed = after.trim_start();
    if !trimmed.starts_with('(') {
        return None;
    }
    let paren_start_in_after = after.len() - trimmed.len();
    let paren_open = idx + "with context".len() + paren_start_in_after;
    // Match balanced parens.
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut j = paren_open;
    let mut in_str = false;
    while j < bytes.len() {
        let b = bytes[j];
        if in_str {
            if b == b'\'' {
                if j + 1 < bytes.len() && bytes[j + 1] == b'\'' {
                    j += 2;
                    continue;
                }
                in_str = false;
            }
        } else {
            match b {
                b'\'' => in_str = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        j += 1;
    }
    if depth != 0 || j >= bytes.len() {
        return None;
    }
    let inner = &s[paren_open + 1..j];
    let opts = parse_options(inner)?;
    let mut stripped = String::new();
    stripped.push_str(&s[..idx]);
    stripped.push_str(&s[j + 1..]);
    Some((stripped.trim().to_string(), opts))
}

fn find_toplevel(lower: &str, original: &str, needle: &str) -> Option<usize> {
    let bytes = original.as_bytes();
    let lower_bytes = lower.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = 0;
    let mut last_hit: Option<usize> = None;
    while i + needle_bytes.len() <= bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_str = false;
            }
        } else {
            match b {
                b'\'' => in_str = true,
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {
                    if depth == 0 {
                        if lower_bytes.get(i..i + needle_bytes.len()) == Some(needle_bytes) {
                            let before_ok = i == 0 || {
                                let c = bytes[i - 1];
                                !c.is_ascii_alphanumeric() && c != b'_'
                            };
                            let after_idx = i + needle_bytes.len();
                            let after_ok = after_idx == bytes.len() || {
                                let c = bytes[after_idx];
                                !c.is_ascii_alphanumeric() && c != b'_'
                            };
                            if before_ok && after_ok {
                                last_hit = Some(i);
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }
    last_hit
}

fn parse_options(inner: &str) -> Option<WithContextOptions> {
    let mut opts = WithContextOptions::default();
    // Split on commas at depth 0.
    let parts = split_top_commas(inner);
    for raw in parts {
        let raw_trim = raw.trim();
        if raw_trim.is_empty() {
            continue;
        }
        let lower = raw_trim.to_ascii_lowercase();
        if let Some(n) = strip_prefix_ci(raw_trim, &lower, "hops") {
            opts.hops = n.trim().parse().ok()?;
        } else if let Some(n) = strip_prefix_ci(raw_trim, &lower, "expand_limit") {
            opts.expand_limit = Some(n.trim().parse().ok()?);
        } else if let Some(n) = strip_prefix_ci(raw_trim, &lower, "limit") {
            opts.limit = Some(n.trim().parse().ok()?);
        } else if let Some(v) = strip_prefix_ci(raw_trim, &lower, "edges") {
            opts.edges = v
                .split('|')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        } else if let Some(v) = strip_prefix_ci(raw_trim, &lower, "direction") {
            opts.direction = match v.trim().to_ascii_lowercase().as_str() {
                "in" => Some(Direction::In),
                "out" => Some(Direction::Out),
                "both" => Some(Direction::Both),
                _ => return None,
            };
        } else if lower.starts_with("rerank by") {
            // "RERANK BY <expr>" — take everything after the keyword.
            let rest = &raw_trim["rerank by".len()..];
            opts.rerank_by = Some(rest.trim().to_string());
        } else {
            // Forward-compat: unknown keys are ignored (not an error).
        }
    }
    if opts.hops == 0 {
        // HOPS is required per the FR.
        return None;
    }
    Some(opts)
}

fn split_top_commas(s: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut start = 0usize;
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if in_str {
            if *b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    continue;
                }
                in_str = false;
            }
            continue;
        }
        match *b {
            b'\'' => in_str = true,
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn strip_prefix_ci<'a>(original: &'a str, lower: &str, prefix: &str) -> Option<&'a str> {
    if lower.starts_with(prefix) {
        let after = &original[prefix.len()..];
        // Require a space or `=` between key and value.
        let trimmed = after.trim_start();
        if trimmed.starts_with('=') || after.len() != trimmed.len() {
            let after = trimmed.strip_prefix('=').unwrap_or(trimmed);
            return Some(after.trim());
        }
    }
    None
}

/// Execute a `SELECT … WITH CONTEXT (...)` statement.  The seed
/// query must return a `node_id BIGINT` column (the normal shape
/// when selecting from `_hdb_graph_nodes` directly).
pub fn graph_rag_expand_with_context(
    db: &EmbeddedDatabase,
    stripped_sql: &str,
    opts: &WithContextOptions,
) -> Result<Vec<GraphRagHit>> {
    // 1. Run the inner SELECT. Expect its first column to be a
    //    `node_id` BIGINT.
    let seed_rows = db.query(stripped_sql, &[])?;
    let mut seed_ids: Vec<i64> = Vec::with_capacity(seed_rows.len());
    for row in seed_rows {
        let id = match row.values.first() {
            Some(Value::Int2(n)) => *n as i64,
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            other => {
                return Err(Error::query_execution(format!(
                    "WITH CONTEXT seed query must project node_id BIGINT as the first column; got {other:?}"
                )))
            }
        };
        seed_ids.push(id);
    }
    if seed_ids.is_empty() {
        return Ok(Vec::new());
    }

    // 2. Expand the pre-selected seeds one frontier per hop (the shared
    //    `search::expand_hops`): seeds are loaded with ONE primary-key IN
    //    statement (a seed id with no node row is skipped); the first `limit`
    //    seeds in query order are kept.
    let direction = opts.direction.unwrap_or(Direction::Both);
    let limit = opts.limit.unwrap_or(50);
    use std::collections::{HashMap, HashSet};
    let mut seen_seed: HashSet<i64> = HashSet::new();
    seed_ids.retain(|id| seen_seed.insert(*id));
    let mut loaded = load_nodes(db, &seed_ids)?;
    let mut visited: HashMap<i64, GraphRagHit> = HashMap::new();
    let mut frontier: Vec<i64> = Vec::new();
    for id in &seed_ids {
        if visited.len() >= limit {
            break;
        }
        if let Some(hit) = loaded.remove(id) {
            visited.insert(*id, hit);
            frontier.push(*id);
        }
    }
    expand_hops(
        db,
        &mut visited,
        frontier,
        opts.hops,
        direction,
        &opts.edges,
        limit,
        opts.expand_limit,
    )?;
    let mut out: Vec<GraphRagHit> = visited.into_values().collect();
    out.sort_by(|a, b| {
        a.hop_distance
            .cmp(&b.hop_distance)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    out.truncate(limit);
    // rerank_by is accepted but not applied — FR 3 §4.4 RERANK BY is
    // flagged as an optimisation layer; phase-3.1 wires vector
    // reranking when the graph layer has embeddings.  Keep the
    // recorded option so consumers can inspect it via the
    // `HitTransport` shape when ExpandOperator ships.
    let _ = &opts.rerank_by;
    Ok(out)
}

/// Silence the unused `graph_rag_search` re-export when `WITH CONTEXT`
/// is live but a caller does not also use the typed search API.
pub fn _graph_rag_search_typed_link(db: &EmbeddedDatabase, opts: &GraphRagOptions) -> Result<Vec<GraphRagHit>> {
    graph_rag_search(db, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_basic() {
        let sql = "SELECT node_id FROM _hdb_graph_nodes WITH CONTEXT (HOPS 2)";
        let (stripped, opts) = detect_with_context(sql).unwrap();
        assert!(!stripped.contains("WITH CONTEXT"));
        assert_eq!(opts.hops, 2);
    }

    #[test]
    fn detect_full_options() {
        let sql = "SELECT node_id FROM _hdb_graph_nodes WHERE n = 1 \
                   WITH CONTEXT (HOPS 3, EDGES CALLS|CITES, DIRECTION both, \
                                 EXPAND_LIMIT 5, LIMIT 30)";
        let (_s, opts) = detect_with_context(sql).unwrap();
        assert_eq!(opts.hops, 3);
        assert_eq!(opts.edges, vec!["CALLS", "CITES"]);
        assert_eq!(opts.direction, Some(Direction::Both));
        assert_eq!(opts.expand_limit, Some(5));
        assert_eq!(opts.limit, Some(30));
    }

    #[test]
    fn detect_rerank_by() {
        let sql = "SELECT 1 WITH CONTEXT (HOPS 1, RERANK BY embedding <-> $1)";
        let (_s, opts) = detect_with_context(sql).unwrap();
        assert_eq!(opts.rerank_by.as_deref(), Some("embedding <-> $1"));
    }

    #[test]
    fn detect_rejects_missing_hops() {
        let sql = "SELECT 1 WITH CONTEXT (EDGES CALLS)";
        assert!(detect_with_context(sql).is_none());
    }

    #[test]
    fn detect_returns_none_without_clause() {
        assert!(detect_with_context("SELECT 1").is_none());
    }

    #[test]
    fn detect_ignores_with_context_inside_string() {
        let sql = "SELECT 'WITH CONTEXT (HOPS 9)' FROM t";
        assert!(detect_with_context(sql).is_none());
    }

    #[test]
    fn detect_handles_multibyte_before_clause() {
        let sql = "SELECT 'a—b' AS label FROM _hdb_graph_nodes WITH CONTEXT (HOPS 1)";
        let (stripped, opts) = detect_with_context(sql).unwrap();
        assert_eq!(opts.hops, 1);
        assert!(stripped.contains("'a—b'"));
        assert!(!stripped.contains("WITH CONTEXT"));
    }
}
