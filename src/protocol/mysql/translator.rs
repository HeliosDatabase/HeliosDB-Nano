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

    // Apply transformations in order
    result = translate_backticks(&result);
    result = translate_types(&result);
    result = translate_auto_increment(&result);
    result = translate_charset_collation(&result);
    result = translate_on_duplicate_key(&result);
    result = translate_replace_into(&result);
    result = translate_insert_ignore(&result);
    result = translate_limit_offset(&result);
    result = translate_functions(&result);
    result = translate_misc(&result);

    result
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
            // Replace backtick with double-quote.
            '`' => out.push('"'),
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

    // CHARACTER SET <name>
    static CHARSET_LONG_RE: OnceLock<Regex> = OnceLock::new();
    let re = CHARSET_LONG_RE.get_or_init(|| init_regex(r"(?i)\s*CHARACTER\s+SET\s+\w+"));
    s = re.replace_all(&s, "").to_string();

    // CHARSET <name>  (short form, not preceded by DEFAULT word yet)
    static CHARSET_SHORT_RE: OnceLock<Regex> = OnceLock::new();
    let re = CHARSET_SHORT_RE.get_or_init(|| init_regex(r"(?i)\s*CHARSET\s*=?\s*\w+"));
    s = re.replace_all(&s, "").to_string();

    // DEFAULT CHARSET=<name>
    static DEFAULT_CHARSET_RE: OnceLock<Regex> = OnceLock::new();
    let re =
        DEFAULT_CHARSET_RE.get_or_init(|| init_regex(r"(?i)\s*DEFAULT\s+CHARSET\s*=\s*\w+"));
    s = re.replace_all(&s, "").to_string();

    // COLLATE <name>  (with or without =)
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

    if let Some(caps) = re.captures(sql) {
        let set_clause = &caps[1];
        // Replace VALUES(col) → EXCLUDED.col inside the SET clause.
        let translated_set = translate_values_refs(set_clause);
        let prefix = &sql[..caps.get(0).map_or(0, |m| m.start())];
        format!("{prefix} ON CONFLICT DO UPDATE SET {translated_set}")
    } else {
        sql.to_string()
    }
}

/// Replace `VALUES(column_name)` references with `EXCLUDED.column_name`.
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
// 10. Miscellaneous MySQL syntax
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
        assert!(
            result.contains("ON CONFLICT"),
            "Should convert to ON CONFLICT: {result}"
        );
        assert!(
            result.contains("EXCLUDED."),
            "Should use EXCLUDED: {result}"
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
    fn test_backtick_to_double_quote() {
        let sql = "SELECT `id`, `name` FROM `wp_posts` WHERE `status` = 'publish'";
        let result = translate(sql);
        assert!(!result.contains('`'), "Backticks should be replaced");
        assert!(result.contains("\"id\""), "Should use double quotes");
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
            result.contains("\"col\""),
            "Backtick identifier converted: {result}"
        );
        // The backtick inside the single-quoted string literal must be
        // preserved as-is — it is not an identifier delimiter.
        assert!(
            result.contains("'it`s a test'"),
            "Backtick inside string literal preserved: {result}"
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
}
