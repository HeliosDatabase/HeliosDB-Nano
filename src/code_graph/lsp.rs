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

use super::storage;

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

pub fn lsp_definition(
    db: &EmbeddedDatabase,
    name: &str,
    hint: &DefinitionHint,
) -> Result<Vec<DefinitionRow>> {
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
    // Manual BFS for phase 1 — phase 2 wraps as a recursive CTE so
    // callers can run it from pure SQL.
    let mut out = Vec::new();
    let mut frontier: Vec<i64> = vec![symbol_id];
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    seen.insert(symbol_id);
    for d in 1..=depth {
        if frontier.is_empty() {
            break;
        }
        let ids_csv = frontier
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = match direction {
            CallDirection::Incoming => format!(
                "SELECT r.from_symbol, s.qualified, f.path, s.line_start \
                 FROM _hdb_code_symbol_refs r \
                 JOIN _hdb_code_symbols s ON s.node_id = r.from_symbol \
                 JOIN _hdb_code_files   f ON f.node_id = s.file_id \
                 WHERE r.to_symbol IN ({ids_csv}) AND r.kind = 'CALLS'"
            ),
            CallDirection::Outgoing => format!(
                "SELECT r.to_symbol, s.qualified, f.path, s.line_start \
                 FROM _hdb_code_symbol_refs r \
                 JOIN _hdb_code_symbols s ON s.node_id = r.to_symbol \
                 JOIN _hdb_code_files   f ON f.node_id = s.file_id \
                 WHERE r.from_symbol IN ({ids_csv}) \
                       AND r.kind = 'CALLS' \
                       AND r.to_symbol IS NOT NULL"
            ),
        };
        let rows = db.query(&sql, &[])?;
        let mut next_frontier: Vec<i64> = Vec::new();
        for row in rows {
            let next_id = int_at(&row, 0)?;
            if !seen.insert(next_id) {
                continue;
            }
            out.push(CallHierarchyRow {
                depth: d as i32,
                symbol_id: next_id,
                qualified: str_at(&row, 1).unwrap_or_default(),
                path: str_at(&row, 2).unwrap_or_default(),
                line: int_at(&row, 3).map(|x| x as i32).unwrap_or(0),
            });
            next_frontier.push(next_id);
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
        &format!(
            "SELECT signature FROM _hdb_code_symbols WHERE node_id = {symbol_id}"
        ),
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
    // File-level sanity so callers can distinguish "unknown id" from
    // "known id with no signature".
    let _ = storage::file_id_for_symbol(db, symbol_id)?;
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
        None => Err(Error::query_execution(format!(
            "missing column at position {idx}"
        ))),
    }
}
