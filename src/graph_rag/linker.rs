//! Entity linker — emits `MENTIONS` edges from text-bearing nodes
//! (DocChunk / Email / Issue / …) to code symbols.
//!
//! Two strategies:
//! * `link_exact_qualified` — whole-word match on the symbol's
//!   `qualified` name (the original phase-3 MVP).
//! * `link_vector_similar` — cosine-similarity match between caller-
//!   supplied embeddings.  The caller computes embeddings for the
//!   text-bearing graph nodes and the target code symbols (typically
//!   via an external embedder), then hands both sides as
//!   `(node_id, vector)` pairs.  Each text node gets up to `top_k`
//!   `MENTIONS` edges to its closest symbols above `threshold`.
//!
//! Idempotent: re-running the linker against the same corpus produces
//! the same edge set (duplicates are deduped on insert).

use std::collections::{HashMap, HashSet};

use crate::{EmbeddedDatabase, Result, Value};

use super::schema::ensure_tables;

#[derive(Debug, Clone, Default)]
pub struct LinkerStats {
    pub nodes_scanned: u64,
    pub mentions_added: u64,
    pub candidates_seen: u64,
}

/// Scan every text-bearing node (`title` or `text` non-empty) and
/// emit a `MENTIONS` edge from the node to any code symbol whose
/// `qualified` name appears as a whole word in the text.  Matching
/// kinds are `DocChunk`, `Email`, `Issue`, `InvestorQuestion`,
/// `Answer`; extend this list by passing `extra_kinds`.
pub fn link_exact_qualified(
    db: &EmbeddedDatabase,
    extra_kinds: &[&str],
) -> Result<LinkerStats> {
    ensure_tables(db)?;
    let mut stats = LinkerStats::default();

    // Build the lookup: qualified → Vec<node_id>. We match
    // case-sensitively on qualified because lowercasing would
    // conflate `Foo` / `foo` types.
    let sym_rows = db.query(
        "SELECT qualified, node_id FROM _hdb_code_symbols \
         WHERE qualified IS NOT NULL AND qualified <> ''",
        &[],
    )?;
    let mut by_name: HashMap<String, Vec<i64>> = HashMap::new();
    for row in sym_rows {
        let name = match row.values.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let sid = match row.values.get(1) {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        by_name.entry(name).or_default().push(sid);
    }
    if by_name.is_empty() {
        return Ok(stats);
    }

    // Map code_symbol node_id → _hdb_graph_nodes.node_id.
    let mut code_to_graph: HashMap<i64, i64> = HashMap::new();
    for row in db.query(
        "SELECT source_ref, node_id FROM _hdb_graph_nodes",
        &[],
    )? {
        let Some(Value::String(sref)) = row.values.first() else {
            continue;
        };
        let gid = match row.values.get(1) {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        if let Some(id_str) = sref.strip_prefix("code_symbol:") {
            if let Ok(code_id) = id_str.parse::<i64>() {
                code_to_graph.insert(code_id, gid);
            }
        }
    }

    // Dedup of edges we already have.
    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    for row in db.query(
        "SELECT from_node, to_node FROM _hdb_graph_edges WHERE edge_kind = 'MENTIONS'",
        &[],
    )? {
        let from = to_int(row.values.first());
        let to = to_int(row.values.get(1));
        if let (Some(f), Some(t)) = (from, to) {
            seen.insert((f, t));
        }
    }

    let mut kinds: Vec<&str> = vec!["DocChunk", "DocSection", "Email", "Issue", "Comment", "InvestorQuestion", "Answer"];
    kinds.extend_from_slice(extra_kinds);
    let kind_list = kinds
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(",");

    let text_rows = db.query(
        &format!(
            "SELECT node_id, title, text FROM _hdb_graph_nodes \
             WHERE node_kind IN ({kind_list})"
        ),
        &[],
    )?;

    for row in text_rows {
        stats.nodes_scanned += 1;
        let node_id = match row.values.first() {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        let title = as_string(row.values.get(1)).unwrap_or_default();
        let text = as_string(row.values.get(2)).unwrap_or_default();
        let haystack = format!("{title}\n{text}");

        for (needle, sym_ids) in &by_name {
            if needle.is_empty() {
                continue;
            }
            if !contains_whole_word(&haystack, needle) {
                continue;
            }
            stats.candidates_seen += 1;
            for sid in sym_ids {
                let Some(gid) = code_to_graph.get(sid) else { continue };
                if seen.contains(&(node_id, *gid)) {
                    continue;
                }
                db.execute(&format!(
                    "INSERT INTO _hdb_graph_edges (from_node, to_node, edge_kind, weight) \
                     VALUES ({node_id}, {gid}, 'MENTIONS', 1.0)"
                ))?;
                seen.insert((node_id, *gid));
                stats.mentions_added += 1;
            }
        }
    }
    Ok(stats)
}

fn contains_whole_word(haystack: &str, needle: &str) -> bool {
    // Simple word-boundary check: `needle` must not be preceded or
    // followed by an identifier character.
    let mut start = 0usize;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0
            || !is_ident_char(
                haystack
                    .as_bytes()
                    .get(abs - 1)
                    .copied()
                    .unwrap_or(b' '),
            );
        let after_idx = abs + needle.len();
        let after_ok = after_idx == haystack.len()
            || !is_ident_char(
                haystack
                    .as_bytes()
                    .get(after_idx)
                    .copied()
                    .unwrap_or(b' '),
            );
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b':' || b == b'.'
}

// ---------------------------------------------------------------------------
// Vector-similar linker (FR 4 §4.3 strategy 3)
// ---------------------------------------------------------------------------

/// Per-text-node embedding query handed to [`link_vector_similar`].
/// `node_id` is the `_hdb_graph_nodes.node_id` of the text-bearing
/// node (DocChunk / Email / Issue / …); `vector` is its embedding.
#[derive(Debug, Clone)]
pub struct TextEmbedding {
    pub node_id: i64,
    pub vector: Vec<f32>,
}

/// Per-symbol embedding target. `node_id` is the
/// `_hdb_code_symbols.node_id`; `vector` is the symbol's embedding
/// (typically what would live in `_hdb_code_symbols.body_vec` once
/// the indexer's embedder runs).
#[derive(Debug, Clone)]
pub struct SymbolEmbedding {
    pub node_id: i64,
    pub vector: Vec<f32>,
}

/// For each text query, find the top-`k` code-symbol embeddings by
/// cosine similarity, gate by `threshold` (drop pairs scoring below
/// it), and emit a `MENTIONS` edge per surviving pair with
/// `weight = similarity`.
///
/// Embeddings whose dimensions don't match a target's are skipped
/// (so callers can mix multiple embedders without crashing).  All
/// inputs are caller-supplied: this primitive doesn't pull a body_vec
/// column from `_hdb_code_symbols` — the schema doesn't carry one
/// yet — so it's usable today against any external embedder.
///
/// `top_k = 0` and empty inputs are no-ops.  `threshold ∈ [-1, 1]`
/// (cosine range); pass `0.0` to accept everything not orthogonal
/// or worse.
pub fn link_vector_similar(
    db: &EmbeddedDatabase,
    text_queries: &[TextEmbedding],
    symbol_targets: &[SymbolEmbedding],
    top_k: usize,
    threshold: f32,
) -> Result<LinkerStats> {
    ensure_tables(db)?;
    let mut stats = LinkerStats::default();
    if text_queries.is_empty() || symbol_targets.is_empty() || top_k == 0 {
        return Ok(stats);
    }

    // Map code_symbol node_id → _hdb_graph_nodes.node_id (project
    // through the same source_ref convention as the exact linker).
    let mut code_to_graph: HashMap<i64, i64> = HashMap::new();
    for row in db.query("SELECT source_ref, node_id FROM _hdb_graph_nodes", &[])? {
        let Some(Value::String(sref)) = row.values.first() else {
            continue;
        };
        let gid = match row.values.get(1) {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        if let Some(id_str) = sref.strip_prefix("code_symbol:") {
            if let Ok(code_id) = id_str.parse::<i64>() {
                code_to_graph.insert(code_id, gid);
            }
        }
    }

    // Pre-existing MENTIONS edges so we don't double-insert.
    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    for row in db.query(
        "SELECT from_node, to_node FROM _hdb_graph_edges WHERE edge_kind = 'MENTIONS'",
        &[],
    )? {
        if let (Some(f), Some(t)) = (to_int(row.values.first()), to_int(row.values.get(1))) {
            seen.insert((f, t));
        }
    }

    for q in text_queries {
        stats.nodes_scanned += 1;
        if q.vector.is_empty() {
            continue;
        }
        // Collect (graph_node_id, score) for every target whose
        // embedding has the matching dimension.
        let mut scored: Vec<(i64, f32)> = Vec::with_capacity(symbol_targets.len());
        for s in symbol_targets {
            if s.vector.len() != q.vector.len() {
                continue;
            }
            let Some(gid) = code_to_graph.get(&s.node_id) else {
                continue;
            };
            let sim = cosine(&q.vector, &s.vector);
            stats.candidates_seen += 1;
            if sim < threshold {
                continue;
            }
            scored.push((*gid, sim));
        }
        if scored.is_empty() {
            continue;
        }
        // Top-k by similarity descending. Ties broken by graph node_id
        // for determinism.
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(top_k);

        for (gid, sim) in scored {
            if seen.contains(&(q.node_id, gid)) {
                continue;
            }
            db.execute(&format!(
                "INSERT INTO _hdb_graph_edges (from_node, to_node, edge_kind, weight) \
                 VALUES ({from}, {to}, 'MENTIONS', {w})",
                from = q.node_id,
                to = gid,
                w = sim,
            ))?;
            seen.insert((q.node_id, gid));
            stats.mentions_added += 1;
        }
    }
    Ok(stats)
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn as_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn to_int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Int2(n)) => Some(*n as i64),
        Some(Value::Int4(n)) => Some(*n as i64),
        Some(Value::Int8(n)) => Some(*n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_word_match() {
        assert!(contains_whole_word("foo bar baz", "bar"));
        assert!(!contains_whole_word("foobar", "bar"));
        assert!(contains_whole_word("see Foo::bar please", "Foo::bar"));
        assert!(!contains_whole_word("callFooBar()", "Foo"));
    }
}
