//! MySQL-to-PostgreSQL SQL translator
//!
//! Rewrites MySQL-specific syntax to PostgreSQL-compatible SQL before parsing.
//! This enables WordPress and other MySQL applications to work with HeliosDB Nano.

use regex::Regex;
use std::sync::OnceLock;

/// Initialize a static regex pattern.
///
/// All patterns are compile-time string literals; invalid patterns cause
/// immediate startup failure (fail-fast), posing zero runtime risk.
#[allow(clippy::expect_used)]
fn init_regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static regex pattern must be valid")
}

/// Translate MySQL SQL to PostgreSQL-compatible SQL.
pub fn translate(sql: &str) -> String {
    let mut result = sql.to_string();

    // Apply transformations in order.
    // Backslash escapes MUST be processed first — before backtick translation
    // — so we can distinguish backslash-escaped quotes inside string literals
    // from identifier delimiters.
    result = translate_backslash_escapes(&result);
    result = translate_backticks(&result);
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
    result = translate_misc(&result);

    result
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
fn translate_backslash_escapes(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\'' {
            // Enter single-quoted string literal
            out.push('\'');
            loop {
                match chars.next() {
                    Some('\\') => {
                        // Backslash inside a single-quoted string
                        match chars.peek() {
                            Some('"') => {
                                // \" → "  (strip the backslash)
                                out.push('"');
                                chars.next();
                            }
                            Some('\\') => {
                                // \\ → \  (one backslash)
                                out.push('\\');
                                chars.next();
                            }
                            Some('n') => {
                                out.push('\n');
                                chars.next();
                            }
                            Some('r') => {
                                out.push('\r');
                                chars.next();
                            }
                            Some('t') => {
                                out.push('\t');
                                chars.next();
                            }
                            Some('0') => {
                                // \0 → strip (NUL not storable in PG text)
                                chars.next();
                            }
                            Some('\'') => {
                                // \' → '' (PostgreSQL-style escaped quote)
                                out.push('\'');
                                out.push('\'');
                                chars.next();
                            }
                            _ => {
                                // Unknown escape — keep backslash
                                out.push('\\');
                            }
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
        } else {
            out.push(ch);
        }
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
            '`' => {},
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
    let re = INT_WIDTH_RE
        .get_or_init(|| init_regex(r"(?i)\b(INT|BIGINT|INTEGER|SMALLINT)\s*\(\s*\d+\s*\)"));
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
    let re = DOUBLE_FIX_RE
        .get_or_init(|| init_regex(r"(?i)\bDOUBLE\s+PRECISION\s+PRECISION\b"));
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
    let re = BIGINT_AI_RE.get_or_init(|| {
        init_regex(r"(?i)\bBIGINT\b([^,)]*?)\s+AUTO_INCREMENT\b")
    });
    let mut s = re.replace_all(sql, "BIGSERIAL$1").to_string();

    // INT/INTEGER ... AUTO_INCREMENT  →  SERIAL
    static INT_AI_RE: OnceLock<Regex> = OnceLock::new();
    let re = INT_AI_RE.get_or_init(|| {
        init_regex(r"(?i)\b(?:INT|INTEGER)\b([^,)]*?)\s+AUTO_INCREMENT\b")
    });
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
    let re = DEFAULT_CHARSET_LONG_RE
        .get_or_init(|| init_regex(r"(?i)\s*DEFAULT\s+CHARACTER\s+SET\s*=?\s*\w+"));
    s = re.replace_all(&s, "").to_string();

    // DEFAULT CHARSET=<name>
    static DEFAULT_CHARSET_RE: OnceLock<Regex> = OnceLock::new();
    let re =
        DEFAULT_CHARSET_RE.get_or_init(|| init_regex(r"(?i)\s*DEFAULT\s+CHARSET\s*=?\s*\w+"));
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
    let re = ODK_RE.get_or_init(|| {
        init_regex(r"(?i)\s+ON\s+DUPLICATE\s+KEY\s+UPDATE\s+(.+)$")
    });

    // Strip the ON DUPLICATE KEY UPDATE clause entirely.
    // The MySQL handler implements upsert semantics by catching duplicate-key
    // errors and falling back to UPDATE (see handle_upsert_dml).
    if let Some(caps) = re.captures(sql) {
        let prefix = &sql[..caps.get(0).map_or(0, |m| m.start())];
        prefix.to_string()
    } else {
        sql.to_string()
    }
}

/// Replace `VALUES(column_name)` references with `EXCLUDED.column_name`.
///
/// Kept for potential future use when planner gains ON CONFLICT support.
#[allow(dead_code)]
fn translate_values_refs(clause: &str) -> String {
    static VALUES_REF_RE: OnceLock<Regex> = OnceLock::new();
    let re = VALUES_REF_RE.get_or_init(|| init_regex(r"(?i)\bVALUES\s*\(\s*(\w+)\s*\)"));
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
    let re = GC_SEP_RE
        .get_or_init(|| init_regex(r"(?i)\bGROUP_CONCAT\s*\((.+?)\s+SEPARATOR\s+'([^']*)'\)"));
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
    let re = MULTI_DEL_RE.get_or_init(|| {
        init_regex(
            r"(?i)^(\s*DELETE)\s+\w+(?:\s*,\s*\w+)*\s+FROM\s+(\w+)\s+(?:AS\s+)?(\w+)\s+((?:INNER\s+)?JOIN\s+(\w+)\s+(?:AS\s+)?(\w+)\s+ON\s+(.+?))\s+(WHERE\s+.+)$"
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
        let where_body = where_clause.trim()
            .strip_prefix("WHERE ")
            .or_else(|| where_clause.trim().strip_prefix("where "))
            .unwrap_or(where_clause.trim());

        // Build a common subquery FROM clause that uses the original aliases.
        // This lets the ON and WHERE clauses use their original alias references.
        let subquery_from = format!(
            "{table1} AS {alias1} INNER JOIN {table2} AS {alias2} ON {on_condition}"
        );
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
    let re = UNIQUE_KEY_RE.get_or_init(|| {
        init_regex(r"(?im),\s*UNIQUE\s+KEY\s+\w+\s*\(((?:[^()]*\([^)]*\))*[^)]*)\)")
    });

    let mut s = re.replace_all(sql, |caps: &regex::Captures<'_>| {
        let col_list = &caps[1];
        // Strip prefix lengths: col_name(191) → col_name
        let clean_cols = strip_prefix_lengths(col_list);
        format!(", UNIQUE({})", clean_cols)
    }).to_string();

    // Step 2: Remove plain (non-unique) KEY definitions.
    static KEY_LINE_RE: OnceLock<Regex> = OnceLock::new();
    let re = KEY_LINE_RE.get_or_init(|| {
        init_regex(r"(?im),\s*KEY\s+\w+\s*\((?:[^()]*\([^)]*\))*[^)]*\)")
    });

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
// 12. Miscellaneous MySQL syntax
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

    // Strip SQL_CALC_FOUND_ROWS
    static CALC_FOUND_RE: OnceLock<Regex> = OnceLock::new();
    let re = CALC_FOUND_RE.get_or_init(|| init_regex(r"(?i)\bSQL_CALC_FOUND_ROWS\b"));
    s = re.replace_all(&s, "").to_string();

    // Strip HIGH_PRIORITY / LOW_PRIORITY / DELAYED
    static PRIORITY_RE: OnceLock<Regex> = OnceLock::new();
    let re =
        PRIORITY_RE.get_or_init(|| init_regex(r"(?i)\b(?:HIGH_PRIORITY|LOW_PRIORITY|DELAYED)\b"));
    s = re.replace_all(&s, "").to_string();

    // Strip BINARY keyword before string comparisons
    static BINARY_RE: OnceLock<Regex> = OnceLock::new();
    let re = BINARY_RE.get_or_init(|| init_regex(r"(?i)\bBINARY\s+(')"));
    s = re.replace_all(&s, "$1").to_string();

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
        let sql =
            "CREATE TABLE wp_posts (ID bigint(20) NOT NULL AUTO_INCREMENT, PRIMARY KEY (ID))";
        let result = translate(sql);
        assert!(
            result.contains("BIGSERIAL"),
            "Should convert to BIGSERIAL: {result}"
        );
        assert!(
            !result.contains("AUTO_INCREMENT"),
            "Should remove AUTO_INCREMENT: {result}"
        );
    }

    #[test]
    fn test_on_duplicate_key() {
        let sql = "INSERT INTO wp_options (option_name, option_value) VALUES ('siteurl', 'http://example.com') ON DUPLICATE KEY UPDATE option_value = VALUES(option_value)";
        let result = translate(sql);
        // The translator now strips ON DUPLICATE KEY UPDATE entirely;
        // upsert semantics are handled by the MySQL handler via try-insert/fallback-update.
        assert!(
            !result.contains("ON DUPLICATE KEY"),
            "Should strip ON DUPLICATE KEY UPDATE: {result}"
        );
        assert!(
            result.contains("VALUES"),
            "Should keep INSERT VALUES: {result}"
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
        assert!(
            result.contains("'publish'"),
            "String literals should be preserved"
        );
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
        assert!(
            result.contains("LIMIT 10"),
            "Single LIMIT should be unchanged"
        );
    }

    #[test]
    fn test_replace_into() {
        let sql = "REPLACE INTO t (id, name) VALUES (1, 'foo')";
        let result = translate(sql);
        assert!(
            result.starts_with("INSERT INTO"),
            "REPLACE INTO -> INSERT INTO: {result}"
        );
        assert!(
            !result.contains("REPLACE"),
            "REPLACE should be removed: {result}"
        );
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
        assert!(
            result.contains("BIGINT"),
            "INT UNSIGNED -> BIGINT: {result}"
        );
        assert!(
            !result.contains("UNSIGNED"),
            "UNSIGNED should be stripped: {result}"
        );
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
        assert!(
            !result.contains("ENUM"),
            "ENUM should be removed: {result}"
        );
    }

    #[test]
    fn test_serial_auto_increment() {
        let sql = "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT)";
        let result = translate(sql);
        assert!(
            result.contains("SERIAL"),
            "INT AUTO_INCREMENT -> SERIAL: {result}"
        );
        assert!(
            !result.contains("AUTO_INCREMENT"),
            "AUTO_INCREMENT should be removed: {result}"
        );
    }

    #[test]
    fn test_year_to_smallint() {
        let sql = "CREATE TABLE t (yr YEAR)";
        let result = translate(sql);
        assert!(
            result.contains("SMALLINT"),
            "YEAR -> SMALLINT: {result}"
        );
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
        assert!(
            result.contains("TIMESTAMP"),
            "lowercase DATETIME -> TIMESTAMP"
        );
        assert!(
            result.contains("BOOLEAN"),
            "lowercase TINYINT(1) -> BOOLEAN"
        );
    }

    #[test]
    fn test_backtick_preserves_string_contents() {
        let sql = "SELECT `col` FROM t WHERE val = 'it`s a test'";
        let result = translate(sql);
        assert!(
            result.contains("col"),
            "Backtick identifier stripped: {result}"
        );
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
        assert!(
            result.contains("INTEGER"),
            "MEDIUMINT -> INTEGER: {result}"
        );
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
        assert!(
            parts.len() >= 2,
            "Should produce two DELETE statements: {result}"
        );
        assert!(
            parts[0].contains("DELETE FROM wp_terms WHERE term_id IN"),
            "First DELETE should target wp_terms with IN subquery: {result}"
        );
        assert!(
            parts[1].contains("DELETE FROM wp_term_taxonomy WHERE term_id IN"),
            "Second DELETE should target wp_term_taxonomy with IN subquery: {result}"
        );
        // Subqueries should use original aliases, not resolved table names
        assert!(
            result.contains("INNER JOIN"),
            "Subquery should contain JOIN: {result}"
        );
    }

    #[test]
    fn test_multi_table_delete_no_alias_keyword() {
        let sql = "DELETE a, b FROM wp_terms a JOIN wp_term_taxonomy b ON a.term_id = b.term_id WHERE b.count = 0";
        let result = translate(sql);
        let parts: Vec<&str> = result.split(';').collect();
        assert!(
            parts.len() >= 2,
            "Should handle JOIN without AS keyword: {result}"
        );
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
}
