//! Oracle SQL to PostgreSQL SQL Translator
//!
//! This module translates Oracle-specific SQL syntax to PostgreSQL-compatible SQL
//! that can be executed by HeliosDB-Lite's query engine.

use crate::{Error, Result};
use regex::Regex;
use std::sync::OnceLock;

/// Initialize a static regex pattern.
///
/// # Safety
/// This function uses `unwrap()` on regex compilation because:
/// - All patterns are compile-time string literals
/// - Invalid patterns will cause immediate startup failure (fail-fast)
/// - This poses zero runtime risk as patterns are validated once at initialization
#[allow(clippy::expect_used)]
fn init_regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static regex pattern must be valid")
}

/// Oracle SQL translator
pub struct OracleTranslator {
    // Regex patterns cached for performance
}

impl OracleTranslator {
    /// Create a new Oracle translator
    pub fn new() -> Self {
        Self {}
    }

    /// Translate Oracle SQL to PostgreSQL SQL
    pub fn translate(&self, oracle_sql: &str) -> Result<String> {
        let mut sql = oracle_sql.to_string();

        // Handle empty or whitespace-only queries
        if sql.trim().is_empty() {
            return Ok(sql);
        }

        // Apply translation rules in order
        sql = self.translate_dual_table(&sql)?;
        sql = self.translate_sysdate(&sql)?;
        sql = self.translate_systimestamp(&sql)?;
        sql = self.translate_decode(&sql)?;
        sql = self.translate_nvl(&sql)?;
        sql = self.translate_nvl2(&sql)?;
        sql = self.translate_sequence_nextval(&sql)?;
        sql = self.translate_rownum(&sql)?;
        sql = self.translate_concat_operator(&sql)?;
        sql = self.translate_outer_join(&sql)?;
        sql = self.translate_to_date(&sql)?;
        sql = self.translate_to_char(&sql)?;
        sql = self.translate_to_number(&sql)?;

        // Check for unsupported PL/SQL blocks
        self.check_plsql_blocks(&sql)?;

        Ok(sql)
    }

    /// Translate DUAL table references
    /// Oracle: SELECT 1 FROM DUAL
    /// PostgreSQL: SELECT 1
    fn translate_dual_table(&self, sql: &str) -> Result<String> {
        static DUAL_RE: OnceLock<Regex> = OnceLock::new();
        let re = DUAL_RE.get_or_init(|| {
            init_regex(r"(?i)\s+FROM\s+DUAL\s*($|;|\s+WHERE|\s+ORDER|\s+LIMIT)")
        });

        let result = re.replace_all(sql, "$1");
        Ok(result.to_string())
    }

    /// Translate SYSDATE to CURRENT_TIMESTAMP
    /// Oracle: SELECT SYSDATE FROM DUAL
    /// PostgreSQL: SELECT CURRENT_TIMESTAMP
    fn translate_sysdate(&self, sql: &str) -> Result<String> {
        static SYSDATE_RE: OnceLock<Regex> = OnceLock::new();
        let re = SYSDATE_RE.get_or_init(|| init_regex(r"(?i)\bSYSDATE\b"));

        let result = re.replace_all(sql, "CURRENT_TIMESTAMP");
        Ok(result.to_string())
    }

    /// Translate SYSTIMESTAMP to CURRENT_TIMESTAMP
    /// Oracle: SELECT SYSTIMESTAMP FROM DUAL
    /// PostgreSQL: SELECT CURRENT_TIMESTAMP
    fn translate_systimestamp(&self, sql: &str) -> Result<String> {
        static SYSTIMESTAMP_RE: OnceLock<Regex> = OnceLock::new();
        let re = SYSTIMESTAMP_RE.get_or_init(|| init_regex(r"(?i)\bSYSTIMESTAMP\b"));

        let result = re.replace_all(sql, "CURRENT_TIMESTAMP");
        Ok(result.to_string())
    }

    /// Translate DECODE to CASE expression
    /// Oracle: DECODE(col, 1, 'one', 2, 'two', 'other')
    /// PostgreSQL: CASE WHEN col = 1 THEN 'one' WHEN col = 2 THEN 'two' ELSE 'other' END
    fn translate_decode(&self, sql: &str) -> Result<String> {
        static DECODE_RE: OnceLock<Regex> = OnceLock::new();
        let re = DECODE_RE.get_or_init(|| init_regex(r"(?i)DECODE\s*\(([^)]+)\)"));

        let mut result = sql.to_string();

        // Simple DECODE translation (handles basic cases)
        // For complex nested DECODE, may need recursive parsing
        while let Some(cap) = re.captures(&result.clone()) {
            let args_str = &cap[1];
            let args: Vec<&str> = args_str.split(',').map(|s| s.trim()).collect();

            if args.len() < 3 {
                return Err(Error::query_execution(
                    "DECODE requires at least 3 arguments"
                ));
            }

            let expr = match args.first() {
                Some(e) => e,
                None => return Err(Error::query_execution("DECODE missing expression")),
            };
            let mut case_expr = format!("CASE");

            // Process value/result pairs
            let mut i = 1;
            while let (Some(value), Some(result_val)) = (args.get(i), args.get(i + 1)) {
                case_expr.push_str(&format!(" WHEN {} = {} THEN {}", expr, value, result_val));
                i += 2;
            }

            // Handle default value if present
            if let Some(default_val) = args.get(i) {
                case_expr.push_str(&format!(" ELSE {}", default_val));
            }

            case_expr.push_str(" END");

            // Replace this DECODE occurrence
            result = result.replacen(&cap[0], &case_expr, 1);
        }

        Ok(result)
    }

    /// Translate NVL to COALESCE
    /// Oracle: NVL(col, 0)
    /// PostgreSQL: COALESCE(col, 0)
    fn translate_nvl(&self, sql: &str) -> Result<String> {
        static NVL_RE: OnceLock<Regex> = OnceLock::new();
        let re = NVL_RE.get_or_init(|| init_regex(r"(?i)\bNVL\s*\("));

        let result = re.replace_all(sql, "COALESCE(");
        Ok(result.to_string())
    }

    /// Translate NVL2 to CASE expression
    /// Oracle: NVL2(col, 'not null', 'is null')
    /// PostgreSQL: CASE WHEN col IS NOT NULL THEN 'not null' ELSE 'is null' END
    fn translate_nvl2(&self, sql: &str) -> Result<String> {
        static NVL2_RE: OnceLock<Regex> = OnceLock::new();
        let re = NVL2_RE.get_or_init(|| {
            init_regex(r"(?i)NVL2\s*\(([^,]+),\s*([^,]+),\s*([^)]+)\)")
        });

        let result = re.replace_all(sql, "CASE WHEN $1 IS NOT NULL THEN $2 ELSE $3 END");
        Ok(result.to_string())
    }

    /// Translate sequence NEXTVAL
    /// Oracle: seq_name.NEXTVAL
    /// PostgreSQL: nextval('seq_name')
    fn translate_sequence_nextval(&self, sql: &str) -> Result<String> {
        static NEXTVAL_RE: OnceLock<Regex> = OnceLock::new();
        let re = NEXTVAL_RE.get_or_init(|| init_regex(r"(?i)(\w+)\.NEXTVAL"));

        let result = re.replace_all(sql, "nextval('$1')");
        Ok(result.to_string())
    }

    /// Translate ROWNUM to LIMIT
    /// Oracle: SELECT * FROM table WHERE ROWNUM <= 10
    /// PostgreSQL: SELECT * FROM table LIMIT 10
    ///
    /// Note: This is a simplified translation. Oracle ROWNUM behavior
    /// is more complex and may not match exactly in all cases.
    fn translate_rownum(&self, sql: &str) -> Result<String> {
        static ROWNUM_RE: OnceLock<Regex> = OnceLock::new();
        let re = ROWNUM_RE.get_or_init(|| init_regex(r"(?i)WHERE\s+ROWNUM\s*<=\s*(\d+)"));

        let result = if re.is_match(sql) {
            let with_limit = re.replace(sql, "");
            if let Some(cap) = re.captures(sql) {
                let limit = &cap[1];
                format!("{} LIMIT {}", with_limit, limit)
            } else {
                sql.to_string()
            }
        } else {
            sql.to_string()
        };

        Ok(result)
    }

    /// Translate concatenation operator
    /// Oracle: 'Hello' || ' ' || 'World'
    /// PostgreSQL: 'Hello' || ' ' || 'World' (same, but verify syntax)
    fn translate_concat_operator(&self, sql: &str) -> Result<String> {
        // PostgreSQL supports || operator, so no translation needed
        // This is a placeholder for potential future enhancements
        Ok(sql.to_string())
    }

    /// Translate outer join syntax
    /// Oracle: SELECT * FROM a, b WHERE a.id = b.id(+)
    /// PostgreSQL: SELECT * FROM a LEFT JOIN b ON a.id = b.id
    ///
    /// Note: This is a complex translation and this implementation
    /// handles only the detection and returns an error for now.
    fn translate_outer_join(&self, sql: &str) -> Result<String> {
        static OUTER_JOIN_RE: OnceLock<Regex> = OnceLock::new();
        let re = OUTER_JOIN_RE.get_or_init(|| init_regex(r"(?i)\(\+\)"));

        if re.is_match(sql) {
            return Err(Error::query_execution(
                "Oracle (+) outer join syntax not supported. Use ANSI JOIN syntax instead."
            ));
        }

        Ok(sql.to_string())
    }

    /// Translate TO_DATE function
    /// Oracle: TO_DATE('2024-01-01', 'YYYY-MM-DD')
    /// PostgreSQL: TO_TIMESTAMP('2024-01-01', 'YYYY-MM-DD')::DATE
    fn translate_to_date(&self, sql: &str) -> Result<String> {
        static TO_DATE_RE: OnceLock<Regex> = OnceLock::new();
        let re = TO_DATE_RE.get_or_init(|| init_regex(r"(?i)TO_DATE\s*\(([^)]+)\)"));

        let result = re.replace_all(sql, "TO_TIMESTAMP($1)::DATE");
        Ok(result.to_string())
    }

    /// Translate TO_CHAR function (basic date formatting)
    /// Oracle: TO_CHAR(date_col, 'YYYY-MM-DD')
    /// PostgreSQL: TO_CHAR(date_col, 'YYYY-MM-DD') (same format codes)
    fn translate_to_char(&self, sql: &str) -> Result<String> {
        // PostgreSQL TO_CHAR is compatible with Oracle for most format codes
        // No translation needed for basic cases
        Ok(sql.to_string())
    }

    /// Translate TO_NUMBER function
    /// Oracle: TO_NUMBER('123')
    /// PostgreSQL: CAST('123' AS NUMERIC)
    fn translate_to_number(&self, sql: &str) -> Result<String> {
        static TO_NUMBER_RE: OnceLock<Regex> = OnceLock::new();
        let re = TO_NUMBER_RE.get_or_init(|| init_regex(r"(?i)TO_NUMBER\s*\(([^)]+)\)"));

        let result = re.replace_all(sql, "CAST($1 AS NUMERIC)");
        Ok(result.to_string())
    }

    /// Check for PL/SQL blocks and return error
    fn check_plsql_blocks(&self, sql: &str) -> Result<()> {
        let sql_upper = sql.trim().to_uppercase();

        // Check for PL/SQL block keywords
        if sql_upper.starts_with("BEGIN") ||
           sql_upper.starts_with("DECLARE") ||
           sql_upper.contains("BEGIN\n") ||
           sql_upper.contains("DECLARE\n") {
            return Err(Error::query_execution(
                "PL/SQL blocks are not supported. Use simple SQL statements instead."
            ));
        }

        // Check for stored procedure calls
        if sql_upper.starts_with("EXECUTE") ||
           sql_upper.starts_with("EXEC") ||
           sql_upper.starts_with("CALL") {
            return Err(Error::query_execution(
                "Stored procedure execution not yet supported."
            ));
        }

        Ok(())
    }

    /// Get list of supported Oracle features
    pub fn supported_features() -> Vec<&'static str> {
        vec![
            "DUAL table emulation",
            "SYSDATE function",
            "SYSTIMESTAMP function",
            "DECODE function (basic cases)",
            "NVL function",
            "NVL2 function",
            "Sequence NEXTVAL",
            "ROWNUM (simple cases)",
            "TO_DATE function",
            "TO_NUMBER function",
            "Concatenation operator (||)",
        ]
    }

    /// Get list of unsupported Oracle features
    pub fn unsupported_features() -> Vec<&'static str> {
        vec![
            "PL/SQL blocks",
            "Stored procedures",
            "Packages",
            "Triggers (Oracle syntax)",
            "Oracle (+) outer join syntax",
            "CONNECT BY hierarchical queries",
            "MERGE statement",
            "Advanced DECODE with nested functions",
            "ROWNUM with complex predicates",
            "Flashback queries",
            "Advanced partitioning syntax",
        ]
    }
}

impl Default for OracleTranslator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_translate_dual() {
        let translator = OracleTranslator::new();
        let oracle_sql = "SELECT 1 FROM DUAL";
        let pg_sql = translator.translate(oracle_sql).unwrap();
        assert_eq!(pg_sql.trim(), "SELECT 1");
    }

    #[test]
    fn test_translate_sysdate() {
        let translator = OracleTranslator::new();
        let oracle_sql = "SELECT SYSDATE FROM DUAL";
        let pg_sql = translator.translate(oracle_sql).unwrap();
        assert!(pg_sql.contains("CURRENT_TIMESTAMP"));
    }

    #[test]
    fn test_translate_nvl() {
        let translator = OracleTranslator::new();
        let oracle_sql = "SELECT NVL(col, 0) FROM table";
        let pg_sql = translator.translate(oracle_sql).unwrap();
        assert!(pg_sql.contains("COALESCE(col, 0)"));
    }

    #[test]
    fn test_translate_decode() {
        let translator = OracleTranslator::new();
        let oracle_sql = "SELECT DECODE(status, 1, 'active', 0, 'inactive', 'unknown') FROM table";
        let pg_sql = translator.translate(oracle_sql).unwrap();
        assert!(pg_sql.contains("CASE WHEN"));
        assert!(pg_sql.contains("END"));
    }

    #[test]
    fn test_plsql_block_detection() {
        let translator = OracleTranslator::new();
        let plsql = "BEGIN\n  SELECT 1;\nEND;";
        let result = translator.translate(plsql);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("PL/SQL"));
    }

    #[test]
    fn test_translate_sequence_nextval() {
        let translator = OracleTranslator::new();
        let oracle_sql = "SELECT my_seq.NEXTVAL FROM DUAL";
        let pg_sql = translator.translate(oracle_sql).unwrap();
        assert!(pg_sql.contains("nextval('my_seq')"));
    }
}
