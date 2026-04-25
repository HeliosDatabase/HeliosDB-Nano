//! `_hdb_graph_nodes` / `_hdb_graph_edges` DDL bootstrap and the
//! projection from `_hdb_code_symbols` into the universal node set.

use crate::{EmbeddedDatabase, Result, Value};

#[derive(Debug, Clone, Default)]
pub struct GraphRagStats {
    pub code_symbols_projected: u64,
    pub doc_chunks_ingested: u64,
    pub mentions_linked: u64,
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
    Ok(())
}

/// Project every `_hdb_code_symbols` row into `_hdb_graph_nodes` as a
/// node with `node_kind` matching the symbol kind (lower-cased to
/// match the `Function` / `Class` / ... convention from
/// `FEATURE_REQUEST_graphrag_with_context.md`).
///
/// Idempotent: the `source_ref` column carries the symbol's
/// `_hdb_code_symbols.node_id` as `"code_symbol:<id>"`, so re-runs
/// update in place. Edges from `_hdb_code_symbol_refs` are projected
/// as `CALLS` / `REFERENCES` / etc. — kind names are lifted verbatim.
pub fn project_code_symbols(
    db: &EmbeddedDatabase,
    stats: &mut GraphRagStats,
) -> Result<()> {
    ensure_tables(db)?;

    // Be tolerant of callers that run the graph-rag layer before the
    // code-graph layer has written anything — we just have nothing
    // to project. The same `SELECT ... FROM _hdb_code_symbols`
    // later in the pass fails hard if the table never existed, so
    // short-circuit by probing the planner first.
    let probe = db.query(
        "SELECT 1 FROM _hdb_code_symbols LIMIT 1",
        &[],
    );
    if probe.is_err() {
        return Ok(());
    }

    // Pull every symbol row. Phase 1 scope; phase 4 can replace the
    // full pass with an incremental trigger bound to the symbols
    // table.
    let rows = db.query(
        "SELECT node_id, name, qualified, kind, signature FROM _hdb_code_symbols",
        &[],
    )?;
    let mut existing: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for row in db.query(
        "SELECT source_ref, node_id FROM _hdb_graph_nodes WHERE node_kind != 'DocChunk'",
        &[],
    )? {
        let r = match row.values.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let id = match row.values.get(1) {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        existing.insert(r, id);
    }

    for row in rows {
        let sid = match row.values.first() {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        let name = as_string(row.values.get(1)).unwrap_or_default();
        let qualified = as_string(row.values.get(2)).unwrap_or_default();
        let kind = as_string(row.values.get(3)).unwrap_or_default();
        let signature = as_string(row.values.get(4)).unwrap_or_default();
        let source_ref = format!("code_symbol:{sid}");
        let node_kind = kind_for_graph(&kind);
        let title_sql = sql_text(&qualified.is_empty().then(|| name.clone()).unwrap_or(qualified.clone()));
        let text_sql = sql_text(&signature);
        let src_sql = sql_text(&source_ref);
        let kind_sql = sql_text(&node_kind);
        if let Some(existing_id) = existing.get(&source_ref) {
            db.execute(&format!(
                "UPDATE _hdb_graph_nodes SET node_kind = {kind_sql}, title = {title_sql}, text = {text_sql} \
                 WHERE node_id = {existing_id}"
            ))?;
        } else {
            db.execute(&format!(
                "INSERT INTO _hdb_graph_nodes (node_kind, source_ref, title, text) \
                 VALUES ({kind_sql}, {src_sql}, {title_sql}, {text_sql})"
            ))?;
            stats.code_symbols_projected += 1;
        }
    }

    // Project edges. For each `_hdb_code_symbol_refs` with a resolved
    // `to_symbol`, write a matching `_hdb_graph_edges` row. We look up
    // the graph node_ids via `source_ref`.
    let refs = db.query(
        "SELECT from_symbol, to_symbol, kind \
         FROM _hdb_code_symbol_refs \
         WHERE to_symbol IS NOT NULL",
        &[],
    )?;
    // Build lookup source_ref → node_id for this call (fresh post-insert).
    let mut map: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for row in db.query(
        "SELECT source_ref, node_id FROM _hdb_graph_nodes",
        &[],
    )? {
        let r = match row.values.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let id = match row.values.get(1) {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        map.insert(r, id);
    }

    // Only insert edges we don't already have (simple dedupe by
    // (from, to, kind)).
    let existing_edges = db.query(
        "SELECT from_node, to_node, edge_kind FROM _hdb_graph_edges",
        &[],
    )?;
    let mut seen: std::collections::HashSet<(i64, i64, String)> = std::collections::HashSet::new();
    for row in existing_edges {
        let f = as_int(row.values.first());
        let t = as_int(row.values.get(1));
        let k = as_string(row.values.get(2));
        if let (Some(f), Some(t), Some(k)) = (f, t, k) {
            seen.insert((f, t, k));
        }
    }
    for row in refs {
        let fsym = match as_int(row.values.first()) {
            Some(n) => n,
            None => continue,
        };
        let tsym = match as_int(row.values.get(1)) {
            Some(n) => n,
            None => continue,
        };
        let kind = as_string(row.values.get(2)).unwrap_or_default();
        let f_id = match map.get(&format!("code_symbol:{fsym}")) {
            Some(n) => *n,
            None => continue,
        };
        let t_id = match map.get(&format!("code_symbol:{tsym}")) {
            Some(n) => *n,
            None => continue,
        };
        if seen.contains(&(f_id, t_id, kind.clone())) {
            continue;
        }
        let kind_sql = sql_text(&kind);
        db.execute(&format!(
            "INSERT INTO _hdb_graph_edges (from_node, to_node, edge_kind, weight) \
             VALUES ({f_id}, {t_id}, {kind_sql}, 1.0)"
        ))?;
        seen.insert((f_id, t_id, kind));
    }

    Ok(())
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
