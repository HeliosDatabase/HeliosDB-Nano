//! `graph_rag_search` — seed selection → BFS expansion → rerank.
//! Minimal phase-3 surface: returns expanded subgraph rows ordered by
//! hop distance then by stable id. The vector rerank step is a
//! feature follow-up (phase 3.1) once the shared embedder story lands
//! across the whole track.

use std::collections::{HashMap, HashSet};

use crate::{EmbeddedDatabase, Error, Result, Value};

#[derive(Debug, Clone)]
pub struct GraphRagOptions {
    /// Seed text to match against `_hdb_graph_nodes.title` /
    /// `.text`. Case-insensitive substring match for phase 3 MVP;
    /// phase 3.1 promotes to vector + BM25 hybrid.
    pub seed_text: String,
    /// Restrict seeds to these node kinds. Empty = all kinds.
    pub seed_kinds: Vec<String>,
    /// Maximum BFS depth from seeds.
    pub hops: u32,
    /// Edge kinds to traverse. Empty = all kinds.
    pub edge_kinds: Vec<String>,
    /// Traversal direction.
    pub direction: Direction,
    /// Cap the returned subgraph (seeds + expanded) at this many rows.
    pub limit: usize,
}

impl Default for GraphRagOptions {
    fn default() -> Self {
        Self {
            seed_text: String::new(),
            seed_kinds: Vec::new(),
            hops: 2,
            edge_kinds: Vec::new(),
            direction: Direction::Both,
            limit: 50,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Both,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphRagHit {
    pub node_id: i64,
    pub node_kind: String,
    pub title: Option<String>,
    pub text: Option<String>,
    pub source_ref: Option<String>,
    pub hop_distance: u32,
}

static SEARCH_STATEMENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Number of SQL statements the graph search / WITH CONTEXT expansion has
/// issued in this process (monotonic). The benchmark harness takes the delta
/// around one search to report statements per search.
pub fn search_statement_count() -> u64 {
    SEARCH_STATEMENTS.load(std::sync::atomic::Ordering::Relaxed)
}

thread_local! {
    static THREAD_SEARCH_STATEMENTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The same count for statements issued on the calling thread only, so a
/// caller can measure its own search while others run in parallel.
pub fn search_statement_count_this_thread() -> u64 {
    THREAD_SEARCH_STATEMENTS.with(std::cell::Cell::get)
}

pub(crate) fn count_search_statement() {
    SEARCH_STATEMENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    THREAD_SEARCH_STATEMENTS.with(|count| count.set(count.get() + 1));
}

/// Run a seed → expand → return pass. See `GraphRagOptions` for knobs.
///
/// Storage-level filtering: seed WHERE predicates and edge-kind
/// filters are pushed down via `FilteredScan` against
/// `_hdb_graph_nodes` / `_hdb_graph_edges`. Edge-scan bloom filters
/// help when `edge_kinds` is set.
pub fn graph_rag_search(db: &EmbeddedDatabase, opts: &GraphRagOptions) -> Result<Vec<GraphRagHit>> {
    if opts.seed_text.trim().is_empty() {
        return Err(Error::query_execution(
            "graph_rag_search requires a non-empty seed_text",
        ));
    }

    // 1. Seed collection: substring match on title/text (case-insensitive).
    let needle = opts.seed_text.to_lowercase();
    let mut seed_sql =
        String::from("SELECT node_id, node_kind, title, text, source_ref FROM _hdb_graph_nodes WHERE 1 = 1");
    if !opts.seed_kinds.is_empty() {
        seed_sql.push_str(" AND node_kind IN (");
        for (i, k) in opts.seed_kinds.iter().enumerate() {
            if i > 0 {
                seed_sql.push(',');
            }
            seed_sql.push_str(&sql_text(k));
        }
        seed_sql.push(')');
    }
    count_search_statement();
    let rows = db.query(&seed_sql, &[])?;

    let mut seeds: Vec<GraphRagHit> = Vec::new();
    for row in rows {
        let title = as_string(row.values.get(2)).unwrap_or_default();
        let text = as_string(row.values.get(3)).unwrap_or_default();
        if !title.to_lowercase().contains(&needle) && !text.to_lowercase().contains(&needle) {
            continue;
        }
        seeds.push(GraphRagHit {
            node_id: as_int(row.values.first()).unwrap_or(0),
            node_kind: as_string(row.values.get(1)).unwrap_or_default(),
            title: Some(title),
            text: Some(text),
            source_ref: as_string(row.values.get(4)),
            hop_distance: 0,
        });
        if seeds.len() >= opts.limit {
            break;
        }
    }

    if seeds.is_empty() {
        return Ok(seeds);
    }

    // 2. Expansion, one frontier per hop (see `expand_hops`).
    let mut visited: HashMap<i64, GraphRagHit> = HashMap::new();
    for s in &seeds {
        visited.insert(s.node_id, s.clone());
    }
    let seed_ids: Vec<i64> = seeds.iter().map(|s| s.node_id).collect();
    expand_hops(
        db,
        &mut visited,
        seed_ids,
        opts.hops,
        opts.direction,
        &opts.edge_kinds,
        opts.limit,
        None,
    )?;

    let mut out: Vec<GraphRagHit> = visited.into_values().collect();
    // Deterministic order: hop_distance ascending, then node_id.
    out.sort_by(|a, b| {
        a.hop_distance
            .cmp(&b.hop_distance)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    out.truncate(opts.limit);
    Ok(out)
}

const IN_BATCH: usize = 1000;

/// One hop of edges for a whole frontier: `(source, peer)` pairs with `source`
/// in `ids` and `peer != source`. `Out` is `from_node IN (..)`, `In` is
/// `to_node IN (..)`, `Both` runs both statements and unions them. Every
/// statement is single-table with an `IN` list (index-served with the
/// `_hdb_graph_edges(from_node / to_node)` indexes), ids are chunked at 1000.
/// Pairs come back in no particular order and may repeat.
pub(crate) fn expand_frontier(
    db: &EmbeddedDatabase,
    ids: &[i64],
    direction: Direction,
    edge_kinds: &[String],
) -> Result<Vec<(i64, i64)>> {
    let kind_filter = if edge_kinds.is_empty() {
        String::new()
    } else {
        let list = edge_kinds.iter().map(|k| sql_text(k)).collect::<Vec<_>>().join(",");
        format!(" AND edge_kind IN ({list})")
    };
    let mut pairs: Vec<(i64, i64)> = Vec::new();
    for batch in ids.chunks(IN_BATCH) {
        let csv = batch.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        if matches!(direction, Direction::Out | Direction::Both) {
            count_search_statement();
            for row in db.query(
                &format!("SELECT from_node, to_node FROM _hdb_graph_edges WHERE from_node IN ({csv}){kind_filter}"),
                &[],
            )? {
                if let (Some(src), Some(peer)) = (as_int(row.values.first()), as_int(row.values.get(1))) {
                    if src != peer {
                        pairs.push((src, peer));
                    }
                }
            }
        }
        if matches!(direction, Direction::In | Direction::Both) {
            count_search_statement();
            for row in db.query(
                &format!("SELECT to_node, from_node FROM _hdb_graph_edges WHERE to_node IN ({csv}){kind_filter}"),
                &[],
            )? {
                if let (Some(src), Some(peer)) = (as_int(row.values.first()), as_int(row.values.get(1))) {
                    if src != peer {
                        pairs.push((src, peer));
                    }
                }
            }
        }
    }
    Ok(pairs)
}

/// Nodes by primary key, one `IN` statement per 1000 ids. Missing ids are
/// simply absent from the result. `hop_distance` is 0; the caller sets it.
pub(crate) fn load_nodes(db: &EmbeddedDatabase, ids: &[i64]) -> Result<HashMap<i64, GraphRagHit>> {
    let mut out = HashMap::with_capacity(ids.len());
    for batch in ids.chunks(IN_BATCH) {
        let csv = batch.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        count_search_statement();
        for row in db.query(
            &format!(
                "SELECT node_id, node_kind, title, text, source_ref FROM _hdb_graph_nodes WHERE node_id IN ({csv})"
            ),
            &[],
        )? {
            let Some(node_id) = as_int(row.values.first()) else {
                continue;
            };
            out.insert(
                node_id,
                GraphRagHit {
                    node_id,
                    node_kind: as_string(row.values.get(1)).unwrap_or_default(),
                    title: as_string(row.values.get(2)),
                    text: as_string(row.values.get(3)),
                    source_ref: as_string(row.values.get(4)),
                    hop_distance: 0,
                },
            );
        }
    }
    Ok(out)
}

/// Breadth-first expansion of `visited` (already holding the seeds at hop 0)
/// by one frontier per hop: three statements per hop at most (edges out, edges
/// in, nodes by key) instead of two per visited node.
///
/// Rules, shared by `graph_rag_search` and `WITH CONTEXT`:
/// - a node's hop distance is the hop at which it was FIRST reached;
/// - within a hop, sources are taken in ascending node id and each source's
///   peers in ascending node id; a peer already visited (or already a
///   candidate of this hop) is skipped; `expand_limit`, when given, caps the
///   NEW peers one source contributes;
/// - the hop's candidates are loaded, those without a node row are dropped, and
///   the hop is cut at the room left under `limit` keeping the LOWEST node ids
///   (ties under a limit are therefore broken by node id, not by the order in
///   which the database happened to return edges);
/// - expansion stops after `hops` hops or when `limit` nodes are visited.
#[allow(clippy::too_many_arguments)]
pub(crate) fn expand_hops(
    db: &EmbeddedDatabase,
    visited: &mut HashMap<i64, GraphRagHit>,
    mut frontier: Vec<i64>,
    hops: u32,
    direction: Direction,
    edge_kinds: &[String],
    limit: usize,
    expand_limit: Option<usize>,
) -> Result<()> {
    for depth in 0..hops {
        if frontier.is_empty() || visited.len() >= limit {
            break;
        }
        frontier.sort_unstable();
        frontier.dedup();
        let pairs = expand_frontier(db, &frontier, direction, edge_kinds)?;
        let mut by_source: std::collections::BTreeMap<i64, std::collections::BTreeSet<i64>> =
            std::collections::BTreeMap::new();
        for (src, peer) in pairs {
            by_source.entry(src).or_default().insert(peer);
        }
        let mut candidates: HashSet<i64> = HashSet::new();
        for peers in by_source.values() {
            let mut taken = 0usize;
            for peer in peers {
                if expand_limit.is_some_and(|cap| taken >= cap) {
                    break;
                }
                if visited.contains_key(peer) || !candidates.insert(*peer) {
                    continue;
                }
                taken += 1;
            }
        }
        if candidates.is_empty() {
            break;
        }
        let ids: Vec<i64> = candidates.into_iter().collect();
        let mut loaded = load_nodes(db, &ids)?;
        let mut found: Vec<i64> = loaded.keys().copied().collect();
        found.sort_unstable();
        found.truncate(limit.saturating_sub(visited.len()));
        frontier = Vec::with_capacity(found.len());
        for id in found {
            if let Some(mut hit) = loaded.remove(&id) {
                hit.hop_distance = depth + 1;
                visited.insert(id, hit);
                frontier.push(id);
            }
        }
    }
    Ok(())
}

fn sql_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn as_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn as_int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Int2(n)) => Some(*n as i64),
        Some(Value::Int4(n)) => Some(*n as i64),
        Some(Value::Int8(n)) => Some(*n),
        _ => None,
    }
}
