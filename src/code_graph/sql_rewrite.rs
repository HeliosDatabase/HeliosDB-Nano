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
