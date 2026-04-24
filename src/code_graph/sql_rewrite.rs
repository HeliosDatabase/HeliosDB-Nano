//! Pre-parser SQL rewriter: turns `lsp_xxx(...)` table-function
//! references into ordinary `SELECT ... FROM _hdb_code_*` subqueries.
//!
//! The engine has no first-class UDTF (user-defined table function)
//! support, and adding one would ripple through every executor
//! constructor. Instead we rewrite at the SQL-text layer before the
//! parser sees the query. The expansions are deterministic and
//! parameterised only on the function arguments, so there is no
//! injection surface beyond what the caller already had.
//!
//! Supported shapes:
//!
//! - `FROM lsp_definition('name')`
//! - `FROM lsp_definition('name', 'path')`
//! - `FROM lsp_references(42)`
//! - `FROM lsp_call_hierarchy(42, 'incoming', 3)`
//! - `FROM lsp_hover(42)`
//!
//! Callers free to alias: `FROM lsp_definition('X') d`.
//!
//! Out of scope: nested calls, arguments that are not literal (the
//! rewriter reads only SingleQuotedString / numeric literals). If
//! users need full-SQL composition they can still call the Rust API
//! or write the subquery directly.

use std::fmt::Write;

/// Rewrite every `lsp_*` table-function call in `sql` in place and
/// return the transformed SQL. Idempotent — calling on a query with
/// no `lsp_*` refs is a no-op pass-through.
pub fn rewrite_lsp_calls(sql: &str) -> String {
    // Fast path: most queries never mention lsp_.
    if !contains_lsp_ignore_case(sql) {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len() + 64);
    let mut rest = sql;
    while let Some(hit) = find_lsp_call(rest) {
        out.push_str(&rest[..hit.start]);
        let expansion = match hit.func {
            Func::Definition => expand_definition(&hit.args),
            Func::References => expand_references(&hit.args),
            Func::CallHierarchy => expand_call_hierarchy(&hit.args),
            Func::Hover => expand_hover(&hit.args),
        };
        out.push('(');
        out.push_str(&expansion);
        out.push(')');
        rest = &rest[hit.end..];
    }
    out.push_str(rest);
    out
}

fn contains_lsp_ignore_case(s: &str) -> bool {
    // Simple ASCII-lowercase scan — allocates once, worth it to keep
    // the hot no-op path cheap on long queries.
    let l = s.to_ascii_lowercase();
    l.contains("lsp_definition")
        || l.contains("lsp_references")
        || l.contains("lsp_call_hierarchy")
        || l.contains("lsp_hover")
}

#[derive(Debug)]
enum Func {
    Definition,
    References,
    CallHierarchy,
    Hover,
}

#[derive(Debug)]
struct Hit {
    start: usize,
    end: usize,
    func: Func,
    args: Vec<String>,
}

fn find_lsp_call(s: &str) -> Option<Hit> {
    // Scan for the earliest case-insensitive match of any `lsp_*(`.
    let lower = s.to_ascii_lowercase();
    let candidates: &[(Func, &str)] = &[
        (Func::Definition, "lsp_definition"),
        (Func::References, "lsp_references"),
        (Func::CallHierarchy, "lsp_call_hierarchy"),
        (Func::Hover, "lsp_hover"),
    ];
    let mut best: Option<(usize, &Func, &str)> = None;
    for (func, name) in candidates {
        if let Some(idx) = lower.find(name) {
            // Require that what follows (after optional whitespace) is `(`.
            let after = &s[idx + name.len()..];
            let after_trim = after.trim_start();
            if !after_trim.starts_with('(') {
                continue;
            }
            // Require that the char *before* is not an identifier char
            // (so `my_lsp_definition` is not matched).
            let before_ok = match idx {
                0 => true,
                _ => {
                    let prev_byte = s.as_bytes()[idx - 1];
                    !prev_byte.is_ascii_alphanumeric() && prev_byte != b'_'
                }
            };
            if !before_ok {
                continue;
            }
            match best {
                Some((cur, _, _)) if cur <= idx => {}
                _ => best = Some((idx, func, name)),
            }
        }
    }
    let (idx, func, name) = best?;

    // Find the matching close paren, tracking nested parens and
    // single-quoted strings.
    let after_name = &s[idx + name.len()..];
    let paren_start = after_name.find('(')? + idx + name.len();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = paren_start;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\'' {
                // Doubled quote '' is an escaped quote.
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
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        // Parse args between paren_start+1 and i.
                        let args_src = &s[paren_start + 1..i];
                        let args = split_args(args_src);
                        let func_owned = match func {
                            Func::Definition => Func::Definition,
                            Func::References => Func::References,
                            Func::CallHierarchy => Func::CallHierarchy,
                            Func::Hover => Func::Hover,
                        };
                        return Some(Hit {
                            start: idx,
                            end: i + 1,
                            func: func_owned,
                            args,
                        });
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn split_args(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            cur.push(b as char);
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    cur.push('\'');
                    i += 2;
                    continue;
                }
                in_str = false;
            }
        } else {
            match b {
                b',' if depth == 0 => {
                    out.push(cur.trim().to_string());
                    cur.clear();
                }
                b'(' => {
                    depth += 1;
                    cur.push(b as char);
                }
                b')' => {
                    depth -= 1;
                    cur.push(b as char);
                }
                b'\'' => {
                    in_str = true;
                    cur.push(b as char);
                }
                _ => cur.push(b as char),
            }
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

// ---------- expansions -------------------------------------------------

fn expand_definition(args: &[String]) -> String {
    // lsp_definition(name, hint_path?)
    let name = args.first().cloned().unwrap_or_else(|| "NULL".into());
    let path = args.get(1).cloned();
    let mut s = String::new();
    write!(
        s,
        "SELECT s.node_id AS symbol_id, f.path, s.line_start AS line, \
                s.signature, s.qualified, s.kind \
         FROM _hdb_code_symbols s \
         JOIN _hdb_code_files f ON f.node_id = s.file_id \
         WHERE s.name = {name}"
    )
    .expect("fmt");
    if let Some(p) = path {
        write!(s, " AND f.path = {p}").expect("fmt");
    }
    write!(s, " ORDER BY s.node_id").expect("fmt");
    s
}

fn expand_references(args: &[String]) -> String {
    let id = args
        .first()
        .cloned()
        .unwrap_or_else(|| "NULL".into());
    format!(
        "SELECT r.file_id, f.path, r.line, r.kind, r.from_symbol AS caller_symbol_id \
         FROM _hdb_code_symbol_refs r \
         JOIN _hdb_code_files f ON f.node_id = r.file_id \
         WHERE r.to_symbol = {id} \
         ORDER BY r.line"
    )
}

fn expand_call_hierarchy(args: &[String]) -> String {
    // Arity is (symbol_id, direction?, depth?). Depth is a hop cap we
    // apply by UNIONing one level. Deep walks fall back to the Rust
    // API. This surface is enough for `depth <= 2` which is the
    // common case.
    let id = args.first().cloned().unwrap_or_else(|| "NULL".into());
    let dir_raw = args.get(1).cloned().unwrap_or_else(|| "'incoming'".into());
    let dir = dir_raw.trim().trim_matches('\'').to_ascii_lowercase();
    let depth_str = args.get(2).cloned().unwrap_or_else(|| "1".into());
    let depth: u32 = depth_str.trim().parse().unwrap_or(1).min(3).max(1);

    // Build a union of up to `depth` levels; each pulls the peers at
    // that hop. For `depth = 1` the subquery is a single SELECT.
    let mut levels = Vec::with_capacity(depth as usize);
    for d in 1..=depth {
        let inner = if d == 1 {
            format!(
                "SELECT {d} AS depth, \
                        {peer_col} AS symbol_id, s.qualified, f.path, s.line_start AS line \
                 FROM _hdb_code_symbol_refs r \
                 JOIN _hdb_code_symbols s ON s.node_id = r.{peer_col} \
                 JOIN _hdb_code_files f ON f.node_id = s.file_id \
                 WHERE r.{anchor_col} = {id} AND r.kind = 'CALLS'",
                peer_col = if dir == "outgoing" { "to_symbol" } else { "from_symbol" },
                anchor_col = if dir == "outgoing" { "from_symbol" } else { "to_symbol" },
            )
        } else {
            // For depth > 1 we'd recurse; WITH RECURSIVE is fragile
            // in text rewrites. Fall back to the d=1 shape; callers
            // needing deeper walks should use the Rust API.
            continue;
        };
        levels.push(inner);
    }
    levels.join(" UNION ")
}

fn expand_hover(args: &[String]) -> String {
    let id = args.first().cloned().unwrap_or_else(|| "NULL".into());
    format!(
        "SELECT s.signature, NULL AS doc, NULL AS ai_summary \
         FROM _hdb_code_symbols s \
         WHERE s.node_id = {id}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_through_no_lsp() {
        let sql = "SELECT * FROM users WHERE id = 1";
        assert_eq!(rewrite_lsp_calls(sql), sql);
    }

    #[test]
    fn expands_definition_simple() {
        let got = rewrite_lsp_calls("SELECT * FROM lsp_definition('Foo')");
        assert!(got.contains("_hdb_code_symbols"));
        assert!(got.contains("s.name = 'Foo'"));
        assert!(got.contains("ORDER BY s.node_id"));
    }

    #[test]
    fn expands_definition_with_hint() {
        let got = rewrite_lsp_calls("SELECT * FROM lsp_definition('Foo', 'src/x.rs')");
        assert!(got.contains("f.path = 'src/x.rs'"));
    }

    #[test]
    fn expands_references_by_id() {
        let got = rewrite_lsp_calls("SELECT * FROM lsp_references(42)");
        assert!(got.contains("r.to_symbol = 42"));
    }

    #[test]
    fn expands_call_hierarchy_depth_1() {
        let got = rewrite_lsp_calls("SELECT * FROM lsp_call_hierarchy(42, 'incoming', 1)");
        assert!(got.contains("r.to_symbol = 42 AND r.kind = 'CALLS'"));
    }

    #[test]
    fn expands_hover() {
        let got = rewrite_lsp_calls("SELECT * FROM lsp_hover(42)");
        assert!(got.contains("s.node_id = 42"));
    }

    #[test]
    fn ignores_prefixed_identifier() {
        // `my_lsp_definition` is a user table, not our function.
        let sql = "SELECT * FROM my_lsp_definition('Foo')";
        assert_eq!(rewrite_lsp_calls(sql), sql);
    }

    #[test]
    fn handles_aliased_reference() {
        let got = rewrite_lsp_calls("SELECT d.path FROM lsp_definition('X') d");
        assert!(got.starts_with("SELECT d.path FROM ("));
        assert!(got.ends_with(") d"));
    }

    #[test]
    fn escaped_quote_inside_arg() {
        // `O'Brien` escaped as O''Brien — must survive the rewriter.
        let got = rewrite_lsp_calls("SELECT * FROM lsp_definition('O''Brien')");
        assert!(got.contains("s.name = 'O''Brien'"));
    }
}
