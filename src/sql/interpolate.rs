//! `$`-placeholder interpolation for routine bodies.
//!
//! This module is the SINGLE implementation of `$`-placeholder substitution into SQL
//! text, and the single place values are rendered as SQL literals. Do not fork a second
//! copy — both routine families call in here:
//!
//! * the `LANGUAGE sql` paths (`FunctionRegistry::execute_sql_function` and
//!   `execute_sql_procedure` in `src/sql/functions.rs`), which resolve `$1` and
//!   `$<paramname>` against the call-site argument list; and
//! * the procedural runtime (`ExecutionContext::interpolate` in
//!   `src/sql/procedural/runtime.rs`), which resolves `$<name>` against the PL/pgSQL
//!   variable scope and `$1` against the call-site arguments.
//!
//! `interpolate_sql_placeholders` replaces a sequence of `String::replace` calls that
//! corrupted bodies four ways: `$1` prefix-matched inside `$10`, `$p` prefix-matched
//! inside `$p_id`, substitution happened inside `'string literals'`, and — because the
//! passes ran in sequence — an already-substituted VALUE was re-scanned by the next
//! pass, so argument data could influence the interpolation of other placeholders. The
//! scanner here is a single left-to-right pass that is aware of every SQL region in
//! which a `$` is not a placeholder, and it never re-scans the text it emits, so all
//! four are structurally impossible.
//!
//! ## Sigil policy
//!
//! Only `$name` and `$N` are placeholders. A bare identifier is always a column
//! reference — PostgreSQL resolves bare PL/pgSQL variable names, HeliosDB Nano
//! deliberately does not, so that a variable can never silently shadow a column.
//!
//! ## Textual, not bound
//!
//! Substitution is textual: values are rendered by `value_to_sql_literal` and the
//! resulting SQL is re-parsed by the ordinary planner. There is no injection via the
//! quote route (`'` is doubled) and a substituted value can never smuggle in a
//! placeholder or change the scanner's quote state (replacements are not re-scanned),
//! but every value does round-trip through SQL text. Moving to real bind parameters is
//! a follow-up; it would reuse this same scanner, emitting `$1..$k` plus a value vector
//! instead of literals.
//!
//! All byte offsets used below are UTF-8 character boundaries by construction: they are
//! produced either by accumulating `char::len_utf8` or by `str::find`.

use crate::Value;

/// A `$`-placeholder found in interpolatable text — that is, outside string literals,
/// quoted identifiers, comments and dollar-quoted strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placeholder<'a> {
    /// `$3` becomes `Positional(3)`. `$0` becomes `Positional(0)`: resolvers MUST return
    /// `None` for it rather than underflowing a `n - 1` index computation.
    Positional(usize),
    /// `$p_id` becomes `Named("p_id")` — the maximal run of identifier characters after
    /// the `$`. Never empty, and never starts with a digit.
    Named(&'a str),
}

/// Interpolate `$`-placeholders in `sql` with a single left-to-right scan.
///
/// For every placeholder found in interpolatable text, `resolve` is called once:
/// `Some(text)` replaces the placeholder with `text` VERBATIM (the replacement is never
/// re-scanned, so it cannot contain a placeholder or open a quote), and `None` leaves
/// the original placeholder characters untouched so the ordinary planner raises its
/// existing loud error (`Invalid parameter placeholder: $oops`, `Parameter $9 not
/// provided`). Unknown placeholders therefore keep failing loudly; nothing new is
/// swallowed.
///
/// `$` is NOT a placeholder inside: `'string literals'` (including `E'...'` escape
/// strings), `"quoted identifiers"`, `-- line comments`, `/* nested /* block */
/// comments */`, and `$tag$ dollar-quoted strings $tag$`. Those regions are copied
/// through verbatim.
///
/// Placeholder names use maximal munch, which IS the longest-match rule: `$p_id` can
/// never be read as `$p`, and `$10` can never be read as `$1`.
///
/// Infallible: an unterminated literal or comment is emitted as-is and the downstream
/// SQL parser produces the error.
pub(crate) fn interpolate_sql_placeholders<'s, F>(sql: &'s str, mut resolve: F) -> String
where
    F: FnMut(Placeholder<'s>) -> Option<String>,
{
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;

    while let Some(c) = char_at(sql, i) {
        match c {
            '\'' => i = copy_quoted(sql, i, &mut out, '\'', false),
            '"' => i = copy_quoted(sql, i, &mut out, '"', false),
            '-' if char_at(sql, i + 1) == Some('-') => i = copy_line_comment(sql, i, &mut out),
            '/' if char_at(sql, i + 1) == Some('*') => i = copy_block_comment(sql, i, &mut out),
            '$' => i = interpolate_at_dollar(sql, i, &mut out, &mut resolve),
            _ if is_ident_start(c) => {
                // Munch the whole word so that `E` is only recognised as the PostgreSQL
                // escape-string prefix when it stands alone as a token: `abcE'x'` is the
                // identifier `abcE` followed by an ordinary literal, not an E-string.
                let end = munch_ident(sql, i);
                let word = &sql[i..end];
                out.push_str(word);
                if (word == "E" || word == "e") && char_at(sql, end) == Some('\'') {
                    i = copy_quoted(sql, end, &mut out, '\'', true);
                } else {
                    i = end;
                }
            }
            _ => {
                out.push(c);
                i += c.len_utf8();
            }
        }
    }

    out
}

/// Handle a `$` seen in interpolatable text. Returns the byte offset to resume from.
fn interpolate_at_dollar<'s, F>(sql: &'s str, start: usize, out: &mut String, resolve: &mut F) -> usize
where
    F: FnMut(Placeholder<'s>) -> Option<String>,
{
    let after = start + 1; // `$` is ASCII, so this is a character boundary.
    match char_at(sql, after) {
        // `$$ ... $$` — dollar-quoted string with an empty tag.
        Some('$') => copy_dollar_quoted(sql, start, out, 2),

        // `$N` — positional. A digit run can never be a dollar-quote tag (PostgreSQL
        // tags may not start with a digit), so `$1$x$..$x$` is Positional(1) followed by
        // a `$x$` opener: do not consume the trailing `$`.
        Some(c) if c.is_ascii_digit() => {
            let end = munch_digits(sql, after);
            let resolved = sql[after..end]
                .parse::<usize>()
                .ok()
                .and_then(|n| resolve(Placeholder::Positional(n)));
            match resolved {
                Some(text) => out.push_str(&text),
                None => out.push_str(&sql[start..end]),
            }
            end
        }

        // `$tag$ ... $tag$` opener, or `$name` — decided by what follows the run.
        Some(c) if is_ident_start(c) => {
            let end = munch_ident(sql, after);
            if char_at(sql, end) == Some('$') {
                copy_dollar_quoted(sql, start, out, end + 1 - start)
            } else {
                match resolve(Placeholder::Named(&sql[after..end])) {
                    Some(text) => out.push_str(&text),
                    None => out.push_str(&sql[start..end]),
                }
                end
            }
        }

        // A lone `$` (whitespace, punctuation, EOF, ...). Emit it and re-process the
        // following character in the normal state — a following `'` must still open a
        // string literal.
        _ => {
            out.push('$');
            after
        }
    }
}

/// Copy a `'...'` / `"..."` region verbatim, honouring the doubled-quote escape and,
/// for PostgreSQL `E'...'` strings, backslash escapes. `start` is the opening quote.
fn copy_quoted(sql: &str, start: usize, out: &mut String, quote: char, backslash_escapes: bool) -> usize {
    out.push(quote);
    let mut i = start + quote.len_utf8();

    while let Some(c) = char_at(sql, i) {
        if backslash_escapes && c == '\\' {
            out.push(c);
            i += c.len_utf8();
            if let Some(escaped) = char_at(sql, i) {
                out.push(escaped);
                i += escaped.len_utf8();
            }
            continue;
        }
        if c == quote {
            out.push(c);
            i += c.len_utf8();
            // A doubled quote is an escaped quote, not the end of the region.
            if char_at(sql, i) == Some(quote) {
                out.push(quote);
                i += quote.len_utf8();
                continue;
            }
            return i;
        }
        out.push(c);
        i += c.len_utf8();
    }

    i // Unterminated: emitted as-is, downstream parser complains.
}

/// Copy a `-- ...` comment through the terminating newline (or to end of input).
fn copy_line_comment(sql: &str, start: usize, out: &mut String) -> usize {
    let mut i = start;
    while let Some(c) = char_at(sql, i) {
        out.push(c);
        i += c.len_utf8();
        if c == '\n' {
            break;
        }
    }
    i
}

/// Copy a `/* ... */` comment, which nests in PostgreSQL.
fn copy_block_comment(sql: &str, start: usize, out: &mut String) -> usize {
    let mut i = start;
    let mut depth = 0usize;

    while let Some(c) = char_at(sql, i) {
        if c == '/' && char_at(sql, i + 1) == Some('*') {
            out.push_str("/*");
            i += 2;
            depth += 1;
        } else if c == '*' && char_at(sql, i + 1) == Some('/') {
            out.push_str("*/");
            i += 2;
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return i;
            }
        } else {
            out.push(c);
            i += c.len_utf8();
        }
    }

    i
}

/// Copy a dollar-quoted string verbatim. `start` is the opening `$`, `delim_len` the
/// byte length of the opening delimiter (`2` for `$$`, `tag.len() + 2` for `$tag$`).
fn copy_dollar_quoted(sql: &str, start: usize, out: &mut String, delim_len: usize) -> usize {
    let body_start = start + delim_len;
    let delim = &sql[start..body_start];

    match sql[body_start..].find(delim) {
        Some(offset) => {
            let end = body_start + offset + delim_len;
            out.push_str(&sql[start..end]);
            end
        }
        None => {
            // Unterminated: emit the rest verbatim and let the SQL parser object.
            out.push_str(&sql[start..]);
            sql.len()
        }
    }
}

/// The character at byte offset `i`, or `None` at/after the end (or at a non-boundary,
/// which cannot happen here).
fn char_at(sql: &str, i: usize) -> Option<char> {
    sql.get(i..).and_then(|rest| rest.chars().next())
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offset just past the maximal identifier run starting at `start`.
fn munch_ident(sql: &str, start: usize) -> usize {
    let mut i = start;
    while let Some(c) = char_at(sql, i) {
        if is_ident_char(c) {
            i += c.len_utf8();
        } else {
            break;
        }
    }
    i
}

/// Byte offset just past the maximal ASCII digit run starting at `start`.
fn munch_digits(sql: &str, start: usize) -> usize {
    let mut i = start;
    while let Some(c) = char_at(sql, i) {
        if c.is_ascii_digit() {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// Render a `Value` as a SQL literal for textual interpolation.
///
/// The `'` doubling for string-ish variants is load-bearing: it is what keeps
/// `O'Brien` a value rather than a syntax error or an injection point.
pub(crate) fn value_to_sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Int2(v) => v.to_string(),
        Value::Int4(v) => v.to_string(),
        Value::Int8(v) => v.to_string(),
        Value::Float4(v) => v.to_string(),
        Value::Float8(v) => v.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Numeric(d) => d.clone(),
        Value::Date(d) => format!("'{}'", d),
        Value::Time(t) => format!("'{}'", t),
        Value::Timestamp(ts) => format!("'{}'", ts),
        Value::Uuid(u) => format!("'{}'", u),
        Value::Json(j) => format!("'{}'", j.replace('\'', "''")),
        Value::Bytes(b) => format!("E'\\\\x{}'", hex::encode(b)),
        Value::Vector(v) => format!("[{}]", v.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(",")),
        Value::Array(arr) => {
            let elements: Vec<String> = arr.iter().map(value_to_sql_literal).collect();
            format!("ARRAY[{}]", elements.join(","))
        }
        // Storage references (should be resolved before reaching here)
        Value::DictRef { dict_id } => format!("'dict:{}'", dict_id),
        Value::CasRef { hash } => format!("E'\\\\x{}'", hex::encode(hash)),
        Value::ColumnarRef => "NULL".to_string(), // Placeholder
        Value::Interval(iv) => format!("INTERVAL '{} microseconds'", iv),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Resolve `$1`/`$2`/... to `a1`/`a2`/... and `$name` to `<name>`; everything else
    /// is unknown. Deliberately emits text that would be re-mangled by a second pass.
    fn demo(sql: &str) -> String {
        interpolate_sql_placeholders(sql, |ph| match ph {
            Placeholder::Positional(0) => None,
            Placeholder::Positional(n) => Some(format!("a{}", n)),
            Placeholder::Named("oops") => None,
            Placeholder::Named(name) => Some(format!("<{}>", name)),
        })
    }

    #[test]
    fn empty_input() {
        assert_eq!(demo(""), "");
    }

    #[test]
    fn placeholder_at_start_and_at_eof() {
        assert_eq!(demo("$1"), "a1");
        assert_eq!(demo("$1 + $2"), "a1 + a2");
        assert_eq!(demo("x$name"), "x<name>");
    }

    #[test]
    fn maximal_munch_beats_prefix_capture() {
        // The whole point: `$1` must not eat the prefix of `$10`, and `$p` must not eat
        // the prefix of `$p_id`.
        assert_eq!(demo("$10"), "a10");
        assert_eq!(demo("$1,$10,$100"), "a1,a10,a100");
        assert_eq!(demo("$p, $p_id"), "<p>, <p_id>");
        assert_eq!(demo("$p_id, $p"), "<p_id>, <p>");
    }

    #[test]
    fn leading_zeros_parse_as_the_number() {
        // Behavioural note (CHANGELOG): `$01` used to survive verbatim because the old
        // `replace` looked for the exact text `$1`. It now resolves as 1.
        assert_eq!(demo("$01"), "a1");
        // `$0` and `$00` have no argument, so the resolver declines and they stay put.
        assert_eq!(demo("$0"), "$0");
        assert_eq!(demo("$00"), "$00");
    }

    #[test]
    fn unknown_placeholder_is_left_verbatim() {
        assert_eq!(demo("VALUES ($oops)"), "VALUES ($oops)");
    }

    #[test]
    fn no_substitution_inside_single_quoted_literals() {
        assert_eq!(
            demo("INSERT INTO t VALUES ($1, 'price is $1 dollars')"),
            "INSERT INTO t VALUES (a1, 'price is $1 dollars')"
        );
    }

    #[test]
    fn doubled_quote_escape_keeps_the_literal_open() {
        assert_eq!(demo("'O''Brien $1' , $2"), "'O''Brien $1' , a2");
        // A literal that is only doubled quotes must not fall out of the string state.
        assert_eq!(demo("'''' $1"), "'''' a1");
    }

    #[test]
    fn escape_string_backslash_does_not_close_the_literal() {
        assert_eq!(demo(r"E'a\'$1' , $2"), r"E'a\'$1' , a2");
        assert_eq!(demo(r"e'\\' , $1"), r"e'\\' , a1");
        // A word merely ENDING in `e` is not an escape-string prefix.
        assert_eq!(demo("value'$1'"), "value'$1'");
    }

    #[test]
    fn double_quoted_identifiers_are_skipped() {
        assert_eq!(demo(r#"SELECT "col $1" , $1"#), r#"SELECT "col $1" , a1"#);
        assert_eq!(demo(r#""a""b $1" $2"#), r#""a""b $1" a2"#);
    }

    #[test]
    fn comments_are_skipped() {
        assert_eq!(demo("VALUES ($1) -- see $2"), "VALUES (a1) -- see $2");
        assert_eq!(demo("-- $1\n$2"), "-- $1\na2");
        assert_eq!(demo("/* $1 */ $2"), "/* $1 */ a2");
        // PostgreSQL block comments nest.
        assert_eq!(demo("/* a /* $1 */ b */ $2"), "/* a /* $1 */ b */ a2");
        // A lone `-` or `/` is not a comment.
        assert_eq!(demo("$1 - $2"), "a1 - a2");
        assert_eq!(demo("$1 / $2"), "a1 / a2");
    }

    #[test]
    fn dollar_quoted_strings_are_skipped() {
        assert_eq!(demo("$q$has $1 and $p_id$q$, $1"), "$q$has $1 and $p_id$q$, a1");
        assert_eq!(demo("$$raw $1$$ , $1"), "$$raw $1$$ , a1");
        // `$tag` is only a dollar-quote opener when a `$` follows the tag.
        assert_eq!(demo("$q$x$q$"), "$q$x$q$");
        assert_eq!(demo("$q"), "<q>");
    }

    #[test]
    fn digit_run_is_never_a_dollar_quote_tag() {
        // `$1$x$..$x$` is Positional(1) followed by a `$x$` opener.
        assert_eq!(demo("$1$x$body $2$x$"), "a1$x$body $2$x$");
    }

    #[test]
    fn unterminated_regions_are_emitted_verbatim() {
        assert_eq!(demo("'unterminated $1"), "'unterminated $1");
        assert_eq!(demo("$q$unterminated $1"), "$q$unterminated $1");
        assert_eq!(demo("/* unterminated $1"), "/* unterminated $1");
    }

    #[test]
    fn lone_dollar_does_not_swallow_the_next_char() {
        // The `'` after the `$` must still open a string literal.
        assert_eq!(demo("$'a $1' $2"), "$'a $1' a2");
        assert_eq!(demo("$ $1"), "$ a1");
        assert_eq!(demo("$"), "$");
        assert_eq!(demo("cost$"), "cost$");
    }

    #[test]
    fn multibyte_characters_adjacent_to_placeholders() {
        assert_eq!(demo("é$1é"), "éa1é");
        assert_eq!(demo("'né $1' $2"), "'né $1' a2");
        // Non-ASCII identifier characters are part of the name (maximal munch).
        assert_eq!(demo("$café"), "<café>");
    }

    #[test]
    fn replacements_are_never_re_scanned() {
        // The second-order corruption: a substituted VALUE containing a placeholder or a
        // quote must not influence the interpolation of anything else.
        let out = interpolate_sql_placeholders("VALUES ($1, $2)", |ph| match ph {
            Placeholder::Positional(1) => Some("'$name and $2'".to_string()),
            Placeholder::Positional(2) => Some("'it''s'".to_string()),
            _ => None,
        });
        assert_eq!(out, "VALUES ('$name and $2', 'it''s')");
    }

    #[test]
    fn resolver_sees_each_placeholder_exactly_once() {
        let mut seen: Vec<String> = Vec::new();
        let out = interpolate_sql_placeholders("$a $1 $a", |ph| {
            seen.push(format!("{:?}", ph));
            None
        });
        assert_eq!(out, "$a $1 $a");
        assert_eq!(seen, vec!["Named(\"a\")", "Positional(1)", "Named(\"a\")"]);
    }

    #[test]
    fn test_value_to_sql_literal() {
        assert_eq!(value_to_sql_literal(&Value::Null), "NULL");
        assert_eq!(value_to_sql_literal(&Value::Boolean(true)), "TRUE");
        assert_eq!(value_to_sql_literal(&Value::Int4(42)), "42");
        assert_eq!(value_to_sql_literal(&Value::String("hello".to_string())), "'hello'");
        assert_eq!(value_to_sql_literal(&Value::String("it's".to_string())), "'it''s'");
    }

    #[test]
    fn quote_doubling_survives_a_round_trip_through_the_scanner() {
        // O'Brien must arrive intact, and the closing quote of the rendered literal must
        // not be mistaken for the start of a new one.
        let out = interpolate_sql_placeholders("VALUES ($1, $2)", |ph| match ph {
            Placeholder::Positional(n) => Some(value_to_sql_literal(&Value::String(if n == 1 {
                "O'Brien".to_string()
            } else {
                "x".to_string()
            }))),
            Placeholder::Named(_) => None,
        });
        assert_eq!(out, "VALUES ('O''Brien', 'x')");
    }
}
