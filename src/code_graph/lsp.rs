//! LSP-shaped queries exposed as Rust API calls on `EmbeddedDatabase`.
//!
//! Phase 1 implements four functions:
//!
//! - `lsp_definition(name, hint)` — "where is X defined?"
//! - `lsp_references(symbol_id)` — "who uses X?"
//! - `lsp_call_hierarchy(symbol_id, direction, depth)` — "what's the
//!   call tree rooted at X?"
//! - `lsp_hover(symbol_id)` — "what does X look like?"
//!
//! Every query goes through the normal SQL planner / storage path, so
//! predicate pushdown (bloom / zone-map / SIMD) kicks in for free —
//! this is the competitive lever called out in the plan.

use crate::{EmbeddedDatabase, Error, Result, Value};

/// Disambiguation hints for `lsp_definition`. All fields optional;
/// callers supply what they know.
#[derive(Debug, Default, Clone)]
pub struct DefinitionHint {
    pub hint_file: Option<String>,
    pub hint_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DefinitionRow {
    pub symbol_id: i64,
    pub path: String,
    pub line: i32,
    pub signature: String,
    pub qualified: String,
    /// Heuristic score: 1.0 = exact match, lower = name-only.
    /// Callers sort/filter by this.
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceRow {
    pub file_id: i64,
    pub path: String,
    pub line: i32,
    pub kind: String,
    pub caller_symbol_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallHierarchyRow {
    pub depth: i32,
    pub symbol_id: i64,
    pub qualified: String,
    pub path: String,
    pub line: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HoverRow {
    pub signature: String,
    pub doc: Option<String>,
    pub ai_summary: Option<String>,
}

// ---------------------------------------------------------------------------
// lsp_definition
// ---------------------------------------------------------------------------

pub fn lsp_definition(db: &EmbeddedDatabase, name: &str, hint: &DefinitionHint) -> Result<Vec<DefinitionRow>> {
    // Storage-level filtering lever: the WHERE clause here is exactly
    // the shape pushed through FilteredScan — Eq on `name` plus an
    // optional Eq on `path`. Both columns are high-cardinality but
    // the bloom filter skips blocks cheaply.
    let mut sql = String::from(
        "SELECT s.node_id, f.path, s.line_start, s.signature, s.qualified, s.kind \
         FROM _hdb_code_symbols s JOIN _hdb_code_files f ON f.node_id = s.file_id \
         WHERE s.name = ",
    );
    sql.push_str(&sql_text(name));
    if let Some(kind) = &hint.hint_kind {
        sql.push_str(" AND s.kind = ");
        sql.push_str(&sql_text(kind));
    }
    if let Some(path) = &hint.hint_file {
        sql.push_str(" AND f.path = ");
        sql.push_str(&sql_text(path));
    }
    sql.push_str(" ORDER BY s.node_id");
    let rows = db.query(&sql, &[])?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let symbol_id = int_at(&row, 0)?;
        let path = str_at(&row, 1).unwrap_or_default();
        let line = int_at(&row, 2)? as i32;
        let signature = str_at(&row, 3).unwrap_or_default();
        let qualified = str_at(&row, 4).unwrap_or_default();
        let kind_val = str_at(&row, 5).unwrap_or_default();
        // Score: 1.0 if single candidate; if the hint_kind matches,
        // bump to 1.1 to break ties. Otherwise 0.8 (name-only).
        let mut score = 0.8;
        if hint.hint_kind.as_deref() == Some(kind_val.as_str()) {
            score = 1.1;
        } else if hint.hint_kind.is_none() {
            score = 1.0;
        }
        out.push(DefinitionRow {
            symbol_id,
            path,
            line,
            signature,
            qualified,
            score,
        });
    }
    // Sort by score descending, then node_id for stability.
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    Ok(out)
}

// ---------------------------------------------------------------------------
// lsp_references
// ---------------------------------------------------------------------------

pub fn lsp_references(db: &EmbeddedDatabase, symbol_id: i64) -> Result<Vec<ReferenceRow>> {
    // Classic reverse-edge scan. Filtered on to_symbol — high selectivity,
    // pushdown through FilteredScan on _hdb_code_symbol_refs.
    let sql = format!(
        "SELECT r.file_id, f.path, r.line, r.kind, r.from_symbol \
         FROM _hdb_code_symbol_refs r JOIN _hdb_code_files f ON f.node_id = r.file_id \
         WHERE r.to_symbol = {symbol_id} \
         ORDER BY r.line"
    );
    let rows = db.query(&sql, &[])?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let file_id = int_at(&row, 0)?;
        let path = str_at(&row, 1).unwrap_or_default();
        let line = int_at(&row, 2)? as i32;
        let kind = str_at(&row, 3).unwrap_or_default();
        let caller_symbol_id = int_at(&row, 4).ok();
        out.push(ReferenceRow {
            file_id,
            path,
            line,
            kind,
            caller_symbol_id,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// lsp_call_hierarchy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallDirection {
    Incoming,
    Outgoing,
}

pub fn lsp_call_hierarchy(
    db: &EmbeddedDatabase,
    symbol_id: i64,
    direction: CallDirection,
    depth: u32,
) -> Result<Vec<CallHierarchyRow>> {
    // Manual BFS — phase 2 wraps as a recursive CTE so callers can run it
    // from pure SQL.
    //
    // Each level is three single-table statements instead of one three-way
    // join: every join in the engine is a hash join that scans the
    // unfiltered side, so the joined form read the whole symbols table per
    // level (hundreds of ms at 200k symbols) while the same predicates on
    // one table are index-served (sub-ms):
    //   1. refs by `to_symbol IN (..)` / `from_symbol IN (..)` and kind,
    //   2. the new symbols by primary key,
    //   3. their file paths by primary key (cached across levels).
    // Within a level, rows come in the order the refs scan returned them,
    // first sighting wins.
    const IN_CHUNK: usize = 1000;
    let mut out = Vec::new();
    let mut frontier: Vec<i64> = vec![symbol_id];
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    seen.insert(symbol_id);
    let mut paths: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    for d in 1..=depth {
        if frontier.is_empty() {
            break;
        }
        // 1. neighbours of the frontier, in scan order, deduplicated.
        let mut candidates: Vec<i64> = Vec::new();
        let mut candidate_set: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for chunk in frontier.chunks(IN_CHUNK) {
            let ids_csv = chunk.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            let sql = match direction {
                CallDirection::Incoming => format!(
                    "SELECT from_symbol FROM _hdb_code_symbol_refs \
                     WHERE to_symbol IN ({ids_csv}) AND kind = 'CALLS'"
                ),
                CallDirection::Outgoing => format!(
                    "SELECT to_symbol FROM _hdb_code_symbol_refs \
                     WHERE from_symbol IN ({ids_csv}) AND kind = 'CALLS' AND to_symbol IS NOT NULL"
                ),
            };
            for row in db.query(&sql, &[])? {
                let id = int_at(&row, 0)?;
                if !seen.contains(&id) && candidate_set.insert(id) {
                    candidates.push(id);
                }
            }
        }
        // 2. the symbols behind them, by primary key.
        let mut info: std::collections::HashMap<i64, (String, i64, i32)> = std::collections::HashMap::new();
        for chunk in candidates.chunks(IN_CHUNK) {
            let ids_csv = chunk.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            let rows = db.query(
                &format!("SELECT node_id, qualified, file_id, line_start FROM _hdb_code_symbols WHERE node_id IN ({ids_csv})"),
                &[],
            )?;
            for row in rows {
                info.insert(
                    int_at(&row, 0)?,
                    (
                        str_at(&row, 1).unwrap_or_default(),
                        int_at(&row, 2)?,
                        int_at(&row, 3).map(|x| x as i32).unwrap_or(0),
                    ),
                );
            }
        }
        // 3. their files, by primary key, once per file across levels.
        let mut missing_files: Vec<i64> = info
            .values()
            .map(|(_, f, _)| *f)
            .filter(|f| !paths.contains_key(f))
            .collect();
        missing_files.sort_unstable();
        missing_files.dedup();
        for chunk in missing_files.chunks(IN_CHUNK) {
            let ids_csv = chunk.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            let rows = db.query(
                &format!("SELECT node_id, path FROM _hdb_code_files WHERE node_id IN ({ids_csv})"),
                &[],
            )?;
            for row in rows {
                paths.insert(int_at(&row, 0)?, str_at(&row, 1).unwrap_or_default());
            }
        }
        let mut next_frontier: Vec<i64> = Vec::new();
        for id in candidates {
            let Some((qualified, file_id, line)) = info.get(&id) else {
                continue;
            };
            let Some(path) = paths.get(file_id) else {
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            out.push(CallHierarchyRow {
                depth: d as i32,
                symbol_id: id,
                qualified: qualified.clone(),
                path: path.clone(),
                line: *line,
            });
            next_frontier.push(id);
        }
        frontier = next_frontier;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// lsp_hover
// ---------------------------------------------------------------------------

pub fn lsp_hover(db: &EmbeddedDatabase, symbol_id: i64) -> Result<Option<HoverRow>> {
    let rows = db.query(
        &format!("SELECT signature FROM _hdb_code_symbols WHERE node_id = {symbol_id}"),
        &[],
    )?;
    if let Some(row) = rows.first() {
        let signature = str_at(row, 0).unwrap_or_default();
        return Ok(Some(HoverRow {
            signature,
            doc: None,
            ai_summary: None,
        }));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn sql_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn str_at(row: &crate::Tuple, idx: usize) -> Option<String> {
    row.values.get(idx).and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        _ => None,
    })
}

fn int_at(row: &crate::Tuple, idx: usize) -> Result<i64> {
    match row.values.get(idx) {
        Some(Value::Int2(n)) => Ok(*n as i64),
        Some(Value::Int4(n)) => Ok(*n as i64),
        Some(Value::Int8(n)) => Ok(*n),
        Some(other) => Err(Error::query_execution(format!(
            "expected integer at position {idx}, got {other:?}"
        ))),
        None => Err(Error::query_execution(format!("missing column at position {idx}"))),
    }
}
