//! COPY statement parsing for the PG wire `COPY … FROM STDIN | TO STDOUT`
//! sub-protocol (v3.58 item 2b).
//!
//! COPY is a wire-protocol operation, not a normal query plan: `FROM STDIN`
//! streams `CopyData` frames from the client and `TO STDOUT` streams them back.
//! The handler intercepts a COPY statement here (before the normal parse/plan
//! path) and drives the copy state machine (item 2c). Kept standalone — no
//! `LogicalPlan` variant — so the central plan enum and its many exhaustive
//! matches are untouched, and OLTP/`pg35` paths never reach this code.

// Items are consumed by the handler copy state machine in item 2c; allow the
// transient unused warning until that lands in the next increment.
#![allow(dead_code)]

/// On-the-wire COPY data format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    Text,
    Csv,
    Binary,
}

/// A parsed `COPY` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopyStatement {
    pub table: String,
    /// Explicit column list, or empty for "all columns in table order".
    pub columns: Vec<String>,
    /// `true` = `COPY … TO STDOUT`, `false` = `COPY … FROM STDIN`.
    pub to_stdout: bool,
    pub format: CopyFormat,
}

/// Parse a `COPY` statement that targets STDIN/STDOUT. Returns `None` for any
/// SQL that is not such a COPY (so the caller falls through to the normal
/// parse/plan path). Only the STDIN/STDOUT forms are handled here; `COPY …
/// FROM/TO 'file'` is a server-side file op and is left to the normal path.
///
/// Grammar accepted:
///   COPY <table> [ ( <col> [, <col>]* ) ] (FROM STDIN | TO STDOUT)
///        [ [WITH] ( <opt> [, <opt>]* ) ]   -- modern: FORMAT text|csv|binary, …
///        [ [WITH] (CSV|BINARY|TEXT) ]       -- legacy bare keyword
pub(crate) fn parse_copy(sql: &str) -> Option<CopyStatement> {
    let s = sql.trim().trim_end_matches(';').trim();
    let mut rest = strip_kw(s, "COPY")?;

    // table name (up to '(' or whitespace)
    rest = rest.trim_start();
    let (table, mut rest) = take_ident(rest)?;
    rest = rest.trim_start();

    // optional column list
    let mut columns = Vec::new();
    if let Some(after_paren) = rest.strip_prefix('(') {
        let close = after_paren.find(')')?;
        let cols = &after_paren[..close];
        for c in cols.split(',') {
            let c = c.trim().trim_matches('"').trim();
            if c.is_empty() {
                return None;
            }
            columns.push(c.to_string());
        }
        rest = after_paren[close + 1..].trim_start();
    }

    // direction
    let to_stdout = if let Some(r) = strip_kw(rest, "FROM") {
        rest = strip_kw(r.trim_start(), "STDIN")?;
        false
    } else if let Some(r) = strip_kw(rest, "TO") {
        rest = strip_kw(r.trim_start(), "STDOUT")?;
        true
    } else {
        return None;
    };

    // options (default text)
    let format = parse_format(rest.trim_start());
    Some(CopyStatement {
        table,
        columns,
        to_stdout,
        format,
    })
}

fn parse_format(mut rest: &str) -> CopyFormat {
    if rest.is_empty() {
        return CopyFormat::Text;
    }
    if let Some(r) = strip_kw(rest, "WITH") {
        rest = r.trim_start();
    }
    let body = rest.trim();
    // modern: ( FORMAT csv, … )  — scan the parenthesized block
    let scan = body.trim_start_matches('(').trim_end_matches(')');
    let lower = scan.to_ascii_lowercase();
    if lower.contains("binary") {
        CopyFormat::Binary
    } else if lower.contains("csv") {
        CopyFormat::Csv
    } else {
        CopyFormat::Text
    }
}

/// Strip a leading keyword (case-insensitive) if present at a word boundary.
fn strip_kw<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let s = s.trim_start();
    if s.len() >= kw.len() && s[..kw.len()].eq_ignore_ascii_case(kw) {
        let after = &s[kw.len()..];
        if after.is_empty() || after.starts_with(|c: char| c.is_whitespace() || c == '(') {
            return Some(after);
        }
    }
    None
}

/// Take a (possibly quoted) identifier from the front; returns (ident, rest).
fn take_ident(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if let Some(after_q) = s.strip_prefix('"') {
        let end = after_q.find('"')?;
        return Some((after_q[..end].to_string(), &after_q[end + 1..]));
    }
    let end = s
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some((s[..end].to_string(), &s[end..]))
}

// ── COPY-text row decoding (item 2c) ────────────────────────────────────────
// Pure, unit-tested helpers — the correctness-critical core of COPY FROM STDIN.
// The handler accumulates CopyData bytes and calls these; the SQL it builds is
// injection-safe by construction (single-quote doubling + identifier quoting).

/// Decode one COPY-text field. `\N` is the NULL sentinel -> None; otherwise the
/// value with standard COPY escapes (`\t \n \r \b \f \v \\`) unescaped.
pub(crate) fn decode_text_field(raw: &str) -> Option<String> {
    if raw == "\\N" {
        return None;
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('b') => out.push('\u{8}'),
                Some('f') => out.push('\u{c}'),
                Some('v') => out.push('\u{b}'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other), // unknown escape: take literal
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Parse accumulated COPY-text bytes into rows of optional fields (None = NULL).
/// Stops at the `\.` end-of-data marker; drops the trailing empty segment left
/// by a final newline. UTF-8 is decoded lossily.
pub(crate) fn parse_text_rows(data: &[u8]) -> Vec<Vec<Option<String>>> {
    let text = String::from_utf8_lossy(data);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line == "\\." {
            break;
        }
        if line.is_empty() {
            continue;
        }
        rows.push(line.split('\t').map(decode_text_field).collect());
    }
    rows
}

/// Quote a SQL identifier (double-quote; double embedded quotes).
fn quote_ident(id: &str) -> String {
    format!("\"{}\"", id.replace('"', "\"\""))
}

/// Render one field as a SQL literal: NULL, or a single-quoted, `'`-escaped
/// string. All COPY-text values arrive as text and are inserted as string
/// literals; the engine coerces to the column type.
fn sql_value(v: &Option<String>) -> String {
    match v {
        None => "NULL".to_string(),
        Some(s) => format!("'{}'", s.replace('\'', "''")),
    }
}

/// Build an injection-safe multi-row INSERT for a batch of COPY rows.
/// `None` if the batch is empty.
pub(crate) fn build_insert_sql(
    table: &str,
    columns: &[String],
    rows: &[Vec<Option<String>>],
) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let cols_clause = if columns.is_empty() {
        String::new()
    } else {
        let q: Vec<String> = columns.iter().map(|c| quote_ident(c)).collect();
        format!(" ({})", q.join(", "))
    };
    let values: Vec<String> = rows
        .iter()
        .map(|r| {
            let vs: Vec<String> = r.iter().map(sql_value).collect();
            format!("({})", vs.join(", "))
        })
        .collect();
    Some(format!(
        "INSERT INTO {}{} VALUES {}",
        quote_ident(table),
        cols_clause,
        values.join(", ")
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn from_stdin_basic() {
        let c = parse_copy("COPY users FROM STDIN").unwrap();
        assert_eq!(c.table, "users");
        assert!(!c.to_stdout);
        assert!(c.columns.is_empty());
        assert_eq!(c.format, CopyFormat::Text);
    }

    #[test]
    fn from_stdin_cols_csv() {
        let c = parse_copy("COPY users (id, name) FROM STDIN WITH (FORMAT csv)").unwrap();
        assert_eq!(c.table, "users");
        assert_eq!(c.columns, vec!["id", "name"]);
        assert!(!c.to_stdout);
        assert_eq!(c.format, CopyFormat::Csv);
    }

    #[test]
    fn to_stdout_binary_and_legacy() {
        let c = parse_copy("COPY t TO STDOUT (FORMAT binary)").unwrap();
        assert!(c.to_stdout);
        assert_eq!(c.format, CopyFormat::Binary);
        let l = parse_copy("COPY t FROM STDIN WITH BINARY").unwrap();
        assert_eq!(l.format, CopyFormat::Binary);
    }

    #[test]
    fn non_copy_and_file_copy_return_none() {
        assert!(parse_copy("SELECT 1").is_none());
        // file COPY (no STDIN/STDOUT) is not handled here
        assert!(parse_copy("COPY t FROM '/tmp/x.csv'").is_none());
        assert!(parse_copy("COPYISH t FROM STDIN").is_none());
    }

    #[test]
    fn decode_field_null_and_escapes() {
        assert_eq!(decode_text_field("\\N"), None);
        assert_eq!(decode_text_field("plain"), Some("plain".to_string()));
        assert_eq!(decode_text_field("a\\tb\\nc"), Some("a\tb\nc".to_string()));
        assert_eq!(decode_text_field("a\\\\b"), Some("a\\b".to_string()));
        // empty string field is NOT null
        assert_eq!(decode_text_field(""), Some(String::new()));
    }

    #[test]
    fn parse_rows_with_null_and_terminator() {
        let data = b"1\thello\n2\t\\N\n\\.\n3\tignored";
        let rows = parse_text_rows(data);
        assert_eq!(rows.len(), 2); // stops at \.
        assert_eq!(rows[0], vec![Some("1".to_string()), Some("hello".to_string())]);
        assert_eq!(rows[1], vec![Some("2".to_string()), None]);
    }

    #[test]
    fn insert_sql_is_injection_safe() {
        let rows = vec![vec![
            Some("1".to_string()),
            Some("x'); DROP TABLE users;--".to_string()),
        ]];
        let sql = build_insert_sql("users", &["id".to_string(), "name".to_string()], &rows).unwrap();
        // the embedded quote is doubled, so the literal never closes early
        assert!(sql.contains("'x''); DROP TABLE users;--'"));
        assert!(sql.starts_with("INSERT INTO \"users\" (\"id\", \"name\") VALUES ("));
        // NULL renders unquoted
        let nrows = vec![vec![None, Some("a".to_string())]];
        let s2 = build_insert_sql("t", &[], &nrows).unwrap();
        assert!(s2.contains("VALUES (NULL, 'a')"));
        // identifier with a quote is escaped
        assert_eq!(super::quote_ident("we\"ird"), "\"we\"\"ird\"");
    }
}
