//! Pre-parser SQL rewriter + `CREATE AST INDEX` detector.
//!
//! First role: turns `lsp_xxx(...)` table-function references into
//! ordinary `SELECT ... FROM _hdb_code_*` subqueries.
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

/// Structured result of a `lsp_*` rewrite — the rewritten SQL plus any
/// session-scoped directives (currently just an `ON BRANCH '…'`
/// override) the rewriter peeled off the input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LspRewrite {
    pub sql: String,
    pub branch_override: Option<String>,
}

/// Rewrite every `lsp_*` table-function call in `sql` and return the
/// transformed SQL plus any session directives. Idempotent — input
/// with no `lsp_*` refs comes back as-is with `branch_override =
/// None`.
pub fn rewrite_lsp_calls_full(sql: &str) -> LspRewrite {
    // Fast path: most queries never mention lsp_.
    if !contains_lsp_ignore_case(sql) {
        return LspRewrite { sql: sql.to_string(), branch_override: None };
    }
    let mut out = String::with_capacity(sql.len() + 64);
    let mut rest = sql;
    let mut branch_override: Option<String> = None;
    while let Some(hit) = find_lsp_call(rest) {
        out.push_str(&rest[..hit.start]);
        let expansion = match hit.func {
            Func::Definition => expand_definition(&hit.args, hit.as_of.as_deref()),
            Func::References => expand_references(&hit.args, hit.as_of.as_deref()),
            Func::CallHierarchy => {
                expand_call_hierarchy(&hit.args, hit.as_of.as_deref())
            }
            Func::Hover => expand_hover(&hit.args, hit.as_of.as_deref()),
        };
        out.push('(');
        out.push_str(&expansion);
        out.push(')');
        // First branch override wins; later occurrences are ignored
        // rather than letting the second clobber the session scope.
        if branch_override.is_none() {
            if let Some(b) = &hit.branch {
                branch_override = Some(b.clone());
            }
        }
        rest = &rest[hit.end..];
    }
    out.push_str(rest);
    LspRewrite { sql: out, branch_override }
}

/// Backwards-compatible string-only API. Returns just the rewritten
/// SQL — any `ON BRANCH '…'` directive is lost. Internal entry points
/// (`maybe_rewrite_code_graph`) call `rewrite_lsp_calls_full` so the
/// branch directive survives.
pub fn rewrite_lsp_calls(sql: &str) -> String {
    rewrite_lsp_calls_full(sql).sql
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
    /// Optional `AS OF …` clause attached to the lsp_* reference.
    as_of: Option<String>,
    /// Optional `ON BRANCH '…'` directive attached to the lsp_*
    /// reference. Stripped from the rewrite output and surfaced via
    /// `LspRewrite::branch_override` so the call site can scope the
    /// branch switch around execution.
    branch: Option<String>,
}

/// Trailing temporal / branch clause that can follow an `lsp_*(...)`
/// table-function call.  Examples:
///   `lsp_definition('X') AS OF COMMIT 'abc'`
///   `lsp_references(42) ON BRANCH 'preview'`
///   `lsp_call_hierarchy(7, 'incoming', 2) AS OF NOW ON BRANCH 'main'`
#[derive(Debug, Clone, Default)]
struct TrailingClause {
    /// Raw SQL fragment of the AS OF clause (`AS OF COMMIT 'abc'`,
    /// `AS OF TIMESTAMP '2025-01-02'`, etc.). Propagated verbatim
    /// into every scan target in the rewritten subquery so Nano's
    /// executor handles resolution uniformly.
    as_of: Option<String>,
    /// `ON BRANCH '<name>'` override. Stripped from the rewritten
    /// SQL (Nano's parser doesn't recognise the clause); surfaced via
    /// `LspRewrite::branch_override` so the entry point can scope
    /// the branch switch around the actual query execution.
    branch: Option<String>,
    /// Length in bytes consumed from the outer SQL (so the rewriter
    /// knows where to resume).
    consumed: usize,
}

fn scan_trailing_clause(s: &str) -> TrailingClause {
    // The two clauses can appear in any order. Loop, peeling off
    // whichever matches next, until neither does.
    let mut clause = TrailingClause::default();
    let mut cursor = 0usize;
    loop {
        let mut i = cursor;
        let bytes = s.as_bytes();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let rest = &s[i..];
        let lower = rest.to_ascii_lowercase();

        if clause.as_of.is_none() && lower.starts_with("as of ") {
            if let Some((as_of, consumed_inner)) = parse_as_of(rest) {
                clause.as_of = Some(as_of);
                cursor = i + consumed_inner;
                clause.consumed = cursor;
                continue;
            }
            break;
        }

        if clause.branch.is_none() && lower.starts_with("on branch") {
            if let Some((branch, consumed_inner)) = parse_on_branch(rest) {
                clause.branch = Some(branch);
                cursor = i + consumed_inner;
                clause.consumed = cursor;
                continue;
            }
            break;
        }

        break;
    }
    clause
}

fn parse_as_of(rest: &str) -> Option<(String, usize)> {
    let after_as_of = &rest[6..];
    let after_trim = after_as_of.trim_start();
    let kw_start = rest.len() - after_trim.len();
    let low_after = after_trim.to_ascii_lowercase();
    let (kw_len, expects_literal) = if low_after.starts_with("commit") {
        (6, true)
    } else if low_after.starts_with("timestamp") {
        (9, true)
    } else if low_after.starts_with("now") {
        (3, false)
    } else if low_after.starts_with("transaction") {
        (11, true)
    } else if low_after.starts_with("scn") {
        (3, true)
    } else {
        return None;
    };
    let after_kw = &after_trim[kw_len..];
    let mut consumed_inner = kw_start + kw_len;
    let mut literal: Option<String> = None;
    if expects_literal {
        let tail = after_kw.trim_start();
        let ws_skip = after_kw.len() - tail.len();
        if !tail.starts_with('\'') {
            return None;
        }
        let close = tail[1..].find('\'')?;
        let lit = &tail[..close + 2];
        literal = Some(lit.to_string());
        consumed_inner += ws_skip + lit.len();
    }
    let kw = match kw_len {
        6 => "COMMIT",
        9 => "TIMESTAMP",
        3 => "NOW",
        11 => "TRANSACTION",
        _ => "SCN",
    };
    let clause = match literal.as_deref() {
        Some(lit) => format!("AS OF {kw} {lit}"),
        None => format!("AS OF {kw}"),
    };
    Some((clause, consumed_inner))
}

fn parse_on_branch(rest: &str) -> Option<(String, usize)> {
    // "on branch" is 9 bytes; expect whitespace then a single-quoted
    // identifier. Doubled `''` inside the literal is an escaped
    // single-quote per ANSI SQL.
    let after = &rest[9..];
    let trimmed = after.trim_start();
    let ws_skip = after.len() - trimmed.len();
    let bytes = trimmed.as_bytes();
    if bytes.first() != Some(&b'\'') {
        return None;
    }
    let mut i = 1usize;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            // Closing quote.
            let raw = &trimmed[1..i];
            let unescaped = raw.replace("''", "'");
            let total = 9 + ws_skip + i + 1;
            return Some((unescaped, total));
        }
        i += 1;
    }
    None
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
                        // Peek at what follows for AS OF / ON BRANCH.
                        let trailing = scan_trailing_clause(&s[i + 1..]);
                        return Some(Hit {
                            start: idx,
                            end: i + 1 + trailing.consumed,
                            func: func_owned,
                            args,
                            as_of: trailing.as_of,
                            branch: trailing.branch,
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

fn expand_definition(args: &[String], as_of: Option<&str>) -> String {
    // lsp_definition(name, hint_path?)
    let name = args.first().cloned().unwrap_or_else(|| "NULL".into());
    let path = args.get(1).cloned();
    let ao = as_of.map(|a| format!(" {a}")).unwrap_or_default();
    let mut s = String::new();
    write!(
        s,
        "SELECT s.node_id AS symbol_id, f.path, s.line_start AS line, \
                s.signature, s.qualified, s.kind \
         FROM _hdb_code_symbols s{ao} \
         JOIN _hdb_code_files f{ao} ON f.node_id = s.file_id \
         WHERE s.name = {name}"
    )
    .expect("fmt");
    if let Some(p) = path {
        write!(s, " AND f.path = {p}").expect("fmt");
    }
    write!(s, " ORDER BY s.node_id").expect("fmt");
    s
}

fn expand_references(args: &[String], as_of: Option<&str>) -> String {
    let id = args.first().cloned().unwrap_or_else(|| "NULL".into());
    let ao = as_of.map(|a| format!(" {a}")).unwrap_or_default();
    format!(
        "SELECT r.file_id, f.path, r.line, r.kind, r.from_symbol AS caller_symbol_id \
         FROM _hdb_code_symbol_refs r{ao} \
         JOIN _hdb_code_files f{ao} ON f.node_id = r.file_id \
         WHERE r.to_symbol = {id} \
         ORDER BY r.line"
    )
}

fn expand_call_hierarchy(args: &[String], as_of: Option<&str>) -> String {
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
    let ao = as_of.map(|a| format!(" {a}")).unwrap_or_default();
    let mut levels = Vec::with_capacity(depth as usize);
    for d in 1..=depth {
        let inner = if d == 1 {
            format!(
                "SELECT {d} AS depth, \
                        {peer_col} AS symbol_id, s.qualified, f.path, s.line_start AS line \
                 FROM _hdb_code_symbol_refs r{ao} \
                 JOIN _hdb_code_symbols s{ao} ON s.node_id = r.{peer_col} \
                 JOIN _hdb_code_files f{ao} ON f.node_id = s.file_id \
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

fn expand_hover(args: &[String], as_of: Option<&str>) -> String {
    let id = args.first().cloned().unwrap_or_else(|| "NULL".into());
    let ao = as_of.map(|a| format!(" {a}")).unwrap_or_default();
    format!(
        "SELECT s.signature, NULL AS doc, NULL AS ai_summary \
         FROM _hdb_code_symbols s{ao} \
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

    #[test]
    fn on_branch_directive_extracted_and_stripped() {
        let r = rewrite_lsp_calls_full(
            "SELECT * FROM lsp_definition('Foo') ON BRANCH 'preview'",
        );
        assert_eq!(r.branch_override.as_deref(), Some("preview"));
        assert!(!r.sql.to_ascii_lowercase().contains("on branch"));
        assert!(r.sql.contains("_hdb_code_symbols"));
    }

    #[test]
    fn on_branch_combines_with_as_of() {
        // Both clauses, AS OF first.
        let r = rewrite_lsp_calls_full(
            "SELECT * FROM lsp_references(42) AS OF COMMIT 'sha' ON BRANCH 'feat/x'",
        );
        assert_eq!(r.branch_override.as_deref(), Some("feat/x"));
        assert!(r.sql.contains("AS OF COMMIT 'sha'"));
        // ON BRANCH stripped from the rewritten output.
        assert!(!r.sql.to_ascii_lowercase().contains("on branch"));
    }

    #[test]
    fn on_branch_combines_reverse_order() {
        // ON BRANCH before AS OF is also accepted.
        let r = rewrite_lsp_calls_full(
            "SELECT * FROM lsp_hover(7) ON BRANCH 'b1' AS OF NOW",
        );
        assert_eq!(r.branch_override.as_deref(), Some("b1"));
        assert!(r.sql.contains("AS OF NOW"));
    }

    #[test]
    fn on_branch_quote_escaping() {
        // 'O''Brien' style branch name (unlikely, but the parser
        // should tolerate it).
        let r = rewrite_lsp_calls_full(
            "SELECT * FROM lsp_hover(1) ON BRANCH 'feat-O''Brien'",
        );
        assert_eq!(r.branch_override.as_deref(), Some("feat-O'Brien"));
    }

    #[test]
    fn no_on_branch_means_no_override() {
        let r = rewrite_lsp_calls_full("SELECT * FROM lsp_hover(1)");
        assert!(r.branch_override.is_none());
    }

    #[test]
    fn first_on_branch_wins() {
        // Two lsp_* calls, only the first override is honored.
        let r = rewrite_lsp_calls_full(
            "SELECT * FROM lsp_hover(1) ON BRANCH 'a', \
             lsp_hover(2) ON BRANCH 'b'",
        );
        assert_eq!(r.branch_override.as_deref(), Some("a"));
    }
}

// ============================================================================
// CREATE AST INDEX / hdb_code.pause|resume detection
// ============================================================================

/// Everything an `CREATE AST INDEX` statement binds on the engine
/// side after we've pulled it out of the SQL text. The executor takes
/// this and routes it to `code_graph::storage::code_index`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstIndexDdl {
    pub index_name: String,
    pub table: String,
    /// Column that holds source text (currently informational — the
    /// indexer reads `content` from the source table by convention).
    pub content_col: String,
    /// `lang_col` may be the name of a column that holds the per-row
    /// language tag, OR `None` when the user wrote `USING tree_sitter`
    /// without arguments (then all rows are treated as the default
    /// language — error at index time).
    pub lang_col: Option<String>,
    pub if_not_exists: bool,
    pub auto_reparse: bool,
    pub embed_bodies: bool,
    pub embed_endpoint: Option<String>,
    pub embed_bearer: Option<String>,
    pub resolve_cross_file: bool,
}

/// Return `Some(AstIndexDdl)` if `sql` is a `CREATE AST INDEX`
/// statement, else `None`.  Syntax accepted:
///
/// ```text
/// CREATE AST INDEX [IF NOT EXISTS] <name>
///     ON <table> (<content_col>)
///     [ USING tree_sitter[(<lang_col>)] ]
///     [ WITH (opt = value, ...) ]
///     [;]
/// ```
pub fn detect_create_ast_index(sql: &str) -> Option<AstIndexDdl> {
    let s = sql.trim().trim_end_matches(';').trim();
    let lower = s.to_ascii_lowercase();
    // Must start with `create ast index` (whitespace between words OK).
    let mut it = lower.split_ascii_whitespace();
    if it.next()? != "create" {
        return None;
    }
    if it.next()? != "ast" {
        return None;
    }
    if it.next()? != "index" {
        return None;
    }

    // Work on the original-case string from here on, using a simple
    // tokenizer that treats whitespace, parens, and commas as
    // separators.
    let mut t = Tokenizer::new(s);
    // advance past CREATE AST INDEX
    t.expect_word("create")?;
    t.expect_word("ast")?;
    t.expect_word("index")?;

    let mut if_not_exists = false;
    if t.peek_word_eq("if") {
        t.expect_word("if")?;
        t.expect_word("not")?;
        t.expect_word("exists")?;
        if_not_exists = true;
    }

    let index_name = t.take_ident()?;
    t.expect_word("on")?;
    let table = t.take_ident()?;
    t.expect_char('(')?;
    let content_col = t.take_ident()?;
    t.expect_char(')')?;

    // Optional USING tree_sitter[(col)]
    let mut lang_col: Option<String> = None;
    if t.peek_word_eq("using") {
        t.expect_word("using")?;
        let meth = t.take_ident()?.to_ascii_lowercase();
        if meth != "tree_sitter" {
            return None;
        }
        if t.peek_char() == Some('(') {
            t.expect_char('(')?;
            lang_col = Some(t.take_ident()?);
            t.expect_char(')')?;
        }
    }

    // Optional WITH (k = v, k = v, ...)
    let mut auto_reparse = false;
    let mut embed_bodies = false;
    let mut embed_endpoint: Option<String> = None;
    let mut embed_bearer: Option<String> = None;
    let mut resolve_cross_file = true;
    if t.peek_word_eq("with") {
        t.expect_word("with")?;
        t.expect_char('(')?;
        loop {
            let key = t.take_ident()?.to_ascii_lowercase();
            t.expect_char('=')?;
            let val = t.take_value()?;
            match key.as_str() {
                "auto_reparse" => auto_reparse = parse_bool(&val),
                "embed_bodies" => embed_bodies = parse_bool(&val),
                "embed_endpoint" => embed_endpoint = Some(val),
                "embed_bearer" => embed_bearer = Some(val),
                "resolve_cross_file" => resolve_cross_file = parse_bool(&val),
                _ => { /* ignore unknown keys for forward compat */ }
            }
            match t.peek_char() {
                Some(',') => {
                    t.expect_char(',')?;
                }
                Some(')') => break,
                _ => return None,
            }
        }
        t.expect_char(')')?;
    }

    Some(AstIndexDdl {
        index_name,
        table,
        content_col,
        lang_col,
        if_not_exists,
        auto_reparse,
        embed_bodies,
        embed_endpoint,
        embed_bearer,
        resolve_cross_file,
    })
}

/// Detect `SELECT hdb_code.pause('index_name')` and
/// `SELECT hdb_code.resume('index_name')` — both are admin calls
/// that toggle the index's auto_reparse flag in process-local state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PauseResume {
    Pause(String),
    Resume(String),
}

pub fn detect_pause_resume(sql: &str) -> Option<PauseResume> {
    let s = sql.trim().trim_end_matches(';');
    let low = s.to_ascii_lowercase();
    for (needle, ctor) in &[
        ("hdb_code.pause", true),
        ("hdb_code.resume", false),
    ] {
        if let Some(i) = low.find(needle) {
            let after = &s[i + needle.len()..];
            let after = after.trim_start();
            if !after.starts_with('(') {
                continue;
            }
            let inner = &after[1..];
            let close = inner.find(')')?;
            let arg = inner[..close].trim().trim_matches('\'').to_string();
            if arg.is_empty() {
                return None;
            }
            return Some(if *ctor {
                PauseResume::Pause(arg)
            } else {
                PauseResume::Resume(arg)
            });
        }
    }
    None
}

fn parse_bool(v: &str) -> bool {
    matches!(
        v.trim().trim_matches('\'').to_ascii_lowercase().as_str(),
        "true" | "t" | "1" | "yes"
    )
}

// ---------- tiny tokenizer -------------------------------------------------

struct Tokenizer<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(s: &'a str) -> Self { Self { src: s, pos: 0 } }

    fn skip_ws(&mut self) {
        while self.pos < self.src.len() {
            let c = self.src.as_bytes()[self.pos];
            if c.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek_char(&mut self) -> Option<char> {
        self.skip_ws();
        self.src.as_bytes().get(self.pos).map(|b| *b as char)
    }

    fn expect_char(&mut self, c: char) -> Option<()> {
        self.skip_ws();
        let b = self.src.as_bytes().get(self.pos).copied()?;
        if b as char != c {
            return None;
        }
        self.pos += 1;
        Some(())
    }

    fn expect_word(&mut self, word: &str) -> Option<()> {
        self.skip_ws();
        let end = self.pos + word.len();
        let slice = self.src.get(self.pos..end)?;
        if !slice.eq_ignore_ascii_case(word) {
            return None;
        }
        // Must not be followed by an identifier char.
        let next = self.src.as_bytes().get(end).copied();
        if matches!(next, Some(c) if (c as char).is_ascii_alphanumeric() || c == b'_') {
            return None;
        }
        self.pos = end;
        Some(())
    }

    fn peek_word_eq(&mut self, word: &str) -> bool {
        self.skip_ws();
        self.src
            .get(self.pos..self.pos + word.len())
            .map(|s| s.eq_ignore_ascii_case(word))
            .unwrap_or(false)
            && self
                .src
                .as_bytes()
                .get(self.pos + word.len())
                .map(|c| !((*c as char).is_ascii_alphanumeric() || *c == b'_'))
                .unwrap_or(true)
    }

    fn take_ident(&mut self) -> Option<String> {
        self.skip_ws();
        let bytes = self.src.as_bytes();
        // Support double-quoted identifiers: "foo"
        if bytes.get(self.pos).copied() == Some(b'"') {
            self.pos += 1;
            let start = self.pos;
            while self.pos < bytes.len() && bytes[self.pos] != b'"' {
                self.pos += 1;
            }
            let name = self.src.get(start..self.pos)?.to_string();
            if self.pos < bytes.len() && bytes[self.pos] == b'"' {
                self.pos += 1;
            }
            return Some(name);
        }
        let start = self.pos;
        while self.pos < bytes.len() {
            let b = bytes[self.pos];
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return None;
        }
        Some(self.src.get(start..self.pos)?.to_string())
    }

    fn take_value(&mut self) -> Option<String> {
        self.skip_ws();
        let bytes = self.src.as_bytes();
        if bytes.get(self.pos).copied() == Some(b'\'') {
            self.pos += 1;
            let start = self.pos;
            while self.pos < bytes.len() && bytes[self.pos] != b'\'' {
                self.pos += 1;
            }
            let v = self.src.get(start..self.pos)?.to_string();
            if self.pos < bytes.len() && bytes[self.pos] == b'\'' {
                self.pos += 1;
            }
            return Some(v);
        }
        // Unquoted: take until whitespace, comma, or paren.
        let start = self.pos;
        while self.pos < bytes.len() {
            let b = bytes[self.pos];
            if b.is_ascii_whitespace() || b == b',' || b == b')' {
                break;
            }
            self.pos += 1;
        }
        Some(self.src.get(start..self.pos)?.to_string())
    }
}

#[cfg(test)]
mod ast_index_tests {
    use super::*;

    #[test]
    fn simple_create_ast_index() {
        let d = detect_create_ast_index(
            "CREATE AST INDEX src_ast ON src (content) USING tree_sitter(lang)",
        )
        .unwrap();
        assert_eq!(d.index_name, "src_ast");
        assert_eq!(d.table, "src");
        assert_eq!(d.content_col, "content");
        assert_eq!(d.lang_col.as_deref(), Some("lang"));
        assert!(!d.auto_reparse);
    }

    #[test]
    fn with_options() {
        let d = detect_create_ast_index(
            "CREATE AST INDEX IF NOT EXISTS a ON t (content) \
             USING tree_sitter(lang) \
             WITH (auto_reparse = true, embed_endpoint = 'http://x', resolve_cross_file = false);",
        )
        .unwrap();
        assert!(d.if_not_exists);
        assert!(d.auto_reparse);
        assert_eq!(d.embed_endpoint.as_deref(), Some("http://x"));
        assert!(!d.resolve_cross_file);
    }

    #[test]
    fn not_an_ast_index() {
        assert!(detect_create_ast_index("CREATE INDEX x ON t (a)").is_none());
        assert!(detect_create_ast_index("SELECT 1").is_none());
    }

    #[test]
    fn pause_resume() {
        assert_eq!(
            detect_pause_resume("SELECT hdb_code.pause('src_ast')"),
            Some(PauseResume::Pause("src_ast".into()))
        );
        assert_eq!(
            detect_pause_resume("select  hdb_code.resume('a') ;"),
            Some(PauseResume::Resume("a".into()))
        );
        assert!(detect_pause_resume("SELECT 1").is_none());
    }
}
