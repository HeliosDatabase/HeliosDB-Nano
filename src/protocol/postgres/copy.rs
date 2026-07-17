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
///
/// Since W2.4 the wire path streams frames through `CopyStreamDecoder` instead of
/// buffering the whole stream, so this whole-buffer parser is retained only as the
/// independent oracle the streaming tests assert byte-for-byte equivalence against
/// (hence `#[cfg(test)]`).
#[cfg(test)]
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

/// Encode one field for COPY-text OUTPUT (inverse of `decode_text_field`):
/// None -> the `\N` NULL sentinel; otherwise the value with backslash, tab,
/// newline and carriage-return escaped per the COPY text format.
pub(crate) fn encode_text_field(v: Option<&[u8]>) -> String {
    match v {
        None => "\\N".to_string(),
        Some(bytes) => {
            let s = String::from_utf8_lossy(bytes);
            let mut out = String::with_capacity(s.len() + 2);
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\t' => out.push_str("\\t"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    other => out.push(other),
                }
            }
            out
        }
    }
}

/// Encode a row of already-rendered field bytes as a COPY-text line
/// (tab-joined, newline-terminated).
pub(crate) fn encode_text_row(fields: &[Option<Vec<u8>>]) -> Vec<u8> {
    let parts: Vec<String> = fields.iter().map(|f| encode_text_field(f.as_deref())).collect();
    let mut line = parts.join("\t");
    line.push('\n');
    line.into_bytes()
}

// ── CSV format (item 2f) ────────────────────────────────────────────────────

fn finish_csv_field(field: &str, was_quoted: bool) -> Option<String> {
    // PG CSV default: an UNQUOTED empty field is NULL; a quoted "" is the empty
    // string.
    if !was_quoted && field.is_empty() {
        None
    } else {
        Some(field.to_string())
    }
}

/// Parse COPY CSV bytes into rows of optional fields. A proper stateful parser:
/// quoted fields may contain the comma delimiter, embedded newlines, and `""`
/// (escaped quote). Comma delimiter, newline (LF or CRLF) row separator.
///
/// Retained since W2.4 only as the streaming decoder's test oracle (see
/// `parse_text_rows`); the wire path decodes incrementally via
/// `CopyStreamDecoder`.
#[cfg(test)]
pub(crate) fn parse_csv_rows(data: &[u8]) -> Vec<Vec<Option<String>>> {
    let text = String::from_utf8_lossy(data);
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut row: Vec<Option<String>> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut was_quoted = false;
    let mut field_started = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if !field_started => {
                in_quotes = true;
                was_quoted = true;
                field_started = true;
            }
            ',' => {
                row.push(finish_csv_field(&field, was_quoted));
                field.clear();
                was_quoted = false;
                field_started = false;
            }
            '\r' => {} // tolerate CRLF
            '\n' => {
                row.push(finish_csv_field(&field, was_quoted));
                rows.push(std::mem::take(&mut row));
                field.clear();
                was_quoted = false;
                field_started = false;
            }
            other => {
                field.push(other);
                field_started = true;
            }
        }
    }
    // trailing field/row when the data does not end with a newline
    if field_started || was_quoted || !row.is_empty() {
        row.push(finish_csv_field(&field, was_quoted));
        rows.push(row);
    }
    rows
}

/// Encode one field for COPY CSV output. None -> empty (the default CSV NULL).
/// Quotes the field (and doubles internal quotes) when it contains the comma
/// delimiter, a quote, or a newline.
pub(crate) fn encode_csv_field(v: Option<&[u8]>) -> String {
    match v {
        None => String::new(),
        Some(bytes) => {
            let s = String::from_utf8_lossy(bytes);
            if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
                format!("\"{}\"", s.replace('"', "\"\""))
            } else {
                s.into_owned()
            }
        }
    }
}

/// Encode a row of rendered field bytes as a COPY CSV line (comma-joined,
/// newline-terminated).
pub(crate) fn encode_csv_row(fields: &[Option<Vec<u8>>]) -> Vec<u8> {
    let parts: Vec<String> = fields.iter().map(|f| encode_csv_field(f.as_deref())).collect();
    let mut line = parts.join(",");
    line.push('\n');
    line.into_bytes()
}

// ── streaming decode (W2.4) ──────────────────────────────────────────────────

/// Byte length of the prefix of `buf` that is safe to lossily UTF-8-decode right
/// now: the whole buffer, unless it ends with the leading 1–3 bytes of a
/// still-incomplete multibyte UTF-8 character, whose bytes are withheld for the
/// next frame so a code point split across a CopyData boundary is never turned
/// into a premature replacement char. Genuinely invalid interior bytes are left
/// in place — the caller's `from_utf8_lossy` replaces them exactly as the batch
/// parsers do. A delimiter/newline byte is ASCII, so it is never withheld.
fn decodable_prefix_len(buf: &[u8]) -> usize {
    // Walk back from the end over UTF-8 continuation bytes (0b10xx_xxxx), at most
    // 3, to the lead byte of the final character. `trailing` counts the bytes of
    // that final sequence (lead + its continuation bytes).
    let mut trailing = 0usize;
    let mut lead = None;
    for &b in buf.iter().rev().take(4) {
        trailing += 1;
        if b & 0xC0 != 0x80 {
            lead = Some(b);
            break;
        }
    }
    let Some(lead) = lead else {
        // Empty buffer, or 4+ continuation bytes with no lead (malformed):
        // nothing to withhold — decode now and let lossy replacement handle it.
        return buf.len();
    };
    let expected = if lead < 0x80 {
        1
    } else if lead & 0xE0 == 0xC0 {
        2
    } else if lead & 0xF0 == 0xE0 {
        3
    } else if lead & 0xF8 == 0xF0 {
        4
    } else {
        // Not a valid lead byte (stray continuation or 0xF8..=0xFF): decode now.
        return buf.len();
    };
    if trailing < expected {
        buf.len() - trailing // incomplete final character — hold its bytes back
    } else {
        buf.len()
    }
}

/// Incremental `COPY … FROM STDIN` decoder (W2.4). `handle_copy` feeds it the raw
/// bytes of each `CopyData` frame as they arrive; it produces TYPED rows and
/// drops the raw bytes, so peak memory tracks the parsed batch instead of (~4–6×)
/// the whole stream buffered as one `Vec<u8>` (the pre-W2.4 OOM vector). Frames
/// split at arbitrary byte offsets — mid-line, mid-quoted-field, and
/// mid-UTF8-character — so the decoder carries the partial trailing line (text),
/// the CSV quote/field state, and any incomplete-UTF8 tail across `push` calls.
/// On a fully-buffered stream it yields byte-identical rows to `parse_text_rows`
/// / `parse_csv_rows` (asserted by the streaming unit tests). Two independent
/// caps bound memory: a *row* cap (`max_rows`, over completed rows) and a
/// *per-record byte* cap (`max_record_bytes`, over the in-progress record state —
/// the partial text line `pending`, or the growing CSV `csv_field`/`csv_row`).
/// The per-record cap closes the hole the row cap never sees: an unterminated
/// record — a multi-GB single-line blob loaded in text mode, or an unclosed
/// quoted CSV field — accumulates raw bytes with no completed row to trip the
/// row cap. Exceeding EITHER cap drops everything buffered and reports
/// `overflowed()` (with `overflow_reason()` naming which cap tripped), letting
/// the caller abort the COPY with zero rows applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyOverflow {
    /// More than `max_rows` completed rows were buffered.
    Rows,
    /// The in-progress record exceeded `max_record_bytes`.
    RecordBytes,
}

pub(crate) struct CopyStreamDecoder {
    format: CopyFormat,
    rows: Vec<Vec<Option<String>>>,
    /// Maximum rows to buffer; 0 = unlimited.
    max_rows: usize,
    /// Maximum bytes for a single in-progress record; 0 = unlimited.
    max_record_bytes: usize,
    /// Bytes accumulated for the current, not-yet-terminated record. Reset to 0
    /// at every record boundary (text newline / CSV unquoted newline).
    record_bytes: usize,
    overflow: bool,
    /// Which cap tripped `overflow`, for the caller's error message.
    overflow_kind: Option<CopyOverflow>,
    /// Text: raw bytes of the current partial line (no `\n` seen yet).
    /// CSV: only the incomplete-UTF8 tail (row/field state lives in the fields
    /// below).
    pending: Vec<u8>,
    /// Text: the `\.` end-of-data marker was seen — ignore all further input.
    text_done: bool,
    // CSV state — mirrors the locals of `parse_csv_rows` so a frame boundary
    // anywhere inside a record is transparent.
    csv_field: String,
    csv_row: Vec<Option<String>>,
    csv_in_quotes: bool,
    csv_was_quoted: bool,
    csv_field_started: bool,
    /// A `"` seen while inside a quoted field whose escaped-vs-closing meaning
    /// depends on the NEXT char — which may arrive in the next frame.
    csv_quote_pending: bool,
}

impl CopyStreamDecoder {
    pub(crate) fn new(format: CopyFormat, max_rows: usize, max_record_bytes: usize) -> Self {
        Self {
            format,
            rows: Vec::new(),
            max_rows,
            max_record_bytes,
            record_bytes: 0,
            overflow: false,
            overflow_kind: None,
            pending: Vec::new(),
            text_done: false,
            csv_field: String::new(),
            csv_row: Vec::new(),
            csv_in_quotes: false,
            csv_was_quoted: false,
            csv_field_started: false,
            csv_quote_pending: false,
        }
    }

    /// True once a memory cap tripped; the buffers have been dropped and the COPY
    /// must be aborted with no rows applied. `overflow_reason` names which cap.
    pub(crate) fn overflowed(&self) -> bool {
        self.overflow
    }

    /// Which memory cap tripped `overflowed`, or `None` if it has not.
    pub(crate) fn overflow_reason(&self) -> Option<CopyOverflow> {
        self.overflow_kind
    }

    /// Feed one `CopyData` frame's bytes.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        if self.overflow {
            return;
        }
        match self.format {
            CopyFormat::Csv => self.push_csv(chunk),
            // Text (and Binary, which handle_copy rejects before we are reached)
            // use the newline-delimited line decoder.
            _ => self.push_text(chunk),
        }
    }

    /// Consume all buffered rows, flushing any partial trailing record. After
    /// this the decoder is spent. Returns no rows when it has overflowed.
    pub(crate) fn finish(mut self) -> Vec<Vec<Option<String>>> {
        if self.overflow {
            return Vec::new();
        }
        match self.format {
            CopyFormat::Csv => self.finish_csv(),
            _ => self.finish_text(),
        }
        self.rows
    }

    /// Append a completed row, tripping the row cap (and releasing everything
    /// buffered) if it is exceeded.
    fn push_row(&mut self, row: Vec<Option<String>>) {
        if self.overflow {
            return;
        }
        self.rows.push(row);
        if self.max_rows != 0 && self.rows.len() > self.max_rows {
            self.trip_overflow(CopyOverflow::Rows);
        }
    }

    /// Account for `n` more bytes in the current in-progress record, tripping the
    /// per-record byte cap (and releasing everything buffered) if it is exceeded.
    /// Bounds the partial-record state the row cap never sees: the unterminated
    /// text line (`pending`) and the growing CSV `csv_field`/`csv_row`. Callers
    /// reset `record_bytes` to 0 at each record boundary (a completed row).
    fn bump_record_bytes(&mut self, n: usize) {
        if self.overflow || self.max_record_bytes == 0 {
            return;
        }
        self.record_bytes = self.record_bytes.saturating_add(n);
        if self.record_bytes > self.max_record_bytes {
            self.trip_overflow(CopyOverflow::RecordBytes);
        }
    }

    /// Trip the overflow flag, record which cap fired, and release every buffer
    /// (parsed rows AND in-progress record state) so a rejected COPY frees its
    /// memory immediately.
    fn trip_overflow(&mut self, kind: CopyOverflow) {
        self.overflow = true;
        self.overflow_kind = Some(kind);
        self.rows = Vec::new();
        self.pending = Vec::new();
        self.csv_field = String::new();
        self.csv_row = Vec::new();
    }

    // ── text format ──────────────────────────────────────────────────────────
    // Slices are `chunk[start..i]` / `chunk[start..]` with `start <= i < len`
    // maintained by the loop, so every range is in bounds by construction.
    #[allow(clippy::indexing_slicing)]
    fn push_text(&mut self, chunk: &[u8]) {
        if self.text_done {
            return;
        }
        let mut start = 0;
        for (i, &b) in chunk.iter().enumerate() {
            if b == b'\n' {
                // The bytes of this frame since the last terminator complete the
                // record ending here; a single over-cap line (even one delivered
                // whole, so `pending` is empty) must trip before we consume it.
                self.bump_record_bytes(i - start);
                if self.overflow {
                    return;
                }
                if self.pending.is_empty() {
                    self.consume_text_line(&chunk[start..i]);
                } else {
                    self.pending.extend_from_slice(&chunk[start..i]);
                    let line = std::mem::take(&mut self.pending);
                    self.consume_text_line(&line);
                }
                self.record_bytes = 0; // newline = record boundary
                start = i + 1;
                if self.text_done {
                    self.pending.clear();
                    return;
                }
            }
        }
        // Buffer the trailing partial line (no `\n` yet) for the next frame — the
        // unbounded-accumulation vector: count it before it grows `pending`.
        let tail = &chunk[start..];
        self.bump_record_bytes(tail.len());
        if self.overflow {
            return;
        }
        self.pending.extend_from_slice(tail);
    }

    fn finish_text(&mut self) {
        // A final record without a trailing newline is still a row (matches
        // `parse_text_rows` processing the last `split('\n')` segment).
        if !self.text_done && !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.consume_text_line(&line);
        }
    }

    /// Decode one text line (bytes between newlines, `\n` already stripped),
    /// mirroring the body of `parse_text_rows`' loop.
    fn consume_text_line(&mut self, line: &[u8]) {
        let cow = String::from_utf8_lossy(line);
        let s: &str = cow.strip_suffix('\r').unwrap_or(&cow);
        if s == "\\." {
            self.text_done = true;
            return;
        }
        if s.is_empty() {
            return;
        }
        let row: Vec<Option<String>> = s.split('\t').map(decode_text_field).collect();
        self.push_row(row);
    }

    // ── csv format ───────────────────────────────────────────────────────────
    fn push_csv(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
        let take = decodable_prefix_len(&self.pending);
        if take == 0 {
            return; // whole buffer is one still-incomplete UTF-8 character
        }
        let consumed: Vec<u8> = self.pending.drain(..take).collect();
        let text = String::from_utf8_lossy(&consumed);
        for c in text.chars() {
            self.feed_csv_char(c);
            if self.overflow {
                return;
            }
        }
    }

    fn finish_csv(&mut self) {
        // Flush any remaining incomplete-UTF8 tail lossily, exactly as the
        // whole-buffer `parse_csv_rows` would.
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            let text = String::from_utf8_lossy(&tail);
            for c in text.chars() {
                self.feed_csv_char(c);
                if self.overflow {
                    return;
                }
            }
        }
        // A `"` at end-of-stream closes the quoted field (no escaped-quote
        // partner followed) — matches `parse_csv_rows`' `peek() == None` arm.
        if self.csv_quote_pending {
            self.csv_quote_pending = false;
            self.csv_in_quotes = false;
        }
        // Trailing field/row when the data does not end with a newline.
        if self.csv_field_started || self.csv_was_quoted || !self.csv_row.is_empty() {
            let f = finish_csv_field(&self.csv_field, self.csv_was_quoted);
            self.csv_row.push(f);
            let row = std::mem::take(&mut self.csv_row);
            self.push_row(row);
        }
    }

    /// Advance the CSV state machine by one char — equivalent to one iteration of
    /// `parse_csv_rows`' loop, with that loop's `""`-escape lookahead expressed
    /// as the `csv_quote_pending` flag so it can straddle a frame boundary.
    fn feed_csv_char(&mut self, c: char) {
        // Every char either grows `csv_field`/`csv_row` or is structural; count
        // its bytes toward the in-progress record so an unclosed quoted field
        // (embedded newlines and all) or a comma-only row cannot grow without
        // limit. The count resets when a record terminates (the `'\n'` arm).
        self.bump_record_bytes(c.len_utf8());
        if self.overflow {
            return;
        }
        if self.csv_quote_pending {
            self.csv_quote_pending = false;
            if c == '"' {
                self.csv_field.push('"'); // "" -> a literal quote; stay in quotes
                return;
            }
            // The pending quote closed the field; handle `c` as an unquoted char.
            self.csv_in_quotes = false;
        } else if self.csv_in_quotes {
            if c == '"' {
                self.csv_quote_pending = true;
            } else {
                self.csv_field.push(c);
            }
            return;
        }
        match c {
            '"' if !self.csv_field_started => {
                self.csv_in_quotes = true;
                self.csv_was_quoted = true;
                self.csv_field_started = true;
            }
            ',' => {
                let f = finish_csv_field(&self.csv_field, self.csv_was_quoted);
                self.csv_row.push(f);
                self.csv_field.clear();
                self.csv_was_quoted = false;
                self.csv_field_started = false;
            }
            '\r' => {} // tolerate CRLF
            '\n' => {
                let f = finish_csv_field(&self.csv_field, self.csv_was_quoted);
                self.csv_row.push(f);
                let row = std::mem::take(&mut self.csv_row);
                self.push_row(row);
                self.csv_field.clear();
                self.csv_was_quoted = false;
                self.csv_field_started = false;
                self.record_bytes = 0; // unquoted newline = record boundary
            }
            other => {
                self.csv_field.push(other);
                self.csv_field_started = true;
            }
        }
    }
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

    #[test]
    fn encode_field_null_and_escapes() {
        assert_eq!(encode_text_field(None), "\\N");
        assert_eq!(encode_text_field(Some(b"plain")), "plain");
        assert_eq!(encode_text_field(Some(b"a\tb\nc")), "a\\tb\\nc");
        assert_eq!(encode_text_field(Some(b"a\\b")), "a\\\\b");
        assert_eq!(encode_text_field(Some(b"")), ""); // empty string, not NULL
    }

    #[test]
    fn encode_decode_roundtrip() {
        // a row encodes to a line that decodes back to the same fields
        let fields: Vec<Option<Vec<u8>>> =
            vec![Some(b"1".to_vec()), None, Some(b"has\ttab\nand nl".to_vec())];
        let line = encode_text_row(&fields);
        assert_eq!(line.last(), Some(&b'\n'));
        let rows = parse_text_rows(&line);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            vec![Some("1".to_string()), None, Some("has\ttab\nand nl".to_string())]
        );
    }

    #[test]
    fn csv_parse_quoting_and_null() {
        // unquoted empty = NULL; quoted "" = empty string; quoted field with a
        // comma, an escaped quote, and an embedded newline.
        let data = b"1,,\"\"\n2,\"a,b\",\"she said \"\"hi\"\"\"\n3,\"line1\nline2\",x\n";
        let rows = parse_csv_rows(data);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], vec![Some("1".into()), None, Some("".into())]);
        assert_eq!(
            rows[1],
            vec![Some("2".into()), Some("a,b".into()), Some("she said \"hi\"".into())]
        );
        assert_eq!(rows[2], vec![Some("3".into()), Some("line1\nline2".into()), Some("x".into())]);
    }

    #[test]
    fn csv_encode_quotes_when_needed() {
        assert_eq!(encode_csv_field(None), ""); // NULL
        assert_eq!(encode_csv_field(Some(b"plain")), "plain");
        assert_eq!(encode_csv_field(Some(b"a,b")), "\"a,b\"");
        assert_eq!(encode_csv_field(Some(b"she \"q\"")), "\"she \"\"q\"\"\"");
        assert_eq!(encode_csv_field(Some(b"l1\nl2")), "\"l1\nl2\"");
    }

    #[test]
    fn csv_encode_decode_roundtrip() {
        let fields: Vec<Option<Vec<u8>>> =
            vec![Some(b"1".to_vec()), None, Some(b"a,b\"c\nd".to_vec())];
        let line = encode_csv_row(&fields);
        let rows = parse_csv_rows(&line);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            vec![Some("1".to_string()), None, Some("a,b\"c\nd".to_string())]
        );
    }

    // ── streaming decode (W2.4) ──────────────────────────────────────────────

    /// Feed `data` to a `CopyStreamDecoder` broken at the given cut points and
    /// return the decoded rows. `splits` are absolute byte offsets; the segments
    /// between them (and the final remainder) are pushed as separate frames.
    #[allow(clippy::indexing_slicing)]
    fn stream_split(
        format: CopyFormat,
        data: &[u8],
        splits: &[usize],
    ) -> Vec<Vec<Option<String>>> {
        let mut d = CopyStreamDecoder::new(format, 0, 0);
        let mut prev = 0;
        for &s in splits {
            d.push(&data[prev..s]);
            prev = s;
        }
        d.push(&data[prev..]);
        d.finish()
    }

    #[test]
    fn stream_text_every_two_way_split_matches_batch() {
        // Frames split mid-line, mid-field, and mid-NULL-sentinel at every byte
        // offset must all reproduce the single-frame `parse_text_rows` result.
        let data = b"1\thello\n2\t\\N\n3\tworld\n";
        let want = parse_text_rows(data);
        for k in 0..=data.len() {
            assert_eq!(stream_split(CopyFormat::Text, data, &[k]), want, "split at {k}");
        }
    }

    #[test]
    fn stream_text_terminator_and_no_trailing_newline() {
        // `\.` end-of-data marker halts decoding wherever the frame boundary
        // falls, and a final record without a trailing newline is still a row.
        let term = b"1\ta\n2\tb\n\\.\n3\tignored";
        let want = parse_text_rows(term);
        assert_eq!(want.len(), 2); // stops at \.
        for k in 0..=term.len() {
            assert_eq!(stream_split(CopyFormat::Text, term, &[k]), want, "term split at {k}");
        }
        let no_nl = b"x\ty\nz\tw";
        let want2 = parse_text_rows(no_nl);
        for k in 0..=no_nl.len() {
            assert_eq!(stream_split(CopyFormat::Text, no_nl, &[k]), want2, "no-nl split at {k}");
        }
    }

    #[test]
    fn stream_csv_every_two_way_split_matches_batch() {
        // Splits fall inside quoted fields, on the "" escape, and inside embedded
        // newlines — the CSV quote/field state must carry across every boundary.
        let data = b"1,,\"\"\n2,\"a,b\",\"she said \"\"hi\"\"\"\n3,\"line1\nline2\",x\n";
        let want = parse_csv_rows(data);
        for k in 0..=data.len() {
            assert_eq!(stream_split(CopyFormat::Csv, data, &[k]), want, "split at {k}");
        }
    }

    #[test]
    fn stream_split_mid_utf8_char_matches_batch() {
        // é (2 bytes), € (3 bytes) and 😀 (4 bytes): a 2-way split at EVERY byte
        // offset lands inside each multibyte char. Wrong mid-UTF8 handling would
        // emit a premature U+FFFD and diverge from the batch oracle here.
        let text = "1\tcafé\n2\t€uro\n3\t😀smile\n".as_bytes();
        let want_t = parse_text_rows(text);
        for k in 0..=text.len() {
            assert_eq!(stream_split(CopyFormat::Text, text, &[k]), want_t, "text split at {k}");
        }
        let csv = "1,café\n2,\"€,uro\"\n3,😀smile\n".as_bytes();
        let want_c = parse_csv_rows(csv);
        for k in 0..=csv.len() {
            assert_eq!(stream_split(CopyFormat::Csv, csv, &[k]), want_c, "csv split at {k}");
        }
    }

    #[test]
    fn stream_byte_by_byte_matches_batch() {
        // The pathological one-byte-per-frame stream exercises the carry logic to
        // its limit for both formats.
        let text = b"a\tb\tc\n\\N\tx\ty\n";
        let mut dt = CopyStreamDecoder::new(CopyFormat::Text, 0, 0);
        for b in text {
            dt.push(std::slice::from_ref(b));
        }
        assert_eq!(dt.finish(), parse_text_rows(text));

        let csv = b"a,b,c\n\"q1\",\"q,2\",\"q\"\"3\"\n";
        let mut dc = CopyStreamDecoder::new(CopyFormat::Csv, 0, 0);
        for b in csv {
            dc.push(std::slice::from_ref(b));
        }
        assert_eq!(dc.finish(), parse_csv_rows(csv));
    }

    #[test]
    fn stream_row_cap_overflows_and_drops_rows() {
        let data = b"1\ta\n2\tb\n3\tc\n4\td\n";
        // Cap of 2: the 3rd row trips overflow; finish() yields zero rows.
        let mut over = CopyStreamDecoder::new(CopyFormat::Text, 2, 0);
        over.push(data);
        assert!(over.overflowed());
        assert_eq!(over.overflow_reason(), Some(CopyOverflow::Rows));
        assert!(over.finish().is_empty(), "overflow applies zero rows");
        // Exactly at the cap is allowed (overflow is strictly > max_rows).
        let two = b"1\ta\n2\tb\n";
        let mut ok = CopyStreamDecoder::new(CopyFormat::Text, 2, 0);
        ok.push(two);
        assert!(!ok.overflowed());
        assert_eq!(ok.finish().len(), 2);
        // 0 = unlimited.
        let mut unlimited = CopyStreamDecoder::new(CopyFormat::Text, 0, 0);
        unlimited.push(data);
        assert!(!unlimited.overflowed());
        assert_eq!(unlimited.finish().len(), 4);
    }

    #[test]
    fn stream_record_byte_cap_bounds_unterminated_records() {
        // The row cap (arg 2) never fires on an UNTERMINATED record — no row ever
        // completes — so the per-record byte cap (arg 3) is what bounds memory for
        // the OOM class this item retires. Every assertion below is false on
        // pre-change code, where no per-record cap exists (the constructor took no
        // such argument and the in-progress buffer grew without limit).

        // Text: a 1000-byte line with NO newline (the multi-GB single-line blob)
        // trips the 64-byte record cap even with an unlimited row cap.
        let blob = vec![b'a'; 1000];
        let mut d = CopyStreamDecoder::new(CopyFormat::Text, 0, 64);
        d.push(&blob);
        assert!(d.overflowed(), "unterminated 1000-byte line trips 64-byte record cap");
        assert_eq!(d.overflow_reason(), Some(CopyOverflow::RecordBytes));
        assert!(d.finish().is_empty(), "overflow applies zero rows");

        // CSV: an unclosed quoted field grows `csv_field` (embedded newlines and
        // all) without a completed row — the per-record cap must still catch it.
        let mut open_quote = vec![b'"']; // a lone opening quote, never closed …
        open_quote.resize(1001, b'x'); // … followed by 1000 field bytes.
        let mut c = CopyStreamDecoder::new(CopyFormat::Csv, 0, 64);
        c.push(&open_quote);
        assert!(c.overflowed(), "unclosed quoted field trips 64-byte record cap");
        assert_eq!(c.overflow_reason(), Some(CopyOverflow::RecordBytes));
        assert!(c.finish().is_empty());

        // Byte-at-a-time delivery trips at the same boundary (carry across frames).
        let mut bb = CopyStreamDecoder::new(CopyFormat::Text, 0, 64);
        for _ in 0..1000 {
            bb.push(b"a");
        }
        assert!(bb.overflowed(), "record cap carries the count across frames");

        // 0 = unlimited: the same blob decodes as one row, no trip.
        let mut unbounded = CopyStreamDecoder::new(CopyFormat::Text, 0, 0);
        unbounded.push(&blob);
        assert!(!unbounded.overflowed());
        assert_eq!(unbounded.finish().len(), 1);

        // The cap is PER-RECORD, not cumulative: many short rows whose SUM far
        // exceeds the cap never trip, because each newline resets the counter.
        let mut many = CopyStreamDecoder::new(CopyFormat::Text, 0, 8);
        for _ in 0..100 {
            many.push(b"ab\n"); // 2-byte record, well under the 8-byte cap
        }
        assert!(!many.overflowed(), "per-record cap resets at each record boundary");
        assert_eq!(many.finish().len(), 100);
    }
}
