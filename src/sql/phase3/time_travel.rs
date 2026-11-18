//! Time-Travel SQL Parser
//!
//! Parses SQL extensions for time-travel queries:
//! - SELECT ... FROM table AS OF TIMESTAMP '2025-11-15 06:00:00'
//! - SELECT ... FROM table AS OF TRANSACTION 987654
//! - SELECT ... FROM table AS OF SCN 123456789
//! - SELECT ... FROM table VERSIONS BETWEEN ...

use super::super::logical_plan::AsOfClause;
use crate::{Error, Result};

/// Parser for time-travel SQL clauses
pub struct TimeTravelParser;

impl TimeTravelParser {
    /// Parse AS OF clause from table reference
    ///
    /// Called when parser encounters: `FROM table AS OF <clause>`
    pub fn parse_as_of_clause(clause_str: &str) -> Result<AsOfClause> {
        let trimmed = clause_str.trim().to_uppercase();

        if trimmed == "NOW" {
            return Ok(AsOfClause::Now);
        }

        // Parse TIMESTAMP '2025-11-15 06:00:00'
        if trimmed.starts_with("TIMESTAMP") {
            let ts_str = clause_str
                .trim()
                .strip_prefix("TIMESTAMP")
                .or_else(|| clause_str.trim().strip_prefix("timestamp"))
                .ok_or_else(|| Error::query_execution("Invalid TIMESTAMP syntax"))?
                .trim()
                .trim_matches('\'')
                .trim_matches('"');
            if ts_str.is_empty() {
                return Err(Error::query_execution(
                    "AS OF TIMESTAMP requires a string literal, e.g. TIMESTAMP '2025-11-15 06:00:00'",
                ));
            }
            return Ok(AsOfClause::Timestamp(ts_str.to_string()));
        }

        // Parse TRANSACTION 987654
        if trimmed.starts_with("TRANSACTION") {
            let txn_str = trimmed
                .strip_prefix("TRANSACTION")
                .ok_or_else(|| Error::query_execution("Invalid TRANSACTION syntax"))?
                .trim();
            let txn_id = txn_str
                .parse::<u64>()
                .map_err(|_| Error::query_execution("Invalid transaction ID"))?;
            return Ok(AsOfClause::Transaction(txn_id));
        }

        // Parse SCN 123456789
        if trimmed.starts_with("SCN") {
            let scn_str = trimmed
                .strip_prefix("SCN")
                .ok_or_else(|| Error::query_execution("Invalid SCN syntax"))?
                .trim();
            let scn = scn_str
                .parse::<u64>()
                .map_err(|_| Error::query_execution("Invalid SCN"))?;
            return Ok(AsOfClause::Scn(scn));
        }

        Err(Error::query_execution(format!(
            "Invalid AS OF clause: {}. Expected NOW, TIMESTAMP, TRANSACTION, or SCN",
            clause_str
        )))
    }

    /// Check if a query contains a real time-travel clause
    ///
    /// A clause is `<table> AS OF <NOW|TIMESTAMP|TRANSACTION|SCN|COMMIT> ...` or
    /// `<table> VERSIONS BETWEEN <point> AND <point>`, found by tokens, so
    /// aliases such as `AS offer_id` and text inside literals, comments and
    /// quoted identifiers never count.
    pub fn contains_time_travel_syntax(sql: &str) -> bool {
        !Self::scan(sql).is_empty()
    }

    /// Extract the first AS OF clause operand from SQL (e.g. `TIMESTAMP '...'`)
    pub fn extract_as_of_from_sql(sql: &str) -> Option<String> {
        Self::scan(sql).into_iter().find(|c| !c.is_versions).map(|c| c.operand)
    }

    /// Find every time-travel clause in the statement, in text order.
    ///
    /// One pass over the original text; byte ranges refer to `sql` itself.
    pub fn scan(sql: &str) -> Vec<TimeTravelClause> {
        if !Self::may_contain_clause(sql) {
            return Vec::new();
        }
        let toks = tokenize(sql);
        scan_tokens(sql, &toks)
    }

    /// Return the statement with every clause removed (what sqlparser can read).
    pub fn strip_clauses(sql: &str, clauses: &[TimeTravelClause]) -> String {
        let mut out = String::with_capacity(sql.len());
        let mut pos = 0usize;
        for c in clauses {
            if c.start < pos {
                continue;
            }
            out.push_str(sql.get(pos..c.start).unwrap_or_default());
            out.push(' ');
            pos = c.end;
        }
        out.push_str(sql.get(pos..).unwrap_or_default());
        out
    }

    /// Cheap byte-level pre-check: `as` + whitespace + `of`, or `versions`.
    fn may_contain_clause(sql: &str) -> bool {
        let b = sql.as_bytes();
        let n = b.len();
        let mut i = 0;
        while i + 1 < n {
            let c = b.get(i).copied().unwrap_or(0) | 0x20;
            let d = b.get(i + 1).copied().unwrap_or(0) | 0x20;
            if c == b'a' && d == b's' {
                let mut j = i + 2;
                let mut ws = false;
                while b
                    .get(j)
                    .is_some_and(|x| x.is_ascii_whitespace() || *x == b'/' || *x == b'-')
                {
                    ws = true;
                    j += 1;
                }
                if ws && b.get(j).is_some_and(|x| x | 0x20 == b'o') && b.get(j + 1).is_some_and(|x| x | 0x20 == b'f') {
                    return true;
                }
            } else if c == b'v'
                && d == b'e'
                && sql.get(i..).is_some_and(|r| {
                    r.len() >= 8
                        && r.as_bytes()
                            .get(..8)
                            .is_some_and(|w| w.eq_ignore_ascii_case(b"versions"))
                })
            {
                return true;
            }
            i += 1;
        }
        false
    }

    /// Parse VERSIONS BETWEEN clause
    ///
    /// Syntax:
    /// ```sql
    /// VERSIONS BETWEEN TIMESTAMP '2025-11-15 06:00:00'
    ///          AND TIMESTAMP '2025-11-15 07:00:00'
    /// ```
    ///
    /// Returns (start_clause, end_clause)
    pub fn parse_versions_between(clause_str: &str) -> Result<(AsOfClause, AsOfClause)> {
        // Find the AND separator by token, so a literal containing " AND " is safe
        let toks = tokenize(clause_str);
        let and_tok = toks
            .iter()
            .find(|t| {
                t.kind == Tk::Word
                    && clause_str
                        .get(t.start..t.end)
                        .is_some_and(|w| w.eq_ignore_ascii_case("AND"))
            })
            .ok_or_else(|| Error::query_execution("VERSIONS BETWEEN requires AND"))?;

        let start_str = clause_str.get(..and_tok.start).unwrap_or_default().trim();
        let end_str = clause_str.get(and_tok.end..).unwrap_or_default().trim();

        let start_clause = Self::parse_as_of_clause(start_str)?;
        let end_clause = Self::parse_as_of_clause(end_str)?;

        Ok((start_clause, end_clause))
    }
}

/// A time-travel clause found in a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeTravelClause {
    /// Lower-cased, unquoted name of the table the clause follows
    pub table: String,
    /// How many earlier FROM/JOIN references to the same table precede it
    pub occurrence: usize,
    /// Byte range in the original text to remove (`AS OF ...` / `VERSIONS BETWEEN ...`)
    pub start: usize,
    pub end: usize,
    /// Text after `AS OF` / `VERSIONS BETWEEN` (e.g. `TIMESTAMP '...'`)
    pub operand: String,
    /// True for VERSIONS BETWEEN
    pub is_versions: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tk {
    Word,
    Quoted,
    Str,
    Punct,
}

#[derive(Debug, Clone, Copy)]
struct Token {
    kind: Tk,
    start: usize,
    end: usize,
}

fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

fn is_word_char(c: u8) -> bool {
    is_word_start(c) || c == b'$'
}

/// Single pass over the bytes. Literals, quoted identifiers, dollar quotes and
/// comments become one token (or none); every token boundary is an ASCII byte,
/// so all offsets are char boundaries of the original string.
fn tokenize(sql: &str) -> Vec<Token> {
    let b = sql.as_bytes();
    let n = b.len();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut toks: Vec<Token> = Vec::new();
    let mut i = 0usize;
    while i < n {
        let c = at(i);
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'-' && at(i + 1) == b'-' {
            while i < n && at(i) != b'\n' {
                i += 1;
            }
        } else if c == b'/' && at(i + 1) == b'*' {
            let mut depth = 1usize;
            i += 2;
            while i < n && depth > 0 {
                if at(i) == b'/' && at(i + 1) == b'*' {
                    depth += 1;
                    i += 2;
                } else if at(i) == b'*' && at(i + 1) == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if c == b'\'' {
            let mut start = i;
            // E'...' allows backslash escapes
            let mut backslash = false;
            if let Some(last) = toks.last() {
                if last.kind == Tk::Word && last.end == i && last.end - last.start == 1 {
                    let w = at(last.start);
                    if w == b'e' || w == b'E' {
                        backslash = true;
                    }
                    if matches!(w, b'e' | b'E' | b'b' | b'B' | b'x' | b'X' | b'n' | b'N') {
                        start = last.start;
                        toks.pop();
                    }
                }
            }
            i += 1;
            while i < n {
                let d = at(i);
                if backslash && d == b'\\' {
                    i += 2;
                } else if d == b'\'' {
                    if at(i + 1) == b'\'' {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            let end = i.min(n);
            i = end;
            toks.push(Token {
                kind: Tk::Str,
                start,
                end,
            });
        } else if c == b'"' || c == b'`' {
            let start = i;
            i += 1;
            while i < n {
                if at(i) == c {
                    if at(i + 1) == c {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            let end = i.min(n);
            i = end;
            toks.push(Token {
                kind: Tk::Quoted,
                start,
                end,
            });
        } else if c == b'$' {
            // $tag$ ... $tag$ (tag may be empty, may not start with a digit)
            let mut j = i + 1;
            if is_word_start(at(j)) && !at(j).is_ascii_digit() {
                while j < n && is_word_char(at(j)) && at(j) != b'$' {
                    j += 1;
                }
            }
            if at(j) == b'$' && !(j > i + 1 && at(i + 1).is_ascii_digit()) {
                let tag = sql.get(i..=j).unwrap_or("$$");
                let body = j + 1;
                let end = match sql.get(body..).and_then(|r| r.find(tag)) {
                    Some(off) => body + off + tag.len(),
                    None => n,
                };
                toks.push(Token {
                    kind: Tk::Str,
                    start: i,
                    end,
                });
                i = end;
            } else {
                toks.push(Token {
                    kind: Tk::Punct,
                    start: i,
                    end: i + 1,
                });
                i += 1;
            }
        } else if is_word_start(c) {
            let start = i;
            while i < n && is_word_char(at(i)) {
                i += 1;
            }
            toks.push(Token {
                kind: Tk::Word,
                start,
                end: i,
            });
        } else {
            toks.push(Token {
                kind: Tk::Punct,
                start: i,
                end: i + 1,
            });
            i += 1;
        }
    }
    toks
}

fn is_kw(sql: &str, t: Option<&Token>, kw: &str) -> bool {
    t.is_some_and(|t| t.kind == Tk::Word && sql.get(t.start..t.end).is_some_and(|w| w.eq_ignore_ascii_case(kw)))
}

fn ident_name(sql: &str, t: &Token) -> String {
    let raw = sql.get(t.start..t.end).unwrap_or_default();
    if t.kind == Tk::Quoted {
        let q = raw.chars().next().unwrap_or('"');
        let inner = raw.get(1..raw.len().saturating_sub(1)).unwrap_or_default();
        inner.replace(&format!("{q}{q}"), &q.to_string()).to_lowercase()
    } else {
        raw.to_lowercase()
    }
}

/// Words that cannot be a table alias or name sitting in front of a clause.
const NOT_A_TABLE_WORD: &[&str] = &[
    "FROM",
    "JOIN",
    "ONLY",
    "LATERAL",
    "INTO",
    "UPDATE",
    "TABLE",
    "USING",
    "ON",
    "AND",
    "OR",
    "WHERE",
    "SELECT",
    "BY",
    "AS",
    "BETWEEN",
    "NOT",
    "IN",
    "IS",
    "WHEN",
    "THEN",
    "ELSE",
    "CASE",
    "LIKE",
    "SET",
    "VALUES",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "UNION",
    "ALL",
    "DISTINCT",
    "RETURNING",
    "WITH",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "NATURAL",
    "OUTER",
    "NULL",
    "TRUE",
    "FALSE",
    "OF",
    "GROUP",
    "ORDER",
];

fn is_name_token(sql: &str, t: &Token) -> bool {
    match t.kind {
        Tk::Quoted => true,
        Tk::Word => {
            let w = sql.get(t.start..t.end).unwrap_or_default();
            !NOT_A_TABLE_WORD.iter().any(|k| w.eq_ignore_ascii_case(k))
                && !w.bytes().next().is_some_and(|c| c.is_ascii_digit())
        }
        _ => false,
    }
}

/// The table factor a clause keyword at token index `kw` follows: the token
/// before it, or - when that is an alias (`t a`, `t AS a`) - the table before
/// the alias. Returns the index of the table-name token.
fn table_before(sql: &str, toks: &[Token], kw: usize) -> Option<usize> {
    let p1 = kw.checked_sub(1)?;
    let t1 = toks.get(p1)?;
    if !is_name_token(sql, t1) {
        return None;
    }
    let p0 = match p1.checked_sub(1) {
        Some(p) => p,
        None => return Some(p1),
    };
    let before = toks.get(p0)?;
    if is_kw(sql, Some(before), "AS") {
        let tb = p0.checked_sub(1)?;
        return toks.get(tb).filter(|t| is_name_token(sql, t)).map(|_| tb);
    }
    if is_name_token(sql, before) && before.kind == Tk::Word {
        // `t a`: t is the table, a the alias
        return Some(p0);
    }
    Some(p1)
}

/// Token count of a time point starting at `i` (NOW | TIMESTAMP 'lit' |
/// TRANSACTION n | SCN n | COMMIT 'lit'); `None` when no point starts there.
fn point_len(sql: &str, toks: &[Token], i: usize) -> Option<usize> {
    let t = toks.get(i)?;
    if t.kind != Tk::Word {
        return None;
    }
    let w = sql.get(t.start..t.end)?;
    let next = toks.get(i + 1);
    if w.eq_ignore_ascii_case("NOW") {
        Some(1)
    } else if w.eq_ignore_ascii_case("TIMESTAMP") || w.eq_ignore_ascii_case("COMMIT") {
        Some(if next.is_some_and(|n| n.kind == Tk::Str) { 2 } else { 1 })
    } else if w.eq_ignore_ascii_case("TRANSACTION") || w.eq_ignore_ascii_case("SCN") {
        let num = next.is_some_and(|n| {
            n.kind == Tk::Word
                && sql
                    .get(n.start..n.end)
                    .is_some_and(|s| s.bytes().all(|c| c.is_ascii_digit()))
        });
        Some(if num { 2 } else { 1 })
    } else {
        None
    }
}

fn scan_tokens(sql: &str, toks: &[Token]) -> Vec<TimeTravelClause> {
    // Branch statements carry their own AS OF grammar (handled elsewhere).
    if is_kw(sql, toks.first(), "CREATE")
        && (is_kw(sql, toks.get(1), "BRANCH")
            || (is_kw(sql, toks.get(1), "DATABASE") && is_kw(sql, toks.get(2), "BRANCH")))
    {
        return Vec::new();
    }
    // Table references: a name directly after FROM / JOIN (ONLY, LATERAL skipped).
    let mut refs: Vec<(usize, String)> = Vec::new();
    for (idx, t) in toks.iter().enumerate() {
        if !(t.kind == Tk::Word || t.kind == Tk::Quoted) {
            continue;
        }
        let mut p = idx.checked_sub(1);
        while let Some(pi) = p {
            if is_kw(sql, toks.get(pi), "ONLY") {
                p = pi.checked_sub(1);
            } else {
                break;
            }
        }
        if p.is_some_and(|pi| is_kw(sql, toks.get(pi), "FROM") || is_kw(sql, toks.get(pi), "JOIN"))
            && is_name_token(sql, t)
        {
            refs.push((idx, ident_name(sql, t)));
        }
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        let as_of = is_kw(sql, toks.get(i), "AS") && is_kw(sql, toks.get(i + 1), "OF");
        let versions = is_kw(sql, toks.get(i), "VERSIONS") && is_kw(sql, toks.get(i + 1), "BETWEEN");
        if !(as_of || versions) {
            i += 1;
            continue;
        }
        let Some(first_len) = point_len(sql, toks, i + 2) else {
            i += 1;
            continue;
        };
        let Some(table_idx) = table_before(sql, toks, i) else {
            i += 1;
            continue;
        };
        let mut last = i + 2 + first_len; // exclusive
        if versions {
            if !is_kw(sql, toks.get(last), "AND") {
                i += 1;
                continue;
            }
            let Some(second_len) = point_len(sql, toks, last + 1) else {
                i += 1;
                continue;
            };
            last += 1 + second_len;
        }
        let (Some(first), Some(lastt), Some(tt)) = (toks.get(i), toks.get(last - 1), toks.get(table_idx)) else {
            break;
        };
        let operand_start = toks.get(i + 2).map_or(first.end, |t| t.start);
        let name = ident_name(sql, tt);
        let occurrence = refs.iter().filter(|(ri, rn)| *ri < table_idx && *rn == name).count();
        out.push(TimeTravelClause {
            table: name,
            occurrence,
            start: first.start,
            end: lastt.end,
            operand: sql.get(operand_start..lastt.end).unwrap_or_default().to_string(),
            is_versions: versions,
        });
        i = last;
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_as_of_now() {
        let clause = TimeTravelParser::parse_as_of_clause("NOW").unwrap();
        assert_eq!(clause, AsOfClause::Now);
    }

    #[test]
    fn test_parse_as_of_timestamp() {
        let clause = TimeTravelParser::parse_as_of_clause("TIMESTAMP '2025-11-15 06:00:00'").unwrap();
        assert_eq!(clause, AsOfClause::Timestamp("2025-11-15 06:00:00".to_string()));
    }

    #[test]
    fn test_contains_time_travel_syntax() {
        assert!(TimeTravelParser::contains_time_travel_syntax(
            "SELECT * FROM orders AS OF TIMESTAMP '2025-11-15 06:00:00'"
        ));
        assert!(TimeTravelParser::contains_time_travel_syntax(
            "SELECT * FROM orders VERSIONS BETWEEN TIMESTAMP '2025-11-15 06:00:00' AND NOW"
        ));
        assert!(!TimeTravelParser::contains_time_travel_syntax(
            "SELECT * FROM orders WHERE id = 1"
        ));
    }

    #[test]
    fn test_extract_as_of_from_sql() {
        let sql = "SELECT * FROM orders AS OF TIMESTAMP '2025-11-15 06:00:00' WHERE id = 1";
        let clause = TimeTravelParser::extract_as_of_from_sql(sql).unwrap();
        assert!(clause.contains("TIMESTAMP"));
        assert!(clause.contains("2025-11-15 06:00:00"));
    }

    #[test]
    fn test_parse_versions_between() {
        let clause_str = "TIMESTAMP '2025-11-15 06:00:00' AND TIMESTAMP '2025-11-15 07:00:00'";
        let (start, end) = TimeTravelParser::parse_versions_between(clause_str).unwrap();

        assert_eq!(start, AsOfClause::Timestamp("2025-11-15 06:00:00".to_string()));
        assert_eq!(end, AsOfClause::Timestamp("2025-11-15 07:00:00".to_string()));
    }

    fn tables(sql: &str) -> Vec<(String, usize, String)> {
        TimeTravelParser::scan(sql)
            .into_iter()
            .map(|c| (c.table, c.occurrence, c.operand))
            .collect()
    }

    #[test]
    fn alias_starting_with_of_is_not_a_clause() {
        for sql in [
            "SELECT id AS offer_id FROM acc WHERE id = 1",
            "SELECT id AS offset_bal FROM acc",
            "SELECT id AS of_id FROM acc",
            "SELECT id AS OFFICIAL FROM acc",
            "SELECT id AS of FROM acc",
            "SELECT id AS \"as of\" FROM notes",
            "SELECT * FROM acc AS of WHERE of.id = 1",
        ] {
            assert!(TimeTravelParser::scan(sql).is_empty(), "{sql}");
            assert!(!TimeTravelParser::contains_time_travel_syntax(sql), "{sql}");
        }
    }

    #[test]
    fn clause_text_inside_literal_dollar_quote_and_comment() {
        for sql in [
            "SELECT 'as of now' FROM t",
            "SELECT 'x AS OF NOW' AS label FROM t",
            "DELETE FROM t WHERE body = 'balance as of yesterday' RETURNING id",
            "SELECT E'it\\'s t AS OF NOW' FROM t",
            "SELECT $$ t AS OF NOW $$ FROM t",
            "SELECT $q$ t AS OF NOW $q$ FROM t",
            "SELECT 1 FROM t -- t AS OF NOW\n",
            "SELECT 1 /* a /* nested */ t AS OF NOW */ FROM t",
            "SELECT 1 AS \"t AS OF NOW\" FROM t",
            "SELECT 1 FROM t WHERE x = 'versions between now and now'",
        ] {
            assert!(TimeTravelParser::scan(sql).is_empty(), "{sql}");
        }
    }

    #[test]
    fn case_expanding_characters_before_a_clause_do_not_panic() {
        for ch in ["ŉ", "ǰ", "ß", "İ"] {
            let sql = format!("SELECT '{}' AS x, id FROM notes AS OF NOW", ch.repeat(30));
            let c = TimeTravelParser::scan(&sql);
            assert_eq!(c.len(), 1, "{ch}");
            assert_eq!(sql.get(c[0].start..c[0].end), Some("AS OF NOW"));
            let stripped = TimeTravelParser::strip_clauses(&sql, &c);
            assert!(!stripped.contains("AS OF"));
            assert!(TimeTravelParser::parse_versions_between(&format!("NOW AND {ch}")).is_err());
        }
    }

    #[test]
    fn clause_after_alias_and_before_alias() {
        assert_eq!(
            tables("SELECT * FROM t AS OF NOW n WHERE n.id > 1"),
            [("t".into(), 0, "NOW".into())]
        );
        assert_eq!(
            tables("SELECT * FROM t a AS OF NOW WHERE a.id > 1"),
            [("t".into(), 0, "NOW".into())]
        );
        assert_eq!(
            tables("SELECT * FROM t AS a AS OF SCN 42 WHERE a.id > 1"),
            [("t".into(), 0, "SCN 42".into())]
        );
        let sql = "SELECT * FROM t AS OF NOW WHERE id > 1";
        let c = TimeTravelParser::scan(sql);
        assert_eq!(
            TimeTravelParser::strip_clauses(sql, &c)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            "SELECT * FROM t WHERE id > 1"
        );
    }

    #[test]
    fn clause_inside_parentheses() {
        assert_eq!(
            tables("SELECT * FROM (SELECT id FROM t AS OF TRANSACTION 7) s"),
            [("t".into(), 0, "TRANSACTION 7".into())]
        );
        assert_eq!(
            tables("WITH c AS (SELECT * FROM t AS OF TIMESTAMP '2025-01-01 00:00:00') SELECT * FROM c"),
            [("t".into(), 0, "TIMESTAMP '2025-01-01 00:00:00'".into())]
        );
    }

    #[test]
    fn versions_between_range() {
        let c = tables("SELECT * FROM t VERSIONS BETWEEN TIMESTAMP '2025-01-01 00:00:00' AND NOW WHERE id = 1");
        assert_eq!(c, [("t".into(), 0, "TIMESTAMP '2025-01-01 00:00:00' AND NOW".into())]);
        let (a, b) = TimeTravelParser::parse_versions_between(&c[0].2).unwrap();
        assert_eq!(a, AsOfClause::Timestamp("2025-01-01 00:00:00".into()));
        assert_eq!(b, AsOfClause::Now);
        // a literal holding " and " does not split the range
        let (a, _) = TimeTravelParser::parse_versions_between("TIMESTAMP 'a and b' AND NOW").unwrap();
        assert_eq!(a, AsOfClause::Timestamp("a and b".into()));
    }

    #[test]
    fn two_tables_two_clauses() {
        let c = tables("SELECT * FROM tt a JOIN keep3 b ON a.id = b.id AS OF TRANSACTION 3 WHERE a.id > 0");
        assert!(c.is_empty() || c[0].0 != "tt", "{c:?}");
        let c = tables("SELECT * FROM tt a JOIN keep3 b AS OF TRANSACTION 3 ON a.id = b.id");
        assert_eq!(c, [("keep3".into(), 0, "TRANSACTION 3".into())]);
        let c = tables("SELECT * FROM t AS OF NOW x JOIN t AS OF SCN 9 y ON x.id = y.id");
        assert_eq!(c, [("t".into(), 0, "NOW".into()), ("t".into(), 1, "SCN 9".into())]);
    }
}
