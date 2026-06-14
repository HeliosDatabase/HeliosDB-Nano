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
}
