//! One statement scanner for the shell.
//!
//! The shell reads raw lines and hands them to [`StatementScanner`], which cuts
//! them into statements at the semicolons that are not inside a quote, a dollar
//! quote or a comment. It follows, rule for rule, the splitter the PostgreSQL
//! wire and `execute_sql_script` use
//! ([`pg_split_sql_respecting_quotes`](crate::protocol::postgres::handler)):
//!
//! * a single-quoted literal ends at the closing `'`; `''` is a quote inside it
//!   and a backslash escapes the next character;
//! * a double-quoted identifier ends at the closing `"`;
//! * `--` runs to the end of the line, `/* ... */` nests;
//! * `$tag$ ... $tag$` (the tag is `[A-Za-z0-9_]*`) is one block;
//! * `#` is not a comment: `#>` and `#>>` are the JSON path operators.
//!
//! The scanner is incremental. Lines can be fed one at a time, and the state
//! after each tells the shell which continuation prompt to show. Text the shell
//! has read is never altered: the statement keeps the whitespace of the literal
//! it contains.

use crate::protocol::postgres::handler::pg_skip_leading_comments;

/// Where the scanner is in the text it has read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanState {
    /// Outside any quote or comment.
    Normal,
    /// Inside a single-quoted literal.
    SingleQuote,
    /// Inside a double-quoted identifier.
    DoubleQuote,
    /// Inside a dollar-quoted block.
    Dollar,
    /// Inside a `--` comment (ends at the end of the line).
    LineComment,
    /// Inside a `/* ... */` comment.
    BlockComment,
}

/// Cuts raw input into statements.
#[derive(Debug, Clone)]
pub struct StatementScanner {
    /// The text of the statement being read, from its first character.
    buf: String,
    /// How much of `buf` has been scanned.
    pos: usize,
    state: ScanState,
    /// The tag of the open dollar quote.
    tag: String,
    /// Nesting depth of the open block comment.
    depth: usize,
    /// Statements completed and not yet returned.
    done: Vec<String>,
}

impl Default for StatementScanner {
    fn default() -> Self {
        Self::new()
    }
}

fn is_tag_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Byte length of the UTF-8 character that starts with `lead`.
fn char_len(lead: u8) -> usize {
    match lead {
        0xF0..=0xF7 => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

impl StatementScanner {
    /// A scanner that has read nothing.
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            pos: 0,
            state: ScanState::Normal,
            tag: String::new(),
            depth: 0,
            done: Vec::new(),
        }
    }

    /// Read `text` (any chunk of the input, a line or less) and return the
    /// statements it completed, in order, without their semicolon.
    pub fn feed(&mut self, text: &str) -> Vec<String> {
        self.buf.push_str(text);
        self.scan(false);
        std::mem::take(&mut self.done)
    }

    /// The input has ended: return the statements it completed and, last, the
    /// statement left without a terminator. An open quote or comment is
    /// returned as it stands; read [`state`](Self::state) before this to tell.
    pub fn finish(&mut self) -> Vec<String> {
        self.scan(true);
        self.push_statement(self.buf.len());
        self.buf.clear();
        self.pos = 0;
        self.state = ScanState::Normal;
        std::mem::take(&mut self.done)
    }

    /// Forget everything read (Ctrl-C).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.pos = 0;
        self.state = ScanState::Normal;
        self.tag.clear();
        self.depth = 0;
        self.done.clear();
    }

    /// Where the scanner is. After a line, a `LineComment` is over.
    pub fn state(&self) -> ScanState {
        match self.state {
            ScanState::LineComment => ScanState::Normal,
            other => other,
        }
    }

    /// Whether a statement is open: a quote or comment is open, or text other
    /// than whitespace and comments has been read since the last statement.
    pub fn is_open(&self) -> bool {
        match self.state {
            ScanState::Normal | ScanState::LineComment => !pg_skip_leading_comments(&self.buf).trim().is_empty(),
            _ => true,
        }
    }

    /// The continuation prompt that names the open construct.
    pub fn continuation_prompt(&self) -> &'static str {
        match self.state() {
            ScanState::Normal | ScanState::LineComment => "->",
            ScanState::SingleQuote => "'>",
            ScanState::DoubleQuote => "\">",
            ScanState::Dollar => "$>",
            ScanState::BlockComment => "*>",
        }
    }

    /// Name of the open construct, for the error at the end of input.
    pub fn open_construct(&self) -> Option<&'static str> {
        match self.state {
            ScanState::SingleQuote => Some("string literal"),
            ScanState::DoubleQuote => Some("quoted identifier"),
            ScanState::Dollar => Some("dollar-quoted string"),
            ScanState::BlockComment => Some("block comment"),
            ScanState::Normal | ScanState::LineComment => None,
        }
    }

    fn byte(&self, index: usize) -> Option<u8> {
        self.buf.as_bytes().get(index).copied()
    }

    /// Complete the statement `buf[..end]` unless it is empty or only comments.
    fn push_statement(&mut self, end: usize) {
        let Some(text) = self.buf.get(..end) else { return };
        let trimmed = text.trim();
        if !trimmed.is_empty() && !pg_skip_leading_comments(trimmed).trim().is_empty() {
            self.done.push(trimmed.to_string());
        }
    }

    /// Scan `buf` from `pos`. Unless `eof`, stop where the next byte decides
    /// what the current one means and has not been read yet.
    fn scan(&mut self, eof: bool) {
        let mut i = self.pos;
        loop {
            let Some(b) = self.byte(i) else { break };
            match self.state {
                ScanState::Dollar => {
                    if b == b'$' {
                        let close_len = self.tag.len() + 2;
                        if i + close_len > self.buf.len() {
                            if !eof {
                                break;
                            }
                        } else if self.buf.get(i + 1..i + 1 + self.tag.len()) == Some(self.tag.as_str())
                            && self.byte(i + 1 + self.tag.len()) == Some(b'$')
                        {
                            i += close_len;
                            self.state = ScanState::Normal;
                            continue;
                        }
                    }
                    i += 1;
                }
                ScanState::SingleQuote => {
                    if b == b'\'' {
                        match self.byte(i + 1) {
                            Some(b'\'') => i += 2,
                            Some(_) => {
                                self.state = ScanState::Normal;
                                i += 1;
                            }
                            None if eof => {
                                self.state = ScanState::Normal;
                                i += 1;
                            }
                            None => break,
                        }
                    } else if b == b'\\' {
                        match self.byte(i + 1) {
                            Some(next) => i += 1 + char_len(next),
                            None if eof => i += 1,
                            None => break,
                        }
                    } else {
                        i += 1;
                    }
                }
                ScanState::DoubleQuote => {
                    if b == b'"' {
                        self.state = ScanState::Normal;
                    }
                    i += 1;
                }
                ScanState::LineComment => {
                    if b == b'\n' {
                        self.state = ScanState::Normal;
                    }
                    i += 1;
                }
                ScanState::BlockComment => {
                    let next = self.byte(i + 1);
                    if (b == b'/' || b == b'*') && next.is_none() && !eof {
                        break;
                    }
                    if b == b'/' && next == Some(b'*') {
                        self.depth += 1;
                        i += 2;
                    } else if b == b'*' && next == Some(b'/') {
                        self.depth = self.depth.saturating_sub(1);
                        i += 2;
                        if self.depth == 0 {
                            self.state = ScanState::Normal;
                        }
                    } else {
                        i += 1;
                    }
                }
                ScanState::Normal => match b {
                    b'-' | b'/' => {
                        let next = self.byte(i + 1);
                        if next.is_none() && !eof {
                            break;
                        }
                        if b == b'-' && next == Some(b'-') {
                            self.state = ScanState::LineComment;
                            i += 2;
                        } else if b == b'/' && next == Some(b'*') {
                            self.state = ScanState::BlockComment;
                            self.depth = 1;
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    b'$' => {
                        let mut j = i + 1;
                        while self.byte(j).is_some_and(is_tag_byte) {
                            j += 1;
                        }
                        match self.byte(j) {
                            Some(b'$') => {
                                self.tag = self.buf.get(i + 1..j).unwrap_or("").to_string();
                                self.state = ScanState::Dollar;
                                i = j + 1;
                            }
                            Some(_) => i += 1,
                            None if eof => i += 1,
                            None => break,
                        }
                    }
                    b'\'' => {
                        self.state = ScanState::SingleQuote;
                        i += 1;
                    }
                    b'"' => {
                        self.state = ScanState::DoubleQuote;
                        i += 1;
                    }
                    b';' => {
                        self.push_statement(i);
                        self.buf.drain(..=i);
                        i = 0;
                    }
                    _ => i += 1,
                },
            }
        }
        self.pos = i;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use crate::protocol::postgres::handler::pg_split_sql_respecting_quotes;
    use proptest::prelude::*;

    /// All of `sql` at once.
    fn split(sql: &str) -> Vec<String> {
        let mut scanner = StatementScanner::new();
        let mut out = scanner.feed(sql);
        out.extend(scanner.finish());
        out
    }

    /// `sql` one line at a time, as the shell reads it.
    fn split_lines(sql: &str) -> Vec<String> {
        let mut scanner = StatementScanner::new();
        let mut out = Vec::new();
        for line in sql.split_inclusive('\n') {
            out.extend(scanner.feed(line));
        }
        out.extend(scanner.finish());
        out
    }

    /// `sql` one character at a time.
    fn split_chars(sql: &str) -> Vec<String> {
        let mut scanner = StatementScanner::new();
        let mut out = Vec::new();
        let mut buf = [0u8; 4];
        for ch in sql.chars() {
            out.extend(scanner.feed(ch.encode_utf8(&mut buf)));
        }
        out.extend(scanner.finish());
        out
    }

    /// The cases below: input, then the statements it must yield.
    #[allow(clippy::type_complexity)]
    fn cases() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            ("SELECT 1;", vec!["SELECT 1"]),
            ("SELECT 1", vec!["SELECT 1"]),
            ("SELECT 1; SELECT 2;", vec!["SELECT 1", "SELECT 2"]),
            ("SELECT 1 AS a; SELECT 2 AS b;", vec!["SELECT 1 AS a", "SELECT 2 AS b"]),
            ("SELECT 1;\nSELECT 2", vec!["SELECT 1", "SELECT 2"]),
            ("", vec![]),
            ("   \n\t ", vec![]),
            (";;;", vec![]),
            ("  ;  SELECT 1 ;", vec!["SELECT 1"]),
            // dollar quotes
            ("SELECT $$a;b$$;", vec!["SELECT $$a;b$$"]),
            ("SELECT $fn$ x $$ y; $fn$;", vec!["SELECT $fn$ x $$ y; $fn$"]),
            ("SELECT $a$ $b$ ; $b$ ; $a$; SELECT 2;", vec!["SELECT $a$ $b$ ; $b$ ; $a$", "SELECT 2"]),
            ("SELECT $1, $2;", vec!["SELECT $1, $2"]),
            ("SELECT $1; SELECT $2;", vec!["SELECT $1", "SELECT $2"]),
            ("SELECT a$b$c;", vec!["SELECT a$b$c;"]),
            (
                "CREATE FUNCTION f(x INT) RETURNS INT AS $$\nBEGIN\n  RETURN $1 + 1;\nEND;\n$$ LANGUAGE plpgsql;\nSELECT f(1);",
                vec![
                    "CREATE FUNCTION f(x INT) RETURNS INT AS $$\nBEGIN\n  RETURN $1 + 1;\nEND;\n$$ LANGUAGE plpgsql",
                    "SELECT f(1)",
                ],
            ),
            ("SELECT $$unterminated; more", vec!["SELECT $$unterminated; more"]),
            // single quotes
            ("SELECT 'a;b';", vec!["SELECT 'a;b'"]),
            ("INSERT INTO t VALUES (1, 'a;\nb');", vec!["INSERT INTO t VALUES (1, 'a;\nb')"]),
            ("SELECT 'it''s; ok'; SELECT 2;", vec!["SELECT 'it''s; ok'", "SELECT 2"]),
            ("SELECT 'a\\'b;c'; SELECT 2;", vec!["SELECT 'a\\'b;c'", "SELECT 2"]),
            ("SELECT 'x -- not a comment; y';", vec!["SELECT 'x -- not a comment; y'"]),
            ("SELECT '  indented\n    line';", vec!["SELECT '  indented\n    line'"]),
            ("SELECT 'open; SELECT 2;", vec!["SELECT 'open; SELECT 2;"]),
            ("SELECT 'café;ü'; SELECT 2;", vec!["SELECT 'café;ü'", "SELECT 2"]),
            ("SELECT '\\é'; SELECT 2;", vec!["SELECT '\\é'", "SELECT 2"]),
            // double quotes
            ("SELECT \"a;b\" FROM t;", vec!["SELECT \"a;b\" FROM t"]),
            ("SELECT \"it's\" FROM t; SELECT 2;", vec!["SELECT \"it's\" FROM t", "SELECT 2"]),
            // comments
            ("SELECT 1; -- done", vec!["SELECT 1"]),
            // A comment travels with the statement after it, as on the wire.
            ("SELECT 1; -- done\nSELECT 2;", vec!["SELECT 1", "-- done\nSELECT 2"]),
            ("-- only a comment", vec![]),
            ("-- c; with semicolon\nSELECT 1;", vec!["-- c; with semicolon\nSELECT 1"]),
            ("SELECT 1 -- ; here\n;", vec!["SELECT 1 -- ; here"]),
            ("/* a; b */ SELECT 1;", vec!["/* a; b */ SELECT 1"]),
            ("SELECT /* x\n; y */ 1;", vec!["SELECT /* x\n; y */ 1"]),
            ("/* a /* b */ ; */ SELECT 1;", vec!["/* a /* b */ ; */ SELECT 1"]),
            ("SELECT 1; /* trailing */", vec!["SELECT 1"]),
            ("SELECT 1 /* unterminated; SELECT 2;", vec!["SELECT 1 /* unterminated; SELECT 2;"]),
            ("-- widen to 24\" panel\nSELECT 1; SELECT 2;", vec!["-- widen to 24\" panel\nSELECT 1", "SELECT 2"]),
            ("-- it's\nSELECT 1; SELECT 2;", vec!["-- it's\nSELECT 1", "SELECT 2"]),
            // operators that look like comment starts
            ("SELECT a #> '{a,b}' FROM t;", vec!["SELECT a #> '{a,b}' FROM t"]),
            ("SELECT a #>> '{a,b}'; SELECT 2;", vec!["SELECT a #>> '{a,b}'", "SELECT 2"]),
            ("SELECT 4 - -1; SELECT 6 / 2;", vec!["SELECT 4 - -1", "SELECT 6 / 2"]),
            ("SELECT 1 -", vec!["SELECT 1 -"]),
            ("SELECT 1 /", vec!["SELECT 1 /"]),
            // line endings
            ("SELECT 1;\r\nSELECT 2;\r\n", vec!["SELECT 1", "SELECT 2"]),
            ("SELECT 'a\r\nb';", vec!["SELECT 'a\r\nb'"]),
            ("SELECT 1 -- c\r\n;", vec!["SELECT 1 -- c"]),
        ]
    }

    #[test]
    fn the_cases_split_as_listed() {
        let cases = cases();
        assert!(cases.len() >= 40, "{} cases", cases.len());
        for (input, expected) in cases {
            assert_eq!(split(input), expected, "whole: {input:?}");
        }
    }

    #[test]
    fn line_by_line_gives_the_same_statements() {
        for (input, expected) in cases() {
            assert_eq!(split_lines(input), expected, "by line: {input:?}");
        }
    }

    #[test]
    fn character_by_character_gives_the_same_statements() {
        for (input, expected) in cases() {
            assert_eq!(split_chars(input), expected, "by character: {input:?}");
        }
    }

    #[test]
    fn every_case_agrees_with_the_wire_splitter() {
        for (input, _) in cases() {
            assert_eq!(split(input), pg_split_sql_respecting_quotes(input), "{input:?}");
        }
    }

    #[test]
    fn a_complete_line_returns_its_statements_at_once() {
        let mut scanner = StatementScanner::new();
        assert_eq!(
            scanner.feed("SELECT 1 AS a; SELECT 2 AS b;\n"),
            vec!["SELECT 1 AS a", "SELECT 2 AS b"]
        );
        assert!(!scanner.is_open());
    }

    #[test]
    fn the_last_statement_without_a_semicolon_comes_at_finish() {
        let mut scanner = StatementScanner::new();
        assert!(scanner.feed("SELECT 1;\nSELECT 2\n").len() == 1);
        assert!(scanner.is_open());
        assert_eq!(scanner.finish(), vec!["SELECT 2"]);
        assert!(!scanner.is_open());
    }

    #[test]
    fn the_state_names_the_open_construct() {
        let mut scanner = StatementScanner::new();
        assert_eq!(scanner.state(), ScanState::Normal);
        scanner.feed("SELECT 'abc\n");
        assert_eq!(scanner.state(), ScanState::SingleQuote);
        assert_eq!(scanner.continuation_prompt(), "'>");
        assert_eq!(scanner.open_construct(), Some("string literal"));
        scanner.feed("def';\n");
        assert_eq!(scanner.state(), ScanState::Normal);
        assert_eq!(scanner.continuation_prompt(), "->");

        scanner.feed("SELECT $$ a\n");
        assert_eq!(scanner.state(), ScanState::Dollar);
        assert_eq!(scanner.continuation_prompt(), "$>");
        scanner.feed("$$;\n");
        assert_eq!(scanner.state(), ScanState::Normal);

        scanner.feed("SELECT /* a\n");
        assert_eq!(scanner.state(), ScanState::BlockComment);
        assert_eq!(scanner.continuation_prompt(), "*>");
        scanner.feed("*/ 1;\n");
        assert_eq!(scanner.state(), ScanState::Normal);

        scanner.feed("SELECT \"a\n");
        assert_eq!(scanner.state(), ScanState::DoubleQuote);
        assert_eq!(scanner.continuation_prompt(), "\">");
    }

    #[test]
    fn a_comment_line_does_not_open_a_statement() {
        let mut scanner = StatementScanner::new();
        scanner.feed("-- just a note\n");
        assert!(!scanner.is_open());
        scanner.feed("/* and another */\n");
        assert!(!scanner.is_open());
        scanner.feed("SELECT 1 -- note\n");
        assert!(scanner.is_open());
    }

    #[test]
    fn an_open_block_comment_is_open() {
        let mut scanner = StatementScanner::new();
        scanner.feed("/* started\n");
        assert!(scanner.is_open());
        assert_eq!(scanner.open_construct(), Some("block comment"));
    }

    #[test]
    fn reset_forgets_the_open_statement() {
        let mut scanner = StatementScanner::new();
        scanner.feed("SELECT 'abc\n");
        scanner.reset();
        assert!(!scanner.is_open());
        assert_eq!(scanner.feed("SELECT 1;\n"), vec!["SELECT 1"]);
    }

    #[test]
    fn inner_whitespace_of_a_literal_is_kept() {
        let out = split("INSERT INTO t VALUES ('  a\n\t b');");
        assert_eq!(out, vec!["INSERT INTO t VALUES ('  a\n\t b')"]);
    }

    /// Pieces that make the scanner change state, glued together at random.
    fn piece() -> impl Strategy<Value = &'static str> {
        prop::sample::select(vec![
            ";", "'", "''", "\"", "$$", "$a$", "$a", "$1", "$", "--", "-", "/*", "*/", "/", "*", "\n", "\r\n", " ",
            "x", "SELECT 1", "é", "\\", "#>", "#>>", "ab", "_", "9",
        ])
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn the_scanner_and_the_wire_splitter_agree(pieces in prop::collection::vec(piece(), 0..40)) {
            let sql: String = pieces.concat();
            let wire = pg_split_sql_respecting_quotes(&sql);
            prop_assert_eq!(split(&sql), wire.clone());
            prop_assert_eq!(split_lines(&sql), wire.clone());
            prop_assert_eq!(split_chars(&sql), wire);
        }
    }
}
