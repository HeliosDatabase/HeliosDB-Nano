//! SQLite-to-Helios SQL compatibility translator.
//!
//! Pre-parser SQL rewrites that absorb SQLite-specific syntax so existing
//! `sqlite3`-driven Python apps work over the PostgreSQL wire / embedded
//! REPL with a one-line import swap.
//!
//! Handles:
//! - `?` positional placeholders → `$1, $2, ...` (quote-aware, statement-local).
//! - `INTEGER PRIMARY KEY AUTOINCREMENT` → `BIGSERIAL PRIMARY KEY` (CREATE / ALTER
//!   only, outside quoted text).
//! - `DATETIME('now')` → `CURRENT_TIMESTAMP`.
//!
//! `INSERT OR REPLACE` / `INSERT OR IGNORE` are NOT rewritten here: the parser
//! reads them (`Insert.or`) and the planner builds the conflict clause, so no
//! text is ever altered (see `Planner::sqlite_conflict_clause`).
//!
//! `sqlite_master` is handled as a system view (see `system_views.rs`).
//! `PRAGMA …` is intercepted at the protocol layer / parser entry.

use crate::{Error, Result};
use regex::Regex;
use std::sync::OnceLock;

#[allow(clippy::expect_used)]
fn init_regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static SQLite-compat regex must be valid")
}

/// Apply every SQLite-compat rewrite to `sql` and return a SQL string the
/// PostgreSQL parser can consume directly.
///
/// Errors only on `?`/`$N` placeholder mixing; other failures (unrecognised
/// constructs) fall through unchanged so the downstream parser can produce
/// the canonical diagnostic.
pub fn translate(sql: &str) -> Result<String> {
    let s = rewrite_question_placeholders(sql)?;
    let s = rewrite_autoincrement(&s);
    let s = rewrite_datetime_now(&s);
    Ok(s)
}

/// Quote-aware replacement of `?` placeholders with PostgreSQL-style
/// `$1, $2, ...` markers. Preserves `?` inside single-quoted strings,
/// double-quoted identifiers, dollar-quoted strings, line comments
/// (`-- ...`) and block comments (`/* ... */`).
///
/// Errors if the input mixes both `?` and `$N` placeholders, since
/// the renumbering would otherwise silently corrupt parameter binding.
pub fn rewrite_question_placeholders(sql: &str) -> Result<String> {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut i = 0_usize;
    let mut next_idx = 1_usize;
    let mut saw_dollar_n = false;
    let mut saw_question = false;

    while i < bytes.len() {
        let c = bytes[i];

        // -- line comment
        if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }

        // /* block comment */
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            out.push('/');
            out.push('*');
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                out.push(bytes[i] as char);
                i += 1;
            }
            if i + 1 < bytes.len() {
                out.push('*');
                out.push('/');
                i += 2;
            }
            continue;
        }

        // 'single-quoted string' (with '' escape)
        if c == b'\'' {
            out.push('\'');
            i += 1;
            while i < bytes.len() {
                let cc = bytes[i];
                if cc == b'\'' {
                    out.push('\'');
                    i += 1;
                    if bytes.get(i) == Some(&b'\'') {
                        out.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
                out.push(cc as char);
                i += 1;
            }
            continue;
        }

        // "double-quoted identifier"
        if c == b'"' {
            out.push('"');
            i += 1;
            while i < bytes.len() {
                let cc = bytes[i];
                out.push(cc as char);
                i += 1;
                if cc == b'"' {
                    break;
                }
            }
            continue;
        }

        // $tag$ ... $tag$ dollar-quoted string OR $N positional placeholder
        if c == b'$' {
            let tag_start = i + 1;
            let mut tag_end = tag_start;
            while tag_end < bytes.len() && (bytes[tag_end].is_ascii_alphanumeric() || bytes[tag_end] == b'_') {
                tag_end += 1;
            }

            // Empty tag ($$ … $$) or tag followed by another `$` → dollar-quoted string
            if bytes.get(tag_end) == Some(&b'$') {
                let tag = &bytes[tag_start..tag_end];
                let close_total = (tag_end - tag_start) + 2; // `$tag$`
                let mut j = tag_end + 1;
                let mut closed_at: Option<usize> = None;
                while j < bytes.len() {
                    if bytes[j] == b'$'
                        && j + 1 + tag.len() <= bytes.len()
                        && &bytes[j + 1..j + 1 + tag.len()] == tag
                        && bytes.get(j + 1 + tag.len()) == Some(&b'$')
                    {
                        closed_at = Some(j + close_total);
                        break;
                    }
                    j += 1;
                }
                if let Some(close) = closed_at {
                    out.push_str(&sql[i..close]);
                    i = close;
                    continue;
                }
            }

            // $N (PG positional placeholder)
            if tag_end > tag_start && bytes[tag_start..tag_end].iter().all(|b| b.is_ascii_digit()) {
                saw_dollar_n = true;
            }
            out.push_str(&sql[i..tag_end]);
            i = tag_end;
            continue;
        }

        // ? positional placeholder.
        //
        // `?|` and `?&` are PostgreSQL's jsonb key-existence operators, never
        // placeholders — a `$N` cannot be followed by `|`/`&` in any valid
        // statement. Rewriting them produced `$1|` / `$1&`, so those operators
        // could never reach the parser at all. This translate() runs on EVERY
        // statement, not only SQLite-dialect ones, so the rewrite has to leave
        // them alone.
        //
        // The bare `?` key-existence operator stays ambiguous with a positional
        // placeholder and is still rewritten: that ambiguity is real in
        // PostgreSQL too, and SQLite placeholder compatibility is this
        // function's whole purpose.
        //
        // Workaround for the single-key form: `col ?| ARRAY['key']`, which is
        // exempted above and means the same thing. (PostgreSQL's own escape
        // hatch, `jsonb_exists(col, 'key')`, is NOT implemented here — do not
        // recommend it until it is.)
        if c == b'?' {
            let next = bytes.get(i + 1).copied();
            if next == Some(b'|') || next == Some(b'&') {
                out.push('?');
                out.push(next.unwrap_or(b'?') as char);
                i += 2;
                continue;
            }
            saw_question = true;
            out.push('$');
            out.push_str(&next_idx.to_string());
            next_idx += 1;
            i += 1;
            continue;
        }

        out.push(c as char);
        i += 1;
    }

    if saw_question && saw_dollar_n {
        return Err(Error::sql_parse(
            "Cannot mix `?` and `$N` placeholders in the same statement".to_string(),
        ));
    }

    Ok(out)
}

/// `INTEGER PRIMARY KEY AUTOINCREMENT` → `BIGSERIAL PRIMARY KEY`.
///
/// SQLite's clause is the canonical idiom for a single-column auto-incrementing
/// PK. Map it to PostgreSQL's `BIGSERIAL`. Other AUTOINCREMENT forms (e.g.
/// MySQL `AUTO_INCREMENT`) are handled separately in the MySQL translator.
fn rewrite_autoincrement(sql: &str) -> String {
    // A column clause: only CREATE / ALTER statements, and never inside a quoted
    // region (`SELECT 'x INTEGER PRIMARY KEY AUTOINCREMENT y'` is data).
    let head = sql.trim_start();
    let first: String = head.chars().take_while(char::is_ascii_alphabetic).collect();
    if !(first.eq_ignore_ascii_case("CREATE") || first.eq_ignore_ascii_case("ALTER")) {
        return sql.to_string();
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| init_regex(r"(?i)\b(?:INTEGER|INT|BIGINT)\s+PRIMARY\s+KEY\s+AUTOINCREMENT\b"));
    let mut out = String::with_capacity(sql.len());
    let mut segment = String::new();
    let mut quote: Option<char> = None;
    for ch in sql.chars() {
        match quote {
            Some(q) => {
                out.push(ch);
                if ch == q {
                    quote = None;
                }
            }
            None if ch == '\'' || ch == '"' => {
                out.push_str(&re.replace_all(&segment, "BIGSERIAL PRIMARY KEY"));
                segment.clear();
                out.push(ch);
                quote = Some(ch);
            }
            None => segment.push(ch),
        }
    }
    out.push_str(&re.replace_all(&segment, "BIGSERIAL PRIMARY KEY"));
    out
}

/// `DATETIME('now')` (and case variants) → `CURRENT_TIMESTAMP`.
fn rewrite_datetime_now(sql: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| init_regex(r"(?i)\bDATETIME\s*\(\s*'now'\s*\)"));
    re.replace_all(sql, "CURRENT_TIMESTAMP").to_string()
}

/// Detect a `PRAGMA …` statement (used by the parser/protocol layer to short-
/// circuit before sqlparser sees it). Returns `Some((name, optional_arg))`
/// or `None`. Whitespace and case-insensitive.
pub fn parse_pragma(sql: &str) -> Option<(String, Option<String>)> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let upper = trimmed.to_uppercase();
    if !upper.starts_with("PRAGMA") {
        return None;
    }
    let rest = trimmed.get(6..)?.trim();
    if rest.is_empty() {
        return None;
    }

    // PRAGMA name(arg) | PRAGMA name = value | PRAGMA name
    if let Some(open) = rest.find('(') {
        let close = rest.rfind(')')?;
        if close <= open {
            return None;
        }
        let name = rest.get(..open)?.trim().to_string();
        let arg = rest.get(open + 1..close)?.trim().to_string();
        return Some((name, Some(arg)));
    }
    if let Some(eq) = rest.find('=') {
        let name = rest.get(..eq)?.trim().to_string();
        let arg = rest.get(eq + 1..)?.trim().to_string();
        return Some((name, Some(arg)));
    }
    Some((rest.to_string(), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_placeholder_basic() {
        let r = rewrite_question_placeholders("SELECT * FROM t WHERE a = ? AND b = ?").unwrap();
        assert_eq!(r, "SELECT * FROM t WHERE a = $1 AND b = $2");
    }

    #[test]
    fn question_placeholder_skips_string_literal() {
        let r = rewrite_question_placeholders("SELECT 'hello?world' WHERE x = ?").unwrap();
        assert_eq!(r, "SELECT 'hello?world' WHERE x = $1");
    }

    #[test]
    fn question_placeholder_skips_quoted_ident() {
        let r = rewrite_question_placeholders(r#"SELECT "col?name" FROM t WHERE x = ?"#).unwrap();
        assert_eq!(r, r#"SELECT "col?name" FROM t WHERE x = $1"#);
    }

    #[test]
    fn question_placeholder_skips_dollar_quoted() {
        let r = rewrite_question_placeholders("DO $$ BEGIN RAISE NOTICE 'a?b'; END $$;").unwrap();
        assert_eq!(r, "DO $$ BEGIN RAISE NOTICE 'a?b'; END $$;");
    }

    #[test]
    fn question_placeholder_skips_line_comment() {
        let r = rewrite_question_placeholders("SELECT 1 -- comment ?\nWHERE a=?").unwrap();
        assert_eq!(r, "SELECT 1 -- comment ?\nWHERE a=$1");
    }

    #[test]
    fn question_placeholder_mixed_rejected() {
        let err = rewrite_question_placeholders("SELECT $1, ?").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Cannot mix"), "msg: {msg}");
    }

    #[test]
    fn autoincrement_rewrite() {
        let r = rewrite_autoincrement("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)");
        assert_eq!(r, "CREATE TABLE t (id BIGSERIAL PRIMARY KEY)");
    }

    #[test]
    fn datetime_now_rewrite() {
        let r = rewrite_datetime_now("INSERT INTO t VALUES (DATETIME('now'))");
        assert_eq!(r, "INSERT INTO t VALUES (CURRENT_TIMESTAMP)");
    }

    #[test]
    fn translate_pipeline_leaves_conflict_clauses_to_the_parser() {
        let sql = "INSERT OR REPLACE INTO files (path, mtime) VALUES (?, ?)";
        assert_eq!(
            translate(sql).unwrap(),
            "INSERT OR REPLACE INTO files (path, mtime) VALUES ($1, $2)"
        );
    }

    #[test]
    fn autoincrement_only_in_ddl_and_outside_quotes() {
        assert_eq!(
            rewrite_autoincrement("SELECT 'x INTEGER PRIMARY KEY AUTOINCREMENT y'"),
            "SELECT 'x INTEGER PRIMARY KEY AUTOINCREMENT y'"
        );
        assert_eq!(
            rewrite_autoincrement("CREATE TABLE t (\"INTEGER PRIMARY KEY AUTOINCREMENT\" INT, id INTEGER PRIMARY KEY AUTOINCREMENT)"),
            "CREATE TABLE t (\"INTEGER PRIMARY KEY AUTOINCREMENT\" INT, id BIGSERIAL PRIMARY KEY)"
        );
    }

    #[test]
    fn parse_pragma_table_info() {
        let p = parse_pragma("PRAGMA table_info(messages);").unwrap();
        assert_eq!(p.0, "table_info");
        assert_eq!(p.1.as_deref(), Some("messages"));
    }

    #[test]
    fn parse_pragma_assignment() {
        let p = parse_pragma("PRAGMA foreign_keys = ON").unwrap();
        assert_eq!(p.0, "foreign_keys");
        assert_eq!(p.1.as_deref(), Some("ON"));
    }

    #[test]
    fn parse_pragma_bare() {
        let p = parse_pragma("PRAGMA journal_mode").unwrap();
        assert_eq!(p.0, "journal_mode");
        assert!(p.1.is_none());
    }

    #[test]
    fn parse_pragma_none_for_select() {
        assert!(parse_pragma("SELECT 1").is_none());
    }
}
