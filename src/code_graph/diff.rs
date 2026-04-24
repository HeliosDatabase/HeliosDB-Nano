//! Temporal diff helpers (FR 3 §3.3).
//!
//! `lsp_references_diff`, `lsp_body_diff`, and `ast_diff` run the
//! equivalent lsp_* query at two temporal points and classify the
//! results as `added`, `removed`, or `moved`.
//!
//! The temporal points are expressed as `AsOfRef` which is a thin
//! wrapper over Nano's native `AS OF` clause (`COMMIT`, `TIMESTAMP`,
//! `NOW`). Callers build these with `AsOfRef::commit("abc")`,
//! `AsOfRef::timestamp("2025-01-02")`, or `AsOfRef::now()`.

use std::collections::HashMap;

use crate::{EmbeddedDatabase, Result, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsOfRef {
    Now,
    Commit(String),
    Timestamp(String),
}

impl AsOfRef {
    pub fn commit(sha: impl Into<String>) -> Self {
        Self::Commit(sha.into())
    }
    pub fn timestamp(ts: impl Into<String>) -> Self {
        Self::Timestamp(ts.into())
    }
    pub fn now() -> Self {
        Self::Now
    }

    /// Render the SQL `AS OF …` clause fragment, or empty when `Now`.
    pub fn to_sql_clause(&self) -> String {
        match self {
            AsOfRef::Now => String::new(),
            AsOfRef::Commit(sha) => format!(" AS OF COMMIT '{}'", escape(sha)),
            AsOfRef::Timestamp(ts) => format!(" AS OF TIMESTAMP '{}'", escape(ts)),
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('\'', "''")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffChange {
    Added,
    Removed,
    Moved,
}

impl DiffChange {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Moved => "moved",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RefDiffRow {
    pub change: DiffChange,
    pub path: String,
    pub line: i32,
    pub caller_symbol_id: Option<i64>,
}

/// `lsp_references_diff(symbol_id, at_a, at_b)` — added / removed /
/// moved refs between two points in time.
///
/// * `added` — present at B, not at A.
/// * `removed` — present at A, not at B.
/// * `moved` — present at both, but at a different `(path, line)`.
///
/// Matching key is `caller_symbol_id`. Refs without a caller id (the
/// unresolved-heuristic bucket) are compared by `(path, line, kind)`.
pub fn lsp_references_diff(
    db: &EmbeddedDatabase,
    symbol_id: i64,
    at_a: &AsOfRef,
    at_b: &AsOfRef,
) -> Result<Vec<RefDiffRow>> {
    let a = fetch_refs(db, symbol_id, at_a)?;
    let b = fetch_refs(db, symbol_id, at_b)?;

    let key_a: HashMap<RefKey, (String, i32)> = a
        .iter()
        .map(|r| (r.key(), (r.path.clone(), r.line)))
        .collect();
    let key_b: HashMap<RefKey, (String, i32)> = b
        .iter()
        .map(|r| (r.key(), (r.path.clone(), r.line)))
        .collect();

    let mut out: Vec<RefDiffRow> = Vec::new();
    for r in &a {
        match key_b.get(&r.key()) {
            None => out.push(RefDiffRow {
                change: DiffChange::Removed,
                path: r.path.clone(),
                line: r.line,
                caller_symbol_id: r.caller_symbol_id,
            }),
            Some((p, l)) if *p != r.path || *l != r.line => {
                out.push(RefDiffRow {
                    change: DiffChange::Moved,
                    path: p.clone(),
                    line: *l,
                    caller_symbol_id: r.caller_symbol_id,
                })
            }
            _ => {}
        }
    }
    for r in &b {
        if !key_a.contains_key(&r.key()) {
            out.push(RefDiffRow {
                change: DiffChange::Added,
                path: r.path.clone(),
                line: r.line,
                caller_symbol_id: r.caller_symbol_id,
            });
        }
    }
    // Stable order: change, path, line.
    out.sort_by(|x, y| {
        x.change
            .as_str()
            .cmp(y.change.as_str())
            .then_with(|| x.path.cmp(&y.path))
            .then_with(|| x.line.cmp(&y.line))
    });
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct BodyDiffLine {
    pub line_a: i32,
    pub line_b: i32,
    pub op: BodyOp,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyOp {
    Equal,
    Added,
    Removed,
}

impl BodyOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Equal => "equal",
            Self::Added => "added",
            Self::Removed => "removed",
        }
    }
}

/// Line-level diff of a symbol's body (via its signature field, which
/// we track as first-line text).  Returns Myers-diff-shaped
/// `BodyDiffLine` rows suitable for UI rendering.
pub fn lsp_body_diff(
    db: &EmbeddedDatabase,
    symbol_id: i64,
    at_a: &AsOfRef,
    at_b: &AsOfRef,
) -> Result<Vec<BodyDiffLine>> {
    let a = fetch_signature(db, symbol_id, at_a)?;
    let b = fetch_signature(db, symbol_id, at_b)?;
    Ok(myers_diff(&a, &b))
}

#[derive(Debug, Clone, PartialEq)]
pub struct AstDiffRow {
    pub change: DiffChange,
    pub kind: String,
    pub qualified: String,
    pub line_a: Option<i32>,
    pub line_b: Option<i32>,
}

/// File-level structural diff — which symbols exist / moved / disappeared
/// between two temporal points. Matching key is `qualified`.
pub fn ast_diff(
    db: &EmbeddedDatabase,
    file_path: &str,
    at_a: &AsOfRef,
    at_b: &AsOfRef,
) -> Result<Vec<AstDiffRow>> {
    let a = fetch_symbols_for_path(db, file_path, at_a)?;
    let b = fetch_symbols_for_path(db, file_path, at_b)?;
    let a_map: HashMap<String, (String, i32)> = a
        .iter()
        .map(|s| (s.qualified.clone(), (s.kind.clone(), s.line_start)))
        .collect();
    let b_map: HashMap<String, (String, i32)> = b
        .iter()
        .map(|s| (s.qualified.clone(), (s.kind.clone(), s.line_start)))
        .collect();
    let mut out = Vec::new();
    for s in &a {
        match b_map.get(&s.qualified) {
            None => out.push(AstDiffRow {
                change: DiffChange::Removed,
                kind: s.kind.clone(),
                qualified: s.qualified.clone(),
                line_a: Some(s.line_start),
                line_b: None,
            }),
            Some((_, lb)) if *lb != s.line_start => out.push(AstDiffRow {
                change: DiffChange::Moved,
                kind: s.kind.clone(),
                qualified: s.qualified.clone(),
                line_a: Some(s.line_start),
                line_b: Some(*lb),
            }),
            _ => {}
        }
    }
    for s in &b {
        if !a_map.contains_key(&s.qualified) {
            out.push(AstDiffRow {
                change: DiffChange::Added,
                kind: s.kind.clone(),
                qualified: s.qualified.clone(),
                line_a: None,
                line_b: Some(s.line_start),
            });
        }
    }
    out.sort_by(|x, y| {
        x.change
            .as_str()
            .cmp(y.change.as_str())
            .then_with(|| x.qualified.cmp(&y.qualified))
    });
    Ok(out)
}

// ---------- internals ------------------------------------------------------

struct RefAt {
    path: String,
    line: i32,
    kind: String,
    caller_symbol_id: Option<i64>,
}

impl RefAt {
    fn key(&self) -> RefKey {
        match self.caller_symbol_id {
            Some(id) => RefKey::ByCaller(id),
            None => RefKey::ByLocation(self.path.clone(), self.line, self.kind.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RefKey {
    ByCaller(i64),
    ByLocation(String, i32, String),
}

fn fetch_refs(
    db: &EmbeddedDatabase,
    symbol_id: i64,
    at: &AsOfRef,
) -> Result<Vec<RefAt>> {
    let clause = at.to_sql_clause();
    let sql = format!(
        "SELECT f.path, r.line, r.kind, r.from_symbol \
         FROM _hdb_code_symbol_refs r{clause} \
         JOIN _hdb_code_files f{clause} ON f.node_id = r.file_id \
         WHERE r.to_symbol = {symbol_id}"
    );
    let rows = db.query(&sql, &[])?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let path = match row.values.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let line = match row.values.get(1) {
            Some(Value::Int4(n)) => *n,
            Some(Value::Int8(n)) => *n as i32,
            _ => continue,
        };
        let kind = match row.values.get(2) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let caller = match row.values.get(3) {
            Some(Value::Int4(n)) => Some(*n as i64),
            Some(Value::Int8(n)) => Some(*n),
            _ => None,
        };
        out.push(RefAt {
            path,
            line,
            kind,
            caller_symbol_id: caller,
        });
    }
    Ok(out)
}

fn fetch_signature(
    db: &EmbeddedDatabase,
    symbol_id: i64,
    at: &AsOfRef,
) -> Result<String> {
    let clause = at.to_sql_clause();
    let sql = format!(
        "SELECT signature FROM _hdb_code_symbols s{clause} \
         WHERE s.node_id = {symbol_id}"
    );
    let rows = db.query(&sql, &[])?;
    Ok(match rows.first().and_then(|r| r.values.first()) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    })
}

struct SymAt {
    qualified: String,
    kind: String,
    line_start: i32,
}

fn fetch_symbols_for_path(
    db: &EmbeddedDatabase,
    file_path: &str,
    at: &AsOfRef,
) -> Result<Vec<SymAt>> {
    let clause = at.to_sql_clause();
    let esc = file_path.replace('\'', "''");
    let sql = format!(
        "SELECT s.qualified, s.kind, s.line_start \
         FROM _hdb_code_symbols s{clause} \
         JOIN _hdb_code_files f{clause} ON f.node_id = s.file_id \
         WHERE f.path = '{esc}'"
    );
    let rows = db.query(&sql, &[])?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let qualified = match row.values.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let kind = match row.values.get(1) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let line_start = match row.values.get(2) {
            Some(Value::Int4(n)) => *n,
            Some(Value::Int8(n)) => *n as i32,
            _ => 0,
        };
        out.push(SymAt {
            qualified,
            kind,
            line_start,
        });
    }
    Ok(out)
}

/// Minimal line-by-line Myers-style diff over two strings split on
/// newline.  Good enough for `lsp_body_diff` — proper word / char
/// diff is explicitly out of scope (callers can wrap if they want).
fn myers_diff(a: &str, b: &str) -> Vec<BodyDiffLine> {
    let a_lines: Vec<&str> = a.lines().collect();
    let b_lines: Vec<&str> = b.lines().collect();
    // LCS table
    let (m, n) = (a_lines.len(), b_lines.len());
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in 0..m {
        for j in 0..n {
            dp[i + 1][j + 1] = if a_lines[i] == b_lines[j] {
                dp[i][j] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (m, n);
    let mut out: Vec<BodyDiffLine> = Vec::with_capacity(m + n);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a_lines[i - 1] == b_lines[j - 1] {
            out.push(BodyDiffLine {
                line_a: i as i32,
                line_b: j as i32,
                op: BodyOp::Equal,
                text: a_lines[i - 1].to_string(),
            });
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            out.push(BodyDiffLine {
                line_a: 0,
                line_b: j as i32,
                op: BodyOp::Added,
                text: b_lines[j - 1].to_string(),
            });
            j -= 1;
        } else {
            out.push(BodyDiffLine {
                line_a: i as i32,
                line_b: 0,
                op: BodyOp::Removed,
                text: a_lines[i - 1].to_string(),
            });
            i -= 1;
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_of_clause_renders() {
        assert_eq!(AsOfRef::Now.to_sql_clause(), "");
        assert_eq!(
            AsOfRef::commit("abc").to_sql_clause(),
            " AS OF COMMIT 'abc'"
        );
        assert_eq!(
            AsOfRef::timestamp("2025-01-01").to_sql_clause(),
            " AS OF TIMESTAMP '2025-01-01'"
        );
    }

    #[test]
    fn escapes_quote_in_as_of_literal() {
        assert_eq!(
            AsOfRef::commit("a'b").to_sql_clause(),
            " AS OF COMMIT 'a''b'"
        );
    }

    #[test]
    fn myers_diff_identifies_added_removed_equal() {
        let diff = myers_diff("fn foo()\nbody\n", "fn foo()\nchanged\n");
        let ops: Vec<BodyOp> = diff.iter().map(|d| d.op).collect();
        assert!(ops.contains(&BodyOp::Equal));
        assert!(ops.contains(&BodyOp::Added));
        assert!(ops.contains(&BodyOp::Removed));
    }
}
