//! A small lexical scanner over SQL text.
//!
//! It yields the words, numbers and punctuation of a statement with their
//! byte positions and parenthesis depth, and skips what cannot hold a keyword:
//! string literals (`'…'`, `E'…'`, `$tag$…$tag$`), quoted identifiers and
//! comments. The statements whose grammar the SQL parser does not have
//! (`MERGE … DO NOTHING … RETURNING`, `DECLARE … CURSOR`, `FETCH`, `MOVE`)
//! are split with it, so a keyword inside a literal is never taken for one.

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokKind {
    /// An unquoted word: a keyword or an identifier.
    Word,
    /// A quoted identifier (`"…"`).
    Quoted,
    /// A string literal of any spelling.
    Str,
    /// A number.
    Number,
    /// One punctuation character (or a multi-character operator's part).
    Punct,
}

/// One token of a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tok {
    /// What it is.
    pub kind: TokKind,
    /// Byte offset of its first character.
    pub start: usize,
    /// Byte offset one past its last character.
    pub end: usize,
    /// Parenthesis depth at the token (an opening parenthesis has the depth
    /// outside it, a closing one too).
    pub depth: usize,
}

impl Tok {
    /// The token's text.
    pub(crate) fn text<'a>(&self, sql: &'a str) -> &'a str {
        sql.get(self.start..self.end).unwrap_or("")
    }

    /// Is this the unquoted word `word` (ASCII case-insensitive)?
    pub(crate) fn is_word(&self, sql: &str, word: &str) -> bool {
        self.kind == TokKind::Word && self.text(sql).eq_ignore_ascii_case(word)
    }

    /// Is this the punctuation character `c`?
    pub(crate) fn is_punct(&self, sql: &str, c: char) -> bool {
        self.kind == TokKind::Punct && self.text(sql).starts_with(c)
    }
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_word_part(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The end of a `$tag$` opener starting at `start` (which holds `$`), if one
/// starts there: the offset one past its closing `$`.
fn dollar_tag_end(sql: &str, start: usize) -> Option<usize> {
    let rest = sql.get(start + 1..)?;
    for (offset, c) in rest.char_indices() {
        if c == '$' {
            return Some(start + 1 + offset + 1);
        }
        if !(c.is_alphanumeric() || c == '_') || (offset == 0 && c.is_ascii_digit()) {
            return None;
        }
    }
    None
}

/// Tokenize `sql`. An unterminated literal or comment runs to the end of the
/// text.
pub(crate) fn tokens(sql: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut chars = sql.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        let next = chars.peek().map(|&(_, n)| n);
        // Comments.
        if c == '-' && next == Some('-') {
            for (_, n) in chars.by_ref() {
                if n == '\n' {
                    break;
                }
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            chars.next();
            let mut nesting = 1usize;
            let mut previous = '\0';
            while let Some((_, n)) = chars.next() {
                if previous == '*' && n == '/' {
                    nesting -= 1;
                    if nesting == 0 {
                        break;
                    }
                    previous = '\0';
                    continue;
                }
                if previous == '/' && n == '*' {
                    nesting += 1;
                    previous = '\0';
                    continue;
                }
                previous = n;
            }
            continue;
        }
        // String literals.
        if c == '\'' {
            let end = scan_quoted(sql, &mut chars, '\'', false);
            out.push(Tok {
                kind: TokKind::Str,
                start,
                end,
                depth,
            });
            continue;
        }
        if (c == 'e' || c == 'E') && next == Some('\'') {
            chars.next();
            let end = scan_quoted(sql, &mut chars, '\'', true);
            out.push(Tok {
                kind: TokKind::Str,
                start,
                end,
                depth,
            });
            continue;
        }
        if c == '"' {
            let end = scan_quoted(sql, &mut chars, '"', false);
            out.push(Tok {
                kind: TokKind::Quoted,
                start,
                end,
                depth,
            });
            continue;
        }
        if c == '$' {
            if let Some(tag_end) = dollar_tag_end(sql, start) {
                let tag = sql.get(start..tag_end).unwrap_or("$$");
                let end = match sql.get(tag_end..).and_then(|rest| rest.find(tag)) {
                    Some(offset) => tag_end + offset + tag.len(),
                    None => sql.len(),
                };
                while chars.peek().is_some_and(|&(i, _)| i < end) {
                    chars.next();
                }
                out.push(Tok {
                    kind: TokKind::Str,
                    start,
                    end,
                    depth,
                });
                continue;
            }
        }
        if is_word_start(c) {
            let mut end = start + c.len_utf8();
            while let Some(&(i, n)) = chars.peek() {
                if is_word_part(n) {
                    end = i + n.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(Tok {
                kind: TokKind::Word,
                start,
                end,
                depth,
            });
            continue;
        }
        if c.is_ascii_digit() {
            let mut end = start + 1;
            while let Some(&(i, n)) = chars.peek() {
                if n.is_ascii_alphanumeric() || n == '.' {
                    // `1..10` is a range, not a number with two dots.
                    if n == '.' && sql.get(i + 1..i + 2) == Some(".") {
                        break;
                    }
                    end = i + 1;
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(Tok {
                kind: TokKind::Number,
                start,
                end,
                depth,
            });
            continue;
        }
        if c == ')' || c == ']' {
            depth = depth.saturating_sub(1);
        }
        out.push(Tok {
            kind: TokKind::Punct,
            start,
            end: start + c.len_utf8(),
            depth,
        });
        if c == '(' || c == '[' {
            depth += 1;
        }
    }
    out
}

/// Consume a quoted run whose opening quote was already taken; returns the
/// offset one past its closing quote. A doubled quote is an escaped quote;
/// with `backslash`, so is a backslash-escaped one.
fn scan_quoted(
    sql: &str,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    quote: char,
    backslash: bool,
) -> usize {
    while let Some((i, n)) = chars.next() {
        if backslash && n == '\\' {
            chars.next();
            continue;
        }
        if n == quote {
            if chars.peek().is_some_and(|&(_, m)| m == quote) {
                chars.next();
                continue;
            }
            return i + n.len_utf8();
        }
    }
    sql.len()
}

/// The value of an identifier token: a quoted one loses its quotes and keeps
/// its case, an unquoted one is lower-cased.
pub(crate) fn ident_value(tok: &Tok, sql: &str) -> String {
    let text = tok.text(sql);
    match tok.kind {
        TokKind::Quoted => text
            .strip_prefix('"')
            .and_then(|t| t.strip_suffix('"'))
            .unwrap_or(text)
            .replace("\"\"", "\""),
        _ => text.to_lowercase(),
    }
}

/// `sql` without leading whitespace and comments and without a trailing `;`.
pub(crate) fn statement_text(sql: &str) -> &str {
    let toks = tokens(sql);
    let Some(first) = toks.first() else {
        return "";
    };
    let mut end = sql.len();
    if let Some(last) = toks.last() {
        end = last.end;
        if last.is_punct(sql, ';') {
            end = last.start;
        }
    }
    sql.get(first.start..end).unwrap_or("").trim_end()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn words(sql: &str) -> Vec<String> {
        tokens(sql)
            .iter()
            .filter(|t| t.kind == TokKind::Word)
            .map(|t| t.text(sql).to_uppercase())
            .collect()
    }

    #[test]
    fn literals_and_comments_hold_no_words() {
        let sql = "SELECT 'THEN x', \"Then\", $q$ RETURNING $q$ -- when\n /* do /* nothing */ */ FROM t";
        assert_eq!(words(sql), vec!["SELECT", "FROM", "T"]);
    }

    #[test]
    fn depth_follows_parentheses() {
        let sql = "a (b (c) d) e";
        let depths: Vec<usize> = tokens(sql)
            .iter()
            .filter(|t| t.kind == TokKind::Word)
            .map(|t| t.depth)
            .collect();
        assert_eq!(depths, vec![0, 1, 2, 1, 0]);
    }

    #[test]
    fn a_positional_parameter_is_not_a_dollar_quote() {
        let sql = "x = $1 AND y = $2";
        assert_eq!(words(sql), vec!["X", "AND", "Y"]);
    }

    #[test]
    fn statement_text_drops_the_semicolon_and_leading_comment() {
        assert_eq!(statement_text("  -- c\n MERGE INTO t ; "), "MERGE INTO t");
    }

    #[test]
    fn escaped_quotes_stay_inside_the_literal() {
        let sql = "'it''s THEN' x E'a\\' THEN' y";
        assert_eq!(words(sql), vec!["X", "Y"]);
    }
}
