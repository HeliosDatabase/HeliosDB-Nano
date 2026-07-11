//! A2: token-level literal normalization for the plan cache.
//!
//! A pgbench-style point read arrives with a fresh literal every statement
//! (`… WHERE aid = 5`, `… = 6`, …), so its raw-SQL cache key can never hit.
//! This byte-lexer runs BEFORE parse and rewrites the literals in the WHERE
//! predicate to `$1,$2,…` positional parameters, returning the normalized SQL
//! (a stable cache key) plus the extracted parameter values. The caller can
//! then reuse ONE cached parameterized plan across every literal variant.
//!
//! CORRECTNESS IS EVERYTHING here — a wrong normalization means silently wrong
//! query results. Two safeguards:
//!   1. Parameters are typed through the SAME path as inline literals
//!      (`Planner::number_literal_to_value` / `single_quoted_content_to_value`),
//!      so a `$n` and the literal it replaced can never diverge in type.
//!   2. The lexer BAILS (returns `None` → the caller uses the raw path) on
//!      anything it is not certain is safe. It normalizes number and
//!      single-quoted-string literals in the top-level WHERE predicate, plus
//!      (next-batch item) a bare integer literal directly after a top-level
//!      LIMIT or OFFSET keyword — never one nested inside a parenthesized
//!      subquery, and never `LIMIT ALL` or an expression. Literals in the
//!      SELECT list / GROUP / ORDER (which can change the output shape) are
//!      left untouched, and a large set of constructs (comments, casts,
//!      IN/BETWEEN/subquery lists, escaped/dollar-quoted strings, existing
//!      placeholders) force a full bail.
//!
//! This file's own `mod differential` oracle asserts, over a large corpus,
//! that executing the raw SQL (via `EmbeddedDatabase::query_raw_unnormalized`,
//! which bypasses this normalizer) and executing the normalized+parameterized
//! SQL return identical rows — the real proof that this is sound. The wired
//! end-to-end path is covered by `tests/query_normalization.rs`.

use crate::sql::Planner;
use crate::Value;

/// Attempt to normalize a single SELECT statement's WHERE-predicate literals to
/// positional parameters. Returns `Some((normalized_sql, params))` when the
/// rewrite is provably result-preserving, else `None` (caller uses raw SQL).
pub(crate) fn normalize_select_literals(sql: &str) -> Option<(String, Vec<Value>)> {
    let bytes = sql.as_bytes();
    let n = bytes.len();

    // Only single-statement SELECTs. `WITH` (CTEs) and everything else bail —
    // the WHERE we would target could be inside a subquery with different scope.
    let start = skip_ws(bytes, 0);
    if !keyword_at(bytes, start, b"SELECT") {
        return None;
    }

    let mut out = String::with_capacity(n + 16);
    let mut params: Vec<Value> = Vec::new();
    let mut i = 0usize;
    let mut depth: i32 = 0;
    // Are we inside the top-level (depth-0) WHERE predicate right now?
    let mut in_where = false;
    // Are we inside a top-level (depth-0) LIMIT / OFFSET clause right now?
    // Only ever set while `depth == 0` (see the clause-keyword handling
    // below), so a LIMIT/OFFSET belonging to a parenthesized subquery never
    // sets these — it is read at `depth > 0` and left untouched.
    let mut in_limit = false;
    let mut in_offset = false;

    while i < n {
        let b = bytes[i];

        // --- constructs that force a full bail wherever they appear ---
        // Line/block comments can hide structure from this shallow lexer.
        if b == b'-' && i + 1 < n && bytes[i + 1] == b'-' {
            return None;
        }
        if b == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
            return None;
        }
        // Dollar-quoted string ($$…$$ / $tag$…$tag$) or a positional parameter
        // that is already present ($1): either way, do not touch this query.
        if b == b'$' {
            return None;
        }

        // --- string literal ---
        if b == b'\'' {
            let end = skip_single_quoted(bytes, i)?; // index just past closing quote
            if in_where {
                // Collapse the doubled-'' escapes to recover the content, then
                // type it through the planner's own string path.
                let raw = &sql[i + 1..end - 1]; // between the quotes, may contain ''
                let content = raw.replace("''", "'");
                params.push(Planner::single_quoted_content_to_value(&content));
                out.push('$');
                out.push_str(&params.len().to_string());
            } else {
                out.push_str(&sql[i..end]);
            }
            i = end;
            continue;
        }

        // --- prefixed string literals we must not reinterpret: E'…' U&'…'
        //     X'…' B'…' (and lowercase). Detect `<letter>'` or `U&'`. ---
        if (b == b'E' || b == b'e' || b == b'X' || b == b'x' || b == b'B' || b == b'b')
            && i + 1 < n
            && bytes[i + 1] == b'\''
        {
            return None;
        }
        if (b == b'U' || b == b'u') && i + 1 < n && bytes[i + 1] == b'&' {
            return None;
        }

        // --- identifier / keyword (starts with a letter or underscore) ---
        if b.is_ascii_alphabetic() || b == b'_' {
            let word_end = read_word(bytes, i);
            let word = &sql[i..word_end];

            if depth == 0 {
                // Top-level clause keywords delimit the WHERE region.
                if word.eq_ignore_ascii_case("where") {
                    in_where = true;
                    in_limit = false;
                    in_offset = false;
                    out.push_str(word);
                    i = word_end;
                    continue;
                }
                if is_where_terminator_kw(word) {
                    in_where = false;
                    // LIMIT/OFFSET open their own literal-normalizable
                    // region; any OTHER top-level clause keyword (GROUP,
                    // ORDER, HAVING, WINDOW, FETCH, FOR, UNION, INTERSECT,
                    // EXCEPT) closes it.
                    if word.eq_ignore_ascii_case("limit") {
                        in_limit = true;
                        in_offset = false;
                    } else if word.eq_ignore_ascii_case("offset") {
                        in_offset = true;
                        in_limit = false;
                    } else {
                        in_limit = false;
                        in_offset = false;
                    }
                }
            }

            if in_where {
                // Widened IN handling (next-batch item 1). A predicate `IN`
                // must be followed by `(` — anything else (`POSITION('x' IN
                // s)`, exotic syntax) is unknown territory: bail to raw.
                if word.eq_ignore_ascii_case("in") {
                    let after = skip_ws(bytes, word_end);
                    if after >= n || bytes[after] != b'(' {
                        return None;
                    }
                    if let Some((consumed_to, elems)) = lex_pure_literal_list(sql, bytes, after) {
                        // Pure-literal list: parameterize + pad to the next
                        // power of two by REPEATING THE LAST PLACEHOLDER, so an
                        // ORM sweeping arities 1..N produces log2(N) plan-cache
                        // shapes instead of N. Repetition is result-neutral by
                        // IN's set semantics (incl. NOT IN); the count fast
                        // path dedups encoded keys (count_single_pk_in_list)
                        // and the evaluator is a per-row membership test.
                        if elems.is_empty() || elems.len() > MAX_IN_LIST {
                            return None;
                        }
                        let count = elems.len();
                        out.push_str(word);
                        out.push_str(" (");
                        for (k, value) in elems.into_iter().enumerate() {
                            if k > 0 {
                                out.push(',');
                            }
                            params.push(value);
                            out.push('$');
                            out.push_str(&params.len().to_string());
                        }
                        let last_placeholder = params.len();
                        for _ in count..count.next_power_of_two() {
                            out.push(',');
                            out.push('$');
                            out.push_str(&last_placeholder.to_string());
                        }
                        out.push(')');
                        i = consumed_to;
                        continue;
                    }
                    // Not a pure-literal list (columns, NULL, mixed, subquery):
                    // emit the word and let the generic loop handle the parens
                    // — literals inside still parameterize (unpadded), and an
                    // `IN (SELECT …)` bails via the retained `select` keyword.
                }
                // Subqueries, ANY/ALL/SOME arrays, CASE, row constructors:
                // unsafe for this rewrite. Bail.
                if is_predicate_bail_kw(word) {
                    return None;
                }
            }

            out.push_str(word);
            i = word_end;
            continue;
        }

        // --- quoted identifier "…" : copy verbatim (never a value) ---
        if b == b'"' {
            let end = skip_double_quoted(bytes, i)?;
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }

        // --- number literal (starts with an ASCII digit) ---
        if b.is_ascii_digit() {
            let end = read_number(bytes, i)?;
            // LIMIT/OFFSET only ever normalize a literal sitting directly at
            // depth 0 (i.e. immediately after the clause keyword, not inside
            // some parenthesized expression under it) — `in_where` is
            // deliberately depth-agnostic (a parenthesized boolean group like
            // `(a = 1)` is still the same predicate), but LIMIT/OFFSET are
            // conservative: any paren nesting after the keyword falls back to
            // copying the literal through unchanged.
            if in_where {
                let text = &sql[i..end];
                let value = Planner::number_literal_to_value(text).ok()?;
                params.push(value);
                out.push('$');
                out.push_str(&params.len().to_string());
            } else if depth == 0
                && (in_limit || in_offset)
                && bytes[i..end].iter().all(u8::is_ascii_digit)
            {
                // A LIMIT/OFFSET bound: exactly ONE bare non-negative integer
                // per clause keyword. Parameterizing it is result-preserving —
                // the plan is identical for every bound value (the executor
                // applies the count at runtime), so all `LIMIT n OFFSET m`
                // variants collapse onto one cached parameterized plan. The
                // all-digits guard keeps the param typed as Int4/Int8 (the only
                // shapes the LIMIT resolver accepts) and, together with clearing
                // the flag below, leaves a two-operand or expression bound
                // (`LIMIT 10+5`, MySQL `LIMIT 5,10`, `LIMIT 5.5`) inline so it
                // errors identically on the raw and normalized sides.
                let text = &sql[i..end];
                let value = Planner::number_literal_to_value(text).ok()?;
                params.push(value);
                out.push('$');
                out.push_str(&params.len().to_string());
                in_limit = false;
                in_offset = false;
            } else {
                out.push_str(&sql[i..end]);
            }
            i = end;
            continue;
        }

        // A bare `.` immediately before a digit would be a leading-dot decimal
        // (`.5`) that read_number (digit-led) would mis-split into `.` + `5`.
        // Rare in predicates; bail rather than risk it.
        if b == b'.'
            && i + 1 < n
            && bytes[i + 1].is_ascii_digit()
            && (in_where || in_limit || in_offset)
        {
            return None;
        }

        // `::type` casts (next-batch item 1): pass through whitelisted bare
        // type names — `$n::uuid` executes through the same Cast machinery as
        // `'lit'::uuid` (Cast-over-Parameter unwrap in the index probe +
        // fast_cast_value), and copying `col::type` verbatim never changes
        // semantics. Non-whitelisted targets and parenthesized precisions
        // (`varchar(10)`) bail as before.
        if b == b':' && i + 1 < n && bytes[i + 1] == b':' && in_where {
            let ty_start = i + 2;
            if ty_start >= n || !(bytes[ty_start].is_ascii_alphabetic() || bytes[ty_start] == b'_') {
                return None;
            }
            let ty_end = read_word(bytes, ty_start);
            let ty = &sql[ty_start..ty_end];
            let after_ty = skip_ws(bytes, ty_end);
            if !is_whitelisted_cast_type(ty) || (after_ty < n && bytes[after_ty] == b'(') {
                return None;
            }
            out.push_str("::");
            out.push_str(ty);
            i = ty_end;
            continue;
        }

        // --- punctuation / operators ---
        if b == b'(' {
            depth += 1;
        } else if b == b')' {
            depth -= 1;
            if depth < 0 {
                return None; // unbalanced — let the real parser produce the error
            }
        } else if b == b';' {
            // A statement terminator: only a single trailing `;` (then only
            // whitespace) is acceptable; anything after it is a second
            // statement we will not touch.
            if skip_ws(bytes, i + 1) != n {
                return None;
            }
            out.push_str(&sql[i..]);
            i = n;
            break;
        }

        // Default: copy this byte through. (ASCII operators, commas, spaces.)
        out.push(b as char);
        i += 1;
    }

    // Only worth returning a rewrite that actually parameterized something —
    // either a WHERE predicate literal or a top-level LIMIT/OFFSET literal. A
    // query with neither (e.g. `SELECT x FROM t`) has nothing to extract.
    if !params.is_empty() {
        Some((out, params))
    } else {
        None
    }
}

// ── lexer helpers ────────────────────────────────────────────────────────────

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True if `kw` matches at `i` case-insensitively AND on a word boundary
/// (previous and next bytes are not word bytes).
fn keyword_at(bytes: &[u8], i: usize, kw: &[u8]) -> bool {
    if i + kw.len() > bytes.len() {
        return false;
    }
    if i > 0 && is_word_byte(bytes[i - 1]) {
        return false;
    }
    if !bytes[i..i + kw.len()].eq_ignore_ascii_case(kw) {
        return false;
    }
    let after = i + kw.len();
    after >= bytes.len() || !is_word_byte(bytes[after])
}

/// Read a word starting at `i` (already known to start with a letter/`_`);
/// return the index just past it.
fn read_word(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && is_word_byte(bytes[i]) {
        i += 1;
    }
    i
}

/// Read a digit-led number starting at `i`; return the index just past it, or
/// `None` on a malformed shape (double dot, etc.).
fn read_number(bytes: &[u8], start: usize) -> Option<usize> {
    let n = bytes.len();
    let mut i = start;
    while i < n && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // Optional single fractional part.
    if i < n && bytes[i] == b'.' {
        i += 1;
        // A second `.` (e.g. `1.2.3`) is not a number — bail.
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i < n && bytes[i] == b'.' {
            return None;
        }
    }
    // Optional exponent.
    if i < n && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < n && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        if j < n && bytes[j].is_ascii_digit() {
            i = j;
            while i < n && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
        // else: not an exponent (e.g. `1e` followed by nothing) — leave `e`.
    }
    // A number immediately fused to a letter/underscore is not a plain literal
    // (defensive; the digit-led branch already excludes identifiers).
    if i < n && is_word_byte(bytes[i]) {
        return None;
    }
    Some(i)
}

/// Given `i` at an opening `'`, return the index just past the matching close,
/// treating `''` as an escaped quote. `None` if unterminated.
fn skip_single_quoted(bytes: &[u8], i: usize) -> Option<usize> {
    let n = bytes.len();
    let mut j = i + 1;
    while j < n {
        if bytes[j] == b'\'' {
            if j + 1 < n && bytes[j + 1] == b'\'' {
                j += 2; // escaped quote, stay in-string
                continue;
            }
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

/// Given `i` at an opening `"`, return the index just past the matching close,
/// treating `""` as an escaped quote. `None` if unterminated.
fn skip_double_quoted(bytes: &[u8], i: usize) -> Option<usize> {
    let n = bytes.len();
    let mut j = i + 1;
    while j < n {
        if bytes[j] == b'"' {
            if j + 1 < n && bytes[j + 1] == b'"' {
                j += 2;
                continue;
            }
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

/// Top-level clause keywords that end the WHERE predicate region.
fn is_where_terminator_kw(word: &str) -> bool {
    const KWS: [&str; 11] = [
        "group", "order", "limit", "offset", "having", "window", "fetch", "for", "union", "intersect", "except",
    ];
    KWS.iter().any(|k| word.eq_ignore_ascii_case(k))
}

/// Predicate constructs whose literals this rewrite cannot safely parameterize
/// (subqueries, array/row constructors, branch values). `select` appearing
/// inside the WHERE region is a subquery — its scope differs from the outer
/// predicate, so bail (function-call parens contain no `select`, so they are
/// unaffected and their constant args still parameterize). `IN` and `BETWEEN`
/// are handled positively by the lexer (widened in the next-batch item 1) and
/// are deliberately NOT in this list.
fn is_predicate_bail_kw(word: &str) -> bool {
    const KWS: [&str; 8] = ["exists", "any", "all", "some", "values", "array", "case", "select"];
    KWS.iter().any(|k| word.eq_ignore_ascii_case(k))
}

/// Largest IN-list this rewrite will parameterize. Above this, padding cost and
/// plan quality both degrade — bail to the raw path.
const MAX_IN_LIST: usize = 128;

/// Cast targets safe to pass through as `$n::type`: the executor's
/// Cast-over-Parameter unwrap (index probe + `fast_cast_value`) is verified for
/// these. Bare identifiers only — a parenthesized precision (`varchar(10)`)
/// bails at the call site.
fn is_whitelisted_cast_type(ty: &str) -> bool {
    const TYPES: [&str; 20] = [
        "uuid",
        "int2",
        "int4",
        "int8",
        "smallint",
        "integer",
        "int",
        "bigint",
        "text",
        "varchar",
        "date",
        "timestamp",
        "timestamptz",
        "numeric",
        "decimal",
        "float4",
        "float8",
        "real",
        "boolean",
        "bool",
    ];
    TYPES.iter().any(|k| ty.eq_ignore_ascii_case(k))
}

/// Try to lex a PURE-LITERAL parenthesized list starting at the `(` at
/// `open_idx`: `( [±]number | 'string' (, …)* )`. Returns the index just past
/// the closing `)` plus the typed element values, or `None` when any element
/// is not a plain literal (column, expression, NULL/TRUE/FALSE, subquery —
/// those fall back to the generic per-byte path, unpadded).
fn lex_pure_literal_list(sql: &str, bytes: &[u8], open_idx: usize) -> Option<(usize, Vec<Value>)> {
    let n = bytes.len();
    let mut j = open_idx + 1;
    let mut elems = Vec::new();
    loop {
        j = skip_ws(bytes, j);
        if j >= n {
            return None;
        }
        match bytes[j] {
            b'\'' => {
                let end = skip_single_quoted(bytes, j)?;
                let content = sql[j + 1..end - 1].replace("''", "'");
                elems.push(Planner::single_quoted_content_to_value(&content));
                j = end;
            }
            b'+' | b'-' => {
                let sign_start = j;
                if j + 1 >= n || !bytes[j + 1].is_ascii_digit() {
                    return None;
                }
                let end = read_number(bytes, j + 1)?;
                // Fold the sign into the value: for IN elements this is
                // semantically identical to a unary operator on the literal.
                elems.push(Planner::number_literal_to_value(&sql[sign_start..end]).ok()?);
                j = end;
            }
            b if b.is_ascii_digit() => {
                let end = read_number(bytes, j)?;
                elems.push(Planner::number_literal_to_value(&sql[j..end]).ok()?);
                j = end;
            }
            _ => return None,
        }
        j = skip_ws(bytes, j);
        if j >= n {
            return None;
        }
        match bytes[j] {
            b',' => {
                j += 1;
            }
            b')' => return Some((j + 1, elems)),
            _ => return None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn norm(sql: &str) -> Option<(String, Vec<Value>)> {
        normalize_select_literals(sql)
    }

    #[test]
    fn simple_int_eq() {
        let (s, p) = norm("SELECT abalance FROM t50 WHERE aid = 5").unwrap();
        assert_eq!(s, "SELECT abalance FROM t50 WHERE aid = $1");
        assert_eq!(p, vec![Value::Int4(5)]);
    }

    #[test]
    fn multi_predicate() {
        let (s, p) = norm("SELECT x FROM t WHERE a = 5 AND b = 'hi'").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE a = $1 AND b = $2");
        assert_eq!(p, vec![Value::Int4(5), Value::String("hi".to_string())]);
    }

    #[test]
    fn string_with_doubled_quote() {
        let (s, p) = norm("SELECT x FROM t WHERE name = 'O''Brien'").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE name = $1");
        assert_eq!(p, vec![Value::String("O'Brien".to_string())]);
    }

    #[test]
    fn float_literal() {
        let (_s, p) = norm("SELECT x FROM t WHERE ratio = 3.5").unwrap();
        assert_eq!(p, vec![Value::Float8(3.5)]);
    }

    #[test]
    fn negative_stays_operator() {
        // The sign is left as a unary operator on the parameter.
        let (s, p) = norm("SELECT x FROM t WHERE v = -7").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE v = -$1");
        assert_eq!(p, vec![Value::Int4(7)]);
    }

    #[test]
    fn literal_in_select_list_not_touched() {
        // `1` in the projection changes the output; only the WHERE `2` is a param.
        let (s, p) = norm("SELECT 1, x FROM t WHERE a = 2").unwrap();
        assert_eq!(s, "SELECT 1, x FROM t WHERE a = $1");
        assert_eq!(p, vec![Value::Int4(2)]);
    }

    #[test]
    fn limit_offset_parameterized() {
        // LIMIT/OFFSET literals are now parameterized too, so a rotating
        // LIMIT/OFFSET workload shares ONE cached plan instead of one per
        // distinct (limit, offset) pair.
        let (s, p) = norm("SELECT x FROM t WHERE a = 5 LIMIT 10 OFFSET 3").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE a = $1 LIMIT $2 OFFSET $3");
        assert_eq!(p, vec![Value::Int4(5), Value::Int4(10), Value::Int4(3)]);
    }

    #[test]
    fn limit_offset_only_no_where() {
        // No WHERE at all — LIMIT/OFFSET literals alone are still worth
        // parameterizing (the `saw_where` gate was dropped for exactly this).
        let (s, p) = norm("SELECT x FROM t LIMIT 20 OFFSET 40").unwrap();
        assert_eq!(s, "SELECT x FROM t LIMIT $1 OFFSET $2");
        assert_eq!(p, vec![Value::Int4(20), Value::Int4(40)]);
    }

    #[test]
    fn offset_before_limit_parameterized() {
        // Postgres also accepts OFFSET before LIMIT.
        let (s, p) = norm("SELECT x FROM t OFFSET 40 LIMIT 20").unwrap();
        assert_eq!(s, "SELECT x FROM t OFFSET $1 LIMIT $2");
        assert_eq!(p, vec![Value::Int4(40), Value::Int4(20)]);
    }

    #[test]
    fn offset_only_no_limit_parameterized() {
        // OFFSET with no LIMIT: limit_param stays None, offset_param is set.
        let (s, p) = norm("SELECT x FROM t OFFSET 5").unwrap();
        assert_eq!(s, "SELECT x FROM t OFFSET $1");
        assert_eq!(p, vec![Value::Int4(5)]);
    }

    #[test]
    fn limit_expression_takes_only_first_bare_integer() {
        // A two-operand / expression bound is left as an expression: only the
        // FIRST bare integer after the keyword is taken (the flag clears after
        // it), so the second operand stays inline. `$1 + 5` is then rejected by
        // the planner's LIMIT bound check identically to raw `10 + 5` — no
        // divergence. (The point of the guard: never parameterize BOTH operands
        // of a MySQL `LIMIT a,b` or an arithmetic bound.)
        let (s, p) = norm("SELECT x FROM t LIMIT 10+5").unwrap();
        assert_eq!(s, "SELECT x FROM t LIMIT $1+5");
        assert_eq!(p, vec![Value::Int4(10)]);
        // MySQL `LIMIT a,b`: first taken, comma+second left inline.
        let (s2, p2) = norm("SELECT x FROM t LIMIT 5,10").unwrap();
        assert_eq!(s2, "SELECT x FROM t LIMIT $1,10");
        assert_eq!(p2, vec![Value::Int4(5)]);
    }

    #[test]
    fn limit_non_integer_left_inline() {
        // A non-integer LIMIT bound (invalid SQL) is copied through verbatim,
        // not parameterized — so it errors identically on both sides.
        let (s, p) = norm("SELECT x FROM t WHERE a = 1 LIMIT 5.5").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE a = $1 LIMIT 5.5");
        assert_eq!(p, vec![Value::Int4(1)]);
    }

    #[test]
    fn limit_all_untouched() {
        // `LIMIT ALL` has no literal to extract; only the WHERE literal
        // parameterizes.
        let (s, p) = norm("SELECT x FROM t WHERE a = 5 LIMIT ALL").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE a = $1 LIMIT ALL");
        assert_eq!(p, vec![Value::Int4(5)]);
    }

    #[test]
    fn limit_inside_subquery_not_touched() {
        // A LIMIT belonging to a FROM-clause subquery is at paren depth > 0
        // when the keyword is seen, so `in_limit` never activates for it —
        // only the outer WHERE literal parameterizes.
        let (s, p) = norm("SELECT * FROM (SELECT y FROM z LIMIT 5) t WHERE a = 1").unwrap();
        assert_eq!(s, "SELECT * FROM (SELECT y FROM z LIMIT 5) t WHERE a = $1");
        assert_eq!(p, vec![Value::Int4(1)]);
    }

    #[test]
    fn existing_limit_placeholder_still_bails() {
        // `$n` anywhere forces a full bail (the global `$` guard), including
        // an already-parameterized LIMIT.
        assert!(norm("SELECT x FROM t WHERE a = 5 LIMIT $1").is_none());
    }

    #[test]
    fn qualified_column_not_a_number() {
        let (s, p) = norm("SELECT x FROM t WHERE t.col1 = 9").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE t.col1 = $1");
        assert_eq!(p, vec![Value::Int4(9)]);
    }

    #[test]
    fn in_list_padded_to_power_of_two() {
        // 3 literals → 4 placeholders, the last repeated; 3 params.
        let (s, p) = norm("SELECT x FROM t WHERE a IN (1,2,3)").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE a IN ($1,$2,$3,$3)");
        assert_eq!(p, vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)]);
    }

    #[test]
    fn in_list_exact_power_of_two_not_padded() {
        let (s, p) = norm("SELECT x FROM t WHERE a IN (1,2)").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE a IN ($1,$2)");
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn in_list_arity_buckets_share_shape() {
        // Different literals, same arity → identical normalized SQL (the whole
        // point: one cached plan per arity bucket).
        let (s1, _) = norm("SELECT x FROM t WHERE a IN (1,2,3)").unwrap();
        let (s2, _) = norm("SELECT x FROM t WHERE a IN (7,8,9)").unwrap();
        assert_eq!(s1, s2);
        // Arity 5 and arity 7 both pad to 8 → same shape.
        let (s5, p5) = norm("SELECT x FROM t WHERE a IN (1,2,3,4,5)").unwrap();
        let (s7, _) = norm("SELECT x FROM t WHERE a IN (1,2,3,4,5,6,7)").unwrap();
        assert_eq!(s5.matches('$').count(), 8);
        assert_eq!(s7.matches('$').count(), 8);
        assert_eq!(p5.len(), 5, "params carry the true arity, padding repeats the last placeholder");
    }

    #[test]
    fn in_list_strings_and_signs() {
        let (s, p) = norm("SELECT x FROM t WHERE name IN ('a', 'o''b') AND v IN (-1, +2)").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE name IN ($1,$2) AND v IN ($3,$4)");
        assert_eq!(
            p,
            vec![
                Value::String("a".into()),
                Value::String("o'b".into()),
                Value::Int4(-1),
                Value::Int4(2),
            ]
        );
    }

    #[test]
    fn not_in_padded_same_as_in() {
        let (s, _) = norm("SELECT x FROM t WHERE a NOT IN (1,2,3)").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE a NOT IN ($1,$2,$3,$3)");
    }

    #[test]
    fn in_list_over_cap_bails() {
        let list: Vec<String> = (1..=129).map(|i| i.to_string()).collect();
        let sql = format!("SELECT x FROM t WHERE a IN ({})", list.join(","));
        assert!(norm(&sql).is_none());
    }

    #[test]
    fn in_without_paren_bails_position() {
        // POSITION's IN is not a predicate list — must bail to the raw path.
        assert!(norm("SELECT x FROM t WHERE POSITION('x' IN name) = 1").is_none());
    }

    #[test]
    fn in_with_null_or_columns_falls_back_unpadded() {
        // NULL element → not a pure-literal list → generic path (no padding),
        // numbers still parameterized, NULL inline.
        let (s, p) = norm("SELECT x FROM t WHERE a IN (1, NULL)").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE a IN ($1, NULL)");
        assert_eq!(p, vec![Value::Int4(1)]);
        // Column element → same fallback.
        let (s2, p2) = norm("SELECT x FROM t WHERE a IN (1, b)").unwrap();
        assert_eq!(s2, "SELECT x FROM t WHERE a IN ($1, b)");
        assert_eq!(p2.len(), 1);
    }

    #[test]
    fn in_subquery_still_bails() {
        assert!(norm("SELECT x FROM t WHERE a IN (SELECT b FROM u)").is_none());
        assert!(norm("SELECT x FROM t WHERE a NOT IN (SELECT b FROM u)").is_none());
    }

    #[test]
    fn between_parameterizes_both_bounds() {
        let (s, p) = norm("SELECT x FROM t WHERE a BETWEEN 1 AND 10").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE a BETWEEN $1 AND $2");
        assert_eq!(p, vec![Value::Int4(1), Value::Int4(10)]);
        let (s2, _) = norm("SELECT x FROM t WHERE a NOT BETWEEN 1 AND 10").unwrap();
        assert_eq!(s2, "SELECT x FROM t WHERE a NOT BETWEEN $1 AND $2");
    }

    #[test]
    fn whitelisted_cast_passes_through() {
        let (s, p) =
            norm("SELECT x FROM t WHERE id = '11111111-1111-1111-1111-111111111111'::uuid").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE id = $1::uuid");
        assert_eq!(p.len(), 1);
        let (s2, _) = norm("SELECT x FROM t WHERE ts = '2026-01-01'::timestamp AND n = 5::int8").unwrap();
        assert_eq!(s2, "SELECT x FROM t WHERE ts = $1::timestamp AND n = $2::int8");
        // Cast on a COLUMN copies through; the compared literal still params.
        let (s3, p3) = norm("SELECT x FROM t WHERE c::int = 5").unwrap();
        assert_eq!(s3, "SELECT x FROM t WHERE c::int = $1");
        assert_eq!(p3, vec![Value::Int4(5)]);
    }

    #[test]
    fn non_whitelisted_or_precision_cast_bails() {
        assert!(norm("SELECT x FROM t WHERE v = '1 day'::interval").is_none());
        assert!(norm("SELECT x FROM t WHERE v = 'abc'::varchar(10)").is_none());
        assert!(norm("SELECT x FROM t WHERE v = '{}'::jsonb").is_none());
    }

    #[test]
    fn bails_on_subquery() {
        assert!(norm("SELECT x FROM t WHERE a = (SELECT max(b) FROM u WHERE b = 3)").is_none());
    }

    #[test]
    fn bails_on_comment() {
        assert!(norm("SELECT x FROM t WHERE a = 5 -- c").is_none());
        assert!(norm("SELECT x FROM t WHERE a = 5 /* c */").is_none());
    }

    #[test]
    fn bails_on_escaped_and_dollar() {
        assert!(norm(r"SELECT x FROM t WHERE b = E'\x41'").is_none());
        assert!(norm("SELECT x FROM t WHERE a = $1").is_none());
        assert!(norm("SELECT x FROM t WHERE b = $$hi$$").is_none());
    }

    #[test]
    fn bails_without_where() {
        assert!(norm("SELECT 1").is_none());
        assert!(norm("SELECT x FROM t").is_none());
    }

    #[test]
    fn bails_on_non_select() {
        assert!(norm("UPDATE t SET x = 1 WHERE a = 2").is_none());
        assert!(norm("WITH c AS (SELECT 1) SELECT * FROM c WHERE a = 2").is_none());
        assert!(norm("INSERT INTO t VALUES (1)").is_none());
    }

    #[test]
    fn bails_on_second_statement() {
        assert!(norm("SELECT x FROM t WHERE a = 5; DROP TABLE t").is_none());
    }

    #[test]
    fn trailing_semicolon_ok() {
        let (s, p) = norm("SELECT x FROM t WHERE a = 5;").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE a = $1;");
        assert_eq!(p, vec![Value::Int4(5)]);
    }

    #[test]
    fn leading_dot_decimal_bails() {
        assert!(norm("SELECT x FROM t WHERE r = .5").is_none());
    }

    #[test]
    fn string_in_select_list_not_touched() {
        let (s, p) = norm("SELECT 'label', x FROM t WHERE a = 1").unwrap();
        assert_eq!(s, "SELECT 'label', x FROM t WHERE a = $1");
        assert_eq!(p, vec![Value::Int4(1)]);
    }

    #[test]
    fn like_predicate_ok() {
        let (s, p) = norm("SELECT x FROM t WHERE name LIKE 'a%'").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE name LIKE $1");
        assert_eq!(p, vec![Value::String("a%".to_string())]);
    }

    #[test]
    fn big_int_types_as_int8() {
        let (_s, p) = norm("SELECT x FROM t WHERE a = 5000000000").unwrap();
        assert_eq!(p, vec![Value::Int8(5_000_000_000)]);
    }

    #[test]
    fn does_not_panic_on_random_ish_input() {
        // Must never panic; either normalizes or bails.
        for s in [
            "SELECT",
            "SELECT ''''",
            "SELECT x FROM t WHERE a = '",
            "SELECT x FROM t WHERE = = =",
            "SELECT x FROM t WHERE a = 1.2.3",
            "select x from t where a=1and b=2",
            "SELECT x FROM t WHERE a = 1)))",
        ] {
            let _ = norm(s);
        }
    }
}

/// The differential oracle: for every query the normalizer accepts, executing
/// the normalized+parameterized form must return byte-identical rows to
/// executing the raw SQL. This is the real proof that the rewrite is
/// result-preserving; a lib test so it can reach the crate-internal normalizer
/// and the parameterized query path together.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod differential {
    use super::normalize_select_literals;
    use crate::{EmbeddedDatabase, Tuple};

    fn repr(rows: &[Tuple]) -> Vec<String> {
        let mut out: Vec<String> = rows
            .iter()
            .map(|t| t.values.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join("|"))
            .collect();
        out.sort();
        out
    }

    fn seed() -> EmbeddedDatabase {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute(
            "CREATE TABLE d (id INT PRIMARY KEY, n INT, big INT8, r FLOAT8, name TEXT, flag BOOLEAN, note TEXT)",
        )
        .unwrap();
        db.execute("CREATE INDEX d_name ON d(name)").unwrap();
        db.execute("CREATE INDEX d_n ON d(n)").unwrap();
        let rows = [
            "(1, 10, 5000000000, 1.5, 'alice', true, 'x')",
            "(2, 20, 6000000000, -2.25, 'o''brien', false, NULL)",
            "(3, 10, 7000000000, 0.0, 'carol', true, 'y')",
            "(4, 40, 8000000000, 3.14159, 'dave', false, 'z')",
            "(5, 20, 9000000000, -100.5, '', true, 'alice')",
            "(6, 30, 1000000000, 42.0, 'Eve', false, 'x')",
        ];
        for r in rows {
            db.execute(&format!("INSERT INTO d VALUES {r}")).unwrap();
        }
        db
    }

    #[test]
    fn raw_equals_normalized_over_corpus() {
        let db = seed();
        let corpus = [
            "SELECT id FROM d WHERE n = 10",
            "SELECT id FROM d WHERE n = 20 AND flag = true",
            "SELECT id, name FROM d WHERE name = 'alice'",
            "SELECT id FROM d WHERE name = 'o''brien'",
            "SELECT id FROM d WHERE name = ''",
            "SELECT id FROM d WHERE big = 7000000000",
            "SELECT id FROM d WHERE r = 3.14159",
            "SELECT id FROM d WHERE r = -2.25",
            "SELECT id FROM d WHERE n = -5",
            "SELECT id FROM d WHERE n > 15 AND n < 35",
            "SELECT id FROM d WHERE n >= 20 AND r <= 0.0",
            "SELECT id FROM d WHERE name <> 'alice'",
            "SELECT id FROM d WHERE name LIKE 'a%'",
            "SELECT id FROM d WHERE note = 'x' AND flag = false",
            "SELECT id FROM d WHERE n = 20 OR name = 'dave'",
            "SELECT id FROM d WHERE (n = 10 OR n = 20) AND flag = true",
            "SELECT count(*) FROM d WHERE n = 10",
            "SELECT id FROM d WHERE n = 10 ORDER BY id DESC",
            "SELECT id FROM d WHERE n = 20 LIMIT 1",
            "SELECT id FROM d WHERE id = 3",
            "SELECT id FROM d WHERE name = 'Eve'",
            "SELECT id FROM d WHERE big > 5000000000 AND big < 9000000000",
            "SELECT id, n, r FROM d WHERE flag = true AND n = 10",
            "SELECT id FROM d WHERE abs(r) = 2.25",
            "SELECT id FROM d WHERE n = 10;",
            // ---- widened shapes (item 1): IN / NOT IN / padding / BETWEEN / casts ----
            "SELECT id FROM d WHERE n IN (10)",
            "SELECT id FROM d WHERE n IN (10, 20)",
            "SELECT id FROM d WHERE n IN (10, 20, 30)",
            "SELECT id FROM d WHERE n IN (10, 20, 30, 40, 50)",
            "SELECT id FROM d WHERE n IN (10, 10, 20)",
            "SELECT id FROM d WHERE n NOT IN (10, 30)",
            "SELECT id FROM d WHERE n NOT IN (10, 10)",
            "SELECT id FROM d WHERE name IN ('alice', 'o''brien', '')",
            "SELECT id FROM d WHERE n IN (-5, 10, 20)",
            "SELECT id FROM d WHERE big IN (5000000000, 7000000000)",
            "SELECT id FROM d WHERE n IN (1, NULL)",
            "SELECT id FROM d WHERE n NOT IN (10, NULL)",
            "SELECT id FROM d WHERE n BETWEEN 10 AND 25",
            "SELECT id FROM d WHERE n NOT BETWEEN 10 AND 25",
            "SELECT id FROM d WHERE r BETWEEN -3.0 AND 1.6",
            "SELECT id FROM d WHERE big = 7000000000::int8",
            "SELECT id FROM d WHERE n = 10 AND name IN ('alice','carol') OR r BETWEEN 40.0 AND 43.0",
            // ---- LIMIT/OFFSET parameterization ----
            "SELECT id FROM d WHERE n = 20 ORDER BY id LIMIT 1 OFFSET 1",
            "SELECT id FROM d WHERE flag = true ORDER BY id LIMIT 2 OFFSET 1",
            "SELECT id FROM d ORDER BY id LIMIT 3 OFFSET 2",
            "SELECT id FROM d ORDER BY id DESC LIMIT 100 OFFSET 0",
            "SELECT id FROM d ORDER BY id LIMIT 0",
            "SELECT id FROM d ORDER BY id LIMIT 2 OFFSET 999",
            "SELECT id FROM d WHERE n = 10 LIMIT ALL",
            "SELECT id FROM d WHERE id IN (SELECT id FROM d LIMIT 3)",
        ];

        let mut normalized_count = 0;
        for raw in corpus {
            let raw_rows = repr(
                &db.query_raw_unnormalized(raw)
                    .unwrap_or_else(|e| panic!("raw failed for {raw}: {e}")),
            );
            if let Some((nsql, params)) = normalize_select_literals(raw) {
                normalized_count += 1;
                let norm_rows = db
                    .query_params(&nsql, &params)
                    .unwrap_or_else(|e| panic!("normalized failed for {raw} -> {nsql}: {e}"));
                assert_eq!(
                    raw_rows,
                    repr(&norm_rows),
                    "\nMISMATCH\n  raw:  {raw}\n  norm: {nsql}\n  params: {params:?}"
                );
            }
        }
        // Guard against the corpus silently all-bailing (which would make the
        // oracle vacuous).
        assert!(
            normalized_count >= 45,
            "expected most of the corpus to normalize, got {normalized_count}"
        );
    }

    /// Deterministic property fuzz: generate a few hundred random predicates
    /// from a small grammar and assert raw==normalized for every query the
    /// normalizer accepts (and no panic for the rest). Seeded LCG — fully
    /// reproducible, no wall-clock/os entropy.
    #[test]
    fn property_fuzz_raw_equals_normalized() {
        let db = seed();
        let mut rng_state: u64 = 0x5eed_1e57_0BAD_F00D;
        let mut next = move || {
            // LCG (Numerical Recipes constants) — deterministic across runs.
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (rng_state >> 33) as u32
        };

        let int_cols = ["n", "id"];
        let ops = ["=", ">", "<", ">=", "<=", "<>"];
        let names = ["alice", "o''brien", "", "carol", "dave", "Eve", "zzz"];

        let mut term = |next: &mut dyn FnMut() -> u32| -> String {
            match next() % 5 {
                0 => {
                    let col = int_cols[(next() % 2) as usize];
                    let op = ops[(next() % 6) as usize];
                    let v = (next() % 60) as i64 - 10;
                    format!("{col} {op} {v}")
                }
                1 => {
                    // IN list, arity 1..=9, values may repeat.
                    let col = int_cols[(next() % 2) as usize];
                    let arity = 1 + (next() % 9) as usize;
                    let vals: Vec<String> = (0..arity).map(|_| ((next() % 60) as i64 - 10).to_string()).collect();
                    let neg = if next() % 4 == 0 { "NOT " } else { "" };
                    format!("{col} {neg}IN ({})", vals.join(", "))
                }
                2 => {
                    let lo = (next() % 40) as i64 - 10;
                    let hi = lo + (next() % 30) as i64;
                    format!("n BETWEEN {lo} AND {hi}")
                }
                3 => {
                    let name = names[(next() % names.len() as u32) as usize];
                    format!("name = '{name}'")
                }
                _ => {
                    let v = ((next() % 800) as f64 / 100.0) - 4.0;
                    format!("r > {v:.2}")
                }
            }
        };

        let mut normalized_count = 0usize;
        for _ in 0..300 {
            let nterms = 1 + (next() % 3) as usize;
            let mut pred = term(&mut next);
            for _ in 1..nterms {
                let joiner = if next() % 2 == 0 { "AND" } else { "OR" };
                pred = format!("{pred} {joiner} {}", term(&mut next));
            }
            let raw = format!("SELECT id FROM d WHERE {pred}");
            let raw_rows = repr(
                &db.query_raw_unnormalized(&raw)
                    .unwrap_or_else(|e| panic!("raw failed for {raw}: {e}")),
            );
            if let Some((nsql, params)) = normalize_select_literals(&raw) {
                normalized_count += 1;
                let norm_rows = db
                    .query_params(&nsql, &params)
                    .unwrap_or_else(|e| panic!("normalized failed for {raw} -> {nsql}: {e}"));
                assert_eq!(
                    raw_rows,
                    repr(&norm_rows),
                    "\nFUZZ MISMATCH\n  raw:  {raw}\n  norm: {nsql}\n  params: {params:?}"
                );
            }
        }
        assert!(
            normalized_count >= 250,
            "fuzz corpus should mostly normalize, got {normalized_count}/300"
        );
    }
}
