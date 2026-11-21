//! SQL cursors: `DECLARE … CURSOR`, `FETCH`, `MOVE`, `CLOSE`, and their
//! closing by `DISCARD ALL`.
//!
//! A cursor belongs to the session that declared it: its name lives in that
//! session's own namespace ([`SessionCursors`], a member of the per-session
//! state), so two sessions can each have a cursor `c` and neither can reach
//! the other's. The embedded handle is a session of its own.
//!
//! What a cursor holds. `DECLARE` runs the query once, as the declaring role
//! and under the declaring transaction's snapshot, and keeps the result. Every
//! later `FETCH` reads from that result: it sees no later change, and a change
//! of role or tenant after `DECLARE` cannot widen it (the cursor records the
//! identity it was declared under and refuses any other). The result is
//! bounded by `[session] cursor_max_rows`.
//!
//! Lifetime. Without `WITH HOLD` a cursor needs a transaction block and ends
//! with it. `WITH HOLD` survives `COMMIT`; it is dropped when the transaction
//! that declared it rolls back, and when the session ends.

use crate::session::scoped::SessionScopedState;
use crate::sql::sql_scan::{self, Tok, TokKind};
use crate::{DataType, EmbeddedDatabase, Error, Result, Schema, Tuple, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Open cursors per open database (engine instance), for observability.
fn open_counts() -> &'static dashmap::DashMap<u64, AtomicUsize> {
    static COUNTS: std::sync::OnceLock<dashmap::DashMap<u64, AtomicUsize>> = std::sync::OnceLock::new();
    COUNTS.get_or_init(dashmap::DashMap::new)
}

fn count_opened(engine: u64) {
    open_counts()
        .entry(engine)
        .or_insert_with(|| AtomicUsize::new(0))
        .fetch_add(1, Ordering::Relaxed);
}

fn count_closed(engine: u64, n: usize) {
    if n == 0 {
        return;
    }
    if let Some(counter) = open_counts().get(&engine) {
        #[allow(deprecated)] // `try_update` needs Rust 1.95; the MSRV is 1.88
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |now| Some(now.saturating_sub(n)));
    }
}

/// Who a cursor was declared by. A `FETCH` under any other identity is
/// refused: the rows were selected and filtered for this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CursorIdentity {
    /// The session's user.
    pub user: String,
    /// The tenant the session was bound to, if any.
    pub tenant: Option<crate::tenant::TenantId>,
}

impl CursorIdentity {
    pub(crate) fn of(scoped: &SessionScopedState) -> Self {
        Self {
            user: scoped.username(),
            tenant: scoped.tenant_id(),
        }
    }
}

/// One open cursor.
#[derive(Debug)]
pub(crate) struct Cursor {
    /// The query after `FOR`, as written.
    pub statement: String,
    /// The result's columns.
    pub schema: Arc<Schema>,
    rows: Vec<Tuple>,
    /// `0` before the first row, `n` on row `n`, `len + 1` after the last.
    position: usize,
    /// `WITH HOLD`.
    pub holdable: bool,
    /// `BINARY`.
    pub binary: bool,
    /// `false` only for `NO SCROLL`.
    pub scrollable: bool,
    /// Declared inside a transaction block that has not ended yet.
    in_declaring_transaction: bool,
    identity: CursorIdentity,
    /// Unix microseconds at DECLARE.
    pub created_micros: i64,
}

/// How a `FETCH` or `MOVE` moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Next,
    Prior,
    First,
    Last,
    Absolute(i64),
    Relative(i64),
    /// `FORWARD n` / a bare count; negative runs backward.
    Forward(i64),
    ForwardAll,
    /// `BACKWARD n`; negative runs forward.
    Backward(i64),
    BackwardAll,
}

fn forward_only(name: &str) -> Error {
    Error::Raised {
        sqlstate: "55000".to_string(),
        message: format!("cursor \"{name}\" can only scan forward"),
        detail: None,
        hint: Some("Declare it with SCROLL option to enable backward scan.".to_string()),
    }
}

impl Cursor {
    /// Rows the cursor holds.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Move as `direction` says; the 1-based positions of the rows a `FETCH`
    /// returns, in the order it returns them.
    fn step(&mut self, name: &str, direction: Direction) -> Result<Vec<usize>> {
        let n = self.rows.len();
        let after = n + 1;
        let p = self.position;
        let mut out = Vec::new();
        let backward = |cursor: &Self| -> Result<()> {
            if cursor.scrollable {
                Ok(())
            } else {
                Err(forward_only(name))
            }
        };
        // Go to an absolute position; `target` may be 0 (before the first row)
        // or past the end.
        let mut go = |cursor: &mut Self, target: usize, out: &mut Vec<usize>| -> Result<()> {
            let target = target.min(after);
            // A cursor that scans forward only cannot return to, or re-read,
            // a row it has passed.
            if target <= p && p > 0 {
                backward(cursor)?;
            }
            cursor.position = target;
            if (1..=n).contains(&target) {
                out.push(target);
            }
            Ok(())
        };
        match direction {
            Direction::Next => go(self, p.saturating_add(1), &mut out)?,
            Direction::Prior => {
                backward(self)?;
                go(self, p.saturating_sub(1), &mut out)?;
            }
            Direction::First => go(self, 1, &mut out)?,
            Direction::Last => {
                backward(self)?;
                go(self, n, &mut out)?;
            }
            Direction::Absolute(k) => {
                if k >= 0 {
                    go(self, usize::try_from(k).unwrap_or(usize::MAX), &mut out)?;
                } else {
                    backward(self)?;
                    let back = usize::try_from(k.unsigned_abs()).unwrap_or(usize::MAX);
                    go(self, after.saturating_sub(back), &mut out)?;
                }
            }
            Direction::Relative(k) => {
                if k >= 0 {
                    let forward = usize::try_from(k).unwrap_or(usize::MAX);
                    go(self, p.saturating_add(forward), &mut out)?;
                } else {
                    backward(self)?;
                    let back = usize::try_from(k.unsigned_abs()).unwrap_or(usize::MAX);
                    go(self, p.saturating_sub(back), &mut out)?;
                }
            }
            Direction::Forward(0) | Direction::Backward(0) => go(self, p, &mut out)?,
            Direction::Forward(k) if k > 0 => self.run_forward(usize::try_from(k).unwrap_or(usize::MAX), &mut out),
            Direction::Backward(k) if k < 0 => {
                self.run_forward(usize::try_from(k.unsigned_abs()).unwrap_or(usize::MAX), &mut out);
            }
            Direction::ForwardAll => self.run_forward(usize::MAX, &mut out),
            Direction::Forward(k) | Direction::Backward(k) => {
                backward(self)?;
                self.run_backward(usize::try_from(k.unsigned_abs()).unwrap_or(usize::MAX), &mut out);
            }
            Direction::BackwardAll => {
                backward(self)?;
                self.run_backward(usize::MAX, &mut out);
            }
        }
        Ok(out)
    }

    fn run_forward(&mut self, count: usize, out: &mut Vec<usize>) {
        let n = self.rows.len();
        let mut taken = 0usize;
        while taken < count && self.position < n {
            self.position += 1;
            out.push(self.position);
            taken += 1;
        }
        if taken < count {
            self.position = n + 1;
        }
    }

    fn run_backward(&mut self, count: usize, out: &mut Vec<usize>) {
        let mut taken = 0usize;
        while taken < count && self.position > 1 {
            self.position -= 1;
            if self.position <= self.rows.len() {
                out.push(self.position);
                taken += 1;
            }
        }
        if taken < count {
            self.position = 0;
        }
    }

    fn rows_at(&self, positions: &[usize]) -> Vec<Tuple> {
        positions
            .iter()
            .filter_map(|p| p.checked_sub(1).and_then(|i| self.rows.get(i)).cloned())
            .collect()
    }
}

/// What a new cursor is declared with.
#[derive(Debug, Clone, Default)]
pub(crate) struct CursorOptions {
    pub holdable: bool,
    pub binary: bool,
    /// `Some(false)` for `NO SCROLL`.
    pub scroll: Option<bool>,
}

/// One session's cursors, by name.
#[derive(Debug)]
pub struct SessionCursors {
    engine: u64,
    map: parking_lot::Mutex<HashMap<String, Cursor>>,
}

fn no_such_cursor(name: &str) -> Error {
    Error::sql_state("34000", format!("cursor \"{name}\" does not exist"))
}

impl SessionCursors {
    /// An empty namespace for a session of the database `engine`.
    pub(crate) fn new(engine: u64) -> Self {
        Self {
            engine,
            map: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Open a cursor over an already computed result.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open(
        &self,
        name: &str,
        statement: &str,
        schema: Arc<Schema>,
        rows: Vec<Tuple>,
        options: &CursorOptions,
        in_transaction: bool,
        identity: CursorIdentity,
    ) -> Result<()> {
        let mut map = self.map.lock();
        if map.contains_key(name) {
            return Err(Error::sql_state("42P03", format!("cursor \"{name}\" already exists")));
        }
        let created_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
        map.insert(
            name.to_string(),
            Cursor {
                statement: statement.to_string(),
                schema,
                rows,
                position: 0,
                holdable: options.holdable,
                binary: options.binary,
                scrollable: options.scroll != Some(false),
                in_declaring_transaction: in_transaction,
                identity,
                created_micros,
            },
        );
        count_opened(self.engine);
        Ok(())
    }

    /// Is a cursor of this name open?
    pub(crate) fn exists(&self, name: &str) -> bool {
        self.map.lock().contains_key(name)
    }

    /// Move the cursor; the rows a `FETCH` returns (a `MOVE` counts them).
    pub(crate) fn fetch(
        &self,
        name: &str,
        direction: Direction,
        identity: &CursorIdentity,
    ) -> Result<(Arc<Schema>, Vec<Tuple>)> {
        let mut map = self.map.lock();
        let cursor = map.get_mut(name).ok_or_else(|| no_such_cursor(name))?;
        if cursor.identity != *identity {
            return Err(Error::Raised {
                sqlstate: "42501".to_string(),
                message: format!("permission denied for cursor \"{name}\""),
                detail: Some(
                    "The cursor was declared under a different role or tenant; its rows were selected for that one."
                        .to_string(),
                ),
                hint: Some("Close it and declare it again.".to_string()),
            });
        }
        let positions = cursor.step(name, direction)?;
        Ok((Arc::clone(&cursor.schema), cursor.rows_at(&positions)))
    }

    /// The columns of an open cursor.
    pub(crate) fn schema_of(&self, name: &str) -> Option<Arc<Schema>> {
        self.map.lock().get(name).map(|cursor| Arc::clone(&cursor.schema))
    }

    /// Close one cursor.
    pub(crate) fn close(&self, name: &str) -> Result<()> {
        match self.map.lock().remove(name) {
            Some(_) => {
                count_closed(self.engine, 1);
                Ok(())
            }
            None => Err(no_such_cursor(name)),
        }
    }

    /// Close every cursor of the session.
    pub(crate) fn close_all(&self) {
        let closed = {
            let mut map = self.map.lock();
            let closed = map.len();
            map.clear();
            closed
        };
        count_closed(self.engine, closed);
    }

    /// The session's transaction block ended. A cursor without HOLD ends with
    /// it; a held cursor of this block stays after COMMIT and goes with
    /// ROLLBACK; a held cursor of an earlier block is not touched.
    pub(crate) fn at_transaction_end(&self, committed: bool) {
        let mut map = self.map.lock();
        if map.is_empty() {
            return;
        }
        let before = map.len();
        map.retain(|_, cursor| {
            if !cursor.in_declaring_transaction {
                return true;
            }
            if cursor.holdable && committed {
                cursor.in_declaring_transaction = false;
                return true;
            }
            false
        });
        let closed = before - map.len();
        drop(map);
        count_closed(self.engine, closed);
    }

    /// `pg_cursors` rows: name, statement, is_holdable, is_binary,
    /// is_scrollable, creation_time.
    pub(crate) fn listing(&self) -> Vec<Tuple> {
        let map = self.map.lock();
        let mut names: Vec<&String> = map.keys().collect();
        names.sort();
        names
            .into_iter()
            .filter_map(|name| map.get(name).map(|cursor| (name, cursor)))
            .map(|(name, cursor)| {
                Tuple::new(vec![
                    Value::String(name.clone()),
                    Value::String(cursor.statement.clone()),
                    Value::Boolean(cursor.holdable),
                    Value::Boolean(cursor.binary),
                    Value::Boolean(cursor.scrollable),
                    Value::Timestamp(
                        chrono::DateTime::from_timestamp_micros(cursor.created_micros)
                            .unwrap_or_default()
                            .naive_utc(),
                    ),
                ])
            })
            .collect()
    }

    /// How many cursors the session has open.
    pub(crate) fn len(&self) -> usize {
        self.map.lock().len()
    }
}

impl Drop for SessionCursors {
    fn drop(&mut self) {
        count_closed(self.engine, self.map.get_mut().len());
    }
}

/// The columns of `pg_cursors`.
pub(crate) fn pg_cursors_schema() -> Schema {
    Schema {
        columns: vec![
            crate::Column::new("name", DataType::Text),
            crate::Column::new("statement", DataType::Text),
            crate::Column::new("is_holdable", DataType::Boolean),
            crate::Column::new("is_binary", DataType::Boolean),
            crate::Column::new("is_scrollable", DataType::Boolean),
            crate::Column::new("creation_time", DataType::Timestamp),
        ],
    }
}

/// `pg_cursors` for the session running the statement on this thread.
pub(crate) fn pg_cursors_rows() -> Vec<Tuple> {
    crate::session_scoped_state_tls().map_or_else(Vec::new, |scoped| scoped.cursors().listing())
}

/// A cursor statement, parsed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CursorCommand {
    Declare {
        name: String,
        options: CursorOptions2,
        query: String,
    },
    Fetch {
        name: String,
        direction: Direction,
    },
    Move {
        name: String,
        direction: Direction,
    },
    Close {
        name: String,
    },
    CloseAll,
}

/// [`CursorOptions`] with equality, for the parsed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CursorOptions2 {
    pub holdable: bool,
    pub binary: bool,
    pub scroll: Option<bool>,
}

impl From<CursorOptions2> for CursorOptions {
    fn from(o: CursorOptions2) -> Self {
        Self {
            holdable: o.holdable,
            binary: o.binary,
            scroll: o.scroll,
        }
    }
}

fn syntax(message: impl Into<String>) -> Error {
    Error::sql_state("42601", message)
}

/// Does the statement start with a cursor keyword? A few byte compares, for
/// the entry points every statement passes.
#[inline]
pub(crate) fn may_be_cursor_statement(sql: &str) -> bool {
    let head = sql.trim_start();
    let Some(first) = head.as_bytes().first() else {
        return false;
    };
    if !matches!(first.to_ascii_uppercase(), b'D' | b'F' | b'M' | b'C' | b'-' | b'/') {
        return false;
    }
    let head = crate::protocol::postgres::handler::pg_skip_leading_comments(head).trim_start();
    ["DECLARE", "FETCH", "MOVE", "CLOSE"].iter().any(|keyword| {
        head.get(..keyword.len())
            .is_some_and(|word| word.eq_ignore_ascii_case(keyword))
            && head
                .get(keyword.len()..)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|c| c.is_whitespace() || c == '/' || c == '-')
    })
}

/// The query of a `DECLARE … CURSOR FOR query`; `None` for anything else.
pub(crate) fn declared_query(sql: &str) -> Option<String> {
    match parse_cursor_command(sql) {
        Ok(Some(CursorCommand::Declare { query, .. })) => Some(query),
        _ => None,
    }
}

/// Is `sql` one of the cursor statements (well formed or not)?
pub(crate) fn is_cursor_statement(sql: &str) -> bool {
    may_be_cursor_statement(sql) && !matches!(parse_cursor_command(sql), Ok(None))
}

fn name_of(tok: Option<&Tok>, sql: &str, what: &str) -> Result<String> {
    match tok {
        Some(tok) if matches!(tok.kind, TokKind::Word | TokKind::Quoted) => Ok(sql_scan::ident_value(tok, sql)),
        _ => Err(syntax(format!("{what}: a cursor name is expected"))),
    }
}

/// A signed integer written as `[-|+] digits` at `toks[at..]`; the value and
/// how many tokens it took.
fn signed_count(toks: &[Tok], at: usize, sql: &str) -> Option<(i64, usize)> {
    let first = toks.get(at)?;
    let (sign, number, used) = if first.is_punct(sql, '-') {
        (-1i64, toks.get(at + 1)?, 2)
    } else if first.is_punct(sql, '+') {
        (1i64, toks.get(at + 1)?, 2)
    } else {
        (1i64, first, 1)
    };
    if number.kind != TokKind::Number {
        return None;
    }
    let value: i64 = number.text(sql).parse().ok()?;
    Some((sign.checked_mul(value)?, used))
}

fn parse_direction(toks: &[Tok], sql: &str, verb: &str) -> Result<Direction> {
    let bad = || syntax(format!("{verb}: the direction is not understood"));
    let Some(first) = toks.first() else {
        return Ok(Direction::Next);
    };
    let rest_count = |from: usize| -> Result<Option<i64>> {
        if toks.len() == from {
            return Ok(None);
        }
        match signed_count(toks, from, sql) {
            Some((value, used)) if from + used == toks.len() => Ok(Some(value)),
            _ => Err(bad()),
        }
    };
    let single = |direction: Direction| -> Result<Direction> {
        if toks.len() == 1 {
            Ok(direction)
        } else {
            Err(bad())
        }
    };
    if first.kind == TokKind::Word {
        let word = first.text(sql).to_ascii_uppercase();
        return match word.as_str() {
            "NEXT" => single(Direction::Next),
            "PRIOR" => single(Direction::Prior),
            "FIRST" => single(Direction::First),
            "LAST" => single(Direction::Last),
            "ALL" => single(Direction::ForwardAll),
            "ABSOLUTE" => rest_count(1)?.map(Direction::Absolute).ok_or_else(bad),
            "RELATIVE" => rest_count(1)?.map(Direction::Relative).ok_or_else(bad),
            "FORWARD" | "BACKWARD" => {
                let forward = word == "FORWARD";
                if toks.len() == 2 && toks.get(1).is_some_and(|t| t.is_word(sql, "ALL")) {
                    return Ok(if forward { Direction::ForwardAll } else { Direction::BackwardAll });
                }
                let count = rest_count(1)?.unwrap_or(1);
                Ok(if forward { Direction::Forward(count) } else { Direction::Backward(count) })
            }
            _ => Err(bad()),
        };
    }
    rest_count(0)?.map(Direction::Forward).ok_or_else(bad)
}

/// Parse a cursor statement; `None` when `sql` is not one (so the caller goes
/// on to its ordinary parser).
pub(crate) fn parse_cursor_command(sql: &str) -> Result<Option<CursorCommand>> {
    if !may_be_cursor_statement(sql) {
        return Ok(None);
    }
    let sql = sql_scan::statement_text(sql);
    let toks = sql_scan::tokens(sql);
    let Some(verb) = toks.first() else {
        return Ok(None);
    };
    if verb.is_word(sql, "DECLARE") {
        // DECLARE name [BINARY] [ASENSITIVE | INSENSITIVE] [[NO] SCROLL]
        //   CURSOR [{WITH | WITHOUT} HOLD] FOR query
        let Some(cursor_at) = toks
            .iter()
            .take(8)
            .position(|t| t.depth == 0 && t.is_word(sql, "CURSOR"))
        else {
            return Ok(None);
        };
        if cursor_at < 2 {
            return Err(syntax("DECLARE: a cursor name is expected"));
        }
        let name = name_of(toks.get(1), sql, "DECLARE")?;
        let mut options = CursorOptions2::default();
        let mut i = 2;
        while i < cursor_at {
            let Some(tok) = toks.get(i) else { break };
            if tok.is_word(sql, "BINARY") {
                options.binary = true;
            } else if tok.is_word(sql, "INSENSITIVE") || tok.is_word(sql, "ASENSITIVE") {
                // The result is fixed at DECLARE either way.
            } else if tok.is_word(sql, "SCROLL") {
                options.scroll = Some(true);
            } else if tok.is_word(sql, "NO") && toks.get(i + 1).is_some_and(|t| t.is_word(sql, "SCROLL")) {
                options.scroll = Some(false);
                i += 1;
            } else {
                return Err(syntax(format!(
                    "DECLARE: unexpected \"{}\" before CURSOR",
                    tok.text(sql)
                )));
            }
            i += 1;
        }
        let mut i = cursor_at + 1;
        if let Some(tok) = toks.get(i) {
            let with = tok.is_word(sql, "WITH");
            if with || tok.is_word(sql, "WITHOUT") {
                if !toks.get(i + 1).is_some_and(|t| t.is_word(sql, "HOLD")) {
                    return Err(syntax("DECLARE: HOLD is expected after WITH or WITHOUT"));
                }
                options.holdable = with;
                i += 2;
            }
        }
        let for_tok = toks
            .get(i)
            .filter(|t| t.is_word(sql, "FOR"))
            .ok_or_else(|| syntax("DECLARE: FOR <query> is expected"))?;
        let query = sql.get(for_tok.end..).unwrap_or("").trim();
        if query.is_empty() {
            return Err(syntax("DECLARE: FOR <query> is expected"));
        }
        return Ok(Some(CursorCommand::Declare {
            name,
            options,
            query: query.to_string(),
        }));
    }
    if verb.is_word(sql, "CLOSE") {
        return match (toks.get(1), toks.len()) {
            (Some(tok), 2) if tok.is_word(sql, "ALL") => Ok(Some(CursorCommand::CloseAll)),
            (tok, 2) => Ok(Some(CursorCommand::Close {
                name: name_of(tok, sql, "CLOSE")?,
            })),
            _ => Err(syntax("CLOSE: a cursor name or ALL is expected")),
        };
    }
    let is_fetch = verb.is_word(sql, "FETCH");
    if !is_fetch && !verb.is_word(sql, "MOVE") {
        return Ok(None);
    }
    let label = if is_fetch { "FETCH" } else { "MOVE" };
    let body = toks.get(1..).unwrap_or(&[]);
    let Some((name_tok, before)) = body.split_last() else {
        return Err(syntax(format!("{label}: a cursor name is expected")));
    };
    let name = name_of(Some(name_tok), sql, label)?;
    let before = match before.split_last() {
        Some((tok, rest)) if tok.is_word(sql, "FROM") || tok.is_word(sql, "IN") => rest,
        _ => before,
    };
    let direction = parse_direction(before, sql, label)?;
    Ok(Some(if is_fetch {
        CursorCommand::Fetch { name, direction }
    } else {
        CursorCommand::Move { name, direction }
    }))
}

/// What a cursor statement answered.
#[derive(Debug, Clone)]
pub(crate) struct CursorOutcome {
    /// The PostgreSQL command tag (`DECLARE CURSOR`, `FETCH 3`, `MOVE 1`,
    /// `CLOSE CURSOR`).
    pub tag: String,
    /// A `FETCH`'s columns and rows.
    pub rows: Option<(Arc<Schema>, Vec<Tuple>)>,
    /// Rows fetched or moved over.
    pub count: u64,
}

impl CursorOutcome {
    fn command(tag: &str) -> Self {
        Self {
            tag: tag.to_string(),
            rows: None,
            count: 0,
        }
    }

    /// The rows of a `FETCH`; none for any other cursor statement.
    pub(crate) fn into_rows(self) -> Vec<Tuple> {
        self.rows.map(|(_, rows)| rows).unwrap_or_default()
    }

    /// The rows and their columns; no columns for a statement without rows.
    pub(crate) fn into_rows_and_schema(self) -> (Vec<Tuple>, Arc<Schema>) {
        match self.rows {
            Some((schema, rows)) => (rows, schema),
            None => (Vec::new(), Arc::new(Schema { columns: Vec::new() })),
        }
    }

    /// What `run_statement` answers.
    pub(crate) fn into_statement_outcome(self) -> crate::StatementOutcome {
        match self.rows {
            Some((schema, rows)) => crate::StatementOutcome::Rows {
                columns: schema.columns.iter().map(|column| column.name.clone()).collect(),
                rows,
            },
            None => crate::StatementOutcome::Affected(self.count),
        }
    }
}

/// Is this `DISCARD ALL`?
pub(crate) fn is_discard_all(sql: &str) -> bool {
    let head = sql.trim_start();
    if !head.as_bytes().get(..7).is_some_and(|w| w.eq_ignore_ascii_case(b"DISCARD")) {
        return false;
    }
    let toks = sql_scan::tokens(sql_scan::statement_text(sql));
    toks.len() == 2 && toks.get(1).is_some_and(|t| t.is_word(sql_scan::statement_text(sql), "ALL"))
}

impl EmbeddedDatabase {
    /// Open cursors across every session of this database.
    pub fn open_cursor_count(&self) -> usize {
        open_counts()
            .get(&self.storage.instance_id())
            .map_or(0, |counter| counter.load(Ordering::Relaxed))
    }

    /// The row limit of one cursor's result.
    fn cursor_row_limit(&self) -> usize {
        self.config.session.cursor_max_rows
    }

    /// Run one parsed cursor statement for the session `scoped` belongs to.
    /// `run_query` runs a DECLARE's query as that session (its role, its
    /// transaction).
    pub(crate) fn run_cursor_command(
        &self,
        scoped: &SessionScopedState,
        in_transaction_block: bool,
        command: CursorCommand,
        run_query: impl FnOnce(&str) -> Result<(Vec<Tuple>, Arc<Schema>)>,
    ) -> Result<CursorOutcome> {
        let cursors = scoped.cursors();
        match command {
            CursorCommand::Declare { name, options, query } => {
                if !options.holdable && !in_transaction_block {
                    return Err(Error::sql_state(
                        "25P01",
                        "DECLARE CURSOR can only be used in transaction blocks",
                    ));
                }
                if cursors.exists(&name) {
                    return Err(Error::sql_state("42P03", format!("cursor \"{name}\" already exists")));
                }
                let head = sql_scan::tokens(&query);
                let reads = head.first().is_some_and(|t| {
                    t.is_word(&query, "SELECT")
                        || t.is_word(&query, "WITH")
                        || t.is_word(&query, "VALUES")
                        || t.is_word(&query, "TABLE")
                        || t.is_punct(&query, '(')
                });
                if !reads {
                    return Err(syntax("DECLARE CURSOR: the cursor's statement must be a query"));
                }
                let (rows, schema) = run_query(&query)?;
                let limit = self.cursor_row_limit();
                if limit > 0 && rows.len() > limit {
                    return Err(Error::Raised {
                        sqlstate: "54000".to_string(),
                        message: format!(
                            "cursor \"{name}\" would hold {} rows; the limit is {limit}",
                            rows.len()
                        ),
                        detail: Some("A cursor keeps the whole result of its query from DECLARE on.".to_string()),
                        hint: Some(
                            "Narrow the query, page with LIMIT and OFFSET, or raise [session] cursor_max_rows."
                                .to_string(),
                        ),
                    });
                }
                cursors.open(
                    &name,
                    &query,
                    schema,
                    rows,
                    &options.into(),
                    in_transaction_block,
                    CursorIdentity::of(scoped),
                )?;
                Ok(CursorOutcome::command("DECLARE CURSOR"))
            }
            CursorCommand::Fetch { name, direction } => {
                let (schema, rows) = cursors.fetch(&name, direction, &CursorIdentity::of(scoped))?;
                let count = rows.len() as u64;
                Ok(CursorOutcome {
                    tag: format!("FETCH {count}"),
                    rows: Some((schema, rows)),
                    count,
                })
            }
            CursorCommand::Move { name, direction } => {
                let (_, rows) = cursors.fetch(&name, direction, &CursorIdentity::of(scoped))?;
                let count = rows.len() as u64;
                Ok(CursorOutcome {
                    tag: format!("MOVE {count}"),
                    rows: None,
                    count,
                })
            }
            CursorCommand::Close { name } => {
                cursors.close(&name)?;
                Ok(CursorOutcome::command("CLOSE CURSOR"))
            }
            CursorCommand::CloseAll => {
                cursors.close_all();
                Ok(CursorOutcome::command("CLOSE CURSOR"))
            }
        }
    }

    /// The embedded entry points' cursor step: answers a cursor statement of
    /// this handle's session (or of the session whose statement this thread
    /// is running), `None` for any other statement. `DISCARD ALL` closes the
    /// cursors and is then left to its own handler.
    pub(crate) fn try_cursor_statement(&self, sql: &str, params: &[Value]) -> Result<Option<CursorOutcome>> {
        if !may_be_cursor_statement(sql) {
            let first = sql.trim_start().as_bytes().first().map(u8::to_ascii_uppercase);
            if first == Some(b'D') && is_discard_all(sql) {
                if let Some(scoped) = self.cursor_scope() {
                    scoped.cursors().close_all();
                }
            }
            return Ok(None);
        }
        let Some(command) = parse_cursor_command(sql)? else {
            return Ok(None);
        };
        // A statement of a session that reached the handle's own entry point:
        // only the session's entry points know its transaction block.
        let Some(scoped) = self.cursor_scope() else {
            return Err(Error::sql_state(
                "0A000",
                "a cursor statement of a session must be issued through that session's own entry points",
            ));
        };
        let in_block = self.global_txn_visible();
        // Inside the handle's transaction block a failed cursor statement
        // aborts the block, as any other statement does.
        self.run_global_statement(sql, || {
            self.run_cursor_command(&scoped, in_block, command, |query| {
                self.query_params_with_schema(query, params)
            })
        })
        .map(Some)
    }

    /// The session state a cursor statement of this handle belongs to: the
    /// handle's own. `None` when this thread is running a statement of some
    /// other session.
    fn cursor_scope(&self) -> Option<Arc<SessionScopedState>> {
        match crate::session_scoped_state_tls() {
            None => Some(Arc::clone(&self.embedded_scoped)),
            Some(scoped) if Arc::ptr_eq(&scoped, &self.embedded_scoped) => Some(scoped),
            Some(_) => None,
        }
    }

    /// The wire's cursor step for one connection's session.
    pub(crate) fn try_cursor_statement_for_session(
        &self,
        session_id: crate::session::SessionId,
        sql: &str,
        params: &[Value],
    ) -> Result<Option<CursorOutcome>> {
        let Some(command) = parse_cursor_command(sql)? else {
            return Ok(None);
        };
        let scoped = self.session_cursor_scope(session_id)?;
        let in_block = self.session_in_transaction(session_id);
        let slot = self.session_txn_slot(session_id);
        if let Some(txn) = slot.as_ref().map(|slot| slot.read()).as_deref().and_then(Option::as_ref) {
            Self::refuse_if_transaction_failed(txn, sql)?;
        }
        let outcome = self.run_cursor_command(&scoped, in_block, command, |query| {
            self.query_params_with_schema_for_session(session_id, query, params)
        });
        // A failed statement aborts the session's transaction block.
        if outcome.is_err() {
            if let Some(txn) = slot.as_ref().map(|slot| slot.read()).as_deref().and_then(Option::as_ref) {
                txn.mark_sql_aborted();
            }
        }
        outcome.map(Some)
    }

    fn session_cursor_scope(&self, session_id: crate::session::SessionId) -> Result<Arc<SessionScopedState>> {
        let session = self.session_manager.get_session(session_id)?;
        let scoped = Arc::clone(&session.read().scoped);
        Ok(scoped)
    }

    /// What a cursor statement of a wire session will return: `None` when
    /// `sql` is not a cursor statement, `Some(None)` for one without rows,
    /// `Some(Some(columns))` for a `FETCH` of a cursor that is open now.
    pub(crate) fn describe_cursor_statement_for_session(
        &self,
        session_id: crate::session::SessionId,
        sql: &str,
    ) -> Result<Option<Option<Schema>>> {
        match parse_cursor_command(sql)? {
            None => Ok(None),
            Some(CursorCommand::Fetch { name, .. }) => {
                let scoped = self.session_cursor_scope(session_id)?;
                Ok(Some(scoped.cursors().schema_of(&name).map(|schema| (*schema).clone())))
            }
            Some(_) => Ok(Some(None)),
        }
    }

    /// A wire session's transaction block ended.
    pub(crate) fn end_session_transaction_cursors(&self, session_id: crate::session::SessionId, committed: bool) {
        if let Ok(scoped) = self.session_cursor_scope(session_id) {
            scoped.cursors().at_transaction_end(committed);
        }
    }

    /// `DISCARD ALL` on a wire session.
    pub(crate) fn close_session_cursors(&self, session_id: crate::session::SessionId) {
        if let Ok(scoped) = self.session_cursor_scope(session_id) {
            scoped.cursors().close_all();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn cursor(n: i32, scrollable: bool) -> Cursor {
        Cursor {
            statement: "q".to_string(),
            schema: Arc::new(Schema { columns: vec![] }),
            rows: (1..=n).map(|i| Tuple::new(vec![Value::Int4(i)])).collect(),
            position: 0,
            holdable: false,
            binary: false,
            scrollable,
            in_declaring_transaction: true,
            identity: CursorIdentity {
                user: "u".to_string(),
                tenant: None,
            },
            created_micros: 0,
        }
    }

    #[test]
    fn every_direction_moves_as_postgresql_does() {
        let mut c = cursor(5, true);
        assert_eq!(c.step("c", Direction::Next).unwrap(), [1]);
        assert_eq!(c.step("c", Direction::Forward(2)).unwrap(), [2, 3]);
        assert_eq!(c.step("c", Direction::Prior).unwrap(), [2]);
        assert_eq!(c.step("c", Direction::Relative(0)).unwrap(), [2]);
        assert_eq!(c.step("c", Direction::Relative(2)).unwrap(), [4]);
        assert_eq!(c.step("c", Direction::Relative(-3)).unwrap(), [1]);
        assert_eq!(c.step("c", Direction::Last).unwrap(), [5]);
        assert_eq!(c.step("c", Direction::Next).unwrap(), Vec::<usize>::new());
        assert_eq!(c.position, 6);
        assert_eq!(c.step("c", Direction::Next).unwrap(), Vec::<usize>::new());
        assert_eq!(c.position, 6);
        assert_eq!(c.step("c", Direction::Prior).unwrap(), [5]);
        assert_eq!(c.step("c", Direction::First).unwrap(), [1]);
        assert_eq!(c.step("c", Direction::Prior).unwrap(), Vec::<usize>::new());
        assert_eq!(c.position, 0);
        assert_eq!(c.step("c", Direction::Absolute(3)).unwrap(), [3]);
        assert_eq!(c.step("c", Direction::Absolute(-1)).unwrap(), [5]);
        assert_eq!(c.step("c", Direction::Absolute(-5)).unwrap(), [1]);
        assert_eq!(c.step("c", Direction::Absolute(-6)).unwrap(), Vec::<usize>::new());
        assert_eq!(c.position, 0);
        assert_eq!(c.step("c", Direction::Absolute(9)).unwrap(), Vec::<usize>::new());
        assert_eq!(c.position, 6);
        assert_eq!(c.step("c", Direction::Backward(2)).unwrap(), [5, 4]);
        assert_eq!(c.step("c", Direction::BackwardAll).unwrap(), [3, 2, 1]);
        assert_eq!(c.position, 0);
        assert_eq!(c.step("c", Direction::ForwardAll).unwrap(), [1, 2, 3, 4, 5]);
        assert_eq!(c.position, 6);
        assert_eq!(c.step("c", Direction::Absolute(0)).unwrap(), Vec::<usize>::new());
        assert_eq!(c.position, 0);
        assert_eq!(c.step("c", Direction::Forward(-1)).unwrap(), Vec::<usize>::new());
        assert_eq!(c.step("c", Direction::Backward(-2)).unwrap(), [1, 2]);
        assert_eq!(c.step("c", Direction::Forward(10)).unwrap(), [3, 4, 5]);
        assert_eq!(c.position, 6);
    }

    #[test]
    fn an_empty_cursor_returns_nothing_in_any_direction() {
        let mut c = cursor(0, true);
        for direction in [
            Direction::Next,
            Direction::Prior,
            Direction::First,
            Direction::Last,
            Direction::ForwardAll,
            Direction::BackwardAll,
            Direction::Absolute(1),
            Direction::Relative(-1),
        ] {
            assert!(c.step("c", direction).unwrap().is_empty(), "{direction:?}");
        }
    }

    #[test]
    fn a_no_scroll_cursor_only_goes_forward() {
        let mut c = cursor(3, false);
        assert_eq!(c.step("c", Direction::Next).unwrap(), [1]);
        assert_eq!(c.step("c", Direction::Absolute(3)).unwrap(), [3]);
        for direction in [
            Direction::Prior,
            Direction::First,
            Direction::Last,
            Direction::Relative(0),
            Direction::Relative(-1),
            Direction::Absolute(1),
            Direction::Backward(1),
            Direction::BackwardAll,
            Direction::Forward(-1),
        ] {
            let err = c.step("c", direction).unwrap_err();
            assert_eq!(err.raised_sqlstate(), Some("55000"), "{direction:?}");
        }
        assert_eq!(c.step("c", Direction::ForwardAll).unwrap(), Vec::<usize>::new());
    }

    #[test]
    fn statements_are_parsed() {
        let declare = parse_cursor_command(
            "DECLARE \"My C\" BINARY INSENSITIVE NO SCROLL CURSOR WITH HOLD FOR SELECT 'CURSOR FOR' FROM t;",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            declare,
            CursorCommand::Declare {
                name: "My C".to_string(),
                options: CursorOptions2 {
                    holdable: true,
                    binary: true,
                    scroll: Some(false)
                },
                query: "SELECT 'CURSOR FOR' FROM t".to_string()
            }
        );
        let fetch = |sql: &str| match parse_cursor_command(sql).unwrap().unwrap() {
            CursorCommand::Fetch { name, direction } => (name, direction),
            other => panic!("{other:?}"),
        };
        assert_eq!(fetch("FETCH c"), ("c".to_string(), Direction::Next));
        assert_eq!(fetch("fetch next from C"), ("c".to_string(), Direction::Next));
        assert_eq!(fetch("FETCH PRIOR IN c"), ("c".to_string(), Direction::Prior));
        assert_eq!(fetch("FETCH 5 FROM c"), ("c".to_string(), Direction::Forward(5)));
        assert_eq!(fetch("FETCH -2 FROM c"), ("c".to_string(), Direction::Forward(-2)));
        assert_eq!(fetch("FETCH ALL FROM c"), ("c".to_string(), Direction::ForwardAll));
        assert_eq!(fetch("FETCH FORWARD FROM c"), ("c".to_string(), Direction::Forward(1)));
        assert_eq!(fetch("FETCH FORWARD ALL c"), ("c".to_string(), Direction::ForwardAll));
        assert_eq!(fetch("FETCH BACKWARD 3 FROM c"), ("c".to_string(), Direction::Backward(3)));
        assert_eq!(fetch("FETCH BACKWARD ALL FROM c"), ("c".to_string(), Direction::BackwardAll));
        assert_eq!(fetch("FETCH ABSOLUTE -1 FROM c"), ("c".to_string(), Direction::Absolute(-1)));
        assert_eq!(fetch("FETCH RELATIVE 0 FROM c"), ("c".to_string(), Direction::Relative(0)));
        assert_eq!(
            parse_cursor_command("MOVE LAST IN c").unwrap().unwrap(),
            CursorCommand::Move {
                name: "c".to_string(),
                direction: Direction::Last
            }
        );
        assert_eq!(parse_cursor_command("CLOSE ALL").unwrap().unwrap(), CursorCommand::CloseAll);
        assert_eq!(
            parse_cursor_command("close c;").unwrap().unwrap(),
            CursorCommand::Close { name: "c".to_string() }
        );
        assert!(parse_cursor_command("SELECT 1").unwrap().is_none());
        assert!(parse_cursor_command("DELETE FROM t").unwrap().is_none());
        assert!(parse_cursor_command("FETCH sideways FROM c").is_err());
        assert!(parse_cursor_command("DECLARE c CURSOR").is_err());
        assert!(is_discard_all("DISCARD ALL;"));
        assert!(!is_discard_all("DISCARD PLANS"));
    }

    #[test]
    fn a_fetch_under_another_identity_is_refused_and_widens_nothing() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
        db.execute("DECLARE c CURSOR WITH HOLD FOR SELECT id FROM t ORDER BY id")
            .unwrap();
        assert_eq!(db.query("FETCH c", &[]).unwrap().len(), 1);

        // Another role on the same session.
        let declared_as = db.embedded_scoped.username();
        db.embedded_scoped.set_username("someone_else");
        let err = db.query("FETCH c", &[]).unwrap_err();
        assert_eq!(err.raised_sqlstate(), Some("42501"), "{err}");
        assert_eq!(db.execute("MOVE c").unwrap_err().raised_sqlstate(), Some("42501"));
        db.embedded_scoped.set_username(&declared_as);

        // Another tenant on the same session.
        let tenant = crate::tenant::TenantId::new_v4();
        let _ = db.embedded_scoped.bind_tenant(crate::tenant::TenantContext {
            tenant_id: tenant,
            user_id: "u".to_string(),
            roles: Vec::new(),
            isolation_mode: crate::tenant::IsolationMode::SharedSchema,
        });
        let err = db.query("FETCH c", &[]).unwrap_err();
        assert_eq!(err.raised_sqlstate(), Some("42501"), "{err}");
        let _ = db.embedded_scoped.unbind_tenant();

        // Back under the declaring identity the cursor is where it was: the
        // refused statements did not move it.
        let rows = db.query("FETCH c", &[]).unwrap();
        assert_eq!(rows.first().and_then(|r| r.values.first()), Some(&Value::Int4(2)));
        db.execute("CLOSE c").unwrap();
    }

    #[test]
    fn hold_decides_what_a_transaction_end_keeps() {
        let cursors = SessionCursors::new(u64::MAX - 7);
        let schema = Arc::new(Schema { columns: vec![] });
        let who = CursorIdentity {
            user: "u".to_string(),
            tenant: None,
        };
        let open = |name: &str, holdable: bool, in_txn: bool| {
            cursors
                .open(
                    name,
                    "q",
                    Arc::clone(&schema),
                    Vec::new(),
                    &CursorOptions {
                        holdable,
                        binary: false,
                        scroll: None,
                    },
                    in_txn,
                    who.clone(),
                )
                .unwrap();
        };
        open("plain", false, true);
        open("held", true, true);
        open("older", true, false);
        cursors.at_transaction_end(true);
        assert!(!cursors.exists("plain"));
        assert!(cursors.exists("held"));
        assert!(cursors.exists("older"));
        open("plain2", false, true);
        open("held2", true, true);
        cursors.at_transaction_end(false);
        assert!(!cursors.exists("plain2"));
        assert!(!cursors.exists("held2"));
        assert!(cursors.exists("held"), "committed earlier: a later rollback does not drop it");
        assert!(cursors.exists("older"));
        assert_eq!(cursors.len(), 2);
        let other = CursorIdentity {
            user: "someone else".to_string(),
            tenant: None,
        };
        let err = cursors.fetch("held", Direction::Next, &other).unwrap_err();
        assert_eq!(err.raised_sqlstate(), Some("42501"));
        assert_eq!(cursors.fetch("nope", Direction::Next, &who).unwrap_err().raised_sqlstate(), Some("34000"));
    }
}
