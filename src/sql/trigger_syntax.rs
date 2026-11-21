//! Trigger statements the SQL grammar in use cannot take as written.
//!
//! [`preprocess`] runs on the statement text before it is parsed and rewrites
//! four things into text the grammar accepts, without losing anything:
//!
//! * `CREATE TRIGGER … EXECUTE FUNCTION f('a', 'b')`: the grammar reads the
//!   parenthesis as a parameter declaration list, so literal arguments do not
//!   parse. The function name, the arguments, `OR REPLACE` and `IF NOT
//!   EXISTS` are folded into one quoted name ([`decode_target`] unfolds it).
//! * `CREATE TRIGGER … [FOR EACH ROW] [WHEN …] BEGIN … END` and the MySQL
//!   single-statement body: the inline body is translated to a block the
//!   procedural interpreter runs ([`translate_inline_body`]) and folded into
//!   the same quoted name, as a call of
//!   [`INLINE_TRIGGER_FUNCTION`](super::trigger_exec::INLINE_TRIGGER_FUNCTION).
//! * `DROP TRIGGER name` without `ON table` (MySQL, SQLite): the table is
//!   looked up by the trigger's name ([`ANY_TABLE`]).
//! * `ALTER TRIGGER … RENAME TO …` and `ALTER TABLE … ENABLE | DISABLE
//!   TRIGGER …`: planned as a call of
//!   [`TRIGGER_ADMIN_PROCEDURE`](super::trigger_exec::TRIGGER_ADMIN_PROCEDURE).
//!
//! Anything this module does not recognise is returned unchanged, so the
//! parser reports it as it always did.

use super::trigger_exec::{quoted_span, INLINE_TRIGGER_FUNCTION, TRIGGER_ADMIN_PROCEDURE, TRIGGER_UNSUPPORTED_MARKER};
use crate::{Error, Result};
use std::borrow::Cow;

/// The table name `DROP TRIGGER name` (no `ON`) is planned with.
pub(crate) const ANY_TABLE: &str = "__hdb_trigger_any_table";

const TARGET_PREFIX: &str = "__hdb_trg_z";

/// The name `DROP FUNCTION f CASCADE` is planned under (the plan carries the
/// name only): see [`decode_drop_function`].
const DROP_CASCADE_PREFIX: &str = "__hdb_dropfn_z";

/// `(function name, CASCADE given)` of a `DROP FUNCTION` plan's name.
pub(crate) fn decode_drop_function(name: &str) -> (String, bool) {
    match name.strip_prefix(DROP_CASCADE_PREFIX).and_then(unhex) {
        Some(decoded) => (decoded, true),
        None => (name.to_string(), false),
    }
}

/// `DROP FUNCTION [IF EXISTS] f [(…)] CASCADE`, one function.
fn drop_function_cascade(sql: &str, toks: &[Tok<'_>]) -> Option<String> {
    if !toks.get(1).is_some_and(|t| t.is_word("FUNCTION")) {
        return None;
    }
    let last = toks.iter().rev().find(|t| !t.is_punct(';'))?;
    if !last.is_word("CASCADE") {
        return None;
    }
    let mut k = 2;
    if toks.get(k).is_some_and(|t| t.is_word("IF")) && toks.get(k + 1).is_some_and(|t| t.is_word("EXISTS")) {
        k += 2;
    }
    let first = toks.get(k).filter(|t| t.is_name())?;
    let mut name = first;
    k += 1;
    while toks.get(k).is_some_and(|t| t.is_punct('.')) {
        name = toks.get(k + 1).filter(|t| t.is_name())?;
        k += 2;
    }
    if toks.iter().skip(k).any(|t| t.is_punct(',')) && !toks.get(k).is_some_and(|t| t.is_punct('(')) {
        return None;
    }
    Some(format!(
        "{}\"{DROP_CASCADE_PREFIX}{}\"{}",
        sql.get(..first.start)?,
        hex(&name_of(name)),
        sql.get(name.end..)?
    ))
}

/// What `CREATE TRIGGER` runs, unfolded from the planned function name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TriggerTarget {
    pub function: String,
    pub args: Vec<String>,
    pub or_replace: bool,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Quoted,
    Punct,
}

#[derive(Debug, Clone, Copy)]
struct Tok<'a> {
    kind: Kind,
    text: &'a str,
    start: usize,
    end: usize,
}

impl Tok<'_> {
    fn is_word(&self, word: &str) -> bool {
        self.kind == Kind::Word && self.text.eq_ignore_ascii_case(word)
    }
    fn is_punct(&self, c: char) -> bool {
        self.kind == Kind::Punct && self.text.starts_with(c)
    }
    fn is_name(&self) -> bool {
        self.kind == Kind::Word
            || (self.kind == Kind::Quoted && (self.text.starts_with('"') || self.text.starts_with('`')))
    }
}

fn tokenize(sql: &str) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(rest) = sql.get(at..) {
        let Some(c) = rest.chars().next() else { break };
        if c.is_whitespace() {
            at += c.len_utf8();
            continue;
        }
        if let Some(len) = quoted_span(rest) {
            if !(rest.starts_with("--") || rest.starts_with("/*")) {
                out.push(Tok {
                    kind: Kind::Quoted,
                    text: rest.get(..len).unwrap_or(rest),
                    start: at,
                    end: at + len,
                });
            }
            at += len;
            continue;
        }
        if c.is_alphanumeric() || c == '_' || c == '@' {
            let len = rest
                .char_indices()
                .find(|&(_, ch)| !(ch.is_alphanumeric() || ch == '_' || ch == '@' || ch == '$'))
                .map_or(rest.len(), |(i, _)| i);
            out.push(Tok {
                kind: Kind::Word,
                text: rest.get(..len).unwrap_or(rest),
                start: at,
                end: at + len,
            });
            at += len;
            continue;
        }
        out.push(Tok {
            kind: Kind::Punct,
            text: rest.get(..c.len_utf8()).unwrap_or(rest),
            start: at,
            end: at + c.len_utf8(),
        });
        at += c.len_utf8();
    }
    out
}

fn unsupported(what: &str) -> Error {
    Error::query_execution(format!("{what}: this {TRIGGER_UNSUPPORTED_MARKER}"))
}

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<String> {
    if text.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let mut at = 0;
    while at < text.len() {
        bytes.push(u8::from_str_radix(text.get(at..at + 2)?, 16).ok()?);
        at += 2;
    }
    String::from_utf8(bytes).ok()
}

fn encode_target(target: &TriggerTarget) -> String {
    let mut flags = String::new();
    if target.or_replace {
        flags.push('r');
    }
    if target.if_not_exists {
        flags.push('n');
    }
    let mut out = format!("\"{TARGET_PREFIX}{flags}z{}", hex(&target.function));
    for arg in &target.args {
        out.push('z');
        out.push_str(&hex(arg));
    }
    out.push('"');
    out
}

/// Unfold a function name written by [`preprocess`]. `None` for a plain name.
pub(crate) fn decode_target(name: &str) -> Option<TriggerTarget> {
    let rest = name.strip_prefix(TARGET_PREFIX)?;
    let mut parts = rest.split('z');
    let flags = parts.next()?;
    let function = unhex(parts.next()?)?;
    let args = parts.map(unhex).collect::<Option<Vec<_>>>()?;
    Some(TriggerTarget {
        function,
        args,
        or_replace: flags.contains('r'),
        if_not_exists: flags.contains('n'),
    })
}

/// An identifier token as a name: quotes removed, an unquoted name lower-cased.
fn name_of(tok: &Tok<'_>) -> String {
    let text = tok.text;
    match text.chars().next() {
        Some(q @ ('"' | '`')) if tok.kind == Kind::Quoted => {
            let inner = text.get(1..text.len().saturating_sub(1)).unwrap_or("");
            inner.replace(&format!("{q}{q}"), &q.to_string())
        }
        _ => text.to_lowercase(),
    }
}

/// A literal token as `TG_ARGV` text.
fn literal_text(tok: &Tok<'_>) -> String {
    if tok.kind == Kind::Quoted && tok.text.starts_with('\'') {
        let inner = tok.text.get(1..tok.text.len().saturating_sub(1)).unwrap_or("");
        return inner.replace("''", "'");
    }
    tok.text.to_string()
}

fn sql_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Is `sql` a `CREATE TRIGGER` whose body is written inline (`BEGIN … END` or
/// a single MySQL statement)? Such a statement holds `;` that do not end it,
/// so a statement splitter must hand it on whole.
pub fn is_inline_trigger(sql: &str) -> bool {
    let head = sql.trim_start();
    if !head.get(..6).is_some_and(|h| h.eq_ignore_ascii_case("CREATE"))
        || !sql.as_bytes().windows(7).any(|w| w.eq_ignore_ascii_case(b"TRIGGER"))
    {
        return false;
    }
    let toks = tokenize(sql);
    match create_trigger_shape(&toks) {
        Ok(Some(shape)) => shape.body.is_some(),
        _ => false,
    }
}

struct Shape {
    or_replace: bool,
    if_not_exists: Option<(usize, usize)>,
    /// Index one past the table name.
    after_table: usize,
    level: Option<&'static str>,
    when: Option<(usize, usize)>,
    /// Token index the inline body starts at.
    body: Option<usize>,
    /// Token index of `EXECUTE`.
    execute: Option<usize>,
}

fn create_trigger_shape(toks: &[Tok<'_>]) -> Result<Option<Shape>> {
    let mut i = 0;
    if !toks.first().is_some_and(|t| t.is_word("CREATE")) {
        return Ok(None);
    }
    i += 1;
    let mut or_replace = false;
    if toks.get(i).is_some_and(|t| t.is_word("OR")) && toks.get(i + 1).is_some_and(|t| t.is_word("REPLACE")) {
        or_replace = true;
        i += 2;
    }
    if toks.get(i).is_some_and(|t| t.is_word("TEMP") || t.is_word("TEMPORARY"))
        && toks.get(i + 1).is_some_and(|t| t.is_word("TRIGGER"))
    {
        return Err(unsupported("CREATE TEMPORARY TRIGGER"));
    }
    if toks.get(i).is_some_and(|t| t.is_word("CONSTRAINT")) {
        i += 1;
    }
    if !toks.get(i).is_some_and(|t| t.is_word("TRIGGER")) {
        return Ok(None);
    }
    i += 1;
    let mut if_not_exists = None;
    if toks.get(i).is_some_and(|t| t.is_word("IF"))
        && toks.get(i + 1).is_some_and(|t| t.is_word("NOT"))
        && toks.get(i + 2).is_some_and(|t| t.is_word("EXISTS"))
    {
        let start = toks.get(i).map_or(0, |t| t.start);
        let end = toks.get(i + 2).map_or(0, |t| t.end);
        if_not_exists = Some((start, end));
        i += 3;
    }
    // The table: the first ON after the name, then `name[.name]`.
    let Some(on) = toks
        .iter()
        .enumerate()
        .skip(i + 1)
        .find(|(_, t)| t.is_word("ON"))
        .map(|(k, _)| k)
    else {
        return Ok(None);
    };
    let mut k = on + 1;
    if !toks.get(k).is_some_and(Tok::is_name) {
        return Ok(None);
    }
    k += 1;
    while toks.get(k).is_some_and(|t| t.is_punct('.')) && toks.get(k + 1).is_some_and(Tok::is_name) {
        k += 2;
    }
    let after_table = k;
    let mut shape = Shape {
        or_replace,
        if_not_exists,
        after_table,
        level: None,
        when: None,
        body: None,
        execute: None,
    };
    const BODY_START: [&str; 11] = [
        "BEGIN", "SET", "INSERT", "UPDATE", "DELETE", "REPLACE", "SIGNAL", "IF", "CALL", "SELECT", "WITH",
    ];
    while let Some(tok) = toks.get(k) {
        if tok.is_word("EXECUTE") {
            shape.execute = Some(k);
            return Ok(Some(shape));
        }
        if tok.is_word("FOR") {
            let mut n = k + 1;
            if toks.get(n).is_some_and(|t| t.is_word("EACH")) {
                n += 1;
            }
            if toks.get(n).is_some_and(|t| t.is_word("ROW")) {
                shape.level = Some("ROW");
            } else if toks.get(n).is_some_and(|t| t.is_word("STATEMENT")) {
                shape.level = Some("STATEMENT");
            } else {
                return Ok(None);
            }
            k = n + 1;
            continue;
        }
        if tok.is_word("WHEN") {
            if toks.get(k + 1).is_some_and(|t| t.is_punct('(')) {
                let mut depth = 0usize;
                let mut n = k + 1;
                let mut close = None;
                while let Some(t) = toks.get(n) {
                    if t.is_punct('(') {
                        depth += 1;
                    } else if t.is_punct(')') {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            close = Some(n);
                            break;
                        }
                    }
                    n += 1;
                }
                let Some(close) = close else { return Ok(None) };
                // `WHEN (a) AND (b) BEGIN` (SQLite, no outer parentheses).
                if toks.get(close + 1).is_some_and(|t| t.is_word("AND") || t.is_word("OR")) {
                    let Some(begin) = toks
                        .iter()
                        .enumerate()
                        .skip(k)
                        .find(|(_, t)| t.is_word("BEGIN"))
                        .map(|(b, _)| b)
                    else {
                        return Ok(None);
                    };
                    shape.when = Some((k + 1, begin));
                    k = begin;
                } else {
                    shape.when = Some((k + 2, close));
                    k = close + 1;
                }
            } else {
                let Some(begin) = toks
                    .iter()
                    .enumerate()
                    .skip(k)
                    .find(|(_, t)| t.is_word("BEGIN"))
                    .map(|(b, _)| b)
                else {
                    return Ok(None);
                };
                shape.when = Some((k + 1, begin));
                k = begin;
            }
            continue;
        }
        if tok.is_word("FOLLOWS") || tok.is_word("PRECEDES") {
            return Err(unsupported(&format!(
                "CREATE TRIGGER … {} (triggers fire in name order)",
                tok.text.to_uppercase()
            )));
        }
        if BODY_START.iter().any(|w| tok.is_word(w)) {
            shape.body = Some(k);
            return Ok(Some(shape));
        }
        k += 1;
    }
    Ok(None)
}

/// Rewrite the trigger statements described in the module documentation.
pub(crate) fn preprocess(sql: &str) -> Result<Cow<'_, str>> {
    let head = sql.trim_start();
    let first = head.get(..6).unwrap_or(head);
    let starts =
        |word: &str| first.len() >= word.len() && first.get(..word.len()).is_some_and(|h| h.eq_ignore_ascii_case(word));
    if !(starts("CREATE") || starts("ALTER") || starts("DROP")) {
        return Ok(Cow::Borrowed(sql));
    }
    let mentions = |word: &[u8]| sql.as_bytes().windows(word.len()).any(|w| w.eq_ignore_ascii_case(word));
    if !mentions(b"TRIGGER") {
        if starts("DROP") && mentions(b"CASCADE") {
            let toks = tokenize(sql);
            return Ok(drop_function_cascade(sql, &toks).map_or(Cow::Borrowed(sql), Cow::Owned));
        }
        return Ok(Cow::Borrowed(sql));
    }
    let toks = tokenize(sql);
    if toks.first().is_some_and(|t| t.is_word("ALTER")) {
        return Ok(alter(&toks)?.map_or(Cow::Borrowed(sql), Cow::Owned));
    }
    if toks.first().is_some_and(|t| t.is_word("DROP")) {
        return Ok(drop_without_table(sql, &toks).map_or(Cow::Borrowed(sql), Cow::Owned));
    }
    let Some(shape) = create_trigger_shape(&toks)? else {
        return Ok(Cow::Borrowed(sql));
    };
    let text = |from: usize, to: usize| {
        let start = toks.get(from).map_or(sql.len(), |t| t.start);
        let end = toks.get(to).map_or(sql.len(), |t| t.start);
        sql.get(start..end).unwrap_or("")
    };
    // The statement up to a byte offset, without IF NOT EXISTS and with
    // backtick names as standard quoted names.
    let head_until = |end: usize| -> String {
        let mut head = String::new();
        match shape.if_not_exists {
            Some((a, b)) => {
                head.push_str(sql.get(..a).unwrap_or(""));
                head.push_str(sql.get(b..end).unwrap_or(""));
            }
            None => head.push_str(sql.get(..end).unwrap_or("")),
        }
        head
    };
    if let Some(execute) = shape.execute {
        // EXECUTE FUNCTION|PROCEDURE name[.name] ( literal, … )
        let mut k = execute + 1;
        if !toks
            .get(k)
            .is_some_and(|t| t.is_word("FUNCTION") || t.is_word("PROCEDURE"))
        {
            return Ok(Cow::Borrowed(sql));
        }
        k += 1;
        let Some(mut name_tok) = toks.get(k).filter(|t| t.is_name()) else {
            return Ok(Cow::Borrowed(sql));
        };
        k += 1;
        while toks.get(k).is_some_and(|t| t.is_punct('.')) {
            let Some(next) = toks.get(k + 1).filter(|t| t.is_name()) else {
                return Ok(Cow::Borrowed(sql));
            };
            name_tok = next;
            k += 2;
        }
        let mut args = Vec::new();
        if toks.get(k).is_some_and(|t| t.is_punct('(')) {
            k += 1;
            loop {
                let Some(tok) = toks.get(k) else {
                    return Ok(Cow::Borrowed(sql));
                };
                if tok.is_punct(')') {
                    k += 1;
                    break;
                }
                if tok.is_punct(',') {
                    k += 1;
                    continue;
                }
                let mut arg = literal_text(tok);
                // A signed number: `-1`.
                if tok.is_punct('-') || tok.is_punct('+') {
                    if let Some(next) = toks.get(k + 1) {
                        arg.push_str(next.text);
                        k += 1;
                    }
                }
                args.push(arg);
                k += 1;
            }
        }
        if toks.get(k).is_some_and(|t| !t.is_punct(';')) {
            return Ok(Cow::Borrowed(sql));
        }
        let target = TriggerTarget {
            function: name_of(name_tok),
            args,
            or_replace: shape.or_replace,
            if_not_exists: shape.if_not_exists.is_some(),
        };
        let start = toks.get(execute).map_or(sql.len(), |t| t.start);
        return Ok(Cow::Owned(format!(
            "{}EXECUTE FUNCTION {}()",
            head_until(start),
            encode_target(&target)
        )));
    }
    let Some(body) = shape.body else {
        return Ok(Cow::Borrowed(sql));
    };
    let source = text(body, usize::MAX).trim().trim_end_matches(';').trim().to_string();
    let block = translate_inline_body(&source)?;
    let dialect = if shape.level.is_none() || source.to_ascii_uppercase().contains("RAISE") {
        "sqlite"
    } else {
        "mysql"
    };
    let target = TriggerTarget {
        function: INLINE_TRIGGER_FUNCTION.to_string(),
        args: vec![dialect.to_string(), source, block],
        or_replace: shape.or_replace,
        if_not_exists: shape.if_not_exists.is_some(),
    };
    let header_end = toks
        .get(shape.after_table.saturating_sub(1))
        .map_or(sql.len(), |t| t.end);
    let mut out = head_until(header_end).replace('`', "\"");
    out.push_str(" FOR EACH ");
    out.push_str(shape.level.unwrap_or("ROW"));
    if let Some((from, to)) = shape.when {
        out.push_str(" WHEN (");
        out.push_str(text(from, to).trim());
        out.push(')');
    }
    out.push_str(" EXECUTE FUNCTION ");
    out.push_str(&encode_target(&target));
    out.push_str("()");
    Ok(Cow::Owned(out))
}

/// `DROP TRIGGER [IF EXISTS] name` without `ON table`.
fn drop_without_table(sql: &str, toks: &[Tok<'_>]) -> Option<String> {
    if !toks.get(1).is_some_and(|t| t.is_word("TRIGGER")) {
        return None;
    }
    if toks.iter().any(|t| t.is_word("ON")) {
        return None;
    }
    let last = toks.iter().rev().find(|t| !t.is_punct(';'))?;
    if !last.is_name() {
        return None;
    }
    let head = sql.get(..last.end)?.replace('`', "\"");
    Some(format!("{head} ON {ANY_TABLE}"))
}

/// `ALTER TRIGGER … RENAME TO …`, `ALTER TABLE … ENABLE | DISABLE TRIGGER …`.
fn alter(toks: &[Tok<'_>]) -> Result<Option<String>> {
    let call = |action: &str, table: &str, name: &str, to: &str| {
        format!(
            "CALL {TRIGGER_ADMIN_PROCEDURE}({}, {}, {}, {})",
            sql_string(action),
            sql_string(table),
            sql_string(name),
            sql_string(to)
        )
    };
    // A possibly qualified name starting at `k`: (dotted text, next index).
    let qualified = |mut k: usize| -> Option<(String, usize)> {
        let mut name = name_of(toks.get(k).filter(|t| t.is_name())?);
        k += 1;
        while toks.get(k).is_some_and(|t| t.is_punct('.')) {
            let next = toks.get(k + 1).filter(|t| t.is_name())?;
            name.push('.');
            name.push_str(&name_of(next));
            k += 2;
        }
        Some((name, k))
    };
    let at_end = |k: usize| match toks.get(k) {
        None => true,
        Some(t) => t.is_punct(';') && toks.get(k + 1).is_none(),
    };
    if toks.get(1).is_some_and(|t| t.is_word("TRIGGER")) {
        // ALTER TRIGGER name ON table RENAME TO new_name
        let Some(name) = toks.get(2).filter(|t| t.is_name()) else {
            return Ok(None);
        };
        if !toks.get(3).is_some_and(|t| t.is_word("ON")) {
            return Ok(None);
        }
        let Some((table, k)) = qualified(4) else {
            return Ok(None);
        };
        if toks.get(k).is_some_and(|t| t.is_word("RENAME")) && toks.get(k + 1).is_some_and(|t| t.is_word("TO")) {
            if let Some(to) = toks.get(k + 2).filter(|t| t.is_name()) {
                if at_end(k + 3) {
                    return Ok(Some(call("rename", &table, &name_of(name), &name_of(to))));
                }
            }
            return Ok(None);
        }
        let what = toks.get(k).map_or(String::new(), |t| t.text.to_uppercase());
        return Err(unsupported(&format!("ALTER TRIGGER … {what}")));
    }
    if !toks.get(1).is_some_and(|t| t.is_word("TABLE")) {
        return Ok(None);
    }
    let mut k = 2;
    if toks.get(k).is_some_and(|t| t.is_word("IF")) && toks.get(k + 1).is_some_and(|t| t.is_word("EXISTS")) {
        k += 2;
    }
    if toks.get(k).is_some_and(|t| t.is_word("ONLY")) {
        k += 1;
    }
    let Some((table, mut k)) = qualified(k) else {
        return Ok(None);
    };
    let action = match toks.get(k) {
        Some(t) if t.is_word("ENABLE") => "enable",
        Some(t) if t.is_word("DISABLE") => "disable",
        _ => return Ok(None),
    };
    k += 1;
    if toks.get(k).is_some_and(|t| t.is_word("ALWAYS")) && toks.get(k + 1).is_some_and(|t| t.is_word("TRIGGER")) {
        k += 1;
    } else if toks.get(k).is_some_and(|t| t.is_word("REPLICA")) && toks.get(k + 1).is_some_and(|t| t.is_word("TRIGGER"))
    {
        return Err(unsupported("ALTER TABLE … ENABLE REPLICA TRIGGER"));
    }
    if !toks.get(k).is_some_and(|t| t.is_word("TRIGGER")) {
        return Ok(None);
    }
    let Some(name) = toks.get(k + 1).filter(|t| t.is_name()) else {
        return Ok(None);
    };
    if !at_end(k + 2) {
        return Ok(None);
    }
    let name = if name.is_word("ALL") || name.is_word("USER") {
        "*".to_string()
    } else {
        name_of(name)
    };
    Ok(Some(call(action, &table, &name, "")))
}

/// Split `text` at each top-level `;` (outside quotes and parentheses).
fn split_statements(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut at = 0usize;
    while let Some(rest) = text.get(at..) {
        let Some(c) = rest.chars().next() else { break };
        if let Some(len) = quoted_span(rest) {
            at += len;
            continue;
        }
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                out.push(text.get(start..at).unwrap_or(""));
                start = at + 1;
            }
            _ => {}
        }
        at += c.len_utf8();
    }
    out.push(text.get(start..).unwrap_or(""));
    out
}

/// The byte offset of the first top-level occurrence of the word `word`.
fn find_word(text: &str, word: &str) -> Option<usize> {
    let mut depth = 0usize;
    tokenize(text).into_iter().find_map(|t| {
        if t.is_punct('(') {
            depth += 1;
        } else if t.is_punct(')') {
            depth = depth.saturating_sub(1);
        }
        (depth == 0 && t.is_word(word)).then_some(t.start)
    })
}

fn leading_word(text: &str) -> String {
    tokenize(text)
        .first()
        .filter(|t| t.kind == Kind::Word)
        .map_or(String::new(), |t| t.text.to_uppercase())
}

/// Translate an inline trigger body (MySQL or SQLite form) into a block the
/// procedural interpreter runs.
///
/// Runs as written: `INSERT`, `UPDATE`, `DELETE`, `REPLACE` and `SELECT`
/// statements using `NEW.col` / `OLD.col`; `IF … THEN … [ELSEIF … THEN …]
/// [ELSE …] END IF`; MySQL `SET NEW.col = expr[, NEW.col = expr]` and
/// `SIGNAL SQLSTATE 'xxxxx' [SET MESSAGE_TEXT = expr]`; SQLite `SELECT
/// RAISE(ABORT, 'message')` and `SELECT RAISE(IGNORE)`, each alone or with a
/// `WHERE` or `FROM … WHERE` that decides whether it applies. Everything else
/// is refused here, at CREATE TRIGGER, by name.
pub(crate) fn translate_inline_body(source: &str) -> Result<String> {
    let mut body = source.trim().trim_end_matches(';').trim();
    let toks = tokenize(body);
    if toks.first().is_some_and(|t| t.is_word("BEGIN")) {
        let Some(end) = toks.last().filter(|t| t.is_word("END")) else {
            return Err(unsupported(
                "a trigger body that starts with BEGIN and does not end with END",
            ));
        };
        let start = toks.first().map_or(0, |t| t.end);
        body = body.get(start..end.start).unwrap_or("");
    }
    let mut out = String::from("BEGIN\n");
    for piece in split_statements(body) {
        let translated = translate_piece(piece.trim())?;
        if !translated.is_empty() {
            out.push_str(&translated);
            out.push_str(";\n");
        }
    }
    out.push_str("END");
    Ok(out)
}

fn translate_piece(piece: &str) -> Result<String> {
    if piece.is_empty() {
        return Ok(String::new());
    }
    let toks = tokenize(piece);
    let word = leading_word(piece);
    let after_first = toks.first().and_then(|t| piece.get(t.end..)).unwrap_or("").trim();
    match word.as_str() {
        "IF" | "ELSEIF" | "ELSIF" => {
            let Some(then) = find_word(piece, "THEN") else {
                return Err(unsupported(&format!("{word} without THEN in a trigger body")));
            };
            let cond_start = toks.first().map_or(0, |t| t.end);
            let cond = piece.get(cond_start..then).unwrap_or("").trim();
            let rest = piece.get(then + 4..).unwrap_or("").trim();
            let keyword = if word == "IF" { "IF" } else { "ELSIF" };
            Ok(format!("{keyword} {cond} THEN\n{}", translate_piece(rest)?))
        }
        "ELSE" => Ok(format!("ELSE\n{}", translate_piece(after_first)?)),
        "END" => {
            if toks.get(1).is_some_and(|t| t.is_word("IF")) && toks.len() == 2 {
                // The piece before `END IF` ended with `;`, which the block needs.
                Ok("END IF".to_string())
            } else {
                Err(unsupported(&format!("\"{piece}\" in a trigger body")))
            }
        }
        "SET" => {
            let mut assignments = Vec::new();
            for item in split_commas(after_first) {
                let item = item.trim();
                let is_new = item.get(..4).is_some_and(|h| h.eq_ignore_ascii_case("NEW."));
                let Some((target, expr)) = item.split_once('=').filter(|_| is_new) else {
                    return Err(unsupported(&format!(
                        "SET of anything but NEW.<column> in a trigger body (\"{item}\")"
                    )));
                };
                assignments.push(format!("{} := {}", target.trim().replace('`', "\""), expr.trim()));
            }
            Ok(assignments.join(";\n"))
        }
        "SIGNAL" => {
            let mut k = 1;
            if !toks.get(k).is_some_and(|t| t.is_word("SQLSTATE")) {
                return Err(unsupported("SIGNAL of a named condition in a trigger body"));
            }
            k += 1;
            if toks.get(k).is_some_and(|t| t.is_word("VALUE")) {
                k += 1;
            }
            let Some(code) = toks
                .get(k)
                .filter(|t| t.kind == Kind::Quoted && t.text.starts_with('\''))
            else {
                return Err(unsupported("SIGNAL SQLSTATE without a quoted code in a trigger body"));
            };
            let rest = piece.get(code.end..).unwrap_or("").trim();
            let message = if rest.is_empty() {
                "'Unhandled user-defined exception condition'".to_string()
            } else {
                let items = rest
                    .get(..3)
                    .filter(|h| h.eq_ignore_ascii_case("SET"))
                    .and_then(|_| rest.get(3..));
                let Some(items) = items else {
                    return Err(unsupported(&format!("SIGNAL … {rest} in a trigger body")));
                };
                let mut message = None;
                for item in split_commas(items) {
                    match item.split_once('=') {
                        Some((name, expr)) if name.trim().eq_ignore_ascii_case("MESSAGE_TEXT") => {
                            message = Some(expr.trim().to_string());
                        }
                        _ => {
                            return Err(unsupported(&format!(
                                "SIGNAL … SET {} in a trigger body (only MESSAGE_TEXT is taken)",
                                item.trim()
                            )))
                        }
                    }
                }
                message.unwrap_or_else(|| "'Unhandled user-defined exception condition'".to_string())
            };
            Ok(format!("RAISE EXCEPTION SQLSTATE {} '%', {message}", code.text))
        }
        "SELECT" if toks.get(1).is_some_and(|t| t.is_word("RAISE")) && toks.get(2).is_some_and(|t| t.is_punct('(')) => {
            let Some(close) = toks.iter().position(|t| t.is_punct(')')) else {
                return Err(unsupported("RAISE( without a closing parenthesis in a trigger body"));
            };
            let kind = toks.get(3).map_or(String::new(), |t| t.text.to_uppercase());
            let action = match kind.as_str() {
                "IGNORE" => "RETURN NULL".to_string(),
                "ABORT" => {
                    let message = toks
                        .get(5)
                        .filter(|t| {
                            t.kind == Kind::Quoted && toks.get(4).is_some_and(|c| c.is_punct(',')) && close == 6
                        })
                        .map(|t| t.text);
                    let Some(message) = message else {
                        return Err(unsupported(
                            "RAISE(ABORT, …) without a string message in a trigger body",
                        ));
                    };
                    format!("RAISE EXCEPTION '%', {message}")
                }
                other => {
                    return Err(unsupported(&format!(
                        "RAISE({other}, …) in a trigger body (ABORT and IGNORE are taken)"
                    )))
                }
            };
            let rest = toks.get(close).and_then(|t| piece.get(t.end..)).unwrap_or("").trim();
            let rest_word = leading_word(rest);
            if rest.is_empty() {
                Ok(action)
            } else if rest_word == "WHERE" {
                Ok(format!(
                    "IF ({}) THEN\n{action};\nEND IF",
                    rest.get(5..).unwrap_or("").trim()
                ))
            } else if rest_word == "FROM" {
                Ok(format!("IF EXISTS (SELECT 1 {rest}) THEN\n{action};\nEND IF"))
            } else {
                Err(unsupported(&format!("SELECT RAISE(…) {rest} in a trigger body")))
            }
        }
        "DECLARE" | "WHILE" | "LOOP" | "REPEAT" | "CASE" | "LEAVE" | "ITERATE" | "RETURN" | "OPEN" | "FETCH"
        | "CLOSE" | "BEGIN" | "CALL" | "RAISE" => Err(unsupported(&format!("{word} in a trigger body"))),
        _ => {
            let mut previous: Option<&Tok<'_>> = None;
            for tok in &toks {
                if tok.is_punct('(') && previous.is_some_and(|p| p.is_word("RAISE")) {
                    return Err(unsupported(
                        "RAISE(…) inside an expression in a trigger body (write SELECT RAISE(ABORT, '…') WHERE …)",
                    ));
                }
                if tok.kind == Kind::Word && tok.text.starts_with('@') {
                    return Err(unsupported(&format!(
                        "the user variable {} in a trigger body",
                        tok.text
                    )));
                }
                previous = Some(tok);
            }
            Ok(piece.to_string())
        }
    }
}

/// Split at top-level commas.
fn split_commas(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for tok in tokenize(text) {
        if tok.is_punct('(') {
            depth += 1;
        } else if tok.is_punct(')') {
            depth = depth.saturating_sub(1);
        } else if tok.is_punct(',') && depth == 0 {
            out.push(text.get(start..tok.start).unwrap_or(""));
            start = tok.end;
        }
    }
    out.push(text.get(start..).unwrap_or(""));
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn target_of(sql: &str) -> (String, TriggerTarget) {
        let out = preprocess(sql).unwrap().into_owned();
        let start = out.find(TARGET_PREFIX).expect("an encoded target");
        let end = out[start..].find('"').unwrap() + start;
        (out[..start - 1].to_string(), decode_target(&out[start..end]).unwrap())
    }

    #[test]
    fn other_statements_are_untouched() {
        for sql in [
            "SELECT 1",
            "CREATE TABLE trigger_log (id INT)",
            "ALTER TABLE t ADD COLUMN trigger_count INT",
            "DROP TRIGGER trg ON t",
        ] {
            assert!(matches!(preprocess(sql).unwrap(), Cow::Borrowed(_)), "{sql}");
        }
    }

    #[test]
    fn execute_function_arguments_and_flags_are_folded_into_the_name() {
        let (head, target) = target_of(
            "CREATE OR REPLACE TRIGGER trg AFTER INSERT OR UPDATE OF a ON s.t FOR EACH ROW WHEN (NEW.a > 0) \
             EXECUTE FUNCTION Public.Audit('it''s', 7, -1);",
        );
        assert_eq!(
            head,
            "CREATE OR REPLACE TRIGGER trg AFTER INSERT OR UPDATE OF a ON s.t FOR EACH ROW WHEN (NEW.a > 0) \
             EXECUTE FUNCTION "
        );
        assert_eq!(target.function, "audit");
        assert_eq!(target.args, vec!["it's", "7", "-1"]);
        assert!(target.or_replace && !target.if_not_exists);
    }

    #[test]
    fn a_mysql_body_becomes_an_inline_function() {
        let (head, target) = target_of(
            "CREATE TRIGGER `trg` BEFORE INSERT ON `t` FOR EACH ROW BEGIN IF NEW.qty < 0 THEN SET NEW.qty = 0; \
             ELSEIF NEW.qty > 100 THEN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'too many'; END IF; INSERT INTO \
             log(v) VALUES (NEW.qty); END",
        );
        assert_eq!(
            head,
            "CREATE TRIGGER \"trg\" BEFORE INSERT ON \"t\" FOR EACH ROW EXECUTE FUNCTION "
        );
        assert_eq!(target.function, INLINE_TRIGGER_FUNCTION);
        assert_eq!(target.args[0], "mysql");
        assert_eq!(
            target.args[2],
            "BEGIN\nIF NEW.qty < 0 THEN\nNEW.qty := 0;\nELSIF NEW.qty > 100 THEN\nRAISE EXCEPTION SQLSTATE '45000' \
             '%', 'too many';\nEND IF;\nINSERT INTO log(v) VALUES (NEW.qty);\nEND"
        );
    }

    #[test]
    fn a_mysql_single_statement_body_is_taken() {
        let (_, target) =
            target_of("CREATE TRIGGER trg BEFORE UPDATE ON t FOR EACH ROW SET NEW.a = OLD.a + 1, NEW.b = 2");
        assert_eq!(target.args[2], "BEGIN\nNEW.a := OLD.a + 1;\nNEW.b := 2;\nEND");
    }

    #[test]
    fn a_sqlite_body_gets_row_level_when_and_raise() {
        let (head, target) = target_of(
            "CREATE TRIGGER IF NOT EXISTS trg BEFORE DELETE ON t WHEN OLD.locked = 1 BEGIN SELECT RAISE(ABORT, \
             'locked'); END;",
        );
        assert_eq!(
            head,
            "CREATE TRIGGER  trg BEFORE DELETE ON t FOR EACH ROW WHEN (OLD.locked = 1) EXECUTE FUNCTION "
        );
        assert!(target.if_not_exists);
        assert_eq!(target.args[0], "sqlite");
        assert_eq!(target.args[2], "BEGIN\nRAISE EXCEPTION '%', 'locked';\nEND");

        let (_, target) = target_of(
            "CREATE TRIGGER trg BEFORE INSERT ON t BEGIN SELECT RAISE(IGNORE) WHERE NEW.a IS NULL; SELECT \
             RAISE(ABORT, 'dup') FROM t WHERE a = NEW.a; END",
        );
        assert_eq!(
            target.args[2],
            "BEGIN\nIF (NEW.a IS NULL) THEN\nRETURN NULL;\nEND IF;\nIF EXISTS (SELECT 1 FROM t WHERE a = NEW.a) \
             THEN\nRAISE EXCEPTION '%', 'dup';\nEND IF;\nEND"
        );
    }

    #[test]
    fn unsupported_body_constructs_are_named() {
        for (sql, what) in [
            (
                "CREATE TRIGGER g BEFORE INSERT ON t FOR EACH ROW BEGIN DECLARE x INT; END",
                "DECLARE",
            ),
            (
                "CREATE TRIGGER g BEFORE INSERT ON t FOR EACH ROW BEGIN SET @n = 1; END",
                "SET of anything but",
            ),
            (
                "CREATE TRIGGER g BEFORE INSERT ON t BEGIN SELECT RAISE(ROLLBACK, 'x'); END",
                "RAISE(ROLLBACK",
            ),
            (
                "CREATE TRIGGER g BEFORE INSERT ON t FOR EACH ROW WHILE 1 DO SET NEW.a = 1; END WHILE",
                "WHILE",
            ),
            (
                "CREATE TRIGGER g AFTER INSERT ON t FOR EACH ROW FOLLOWS h SET NEW.a = 1",
                "FOLLOWS",
            ),
            (
                "CREATE TEMP TRIGGER g AFTER INSERT ON t BEGIN DELETE FROM u; END",
                "TEMPORARY",
            ),
        ] {
            let err = preprocess(sql).unwrap_err().to_string();
            assert!(
                err.contains(what) && err.contains(TRIGGER_UNSUPPORTED_MARKER),
                "{sql}: {err}"
            );
        }
    }

    #[test]
    fn inline_bodies_are_recognised_for_statement_splitting() {
        assert!(is_inline_trigger(
            "CREATE TRIGGER g AFTER INSERT ON t BEGIN DELETE FROM u; DELETE FROM v; END"
        ));
        assert!(!is_inline_trigger(
            "CREATE TRIGGER g AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()"
        ));
        assert!(!is_inline_trigger("INSERT INTO t VALUES ('CREATE TRIGGER')"));
    }

    #[test]
    fn drop_function_cascade_is_folded_into_the_name() {
        let out = preprocess("DROP FUNCTION IF EXISTS public.Audit_Fn() CASCADE;")
            .unwrap()
            .into_owned();
        let start = out.find(DROP_CASCADE_PREFIX).unwrap();
        let end = out[start..].find('"').unwrap() + start;
        assert_eq!(decode_drop_function(&out[start..end]), ("audit_fn".to_string(), true));
        assert!(
            out.starts_with("DROP FUNCTION IF EXISTS \"") && out.ends_with("() CASCADE;"),
            "{out}"
        );
        assert_eq!(decode_drop_function("audit_fn"), ("audit_fn".to_string(), false));
        assert!(matches!(preprocess("DROP TABLE t CASCADE").unwrap(), Cow::Borrowed(_)));
    }

    #[test]
    fn drop_without_a_table_and_the_alter_forms_are_planned() {
        assert_eq!(
            preprocess("DROP TRIGGER IF EXISTS `trg`;").unwrap(),
            format!("DROP TRIGGER IF EXISTS \"trg\" ON {ANY_TABLE}")
        );
        assert_eq!(
            preprocess("ALTER TRIGGER a ON s.t RENAME TO b").unwrap(),
            format!("CALL {TRIGGER_ADMIN_PROCEDURE}('rename', 's.t', 'a', 'b')")
        );
        assert_eq!(
            preprocess("ALTER TABLE t DISABLE TRIGGER ALL;").unwrap(),
            format!("CALL {TRIGGER_ADMIN_PROCEDURE}('disable', 't', '*', '')")
        );
        assert_eq!(
            preprocess("ALTER TABLE ONLY t ENABLE ALWAYS TRIGGER \"Trg\"").unwrap(),
            format!("CALL {TRIGGER_ADMIN_PROCEDURE}('enable', 't', 'Trg', '')")
        );
        assert!(preprocess("ALTER TABLE t ENABLE REPLICA TRIGGER g").is_err());
    }
}
