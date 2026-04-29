//! SQLite-to-Helios SQL compatibility translator.
//!
//! Pre-parser SQL rewrites that absorb SQLite-specific syntax so existing
//! `sqlite3`-driven Python apps work over the PostgreSQL wire / embedded
//! REPL with a one-line import swap.
//!
//! Handles:
//! - `?` positional placeholders → `$1, $2, ...` (quote-aware, statement-local).
//! - `INSERT OR REPLACE INTO t (cols) VALUES ...`
//!     → `INSERT INTO t (cols) VALUES ... ON CONFLICT DO UPDATE SET cols = EXCLUDED.cols`.
//! - `INSERT OR IGNORE INTO ...` → `INSERT INTO ... ON CONFLICT DO NOTHING`.
//! - `INTEGER PRIMARY KEY AUTOINCREMENT` → `BIGSERIAL PRIMARY KEY`.
//! - `DATETIME('now')` → `CURRENT_TIMESTAMP`.
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
    let s = rewrite_insert_or_ignore(&s);
    let s = rewrite_insert_or_replace(&s);
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
            while tag_end < bytes.len()
                && (bytes[tag_end].is_ascii_alphanumeric() || bytes[tag_end] == b'_')
            {
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

        // ? positional placeholder
        if c == b'?' {
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
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        init_regex(r"(?i)\bINTEGER\s+PRIMARY\s+KEY\s+AUTOINCREMENT\b")
    });
    re.replace_all(sql, "BIGSERIAL PRIMARY KEY").to_string()
}

/// `DATETIME('now')` (and case variants) → `CURRENT_TIMESTAMP`.
fn rewrite_datetime_now(sql: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| init_regex(r"(?i)\bDATETIME\s*\(\s*'now'\s*\)"));
    re.replace_all(sql, "CURRENT_TIMESTAMP").to_string()
}

/// `INSERT OR IGNORE INTO …` → `INSERT INTO … ON CONFLICT DO NOTHING`.
///
/// Mirrors the MySQL translator's `INSERT IGNORE` handling — appends the
/// conflict clause to the matched statement (up to the next `;` or EOF).
fn rewrite_insert_or_ignore(sql: &str) -> String {
    static FIND_RE: OnceLock<Regex> = OnceLock::new();
    let find_re = FIND_RE.get_or_init(|| init_regex(r"(?i)\bINSERT\s+OR\s+IGNORE\s+INTO\b"));
    if !find_re.is_match(sql) {
        return sql.to_string();
    }

    // Per-statement processing: split on `;` (quote-aware), rewrite each
    // statement that begins (after whitespace) with INSERT OR IGNORE.
    let stmts = split_statements_quote_aware(sql);
    let mut out = String::with_capacity(sql.len() + 32);
    for (idx, stmt) in stmts.iter().enumerate() {
        if idx > 0 {
            out.push(';');
        }
        if find_re.is_match(stmt) {
            let rewritten = find_re.replace_all(stmt, "INSERT INTO").to_string();
            // Append ON CONFLICT DO NOTHING before any trailing whitespace
            let trimmed = rewritten.trim_end();
            let trailing = &rewritten[trimmed.len()..];
            out.push_str(trimmed);
            out.push_str(" ON CONFLICT DO NOTHING");
            out.push_str(trailing);
        } else {
            out.push_str(stmt);
        }
    }
    out
}

/// `INSERT OR REPLACE INTO t (c1, c2, …) VALUES …` →
/// `INSERT INTO t (c1, c2, …) VALUES … ON CONFLICT DO UPDATE SET c1 = EXCLUDED.c1, c2 = EXCLUDED.c2, …`.
///
/// Requires a parenthesised column list — without it we fall back to a plain
/// `INSERT INTO`, which surfaces a regular unique-violation error if the row
/// exists. Token-dashboard always names columns explicitly; clients that
/// don't can either name them or use `ON CONFLICT` directly.
fn rewrite_insert_or_replace(sql: &str) -> String {
    static FIND_RE: OnceLock<Regex> = OnceLock::new();
    let find_re = FIND_RE.get_or_init(|| init_regex(r"(?i)\bINSERT\s+OR\s+REPLACE\s+INTO\b"));
    if !find_re.is_match(sql) {
        return sql.to_string();
    }

    static EXTRACT_RE: OnceLock<Regex> = OnceLock::new();
    let extract_re = EXTRACT_RE.get_or_init(|| {
        init_regex(r#"(?is)\bINSERT\s+OR\s+REPLACE\s+INTO\s+([A-Za-z_][A-Za-z0-9_\."]*)\s*\(([^)]+)\)"#)
    });

    let stmts = split_statements_quote_aware(sql);
    let mut out = String::with_capacity(sql.len() + 64);
    for (idx, stmt) in stmts.iter().enumerate() {
        if idx > 0 {
            out.push(';');
        }
        if !find_re.is_match(stmt) {
            out.push_str(stmt);
            continue;
        }

        if let Some(caps) = extract_re.captures(stmt) {
            let (Some(table_m), Some(cols_m)) = (caps.get(1), caps.get(2)) else {
                out.push_str(&find_re.replace_all(stmt, "INSERT INTO"));
                continue;
            };
            let cols_raw = cols_m.as_str();
            let cols: Vec<String> = cols_raw
                .split(',')
                .map(|c| c.trim().trim_matches('"').to_string())
                .filter(|c| !c.is_empty())
                .collect();
            let set_clause = cols
                .iter()
                .map(|c| format!("{c} = EXCLUDED.{c}"))
                .collect::<Vec<_>>()
                .join(", ");
            // Reconstruct: replace the matched prefix, append ON CONFLICT
            let _ = table_m; // table name kept inline by replacement
            let stripped = find_re.replace_all(stmt, "INSERT INTO").to_string();
            let trimmed = stripped.trim_end();
            let trailing = &stripped[trimmed.len()..];
            out.push_str(trimmed);
            out.push_str(" ON CONFLICT DO UPDATE SET ");
            out.push_str(&set_clause);
            out.push_str(trailing);
        } else {
            // No column list — fall back to plain INSERT INTO.
            out.push_str(&find_re.replace_all(stmt, "INSERT INTO"));
        }
    }
    out
}

/// Split a SQL string on top-level `;` boundaries, ignoring `;` inside
/// single-quoted strings, double-quoted identifiers, dollar-quoted strings,
/// and SQL comments. Returns the original separators stripped — callers
/// rejoin with `;`.
fn split_statements_quote_aware(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0_usize;
    let mut i = 0_usize;

    while i < bytes.len() {
        let c = bytes[i];
        if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            continue;
        }
        if c == b'\'' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    i += 1;
                    if bytes.get(i) == Some(&b'\'') {
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        if c == b'$' {
            let tag_start = i + 1;
            let mut tag_end = tag_start;
            while tag_end < bytes.len()
                && (bytes[tag_end].is_ascii_alphanumeric() || bytes[tag_end] == b'_')
            {
                tag_end += 1;
            }
            if bytes.get(tag_end) == Some(&b'$') {
                let tag = &bytes[tag_start..tag_end];
                let close_total = (tag_end - tag_start) + 2;
                let mut j = tag_end + 1;
                while j < bytes.len() {
                    if bytes[j] == b'$'
                        && j + 1 + tag.len() <= bytes.len()
                        && &bytes[j + 1..j + 1 + tag.len()] == tag
                        && bytes.get(j + 1 + tag.len()) == Some(&b'$')
                    {
                        i = j + close_total;
                        break;
                    }
                    j += 1;
                }
                if i <= tag_end + 1 {
                    i = tag_end + 1;
                }
                continue;
            }
            i = tag_end;
            continue;
        }
        if c == b';' {
            parts.push(sql[start..i].to_string());
            start = i + 1;
            i += 1;
            continue;
        }
        i += 1;
    }
    if start < sql.len() {
        parts.push(sql[start..].to_string());
    }
    parts
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
    fn insert_or_ignore_rewrite() {
        let r = rewrite_insert_or_ignore("INSERT OR IGNORE INTO t (a) VALUES (1)");
        assert_eq!(r, "INSERT INTO t (a) VALUES (1) ON CONFLICT DO NOTHING");
    }

    #[test]
    fn insert_or_replace_with_cols() {
        let r = rewrite_insert_or_replace(
            "INSERT OR REPLACE INTO t (a, b, c) VALUES (1, 2, 3)",
        );
        assert_eq!(
            r,
            "INSERT INTO t (a, b, c) VALUES (1, 2, 3) ON CONFLICT DO UPDATE SET a = EXCLUDED.a, b = EXCLUDED.b, c = EXCLUDED.c"
        );
    }

    #[test]
    fn insert_or_replace_multi_statement() {
        let r = rewrite_insert_or_replace(
            "INSERT OR REPLACE INTO t (a) VALUES (1); INSERT OR REPLACE INTO u (b) VALUES (2)",
        );
        assert!(r.contains("ON CONFLICT DO UPDATE SET a = EXCLUDED.a"));
        assert!(r.contains("ON CONFLICT DO UPDATE SET b = EXCLUDED.b"));
    }

    #[test]
    fn translate_pipeline_combined() {
        let sql = "INSERT OR REPLACE INTO files (path, mtime) VALUES (?, ?)";
        let r = translate(sql).unwrap();
        assert_eq!(
            r,
            "INSERT INTO files (path, mtime) VALUES ($1, $2) ON CONFLICT DO UPDATE SET path = EXCLUDED.path, mtime = EXCLUDED.mtime"
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
