//! `_hdb_graph_nodes` / `_hdb_graph_edges` DDL bootstrap and the
//! projection from `_hdb_code_symbols` into the universal node set.

use crate::{EmbeddedDatabase, Result, Value};

#[derive(Debug, Clone, Default)]
pub struct GraphRagStats {
    /// Code-symbol nodes inserted by this pass.
    pub code_symbols_projected: u64,
    pub doc_chunks_ingested: u64,
    pub mentions_linked: u64,
    /// Existing code nodes whose kind / title / text changed.
    pub nodes_updated: u64,
    /// Code nodes deleted because their symbol no longer exists.
    pub nodes_deleted: u64,
    /// Edges inserted by this pass.
    pub edges_projected: u64,
    /// Edges deleted because they touched a deleted node (all kinds).
    pub edges_deleted: u64,
    /// Of `edges_deleted`, the linker `MENTIONS` edges.
    pub mentions_deleted: u64,
}

/// Create the universal graph tables if they don't exist.
///
/// `_hdb_graph_nodes` — one row per retrievable entity across
/// modalities: code symbols, doc chunks, emails, issues, investor
/// questions, people. `node_kind` disambiguates.
///
/// `_hdb_graph_edges` — typed directed edges:
/// `CALLS / IMPORTS / REFERENCES / MENTIONS / CITES / REPLIES_TO /
/// ASKS_ABOUT / AUTHORED_BY / CONTAINS` etc.
pub fn ensure_tables(db: &EmbeddedDatabase) -> Result<()> {
    db.execute(
        r#"CREATE TABLE IF NOT EXISTS _hdb_graph_nodes (
             node_id    BIGSERIAL PRIMARY KEY,
             node_kind  TEXT NOT NULL,
             source_ref TEXT,
             title      TEXT,
             text       TEXT,
             extra      TEXT
           )"#,
    )?;
    db.execute(
        r#"CREATE TABLE IF NOT EXISTS _hdb_graph_edges (
             edge_id    BIGSERIAL PRIMARY KEY,
             from_node  BIGINT NOT NULL REFERENCES _hdb_graph_nodes(node_id),
             to_node    BIGINT NOT NULL REFERENCES _hdb_graph_nodes(node_id),
             edge_kind  TEXT NOT NULL,
             weight     REAL,
             extra      TEXT
           )"#,
    )?;
    // Read-serving indexes, main only: a column that only has REFERENCES is
    // scanned on read, so graph expansion (`from_node IN`, `to_node IN`) and
    // the projection's `source_ref` lookups need explicit ones. A refused
    // CREATE INDEX on a branch would abort a caller transaction (see
    // `code_graph::storage::bootstrap_tables`), hence the guard.
    if db.current_branch() == "main" {
        db.execute("CREATE INDEX IF NOT EXISTS idx_hdb_graph_edges_from_node ON _hdb_graph_edges(from_node)")?;
        db.execute("CREATE INDEX IF NOT EXISTS idx_hdb_graph_edges_to_node ON _hdb_graph_edges(to_node)")?;
        db.execute("CREATE INDEX IF NOT EXISTS idx_hdb_graph_nodes_source_ref ON _hdb_graph_nodes(source_ref)")?;
    }
    Ok(())
}

/// Project every `_hdb_code_symbols` row into `_hdb_graph_nodes` as a
/// node with `node_kind` matching the symbol kind (lower-cased to
/// match the `Function` / `Class` / ... convention from
/// `FEATURE_REQUEST_graphrag_with_context.md`).
///
/// A diff, not a rewrite. The `source_ref` column carries the symbol's
/// `_hdb_code_symbols.node_id` as `"code_symbol:<id>"`; the pass reads the
/// code nodes and the symbols and
/// - inserts the nodes that are missing (multi-row `INSERT .. RETURNING`),
/// - updates only the nodes whose kind / title / text differ,
/// - deletes the code nodes whose symbol no longer exists (a re-indexed file
///   gets new symbol ids, so its old nodes are ghosts), together with every
///   edge that touches them, including linker `MENTIONS` edges,
/// - inserts the missing edges for resolved refs.
///
/// Nodes of other kinds (`DocChunk`, `Email`, ...) are never touched. The pass
/// runs in one transaction when none is open, and joins the caller's otherwise.
/// Edges from `_hdb_code_symbol_refs` are projected as `CALLS` / `REFERENCES` /
/// etc. — kind names are lifted verbatim.
pub fn project_code_symbols(db: &EmbeddedDatabase, stats: &mut GraphRagStats) -> Result<()> {
    ensure_tables(db)?;

    // Be tolerant of callers that run the graph-rag layer before the
    // code-graph layer has written anything — we just have nothing
    // to project. The same `SELECT ... FROM _hdb_code_symbols`
    // later in the pass fails hard if the table never existed, so
    // short-circuit by probing the planner first.
    let probe = db.query("SELECT 1 FROM _hdb_code_symbols LIMIT 1", &[]);
    if probe.is_err() {
        return Ok(());
    }

    // ---- reads (before the transaction opens) ----
    struct Wanted {
        kind: String,
        title: String,
        text: String,
    }
    let mut wanted: std::collections::BTreeMap<i64, Wanted> = std::collections::BTreeMap::new();
    for row in db.query(
        "SELECT node_id, name, qualified, kind, signature FROM _hdb_code_symbols",
        &[],
    )? {
        let Some(sid) = as_int(row.values.first()) else {
            continue;
        };
        let name = as_string(row.values.get(1)).unwrap_or_default();
        let qualified = as_string(row.values.get(2)).unwrap_or_default();
        let kind = as_string(row.values.get(3)).unwrap_or_default();
        wanted.insert(
            sid,
            Wanted {
                kind: kind_for_graph(&kind),
                title: if qualified.is_empty() { name } else { qualified },
                text: as_string(row.values.get(4)).unwrap_or_default(),
            },
        );
    }
    // source_ref -> (node_id, kind, title, text) of the code nodes already there.
    let mut have: std::collections::HashMap<i64, (i64, String, String, String)> = std::collections::HashMap::new();
    for row in db.query(
        "SELECT node_id, source_ref, node_kind, title, text FROM _hdb_graph_nodes WHERE source_ref LIKE 'code_symbol:%'",
        &[],
    )? {
        let Some(node_id) = as_int(row.values.first()) else { continue };
        let Some(sid) = as_string(row.values.get(1))
            .and_then(|r| r.strip_prefix("code_symbol:").and_then(|n| n.parse::<i64>().ok()))
        else {
            continue;
        };
        have.insert(
            sid,
            (
                node_id,
                as_string(row.values.get(2)).unwrap_or_default(),
                as_string(row.values.get(3)).unwrap_or_default(),
                as_string(row.values.get(4)).unwrap_or_default(),
            ),
        );
    }
    let mut ghosts: Vec<i64> = have
        .iter()
        .filter(|(sid, _)| !wanted.contains_key(sid))
        .map(|(_, (node_id, ..))| *node_id)
        .collect();
    ghosts.sort_unstable();
    // Edges that touch a ghost (single-table `IN` reads, index-served on main).
    let mut ghost_edges: std::collections::BTreeMap<i64, bool> = std::collections::BTreeMap::new();
    for batch in ghosts.chunks(IN_BATCH) {
        let csv = id_list(batch);
        for column in ["from_node", "to_node"] {
            for row in db.query(
                &format!("SELECT edge_id, edge_kind FROM _hdb_graph_edges WHERE {column} IN ({csv})"),
                &[],
            )? {
                if let Some(edge_id) = as_int(row.values.first()) {
                    let mention = as_string(row.values.get(1)).as_deref() == Some("MENTIONS");
                    ghost_edges.insert(edge_id, mention);
                }
            }
        }
    }
    let mut to_insert: Vec<i64> = Vec::new();
    let mut to_update: Vec<(i64, i64)> = Vec::new(); // (symbol id, node id)
    for (sid, w) in &wanted {
        match have.get(sid) {
            None => to_insert.push(*sid),
            Some((node_id, kind, title, text)) => {
                if *kind != w.kind || *title != w.title || *text != w.text {
                    to_update.push((*sid, *node_id));
                }
            }
        }
    }
    let refs = db.query(
        "SELECT from_symbol, to_symbol, kind FROM _hdb_code_symbol_refs WHERE to_symbol IS NOT NULL",
        &[],
    )?;
    let mut seen: std::collections::HashSet<(i64, i64, String)> = std::collections::HashSet::new();
    for row in db.query("SELECT from_node, to_node, edge_kind FROM _hdb_graph_edges", &[])? {
        if let (Some(f), Some(t), Some(k)) = (
            as_int(row.values.first()),
            as_int(row.values.get(1)),
            as_string(row.values.get(2)),
        ) {
            seen.insert((f, t, k));
        }
    }

    // ---- writes (one transaction) ----
    let owned = !db.in_transaction();
    if owned {
        db.begin()?;
    }
    let written = (|| -> Result<()> {
        // Ghosts first: edges (FK children), then nodes.
        let edge_ids: Vec<i64> = ghost_edges.keys().copied().collect();
        for batch in edge_ids.chunks(IN_BATCH) {
            db.execute(&format!(
                "DELETE FROM _hdb_graph_edges WHERE edge_id IN ({})",
                id_list(batch)
            ))?;
        }
        stats.edges_deleted += edge_ids.len() as u64;
        stats.mentions_deleted += ghost_edges.values().filter(|m| **m).count() as u64;
        for batch in ghosts.chunks(IN_BATCH) {
            db.execute(&format!(
                "DELETE FROM _hdb_graph_nodes WHERE node_id IN ({})",
                id_list(batch)
            ))?;
        }
        stats.nodes_deleted += ghosts.len() as u64;
        // Changed nodes: one UPDATE each, and only for those.
        for (sid, node_id) in &to_update {
            let Some(w) = wanted.get(sid) else { continue };
            db.execute_params(
                "UPDATE _hdb_graph_nodes SET node_kind = $1, title = $2, text = $3 WHERE node_id = $4",
                &[
                    Value::String(w.kind.clone()),
                    Value::String(w.title.clone()),
                    Value::String(w.text.clone()),
                    Value::Int8(*node_id),
                ],
            )?;
            stats.nodes_updated += 1;
        }
        // Missing nodes in multi-row batches; RETURNING comes back in input order.
        let mut node_of: std::collections::HashMap<i64, i64> = have
            .iter()
            .filter(|(sid, _)| wanted.contains_key(sid))
            .map(|(sid, (node_id, ..))| (*sid, *node_id))
            .collect();
        for batch in to_insert.chunks(200) {
            let mut sql = String::from("INSERT INTO _hdb_graph_nodes (node_kind, source_ref, title, text) VALUES ");
            let mut params: Vec<Value> = Vec::with_capacity(batch.len() * 4);
            for (i, sid) in batch.iter().enumerate() {
                let Some(w) = wanted.get(sid) else { continue };
                if i != 0 {
                    sql.push_str(", ");
                }
                let b = params.len();
                sql.push_str(&format!("(${}, ${}, ${}, ${})", b + 1, b + 2, b + 3, b + 4));
                params.push(Value::String(w.kind.clone()));
                params.push(Value::String(format!("code_symbol:{sid}")));
                params.push(Value::String(w.title.clone()));
                params.push(Value::String(w.text.clone()));
            }
            sql.push_str(" RETURNING node_id");
            let (_, returned) = db.execute_params_returning(&sql, &params)?;
            if returned.len() != batch.len() {
                return Err(crate::Error::storage(
                    "graph node INSERT returned a different number of rows",
                ));
            }
            for (sid, row) in batch.iter().zip(returned) {
                if let Some(node_id) = as_int(row.values.first()) {
                    node_of.insert(*sid, node_id);
                }
            }
            stats.code_symbols_projected += batch.len() as u64;
        }
        // Missing edges for resolved refs whose endpoints both have nodes.
        let mut new_edges: Vec<(i64, i64, String)> = Vec::new();
        for row in refs {
            let (Some(fsym), Some(tsym)) = (as_int(row.values.first()), as_int(row.values.get(1))) else {
                continue;
            };
            let (Some(f_id), Some(t_id)) = (node_of.get(&fsym), node_of.get(&tsym)) else {
                continue;
            };
            let kind = as_string(row.values.get(2)).unwrap_or_default();
            if seen.insert((*f_id, *t_id, kind.clone())) {
                new_edges.push((*f_id, *t_id, kind));
            }
        }
        for batch in new_edges.chunks(250) {
            let mut sql = String::from("INSERT INTO _hdb_graph_edges (from_node, to_node, edge_kind, weight) VALUES ");
            let mut params: Vec<Value> = Vec::with_capacity(batch.len() * 3);
            for (i, (f, t, k)) in batch.iter().enumerate() {
                if i != 0 {
                    sql.push_str(", ");
                }
                let b = params.len();
                sql.push_str(&format!("(${}, ${}, ${}, 1.0)", b + 1, b + 2, b + 3));
                params.push(Value::Int8(*f));
                params.push(Value::Int8(*t));
                params.push(Value::String(k.clone()));
            }
            db.execute_params(&sql, &params)?;
        }
        stats.edges_projected += new_edges.len() as u64;
        Ok(())
    })();
    if owned {
        match written {
            Ok(()) => db.commit()?,
            Err(e) => {
                let _ = db.rollback();
                return Err(e);
            }
        }
    } else {
        written?;
    }
    Ok(())
}

const IN_BATCH: usize = 1000;

fn id_list(ids: &[i64]) -> String {
    ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
}

fn kind_for_graph(symbol_kind: &str) -> String {
    // Preserve the user-facing Capitalised form from the FR spec.
    match symbol_kind {
        "function" => "Function",
        "method" => "Method",
        "class" => "Class",
        "struct" => "Struct",
        "trait" => "Trait",
        "impl" => "Impl",
        "enum" => "Enum",
        "type" => "Type",
        "module" => "Module",
        "const" => "Const",
        "var" => "Var",
        _ => "Symbol",
    }
    .to_string()
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
