//! SQL parser using sqlparser-rs

use crate::{Result, Error, ColumnStorageMode};
use sqlparser::ast::Statement;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser as SqlParser;

/// SQL parser
pub struct Parser {
    dialect: PostgreSqlDialect,
}

impl Parser {
    /// Create a new parser
    pub fn new() -> Self {
        Self {
            dialect: PostgreSqlDialect {},
        }
    }

    /// Preprocess SQL to handle Phase 3 time-travel syntax
    ///
    /// sqlparser doesn't support AS OF or VERSIONS BETWEEN syntax, so we
    /// temporarily remove them to allow parsing, then restore it later for
    /// the planner to extract.
    fn preprocess_time_travel_sql(&self, sql: &str) -> String {
        let upper = sql.to_uppercase();

        // Handle VERSIONS BETWEEN first (it's more specific)
        if upper.contains("VERSIONS BETWEEN") {
            return self.preprocess_versions_between(sql);
        }

        // Handle AS OF
        if !upper.contains(" AS OF") && !upper.contains("AS OF ") {
            return sql.to_string();
        }

        // Find AS OF and remove the clause
        if let Some(as_of_pos) = upper.find("AS OF") {
            // Keep everything before AS OF
            let before = sql[..as_of_pos].trim_end();

            // Find where AS OF clause ends (at next keyword or end of statement)
            let after_as_of = &sql[as_of_pos + 5..]; // "AS OF".len() = 5
            let upper_after = after_as_of.to_uppercase();

            // Look for keywords that end the AS OF clause
            let end_keywords = [
                "WHERE", "GROUP", "ORDER", "LIMIT", "UNION",
                "INTERSECT", "EXCEPT", ")", ";", "HAVING"
            ];

            let mut end_pos = after_as_of.len();
            for keyword in &end_keywords {
                if let Some(pos) = upper_after.find(keyword) {
                    // Make sure it's a word boundary (preceded by space or parenthesis)
                    if pos == 0 || after_as_of.chars().nth(pos - 1).map(|c| c.is_whitespace() || c == ')').unwrap_or(false) {
                        end_pos = pos;
                        break;
                    }
                }
            }

            let after = after_as_of[end_pos..].trim_start();

            if after.is_empty() {
                before.to_string()
            } else {
                format!("{} {}", before, after)
            }
        } else {
            sql.to_string()
        }
    }

    /// Preprocess VERSIONS BETWEEN clause for sqlparser compatibility
    ///
    /// Removes: VERSIONS BETWEEN TIMESTAMP '...' AND TIMESTAMP '...'
    /// from the SQL to allow sqlparser to parse the basic query structure.
    fn preprocess_versions_between(&self, sql: &str) -> String {
        let upper = sql.to_uppercase();

        if let Some(versions_pos) = upper.find("VERSIONS BETWEEN") {
            // Keep everything before VERSIONS BETWEEN
            let before = sql[..versions_pos].trim_end();

            // Find where VERSIONS BETWEEN clause ends
            // The clause ends after "AND TIMESTAMP '...'" or "AND NOW" or "AND SCN ..."
            let after_versions = &sql[versions_pos..];
            let upper_after = after_versions.to_uppercase();

            // Look for the AND keyword, then find end of the second timestamp/value
            if let Some(and_pos) = upper_after.find(" AND ") {
                let after_and = &after_versions[and_pos + 5..]; // " AND ".len() = 5
                let upper_after_and = after_and.to_uppercase();

                // Find end of the second clause (TIMESTAMP '...', NOW, SCN ...)
                let end_pos = if upper_after_and.starts_with("TIMESTAMP") {
                    // Find the closing quote
                    if let Some(quote_start) = after_and.find('\'') {
                        if let Some(quote_end) = after_and[quote_start + 1..].find('\'') {
                            quote_start + 1 + quote_end + 1
                        } else {
                            after_and.len()
                        }
                    } else {
                        after_and.len()
                    }
                } else if upper_after_and.starts_with("NOW") {
                    3 // "NOW".len()
                } else if upper_after_and.starts_with("SCN") || upper_after_and.starts_with("TRANSACTION") {
                    // Find end of number
                    let num_start = after_and.find(char::is_numeric).unwrap_or(after_and.len());
                    if num_start < after_and.len() {
                        let after_num = &after_and[num_start..];
                        num_start + after_num.find(|c: char| !c.is_numeric()).unwrap_or(after_num.len())
                    } else {
                        after_and.len()
                    }
                } else {
                    after_and.len()
                };

                let total_skip = versions_pos + (and_pos + 5) + end_pos;
                let after = sql[total_skip..].trim_start();

                if after.is_empty() {
                    before.to_string()
                } else {
                    format!("{} {}", before, after)
                }
            } else {
                // No AND found - malformed, return as-is
                sql.to_string()
            }
        } else {
            sql.to_string()
        }
    }

    /// Strip SQL comments from input
    /// Handles both line comments (-- ...) and block comments (/* ... */)
    fn strip_sql_comments(sql: &str) -> String {
        let mut result = String::with_capacity(sql.len());
        let chars: Vec<char> = sql.chars().collect();
        let mut i = 0;
        let mut in_single_quote = false;
        let mut in_double_quote = false;

        // SAFETY: All indexing below is guarded by `while i < chars.len()` and
        // `i + 1 < chars.len()` checks that structurally guarantee bounds.
        #[allow(clippy::indexing_slicing)]
        while i < chars.len() {
            // Handle string literals (don't strip comments inside strings)
            if chars[i] == '\'' && !in_double_quote {
                in_single_quote = !in_single_quote;
                result.push(chars[i]);
                i += 1;
                continue;
            }
            if chars[i] == '"' && !in_single_quote {
                in_double_quote = !in_double_quote;
                result.push(chars[i]);
                i += 1;
                continue;
            }

            // Skip comments only when not inside a string
            if !in_single_quote && !in_double_quote {
                // Line comment: -- until end of line
                if i + 1 < chars.len() && chars[i] == '-' && chars[i + 1] == '-' {
                    // Skip to end of line
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                    // Keep the newline if it exists
                    if i < chars.len() {
                        result.push('\n');
                        i += 1;
                    }
                    continue;
                }
                // Block comment: /* ... */
                if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '*' {
                    i += 2; // Skip /*
                    // Find closing */
                    while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                        i += 1;
                    }
                    if i + 1 < chars.len() {
                        i += 2; // Skip */
                    }
                    // Add a space to prevent tokens from merging
                    result.push(' ');
                    continue;
                }
            }

            result.push(chars[i]);
            i += 1;
        }

        result
    }

    /// Parse a SQL statement
    pub fn parse(&self, sql: &str) -> Result<Vec<Statement>> {
        // Strip SQL comments first
        let sql_no_comments = Self::strip_sql_comments(sql);

        // If the result is only whitespace (comment-only line), return empty vec
        if sql_no_comments.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Preprocess to remove time-travel syntax for parsing
        let mut processed_sql = self.preprocess_time_travel_sql(&sql_no_comments);

        // Preprocess DECIMAL to NUMERIC for sqlparser compatibility
        processed_sql = Self::preprocess_decimal_to_numeric(&processed_sql);

        // Preprocess to remove SECURITY DEFINER/INVOKER (not supported by sqlparser)
        processed_sql = Self::preprocess_remove_security_clause(&processed_sql);

        // Preprocess to remove STORAGE clauses from column definitions (not supported by sqlparser)
        processed_sql = Self::preprocess_remove_storage_clauses(&processed_sql);

        // Preprocess CREATE INDEX USING clause for sqlparser compatibility
        let index_type_override = if Self::is_create_index_using(&processed_sql) {
            let (cleaned_sql, index_type) = Self::preprocess_create_index_using(&processed_sql);
            processed_sql = cleaned_sql;
            index_type
        } else {
            None
        };

        let mut statements = SqlParser::parse_sql(&self.dialect, &processed_sql)
            .map_err(|e| Error::sql_parse(format!("Failed to parse SQL: {}", e)))?;

        // If we extracted an index type from USING clause, inject it into the CreateIndex statement
        if let Some(index_type) = index_type_override {
            for statement in &mut statements {
                if let Statement::CreateIndex(create_index) = statement {
                    // Create an Identifier from the extracted index type
                    use sqlparser::ast::Ident;
                    create_index.using = Some(Ident::new(index_type.clone()));
                }
            }
        }

        Ok(statements)
    }

    /// Parse a single SQL statement
    pub fn parse_one(&self, sql: &str) -> Result<Statement> {
        let statements = self.parse(sql)?;

        if statements.is_empty() {
            return Err(Error::sql_parse("No SQL statement found"));
        }

        if statements.len() > 1 {
            return Err(Error::sql_parse("Multiple statements found, expected one"));
        }

        // Safe to unwrap here because we checked len() == 1, but use ok_or for safety
        statements.into_iter().next()
            .ok_or_else(|| Error::sql_parse("Unexpected: statement vector empty after length check"))
    }

    /// Check if SQL is a CREATE DATABASE BRANCH statement
    pub fn is_create_branch(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("CREATE DATABASE BRANCH") || upper.starts_with("CREATE BRANCH")
    }

    /// Check if SQL is a DROP DATABASE BRANCH statement
    pub fn is_drop_branch(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("DROP DATABASE BRANCH") || upper.starts_with("DROP BRANCH")
    }

    /// Check if SQL is a MERGE DATABASE BRANCH statement
    pub fn is_merge_branch(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("MERGE DATABASE BRANCH") || upper.starts_with("MERGE BRANCH")
    }

    /// Check if SQL is a USE BRANCH statement
    pub fn is_use_branch(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("USE BRANCH") || upper.starts_with("USE DATABASE BRANCH")
    }

    /// Check if SQL is a SHOW BRANCHES statement
    pub fn is_show_branches(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("SHOW BRANCHES") || upper.starts_with("SHOW DATABASE BRANCHES")
    }

    /// Check if SQL is a REFRESH MATERIALIZED VIEW statement
    pub fn is_refresh_materialized_view(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("REFRESH MATERIALIZED VIEW")
    }

    /// Parse REFRESH MATERIALIZED VIEW statement
    ///
    /// Syntax:
    /// - REFRESH MATERIALIZED VIEW `<name>`
    /// - REFRESH MATERIALIZED VIEW CONCURRENTLY `<name>`
    /// - REFRESH MATERIALIZED VIEW `<name>` INCREMENTALLY
    /// - REFRESH MATERIALIZED VIEW CONCURRENTLY `<name>` INCREMENTALLY
    ///
    /// Returns: (view_name, concurrent, incremental)
    pub fn parse_refresh_materialized_view_sql(sql: &str) -> Result<(String, bool, bool)> {
        let cleaned = sql.trim().to_string();

        // Skip "REFRESH MATERIALIZED VIEW"
        let after_refresh = cleaned["REFRESH MATERIALIZED VIEW".len()..].trim_start();
        let upper_after = after_refresh.to_uppercase();

        // Check for CONCURRENTLY
        let concurrent = upper_after.starts_with("CONCURRENTLY");
        let after_concurrent = if concurrent {
            after_refresh["CONCURRENTLY".len()..].trim_start()
        } else {
            after_refresh
        };

        // Check for INCREMENTALLY at the end
        let upper_remaining = after_concurrent.to_uppercase();
        let incremental = upper_remaining.ends_with("INCREMENTALLY")
            || upper_remaining.ends_with("INCREMENTALLY;");

        // Remove INCREMENTALLY from the end if present
        let without_incremental = if incremental {
            let upper = after_concurrent.to_uppercase();
            let inc_pos = upper.rfind("INCREMENTALLY").unwrap_or(after_concurrent.len());
            after_concurrent[..inc_pos].trim_end()
        } else {
            after_concurrent.trim_end_matches(';').trim_end()
        };

        // Extract view name
        let name_end = without_incremental.find(|c: char| c.is_whitespace() || c == ';')
            .unwrap_or(without_incremental.len());
        let view_name = without_incremental[..name_end].trim().to_string();

        if view_name.is_empty() {
            return Err(Error::query_execution("REFRESH MATERIALIZED VIEW requires a view name"));
        }

        Ok((view_name, concurrent, incremental))
    }

    /// Check if SQL is a DROP MATERIALIZED VIEW statement
    pub fn is_drop_materialized_view(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("DROP MATERIALIZED VIEW")
    }

    /// Parse DROP MATERIALIZED VIEW statement
    ///
    /// Syntax:
    /// - DROP MATERIALIZED VIEW `<name>`
    /// - DROP MATERIALIZED VIEW IF EXISTS `<name>`
    pub fn parse_drop_materialized_view_sql(sql: &str) -> Result<(String, bool)> {
        let cleaned = sql.trim().to_string();

        // Skip "DROP MATERIALIZED VIEW"
        let after_drop = cleaned["DROP MATERIALIZED VIEW".len()..].trim_start();
        let upper_after = after_drop.to_uppercase();

        // Check for IF EXISTS
        let if_exists = upper_after.starts_with("IF EXISTS");
        let remaining = if if_exists {
            after_drop["IF EXISTS".len()..].trim_start()
        } else {
            after_drop
        };

        // Extract view name
        let name_end = remaining.find(|c: char| c.is_whitespace() || c == ';')
            .unwrap_or(remaining.len());
        let view_name = remaining[..name_end].trim().to_string();

        if view_name.is_empty() {
            return Err(Error::query_execution("DROP MATERIALIZED VIEW requires a view name"));
        }

        Ok((view_name, if_exists))
    }

    /// Check if SQL is an ALTER MATERIALIZED VIEW statement
    pub fn is_alter_materialized_view(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("ALTER MATERIALIZED VIEW")
    }

    /// Parse ALTER MATERIALIZED VIEW statement
    ///
    /// Syntax:
    /// - ALTER MATERIALIZED VIEW `<name>` SET (option = value, ...)
    ///
    /// Supported options:
    /// - staleness_threshold = `<seconds>`
    /// - max_cpu_percent = `<percent>`
    /// - refresh_strategy = 'manual' | 'auto' | 'incremental'
    /// - priority = <0-10>
    /// - incremental_enabled = true | false
    pub fn parse_alter_materialized_view_sql(sql: &str) -> Result<(String, std::collections::HashMap<String, String>)> {
        let cleaned = sql.trim().to_string();

        // Skip "ALTER MATERIALIZED VIEW"
        let after_alter = cleaned["ALTER MATERIALIZED VIEW".len()..].trim_start();

        // Extract view name (ends at SET or whitespace)
        let upper_after = after_alter.to_uppercase();
        let set_pos = upper_after.find(" SET ");

        let view_name = if let Some(pos) = set_pos {
            after_alter[..pos].trim().to_string()
        } else {
            return Err(Error::query_execution("ALTER MATERIALIZED VIEW requires SET clause"));
        };

        if view_name.is_empty() {
            return Err(Error::query_execution("ALTER MATERIALIZED VIEW requires a view name"));
        }

        // Parse the SET clause
        let set_pos = set_pos.unwrap_or_else(|| unreachable!());
        let after_set = after_alter[set_pos + 5..].trim_start(); // 5 = " SET ".len()

        // Find options within parentheses
        let options_str = if after_set.starts_with('(') {
            let end_paren = after_set.rfind(')');
            if let Some(end) = end_paren {
                &after_set[1..end]
            } else {
                return Err(Error::query_execution("ALTER MATERIALIZED VIEW SET requires closing parenthesis"));
            }
        } else {
            // Options without parentheses (single option)
            after_set.trim_end_matches(';').trim()
        };

        // Parse key=value pairs
        let mut options = std::collections::HashMap::new();
        for pair in options_str.split(',') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }

            let parts: Vec<&str> = pair.splitn(2, '=').collect();
            if parts.len() != 2 {
                return Err(Error::query_execution(format!(
                    "Invalid option format '{}', expected 'key = value'", pair
                )));
            }

            let key = parts.get(0).ok_or_else(|| Error::query_execution(
                format!("Invalid option format '{}', expected 'key = value'", pair)
            ))?.trim().to_lowercase();
            let value = parts.get(1).ok_or_else(|| Error::query_execution(
                format!("Invalid option format '{}', expected 'key = value'", pair)
            ))?.trim().trim_matches('\'').trim_matches('"').to_string();

            // Validate known options
            match key.as_str() {
                "staleness_threshold" | "max_cpu_percent" | "priority" => {
                    // Validate numeric
                    if value.parse::<f64>().is_err() {
                        return Err(Error::query_execution(format!(
                            "Option '{}' requires a numeric value, got '{}'", key, value
                        )));
                    }
                }
                "refresh_strategy" => {
                    let lower = value.to_lowercase();
                    if !["manual", "auto", "incremental"].contains(&lower.as_str()) {
                        return Err(Error::query_execution(format!(
                            "refresh_strategy must be 'manual', 'auto', or 'incremental', got '{}'", value
                        )));
                    }
                }
                "incremental_enabled" => {
                    let lower = value.to_lowercase();
                    if !["true", "false"].contains(&lower.as_str()) {
                        return Err(Error::query_execution(format!(
                            "incremental_enabled must be 'true' or 'false', got '{}'", value
                        )));
                    }
                }
                _ => {
                    // Allow unknown options for future extensibility
                    tracing::debug!("Unknown ALTER MATERIALIZED VIEW option: {}", key);
                }
            }

            options.insert(key, value);
        }

        if options.is_empty() {
            return Err(Error::query_execution("ALTER MATERIALIZED VIEW SET requires at least one option"));
        }

        Ok((view_name, options))
    }

    /// Check if SQL is an ALTER TABLE ALTER COLUMN SET STORAGE statement
    ///
    /// Syntax: ALTER TABLE `<table>` ALTER COLUMN `<column>` SET STORAGE `<mode>`
    pub fn is_alter_column_storage(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("ALTER TABLE") &&
        upper.contains("ALTER COLUMN") &&
        upper.contains("SET STORAGE")
    }

    /// Parse ALTER TABLE ALTER COLUMN SET STORAGE statement
    ///
    /// Syntax: ALTER TABLE `<table_name>` ALTER COLUMN `<column_name>` SET STORAGE `<mode>`
    ///
    /// Supported storage modes:
    /// - DEFAULT: Standard row-oriented storage
    /// - DICTIONARY: Dictionary-encoded strings for low-cardinality columns
    /// - CONTENT_ADDRESSED: Hash-based deduplication for large values
    /// - COLUMNAR: Column-grouped storage for analytics workloads
    pub fn parse_alter_column_storage(sql: &str) -> Result<(String, String, ColumnStorageMode)> {
        let cleaned = sql.trim();

        // Skip "ALTER TABLE"
        let after_alter = cleaned.get(11..).ok_or_else(||
            Error::query_execution("Invalid ALTER TABLE statement")
        )?.trim_start();

        // Extract table name (ends at ALTER)
        let upper_after = after_alter.to_uppercase();
        let alter_pos = upper_after.find(" ALTER ").ok_or_else(||
            Error::query_execution("ALTER TABLE requires ALTER COLUMN clause")
        )?;

        let table_name = after_alter[..alter_pos].trim().to_string();
        if table_name.is_empty() {
            return Err(Error::query_execution("ALTER TABLE requires a table name"));
        }

        // Skip " ALTER COLUMN "
        let after_column = after_alter.get(alter_pos + 7..).ok_or_else(||
            Error::query_execution("Invalid ALTER COLUMN clause")
        )?.trim_start();

        let upper_column = after_column.to_uppercase();
        if !upper_column.starts_with("COLUMN ") {
            return Err(Error::query_execution("Expected COLUMN keyword after ALTER"));
        }

        let after_col_keyword = after_column.get(7..).ok_or_else(||
            Error::query_execution("Invalid ALTER COLUMN clause")
        )?.trim_start();

        // Find SET STORAGE
        let upper_rest = after_col_keyword.to_uppercase();
        let set_pos = upper_rest.find(" SET STORAGE").ok_or_else(||
            Error::query_execution("ALTER COLUMN requires SET STORAGE clause")
        )?;

        let column_name = after_col_keyword[..set_pos].trim().to_string();
        if column_name.is_empty() {
            return Err(Error::query_execution("ALTER COLUMN requires a column name"));
        }

        // Extract storage mode (after " SET STORAGE ")
        let after_storage = after_col_keyword.get(set_pos + 12..).ok_or_else(||
            Error::query_execution("Invalid SET STORAGE clause")
        )?.trim_start();

        let mode_str = after_storage.trim_end_matches(';').trim().to_uppercase();

        let storage_mode = match mode_str.as_str() {
            "DEFAULT" => ColumnStorageMode::Default,
            "DICTIONARY" => ColumnStorageMode::Dictionary,
            "CONTENT_ADDRESSED" => ColumnStorageMode::ContentAddressed,
            "COLUMNAR" => ColumnStorageMode::Columnar,
            _ => return Err(Error::query_execution(format!(
                "Invalid storage mode '{}'. Expected: DEFAULT, DICTIONARY, CONTENT_ADDRESSED, or COLUMNAR",
                mode_str
            ))),
        };

        Ok((table_name, column_name, storage_mode))
    }

    /// Extract column storage modes from CREATE TABLE SQL
    ///
    /// Parses STORAGE DICTIONARY, STORAGE CONTENT_ADDRESSED, and STORAGE COLUMNAR
    /// clauses from column definitions in CREATE TABLE statements.
    ///
    /// Returns: HashMap<column_name, ColumnStorageMode>
    pub fn extract_column_storage_modes(sql: &str) -> std::collections::HashMap<String, ColumnStorageMode> {
        use std::collections::HashMap;

        let mut modes: HashMap<String, ColumnStorageMode> = HashMap::new();
        let upper = sql.to_uppercase();

        // Only process CREATE TABLE statements
        if !upper.trim_start().starts_with("CREATE TABLE") {
            return modes;
        }

        // Find the column definitions section (between first ( and matching ))
        let paren_start = match sql.find('(') {
            Some(pos) => pos + 1,
            None => return modes,
        };

        // Find the matching close paren
        let mut depth = 1;
        let mut paren_end = sql.len();
        for (i, c) in sql[paren_start..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        paren_end = paren_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }

        let columns_section = &sql[paren_start..paren_end];

        // Split by comma (but be careful of nested parentheses)
        let column_defs = Self::split_column_defs(columns_section);

        for col_def in column_defs {
            let col_upper = col_def.to_uppercase();

            // Check for STORAGE clause
            if let Some(storage_pos) = col_upper.find(" STORAGE ") {
                // Extract column name (first identifier)
                let col_trimmed = col_def.trim();
                let col_name = col_trimmed.split_whitespace().next().unwrap_or("");

                // Skip if it looks like a constraint (PRIMARY, FOREIGN, UNIQUE, CHECK)
                let first_word = col_name.to_uppercase();
                if first_word == "PRIMARY" || first_word == "FOREIGN" ||
                   first_word == "UNIQUE" || first_word == "CHECK" ||
                   first_word == "CONSTRAINT" {
                    continue;
                }

                // Extract storage mode
                let after_storage = &col_upper[storage_pos + 9..]; // " STORAGE ".len() = 9
                let mode_end = after_storage.find(|c: char| !c.is_alphabetic() && c != '_')
                    .unwrap_or(after_storage.len());
                let mode_str = after_storage[..mode_end].trim();

                let storage_mode = match mode_str {
                    "DICTIONARY" => ColumnStorageMode::Dictionary,
                    "CONTENT_ADDRESSED" => ColumnStorageMode::ContentAddressed,
                    "COLUMNAR" => ColumnStorageMode::Columnar,
                    "DEFAULT" => ColumnStorageMode::Default,
                    _ => continue, // Unknown mode, skip
                };

                modes.insert(col_name.to_string(), storage_mode);
            }
        }

        modes
    }

    /// Remove STORAGE clauses from CREATE TABLE SQL for sqlparser compatibility
    ///
    /// sqlparser doesn't support PostgreSQL-style STORAGE clauses in column definitions,
    /// so we remove them before parsing and extract them separately.
    pub fn preprocess_remove_storage_clauses(sql: &str) -> String {
        let upper = sql.to_uppercase();

        // Only process CREATE TABLE statements
        if !upper.trim_start().starts_with("CREATE TABLE") {
            return sql.to_string();
        }

        let mut result = sql.to_string();

        // Remove all variations of STORAGE clause
        for mode in &["STORAGE DICTIONARY", "STORAGE CONTENT_ADDRESSED", "STORAGE COLUMNAR", "STORAGE DEFAULT"] {
            loop {
                let upper_result = result.to_uppercase();
                if let Some(pos) = upper_result.find(mode) {
                    // Remove the STORAGE clause and any following whitespace
                    let end_pos = pos + mode.len();
                    let before = &result[..pos];
                    let after = &result[end_pos..];
                    result = format!("{}{}", before.trim_end(), after);
                } else {
                    break;
                }
            }
        }

        result
    }

    /// Split column definitions by comma, respecting parentheses
    fn split_column_defs(section: &str) -> Vec<&str> {
        let mut result = Vec::new();
        let mut depth: i32 = 0;
        let mut start = 0;

        for (i, c) in section.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => depth = (depth - 1).max(0),
                ',' if depth == 0 => {
                    result.push(section[start..i].trim());
                    start = i + 1;
                }
                _ => {}
            }
        }

        // Don't forget the last segment
        let last = section[start..].trim();
        if !last.is_empty() {
            result.push(last);
        }

        result
    }

    /// Remove SECURITY DEFINER/INVOKER from SQL for sqlparser compatibility
    ///
    /// PostgreSQL supports SECURITY DEFINER and SECURITY INVOKER clauses on functions,
    /// but sqlparser doesn't parse these. We remove them to allow parsing.
    fn preprocess_remove_security_clause(sql: &str) -> String {
        let upper = sql.to_uppercase();

        // Check if SECURITY clause exists
        if !upper.contains("SECURITY DEFINER") && !upper.contains("SECURITY INVOKER") {
            return sql.to_string();
        }

        let mut result = sql.to_string();

        // Remove SECURITY DEFINER (case-insensitive)
        if let Some(pos) = result.to_uppercase().find("SECURITY DEFINER") {
            result = format!("{}{}", &result[..pos].trim_end(), &result[pos + 16..]);
        }

        // Remove SECURITY INVOKER (case-insensitive)
        if let Some(pos) = result.to_uppercase().find("SECURITY INVOKER") {
            result = format!("{}{}", &result[..pos].trim_end(), &result[pos + 16..]);
        }

        result
    }

    /// Check if SQL is a PostgreSQL-style CREATE PROCEDURE statement
    ///
    /// PostgreSQL uses: CREATE PROCEDURE name(...) LANGUAGE plpgsql AS $$...$$
    /// sqlparser expects: CREATE PROCEDURE name(...) AS BEGIN ... END
    pub fn is_pg_create_procedure(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("CREATE PROCEDURE") &&
        upper.contains("LANGUAGE") &&
        (upper.contains(" AS ") || upper.contains(" AS$"))
    }

    /// Check if SQL is a PostgreSQL-style CREATE OR REPLACE PROCEDURE statement
    pub fn is_pg_create_or_replace_procedure(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("CREATE OR REPLACE PROCEDURE") &&
        upper.contains("LANGUAGE") &&
        (upper.contains(" AS ") || upper.contains(" AS$"))
    }

    /// Parse PostgreSQL-style CREATE [OR REPLACE] PROCEDURE statement
    ///
    /// Syntax: CREATE [OR REPLACE] PROCEDURE name(params) LANGUAGE lang AS $$body$$
    ///
    /// Returns: (name, or_replace, params, language, body)
    pub fn parse_pg_create_procedure(sql: &str) -> Result<(String, bool, Vec<(String, String)>, String, String)> {
        let cleaned = sql.trim().to_string();
        let upper = cleaned.to_uppercase();

        // Check for OR REPLACE
        let or_replace = upper.starts_with("CREATE OR REPLACE PROCEDURE");

        // Find start of procedure name
        let name_start = if or_replace {
            "CREATE OR REPLACE PROCEDURE".len()
        } else {
            "CREATE PROCEDURE".len()
        };

        let after_create = cleaned[name_start..].trim_start();

        // Find the opening parenthesis for parameters
        let paren_pos = after_create.find('(')
            .ok_or_else(|| Error::sql_parse("CREATE PROCEDURE requires parameter list"))?;

        let proc_name = after_create[..paren_pos].trim().to_string();

        if proc_name.is_empty() {
            return Err(Error::sql_parse("CREATE PROCEDURE requires a name"));
        }

        // Find matching closing parenthesis
        let after_name = &after_create[paren_pos..];
        let close_paren = Self::find_matching_paren(after_name)
            .ok_or_else(|| Error::sql_parse("Unmatched parenthesis in parameter list"))?;

        // Extract parameters
        let params_str = &after_name[1..close_paren]; // Skip opening paren
        let params = Self::parse_procedure_params(params_str)?;

        // Parse rest: LANGUAGE lang AS $$body$$
        let after_params = after_name[close_paren + 1..].trim_start();
        let upper_after = after_params.to_uppercase();

        // Find LANGUAGE
        let lang_pos = upper_after.find("LANGUAGE")
            .ok_or_else(|| Error::sql_parse("CREATE PROCEDURE requires LANGUAGE clause"))?;

        let after_lang = after_params[lang_pos + 8..].trim_start(); // "LANGUAGE".len() = 8

        // Extract language name (ends at whitespace or AS)
        let lang_end = after_lang.find(|c: char| c.is_whitespace())
            .unwrap_or(after_lang.len());
        let language = after_lang[..lang_end].trim().to_string();

        // Find AS
        let after_lang_name = after_lang[lang_end..].trim_start();
        let upper_remaining = after_lang_name.to_uppercase();

        if !upper_remaining.starts_with("AS") {
            return Err(Error::sql_parse("CREATE PROCEDURE requires AS clause after LANGUAGE"));
        }

        let after_as = after_lang_name[2..].trim_start(); // "AS".len() = 2

        // Extract body (either dollar-quoted or single-quoted)
        let body = Self::extract_procedure_body(after_as)?;

        Ok((proc_name, or_replace, params, language, body))
    }

    /// Find matching closing parenthesis
    fn find_matching_paren(s: &str) -> Option<usize> {
        let mut depth = 0;
        for (i, c) in s.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Parse procedure parameters
    fn parse_procedure_params(params_str: &str) -> Result<Vec<(String, String)>> {
        let mut params = Vec::new();

        if params_str.trim().is_empty() {
            return Ok(params);
        }

        for param in params_str.split(',') {
            let param = param.trim();
            if param.is_empty() {
                continue;
            }

            // Skip IN/OUT/INOUT mode if present
            let upper_param = param.to_uppercase();
            let param_content = if upper_param.starts_with("IN ") || upper_param.starts_with("OUT ") {
                param[3..].trim()
            } else if upper_param.starts_with("INOUT ") {
                param[6..].trim()
            } else {
                param
            };

            // Split name and type
            let parts: Vec<&str> = param_content.splitn(2, char::is_whitespace).collect();
            if parts.len() >= 2 {
                if let (Some(name), Some(typ)) = (parts.get(0), parts.get(1)) {
                    params.push((name.trim().to_string(), typ.trim().to_string()));
                }
            } else if let Some(typ) = parts.first() {
                // Type only (unnamed parameter)
                params.push(("".to_string(), typ.trim().to_string()));
            }
        }

        Ok(params)
    }

    /// Extract procedure body from dollar-quoted or single-quoted string
    fn extract_procedure_body(s: &str) -> Result<String> {
        let trimmed = s.trim();

        // Dollar quoting: $$...$$ or $tag$...$tag$
        if trimmed.starts_with('$') {
            // Find the end of opening delimiter
            let delim_end = if trimmed.starts_with("$$") {
                2
            } else {
                // Custom tag: $tag$
                trimmed[1..].find('$').map(|p| p + 2).unwrap_or(0)
            };

            if delim_end == 0 {
                return Err(Error::sql_parse("Invalid dollar quoting in procedure body"));
            }

            let delimiter = &trimmed[..delim_end];
            let body_start = delim_end;

            // Find closing delimiter
            if let Some(body_end) = trimmed[body_start..].find(delimiter) {
                let body = trimmed[body_start..body_start + body_end].to_string();
                return Ok(body);
            } else {
                return Err(Error::sql_parse("Unterminated dollar-quoted string in procedure body"));
            }
        }

        // Single-quoted string
        if trimmed.starts_with('\'') {
            // Find matching closing quote (handle escaped quotes)
            let mut i = 1;
            let chars: Vec<char> = trimmed.chars().collect();
            // SAFETY: All indexing below is guarded by `while i < chars.len()` and
            // `i + 1 < chars.len()` checks that structurally guarantee bounds.
            #[allow(clippy::indexing_slicing)]
            while i < chars.len() {
                if chars[i] == '\'' {
                    if i + 1 < chars.len() && chars[i + 1] == '\'' {
                        // Escaped quote
                        i += 2;
                    } else {
                        // End of string
                        let body: String = chars[1..i].iter().collect();
                        // Unescape doubled quotes
                        return Ok(body.replace("''", "'"));
                    }
                } else {
                    i += 1;
                }
            }
            return Err(Error::sql_parse("Unterminated string in procedure body"));
        }

        Err(Error::sql_parse("Procedure body must be quoted with $$ or single quotes"))
    }

    /// Check if SQL is a CREATE INDEX with USING clause
    pub fn is_create_index_using(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.contains("CREATE INDEX") && upper.contains(" USING ")
    }

    /// Remove USING clause from CREATE INDEX statement for sqlparser compatibility
    ///
    /// Supports two syntax forms:
    /// 1. PostgreSQL/pgvector: CREATE INDEX idx ON table USING hnsw(col vector_ops) WITH (...)
    ///    -> CREATE INDEX idx ON table (col) WITH (...)
    /// 2. SQLite style: CREATE INDEX idx ON table(col) USING hnsw
    ///    -> CREATE INDEX idx ON table(col)
    ///
    /// The index type is stored and can be extracted separately
    pub fn preprocess_create_index_using(sql: &str) -> (String, Option<String>) {
        let upper = sql.to_uppercase();

        if !upper.contains("USING") {
            return (sql.to_string(), None);
        }

        let using_pos = match upper.find("USING") {
            Some(pos) => pos,
            None => return (sql.to_string(), None),
        };

        let before_using = sql[..using_pos].trim_end();
        let after_using = sql[using_pos + 5..].trim_start(); // Skip "USING"

        // Check if there's a parenthesis before USING (SQLite style: ON table(col) USING hnsw)
        let has_paren_before = before_using.contains('(');

        if has_paren_before {
            // SQLite style: CREATE INDEX idx ON table(col) USING hnsw
            // Extract just the index type (word after USING, stop at whitespace/semicolon/paren)
            let index_type_end = after_using.find(|c: char| c.is_whitespace() || c == ';' || c == '(')
                .unwrap_or(after_using.len());
            let index_type = after_using[..index_type_end].trim().to_string();
            let remaining = after_using[index_type_end..].trim();

            // Check for WITH clause
            let cleaned_sql = if remaining.is_empty() || remaining == ";" {
                format!("{};", before_using)
            } else if remaining.to_uppercase().starts_with("WITH") {
                format!("{} {};", before_using, remaining.trim_end_matches(';'))
            } else {
                format!("{};", before_using)
            };

            (cleaned_sql, Some(index_type))
        } else {
            // PostgreSQL style: CREATE INDEX idx ON table USING hnsw(col vector_ops) WITH (...)
            // Extract index type (hnsw or ivfflat) - ends at '(' or whitespace
            let index_type_end = after_using.find(|c: char| c == '(' || c.is_whitespace())
                .unwrap_or(after_using.len());
            let index_type = after_using[..index_type_end].trim().to_string();
            let remaining = after_using[index_type_end..].trim_start();

            // Parse column specification from parentheses
            if let Some(paren_start) = remaining.find('(') {
                let paren_content_start = paren_start + 1;
                if let Some(paren_end) = remaining[paren_content_start..].find(')') {
                    let paren_content = &remaining[paren_content_start..paren_content_start + paren_end];

                    // Extract just the column name(s), stripping operator classes
                    // Operator classes like "vector_l2_ops", "vector_cosine_ops" come after column name
                    let column_spec = Self::strip_operator_classes(paren_content);

                    // Get anything after the closing paren (WITH clause, semicolon, etc.)
                    let after_paren = remaining[paren_content_start + paren_end + 1..].trim();

                    // Reconstruct: before_using + (column_spec) + after_paren
                    let cleaned_sql = if after_paren.is_empty() || after_paren == ";" {
                        format!("{} ({});", before_using, column_spec)
                    } else {
                        format!("{} ({}) {};", before_using, column_spec, after_paren.trim_end_matches(';'))
                    };

                    return (cleaned_sql, Some(index_type));
                }
            }

            // Fallback: couldn't parse parentheses, just remove USING clause
            (format!("{};", before_using), Some(index_type))
        }
    }

    /// Strip operator classes from column specification
    /// E.g., "embedding vector_l2_ops" -> "embedding"
    /// E.g., "col1, col2 vector_cosine_ops" -> "col1, col2"
    fn strip_operator_classes(column_spec: &str) -> String {
        // Known vector operator classes to strip
        let op_classes = [
            "vector_l2_ops",
            "vector_cosine_ops",
            "vector_ip_ops",
            "vector_inner_product_ops",
        ];

        let mut result = column_spec.to_string();
        for op_class in &op_classes {
            // Case-insensitive removal
            let upper_result = result.to_uppercase();
            let upper_op = op_class.to_uppercase();
            if let Some(pos) = upper_result.find(&upper_op) {
                result = format!("{}{}",
                    result[..pos].trim_end(),
                    result[pos + op_class.len()..].trim_start()
                );
            }
        }
        result.trim().to_string()
    }

    /// Convert DECIMAL type to NUMERIC for sqlparser compatibility
    ///
    /// Converts: DECIMAL, DECIMAL(p), DECIMAL(p,s) → NUMERIC, NUMERIC(p), NUMERIC(p,s)
    ///
    /// This allows SQLite DECIMAL syntax to work with PostgreSQL parser.
    /// Both types represent arbitrary-precision numbers in HeliosDB.
    pub fn preprocess_decimal_to_numeric(sql: &str) -> String {
        let mut result = String::new();
        let chars: Vec<(usize, char)> = sql.char_indices().collect();
        let mut char_idx = 0;

        // SAFETY: All indexing below is guarded by `while char_idx < chars.len()` and
        // `char_idx + 7 <= chars.len()` / `char_idx + 7 >= chars.len()` checks, plus
        // `char_idx == 0` guard before `char_idx - 1` access. Bounds are structurally guaranteed.
        #[allow(clippy::indexing_slicing)]
        while char_idx < chars.len() {
            let (byte_pos, _) = chars[char_idx];

            // Check for DECIMAL keyword (case-insensitive)
            // Only check if we have at least 7 characters remaining
            if char_idx + 7 <= chars.len() {
                let slice = &sql[byte_pos..];
                if slice.to_uppercase().starts_with("DECIMAL") {
                    // Make sure it's a word boundary (not part of another identifier)
                    let is_word_start = char_idx == 0 || {
                        let (_, prev_char) = chars[char_idx - 1];
                        !prev_char.is_alphanumeric() && prev_char != '_'
                    };

                    let is_word_end = char_idx + 7 >= chars.len() || {
                        let (_, next_char) = chars[char_idx + 7];
                        !next_char.is_alphanumeric() && next_char != '_'
                    };

                    if is_word_start && is_word_end {
                        // Replace DECIMAL with NUMERIC
                        result.push_str("NUMERIC");
                        char_idx += 7;
                        continue;
                    }
                }
            }

            // Copy character as-is
            let (_, c) = chars[char_idx];
            result.push(c);
            char_idx += 1;
        }

        result
    }

    /// Parse CREATE DATABASE BRANCH statement
    ///
    /// Syntax variations:
    /// - CREATE DATABASE BRANCH `<name>` FROM `<parent>` AS OF NOW
    /// - CREATE BRANCH `<name>` AS OF NOW
    /// - CREATE DATABASE BRANCH `<name>` WITH (option = value)
    pub fn parse_create_branch_sql(sql: &str) -> Result<(String, Option<String>, String, Option<String>)> {
        let cleaned = sql.trim().to_string();
        let upper = cleaned.to_uppercase();

        // Extract branch name - first identifier after CREATE [DATABASE] BRANCH
        let name_start = if upper.starts_with("CREATE DATABASE BRANCH") {
            "CREATE DATABASE BRANCH".len()
        } else {
            "CREATE BRANCH".len()
        };

        let after_create = cleaned[name_start..].trim_start();
        let name_end = after_create.find(|c: char| c.is_whitespace() || c == ';')
            .unwrap_or(after_create.len());
        let branch_name = after_create[..name_end].to_string();

        if branch_name.is_empty() {
            return Err(Error::query_execution("CREATE BRANCH requires a branch name"));
        }

        // Find AS OF clause (required)
        let remaining = after_create[name_end..].trim();
        let upper_remaining = remaining.to_uppercase();

        // Look for FROM clause (optional parent)
        let parent = if let Some(from_pos) = upper_remaining.find("FROM ") {
            let after_from = remaining[from_pos + 5..].trim_start();
            let from_end = after_from.find(|c: char| c.is_whitespace() || c == ';')
                .unwrap_or(after_from.len());
            let from_name = after_from[..from_end].trim().to_string();
            if from_name.is_empty() || from_name.to_uppercase() == "CURRENT" {
                None
            } else {
                Some(from_name)
            }
        } else {
            None
        };

        // Find AS OF clause (required)
        let as_of_pos = upper_remaining.find("AS OF")
            .ok_or_else(|| Error::query_execution("CREATE BRANCH requires AS OF clause"))?;

        let after_as_of = remaining[as_of_pos + 5..].trim_start();

        // Find end of AS OF clause (WITH, WHERE, GROUP, ORDER, LIMIT, UNION, ;, or end)
        let as_of_end_keywords = ["WITH", "WHERE", "GROUP", "ORDER", "LIMIT", "UNION", ";"];
        let as_of_end = as_of_end_keywords.iter()
            .filter_map(|&kw| {
                if let Some(pos) = after_as_of.to_uppercase().find(kw) {
                    if pos == 0 || after_as_of.chars().nth(pos.saturating_sub(1))
                        .map(|c| c.is_whitespace())
                        .unwrap_or(true) {
                        return Some(pos);
                    }
                }
                None
            })
            .min()
            .unwrap_or(after_as_of.len());

        let as_of_clause = after_as_of[..as_of_end].trim().trim_end_matches(';').to_string();

        if as_of_clause.is_empty() {
            return Err(Error::query_execution("CREATE BRANCH requires valid AS OF clause"));
        }

        // Find WITH clause (optional)
        let with_options = if let Some(with_pos) = upper_remaining.find("WITH") {
            let after_with = remaining[with_pos + 4..].trim_start();
            // Extract until semicolon or end
            let with_end = after_with.find(';').unwrap_or(after_with.len());
            let opts = after_with[..with_end].trim().to_string();
            if opts.is_empty() { None } else { Some(opts) }
        } else {
            None
        };

        Ok((branch_name, parent, as_of_clause, with_options))
    }

    /// Parse DROP DATABASE BRANCH statement
    ///
    /// Syntax variations:
    /// - DROP DATABASE BRANCH `<name>`
    /// - DROP BRANCH [IF EXISTS] `<name>`
    pub fn parse_drop_branch_sql(sql: &str) -> Result<(String, bool)> {
        let cleaned = sql.trim().to_string();
        let upper = cleaned.to_uppercase();

        // Skip DROP [DATABASE] BRANCH
        let name_start = if upper.starts_with("DROP DATABASE BRANCH") {
            "DROP DATABASE BRANCH".len()
        } else {
            "DROP BRANCH".len()
        };

        let mut remaining = cleaned[name_start..].trim_start();

        // Check for IF EXISTS
        let if_exists = if remaining.to_uppercase().starts_with("IF EXISTS") {
            remaining = remaining[9..].trim_start(); // "IF EXISTS".len() = 9
            true
        } else {
            false
        };

        // Extract branch name
        let name_end = remaining.find(|c: char| c.is_whitespace() || c == ';')
            .unwrap_or(remaining.len());
        let branch_name = remaining[..name_end].trim().to_string();

        if branch_name.is_empty() {
            return Err(Error::query_execution("DROP BRANCH requires a branch name"));
        }

        Ok((branch_name, if_exists))
    }

    /// Parse MERGE DATABASE BRANCH statement
    ///
    /// Syntax:
    /// - MERGE DATABASE BRANCH `<source>` INTO `<target>` [WITH options]
    /// - MERGE BRANCH `<source>` INTO `<target>` [WITH options]
    pub fn parse_merge_branch_sql(sql: &str) -> Result<(String, String, Option<String>)> {
        let cleaned = sql.trim().to_string();
        let upper = cleaned.to_uppercase();

        // Skip MERGE [DATABASE] BRANCH
        let name_start = if upper.starts_with("MERGE DATABASE BRANCH") {
            "MERGE DATABASE BRANCH".len()
        } else {
            "MERGE BRANCH".len()
        };

        let after_merge = cleaned[name_start..].trim_start();

        // Extract source branch name
        let source_end = after_merge.find(|c: char| c.is_whitespace())
            .unwrap_or(after_merge.len());
        let source = after_merge[..source_end].to_string();

        if source.is_empty() {
            return Err(Error::query_execution("MERGE BRANCH requires source branch name"));
        }

        // Find INTO keyword
        let remaining = after_merge[source_end..].trim_start();
        let upper_remaining = remaining.to_uppercase();

        if !upper_remaining.starts_with("INTO") {
            return Err(Error::query_execution("MERGE BRANCH requires INTO keyword"));
        }

        let after_into = remaining[4..].trim_start(); // "INTO".len() = 4

        // Extract target branch name
        let target_end = after_into.find(|c: char| c.is_whitespace() || c == ';')
            .unwrap_or(after_into.len());
        let target = after_into[..target_end].to_string();

        if target.is_empty() {
            return Err(Error::query_execution("MERGE BRANCH requires target branch name"));
        }

        // Find WITH clause (optional)
        let with_options = if let Some(with_pos) = upper_remaining.find("WITH") {
            let after_with = remaining[with_pos + 4..].trim_start();
            let with_end = after_with.find(';').unwrap_or(after_with.len());
            let opts = after_with[..with_end].trim().to_string();
            if opts.is_empty() { None } else { Some(opts) }
        } else {
            None
        };

        Ok((source, target, with_options))
    }

    /// Parse USE BRANCH statement
    ///
    /// Syntax:
    /// - USE BRANCH `<name>`
    /// - USE DATABASE BRANCH `<name>`
    pub fn parse_use_branch_sql(sql: &str) -> Result<String> {
        let cleaned = sql.trim().to_string();
        let upper = cleaned.to_uppercase();

        // Skip USE [DATABASE] BRANCH
        let name_start = if upper.starts_with("USE DATABASE BRANCH") {
            "USE DATABASE BRANCH".len()
        } else {
            "USE BRANCH".len()
        };

        let after_use = cleaned[name_start..].trim_start();

        // Extract branch name
        let name_end = after_use.find(|c: char| c.is_whitespace() || c == ';')
            .unwrap_or(after_use.len());
        let branch_name = after_use[..name_end].trim().to_string();

        if branch_name.is_empty() {
            return Err(Error::query_execution("USE BRANCH requires a branch name"));
        }

        Ok(branch_name)
    }

    // === HA Switchover SQL Detection and Parsing (ha-tier1 feature) ===

    /// Check if SQL is a SWITCHOVER TO statement
    #[cfg(feature = "ha-tier1")]
    pub fn is_switchover(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("SWITCHOVER TO") || upper.starts_with("HA SWITCHOVER TO")
    }

    /// Check if SQL is a SWITCHOVER CHECK statement
    #[cfg(feature = "ha-tier1")]
    pub fn is_switchover_check(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("SWITCHOVER CHECK") || upper.starts_with("HA SWITCHOVER CHECK")
    }

    /// Check if SQL is a SHOW CLUSTER STATUS statement
    #[cfg(feature = "ha-tier1")]
    pub fn is_cluster_status(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("SHOW CLUSTER STATUS") ||
        upper.starts_with("SHOW HA STATUS") ||
        upper.starts_with("SHOW REPLICATION STATUS")
    }

    /// Parse SWITCHOVER TO statement to extract target node ID
    ///
    /// Syntax:
    /// - SWITCHOVER TO '<node-uuid>'
    /// - SWITCHOVER TO node_alias
    /// - HA SWITCHOVER TO '<node-uuid>'
    #[cfg(feature = "ha-tier1")]
    pub fn parse_switchover_sql(sql: &str) -> Result<String> {
        let cleaned = sql.trim().to_string();
        let upper = cleaned.to_uppercase();

        // Find position after SWITCHOVER TO
        let to_pos = upper.find("TO ")
            .ok_or_else(|| Error::query_execution("SWITCHOVER statement requires TO clause"))?;

        let after_to = cleaned[to_pos + 3..].trim_start();

        // Extract node identifier - may be quoted or unquoted
        let node_id = if after_to.starts_with('\'') || after_to.starts_with('"') {
            // Quoted identifier
            let quote_char = if after_to.starts_with('\'') { '\'' } else { '"' };
            let end_quote = after_to[1..].find(quote_char)
                .ok_or_else(|| Error::query_execution("Unterminated quote in node identifier"))?;
            after_to[1..=end_quote].to_string()
        } else {
            // Unquoted identifier
            let end_pos = after_to.find(|c: char| c.is_whitespace() || c == ';')
                .unwrap_or(after_to.len());
            after_to[..end_pos].to_string()
        };

        if node_id.is_empty() {
            return Err(Error::query_execution("SWITCHOVER TO requires a target node identifier"));
        }

        Ok(node_id)
    }

    /// Parse SWITCHOVER CHECK statement to extract target node ID
    ///
    /// Syntax:
    /// - SWITCHOVER CHECK '<node-uuid>'
    /// - SWITCHOVER CHECK node_alias
    /// - HA SWITCHOVER CHECK '<node-uuid>'
    #[cfg(feature = "ha-tier1")]
    pub fn parse_switchover_check_sql(sql: &str) -> Result<String> {
        let cleaned = sql.trim().to_string();
        let upper = cleaned.to_uppercase();

        // Find position after SWITCHOVER CHECK
        let check_pos = upper.find("CHECK ")
            .ok_or_else(|| Error::query_execution("SWITCHOVER CHECK statement malformed"))?;

        let after_check = cleaned[check_pos + 6..].trim_start();

        // Extract node identifier - may be quoted or unquoted
        let node_id = if after_check.starts_with('\'') || after_check.starts_with('"') {
            // Quoted identifier
            let quote_char = if after_check.starts_with('\'') { '\'' } else { '"' };
            let end_quote = after_check[1..].find(quote_char)
                .ok_or_else(|| Error::query_execution("Unterminated quote in node identifier"))?;
            after_check[1..=end_quote].to_string()
        } else {
            // Unquoted identifier
            let end_pos = after_check.find(|c: char| c.is_whitespace() || c == ';')
                .unwrap_or(after_check.len());
            after_check[..end_pos].to_string()
        };

        if node_id.is_empty() {
            return Err(Error::query_execution("SWITCHOVER CHECK requires a target node identifier"));
        }

        Ok(node_id)
    }

    /// Check if SQL is a SET NODE ALIAS statement
    #[cfg(feature = "ha-tier1")]
    pub fn is_set_node_alias(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("SET NODE ALIAS")
    }

    /// Check if SQL is a SHOW TOPOLOGY statement
    #[cfg(feature = "ha-tier1")]
    pub fn is_show_topology(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("SHOW TOPOLOGY") || upper.starts_with("DESCRIBE CLUSTER")
    }

    /// Parse SET NODE ALIAS statement
    ///
    /// Syntax:
    /// - SET NODE ALIAS 'my-alias' FOR 'node-uuid'
    /// - SET NODE ALIAS 'my-alias' FOR node_alias
    /// - SET NODE ALIAS NULL FOR 'node-uuid' (removes alias)
    #[cfg(feature = "ha-tier1")]
    pub fn parse_set_node_alias_sql(sql: &str) -> Result<(String, Option<String>)> {
        let cleaned = sql.trim().to_string();
        let upper = cleaned.to_uppercase();

        // Verify structure: SET NODE ALIAS <alias> FOR <node-id>
        if !upper.starts_with("SET NODE ALIAS") {
            return Err(Error::query_execution("Invalid SET NODE ALIAS syntax"));
        }

        // Find positions
        let alias_start = "SET NODE ALIAS".len();
        let for_pos = upper.find(" FOR ")
            .ok_or_else(|| Error::query_execution("SET NODE ALIAS requires FOR clause"))?;

        // Extract alias (between SET NODE ALIAS and FOR)
        let alias_part = cleaned[alias_start..for_pos].trim();
        let alias = if alias_part.to_uppercase() == "NULL" {
            None
        } else if alias_part.starts_with('\'') || alias_part.starts_with('"') {
            let quote_char = if alias_part.starts_with('\'') { '\'' } else { '"' };
            let end_quote = alias_part[1..].find(quote_char)
                .ok_or_else(|| Error::query_execution("Unterminated quote in alias"))?;
            Some(alias_part[1..=end_quote].to_string())
        } else {
            Some(alias_part.to_string())
        };

        // Extract node identifier (after FOR)
        let after_for = cleaned[for_pos + 5..].trim();
        let node_id = if after_for.starts_with('\'') || after_for.starts_with('"') {
            let quote_char = if after_for.starts_with('\'') { '\'' } else { '"' };
            let end_quote = after_for[1..].find(quote_char)
                .ok_or_else(|| Error::query_execution("Unterminated quote in node identifier"))?;
            after_for[1..=end_quote].to_string()
        } else {
            let end_pos = after_for.find(|c: char| c.is_whitespace() || c == ';')
                .unwrap_or(after_for.len());
            after_for[..end_pos].to_string()
        };

        if node_id.is_empty() {
            return Err(Error::query_execution("SET NODE ALIAS requires a node identifier after FOR"));
        }

        Ok((node_id, alias))
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_select() {
        let parser = Parser::new();
        let result = parser.parse_one("SELECT id, name FROM users WHERE id = 1");
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_create_table() {
        let parser = Parser::new();
        let result = parser.parse_one(
            "CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT NOT NULL)"
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_insert() {
        let parser = Parser::new();
        let result = parser.parse_one(
            "INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com')"
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_error() {
        let parser = Parser::new();
        let result = parser.parse_one("SELECT FROM");
        assert!(result.is_err());
    }

    // HA Switchover SQL tests (ha-tier1 feature)
    #[cfg(feature = "ha-tier1")]
    mod ha_tests {
        use super::*;

        #[test]
        fn test_is_switchover() {
            assert!(Parser::is_switchover("SWITCHOVER TO 'node-123'"));
            assert!(Parser::is_switchover("switchover to node-abc"));
            assert!(Parser::is_switchover("HA SWITCHOVER TO 'uuid-here'"));
            assert!(!Parser::is_switchover("SELECT * FROM nodes"));
            assert!(!Parser::is_switchover("SWITCHOVER CHECK 'node'"));
        }

        #[test]
        fn test_is_switchover_check() {
            assert!(Parser::is_switchover_check("SWITCHOVER CHECK 'node-123'"));
            assert!(Parser::is_switchover_check("switchover check node-abc"));
            assert!(Parser::is_switchover_check("HA SWITCHOVER CHECK 'uuid-here'"));
            assert!(!Parser::is_switchover_check("SWITCHOVER TO 'node'"));
        }

        #[test]
        fn test_is_cluster_status() {
            assert!(Parser::is_cluster_status("SHOW CLUSTER STATUS"));
            assert!(Parser::is_cluster_status("show cluster status"));
            assert!(Parser::is_cluster_status("SHOW HA STATUS"));
            assert!(Parser::is_cluster_status("SHOW REPLICATION STATUS"));
            assert!(!Parser::is_cluster_status("SELECT * FROM status"));
        }

        #[test]
        fn test_parse_switchover_quoted() {
            let result = Parser::parse_switchover_sql("SWITCHOVER TO 'node-uuid-123'");
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), "node-uuid-123");
        }

        #[test]
        fn test_parse_switchover_unquoted() {
            let result = Parser::parse_switchover_sql("SWITCHOVER TO node_alias");
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), "node_alias");
        }

        #[test]
        fn test_parse_switchover_check_quoted() {
            let result = Parser::parse_switchover_check_sql("SWITCHOVER CHECK 'target-node'");
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), "target-node");
        }

        #[test]
        fn test_parse_switchover_check_unquoted() {
            let result = Parser::parse_switchover_check_sql("SWITCHOVER CHECK my_standby");
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), "my_standby");
        }
    }
}
