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
//!      anything it is not certain is safe. It only ever normalizes number and
//!      single-quoted-string literals that sit in the top-level WHERE predicate;
//!      literals in the SELECT list / GROUP / ORDER / LIMIT / OFFSET (which can
//!      change the output shape or row count) are left untouched, and a large
//!      set of constructs (comments, casts, IN/BETWEEN/subquery lists, escaped/
//!      dollar-quoted strings, existing placeholders) force a full bail.
//!
//! The `tests/normalization_differential.rs` oracle asserts, over a large
//! corpus, that executing the raw SQL and executing the normalized+parameterized
//! SQL return identical rows — the real proof that this is sound.

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
    // Did we see a top-level WHERE at all? (Only queries with a WHERE are worth
    // normalizing; a WHERE-less query has no predicate literals to extract.)
    let mut saw_where = false;

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
                    saw_where = true;
                    out.push_str(word);
                    i = word_end;
                    continue;
                }
                if is_where_terminator_kw(word) {
                    in_where = false;
                }
            }

            if in_where {
                // Constructs inside a predicate whose literals are NOT a simple
                // scalar comparison operand — variable-arity lists, ranges,
                // subqueries, CASE — are unsafe for this v1 rewrite. Bail.
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
            if in_where {
                let text = &sql[i..end];
                let value = Planner::number_literal_to_value(text).ok()?;
                params.push(value);
                out.push('$');
                out.push_str(&params.len().to_string());
            } else {
                out.push_str(&sql[i..end]);
            }
            i = end;
            continue;
        }

        // A bare `.` immediately before a digit would be a leading-dot decimal
        // (`.5`) that read_number (digit-led) would mis-split into `.` + `5`.
        // Rare in predicates; bail rather than risk it.
        if b == b'.' && i + 1 < n && bytes[i + 1].is_ascii_digit() && in_where {
            return None;
        }

        // A `::` cast applied to a literal changes its type; the v1 rewrite does
        // not model casts, so bail if one appears in the predicate.
        if b == b':' && i + 1 < n && bytes[i + 1] == b':' && in_where {
            return None;
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

    // Only worth returning a rewrite that actually parameterized something in a
    // real WHERE predicate.
    if saw_where && !params.is_empty() {
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

/// Predicate constructs whose literals this v1 rewrite cannot safely
/// parameterize (variable arity, ranges, subqueries, branch values). `select`
/// appearing inside the WHERE region is a subquery — its scope differs from the
/// outer predicate, so bail (function-call parens contain no `select`, so they
/// are unaffected and their constant args still parameterize).
fn is_predicate_bail_kw(word: &str) -> bool {
    const KWS: [&str; 10] = [
        "in", "between", "exists", "any", "all", "some", "values", "array", "case", "select",
    ];
    KWS.iter().any(|k| word.eq_ignore_ascii_case(k))
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
    fn limit_offset_not_touched() {
        let (s, p) = norm("SELECT x FROM t WHERE a = 5 LIMIT 10 OFFSET 3").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE a = $1 LIMIT 10 OFFSET 3");
        assert_eq!(p, vec![Value::Int4(5)]);
    }

    #[test]
    fn qualified_column_not_a_number() {
        let (s, p) = norm("SELECT x FROM t WHERE t.col1 = 9").unwrap();
        assert_eq!(s, "SELECT x FROM t WHERE t.col1 = $1");
        assert_eq!(p, vec![Value::Int4(9)]);
    }

    #[test]
    fn bails_on_in_list() {
        assert!(norm("SELECT x FROM t WHERE a IN (1,2,3)").is_none());
    }

    #[test]
    fn bails_on_between() {
        assert!(norm("SELECT x FROM t WHERE a BETWEEN 1 AND 10").is_none());
    }

    #[test]
    fn bails_on_cast() {
        assert!(norm("SELECT x FROM t WHERE id = '11111111-1111-1111-1111-111111111111'::uuid").is_none());
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
        ];

        let mut normalized_count = 0;
        for raw in corpus {
            let raw_rows = repr(&db.query(raw, &[]).unwrap_or_else(|e| panic!("raw failed for {raw}: {e}")));
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
        assert!(normalized_count >= 18, "expected most of the corpus to normalize, got {normalized_count}");
    }
}
