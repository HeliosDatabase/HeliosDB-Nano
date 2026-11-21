//! Reader for SQL Logic Test scripts, and the rules that turn a result set into the
//! lines a script expects.
//!
//! The format is the one the public corpus is written in: records separated by blank
//! lines; `statement ok|error`; `query <types> [nosort|rowsort|valuesort] [label]` with
//! the expected values after a `----` line, one value per line, or one line
//! `<n> values hashing to <md5>` once the result is larger than the file's
//! `hash-threshold`; `skipif <engine>` / `onlyif <engine>` in front of a record; `halt`.

use std::collections::HashMap;

/// How a query's values are ordered before they are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    NoSort,
    RowSort,
    ValueSort,
}

/// What one record asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Statement {
        expect_ok: bool,
    },
    Query {
        types: String,
        sort: SortMode,
        label: Option<String>,
        expected: Vec<String>,
    },
    HashThreshold(usize),
    Halt,
}

/// `skipif <engine>` (`only == false`) or `onlyif <engine>` (`only == true`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub only: bool,
    pub engine: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// 1-based line of the record's own first line (after its conditions).
    pub line: usize,
    pub conditions: Vec<Condition>,
    pub kind: Kind,
    pub sql: String,
}

impl Record {
    /// Whether the record runs for an engine of this name.
    pub fn applies_to(&self, engine: &str) -> bool {
        self.conditions.iter().all(|c| {
            let matches = c.engine.eq_ignore_ascii_case(engine);
            if c.only {
                matches
            } else {
                !matches
            }
        })
    }
}

/// Parse a whole script. A malformed record is an error with its line: the corpus is
/// never edited, so the reader has to take every file as it is.
pub fn parse(text: &str) -> Result<Vec<Record>, String> {
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut records = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines.get(i).copied().unwrap_or("");
        if line.trim().is_empty() || line.starts_with('#') {
            i += 1;
            continue;
        }
        let mut conditions = Vec::new();
        let mut head: Vec<&str> = line.split_whitespace().collect();
        while matches!(head.first().copied(), Some("skipif" | "onlyif")) {
            let engine = head
                .get(1)
                .ok_or_else(|| format!("line {}: condition without an engine name", i + 1))?;
            conditions.push(Condition {
                only: head.first().copied() == Some("onlyif"),
                engine: (*engine).to_string(),
            });
            i += 1;
            head = lines.get(i).copied().unwrap_or("").split_whitespace().collect();
        }
        let start = i + 1;
        let word = head
            .first()
            .copied()
            .ok_or_else(|| format!("line {start}: a condition is followed by nothing"))?;
        i += 1;
        match word {
            "statement" => {
                let expect_ok = match head.get(1).copied() {
                    Some("ok") => true,
                    Some("error") => false,
                    other => return Err(format!("line {start}: statement {other:?}: expected ok or error")),
                };
                let mut sql = Vec::new();
                while let Some(l) = lines.get(i).copied() {
                    if l.trim().is_empty() {
                        break;
                    }
                    sql.push(l.trim_end());
                    i += 1;
                }
                records.push(Record {
                    line: start,
                    conditions,
                    kind: Kind::Statement { expect_ok },
                    sql: sql.join("\n"),
                });
            }
            "query" => {
                let types = head
                    .get(1)
                    .copied()
                    .ok_or_else(|| format!("line {start}: query without a type string"))?;
                if types.is_empty() || !types.chars().all(|c| matches!(c, 'T' | 'I' | 'R')) {
                    return Err(format!("line {start}: type string {types:?} is not made of T, I, R"));
                }
                let sort = match head.get(2).copied() {
                    None | Some("nosort") => SortMode::NoSort,
                    Some("rowsort") => SortMode::RowSort,
                    Some("valuesort") => SortMode::ValueSort,
                    Some(other) => return Err(format!("line {start}: unknown sort mode {other:?}")),
                };
                let label = head.get(3).map(|s| (*s).to_string());
                let mut sql = Vec::new();
                let mut has_results = false;
                while let Some(l) = lines.get(i).copied() {
                    if l.trim().is_empty() {
                        break;
                    }
                    i += 1;
                    if l == "----" {
                        has_results = true;
                        break;
                    }
                    sql.push(l.trim_end());
                }
                let mut expected = Vec::new();
                if has_results {
                    while let Some(l) = lines.get(i).copied() {
                        if l.is_empty() {
                            break;
                        }
                        expected.push(l.to_string());
                        i += 1;
                    }
                }
                records.push(Record {
                    line: start,
                    conditions,
                    kind: Kind::Query {
                        types: types.to_string(),
                        sort,
                        label,
                        expected,
                    },
                    sql: sql.join("\n"),
                });
            }
            "hash-threshold" => {
                let n = head
                    .get(1)
                    .and_then(|s| s.parse::<usize>().ok())
                    .ok_or_else(|| format!("line {start}: hash-threshold needs a number"))?;
                records.push(Record {
                    line: start,
                    conditions,
                    kind: Kind::HashThreshold(n),
                    sql: String::new(),
                });
            }
            "halt" => records.push(Record {
                line: start,
                conditions,
                kind: Kind::Halt,
                sql: String::new(),
            }),
            other => return Err(format!("line {start}: unknown record type {other:?}")),
        }
    }
    Ok(records)
}

/// One value as an interface returned it. The wire interfaces only have text.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
}

/// The leading integer of a text, as C's `atoi` reads it: optional sign and digits,
/// anything else ends the number, nothing readable is 0.
fn leading_integer(s: &str) -> i64 {
    let t = s.trim_start();
    let (negative, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let mut n: i64 = 0;
    for c in digits.chars() {
        match c.to_digit(10) {
            Some(d) => n = n.saturating_mul(10).saturating_add(i64::from(d)),
            None => break,
        }
    }
    if negative {
        -n
    } else {
        n
    }
}

fn text_as_float(s: &str) -> f64 {
    let t = s.trim();
    match t {
        "t" | "true" => 1.0,
        "f" | "false" => 0.0,
        _ => t.parse::<f64>().unwrap_or_else(|_| leading_integer(t) as f64),
    }
}

/// Render one value for a column of type `ty` (`I`, `R` or `T`): integers as `%d`
/// (a fractional value is cut towards zero), reals as `%.3f`, NULL as `NULL`, the
/// empty text as `(empty)`, and every character outside printable ASCII as `@`.
pub fn render(cell: &Cell, ty: char) -> String {
    match (cell, ty) {
        (Cell::Null, _) => "NULL".to_string(),
        (Cell::Int(i), 'I') => i.to_string(),
        (Cell::Float(f), 'I') => (f.trunc() as i64).to_string(),
        (Cell::Text(s), 'I') => match s.trim() {
            "t" | "true" => "1".to_string(),
            "f" | "false" => "0".to_string(),
            t => match t.parse::<f64>() {
                Ok(f) if t.contains(['.', 'e', 'E']) => (f.trunc() as i64).to_string(),
                _ => leading_integer(t).to_string(),
            },
        },
        (Cell::Int(i), 'R') => format!("{:.3}", *i as f64),
        (Cell::Float(f), 'R') => format!("{f:.3}"),
        (Cell::Text(s), 'R') => format!("{:.3}", text_as_float(s)),
        (Cell::Int(i), _) => i.to_string(),
        (Cell::Float(f), _) => f.to_string(),
        (Cell::Text(s), _) => {
            if s.is_empty() {
                "(empty)".to_string()
            } else {
                s.chars()
                    .map(|c| if (' '..='~').contains(&c) { c } else { '@' })
                    .collect()
            }
        }
    }
}

/// The values of a result in the order the script compares them. `Err` when a row
/// does not have one value per type character.
pub fn render_rows(rows: &[Vec<Cell>], types: &str, sort: SortMode) -> Result<Vec<String>, String> {
    let width = types.chars().count();
    let mut out: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != width {
            return Err(format!("{} columns returned, {} expected", row.len(), width));
        }
        out.push(row.iter().zip(types.chars()).map(|(c, t)| render(c, t)).collect());
    }
    match sort {
        SortMode::NoSort => Ok(out.into_iter().flatten().collect()),
        SortMode::RowSort => {
            out.sort_by(|a, b| a.iter().map(|s| s.as_bytes()).cmp(b.iter().map(|s| s.as_bytes())));
            Ok(out.into_iter().flatten().collect())
        }
        SortMode::ValueSort => {
            let mut flat: Vec<String> = out.into_iter().flatten().collect();
            flat.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            Ok(flat)
        }
    }
}

/// The digest line of a result: every value followed by a newline, MD5, lower hex.
pub fn hash_values(values: &[String]) -> String {
    let mut ctx = md5::Context::new();
    for v in values {
        ctx.consume(v.as_bytes());
        ctx.consume(b"\n");
    }
    format!("{:x}", ctx.compute())
}

/// Why a query's result does not match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mismatch {
    ColumnCount(String),
    ValueCount {
        expected: usize,
        got: usize,
    },
    Values {
        index: usize,
        expected: String,
        got: String,
    },
    Hash {
        expected: String,
        got: String,
    },
    Label {
        label: String,
    },
}

/// Compare a rendered result with what the record expects. `labels` carries the digest
/// of every labelled result seen so far in the file: two queries under one label must
/// return the same values.
pub fn compare(
    values: &[String],
    expected: &[String],
    label: Option<&str>,
    hash_threshold: usize,
    labels: &mut HashMap<String, String>,
) -> Result<(), Mismatch> {
    let hashed = hash_threshold > 0 && values.len() > hash_threshold;
    let digest = if hashed || label.is_some() {
        Some(hash_values(values))
    } else {
        None
    };
    if hashed {
        let got = format!(
            "{} values hashing to {}",
            values.len(),
            digest.clone().unwrap_or_default()
        );
        let want = expected.first().map(|s| s.trim_end()).unwrap_or("");
        if want != got {
            let expected_count = want.split_whitespace().next().and_then(|n| n.parse::<usize>().ok());
            return Err(match expected_count {
                Some(n) if n != values.len() && want.contains("values hashing to") => Mismatch::ValueCount {
                    expected: n,
                    got: values.len(),
                },
                _ if !want.contains("values hashing to") => Mismatch::ValueCount {
                    expected: expected.len(),
                    got: values.len(),
                },
                _ => Mismatch::Hash {
                    expected: want.to_string(),
                    got,
                },
            });
        }
    } else {
        if expected.len() == 1 && expected.first().is_some_and(|l| l.contains(" values hashing to ")) {
            // The script holds a digest for a larger result than the one returned.
            let want = expected.first().cloned().unwrap_or_default();
            let n = want.split_whitespace().next().and_then(|n| n.parse::<usize>().ok());
            if n != Some(values.len()) {
                return Err(Mismatch::ValueCount {
                    expected: n.unwrap_or(0),
                    got: values.len(),
                });
            }
        }
        if expected.len() != values.len() {
            return Err(Mismatch::ValueCount {
                expected: expected.len(),
                got: values.len(),
            });
        }
        for (index, (want, got)) in expected.iter().zip(values).enumerate() {
            if want.trim_end() != got.trim_end() {
                return Err(Mismatch::Values {
                    index,
                    expected: want.clone(),
                    got: got.clone(),
                });
            }
        }
    }
    if let (Some(label), Some(digest)) = (label, digest) {
        match labels.get(label) {
            Some(previous) if *previous != digest => {
                return Err(Mismatch::Label {
                    label: label.to_string(),
                })
            }
            Some(_) => {}
            None => {
                labels.insert(label.to_string(), digest);
            }
        }
    }
    Ok(())
}
