//! MySQL-to-PostgreSQL SQL translator
//!
//! Rewrites MySQL-specific syntax to PostgreSQL-compatible SQL before parsing.
//! This enables WordPress and other MySQL applications to work with HeliosDB Nano.

use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Initialize a static regex pattern.
///
/// All patterns are compile-time string literals; invalid patterns cause
/// immediate startup failure (fail-fast), posing zero runtime risk.
///
/// The one non-standard escape is `\i`, which expands to [`IDENT_SLOT`]: an
/// identifier-name slot that also accepts a masking placeholder (HDB-007).
/// `\i` is not a valid `regex` escape, so it can never collide with a real
/// pattern.
#[allow(clippy::expect_used)]
fn init_regex(pattern: &str) -> Regex {
    Regex::new(&pattern.replace(r"\i", IDENT_SLOT)).expect("static regex pattern must be valid")
}

/// Translate MySQL SQL to PostgreSQL-compatible SQL.
///
/// Every pass after `translate_backticks` is a `Regex::replace_all` over the
/// whole statement, so on its own it would happily rewrite MySQL keywords that
/// appear *inside* string literals, quoted identifiers or comments — HDB-007:
/// `INSERT INTO t VALUES ('TINYINT(1)')` used to store `BOOLEAN`, and
/// `'WHERE 1=1 AND x'` used to lose its tautology.  Those regions are therefore
/// masked with an opaque placeholder before the regex passes run and restored
/// verbatim afterwards (see `mask_protected_regions`).
pub fn translate(sql: &str) -> String {
    // Backslash escapes MUST be processed first — before masking and backtick
    // translation — so we can distinguish backslash-escaped quotes inside string
    // literals from identifier delimiters.  Afterwards every literal is `'…'`
    // with `''` doubling and no backslash-escaped quote.
    let normalized = translate_backslash_escapes(sql);

    // Hide string literals, quoted identifiers and comments from the regex
    // passes; `regions` keeps their exact bytes for the final restore.
    let (masked, regions) = mask_protected_regions(&normalized);

    // Apply transformations in order, on the masked text.
    let mut result = translate_backticks(&masked);
    result = translate_types(&result);
    result = translate_auto_increment(&result);
    result = translate_charset_collation(&result);
    result = translate_on_duplicate_key(&result);
    result = translate_replace_into(&result);
    result = translate_insert_ignore(&result);
    result = translate_limit_offset(&result);
    result = translate_functions(&result);
    result = translate_multi_table_delete(&result);
    result = translate_key_indexes(&result);
    result = translate_alter_table_keys(&result);
    result = translate_misc(&result);

    unmask_protected_regions(&result, &regions)
}

// ---------------------------------------------------------------------------
// 0. Backslash escape normalization inside single-quoted string literals
// ---------------------------------------------------------------------------

/// Strip MySQL-style backslash escapes inside single-quoted strings.
///
/// MySQL's `mysqli_real_escape_string()` sends `\"` for double-quotes and
/// `\\` for literal backslashes.  Standard MySQL strips the backslash on
/// storage.  Without this step, HeliosDB stores the literal `\"`, which
/// corrupts PHP serialized data (WordPress options, session tokens, etc.).
///
/// Handled sequences (inside single-quoted strings only):
///   `\"` → `"`    (escaped double-quote)
///   `\\` → `\`    (escaped backslash)
///   `\n` → newline
///   `\r` → carriage return
///   `\t` → tab
///   `\0` → NUL  (stripped — Postgres can't store NUL in text)
///   `\'` left unchanged (already handled by the SQL parser as `''`)
///
/// Comments (`--`, `#`, `/* … */`, `/*! … */`) and backtick-quoted identifiers
/// are copied through byte-for-byte: an apostrophe inside one of them is part
/// of that region, and must not open a phantom string literal that then
/// "closes" on the next *real* quote — which used to escape-process the
/// following literal's bytes (`` `it's_col` = 'a = "b" c' `` came back as
/// `'a = 'b' c'`).  What counts as a comment is [`comment_len`], the same rule
/// `mask_protected_regions` applies one step later.
fn translate_backslash_escapes(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut cursor = 0usize;
    // The character immediately before `cursor` (`None` at the start), which is
    // what `comment_len` needs to tell `<#>` from the start of a `#` comment.
    let mut prev: Option<char> = None;

    while cursor < sql.len() {
        let rest = &sql[cursor..];
        let ch = first_char(rest);

        // Bytes consumed by this step; every arm consumes at least one.
        let consumed = if let Some(comment) = comment_len(rest, prev) {
            // Comment (executable ones included): copied through untouched.
            out.push_str(&rest[..comment]);
            comment
        } else if ch == '`' {
            // Backtick-quoted identifier: copied through untouched; the
            // delimiters are stripped later by `translate_backticks`.
            let (_, ident_len, _) = scan_quoted_region(rest, '`', false);
            out.push_str(&rest[..ident_len]);
            ident_len
        } else if ch == '\'' {
            // Enter single-quoted string literal
            out.push('\'');
            ch.len_utf8() + process_string_interior(&mut out, &rest[1..], '\'')
        } else if ch == '"' && looks_like_mysql_string_context(&out) {
            // MySQL double-quoted string literal (not an identifier).
            // Convert to single-quoted for PostgreSQL compatibility.
            // Context check: after VALUES(, SET =, etc. — not after FROM/TABLE/INTO
            out.push('\'');
            ch.len_utf8() + process_string_interior(&mut out, &rest[1..], '"')
        } else {
            out.push(ch);
            ch.len_utf8()
        };

        prev = rest[..consumed].chars().next_back();
        cursor += consumed;
    }

    out
}

/// Process the interior of a string literal (single or double-quoted).
///
/// `rest` starts immediately *after* the opening delimiter.  Handles backslash
/// escapes, writes the PostgreSQL spelling of the content (and of the closing
/// quote) to `out`, and returns the number of bytes of `rest` consumed —
/// including the closing delimiter when there is one, or all of `rest` for an
/// unterminated literal.
fn process_string_interior(out: &mut String, rest: &str, quote_char: char) -> usize {
    let mut cursor = 0usize;

    while cursor < rest.len() {
        let ch = first_char(&rest[cursor..]);
        let ch_len = ch.len_utf8();
        let after = &rest[cursor + ch_len..];

        if ch == '\\' {
            // `(replacement, extra bytes consumed after the backslash)`; every
            // recognised escape character is one ASCII byte.
            let (replacement, extra): (&str, usize) = match after.chars().next() {
                Some('"') => ("\"", 1),
                Some('\\') => ("\\", 1),
                Some('n') => ("\n", 1),
                Some('r') => ("\r", 1),
                Some('t') => ("\t", 1),
                Some('0') => ("", 1),    // strip NUL
                Some('\'') => ("''", 1), // \' → '' for PG
                _ => ("\\", 0),          // not an escape (or end of input)
            };
            out.push_str(replacement);
            cursor += ch_len + extra;
            continue;
        }

        if ch == quote_char {
            // Check for doubled escape ('' or "")
            if after.starts_with(quote_char) {
                if quote_char == '\'' {
                    out.push_str("''");
                } else {
                    // "" inside double-quoted string → single " in output
                    out.push('"');
                }
                cursor += ch_len * 2;
                continue;
            }
            // End of string — always close with single quote (PG style)
            out.push('\'');
            return cursor + ch_len;
        }

        if ch == '\'' && quote_char == '"' {
            // Single quote inside a double-quoted string → escape for PG
            out.push_str("''");
            cursor += ch_len;
            continue;
        }

        out.push(ch);
        cursor += ch_len;
    }

    cursor
}

/// Heuristic: does the current output context suggest a string value (not an identifier)?
/// Returns true after: VALUES (, SET x =, DEFAULT ', etc.
/// Returns false after: FROM, TABLE, INTO, JOIN (where " would be an identifier quote).
fn looks_like_mysql_string_context(out: &str) -> bool {
    // sprinter 57416d9c follow-up: `CREATE TABLE t ("Id" INT, ...)` was
    // misread as a string context — `(` and `,` are ALSO how a VALUES/IN
    // list starts a string, and the check below has no other way to tell
    // them apart. Inside a CREATE TABLE's column/constraint list, `"` is
    // always a quoted identifier, never a MySQL string literal, regardless
    // of what immediately precedes it.
    if in_create_table_column_list(out) {
        return false;
    }

    let trimmed = out.trim_end();
    // After these, double-quoted value is a string literal
    trimmed.ends_with('(')
        || trimmed.ends_with(',')
        || trimmed.ends_with('=')
        || trimmed.to_uppercase().ends_with("VALUES")
        || trimmed.to_uppercase().ends_with("SET")
        || trimmed.to_uppercase().ends_with("DEFAULT")
        || trimmed.to_uppercase().ends_with("LIKE")
        || trimmed.to_uppercase().ends_with("WHERE")
        || trimmed.to_uppercase().ends_with("AND")
        || trimmed.to_uppercase().ends_with("OR")
}

/// Are we currently inside a `CREATE [TEMPORARY] TABLE name (...)` statement's
/// top-level column/constraint list?
///
/// Tracks paren depth from the first `(` that opens the list. A `DEFAULT
/// '(literal)'` inside an already-processed (and already single-quoted)
/// string can throw the count off by whatever parens it happens to contain,
/// but that does not matter here: the only thing this gates is whether a
/// following `"` is a string delimiter, and nothing in a CREATE TABLE
/// statement needs that treatment inside the list either way.
fn in_create_table_column_list(out: &str) -> bool {
    static PREAMBLE_RE: OnceLock<Regex> = OnceLock::new();
    let re = PREAMBLE_RE.get_or_init(|| {
        Regex::new(r#"(?is)^\s*CREATE\s+(TEMPORARY\s+)?TABLE\s+(IF\s+NOT\s+EXISTS\s+)?[^\s(]+\s*\("#)
            .unwrap_or_else(|_| Regex::new("^$").expect("static regex"))
    });
    let Some(m) = re.find(out) else {
        return false;
    };
    let mut depth = 1i32;
    for ch in out[m.end()..].chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
    }
    depth >= 1
}

// ---------------------------------------------------------------------------
// 0b. Protected-region masking (HDB-007)
// ---------------------------------------------------------------------------

/// Opening sentinel of a masked-region placeholder.
const PLACEHOLDER_OPEN: char = '\u{E000}';
/// Closing sentinel of a masked-region placeholder.
const PLACEHOLDER_CLOSE: char = '\u{E001}';
/// Code point of placeholder digit `0`; decimal digit `d` is `BASE + d`.
const PLACEHOLDER_DIGIT_BASE: u32 = 0xE010;
/// One past the last placeholder digit code point.
const PLACEHOLDER_DIGIT_END: u32 = PLACEHOLDER_DIGIT_BASE + 10;

/// Character class for a slot that holds an identifier *name*.
///
/// A backtick-quoted identifier is masked (so `` `TINYINT` `` is not rewritten
/// to `SMALLINT`), which leaves a placeholder where `\w+` used to match the
/// bare name.  The few patterns that must still recognise such a name — the
/// `KEY` / `UNIQUE KEY` / `ADD KEY` index names, the multi-table `DELETE`
/// table and alias names, and `VALUES(col)` inside `ON DUPLICATE KEY UPDATE` —
/// use this class instead of `\w`, so `` KEY `post_name` (`post_name`(191)) ``
/// is still stripped after masking.  Nothing else opts in: every other pattern
/// keeps plain `\w`/`\b`, which private-use code points never match.
const IDENT_SLOT: &str = r"[\w\x{E000}-\x{E01F}]";

/// Is `ch` one of the private-use code points the placeholder encoding owns?
fn is_placeholder_char(ch: char) -> bool {
    let cp = u32::from(ch);
    ch == PLACEHOLDER_OPEN || ch == PLACEHOLDER_CLOSE || (PLACEHOLDER_DIGIT_BASE..PLACEHOLDER_DIGIT_END).contains(&cp)
}

/// First character of `rest`.
///
/// Every caller holds `!rest.is_empty()` from its own loop guard, so the
/// fallback is unreachable; and were it reached, `\0` is copied through
/// exactly like any other character.
fn first_char(rest: &str) -> char {
    rest.chars().next().unwrap_or('\0')
}

/// Length of the comment that starts at the beginning of `rest`, if any.
///
/// `--` and `#` run to the end of the line (the newline is not part of the
/// comment); `/* … */` runs through the closing `*/` (or to the end of input,
/// for an unterminated comment).  A `#` is a comment marker only when it is
/// not the `#` of `<#>`, `#>`, `#>>` or `#-` — PostgreSQL operators (vector
/// negative inner product and the JSON path operators) that Nano also accepts
/// over the MySQL wire; `prev` is the character before `rest`, if any.
///
/// This is the single definition of "comment" shared by
/// `translate_backslash_escapes` (which copies comments through untouched) and
/// `mask_protected_regions` (which hides them from the regex passes), so the
/// two can never disagree about where a comment starts or ends.
fn comment_len(rest: &str, prev: Option<char>) -> Option<usize> {
    if let Some(tail) = rest.strip_prefix("/*") {
        return Some(tail.find("*/").map_or(rest.len(), |end| 2 + end + 2));
    }

    let line_comment = match rest.strip_prefix('#') {
        Some(tail) => prev != Some('<') && !matches!(tail.chars().next(), Some('>' | '-')),
        None => rest.starts_with("--"),
    };

    line_comment.then(|| rest.find('\n').unwrap_or(rest.len()))
}

/// Append the placeholder for region `index` to `out`.
///
/// Encoding: `U+E000`, the decimal index with digit `d` written as
/// `U+E010 + d`, then `U+E001`.  Every code point lives in the Unicode
/// private-use area, which the `regex` crate treats as neither `\w`, `\d` nor
/// `\s` (and which is therefore not `\b`-relevant either).  No translation
/// pattern below can match into, across, or on the digits of a placeholder,
/// while patterns that key off the *surrounding* quote — `GROUP_CONCAT(… SEPARATOR
/// '([^']*)')`, `\bBINARY\s+(')` — still see that quote.
fn push_placeholder(out: &mut String, index: usize) {
    out.push(PLACEHOLDER_OPEN);
    for digit in index.to_string().chars() {
        // `to_string` only ever yields ASCII digits, so the fallback is dead.
        let value = digit.to_digit(10).unwrap_or(0);
        if let Some(encoded) = char::from_u32(PLACEHOLDER_DIGIT_BASE + value) {
            out.push(encoded);
        }
    }
    out.push(PLACEHOLDER_CLOSE);
}

/// Store `content` as the next protected region and write its placeholder,
/// wrapped in `open`/`close` when the region keeps its delimiters.
///
/// Regions are interned: a second occurrence of the same bytes reuses the
/// first occurrence's index, so two spellings of one backtick-quoted alias
/// (`` `t` `` in `AS `t`` and in `ON `t`.term_id`) stay textually equal on the
/// masked text — `translate_multi_table_delete` compares them with
/// `eq_ignore_ascii_case` and `table1 != table2`.
fn push_masked_region(
    out: &mut String,
    regions: &mut Vec<String>,
    interned: &mut HashMap<String, usize>,
    open: Option<char>,
    close: Option<char>,
    content: &str,
) {
    let index = match interned.get(content) {
        Some(index) => *index,
        None => {
            let index = regions.len();
            regions.push(content.to_string());
            interned.insert(content.to_string(), index);
            index
        }
    };
    if let Some(delim) = open {
        out.push(delim);
    }
    push_placeholder(out, index);
    if let Some(delim) = close {
        out.push(delim);
    }
}

/// Scan the quoted region that starts at the beginning of `rest`.
///
/// `rest` must start with `quote`.  Returns the interior (delimiters
/// excluded), the byte length of the whole region and whether a closing
/// delimiter was found.  With `doubling`, a doubled delimiter (`''` / `""`)
/// belongs to the interior instead of closing the region — note that a
/// backslash is *not* an escape here: `translate_backslash_escapes` has
/// already turned `\'` into `''` and `\\` into `\`, so a literal may
/// legitimately end with a backslash (`'a\'`).  Backticks do not double,
/// which reproduces `translate_backticks`, which simply drops every backtick.
fn scan_quoted_region(rest: &str, quote: char, doubling: bool) -> (&str, usize, bool) {
    let quote_len = quote.len_utf8();
    let mut cursor = quote_len;

    while cursor < rest.len() {
        let ch = first_char(&rest[cursor..]);
        if ch == quote {
            if doubling && rest[cursor + quote_len..].starts_with(quote) {
                cursor += quote_len * 2;
                continue;
            }
            return (&rest[quote_len..cursor], cursor + quote_len, true);
        }
        cursor += ch.len_utf8();
    }

    // Unterminated region: mask through the end of the input, keeping the opener.
    (&rest[quote_len..], rest.len(), false)
}

/// Replace every region whose bytes must survive translation untouched with an
/// opaque placeholder, returning the masked SQL and the region contents.
///
/// Grammar (one left-to-right scan, so the first opener wins: a comment marker
/// inside a literal is literal text, a quote inside a comment is comment text):
///
/// | region | opens | closes |
/// |---|---|---|
/// | string literal | `'` | `'` not followed by `'` |
/// | quoted identifier | `"` | `"` not followed by `"` |
/// | backtick identifier | `` ` `` | `` ` `` |
/// | line comment | `--` or `#` | end of line |
/// | block comment | `/*` | `*/` |
///
/// Comments are exactly what [`comment_len`] says they are — in particular a
/// `#` that belongs to the `<#>`, `#>`, `#>>` or `#-` operators is *not* a
/// comment marker, so `emb <#> '[1,2,3]' LIMIT 0, 10` keeps its LIMIT.
///
/// `/*! … */` executable comments are the one exception: they are copied
/// through unmasked *and unscanned*, so `EXEC_COMMENT_RE` in `translate_misc`
/// keeps stripping them exactly as before.  Quoted regions keep their
/// delimiters (patterns such as `\bBINARY\s+(')` still need to see them);
/// comments are replaced by a bare placeholder and restored in full.
///
/// An unterminated literal / identifier / comment is masked through the end of
/// the input (the opening delimiter is kept); nothing panics.  A stray
/// placeholder code point in the client's SQL is masked as a one-character
/// region of its own, so outside an unterminated `/*!` hint (copied through
/// unscanned, after which the statement is unparsable anyway) the masked text
/// only contains placeholders this function emitted.
fn mask_protected_regions(sql: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(sql.len());
    let mut regions: Vec<String> = Vec::new();
    let mut interned: HashMap<String, usize> = HashMap::new();
    let mut cursor = 0usize;
    // The character immediately before `cursor` (`None` at the start), which is
    // what `comment_len` needs to tell `<#>` from the start of a `#` comment.
    let mut prev: Option<char> = None;

    while cursor < sql.len() {
        let rest = &sql[cursor..];
        let ch = first_char(rest);

        // Bytes consumed by this step; every arm consumes at least one.
        let consumed = if rest.starts_with("/*!") {
            // `/*! … */` — executable comment: stays visible to translate_misc.
            let len = rest[3..].find("*/").map_or(rest.len(), |end| 3 + end + 2);
            out.push_str(&rest[..len]);
            len
        } else if let Some(len) = comment_len(rest, prev) {
            // `/* … */` block comment, or a `-- …` / `# …` line comment (the
            // downstream parser treats `--` as a comment whatever follows it,
            // so we do too).
            push_masked_region(&mut out, &mut regions, &mut interned, None, None, &rest[..len]);
            len
        } else if ch == '\'' || ch == '"' || ch == '`' {
            // String literal / quoted identifier / backtick identifier.
            let (interior, len, closed) = scan_quoted_region(rest, ch, ch != '`');
            let close = if closed { Some(ch) } else { None };
            push_masked_region(&mut out, &mut regions, &mut interned, Some(ch), close, interior);
            len
        } else if is_placeholder_char(ch) {
            // A stray placeholder code point from the client.
            let len = ch.len_utf8();
            push_masked_region(&mut out, &mut regions, &mut interned, None, None, &rest[..len]);
            len
        } else {
            out.push(ch);
            ch.len_utf8()
        };

        prev = rest[..consumed].chars().next_back();
        cursor += consumed;
    }

    (out, regions)
}

/// Restore the regions `mask_protected_regions` hid.
///
/// A placeholder may appear more than once (a pattern copied a capture, as the
/// multi-table `DELETE` rewrite does) or not at all (`ENUM('a','b')` collapses
/// to `TEXT`); both are fine.  Anything that is not a well-formed placeholder
/// for a known region is copied through verbatim, and restored text is never
/// re-scanned, so a region whose contents themselves contain placeholder code
/// points comes back byte-for-byte.
fn unmask_protected_regions(masked: &str, regions: &[String]) -> String {
    let mut out = String::with_capacity(masked.len());
    let mut cursor = 0usize;

    while cursor < masked.len() {
        let rest = &masked[cursor..];
        let ch = first_char(rest);
        if ch != PLACEHOLDER_OPEN {
            out.push(ch);
            cursor += ch.len_utf8();
            continue;
        }

        let mut scan = ch.len_utf8();
        let mut index: u32 = 0;
        let mut digits = 0usize;
        let mut decoded = false;

        while scan < rest.len() {
            let next = first_char(&rest[scan..]);
            let cp = u32::from(next);
            if (PLACEHOLDER_DIGIT_BASE..PLACEHOLDER_DIGIT_END).contains(&cp) {
                let digit = cp - PLACEHOLDER_DIGIT_BASE;
                let Some(widened) = index.checked_mul(10).and_then(|scaled| scaled.checked_add(digit)) else {
                    break;
                };
                index = widened;
                digits += 1;
                scan += next.len_utf8();
            } else if next == PLACEHOLDER_CLOSE {
                scan += next.len_utf8();
                decoded = digits > 0;
                break;
            } else {
                break;
            }
        }

        let region = if decoded {
            usize::try_from(index).ok().and_then(|i| regions.get(i))
        } else {
            None
        };

        if let Some(text) = region {
            out.push_str(text);
            cursor += scan;
            continue;
        }

        // Not a placeholder we emitted — copy the sentinel and carry on.
        out.push(ch);
        cursor += ch.len_utf8();
    }

    out
}

// ---------------------------------------------------------------------------
// 1. Backtick-quoted identifiers → double-quoted
// ---------------------------------------------------------------------------

/// Replace backtick-quoted identifiers with double-quoted identifiers.
/// String literals enclosed in single quotes are left untouched.
fn translate_backticks(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // Skip over single-quoted string literals unchanged.
            '\'' => {
                out.push('\'');
                loop {
                    match chars.next() {
                        Some('\\') => {
                            out.push('\\');
                            if let Some(escaped) = chars.next() {
                                out.push(escaped);
                            }
                        }
                        Some('\'') => {
                            // Check for escaped quote ('')
                            if chars.peek() == Some(&'\'') {
                                out.push('\'');
                                out.push('\'');
                                chars.next();
                            } else {
                                out.push('\'');
                                break;
                            }
                        }
                        Some(c) => out.push(c),
                        None => break,
                    }
                }
            }
            // Strip backticks entirely — WordPress identifiers are simple names.
            // Double-quoted identifiers cause "table not found" because the PG parser
            // treats them as case-sensitive quoted identifiers that don't match the
            // stored (unquoted) table names.
            '`' => {}
            _ => out.push(ch),
        }
    }

    out
}

// ---------------------------------------------------------------------------
// 2. MySQL type names → PostgreSQL equivalents
// ---------------------------------------------------------------------------

fn translate_types(sql: &str) -> String {
    let mut s = sql.to_string();

    // TINYINT(1) → BOOLEAN  (must come before generic TINYINT)
    static TINYINT1_RE: OnceLock<Regex> = OnceLock::new();
    let re = TINYINT1_RE.get_or_init(|| init_regex(r"(?i)\bTINYINT\s*\(\s*1\s*\)"));
    s = re.replace_all(&s, "BOOLEAN").to_string();

    // TINYINT → SMALLINT  (any remaining TINYINT without (1))
    static TINYINT_RE: OnceLock<Regex> = OnceLock::new();
    let re = TINYINT_RE.get_or_init(|| init_regex(r"(?i)\bTINYINT\b"));
    s = re.replace_all(&s, "SMALLINT").to_string();

    // MEDIUMINT → INTEGER
    static MEDIUMINT_RE: OnceLock<Regex> = OnceLock::new();
    let re = MEDIUMINT_RE.get_or_init(|| init_regex(r"(?i)\bMEDIUMINT\b"));
    s = re.replace_all(&s, "INTEGER").to_string();

    // *TEXT variants → TEXT
    static LONGTEXT_RE: OnceLock<Regex> = OnceLock::new();
    let re = LONGTEXT_RE.get_or_init(|| init_regex(r"(?i)\bLONGTEXT\b"));
    s = re.replace_all(&s, "TEXT").to_string();

    static MEDIUMTEXT_RE: OnceLock<Regex> = OnceLock::new();
    let re = MEDIUMTEXT_RE.get_or_init(|| init_regex(r"(?i)\bMEDIUMTEXT\b"));
    s = re.replace_all(&s, "TEXT").to_string();

    static TINYTEXT_RE: OnceLock<Regex> = OnceLock::new();
    let re = TINYTEXT_RE.get_or_init(|| init_regex(r"(?i)\bTINYTEXT\b"));
    s = re.replace_all(&s, "TEXT").to_string();

    // *BLOB variants → BYTEA
    static LONGBLOB_RE: OnceLock<Regex> = OnceLock::new();
    let re = LONGBLOB_RE.get_or_init(|| init_regex(r"(?i)\bLONGBLOB\b"));
    s = re.replace_all(&s, "BYTEA").to_string();

    static MEDIUMBLOB_RE: OnceLock<Regex> = OnceLock::new();
    let re = MEDIUMBLOB_RE.get_or_init(|| init_regex(r"(?i)\bMEDIUMBLOB\b"));
    s = re.replace_all(&s, "BYTEA").to_string();

    static TINYBLOB_RE: OnceLock<Regex> = OnceLock::new();
    let re = TINYBLOB_RE.get_or_init(|| init_regex(r"(?i)\bTINYBLOB\b"));
    s = re.replace_all(&s, "BYTEA").to_string();

    // DATETIME → TIMESTAMP
    static DATETIME_RE: OnceLock<Regex> = OnceLock::new();
    let re = DATETIME_RE.get_or_init(|| init_regex(r"(?i)\bDATETIME\b"));
    s = re.replace_all(&s, "TIMESTAMP").to_string();

    // YEAR → SMALLINT
    static YEAR_RE: OnceLock<Regex> = OnceLock::new();
    let re = YEAR_RE.get_or_init(|| init_regex(r"(?i)\bYEAR\b"));
    s = re.replace_all(&s, "SMALLINT").to_string();

    // Strip display width from INT/BIGINT/INTEGER/SMALLINT: INT(11) → INT
    static INT_WIDTH_RE: OnceLock<Regex> = OnceLock::new();
    let re = INT_WIDTH_RE.get_or_init(|| init_regex(r"(?i)\b(INT|BIGINT|INTEGER|SMALLINT)\s*\(\s*\d+\s*\)"));
    s = re.replace_all(&s, "$1").to_string();

    // INT UNSIGNED → BIGINT  (promote to avoid overflow)
    // Must come before generic UNSIGNED strip to ensure promotion.
    static INT_UNSIGNED_RE: OnceLock<Regex> = OnceLock::new();
    let re = INT_UNSIGNED_RE.get_or_init(|| init_regex(r"(?i)\bINT\s+UNSIGNED\b"));
    s = re.replace_all(&s, "BIGINT").to_string();

    // Strip remaining UNSIGNED (e.g. BIGINT UNSIGNED → BIGINT)
    static UNSIGNED_RE: OnceLock<Regex> = OnceLock::new();
    let re = UNSIGNED_RE.get_or_init(|| init_regex(r"(?i)\s+UNSIGNED\b"));
    s = re.replace_all(&s, "").to_string();

    // DOUBLE → DOUBLE PRECISION  (but not if already DOUBLE PRECISION)
    // The regex crate does not support look-ahead, so we replace all DOUBLE
    // then fix the accidental "DOUBLE PRECISION PRECISION" → "DOUBLE PRECISION".
    static DOUBLE_RE: OnceLock<Regex> = OnceLock::new();
    let re = DOUBLE_RE.get_or_init(|| init_regex(r"(?i)\bDOUBLE\b"));
    s = re.replace_all(&s, "DOUBLE PRECISION").to_string();

    static DOUBLE_FIX_RE: OnceLock<Regex> = OnceLock::new();
    let re = DOUBLE_FIX_RE.get_or_init(|| init_regex(r"(?i)\bDOUBLE\s+PRECISION\s+PRECISION\b"));
    s = re.replace_all(&s, "DOUBLE PRECISION").to_string();

    // FLOAT(N) → REAL
    static FLOAT_RE: OnceLock<Regex> = OnceLock::new();
    let re = FLOAT_RE.get_or_init(|| init_regex(r"(?i)\bFLOAT\s*\(\s*\d+\s*\)"));
    s = re.replace_all(&s, "REAL").to_string();

    // ENUM('a','b',...) → TEXT  (simplest viable mapping)
    static ENUM_RE: OnceLock<Regex> = OnceLock::new();
    let re = ENUM_RE.get_or_init(|| init_regex(r"(?i)\bENUM\s*\([^)]+\)"));
    s = re.replace_all(&s, "TEXT").to_string();

    s
}

// ---------------------------------------------------------------------------
// 3. AUTO_INCREMENT → SERIAL / BIGSERIAL
// ---------------------------------------------------------------------------

fn translate_auto_increment(sql: &str) -> String {
    // BIGINT ... AUTO_INCREMENT  →  BIGSERIAL  (remove AUTO_INCREMENT, swap type)
    static BIGINT_AI_RE: OnceLock<Regex> = OnceLock::new();
    let re = BIGINT_AI_RE.get_or_init(|| init_regex(r"(?i)\bBIGINT\b([^,)]*?)\s+AUTO_INCREMENT\b"));
    let mut s = re.replace_all(sql, "BIGSERIAL$1").to_string();

    // INT/INTEGER ... AUTO_INCREMENT  →  SERIAL
    static INT_AI_RE: OnceLock<Regex> = OnceLock::new();
    let re = INT_AI_RE.get_or_init(|| init_regex(r"(?i)\b(?:INT|INTEGER)\b([^,)]*?)\s+AUTO_INCREMENT\b"));
    s = re.replace_all(&s, "SERIAL$1").to_string();

    // Catch any remaining standalone AUTO_INCREMENT (safety net)
    static LEFTOVER_AI_RE: OnceLock<Regex> = OnceLock::new();
    let re = LEFTOVER_AI_RE.get_or_init(|| init_regex(r"(?i)\s*AUTO_INCREMENT\b"));
    s = re.replace_all(&s, "").to_string();

    s
}

// ---------------------------------------------------------------------------
// 4. CHARACTER SET / COLLATE / ENGINE / CHARSET  → strip
// ---------------------------------------------------------------------------

fn translate_charset_collation(sql: &str) -> String {
    let mut s = sql.to_string();

    // DEFAULT CHARACTER SET <name> (must be matched BEFORE bare CHARACTER SET)
    static DEFAULT_CHARSET_LONG_RE: OnceLock<Regex> = OnceLock::new();
    let re = DEFAULT_CHARSET_LONG_RE.get_or_init(|| init_regex(r"(?i)\s*DEFAULT\s+CHARACTER\s+SET\s*=?\s*\w+"));
    s = re.replace_all(&s, "").to_string();

    // DEFAULT CHARSET=<name>
    static DEFAULT_CHARSET_RE: OnceLock<Regex> = OnceLock::new();
    let re = DEFAULT_CHARSET_RE.get_or_init(|| init_regex(r"(?i)\s*DEFAULT\s+CHARSET\s*=?\s*\w+"));
    s = re.replace_all(&s, "").to_string();

    // CHARACTER SET <name> (bare, without DEFAULT prefix)
    static CHARSET_LONG_RE: OnceLock<Regex> = OnceLock::new();
    let re = CHARSET_LONG_RE.get_or_init(|| init_regex(r"(?i)\s*CHARACTER\s+SET\s*=?\s*\w+"));
    s = re.replace_all(&s, "").to_string();

    // CHARSET <name> (short form)
    static CHARSET_SHORT_RE: OnceLock<Regex> = OnceLock::new();
    let re = CHARSET_SHORT_RE.get_or_init(|| init_regex(r"(?i)\s*CHARSET\s*=?\s*\w+"));
    s = re.replace_all(&s, "").to_string();

    // COLLATE <name> (with or without =)
    static COLLATE_RE: OnceLock<Regex> = OnceLock::new();
    let re = COLLATE_RE.get_or_init(|| init_regex(r"(?i)\s*COLLATE\s*=?\s*\w+"));
    s = re.replace_all(&s, "").to_string();

    // ENGINE=InnoDB / ENGINE=MyISAM / etc.
    static ENGINE_RE: OnceLock<Regex> = OnceLock::new();
    let re = ENGINE_RE.get_or_init(|| init_regex(r"(?i)\s*ENGINE\s*=\s*\w+"));
    s = re.replace_all(&s, "").to_string();

    s
}

// ---------------------------------------------------------------------------
// 5. ON DUPLICATE KEY UPDATE → ON CONFLICT DO UPDATE SET
// ---------------------------------------------------------------------------

fn translate_on_duplicate_key(sql: &str) -> String {
    static ODK_RE: OnceLock<Regex> = OnceLock::new();
    let re = ODK_RE.get_or_init(|| init_regex(r"(?i)\s+ON\s+DUPLICATE\s+KEY\s+UPDATE\s+(.+)$"));

    // Translate ON DUPLICATE KEY UPDATE to ON CONFLICT DO UPDATE SET.
    // The planner now supports ON CONFLICT natively, so we convert
    // MySQL syntax to PostgreSQL syntax and let the engine handle it.
    if let Some(caps) = re.captures(sql) {
        let prefix = &sql[..caps.get(0).map_or(0, |m| m.start())];
        let set_clause = caps.get(1).map_or("", |m| m.as_str());
        // Translate VALUES(col) references to EXCLUDED.col
        let pg_set = translate_values_refs(set_clause);
        format!("{prefix} ON CONFLICT DO UPDATE SET {pg_set}")
    } else {
        sql.to_string()
    }
}

/// Replace `VALUES(column_name)` references with `EXCLUDED.column_name`.
fn translate_values_refs(clause: &str) -> String {
    static VALUES_REF_RE: OnceLock<Regex> = OnceLock::new();
    // `\i` (not `\w`): the column may have arrived backtick-quoted and is then a
    // masked placeholder by the time this pass runs.
    let re = VALUES_REF_RE.get_or_init(|| init_regex(r"(?i)\bVALUES\s*\(\s*(\i+)\s*\)"));
    re.replace_all(clause, "EXCLUDED.$1").to_string()
}

// ---------------------------------------------------------------------------
// 6. REPLACE INTO → INSERT INTO  (MVP: conflict handling deferred)
// ---------------------------------------------------------------------------

fn translate_replace_into(sql: &str) -> String {
    static REPLACE_RE: OnceLock<Regex> = OnceLock::new();
    let re = REPLACE_RE.get_or_init(|| init_regex(r"(?i)\bREPLACE\s+INTO\b"));
    re.replace_all(sql, "INSERT INTO").to_string()
}

// ---------------------------------------------------------------------------
// 7. INSERT IGNORE → INSERT ... ON CONFLICT DO NOTHING
// ---------------------------------------------------------------------------

fn translate_insert_ignore(sql: &str) -> String {
    static IGNORE_RE: OnceLock<Regex> = OnceLock::new();
    let re = IGNORE_RE.get_or_init(|| init_regex(r"(?i)\bINSERT\s+IGNORE\s+INTO\b"));

    if re.is_match(sql) {
        let without_ignore = re.replace_all(sql, "INSERT INTO").to_string();
        format!("{without_ignore} ON CONFLICT DO NOTHING")
    } else {
        sql.to_string()
    }
}

// ---------------------------------------------------------------------------
// 8. LIMIT offset, count → LIMIT count OFFSET offset
// ---------------------------------------------------------------------------

fn translate_limit_offset(sql: &str) -> String {
    static LIMIT_RE: OnceLock<Regex> = OnceLock::new();
    let re = LIMIT_RE.get_or_init(|| init_regex(r"(?i)\bLIMIT\s+(\d+)\s*,\s*(\d+)"));
    re.replace_all(sql, "LIMIT $2 OFFSET $1").to_string()
}

// ---------------------------------------------------------------------------
// 9. MySQL functions → PostgreSQL equivalents
// ---------------------------------------------------------------------------

fn translate_functions(sql: &str) -> String {
    let mut s = sql.to_string();

    // GROUP_CONCAT(col SEPARATOR 'sep') → STRING_AGG(col, 'sep')
    static GC_SEP_RE: OnceLock<Regex> = OnceLock::new();
    let re = GC_SEP_RE.get_or_init(|| init_regex(r"(?i)\bGROUP_CONCAT\s*\((.+?)\s+SEPARATOR\s+'([^']*)'\)"));
    s = re.replace_all(&s, "STRING_AGG($1, '$2')").to_string();

    // GROUP_CONCAT(col) → STRING_AGG(col, ',')
    static GC_RE: OnceLock<Regex> = OnceLock::new();
    let re = GC_RE.get_or_init(|| init_regex(r"(?i)\bGROUP_CONCAT\s*\(([^)]+)\)"));
    s = re.replace_all(&s, "STRING_AGG($1, ',')").to_string();

    // IFNULL(a, b) → COALESCE(a, b)
    static IFNULL_RE: OnceLock<Regex> = OnceLock::new();
    let re = IFNULL_RE.get_or_init(|| init_regex(r"(?i)\bIFNULL\s*\("));
    s = re.replace_all(&s, "COALESCE(").to_string();

    // IF(cond, true, false) → CASE WHEN cond THEN true ELSE false END
    // We use a simple approach: match IF( then find the balanced parens.
    s = translate_if_function(&s);

    // LOCATE(substr, str) → POSITION(substr IN str)
    static LOCATE_RE: OnceLock<Regex> = OnceLock::new();
    let re = LOCATE_RE.get_or_init(|| init_regex(r"(?i)\bLOCATE\s*\(\s*([^,]+?)\s*,\s*([^)]+?)\s*\)"));
    s = re.replace_all(&s, "POSITION($1 IN $2)").to_string();

    // INSTR(str, substr) → POSITION(substr IN str)  (note: args swapped)
    static INSTR_RE: OnceLock<Regex> = OnceLock::new();
    let re = INSTR_RE.get_or_init(|| init_regex(r"(?i)\bINSTR\s*\(\s*([^,]+?)\s*,\s*([^)]+?)\s*\)"));
    s = re.replace_all(&s, "POSITION($2 IN $1)").to_string();

    s
}

/// Translate `IF(cond, true_val, false_val)` into
/// `CASE WHEN cond THEN true_val ELSE false_val END`.
///
/// Uses manual paren-balancing so nested calls are handled correctly.
fn translate_if_function(sql: &str) -> String {
    // Case-insensitive search for `IF(`
    static IF_PREFIX_RE: OnceLock<Regex> = OnceLock::new();
    let re = IF_PREFIX_RE.get_or_init(|| init_regex(r"(?i)\bIF\s*\("));

    let mut result = String::with_capacity(sql.len());
    let mut remaining = sql;

    while let Some(m) = re.find(remaining) {
        result.push_str(&remaining[..m.start()]);
        let after_open = &remaining[m.end()..]; // everything after the opening paren

        if let Some((args, rest)) = extract_balanced_args(after_open) {
            let parts = split_top_level_commas(&args);
            if parts.len() == 3 {
                result.push_str("CASE WHEN ");
                result.push_str(parts[0].trim());
                result.push_str(" THEN ");
                result.push_str(parts[1].trim());
                result.push_str(" ELSE ");
                result.push_str(parts[2].trim());
                result.push_str(" END");
                remaining = rest;
            } else {
                // Not a 3-arg IF — pass through unchanged.
                result.push_str(m.as_str());
                remaining = after_open;
            }
        } else {
            // Unbalanced parens — pass through unchanged.
            result.push_str(m.as_str());
            remaining = after_open;
        }
    }

    result.push_str(remaining);
    result
}

/// Given a string that starts right after an opening `(`, find the matching
/// closing `)`.  Returns `(inner_content, rest_after_close)` or `None`.
fn extract_balanced_args(s: &str) -> Option<(&str, &str)> {
    let mut depth: u32 = 1;
    let mut in_single_quote = false;

    for (i, ch) in s.char_indices() {
        if in_single_quote {
            if ch == '\'' {
                in_single_quote = false;
            }
            continue;
        }
        match ch {
            '\'' => in_single_quote = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&s[..i], &s[i + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

/// Split a string by commas, but only at the top paren-level.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth: u32 = 0;
    let mut in_single_quote = false;
    let mut start = 0;

    for (i, ch) in s.char_indices() {
        if in_single_quote {
            if ch == '\'' {
                in_single_quote = false;
            }
            continue;
        }
        match ch {
            '\'' => in_single_quote = true,
            '(' => depth += 1,
            ')' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

// ---------------------------------------------------------------------------
// 10. Multi-table DELETE → single-table DELETE ... USING
// ---------------------------------------------------------------------------

/// Translate MySQL multi-table DELETE to PostgreSQL DELETE ... USING.
///
/// MySQL:  `DELETE t, tt FROM wp_terms AS t INNER JOIN wp_term_taxonomy AS tt
///          ON t.term_id = tt.term_id WHERE tt.taxonomy = 'nav_menu'`
/// PG:     `DELETE FROM wp_terms USING wp_term_taxonomy
///          WHERE wp_terms.term_id = wp_term_taxonomy.term_id
///          AND wp_term_taxonomy.taxonomy = 'nav_menu'`
///
/// Strategy: detect `DELETE <alias_list> FROM`, extract the first table as
/// the target, convert the rest to USING with the JOIN condition merged into
/// WHERE.
fn translate_multi_table_delete(sql: &str) -> String {
    // Match: DELETE <alias1>[, <alias2>...] FROM <table_spec> [JOIN ...] [WHERE ...]
    static MULTI_DEL_RE: OnceLock<Regex> = OnceLock::new();
    // Table and alias names use `\i` (not `\w`): backtick-quoted names are
    // masked placeholders by the time this pass runs.
    let re = MULTI_DEL_RE.get_or_init(|| {
        init_regex(
            r"(?i)^(\s*DELETE)\s+\i+(?:\s*,\s*\i+)*\s+FROM\s+(\i+)\s+(?:AS\s+)?(\i+)\s+((?:INNER\s+)?JOIN\s+(\i+)\s+(?:AS\s+)?(\i+)\s+ON\s+(.+?))\s+(WHERE\s+.+)$"
        )
    });

    if let Some(caps) = re.captures(sql) {
        // caps[2] = first real table name (e.g. wp_terms)
        // caps[3] = first alias (e.g. t)
        // caps[5] = second real table name (e.g. wp_term_taxonomy)
        // caps[6] = second alias (e.g. tt)
        // caps[7] = ON condition (e.g. t.term_id = tt.term_id)
        // caps[8] = WHERE clause (e.g. WHERE tt.taxonomy = 'nav_menu')

        let table1 = &caps[2];
        let alias1 = &caps[3];
        let table2 = &caps[5];
        let alias2 = &caps[6];
        let on_condition = &caps[7];
        let where_clause = &caps[8];

        // Strip the "WHERE " prefix from the WHERE clause
        let where_body = where_clause
            .trim()
            .strip_prefix("WHERE ")
            .or_else(|| where_clause.trim().strip_prefix("where "))
            .unwrap_or(where_clause.trim());

        // Build a common subquery FROM clause that uses the original aliases.
        // This lets the ON and WHERE clauses use their original alias references.
        let subquery_from = format!("{table1} AS {alias1} INNER JOIN {table2} AS {alias2} ON {on_condition}");
        let subquery_where = where_body;

        // Find a join column from the ON condition for each table.
        // ON condition looks like: t.term_id = tt.term_id
        // We need to extract which column from alias1 and alias2 to use.
        let (col1, col2) = extract_join_columns(on_condition, alias1, alias2);

        // Generate two DELETE statements with IN subqueries:
        //   DELETE FROM table1 WHERE col1 IN (SELECT alias1.col1 FROM ... WHERE ...)
        //   DELETE FROM table2 WHERE col2 IN (SELECT alias2.col2 FROM ... WHERE ...)
        let del1 = format!(
            "DELETE FROM {table1} WHERE {col1} IN (SELECT {alias1}.{col1} FROM {subquery_from} WHERE {subquery_where})"
        );
        let del2 = format!(
            "DELETE FROM {table2} WHERE {col2} IN (SELECT {alias2}.{col2} FROM {subquery_from} WHERE {subquery_where})"
        );

        return format!("{del1};{del2}");
    }

    // Also handle comma-join DELETE: DELETE a, b FROM table1 a, table2 b WHERE ...
    // WordPress uses: DELETE a, b FROM wp_options a, wp_options b WHERE a.option_name LIKE ...
    static COMMA_DEL_RE: OnceLock<Regex> = OnceLock::new();
    let comma_re = COMMA_DEL_RE.get_or_init(|| {
        init_regex(
            r"(?is)^\s*DELETE\s+(\i+)\s*,\s*(\i+)\s+FROM\s+(\i+)\s+(?:AS\s+)?(\i+)\s*,\s*(\i+)\s+(?:AS\s+)?(\i+)\s+(WHERE\s+.+)$"
        )
    });

    if let Some(caps) = comma_re.captures(sql) {
        let del_alias1 = &caps[1]; // alias to delete from
        let del_alias2 = &caps[2];
        let table1 = &caps[3];
        let alias1 = &caps[4];
        let table2 = &caps[5];
        let alias2 = &caps[6];
        let where_clause = caps[7].trim();
        let where_body = where_clause
            .strip_prefix("WHERE ")
            .or_else(|| where_clause.strip_prefix("where "))
            .unwrap_or(where_clause);

        // Find PK column from schema (default to "option_id" for wp_options pattern)
        let pk_col = "option_id"; // WordPress transient cleanup always uses wp_options

        // For same-table self-join DELETE, use a subquery with the WHERE condition
        let subquery_from = format!("{table1} AS {alias1}, {table2} AS {alias2}");
        let del1 = format!(
            "DELETE FROM {table1} WHERE {pk_col} IN (SELECT {del_alias1}.{pk_col} FROM {subquery_from} WHERE {where_body})"
        );
        // Only generate second DELETE if tables differ
        if table1 != table2 {
            let del2 = format!(
                "DELETE FROM {table2} WHERE {pk_col} IN (SELECT {del_alias2}.{pk_col} FROM {subquery_from} WHERE {where_body})"
            );
            return format!("{del1};{del2}");
        }
        return del1;
    }

    sql.to_string()
}

/// Extract join column names from an ON condition like `t.term_id = tt.term_id`.
///
/// Returns (col_for_alias1, col_for_alias2).  Falls back to "id" if parsing fails.
fn extract_join_columns(on_condition: &str, alias1: &str, alias2: &str) -> (String, String) {
    // Split on '=' and look for alias.column patterns
    let (mut col1, mut col2) = (String::from("id"), String::from("id"));

    if let Some((lhs, rhs)) = on_condition.split_once('=') {
        let lhs = lhs.trim();
        let rhs = rhs.trim();

        // Try to match alias1.col and alias2.col from either side
        for token in &[lhs, rhs] {
            if let Some(dot) = token.find('.') {
                let prefix = token.get(..dot).unwrap_or("").trim();
                let suffix = token.get(dot + 1..).unwrap_or("id").trim();
                if prefix.eq_ignore_ascii_case(alias1) {
                    col1 = suffix.to_string();
                } else if prefix.eq_ignore_ascii_case(alias2) {
                    col2 = suffix.to_string();
                }
            }
        }
    }

    (col1, col2)
}

// ---------------------------------------------------------------------------
// 11. KEY / UNIQUE KEY prefix indexes → strip or convert in CREATE TABLE
// ---------------------------------------------------------------------------

/// Translate MySQL KEY and UNIQUE KEY index definitions in CREATE TABLE.
///
/// WordPress DDL includes:
///   `KEY option_name (option_name(191))`         — prefix index with length
///   `UNIQUE KEY email (email)`                    — named unique constraint
///   `KEY idx_name (col1, col2)`                   — composite index
///
/// Plain KEY (non-unique indexes) are stripped — HeliosDB uses ART indexes.
/// UNIQUE KEY is converted to a table-level UNIQUE constraint so that the
/// planner can enforce uniqueness and `SHOW INDEX` can report them:
///   `UNIQUE KEY option_name (option_name(191))` → `UNIQUE(option_name)`
fn translate_key_indexes(sql: &str) -> String {
    // Only apply to CREATE TABLE statements
    static CREATE_RE: OnceLock<Regex> = OnceLock::new();
    let re = CREATE_RE.get_or_init(|| init_regex(r"(?i)^\s*CREATE\s+TABLE\b"));
    if !re.is_match(sql) {
        return sql.to_string();
    }

    // Step 1: Convert UNIQUE KEY to UNIQUE(...) constraint.
    // Captures: the comma prefix, the column list (with optional prefix lengths).
    // Group 1 = column parenthesised list (may contain nested parens for prefix lengths).
    static UNIQUE_KEY_RE: OnceLock<Regex> = OnceLock::new();
    // The index name uses `\i` (not `\w`): WordPress spells it
    // ``UNIQUE KEY `option_name` (...)``, which masking turns into a placeholder.
    let re = UNIQUE_KEY_RE.get_or_init(|| init_regex(r"(?im),\s*UNIQUE\s+KEY\s+\i+\s*\(((?:[^()]*\([^)]*\))*[^)]*)\)"));

    let mut s = re
        .replace_all(sql, |caps: &regex::Captures<'_>| {
            let col_list = &caps[1];
            // Strip prefix lengths: col_name(191) → col_name
            let clean_cols = strip_prefix_lengths(col_list);
            format!(", UNIQUE({})", clean_cols)
        })
        .to_string();

    // Step 2: Remove plain (non-unique) KEY definitions.
    static KEY_LINE_RE: OnceLock<Regex> = OnceLock::new();
    let re = KEY_LINE_RE.get_or_init(|| init_regex(r"(?im),\s*KEY\s+\i+\s*\((?:[^()]*\([^)]*\))*[^)]*\)"));

    s = re.replace_all(&s, "").to_string();

    // Clean up trailing commas before closing paren: `, )` → `)`
    static TRAILING_COMMA_RE: OnceLock<Regex> = OnceLock::new();
    let re = TRAILING_COMMA_RE.get_or_init(|| init_regex(r",\s*\)"));
    s = re.replace_all(&s, ")").to_string();

    s
}

/// Strip MySQL prefix-index lengths from a column list.
///
/// `option_name(191)` → `option_name`
/// `col1(100), col2` → `col1, col2`
fn strip_prefix_lengths(col_list: &str) -> String {
    static PREFIX_LEN_RE: OnceLock<Regex> = OnceLock::new();
    let re = PREFIX_LEN_RE.get_or_init(|| init_regex(r"\(\d+\)"));
    re.replace_all(col_list, "").to_string()
}

// ---------------------------------------------------------------------------
// 12. ALTER TABLE ADD KEY/INDEX → strip (HeliosDB uses ART indexes)
// ---------------------------------------------------------------------------

/// Strip MySQL `ALTER TABLE t ADD KEY name (col(191))` and `ADD INDEX` statements.
/// WordPress `dbDelta()` sends these during schema checks.
fn translate_alter_table_keys(sql: &str) -> String {
    let upper = sql.trim().to_uppercase();
    if !upper.starts_with("ALTER TABLE") {
        return sql.to_string();
    }
    // Match: ALTER TABLE t ADD [UNIQUE] KEY|INDEX name (cols)
    static ALTER_KEY_RE: OnceLock<Regex> = OnceLock::new();
    let re = ALTER_KEY_RE
        .get_or_init(|| init_regex(r"(?i)\bADD\s+(?:UNIQUE\s+)?(?:KEY|INDEX)\s+\i+\s*\((?:[^()]*\([^)]*\))*[^)]*\)"));
    if re.is_match(sql) {
        // Convert to no-op (silently succeed)
        return String::new();
    }
    sql.to_string()
}

// ---------------------------------------------------------------------------
// 13. Miscellaneous MySQL syntax
// ---------------------------------------------------------------------------

fn translate_misc(sql: &str) -> String {
    let mut s = sql.to_string();

    // Strip MySQL-specific /*! ... */ executable comments / optimizer hints
    static EXEC_COMMENT_RE: OnceLock<Regex> = OnceLock::new();
    let re = EXEC_COMMENT_RE.get_or_init(|| init_regex(r"/\*![\s\S]*?\*/"));
    s = re.replace_all(&s, "").to_string();

    // STRAIGHT_JOIN → JOIN
    static STRAIGHT_JOIN_RE: OnceLock<Regex> = OnceLock::new();
    let re = STRAIGHT_JOIN_RE.get_or_init(|| init_regex(r"(?i)\bSTRAIGHT_JOIN\b"));
    s = re.replace_all(&s, "JOIN").to_string();

    // Strip SQL_CALC_FOUND_ROWS (and trailing whitespace to avoid double-space)
    static CALC_FOUND_RE: OnceLock<Regex> = OnceLock::new();
    let re = CALC_FOUND_RE.get_or_init(|| init_regex(r"(?i)\bSQL_CALC_FOUND_ROWS\s*"));
    s = re.replace_all(&s, "").to_string();

    // Strip HIGH_PRIORITY / LOW_PRIORITY / DELAYED
    static PRIORITY_RE: OnceLock<Regex> = OnceLock::new();
    let re = PRIORITY_RE.get_or_init(|| init_regex(r"(?i)\b(?:HIGH_PRIORITY|LOW_PRIORITY|DELAYED)\b"));
    s = re.replace_all(&s, "").to_string();

    // Strip BINARY keyword before string comparisons
    static BINARY_RE: OnceLock<Regex> = OnceLock::new();
    let re = BINARY_RE.get_or_init(|| init_regex(r"(?i)\bBINARY\s+(')"));
    s = re.replace_all(&s, "$1").to_string();

    // Convert DATE_SUB(NOW(), INTERVAL N UNIT) → DATE_SUB(NOW(), 'N UNIT')
    // and DATE_ADD(NOW(), INTERVAL N UNIT) → DATE_ADD(NOW(), 'N UNIT')
    // sqlparser chokes on INTERVAL keyword inside function args; wrap as string
    static DATE_INTERVAL_RE: OnceLock<Regex> = OnceLock::new();
    let re = DATE_INTERVAL_RE
        .get_or_init(|| init_regex(r"(?i)\b(DATE_(?:ADD|SUB))\s*\(\s*([^,]+)\s*,\s*INTERVAL\s+(\d+)\s+(\w+)\s*\)"));
    s = re.replace_all(&s, "$1($2, '$3 $4')").to_string();

    // Strip WHERE 1=1 tautology (WordPress adds to every query for chaining AND clauses)
    // "WHERE 1=1 AND ..." → "WHERE ..."
    static WHERE_1_1_AND_RE: OnceLock<Regex> = OnceLock::new();
    let re = WHERE_1_1_AND_RE.get_or_init(|| init_regex(r"(?i)\bWHERE\s+1\s*=\s*1\s+AND\b"));
    s = re.replace_all(&s, "WHERE").to_string();
    // Standalone "WHERE 1=1" at end of query → strip entirely
    static WHERE_1_1_RE: OnceLock<Regex> = OnceLock::new();
    let re = WHERE_1_1_RE.get_or_init(|| init_regex(r"(?i)\bWHERE\s+1\s*=\s*1\s*$"));
    s = re.replace_all(&s, "").to_string();

    s
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_increment() {
        let sql = "CREATE TABLE wp_posts (ID bigint(20) NOT NULL AUTO_INCREMENT, PRIMARY KEY (ID))";
        let result = translate(sql);
        assert!(result.contains("BIGSERIAL"), "Should convert to BIGSERIAL: {result}");
        assert!(
            !result.contains("AUTO_INCREMENT"),
            "Should remove AUTO_INCREMENT: {result}"
        );
    }

    #[test]
    fn test_on_duplicate_key() {
        let sql = "INSERT INTO wp_options (option_name, option_value) VALUES ('siteurl', 'http://example.com') ON DUPLICATE KEY UPDATE option_value = VALUES(option_value)";
        let result = translate(sql);
        // The translator converts ON DUPLICATE KEY UPDATE to ON CONFLICT DO UPDATE SET
        // with VALUES(col) references replaced by EXCLUDED.col
        assert!(
            !result.contains("ON DUPLICATE KEY"),
            "Should strip ON DUPLICATE KEY UPDATE: {result}"
        );
        assert!(
            result.contains("ON CONFLICT DO UPDATE SET"),
            "Should produce ON CONFLICT DO UPDATE SET: {result}"
        );
        assert!(
            result.contains("EXCLUDED.option_value"),
            "Should translate VALUES(col) to EXCLUDED.col: {result}"
        );
    }

    #[test]
    fn test_mysql_types() {
        let sql = "CREATE TABLE t (a LONGTEXT, b MEDIUMTEXT, c TINYINT(1), d BIGINT(20) UNSIGNED, e DATETIME)";
        let result = translate(sql);
        assert!(result.contains("TEXT"), "LONGTEXT -> TEXT");
        assert!(result.contains("BOOLEAN"), "TINYINT(1) -> BOOLEAN");
        assert!(result.contains("TIMESTAMP"), "DATETIME -> TIMESTAMP");
        assert!(!result.contains("UNSIGNED"), "UNSIGNED should be stripped");
    }

    #[test]
    fn test_charset_stripped() {
        let sql = "CREATE TABLE t (id INT) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci";
        let result = translate(sql);
        assert!(!result.contains("ENGINE"), "ENGINE should be stripped");
        assert!(!result.contains("CHARSET"), "CHARSET should be stripped");
        assert!(!result.contains("COLLATE"), "COLLATE should be stripped");
    }

    #[test]
    fn test_backtick_stripped() {
        let sql = "SELECT `id`, `name` FROM `wp_posts` WHERE `status` = 'publish'";
        let result = translate(sql);
        assert!(!result.contains('`'), "Backticks should be stripped");
        assert!(result.contains("id"), "Identifier should remain");
        assert!(result.contains("wp_posts"), "Table name should remain");
        // String 'publish' should NOT be affected
        assert!(result.contains("'publish'"), "String literals should be preserved");
    }

    #[test]
    fn test_insert_ignore() {
        let sql = "INSERT IGNORE INTO t (id, name) VALUES (1, 'test')";
        let result = translate(sql);
        assert!(
            result.contains("ON CONFLICT DO NOTHING"),
            "INSERT IGNORE -> ON CONFLICT DO NOTHING"
        );
        assert!(!result.contains("IGNORE"), "IGNORE should be removed");
    }

    #[test]
    fn test_limit_offset_mysql() {
        let sql = "SELECT * FROM t LIMIT 10, 20";
        let result = translate(sql);
        assert!(
            result.contains("LIMIT 20 OFFSET 10"),
            "Should swap LIMIT args: {result}"
        );
    }

    #[test]
    fn test_limit_single_arg_unchanged() {
        let sql = "SELECT * FROM t LIMIT 10";
        let result = translate(sql);
        assert!(result.contains("LIMIT 10"), "Single LIMIT should be unchanged");
    }

    #[test]
    fn test_replace_into() {
        let sql = "REPLACE INTO t (id, name) VALUES (1, 'foo')";
        let result = translate(sql);
        assert!(
            result.starts_with("INSERT INTO"),
            "REPLACE INTO -> INSERT INTO: {result}"
        );
        assert!(!result.contains("REPLACE"), "REPLACE should be removed: {result}");
    }

    #[test]
    fn test_group_concat() {
        let sql = "SELECT GROUP_CONCAT(name) FROM t";
        let result = translate(sql);
        assert!(
            result.contains("STRING_AGG(name, ',')"),
            "GROUP_CONCAT -> STRING_AGG: {result}"
        );
    }

    #[test]
    fn test_group_concat_separator() {
        let sql = "SELECT GROUP_CONCAT(name SEPARATOR ';') FROM t";
        let result = translate(sql);
        assert!(
            result.contains("STRING_AGG(name, ';')"),
            "GROUP_CONCAT with SEPARATOR: {result}"
        );
    }

    #[test]
    fn test_ifnull() {
        let sql = "SELECT IFNULL(name, 'unknown') FROM t";
        let result = translate(sql);
        assert!(
            result.contains("COALESCE(name, 'unknown')"),
            "IFNULL -> COALESCE: {result}"
        );
    }

    #[test]
    fn test_if_function() {
        let sql = "SELECT IF(a > 0, 'pos', 'neg') FROM t";
        let result = translate(sql);
        assert!(
            result.contains("CASE WHEN a > 0 THEN 'pos' ELSE 'neg' END"),
            "IF -> CASE WHEN: {result}"
        );
    }

    #[test]
    fn test_locate() {
        let sql = "SELECT LOCATE('bar', col1) FROM t";
        let result = translate(sql);
        assert!(
            result.contains("POSITION('bar' IN col1)"),
            "LOCATE -> POSITION: {result}"
        );
    }

    #[test]
    fn test_instr() {
        let sql = "SELECT INSTR(col1, 'bar') FROM t";
        let result = translate(sql);
        assert!(
            result.contains("POSITION('bar' IN col1)"),
            "INSTR -> POSITION: {result}"
        );
    }

    #[test]
    fn test_straight_join() {
        let sql = "SELECT * FROM t1 STRAIGHT_JOIN t2 ON t1.id = t2.id";
        let result = translate(sql);
        assert!(
            result.contains("JOIN") && !result.contains("STRAIGHT_JOIN"),
            "STRAIGHT_JOIN -> JOIN: {result}"
        );
    }

    #[test]
    fn test_sql_calc_found_rows() {
        let sql = "SELECT SQL_CALC_FOUND_ROWS * FROM t LIMIT 10";
        let result = translate(sql);
        assert!(
            !result.contains("SQL_CALC_FOUND_ROWS"),
            "SQL_CALC_FOUND_ROWS should be stripped: {result}"
        );
    }

    #[test]
    fn test_int_unsigned_promotion() {
        let sql = "CREATE TABLE t (id INT UNSIGNED)";
        let result = translate(sql);
        assert!(result.contains("BIGINT"), "INT UNSIGNED -> BIGINT: {result}");
        assert!(!result.contains("UNSIGNED"), "UNSIGNED should be stripped: {result}");
    }

    #[test]
    fn test_double_to_double_precision() {
        let sql = "CREATE TABLE t (val DOUBLE)";
        let result = translate(sql);
        assert!(
            result.contains("DOUBLE PRECISION"),
            "DOUBLE -> DOUBLE PRECISION: {result}"
        );
    }

    #[test]
    fn test_float_precision_stripped() {
        let sql = "CREATE TABLE t (val FLOAT(10))";
        let result = translate(sql);
        assert!(result.contains("REAL"), "FLOAT(N) -> REAL: {result}");
    }

    #[test]
    fn test_enum_to_text() {
        let sql = "CREATE TABLE t (status ENUM('active','inactive'))";
        let result = translate(sql);
        assert!(result.contains("TEXT"), "ENUM -> TEXT: {result}");
        assert!(!result.contains("ENUM"), "ENUM should be removed: {result}");
    }

    #[test]
    fn test_serial_auto_increment() {
        let sql = "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT)";
        let result = translate(sql);
        assert!(result.contains("SERIAL"), "INT AUTO_INCREMENT -> SERIAL: {result}");
        assert!(
            !result.contains("AUTO_INCREMENT"),
            "AUTO_INCREMENT should be removed: {result}"
        );
    }

    #[test]
    fn test_year_to_smallint() {
        let sql = "CREATE TABLE t (yr YEAR)";
        let result = translate(sql);
        assert!(result.contains("SMALLINT"), "YEAR -> SMALLINT: {result}");
    }

    #[test]
    fn test_executable_comments_stripped() {
        let sql = "SELECT /*!40001 SQL_NO_CACHE */ * FROM t";
        let result = translate(sql);
        assert!(
            !result.contains("/*!"),
            "Executable comments should be stripped: {result}"
        );
    }

    #[test]
    fn test_high_priority_stripped() {
        let sql = "SELECT HIGH_PRIORITY * FROM t";
        let result = translate(sql);
        assert!(
            !result.contains("HIGH_PRIORITY"),
            "HIGH_PRIORITY should be stripped: {result}"
        );
    }

    #[test]
    fn test_complex_wordpress_create() {
        let sql = "CREATE TABLE `wp_options` (
  `option_id` bigint(20) UNSIGNED NOT NULL AUTO_INCREMENT,
  `option_name` varchar(191) COLLATE utf8mb4_unicode_ci NOT NULL DEFAULT '',
  `option_value` longtext COLLATE utf8mb4_unicode_ci NOT NULL,
  `autoload` varchar(20) COLLATE utf8mb4_unicode_ci NOT NULL DEFAULT 'yes',
  PRIMARY KEY (`option_id`),
  UNIQUE KEY `option_name` (`option_name`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci";
        let result = translate(sql);
        // Backticks → double quotes
        assert!(!result.contains('`'), "Backticks should be replaced");
        // Types translated
        assert!(
            result.contains("BIGSERIAL"),
            "BIGINT UNSIGNED AUTO_INCREMENT -> BIGSERIAL: {result}"
        );
        assert!(result.contains("TEXT"), "LONGTEXT -> TEXT");
        // Engine/charset stripped
        assert!(!result.contains("ENGINE"), "ENGINE stripped");
        assert!(!result.contains("CHARSET"), "CHARSET stripped");
        assert!(!result.contains("COLLATE"), "COLLATE stripped");
        // UNIQUE KEY should be converted to UNIQUE constraint
        assert!(
            result.contains("UNIQUE(option_name)"),
            "UNIQUE KEY should become UNIQUE(option_name): {result}"
        );
    }

    #[test]
    fn test_case_insensitive_types() {
        let sql = "CREATE TABLE t (a longtext, b datetime, c tinyint(1))";
        let result = translate(sql);
        assert!(result.contains("TEXT"), "lowercase LONGTEXT -> TEXT");
        assert!(result.contains("TIMESTAMP"), "lowercase DATETIME -> TIMESTAMP");
        assert!(result.contains("BOOLEAN"), "lowercase TINYINT(1) -> BOOLEAN");
    }

    #[test]
    fn test_backtick_preserves_string_contents() {
        let sql = "SELECT `col` FROM t WHERE val = 'it`s a test'";
        let result = translate(sql);
        assert!(result.contains("col"), "Backtick identifier stripped: {result}");
        // Identifier backticks are stripped; the one inside the string literal
        // is preserved (it lives inside single quotes, not an identifier).
        assert!(
            result.contains("'it`s a test'"),
            "Backtick inside string literal preserved: {result}"
        );
        // Verify the identifier `col` lost its backticks (SELECT col, not SELECT `col`)
        assert!(
            !result.contains("`col`"),
            "Identifier backticks should be stripped: {result}"
        );
    }

    #[test]
    fn test_limit_offset_no_false_positive() {
        // LIMIT with sub-select should not confuse the regex
        let sql = "SELECT * FROM t LIMIT 5";
        let result = translate(sql);
        assert_eq!(
            result.contains("OFFSET"),
            false,
            "Single LIMIT must not gain OFFSET: {result}"
        );
    }

    #[test]
    fn test_mediumint() {
        let sql = "CREATE TABLE t (id MEDIUMINT)";
        let result = translate(sql);
        assert!(result.contains("INTEGER"), "MEDIUMINT -> INTEGER: {result}");
    }

    #[test]
    fn test_blob_types() {
        let sql = "CREATE TABLE t (a LONGBLOB, b MEDIUMBLOB, c TINYBLOB)";
        let result = translate(sql);
        let count = result.matches("BYTEA").count();
        assert_eq!(count, 3, "All BLOB variants -> BYTEA: {result}");
    }

    #[test]
    fn test_multi_table_delete() {
        let sql = "DELETE t, tt FROM wp_terms AS t INNER JOIN wp_term_taxonomy AS tt ON t.term_id = tt.term_id WHERE tt.taxonomy = 'nav_menu'";
        let result = translate(sql);
        // Should produce two semicolon-separated DELETE statements with IN subqueries
        let parts: Vec<&str> = result.split(';').collect();
        assert!(parts.len() >= 2, "Should produce two DELETE statements: {result}");
        assert!(
            parts[0].contains("DELETE FROM wp_terms WHERE term_id IN"),
            "First DELETE should target wp_terms with IN subquery: {result}"
        );
        assert!(
            parts[1].contains("DELETE FROM wp_term_taxonomy WHERE term_id IN"),
            "Second DELETE should target wp_term_taxonomy with IN subquery: {result}"
        );
        // Subqueries should use original aliases, not resolved table names
        assert!(result.contains("INNER JOIN"), "Subquery should contain JOIN: {result}");
    }

    #[test]
    fn test_multi_table_delete_no_alias_keyword() {
        let sql = "DELETE a, b FROM wp_terms a JOIN wp_term_taxonomy b ON a.term_id = b.term_id WHERE b.count = 0";
        let result = translate(sql);
        let parts: Vec<&str> = result.split(';').collect();
        assert!(parts.len() >= 2, "Should handle JOIN without AS keyword: {result}");
        assert!(
            parts[0].contains("DELETE FROM wp_terms"),
            "First DELETE should target wp_terms: {result}"
        );
    }

    #[test]
    fn test_key_index_stripped() {
        let sql = "CREATE TABLE wp_options (
  option_id BIGSERIAL NOT NULL,
  option_name varchar(191) NOT NULL DEFAULT '',
  PRIMARY KEY (option_id),
  KEY option_name (option_name(191))
)";
        let result = translate(sql);
        assert!(
            !result.contains("KEY option_name"),
            "KEY index should be stripped: {result}"
        );
        assert!(
            result.contains("PRIMARY KEY"),
            "PRIMARY KEY should be preserved: {result}"
        );
    }

    #[test]
    fn test_unique_key_converted() {
        let sql = "CREATE TABLE wp_users (
  ID BIGSERIAL NOT NULL,
  user_email varchar(100),
  PRIMARY KEY (ID),
  UNIQUE KEY user_email (user_email),
  KEY user_login (user_login)
)";
        let result = translate(sql);
        assert!(
            !result.contains("UNIQUE KEY"),
            "UNIQUE KEY syntax should be removed: {result}"
        );
        assert!(
            result.contains("UNIQUE(user_email)"),
            "UNIQUE KEY should be converted to UNIQUE constraint: {result}"
        );
        assert!(
            !result.contains("KEY user_login"),
            "Plain KEY should be stripped: {result}"
        );
        assert!(
            result.contains("PRIMARY KEY"),
            "PRIMARY KEY should be preserved: {result}"
        );
    }

    #[test]
    fn test_key_index_no_false_positive() {
        // Non-CREATE TABLE statements should not be affected
        let sql = "SELECT * FROM t WHERE KEY = 'value'";
        let result = translate(sql);
        assert_eq!(result, sql, "Non-CREATE should be unchanged: {result}");
    }

    #[test]
    fn test_wordpress_full_create_with_keys() {
        let sql = "CREATE TABLE `wp_posts` (
  `ID` bigint(20) UNSIGNED NOT NULL AUTO_INCREMENT,
  `post_title` text COLLATE utf8mb4_unicode_ci NOT NULL,
  PRIMARY KEY (`ID`),
  KEY `post_name` (`post_name`(191)),
  KEY `type_status_date` (`post_type`,`post_status`,`post_date`,`ID`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci";
        let result = translate(sql);
        assert!(
            !result.contains("KEY post_name"),
            "KEY indexes should be stripped: {result}"
        );
        assert!(
            !result.contains("KEY type_status_date"),
            "Composite KEY indexes should be stripped: {result}"
        );
        assert!(
            result.contains("PRIMARY KEY"),
            "PRIMARY KEY should be preserved: {result}"
        );
        assert!(
            result.contains("BIGSERIAL"),
            "AUTO_INCREMENT should become BIGSERIAL: {result}"
        );
    }

    // ---------------------------------------------------------------------
    // HDB-007: string literals, quoted identifiers and comments must not be
    // rewritten by the MySQL→PostgreSQL passes.
    // ---------------------------------------------------------------------

    /// `INSERT INTO t VALUES ('<payload>')` with the payload escaped the way a
    /// MySQL client escapes it (`'` → `''`).
    fn insert_literal(payload: &str) -> String {
        format!("INSERT INTO t VALUES ('{}')", payload.replace('\'', "''"))
    }

    #[test]
    fn test_hdb007_literal_payloads_survive_verbatim() {
        // Every one of these was rewritten inside the literal before HDB-007:
        // 'TINYINT(1)' → 'BOOLEAN', 'WHERE 1=1 AND x' → 'WHERE x', …
        let payloads = [
            "TINYINT(1)",
            "SELECT * LIMIT 5, 10",
            "a AUTO_INCREMENT b",
            "IFNULL(a,b)",
            "WHERE 1=1 AND x",
            "ON DUPLICATE KEY UPDATE x",
            "DATE_SUB(NOW(), INTERVAL 1 DAY)",
            "GROUP_CONCAT(x SEPARATOR ',')",
            "ENGINE=InnoDB DEFAULT CHARSET=utf8",
            "/*! hint */",
            "-- not a comment",
            "it's",
            "IF(a,b,c)",
            "STRAIGHT_JOIN",
            "BINARY 'x'",
        ];

        for payload in payloads {
            let sql = insert_literal(payload);
            let result = translate(&sql);
            assert_eq!(result, sql, "payload must survive byte-identical: {payload}");
        }
    }

    #[test]
    fn test_hdb007_escaped_quote_literal_then_on_duplicate_key() {
        let sql = "INSERT INTO t VALUES ('it''s TINYINT(1)') ON DUPLICATE KEY UPDATE v = VALUES(v)";
        let result = translate(sql);
        assert!(
            result.contains("'it''s TINYINT(1)'"),
            "literal with a doubled quote must be intact: {result}"
        );
        assert!(
            !result.contains("BOOLEAN"),
            "TINYINT(1) inside the literal must not be translated: {result}"
        );
        assert!(
            result.contains("ON CONFLICT DO UPDATE SET v = EXCLUDED.v"),
            "MySQL syntax outside the literal must still translate: {result}"
        );
    }

    #[test]
    fn test_hdb007_mysql_backslash_escapes_still_normalized() {
        // Unchanged behaviour, now asserted: \' becomes '' and \\ becomes \.
        let sql = r"INSERT INTO t VALUES ('it\'s a \\ path')";
        assert_eq!(translate(sql), r"INSERT INTO t VALUES ('it''s a \ path')");
    }

    #[test]
    fn test_hdb007_literal_ending_in_backslash_then_backtick_identifier() {
        // After escape normalisation the literal is 'a\' — a literal that ends
        // with a backslash. The masker must still close it there, so the
        // backtick-quoted identifier that follows stays an identifier.
        let sql = r"SELECT 'a\\' AS x, `TINYINT` FROM t";
        let result = translate(sql);
        assert_eq!(result, r"SELECT 'a\' AS x, TINYINT FROM t", "got: {result}");
        assert!(
            !result.contains("SMALLINT"),
            "a quoted identifier must not be type-translated: {result}"
        );
        assert!(!result.contains('`'), "backticks must be stripped: {result}");
    }

    #[test]
    fn test_hdb007_double_quoted_identifier_not_rewritten() {
        let sql = "SELECT \"LIMIT\", \"tinyint\" FROM t LIMIT 5, 10";
        let result = translate(sql);
        assert!(
            result.contains("\"LIMIT\""),
            "double-quoted identifier must be intact: {result}"
        );
        // `"tinyint"` follows a comma, so the pre-existing MySQL double-quote
        // heuristic in translate_backslash_escapes re-quotes it as 'tinyint';
        // either way its contents must not be rewritten to SMALLINT.
        assert!(result.contains("tinyint"), "quoted name must survive: {result}");
        assert!(
            !result.to_uppercase().contains("SMALLINT"),
            "quoted name must not be type-translated: {result}"
        );
        assert!(
            result.ends_with("LIMIT 10 OFFSET 5"),
            "the LIMIT outside the quotes must still translate: {result}"
        );
    }

    #[test]
    fn test_hdb007_comments_are_not_translated() {
        for sql in [
            "SELECT 1 -- TINYINT(1) LIMIT 5, 10",
            "SELECT 1 # AUTO_INCREMENT",
            "SELECT 1 /* AUTO_INCREMENT */ , 2",
        ] {
            assert_eq!(translate(sql), sql, "comment text must survive: {sql}");
        }

        // Executable comments keep their existing behaviour: stripped whole.
        let result = translate("SELECT /*! STRAIGHT_JOIN */ 1");
        assert!(!result.contains("/*!"), "executable comment must be stripped: {result}");
        assert!(
            !result.contains("STRAIGHT_JOIN"),
            "executable comment body must be stripped: {result}"
        );
    }

    #[test]
    fn test_hdb007_group_concat_separator_literal_preserved() {
        let sql = "SELECT GROUP_CONCAT(name SEPARATOR ' AUTO_INCREMENT ') FROM t";
        let result = translate(sql);
        assert!(
            result.contains("STRING_AGG(name, ' AUTO_INCREMENT ')"),
            "separator literal must not be rewritten: {result}"
        );
    }

    #[test]
    fn test_hdb007_if_function_with_comma_inside_literal() {
        let sql = "SELECT IF(a, 'x,y', 'z') FROM t";
        let result = translate(sql);
        assert!(
            result.contains("CASE WHEN a THEN 'x,y' ELSE 'z' END"),
            "comma inside a literal must not split the IF arguments: {result}"
        );
    }

    #[test]
    fn test_hdb007_unterminated_literal_does_not_panic() {
        let result = translate("SELECT 'abc");
        assert!(result.starts_with("SELECT '"), "unterminated literal: {result}");
        assert!(result.contains("abc"), "unterminated literal body: {result}");
    }

    #[test]
    fn test_hdb007_literal_containing_placeholder_code_points() {
        // A client may send the private-use code points the masker uses.
        let sql = "INSERT INTO t VALUES ('\u{E000}\u{E010}\u{E001} and \u{E001}\u{E011} plain')";
        assert_eq!(translate(sql), sql, "placeholder code points must round-trip");
    }

    #[test]
    fn test_hdb007_stray_placeholder_outside_literal_is_copied_through() {
        let sql = "SELECT 'x', col\u{E000}\u{E010}\u{E001} FROM t";
        assert_eq!(
            translate(sql),
            sql,
            "a forged placeholder must never be substituted with a region"
        );
    }

    #[test]
    fn test_hdb007_wordpress_option_value_preserved() {
        let sql = r#"INSERT INTO `wp_options` (`option_name`, `option_value`, `autoload`) VALUES ('_transient_timeout_x', 'a:2:{s:10:"LIMIT 0, 1";s:8:"DATETIME";}', 'yes') ON DUPLICATE KEY UPDATE `option_value` = VALUES(`option_value`)"#;
        let result = translate(sql);
        assert!(
            result.contains(r#"'a:2:{s:10:"LIMIT 0, 1";s:8:"DATETIME";}'"#),
            "serialized option_value must be byte-identical: {result}"
        );
        assert!(!result.contains('`'), "backticks must be stripped: {result}");
        assert!(
            result.contains("ON CONFLICT DO UPDATE SET option_value = EXCLUDED.option_value"),
            "the upsert clause must still translate through masked identifiers: {result}"
        );
    }

    #[test]
    fn test_hdb007_syntax_outside_literals_still_translates() {
        let sql = "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, flag TINYINT(1), \
                   note TEXT DEFAULT 'TINYINT(1) LIMIT 5, 10')";
        let result = translate(sql);
        assert!(result.contains("id SERIAL PRIMARY KEY"), "AUTO_INCREMENT: {result}");
        assert!(result.contains("flag BOOLEAN"), "TINYINT(1) column: {result}");
        assert!(
            result.contains("'TINYINT(1) LIMIT 5, 10'"),
            "the DEFAULT literal must be untouched: {result}"
        );
    }

    #[test]
    fn test_hdb007_hash_operators_are_not_comments() {
        // `<#>` (vector negative inner product) and the JSON path operators
        // `#>`, `#>>` and `#-` all contain a `#` that is NOT a comment marker:
        // masking it as one swallowed the rest of the statement, so the trailing
        // `LIMIT offset, count` stopped being rewritten.
        fn check_operator(sql: &str, operator: &str, limit: &str) {
            let result = translate(sql);
            assert!(result.contains(operator), "operator must survive: {sql} -> {result}");
            assert!(result.contains(limit), "LIMIT must translate: {sql} -> {result}");
        }

        let vector = "SELECT id FROM docs ORDER BY emb <#> '[1,2,3]' LIMIT 0, 10";
        let json_get = "SELECT j #> '{a}' FROM t LIMIT 5, 10";
        let json_text = "SELECT j #>> '{a}' FROM t LIMIT 5, 10";
        let json_delete = "SELECT j #- '{a}' FROM t LIMIT 5, 10";

        check_operator(vector, "<#> '[1,2,3]'", "LIMIT 10 OFFSET 0");
        check_operator(json_get, "#> '{a}'", "LIMIT 10 OFFSET 5");
        check_operator(json_text, "#>> '{a}'", "LIMIT 10 OFFSET 5");
        check_operator(json_delete, "#- '{a}'", "LIMIT 10 OFFSET 5");
    }

    #[test]
    fn test_hdb007_hash_comment_still_masked() {
        // A real `#` line comment is still a comment: nothing inside it is
        // translated, and the statement comes back byte-for-byte.
        let sql = "SELECT 1 # TINYINT(1) LIMIT 5, 10";
        assert_eq!(translate(sql), sql, "`#` comment text must survive verbatim");
    }

    #[test]
    fn test_hdb007_backtick_identifier_with_apostrophe_does_not_open_a_string() {
        // The apostrophe in the identifier used to open a phantom string in
        // `translate_backslash_escapes`, which then "closed" on the real
        // literal's opening quote — so the payload came back as `'a = 'b' c'`.
        let sql = r#"UPDATE t SET `it's_col` = 'a = "b" c' WHERE id = 1"#;
        let result = translate(sql);
        assert!(
            result.contains(r#"'a = "b" c'"#),
            "the literal payload must be byte-identical: {result}"
        );
        assert!(result.contains("it's_col"), "identifier name must survive: {result}");
        assert!(!result.contains('`'), "backticks must be stripped: {result}");
    }

    #[test]
    fn test_hdb007_comment_with_quotes_is_untouched_by_escape_pass() {
        // `"bye"` used to be re-quoted to `'bye'` by the escape pass, because
        // the apostrophe-free comment still followed the `AND` heuristic.
        let line = r#"SELECT 1 -- say "hi" AND "bye""#;
        assert_eq!(translate(line), line, "line-comment text must survive verbatim");

        // A block comment is copied through untouched, while a real literal
        // outside it is still escape-normalised.
        let block = r#"SELECT 1 /* it's "x" \n */ , 'y\\z'"#;
        let result = translate(block);
        assert!(
            result.contains(r#"/* it's "x" \n */"#),
            "block-comment bytes must be untouched: {result}"
        );
        assert!(
            result.contains(r"'y\z'"),
            "a real literal is still escape-normalised: {result}"
        );
    }

    #[test]
    fn test_hdb007_executable_comment_with_apostrophe() {
        // `/*! … */` is a comment to the escape pass (copied through untouched)
        // and still visible to `translate_misc`, which strips it whole.
        let sql = "SELECT /*! it's */ 1 LIMIT 5, 10";
        let result = translate(sql);
        assert!(!result.contains("/*!"), "executable comment must be stripped: {result}");
        assert!(
            !result.contains("it's"),
            "executable comment body must be stripped: {result}"
        );
        assert!(
            result.contains("LIMIT 10 OFFSET 5"),
            "the LIMIT after it must still translate: {result}"
        );
    }

    #[test]
    fn test_hdb007_backticked_aliases_in_multi_table_delete_still_resolve() {
        // Review finding on the fix pass: every occurrence of `t` used to get its own
        // placeholder, so `extract_join_columns` no longer saw the alias twice and fell
        // back to `id`; the comma form emitted two DELETEs for one table.
        let sql = "DELETE `t`, `tt` FROM wp_terms AS `t` INNER JOIN wp_term_taxonomy AS `tt` \
                   ON `t`.term_id = `tt`.term_id WHERE `tt`.taxonomy = 'nav_menu'";
        let result = translate(sql);
        let parts: Vec<&str> = result.split(';').collect();
        assert_eq!(parts.len(), 2, "two DELETE statements: {result}");
        assert!(
            parts[0].contains("DELETE FROM wp_terms WHERE term_id IN (SELECT t.term_id FROM"),
            "join column must resolve through the backticked alias: {result}"
        );
        assert!(
            parts[1].contains("DELETE FROM wp_term_taxonomy WHERE term_id IN (SELECT tt.term_id FROM"),
            "second target must resolve the same way: {result}"
        );
        assert!(!result.contains('`'), "backticks stripped: {result}");
        assert!(result.contains("'nav_menu'"), "literal intact: {result}");

        let same_table = "DELETE a, b FROM `wp_options` a, `wp_options` b WHERE a.option_name = b.option_name";
        let result = translate(same_table);
        assert_eq!(
            result.matches("DELETE FROM").count(),
            1,
            "one table quoted twice is still one target: {result}"
        );
    }
}
