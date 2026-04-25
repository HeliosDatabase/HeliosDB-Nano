//! `graph_rag_search` — seed selection → BFS expansion → rerank.
//! Minimal phase-3 surface: returns expanded subgraph rows ordered by
//! hop distance then by stable id. The vector rerank step is a
//! feature follow-up (phase 3.1) once the shared embedder story lands
//! across the whole track.

use std::collections::{HashMap, HashSet, VecDeque};

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

/// Run a seed → expand → return pass. See `GraphRagOptions` for knobs.
///
/// Storage-level filtering: seed WHERE predicates and edge-kind
/// filters are pushed down via `FilteredScan` against
/// `_hdb_graph_nodes` / `_hdb_graph_edges`. Edge-scan bloom filters
/// help when `edge_kinds` is set.
pub fn graph_rag_search(
    db: &EmbeddedDatabase,
    opts: &GraphRagOptions,
) -> Result<Vec<GraphRagHit>> {
    if opts.seed_text.trim().is_empty() {
        return Err(Error::query_execution("graph_rag_search requires a non-empty seed_text"));
    }

    // 1. Seed collection: substring match on title/text (case-insensitive).
    let needle = opts.seed_text.to_lowercase();
    let mut seed_sql = String::from(
        "SELECT node_id, node_kind, title, text, source_ref FROM _hdb_graph_nodes WHERE 1 = 1",
    );
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
    let rows = db.query(&seed_sql, &[])?;

    let mut seeds: Vec<GraphRagHit> = Vec::new();
    for row in rows {
        let title = as_string(row.values.get(2)).unwrap_or_default();
        let text = as_string(row.values.get(3)).unwrap_or_default();
        if !title.to_lowercase().contains(&needle)
            && !text.to_lowercase().contains(&needle)
        {
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

    // 2. BFS expansion.
    let mut visited: HashMap<i64, GraphRagHit> = HashMap::new();
    let mut queue: VecDeque<(i64, u32)> = VecDeque::new();
    for s in &seeds {
        visited.insert(s.node_id, s.clone());
        queue.push_back((s.node_id, 0));
    }
    while let Some((nid, depth)) = queue.pop_front() {
        if depth >= opts.hops {
            continue;
        }
        if visited.len() >= opts.limit {
            break;
        }
        let neighbours = fetch_neighbours(db, nid, opts.direction, &opts.edge_kinds)?;
        for n in neighbours {
            if visited.len() >= opts.limit {
                break;
            }
            if visited.contains_key(&n.node_id) {
                continue;
            }
            let hit = GraphRagHit {
                node_id: n.node_id,
                node_kind: n.node_kind.clone(),
                title: n.title.clone(),
                text: n.text.clone(),
                source_ref: n.source_ref.clone(),
                hop_distance: depth + 1,
            };
            visited.insert(n.node_id, hit);
            queue.push_back((n.node_id, depth + 1));
        }
    }

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

#[derive(Debug, Clone)]
struct Neighbour {
    node_id: i64,
    node_kind: String,
    title: Option<String>,
    text: Option<String>,
    source_ref: Option<String>,
}

fn fetch_neighbours(
    db: &EmbeddedDatabase,
    seed: i64,
    direction: Direction,
    kinds: &[String],
) -> Result<Vec<Neighbour>> {
    // Build kind filter SQL fragment. Non-empty `kinds` pushes down
    // through FilteredScan bloom + zone maps.
    let kind_filter = if kinds.is_empty() {
        String::new()
    } else {
        let list = kinds
            .iter()
            .map(|k| sql_text(k))
            .collect::<Vec<_>>()
            .join(",");
        format!(" AND e.edge_kind IN ({list})")
    };

    let where_direction = match direction {
        Direction::Out => format!("e.from_node = {seed}"),
        Direction::In => format!("e.to_node = {seed}"),
        Direction::Both => format!("(e.from_node = {seed} OR e.to_node = {seed})"),
    };

    let sql = format!(
        "SELECT DISTINCT \
           CASE WHEN e.from_node = {seed} THEN e.to_node ELSE e.from_node END AS peer \
         FROM _hdb_graph_edges e \
         WHERE {where_direction}{kind_filter}"
    );
    let rows = db.query(&sql, &[])?;
    let mut ids = Vec::with_capacity(rows.len());
    let mut seen: HashSet<i64> = HashSet::new();
    for row in rows {
        if let Some(id) = as_int(row.values.first()) {
            if id != seed && seen.insert(id) {
                ids.push(id);
            }
        }
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let id_list = ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let nodes_rows = db.query(
        &format!(
            "SELECT node_id, node_kind, title, text, source_ref \
             FROM _hdb_graph_nodes WHERE node_id IN ({id_list})"
        ),
        &[],
    )?;
    let mut out = Vec::with_capacity(nodes_rows.len());
    for row in nodes_rows {
        out.push(Neighbour {
            node_id: as_int(row.values.first()).unwrap_or(0),
            node_kind: as_string(row.values.get(1)).unwrap_or_default(),
            title: as_string(row.values.get(2)),
            text: as_string(row.values.get(3)),
            source_ref: as_string(row.values.get(4)),
        });
    }
    Ok(out)
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
