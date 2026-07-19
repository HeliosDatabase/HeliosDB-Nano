//! SQL parser using sqlparser-rs

use crate::{ColumnStorageMode, Error, Result};
use sqlparser::ast::Statement;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser as SqlParser;

/// SQL parser
pub struct Parser {
    dialect: PostgreSqlDialect,
}

/// Stage-0 partitioning: the pre-parse-captured shape of a
/// `CREATE TABLE child PARTITION OF parent { FOR VALUES … | DEFAULT } …`
/// child-table declaration. sqlparser 0.53 has no `PARTITION OF` grammar, so
/// the statement is rewritten to a plain `CREATE TABLE child ()`
/// ([`Parser::preprocess_partition_of`]) and this spec threads the parent
/// reference to the planner (the first layer with catalog access), which
/// clones the parent's columns onto the child.
///
/// `child` / `parent` are the raw name substrings exactly as written (possibly
/// schema-qualified / quoted); `bound` is the verbatim bound text
/// (`FOR VALUES …` or `DEFAULT`) — recorded, never interpreted, at Stage 0.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartitionOfSpec {
    /// `IF NOT EXISTS` was present on the child `CREATE TABLE`.
    pub if_not_exists: bool,
    /// Raw child table name (possibly schema-qualified / quoted).
    pub child: String,
    /// Raw parent table name (possibly schema-qualified / quoted).
    pub parent: String,
    /// Verbatim bound text (`FOR VALUES …` or `DEFAULT`), stored not interpreted.
    pub bound: String,
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
                "WHERE",
                "GROUP",
                "ORDER",
                "LIMIT",
                "UNION",
                "INTERSECT",
                "EXCEPT",
                ")",
                ";",
                "HAVING",
            ];

            let mut end_pos = after_as_of.len();
            for keyword in &end_keywords {
                if let Some(pos) = upper_after.find(keyword) {
                    // Make sure it's a word boundary (preceded by space or parenthesis)
                    if pos == 0
                        || after_as_of
                            .chars()
                            .nth(pos - 1)
                            .map(|c| c.is_whitespace() || c == ')')
                            .unwrap_or(false)
                    {
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

        // SQLite-compat preprocessing: ?-placeholders, INSERT OR REPLACE/IGNORE,
        // INTEGER PRIMARY KEY AUTOINCREMENT, DATETIME('now'). Runs before any
        // other rewrite so downstream stages see canonical PostgreSQL syntax.
        let sql_compat = crate::sql::sqlite_compat::translate(&sql_no_comments)?;

        // Preprocess to remove time-travel syntax for parsing
        let mut processed_sql = self.preprocess_time_travel_sql(&sql_compat);

        // Preprocess DECIMAL to NUMERIC for sqlparser compatibility
        processed_sql = Self::preprocess_decimal_to_numeric(&processed_sql);

        // Preprocess `<col> IS [NOT] JSON` (SQL:2016 / Oracle predicate) into a
        // `json_valid(col)` call. sqlparser 0.53 has no IS JSON support, so an
        // Oracle->HeliosDB migrate emitting `CHECK (mfa IS JSON)` otherwise
        // fails to parse before the planner is ever reached.
        processed_sql = Self::preprocess_is_json(&processed_sql);

        // Preprocess: strip a trailing comma sitting immediately before the
        // closing `)` of a CREATE TABLE column/constraint list. a2h's
        // Oracle->HeliosDB export emits `... , PRIMARY KEY (id), )`; sqlparser
        // 0.53 (like PostgreSQL) rejects the dangling comma. Scoped to CREATE
        // TABLE and quote-aware so string literals / multi-row VALUES are safe.
        processed_sql = Self::preprocess_strip_trailing_commas(&processed_sql);

        // Preprocess to remove SECURITY DEFINER/INVOKER (not supported by sqlparser)
        processed_sql = Self::preprocess_remove_security_clause(&processed_sql);

        // Preprocess to remove STORAGE clauses from column definitions (not supported by sqlparser)
        processed_sql = Self::preprocess_remove_storage_clauses(&processed_sql);

        // Preprocess: strip the parenthesized sequence-options block on
        // `GENERATED ALWAYS AS IDENTITY (sequence name … INCREMENT BY …
        // CACHE …)`. drizzle-kit / Prisma emit this form; sqlparser
        // doesn't accept it. The bare `GENERATED ALWAYS AS IDENTITY`
        // already auto-generates monotonically, and the parenthesized
        // options are advisory (sequence name, start, increment, cache).
        // (KanttBan bug #4 against v3.27.0.)
        processed_sql = Self::preprocess_strip_identity_options(&processed_sql);

        // Preprocess CREATE INDEX USING clause for sqlparser compatibility
        let index_type_override = if Self::is_create_index_using(&processed_sql) {
            let (cleaned_sql, index_type) = Self::preprocess_create_index_using(&processed_sql);
            processed_sql = cleaned_sql;
            index_type
        } else {
            None
        };

        // Reorder CREATE SEQUENCE option clauses (PostgreSQL allows any order;
        // sqlparser requires INCREMENT before START, etc.).
        processed_sql = Self::preprocess_create_sequence_clause_order(&processed_sql);

        // Round-2 pgrust-corpus compat: strip the PostgreSQL
        // `INHERITS (parent[, …])` table-option clause off CREATE TABLE
        // (sqlparser 0.53 has no INHERITS grammar, so it fails at the parse
        // stage). Faithful parent column/constraint merge is out of scope for
        // this pass; stripping the clause lets the child table create with
        // its own explicitly-listed columns, the pragmatic compatibility win.
        processed_sql = Self::preprocess_strip_inherits(&processed_sql);

        // Attempt the normal parse first. Only if it fails do we apply the
        // Stage-0 partitioning rewrites (strip a parent `PARTITION BY …` clause,
        // rewrite a child `PARTITION OF …` to an empty-column CREATE), so
        // currently-passing SQL is byte-identically untouched — the
        // strictly-additive guarantee: the rewrite fires ONLY on SQL that fails
        // to parse today. A rewrite that still won't parse reports the ORIGINAL
        // diagnostic, never a masked one.
        let mut statements = match SqlParser::parse_sql(&self.dialect, &processed_sql) {
            Ok(statements) => statements,
            Err(orig_err) => {
                let orig_msg = format!("Failed to parse SQL: {}", orig_err);
                match Self::rewrite_partition_syntax(&processed_sql) {
                    Some(rewritten) => {
                        SqlParser::parse_sql(&self.dialect, &rewritten).map_err(|_| Error::sql_parse(orig_msg))?
                    }
                    None => return Err(Error::sql_parse(orig_msg)),
                }
            }
        };

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
        statements
            .into_iter()
            .next()
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

    /// R4.3: check if SQL is a `VACUUM VERSIONS` statement (manual MVCC
    /// version-history collection pass).
    pub fn is_vacuum_versions(sql: &str) -> bool {
        let upper = sql.trim().trim_end_matches(';').trim().to_uppercase();
        upper == "VACUUM VERSIONS"
    }

    /// Priority #5 of the pgrust-corpus diagnosis: does this statement begin
    /// with the standard PostgreSQL `VACUUM` keyword? sqlparser 0.53
    /// implements NO form of the VACUUM grammar at all (confirmed: no
    /// `Keyword::VACUUM` dispatch anywhere in its parser), so every VACUUM
    /// form -- bare, `ANALYZE`, `FULL`, with/without a table list -- fails
    /// at the parse stage with "Expected: an SQL statement, found: VACUUM"
    /// before it ever reaches the planner. This is a pre-parse intercept in
    /// the same spirit as `is_vacuum_versions` above, checked separately
    /// (and excluding the Nano-specific `VACUUM VERSIONS` form, which is
    /// handled by `is_vacuum_versions` earlier in the same dispatch chain).
    ///
    /// Runs on the shared pre-parse dispatch path for every statement
    /// (alongside `is_transaction_control` / `is_vacuum_versions`), so this
    /// is deliberately a single cheap prefix check via `starts_with_icase`
    /// -- no full uppercasing, no allocation -- before any further string
    /// work happens.
    pub fn is_vacuum_statement(sql: &str) -> bool {
        let trimmed = sql.trim();
        crate::starts_with_icase(trimmed, "VACUUM") && !Self::is_vacuum_versions(trimmed)
    }

    /// Hand-parse the optional FULL/FREEZE/VERBOSE/ANALYZE flags (either the
    /// modern `VACUUM (option [, ...])` parenthesized form or the older
    /// bare-keyword form) and an optional comma-separated table list off a
    /// statement already confirmed by `is_vacuum_statement`. Per-table
    /// column lists (`VACUUM t (col1, col2)`) are accepted and ignored --
    /// Nano's `vacuum_table` operates on the whole table.
    ///
    /// Returns the requested table names (already case-folded per the
    /// unquoted-lowercase / quoted-preserved rule used elsewhere in the
    /// planner). An empty vec means "no table list" (whole-database
    /// VACUUM). Flags themselves are deliberately not returned: this pass
    /// accepts-and-ignores FULL/FREEZE/VERBOSE/ANALYZE, matching Postgres's
    /// own idempotent, safe-anytime VACUUM semantics -- a real
    /// ANALYZE-driven stats refresh is a reasonable fast-follow, not a
    /// blocker.
    pub fn parse_vacuum_tables(sql: &str) -> Vec<String> {
        let cleaned = sql.trim().trim_end_matches(';').trim();
        // Strip the leading VACUUM keyword.
        let after_vacuum = cleaned.get(6..).unwrap_or("").trim_start();

        // Modern parenthesized option list: `VACUUM (FULL, ANALYZE) t1, t2`.
        let after_options = if let Some(rest) = after_vacuum.strip_prefix('(') {
            match rest.find(')') {
                Some(close) => rest[close + 1..].trim_start(),
                None => "", // Malformed options list; nothing usable follows.
            }
        } else {
            // Older bare-keyword flags: `VACUUM FULL ANALYZE t1, t2`.
            let mut rest = after_vacuum;
            loop {
                let word_end = rest.find(|c: char| c.is_whitespace() || c == ',').unwrap_or(rest.len());
                let word = &rest[..word_end];
                if word.eq_ignore_ascii_case("FULL")
                    || word.eq_ignore_ascii_case("FREEZE")
                    || word.eq_ignore_ascii_case("VERBOSE")
                    || word.eq_ignore_ascii_case("ANALYZE")
                    || word.eq_ignore_ascii_case("ANALYSE")
                {
                    rest = rest[word_end..].trim_start();
                } else {
                    break;
                }
            }
            rest
        };

        if after_options.is_empty() {
            return Vec::new();
        }

        // Comma-separated `table_and_columns` list. Each entry is a table
        // name optionally followed by a parenthesized column list, which we
        // accept and discard.
        after_options
            .split(',')
            .filter_map(|entry| {
                let name_part = entry
                    .trim()
                    .split(|c: char| c.is_whitespace() || c == '(')
                    .next()?
                    .trim();
                if name_part.is_empty() {
                    return None;
                }
                // Preserve double-quoted identifiers as written; fold
                // unquoted identifiers to lowercase, matching
                // `Planner::normalize_ident`'s PostgreSQL rule.
                if name_part.starts_with('"') && name_part.ends_with('"') && name_part.len() >= 2 {
                    Some(name_part[1..name_part.len() - 1].to_string())
                } else {
                    Some(name_part.to_lowercase())
                }
            })
            .collect()
    }

    /// Priority #7 of the pgrust-corpus diagnosis: does this statement begin
    /// with `CREATE TABLESPACE`? sqlparser 0.53 has no `CreateTablespace`
    /// grammar at all (confirmed: no `TABLESPACE` keyword anywhere in its
    /// parser), so this fails at the parse stage ("Expected: an object type
    /// after CREATE, found: TABLESPACE"), not at the planner's generic
    /// "Statement not yet supported" catch-all. A minimal no-op stub
    /// therefore needs the same pre-parse-intercept treatment as VACUUM
    /// above, rather than a `Statement::CreateTablespace` planner match arm
    /// (there is no such variant to match).
    ///
    /// Real multi-tablespace functionality (LOCATION handling, DROP
    /// TABLESPACE, ALTER ... SET TABLESPACE) is explicitly out of scope --
    /// this only lets the statement parse-and-succeed as a no-op so fixture
    /// loads that issue it (e.g. `CREATE TABLESPACE regress_tblspace
    /// LOCATION '';`) don't fail outright.
    pub fn is_create_tablespace_statement(sql: &str) -> bool {
        let trimmed = sql.trim();
        crate::starts_with_icase(trimmed, "CREATE TABLESPACE")
    }

    /// Round-2 pgrust-corpus compat (~464 corpus statements): standard
    /// PostgreSQL `RESET name` / `RESET ALL`. sqlparser 0.53 has no
    /// top-level RESET grammar at all (the RESET keyword appears only inside
    /// `ALTER … RESET` in its parser), so a `RESET <guc>` / `RESET ALL` that
    /// is not one of Nano's already-modelled session settings fails at the
    /// parse stage with "Expected: an SQL statement, found: RESET" before it
    /// ever reaches the planner. This is a pre-parse intercept in the same
    /// spirit as `is_create_tablespace_statement` above.
    ///
    /// It is deliberately checked only AFTER the specific SET/RESET handlers
    /// (`try_handle_db_setting_statement_with_columns`,
    /// `try_handle_fk_setting`, `try_handle_trace_*`), so a `RESET` of a
    /// real session setting still performs its actual reset; only the
    /// otherwise-unhandled GUCs (`RESET search_path`, `RESET
    /// statement_timeout`, `RESET ALL`, …) fall through to here and are
    /// accepted as a no-op — matching PostgreSQL's safe-anytime RESET
    /// semantics (a GUC Nano does not model has no session state to
    /// restore). A single cheap `starts_with_icase` prefix check, no
    /// allocation, matching the cost profile of the checks it sits beside.
    /// The trailing space excludes a bare `RESET` (not valid PostgreSQL)
    /// and any identifier that merely begins with those letters.
    pub fn is_reset_statement(sql: &str) -> bool {
        let trimmed = sql.trim();
        crate::starts_with_icase(trimmed, "RESET ")
    }

    /// Round-2 pgrust-corpus compat (~80 corpus statements): PostgreSQL
    /// `REINDEX [ ( option … ) ] { INDEX | TABLE | SCHEMA | DATABASE |
    /// SYSTEM } name`. sqlparser 0.53 has no REINDEX statement dispatch at
    /// all (the word appears only in doc comments in its AST, never in the
    /// parser), so every REINDEX form fails at the parse stage before the
    /// planner. Accepted as a pre-parse no-op: Nano's LSM/index storage has
    /// no external-fragmentation rebuild need that a user-issued REINDEX
    /// must satisfy, and PostgreSQL REINDEX is itself an idempotent,
    /// safe-anytime maintenance command, so a success-returning no-op is a
    /// faithful-enough surface.
    ///
    /// Whole-keyword boundary check: the byte immediately after `REINDEX`
    /// (if any) must not continue an identifier, so a hypothetical
    /// `REINDEXED`-prefixed token never matches. Valid REINDEX forms
    /// continue with whitespace (`REINDEX TABLE t`) or `(` (`REINDEX
    /// (VERBOSE) TABLE t`), both of which pass the boundary.
    pub fn is_reindex_statement(sql: &str) -> bool {
        let trimmed = sql.trim();
        if !crate::starts_with_icase(trimmed, "REINDEX") {
            return false;
        }
        // "REINDEX" is 7 ASCII bytes; index 7 is the byte right after it.
        match trimmed.as_bytes().get(7) {
            None => true,
            Some(&c) => !(c.is_ascii_alphanumeric() || c == b'_'),
        }
    }

    /// Round-2 pgrust-corpus compat (~144 corpus statements): PostgreSQL
    /// `CREATE DOMAIN name [AS] base_type [constraints]` and `DROP DOMAIN
    /// [IF EXISTS] name`. sqlparser 0.53 has no DOMAIN object type at all
    /// (no `parse_create_domain`, no `ObjectType::Domain`), so both fail at
    /// the parse stage before the planner -- CREATE DOMAIN as "Expected: an
    /// object type after CREATE, found: DOMAIN", DROP DOMAIN similarly.
    ///
    /// Accepted as a parse-and-accept no-op, the same "single flat
    /// namespace" precedent used for CREATE SCHEMA / CREATE TABLESPACE:
    /// the statement parse-and-succeeds so a fixture load that issues it no
    /// longer aborts. Faithful domain semantics (registering the domain as
    /// an alias of its base type so a later `CREATE TABLE t (c my_domain)`
    /// resolves, plus its CHECK/NOT NULL/DEFAULT constraints) are a
    /// deliberate non-goal for this zero-regression pass: that would touch
    /// the type-resolution / column-binding path. A table that references
    /// an undefined domain therefore still fails on the unknown custom type
    /// exactly as it did before -- this change is strictly additive (a hard
    /// parse error becomes a success for the DOMAIN statement itself) and
    /// regresses nothing.
    ///
    /// The trailing space anchors the whole keyword and excludes an
    /// identifier that merely begins with these letters. `DROP DOMAIN IF
    /// EXISTS name` is covered because it still begins with `DROP DOMAIN `.
    pub fn is_domain_ddl_statement(sql: &str) -> bool {
        let trimmed = sql.trim();
        crate::starts_with_icase(trimmed, "CREATE DOMAIN ") || crate::starts_with_icase(trimmed, "DROP DOMAIN ")
    }

    /// Round-3 pgrust-corpus compat (~341 ATTACH + ~78 DETACH + ~46 ALTER INDEX
    /// ATTACH corpus statements): PostgreSQL
    /// `ALTER TABLE … ATTACH PARTITION child { FOR VALUES … | DEFAULT }`,
    /// `ALTER TABLE … DETACH PARTITION child [CONCURRENTLY | FINALIZE]`, and
    /// `ALTER INDEX … ATTACH PARTITION idx`. sqlparser 0.53 gates ATTACH/DETACH
    /// PARTITION to the ClickHouse/Generic dialects, so under `PostgreSqlDialect`
    /// every form fails at the parse stage before the planner. Accepted as a
    /// pre-parse no-op at Stage 0 — the child already exists as an independent
    /// table, and real catalog attach/detach + overlap validation is Stage 2 —
    /// the same parse-and-accept precedent as CREATE DOMAIN / VACUUM.
    ///
    /// A cheap `ALTER TABLE` / `ALTER INDEX` prefix gate runs first (no work on
    /// the hot SELECT/INSERT path); the `{ATTACH|DETACH} PARTITION` phrase scan
    /// only runs on the rare ALTER path.
    pub fn is_partition_attach_detach_statement(sql: &str) -> bool {
        let trimmed = sql.trim();
        if !(crate::starts_with_icase(trimmed, "ALTER TABLE") || crate::starts_with_icase(trimmed, "ALTER INDEX")) {
            return false;
        }
        Self::contains_kw_phrase(trimmed, b"ATTACH PARTITION") || Self::contains_kw_phrase(trimmed, b"DETACH PARTITION")
    }

    /// Allocation-free case-insensitive search for a two-word keyword phrase
    /// (`WORD1 WORD2`, single space in `phrase`), tolerating one-or-more
    /// whitespace between the words and requiring word boundaries on both ends.
    /// Quote-aware: `'`/`"` spans are skipped, so a currently-passing ALTER
    /// carrying the phrase inside a string literal or quoted identifier (e.g.
    /// `ADD COLUMN c int DEFAULT 'attach partition'`) is never mis-detected as
    /// a no-op — the strict-additive safety invariant.
    #[allow(clippy::indexing_slicing)] // Byte cursor bounded by `while i < n` + explicit `i + w1.len() <= n` check.
    fn contains_kw_phrase(sql: &str, phrase: &[u8]) -> bool {
        let sp = match phrase.iter().position(|&b| b == b' ') {
            Some(p) => p,
            None => return false,
        };
        let (w1, w2) = (&phrase[..sp], &phrase[sp + 1..]);
        let bytes = sql.as_bytes();
        let n = bytes.len();
        let mut i = 0usize;
        while i < n {
            if bytes[i] == b'\'' || bytes[i] == b'"' {
                i = Self::skip_quoted_span(bytes, i);
                continue;
            }
            if i + w1.len() <= n
                && bytes[i..i + w1.len()].eq_ignore_ascii_case(w1)
                && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'))
            {
                let mut j = i + w1.len();
                let ws0 = j;
                while j < n && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j > ws0
                    && j + w2.len() <= n
                    && bytes[j..j + w2.len()].eq_ignore_ascii_case(w2)
                    && (j + w2.len() >= n
                        || !(bytes[j + w2.len()].is_ascii_alphanumeric() || bytes[j + w2.len()] == b'_'))
                {
                    return true;
                }
            }
            i += 1;
        }
        false
    }

    /// Check if SQL is a REFRESH MATERIALIZED VIEW statement
    pub fn is_refresh_materialized_view(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("REFRESH MATERIALIZED VIEW")
    }

    /// Check if SQL is a CREATE MATERIALIZED VIEW statement.
    pub fn is_create_materialized_view(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("CREATE MATERIALIZED VIEW")
    }

    /// Parse CREATE MATERIALIZED VIEW for the PostgreSQL-compatible shape
    /// that sqlparser does not currently accept: `IF NOT EXISTS`.
    ///
    /// The caller still plans the inner query through the normal planner so
    /// the existing MV/DMV execution path stays authoritative.
    pub fn parse_create_materialized_view_sql(sql: &str) -> Result<(String, String, bool)> {
        let cleaned = sql.trim().trim_end_matches(';').trim();
        let after_create = cleaned
            .get("CREATE MATERIALIZED VIEW".len()..)
            .ok_or_else(|| Error::query_execution("CREATE MATERIALIZED VIEW requires a view name"))?
            .trim_start();

        let upper_after = after_create.to_uppercase();
        let if_not_exists = upper_after.starts_with("IF NOT EXISTS");
        let remaining = if if_not_exists {
            after_create["IF NOT EXISTS".len()..].trim_start()
        } else {
            after_create
        };

        let as_pos = Self::find_as_keyword(remaining)
            .ok_or_else(|| Error::query_execution("CREATE MATERIALIZED VIEW requires AS <query>"))?;
        let raw_name = remaining[..as_pos].trim();
        let query = remaining[as_pos + "AS".len()..].trim();

        if raw_name.is_empty() {
            return Err(Error::query_execution("CREATE MATERIALIZED VIEW requires a view name"));
        }
        if query.is_empty() {
            return Err(Error::query_execution("CREATE MATERIALIZED VIEW requires AS <query>"));
        }

        Ok((
            Self::normalize_simple_object_name(raw_name),
            query.to_string(),
            if_not_exists,
        ))
    }

    fn find_as_keyword(sql: &str) -> Option<usize> {
        let upper = sql.to_uppercase();
        for (idx, _) in upper.match_indices("AS") {
            let before_ok = idx == 0 || upper[..idx].chars().next_back().is_some_and(|c| c.is_whitespace());
            let after_idx = idx + "AS".len();
            let after_ok =
                after_idx >= upper.len() || upper[after_idx..].chars().next().is_some_and(|c| c.is_whitespace());
            if before_ok && after_ok {
                return Some(idx);
            }
        }
        None
    }

    fn normalize_simple_object_name(raw_name: &str) -> String {
        let joined = raw_name
            .split('.')
            .map(|part| part.trim().trim_matches('"').to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(".");
        if let Some(rest) = joined.strip_prefix("public.") {
            return rest.to_string();
        }
        if let Some(rest) = joined.strip_prefix("pg_catalog.") {
            return rest.to_string();
        }
        joined
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
        let incremental = upper_remaining.ends_with("INCREMENTALLY") || upper_remaining.ends_with("INCREMENTALLY;");

        // Remove INCREMENTALLY from the end if present
        let without_incremental = if incremental {
            let upper = after_concurrent.to_uppercase();
            let inc_pos = upper.rfind("INCREMENTALLY").unwrap_or(after_concurrent.len());
            after_concurrent[..inc_pos].trim_end()
        } else {
            after_concurrent.trim_end_matches(';').trim_end()
        };

        // Extract view name
        let name_end = without_incremental
            .find(|c: char| c.is_whitespace() || c == ';')
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
        let name_end = remaining
            .find(|c: char| c.is_whitespace() || c == ';')
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
                return Err(Error::query_execution(
                    "ALTER MATERIALIZED VIEW SET requires closing parenthesis",
                ));
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
                    "Invalid option format '{}', expected 'key = value'",
                    pair
                )));
            }

            let key = parts
                .get(0)
                .ok_or_else(|| {
                    Error::query_execution(format!("Invalid option format '{}', expected 'key = value'", pair))
                })?
                .trim()
                .to_lowercase();
            let value = parts
                .get(1)
                .ok_or_else(|| {
                    Error::query_execution(format!("Invalid option format '{}', expected 'key = value'", pair))
                })?
                .trim()
                .trim_matches('\'')
                .trim_matches('"')
                .to_string();

            // Validate known options
            match key.as_str() {
                "staleness_threshold" | "max_cpu_percent" | "priority" => {
                    // Validate numeric
                    if value.parse::<f64>().is_err() {
                        return Err(Error::query_execution(format!(
                            "Option '{}' requires a numeric value, got '{}'",
                            key, value
                        )));
                    }
                }
                "refresh_strategy" => {
                    let lower = value.to_lowercase();
                    if !["manual", "auto", "incremental"].contains(&lower.as_str()) {
                        return Err(Error::query_execution(format!(
                            "refresh_strategy must be 'manual', 'auto', or 'incremental', got '{}'",
                            value
                        )));
                    }
                }
                "incremental_enabled" => {
                    let lower = value.to_lowercase();
                    if !["true", "false"].contains(&lower.as_str()) {
                        return Err(Error::query_execution(format!(
                            "incremental_enabled must be 'true' or 'false', got '{}'",
                            value
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
            return Err(Error::query_execution(
                "ALTER MATERIALIZED VIEW SET requires at least one option",
            ));
        }

        Ok((view_name, options))
    }

    /// Check if SQL is an ALTER TABLE ALTER COLUMN SET STORAGE statement
    ///
    /// Syntax: ALTER TABLE `<table>` ALTER COLUMN `<column>` SET STORAGE `<mode>`
    pub fn is_alter_column_storage(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("ALTER TABLE") && upper.contains("ALTER COLUMN") && upper.contains("SET STORAGE")
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
        let after_alter = cleaned
            .get(11..)
            .ok_or_else(|| Error::query_execution("Invalid ALTER TABLE statement"))?
            .trim_start();

        // Extract table name (ends at ALTER)
        let upper_after = after_alter.to_uppercase();
        let alter_pos = upper_after
            .find(" ALTER ")
            .ok_or_else(|| Error::query_execution("ALTER TABLE requires ALTER COLUMN clause"))?;

        let table_name = after_alter[..alter_pos].trim().to_string();
        if table_name.is_empty() {
            return Err(Error::query_execution("ALTER TABLE requires a table name"));
        }

        // Skip " ALTER COLUMN "
        let after_column = after_alter
            .get(alter_pos + 7..)
            .ok_or_else(|| Error::query_execution("Invalid ALTER COLUMN clause"))?
            .trim_start();

        let upper_column = after_column.to_uppercase();
        if !upper_column.starts_with("COLUMN ") {
            return Err(Error::query_execution("Expected COLUMN keyword after ALTER"));
        }

        let after_col_keyword = after_column
            .get(7..)
            .ok_or_else(|| Error::query_execution("Invalid ALTER COLUMN clause"))?
            .trim_start();

        // Find SET STORAGE
        let upper_rest = after_col_keyword.to_uppercase();
        let set_pos = upper_rest
            .find(" SET STORAGE")
            .ok_or_else(|| Error::query_execution("ALTER COLUMN requires SET STORAGE clause"))?;

        let column_name = after_col_keyword[..set_pos].trim().to_string();
        if column_name.is_empty() {
            return Err(Error::query_execution("ALTER COLUMN requires a column name"));
        }

        // Extract storage mode (after " SET STORAGE ")
        let after_storage = after_col_keyword
            .get(set_pos + 12..)
            .ok_or_else(|| Error::query_execution("Invalid SET STORAGE clause"))?
            .trim_start();

        let mode_str = after_storage.trim_end_matches(';').trim().to_uppercase();

        let storage_mode = match mode_str.as_str() {
            "DEFAULT" => ColumnStorageMode::Default,
            "DICTIONARY" => ColumnStorageMode::Dictionary,
            "CONTENT_ADDRESSED" => ColumnStorageMode::ContentAddressed,
            "COLUMNAR" => ColumnStorageMode::Columnar,
            _ => {
                return Err(Error::query_execution(format!(
                    "Invalid storage mode '{}'. Expected: DEFAULT, DICTIONARY, CONTENT_ADDRESSED, or COLUMNAR",
                    mode_str
                )))
            }
        };

        Ok((table_name, column_name, storage_mode))
    }

    /// Does this SQL look like `ALTER TABLE … SET SCHEMA …`?
    ///
    /// sqlparser 0.53 has no `SetSchema` ALTER-TABLE operation (its trailing
    /// arm only accepts `SET [TBLPROPERTIES] (…)` and otherwise errors), so
    /// `ALTER TABLE t SET SCHEMA s` is routed through a custom pre-parse path
    /// (mirroring `is_alter_column_storage` / `is_alter_sequence`). Guarded to
    /// exclude the column form (`ALTER TABLE … ALTER COLUMN … SET …`), which is
    /// handled by sqlparser / `is_alter_column_storage`.
    pub fn is_alter_table_set_schema(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("ALTER TABLE") && upper.contains(" SET SCHEMA") && !upper.contains(" ALTER COLUMN")
    }

    /// Parse `ALTER TABLE [IF EXISTS] <name> SET SCHEMA <new_schema>` into
    /// `(table_name_raw, new_schema, if_exists)`. `table_name_raw` is the
    /// verbatim (possibly schema-qualified / quoted) name — session
    /// `search_path` resolution to a storage key happens in the caller. The
    /// target schema is lowercased when unquoted, matching how schema keys are
    /// stored; a double-quoted target keeps its case.
    pub fn parse_alter_table_set_schema(sql: &str) -> Result<(String, String, bool)> {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        let upper = trimmed.to_uppercase();
        if !upper.starts_with("ALTER TABLE") {
            return Err(Error::query_execution("Expected ALTER TABLE"));
        }
        // Peel "ALTER TABLE" and an optional "IF EXISTS".
        let mut rest = trimmed["ALTER TABLE".len()..].trim_start();
        let if_exists = rest.to_uppercase().starts_with("IF EXISTS");
        if if_exists {
            rest = rest["IF EXISTS".len()..].trim_start();
        }

        // Split on the (case-insensitive) " SET SCHEMA " delimiter.
        let rest_upper = rest.to_uppercase();
        let set_pos = rest_upper
            .find(" SET SCHEMA")
            .ok_or_else(|| Error::query_execution("ALTER TABLE … SET SCHEMA requires a SET SCHEMA clause"))?;
        let table_name = rest[..set_pos].trim().to_string();
        if table_name.is_empty() {
            return Err(Error::query_execution("ALTER TABLE … SET SCHEMA requires a table name"));
        }
        let after = rest
            .get(set_pos + " SET SCHEMA".len()..)
            .ok_or_else(|| Error::query_execution("Invalid SET SCHEMA clause"))?
            .trim();
        if after.is_empty() {
            return Err(Error::query_execution(
                "ALTER TABLE … SET SCHEMA requires a target schema",
            ));
        }
        // A quoted target keeps its case; an unquoted one is folded to lower
        // (matching `normalize_object_name`'s schema handling).
        let new_schema = if after.starts_with('"') && after.ends_with('"') && after.len() >= 2 {
            after[1..after.len() - 1].to_string()
        } else {
            after.to_lowercase()
        };
        Ok((table_name, new_schema, if_exists))
    }

    /// Does this SQL start an `ALTER SEQUENCE` statement?
    ///
    /// sqlparser 0.53 has no `AlterSequence` variant — `parse_alter` only
    /// accepts VIEW/TABLE/INDEX/ROLE/POLICY and otherwise errors — so this is
    /// routed through a custom pre-parse path (mirroring
    /// `is_alter_column_storage`).
    pub fn is_alter_sequence(sql: &str) -> bool {
        let upper = sql.trim_start().to_uppercase();
        upper.starts_with("ALTER SEQUENCE")
    }

    /// Parse `ALTER SEQUENCE [IF EXISTS] <name> <actions...>` into an
    /// [`AlterSequenceAction`]. Actions may appear in any order:
    /// `RESTART [[WITH] n]`, `INCREMENT [BY] n`, `MINVALUE n | NO MINVALUE`,
    /// `MAXVALUE n | NO MAXVALUE`, `CACHE n`, `CYCLE | NO CYCLE`,
    /// `START [WITH] n`, `AS <type>`, `OWNED BY <table>.<col> | OWNED BY NONE`.
    ///
    /// Strip-and-scan style (mirrors `parse_alter_column_storage`): peel the
    /// `ALTER SEQUENCE [IF EXISTS]` header, read the sequence name up to the
    /// first action keyword, then extract each recognised clause with a regex,
    /// blanking it out. Any non-whitespace left over is an unsupported clause →
    /// a clear error (so we never silently accept a malformed ALTER).
    pub fn parse_alter_sequence(sql: &str) -> Result<AlterSequenceAction> {
        use regex::Regex;
        use std::sync::OnceLock;

        let trimmed = sql.trim().trim_end_matches(';').trim();

        // Strip "ALTER SEQUENCE".
        let upper = trimmed.to_uppercase();
        if !upper.starts_with("ALTER SEQUENCE") {
            return Err(Error::query_execution("Expected ALTER SEQUENCE"));
        }
        let mut rest = trimmed["ALTER SEQUENCE".len()..].trim_start();

        // Optional IF EXISTS.
        let mut if_exists = false;
        if rest.len() >= 9 && rest[..9].eq_ignore_ascii_case("IF EXISTS") {
            if_exists = true;
            rest = rest[9..].trim_start();
        }

        // Read the (possibly quoted, possibly schema-qualified) sequence name,
        // which ends at the first whitespace OUTSIDE a quoted segment. The
        // surrounding clause keywords are all alphabetic, so the first space
        // after the name terminates it.
        let (raw_name, after_name) = Self::read_sequence_name(rest)?;
        if raw_name.is_empty() {
            return Err(Error::query_execution("ALTER SEQUENCE requires a sequence name"));
        }
        let name = crate::sql::Planner::normalize_dotted_name(&raw_name);

        // Scan the action tail. Each entry: (kind, whole-clause matcher).
        // RESTART/START/AS/OWNED BY/CACHE/INCREMENT/MIN/MAX/CYCLE.
        static CLAUSES: OnceLock<Vec<(SeqClauseKind, Regex)>> = OnceLock::new();
        let clauses =
            CLAUSES.get_or_init(|| {
                use SeqClauseKind::*;
                vec![
                // RESTART must be tried before START so "RESTART" isn't seen as
                // a bare token; the `RESTART\b` alternative (no value) is last
                // in the alternation so `RESTART WITH n` wins greedily.
                (
                    Restart,
                    Regex::new(r"(?i)\bRESTART(?:\s+WITH)?\s+[+-]?\d+|\bRESTART\b").unwrap(),
                ),
                (Increment, Regex::new(r"(?i)\bINCREMENT(?:\s+BY)?\s+[+-]?\d+").unwrap()),
                (MinValue, Regex::new(r"(?i)\bNO\s+MINVALUE|\bMINVALUE\s+[+-]?\d+").unwrap()),
                (MaxValue, Regex::new(r"(?i)\bNO\s+MAXVALUE|\bMAXVALUE\s+[+-]?\d+").unwrap()),
                (Cache, Regex::new(r"(?i)\bCACHE\s+[+-]?\d+").unwrap()),
                (Cycle, Regex::new(r"(?i)\bNO\s+CYCLE|\bCYCLE\b").unwrap()),
                (Start, Regex::new(r"(?i)\bSTART(?:\s+WITH)?\s+[+-]?\d+").unwrap()),
                (
                    As,
                    Regex::new(r"(?i)\bAS\s+(?:SMALLINT|INTEGER|INT|BIGINT|INT2|INT4|INT8)\b").unwrap(),
                ),
                (
                    OwnedBy,
                    Regex::new(
                        r#"(?i)\bOWNED\s+BY\s+(?:NONE|(?:"[^"]+"|[A-Za-z_][\w$]*)(?:\.(?:"[^"]+"|[A-Za-z_][\w$]*))*)"#,
                    )
                    .unwrap(),
                ),
            ]
            });

        let mut remaining = after_name.to_string();
        let mut action = AlterSequenceAction {
            name,
            if_exists,
            ..Default::default()
        };

        for (kind, re) in clauses {
            // A clause may legitimately appear at most once; loop in case the
            // statement repeats it (last write wins, like PG tolerates).
            while let Some(m) = re.find(&remaining) {
                let clause = m.as_str().trim().to_string();
                let (a, b) = (m.start(), m.end());
                Self::apply_alter_seq_clause(*kind, &clause, &mut action)?;
                remaining.replace_range(a..b, " ");
            }
        }

        // PostgreSQL/Oracle ALTER SEQUENCE has no SET keyword — the clauses are
        // bare (INCREMENT BY n, MINVALUE m, …). But a2h-style migration tooling
        // (and the request that drove this) may emit `SET <option>`; tolerate a
        // stray standalone SET token as noise rather than erroring.
        let leftover: Vec<&str> = remaining
            .split_whitespace()
            .filter(|t| !t.eq_ignore_ascii_case("SET"))
            .collect();
        if !leftover.is_empty() {
            return Err(Error::query_execution(format!(
                "Unsupported or malformed ALTER SEQUENCE clause: '{}'",
                leftover.join(" ")
            )));
        }

        Ok(action)
    }

    /// Read a (quoted / schema-qualified) sequence name from the front of
    /// `s`, returning `(name, rest)`. Handles `"Quoted Name"`, `schema.name`,
    /// and bare identifiers; the name ends at the first whitespace that is not
    /// inside a double-quoted segment.
    fn read_sequence_name(s: &str) -> Result<(String, &str)> {
        let bytes = s.as_bytes();
        let mut i = 0;
        let mut in_quote = false;
        while i < bytes.len() {
            let c = bytes[i] as char;
            if c == '"' {
                in_quote = !in_quote;
            } else if c.is_whitespace() && !in_quote {
                break;
            }
            i += 1;
        }
        if in_quote {
            return Err(Error::query_execution(
                "Unterminated quoted identifier in ALTER SEQUENCE",
            ));
        }
        let name = s[..i].trim().to_string();
        Ok((name, s[i..].trim_start()))
    }

    /// Apply one parsed ALTER SEQUENCE clause to the accumulating action.
    fn apply_alter_seq_clause(kind: SeqClauseKind, clause: &str, action: &mut AlterSequenceAction) -> Result<()> {
        let upper = clause.to_uppercase();
        // Pull a trailing signed integer out of a clause (the last token).
        let trailing_i64 = |c: &str| -> Option<i64> { c.split_whitespace().last().and_then(|t| t.parse::<i64>().ok()) };
        match kind {
            SeqClauseKind::Restart => {
                // `RESTART` (no value) → Some(None); `RESTART [WITH] n` → Some(Some(n)).
                if upper == "RESTART" {
                    action.restart = Some(None);
                } else {
                    let n = trailing_i64(clause)
                        .ok_or_else(|| Error::query_execution("ALTER SEQUENCE RESTART requires an integer"))?;
                    action.restart = Some(Some(n));
                }
            }
            SeqClauseKind::Increment => {
                let n = trailing_i64(clause)
                    .ok_or_else(|| Error::query_execution("ALTER SEQUENCE INCREMENT requires an integer"))?;
                action.increment = Some(n);
            }
            SeqClauseKind::MinValue => {
                if upper.starts_with("NO") {
                    action.min_value = Some(None);
                } else {
                    let n = trailing_i64(clause)
                        .ok_or_else(|| Error::query_execution("ALTER SEQUENCE MINVALUE requires an integer"))?;
                    action.min_value = Some(Some(n));
                }
            }
            SeqClauseKind::MaxValue => {
                if upper.starts_with("NO") {
                    action.max_value = Some(None);
                } else {
                    let n = trailing_i64(clause)
                        .ok_or_else(|| Error::query_execution("ALTER SEQUENCE MAXVALUE requires an integer"))?;
                    action.max_value = Some(Some(n));
                }
            }
            SeqClauseKind::Cache => {
                let n = trailing_i64(clause)
                    .ok_or_else(|| Error::query_execution("ALTER SEQUENCE CACHE requires an integer"))?;
                action.cache = Some(n);
            }
            SeqClauseKind::Cycle => {
                action.cycle = Some(!upper.starts_with("NO"));
            }
            SeqClauseKind::Start => {
                let n = trailing_i64(clause)
                    .ok_or_else(|| Error::query_execution("ALTER SEQUENCE START requires an integer"))?;
                action.start_value = Some(n);
            }
            SeqClauseKind::As => {
                // `AS <type>` — last token is the type keyword.
                let ty = upper.split_whitespace().last().unwrap_or("BIGINT");
                let canonical = match ty {
                    "SMALLINT" | "INT2" => "smallint",
                    "INT" | "INTEGER" | "INT4" => "integer",
                    _ => "bigint",
                };
                action.data_type = Some(canonical.to_string());
            }
            SeqClauseKind::OwnedBy => {
                // `OWNED BY NONE` → Some(None); `OWNED BY t.c` → Some(Some((t, c))).
                // Strip the two leading keywords (OWNED, BY) by token, keeping
                // the original-cased reference intact.
                let after_owned =
                    clause[clause.char_indices().nth(5).map(|(i, _)| i).unwrap_or(clause.len())..].trim_start();
                let ref_part = if after_owned.len() >= 2 && after_owned[..2].eq_ignore_ascii_case("BY") {
                    after_owned[2..].trim()
                } else {
                    after_owned
                };
                if ref_part.eq_ignore_ascii_case("NONE") {
                    action.owned_by = Some(None);
                } else {
                    let normalized = crate::sql::Planner::normalize_dotted_name(ref_part);
                    let parts: Vec<&str> = normalized.split('.').collect();
                    if parts.len() >= 2 {
                        let table = parts[parts.len() - 2].to_string();
                        let col = parts[parts.len() - 1].to_string();
                        action.owned_by = Some(Some((table, col)));
                    } else {
                        action.owned_by = Some(None);
                    }
                }
            }
        }
        Ok(())
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
                if first_word == "PRIMARY"
                    || first_word == "FOREIGN"
                    || first_word == "UNIQUE"
                    || first_word == "CHECK"
                    || first_word == "CONSTRAINT"
                {
                    continue;
                }

                // Extract storage mode
                let after_storage = &col_upper[storage_pos + 9..]; // " STORAGE ".len() = 9
                let mode_end = after_storage
                    .find(|c: char| !c.is_alphabetic() && c != '_')
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
    /// Strip the parenthesized sequence-options block that follows
    /// `GENERATED ALWAYS AS IDENTITY` (or `BY DEFAULT`) in DDL emitted
    /// by drizzle-kit / Prisma:
    ///
    /// ```sql
    ///   id integer PRIMARY KEY GENERATED ALWAYS AS IDENTITY (
    ///     sequence name "tasks_id_seq" INCREMENT BY 1 MINVALUE 1
    ///     MAXVALUE 2147483647 START WITH 1 CACHE 1
    ///   ),
    /// ```
    ///
    /// becomes
    ///
    /// ```sql
    ///   id integer PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
    /// ```
    ///
    /// Quote-aware paren matching so identifiers like `"my(table)"`
    /// don't fool the scan.
    /// Reorder `CREATE SEQUENCE` option clauses into the fixed order
    /// sqlparser 0.53 requires (INCREMENT, MINVALUE, MAXVALUE, START, CACHE,
    /// CYCLE). PostgreSQL accepts these clauses in any order; sqlparser does
    /// not, so `CREATE SEQUENCE s START 100 INCREMENT 10` fails to parse even
    /// though `INCREMENT BY 10 START WITH 100` succeeds. Only the option tail
    /// of each CREATE SEQUENCE statement is rewritten; on any clause we don't
    /// recognise we leave the statement untouched (never corrupt valid DDL).
    pub fn preprocess_create_sequence_clause_order(sql: &str) -> String {
        use regex::Regex;
        use std::sync::OnceLock;

        // Cheap bail-out: only touch input that creates a sequence.
        let upper = sql.to_ascii_uppercase();
        if !upper.contains("SEQUENCE") || !upper.contains("CREATE") {
            return sql.to_string();
        }

        static STMT_RE: OnceLock<Regex> = OnceLock::new();
        // (1) header up to and including the sequence name,
        // (2) the option tail (non-greedy, up to the terminator),
        // (3) the terminator (`;` or end of string).
        let stmt_re = STMT_RE.get_or_init(|| {
            Regex::new(
                r#"(?is)(CREATE\s+(?:TEMP\s+|TEMPORARY\s+)?SEQUENCE\s+(?:IF\s+NOT\s+EXISTS\s+)?(?:"[^"]+"|[A-Za-z_][\w$]*)(?:\.(?:"[^"]+"|[A-Za-z_][\w$]*))?)(\s+[^;]*?)?(\s*(?:;|$))"#,
            )
            .expect("static CREATE SEQUENCE regex is valid")
        });

        stmt_re
            .replace_all(sql, |caps: &regex::Captures<'_>| {
                let header = &caps[1];
                let tail = caps.get(2).map_or("", |m| m.as_str());
                let term = &caps[3];
                if tail.trim().is_empty() {
                    return format!("{header}{term}");
                }
                match Self::reorder_sequence_option_tail(tail.trim()) {
                    Some(reordered) => format!("{header} {reordered}{term}"),
                    // Unknown clause present — leave the statement verbatim.
                    None => format!("{header}{tail}{term}"),
                }
            })
            .into_owned()
    }

    /// Extract the recognised sequence-option clauses from `tail` (in any
    /// order) and re-emit them in sqlparser's canonical order. Returns `None`
    /// if anything other than whitespace is left over (i.e. an option we don't
    /// model, such as `AS bigint` or `OWNED BY`), so the caller can leave the
    /// original statement untouched.
    fn reorder_sequence_option_tail(tail: &str) -> Option<String> {
        use regex::Regex;
        use std::sync::OnceLock;

        // (canonical rank, clause matcher). Each matches a whole clause.
        // Ranks mirror sqlparser 0.53's canonical Display order for a CREATE
        // SEQUENCE: `AS <type>` (immediately after the name) → the option
        // clauses (strict-ordered) → `OWNED BY <ref>` last. Matching AS/OWNED
        // BY here (instead of bailing) lets the preprocess reorder the FULL
        // statement so sqlparser both accepts it AND fills the data_type /
        // owned_by AST fields.
        static CLAUSES: OnceLock<Vec<(usize, Regex)>> = OnceLock::new();
        let clauses = CLAUSES.get_or_init(|| {
            vec![
                (
                    0usize,
                    Regex::new(r"(?i)\bAS\s+(?:SMALLINT|INTEGER|INT|BIGINT|INT2|INT4|INT8)\b").unwrap(),
                ),
                (1, Regex::new(r"(?i)\bINCREMENT(?:\s+BY)?\s+[+-]?\d+").unwrap()),
                (2, Regex::new(r"(?i)\b(?:NO\s+MINVALUE|MINVALUE\s+[+-]?\d+)").unwrap()),
                (3, Regex::new(r"(?i)\b(?:NO\s+MAXVALUE|MAXVALUE\s+[+-]?\d+)").unwrap()),
                (4, Regex::new(r"(?i)\bSTART(?:\s+WITH)?\s+[+-]?\d+").unwrap()),
                (5, Regex::new(r"(?i)\bCACHE\s+[+-]?\d+").unwrap()),
                (6, Regex::new(r"(?i)\b(?:NO\s+CYCLE|CYCLE)\b").unwrap()),
                (
                    7,
                    Regex::new(
                        r#"(?i)\bOWNED\s+BY\s+(?:NONE|(?:"[^"]+"|[A-Za-z_][\w$]*)(?:\.(?:"[^"]+"|[A-Za-z_][\w$]*))*)"#,
                    )
                    .unwrap(),
                ),
            ]
        });

        let mut remaining = tail.to_string();
        let mut found: Vec<(usize, String)> = Vec::new();
        for (rank, re) in clauses {
            if let Some(m) = re.find(&remaining) {
                let clause = m.as_str().trim().to_string();
                let (a, b) = (m.start(), m.end());
                remaining.replace_range(a..b, " ");
                found.push((*rank, clause));
            }
        }
        // Leftover non-whitespace ⇒ an option we don't understand: bail.
        if !remaining.trim().is_empty() || found.is_empty() {
            return None;
        }
        found.sort_by_key(|(rank, _)| *rank);
        Some(found.into_iter().map(|(_, c)| c).collect::<Vec<_>>().join(" "))
    }

    pub fn preprocess_strip_identity_options(sql: &str) -> String {
        let upper = sql.to_uppercase();
        let bytes = sql.as_bytes();
        let mut result = String::with_capacity(sql.len());
        let mut i = 0;
        let n = sql.len();
        while i < n {
            // Look for "AS IDENTITY" word-bounded.
            let remaining_upper = &upper[i..];
            if let Some(p) = remaining_upper.find("AS IDENTITY") {
                let abs = i + p;
                // Confirm word-bounded on both sides (so `IDENTITYX` isn't a hit).
                let before_ok = abs == 0
                    || !sql
                        .as_bytes()
                        .get(abs - 1)
                        .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                        .unwrap_or(false);
                let end_kw = abs + "AS IDENTITY".len();
                let after_ok = end_kw >= n
                    || !sql
                        .as_bytes()
                        .get(end_kw)
                        .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                        .unwrap_or(false);
                if !(before_ok && after_ok) {
                    // Copy through and continue past this occurrence.
                    let advance = (abs - i) + "AS IDENTITY".len();
                    #[allow(clippy::indexing_slicing)]
                    result.push_str(&sql[i..i + advance]);
                    i += advance;
                    continue;
                }
                // Copy through "AS IDENTITY".
                #[allow(clippy::indexing_slicing)]
                result.push_str(&sql[i..end_kw]);
                i = end_kw;
                // Look ahead: optional whitespace then '('.
                let mut j = i;
                while j < n && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n' || bytes[j] == b'\r') {
                    j += 1;
                }
                if j < n && bytes[j] == b'(' {
                    // Find matching close paren, respecting double-quoted identifiers.
                    let mut depth: usize = 1;
                    let mut k = j + 1;
                    let mut in_dquote = false;
                    while k < n && depth > 0 {
                        match bytes[k] {
                            b'"' => in_dquote = !in_dquote,
                            b'(' if !in_dquote => depth += 1,
                            b')' if !in_dquote => depth -= 1,
                            _ => {}
                        }
                        k += 1;
                    }
                    if depth == 0 {
                        // Drop the entire `(...)` block (k is one past ')').
                        i = k;
                        continue;
                    }
                    // Unbalanced; bail out — don't mutate sql we don't understand.
                }
                // No "(" follows; nothing to strip.
                continue;
            }
            // No more "AS IDENTITY" — copy the rest.
            #[allow(clippy::indexing_slicing)]
            result.push_str(&sql[i..]);
            break;
        }
        result
    }

    pub fn preprocess_remove_storage_clauses(sql: &str) -> String {
        let upper = sql.to_uppercase();

        // Only process CREATE TABLE statements
        if !upper.trim_start().starts_with("CREATE TABLE") {
            return sql.to_string();
        }

        let mut result = sql.to_string();

        // Remove all variations of STORAGE clause
        for mode in &[
            "STORAGE DICTIONARY",
            "STORAGE CONTENT_ADDRESSED",
            "STORAGE COLUMNAR",
            "STORAGE DEFAULT",
        ] {
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
        upper.starts_with("CREATE PROCEDURE")
            && upper.contains("LANGUAGE")
            && (upper.contains(" AS ") || upper.contains(" AS$"))
    }

    /// Check if SQL is a PostgreSQL-style CREATE OR REPLACE PROCEDURE statement
    pub fn is_pg_create_or_replace_procedure(sql: &str) -> bool {
        let upper = sql.trim().to_uppercase();
        upper.starts_with("CREATE OR REPLACE PROCEDURE")
            && upper.contains("LANGUAGE")
            && (upper.contains(" AS ") || upper.contains(" AS$"))
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
        let paren_pos = after_create
            .find('(')
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
        let lang_pos = upper_after
            .find("LANGUAGE")
            .ok_or_else(|| Error::sql_parse("CREATE PROCEDURE requires LANGUAGE clause"))?;

        let after_lang = after_params[lang_pos + 8..].trim_start(); // "LANGUAGE".len() = 8

        // Extract language name (ends at whitespace or AS)
        let lang_end = after_lang.find(|c: char| c.is_whitespace()).unwrap_or(after_lang.len());
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

        Err(Error::sql_parse(
            "Procedure body must be quoted with $$ or single quotes",
        ))
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
            let index_type_end = after_using
                .find(|c: char| c.is_whitespace() || c == ';' || c == '(')
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
            let index_type_end = after_using
                .find(|c: char| c == '(' || c.is_whitespace())
                .unwrap_or(after_using.len());
            let index_type = after_using[..index_type_end].trim().to_string();
            let remaining = after_using[index_type_end..].trim_start();

            // Parse column specification from parentheses
            if let Some(paren_start) = remaining.find('(') {
                let paren_content_start = paren_start + 1;
                if let Some(paren_end) = remaining[paren_content_start..].find(')') {
                    let paren_content = &remaining[paren_content_start..paren_content_start + paren_end];

                    // Extract just the column name(s), preserving vector
                    // operator-class metric as a regular WITH option.
                    let (column_spec, metric) = Self::strip_operator_classes(paren_content);

                    // Get anything after the closing paren (WITH clause, semicolon, etc.)
                    let after_paren = remaining[paren_content_start + paren_end + 1..].trim();
                    let after_paren = Self::append_metric_index_option(after_paren, metric);

                    // Reconstruct: before_using + (column_spec) + after_paren
                    let cleaned_sql = if after_paren.is_empty() || after_paren == ";" {
                        format!("{} ({});", before_using, column_spec)
                    } else {
                        format!(
                            "{} ({}) {};",
                            before_using,
                            column_spec,
                            after_paren.trim_end_matches(';')
                        )
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
    fn strip_operator_classes(column_spec: &str) -> (String, Option<&'static str>) {
        // Known vector operator classes to strip
        let op_classes = [
            ("vector_l2_ops", "l2"),
            ("vector_cosine_ops", "cosine"),
            ("vector_ip_ops", "inner_product"),
            ("vector_inner_product_ops", "inner_product"),
        ];

        let mut result = column_spec.to_string();
        let mut metric = None;
        for (op_class, op_metric) in &op_classes {
            // Case-insensitive removal
            let upper_result = result.to_uppercase();
            let upper_op = op_class.to_uppercase();
            if let Some(pos) = upper_result.find(&upper_op) {
                metric.get_or_insert(*op_metric);
                result = format!(
                    "{}{}",
                    result[..pos].trim_end(),
                    result[pos + op_class.len()..].trim_start()
                );
            }
        }
        (result.trim().to_string(), metric)
    }

    fn append_metric_index_option(after_paren: &str, metric: Option<&str>) -> String {
        let Some(metric) = metric else {
            return after_paren.to_string();
        };
        let trimmed = after_paren.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            return format!("WITH (metric = '{metric}')");
        }

        let upper = trimmed.to_uppercase();
        if upper.starts_with("WITH") {
            if let Some(close_pos) = trimmed.rfind(')') {
                let before_close = trimmed[..close_pos].trim_end();
                let after_close = trimmed[close_pos..].trim_start_matches(')');
                let separator = if before_close.ends_with('(') { "" } else { ", " };
                return format!("{before_close}{separator}metric = '{metric}'){after_close}");
            }
        }

        format!("{trimmed} WITH (metric = '{metric}')")
    }

    /// Rewrite the SQL:2016 / Oracle `<column> IS [NOT] JSON [STRICT|LAX]
    /// [WITH|WITHOUT [UNIQUE] KEYS]` predicate into a `json_valid(<column>)`
    /// call that sqlparser 0.53 accepts.
    ///
    /// sqlparser only understands `IS [NOT] NULL`, `IS TRUE|FALSE|UNKNOWN` and
    /// `IS [NOT] DISTINCT FROM` after `IS`; the bare `JSON` keyword otherwise
    /// errors during parsing — before the planner is ever reached — and blocks
    /// an Oracle→HeliosDB migrate that emits e.g. `CHECK (mfa IS JSON)`.
    ///
    /// The lowering is operand-faithful and NULL-safe:
    ///   `<col> IS JSON`      → `json_valid(<col>)`
    ///   `<col> IS NOT JSON`  → `(NOT json_valid(<col>))`
    /// `json_valid` returns NULL for a NULL input, so inside a CHECK (enforced
    /// per-row) a NULL value is treated as satisfied — exactly as real `IS JSON`
    /// behaves — and a migrate never spuriously rejects a NULL row. Because the
    /// result is a function call it is also precedence-safe inside compound
    /// `AND`/`OR` predicates (unlike rewriting to `IS NOT NULL OR TRUE`).
    ///
    /// Conservative by design: only a simple (optionally schema-qualified)
    /// column-reference operand is matched, and never inside a single-quoted
    /// string literal or a double-quoted identifier. A non-trivial operand
    /// (function call, concatenation, parenthesised sub-expression) is left
    /// untouched and will still fail to parse — acceptable, as migrated `IS
    /// JSON` predicates are applied to bare columns. The optional Oracle
    /// modifier tail (STRICT/LAX, WITH/WITHOUT [UNIQUE] KEYS) is consumed.
    pub fn preprocess_is_json(sql: &str) -> String {
        // Fast path: nothing to do without the JSON keyword.
        if !sql.to_ascii_uppercase().contains("JSON") {
            return sql.to_string();
        }
        use regex::Regex;
        use std::sync::OnceLock;
        // Operand = simple (optionally `schema.col`) identifier, then
        // `IS [NOT] JSON`, then the optional Oracle modifier tail.
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            Regex::new(
                r"(?i)\b([A-Za-z_][A-Za-z0-9_$]*(?:\.[A-Za-z_][A-Za-z0-9_$]*)?)\s+IS\s+(NOT\s+)?JSON\b(?:\s+(?:STRICT|LAX))?(?:\s+(?:WITH|WITHOUT)(?:\s+UNIQUE)?(?:\s+KEYS)?)?",
            )
            .expect("static IS JSON regex is valid")
        });

        let rewrite_unquoted = |span: &str, out: &mut String| {
            let replaced = re.replace_all(span, |caps: &regex::Captures<'_>| {
                let operand = &caps[1];
                if caps.get(2).is_some() {
                    format!("(NOT json_valid({operand}))")
                } else {
                    format!("json_valid({operand})")
                }
            });
            out.push_str(&replaced);
        };

        // Quote-aware: copy single-quoted string literals and double-quoted
        // identifiers verbatim; only rewrite the unquoted spans between them, so
        // a literal like `'this is json text'` is never corrupted. Quote bytes
        // are ASCII, so byte indices land on char boundaries.
        let bytes = sql.as_bytes();
        let mut out = String::with_capacity(sql.len() + 16);
        let mut seg_start = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\'' || c == b'"' {
                rewrite_unquoted(&sql[seg_start..i], &mut out);
                let quote = c;
                let qstart = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        i += 1;
                        // A doubled quote ('' or "") is an escape: stay in the literal.
                        if i < bytes.len() && bytes[i] == quote {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                out.push_str(&sql[qstart..i]);
                seg_start = i;
                continue;
            }
            i += 1;
        }
        rewrite_unquoted(&sql[seg_start..], &mut out);
        out
    }

    /// Strip a trailing comma that sits immediately before the closing `)` of a
    /// CREATE TABLE column/constraint list.
    ///
    /// a2h's Oracle->HeliosDB export emits `CREATE TABLE t ( ... , PRIMARY KEY
    /// (id), )` — a dangling comma after the last item. sqlparser 0.53 (like
    /// PostgreSQL) rejects it: "Expected: column name or constraint definition,
    /// found: )". Nano positions as migration-friendly, so this defensive
    /// rewrite removes only the offending comma.
    ///
    /// Safety:
    /// - Scoped to CREATE TABLE statements only (early return otherwise), so
    ///   INSERT ... VALUES, multi-row `(..),(..)`, ARRAY / row constructors and
    ///   function-call defaults in other statements are never touched.
    /// - Quote-aware (mirrors `preprocess_is_json`): single-quoted string
    ///   literals and double-quoted identifiers are copied verbatim, so a
    ///   literal such as `DEFAULT ',)'` is preserved.
    /// - Only the comma is removed, never the paren; a legitimate `),`
    ///   (comma followed by the next column/constraint) does not match because
    ///   the comma is not immediately followed by `)`.
    pub fn preprocess_strip_trailing_commas(sql: &str) -> String {
        // Only CREATE TABLE statements can carry this artifact.
        if !sql.trim_start().to_ascii_uppercase().starts_with("CREATE TABLE") {
            return sql.to_string();
        }
        // Fast path: no `,` means nothing to strip.
        if !sql.contains(',') {
            return sql.to_string();
        }
        use regex::Regex;
        use std::sync::OnceLock;
        // A comma, then only whitespace, then a closing paren.
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| Regex::new(r",\s*\)").expect("static trailing-comma regex is valid"));

        let rewrite_unquoted = |span: &str, out: &mut String| {
            let replaced = re.replace_all(span, ")");
            out.push_str(&replaced);
        };

        // Quote-aware walk: copy single-quoted string literals and double-quoted
        // identifiers verbatim; only rewrite the unquoted spans between them.
        // Quote bytes are ASCII, so byte indices land on char boundaries.
        let bytes = sql.as_bytes();
        let mut out = String::with_capacity(sql.len());
        let mut seg_start = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\'' || c == b'"' {
                rewrite_unquoted(&sql[seg_start..i], &mut out);
                let quote = c;
                let qstart = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        i += 1;
                        // A doubled quote ('' or "") is an escape: stay in the literal.
                        if i < bytes.len() && bytes[i] == quote {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                out.push_str(&sql[qstart..i]);
                seg_start = i;
                continue;
            }
            i += 1;
        }
        rewrite_unquoted(&sql[seg_start..], &mut out);
        out
    }

    /// Round-2 pgrust-corpus compat (~207 corpus statements): strip the
    /// PostgreSQL `INHERITS (parent[, …])` table-option clause from a
    /// CREATE TABLE so the statement parses. sqlparser 0.53 has no INHERITS
    /// grammar at all, so `CREATE TABLE child (…) INHERITS (parent)` fails
    /// at the parse stage before the planner. Faithful column/constraint
    /// merge from the parent(s) is intentionally out of scope for this
    /// zero-regression pass -- stripping the clause lets the child table be
    /// created with its own explicitly-listed columns, which is the
    /// pragmatic compatibility win the round-2 diagnosis asked for.
    ///
    /// Quote-aware and keyword-boundary-aware, mirroring
    /// `preprocess_strip_trailing_commas` above: an `INHERITS (` sequence
    /// inside a single-quoted string literal or a double-quoted identifier
    /// is copied through untouched, a column named `inherits` (not followed
    /// by `(`) is left alone, and an identifier that merely embeds the
    /// letters (`my_inherits`) never matches. Only the first INHERITS clause
    /// is removed -- a CREATE TABLE carries at most one. Any options that
    /// follow the clause (`WITH (…)`, `TABLESPACE …`, …) are preserved.
    ///
    /// Cheap early-outs keep this off the hot path: an allocation-free
    /// `starts_with_icase("CREATE TABLE")` gate, then a single
    /// case-insensitive substring probe for "INHERITS"; only a CREATE TABLE
    /// statement that actually contains those letters does any byte-walk
    /// work. This runs only at parse time (once per statement, alongside the
    /// sibling preprocessors).
    pub fn preprocess_strip_inherits(sql: &str) -> String {
        // Gate strictly to `CREATE TABLE`, matching the sibling
        // preprocess_strip_trailing_commas. This is where INHERITS lives and
        // -- critically -- it excludes CREATE FUNCTION / PROCEDURE / TRIGGER,
        // whose dollar-quoted bodies this ' / "-only quote walk does not
        // track. UNLOGGED / TEMP table variants are not covered (the same
        // deliberate tradeoff the sibling makes); the corpus INHERITS cases
        // are all plain CREATE TABLE.
        if !crate::starts_with_icase(sql.trim_start(), "CREATE TABLE") {
            return sql.to_string();
        }
        // The keyword must be literally present before any byte walk.
        if !sql.to_ascii_uppercase().contains("INHERITS") {
            return sql.to_string();
        }

        let bytes = sql.as_bytes();
        let n = bytes.len();
        let mut i = 0usize;
        while i < n {
            let c = bytes[i];
            // Copy single-quoted string literals / double-quoted identifiers
            // verbatim (a doubled quote is an escape: stay in the literal).
            if c == b'\'' || c == b'"' {
                let quote = c;
                i += 1;
                while i < n {
                    if bytes[i] == quote {
                        i += 1;
                        if i < n && bytes[i] == quote {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                continue;
            }

            // A standalone INHERITS keyword (case-insensitive) with a left
            // word boundary, immediately followed (after optional whitespace)
            // by '('.
            if (c == b'I' || c == b'i')
                && i + 8 <= n
                && bytes[i..i + 8].eq_ignore_ascii_case(b"INHERITS")
                && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'))
            {
                let after = i + 8;
                let right_boundary_ok = after >= n || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
                if right_boundary_ok {
                    let mut j = after;
                    while j < n && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < n && bytes[j] == b'(' {
                        // Balance to the matching ')', skipping quoted spans.
                        // depth is 0 only before the opening paren is counted,
                        // which is the first byte examined here, so the ')'
                        // decrement can never underflow.
                        let mut depth = 0usize;
                        let mut k = j;
                        let mut close = None;
                        while k < n {
                            let ck = bytes[k];
                            if ck == b'\'' || ck == b'"' {
                                let q = ck;
                                k += 1;
                                while k < n {
                                    if bytes[k] == q {
                                        k += 1;
                                        if k < n && bytes[k] == q {
                                            k += 1;
                                            continue;
                                        }
                                        break;
                                    }
                                    k += 1;
                                }
                                continue;
                            }
                            if ck == b'(' {
                                depth += 1;
                            } else if ck == b')' {
                                depth -= 1;
                                if depth == 0 {
                                    close = Some(k);
                                    break;
                                }
                            }
                            k += 1;
                        }
                        if let Some(close) = close {
                            // Also drop the whitespace run immediately before
                            // INHERITS so `) INHERITS (p)` collapses to `)`.
                            let mut left = i;
                            while left > 0 && bytes[left - 1].is_ascii_whitespace() {
                                left -= 1;
                            }
                            let mut out = String::with_capacity(n);
                            out.push_str(&sql[..left]);
                            out.push_str(&sql[close + 1..]);
                            return out;
                        }
                    }
                }
            }
            i += 1;
        }
        sql.to_string()
    }

    /// Stage-0 partitioning fallback (invoked by [`Parser::parse`] ONLY after
    /// the normal parse fails): apply the child `PARTITION OF` rewrite then the
    /// parent `PARTITION BY` strip. Returns `Some` only when something changed,
    /// so a non-partition parse error is reported unchanged and currently-
    /// passing SQL is never rewritten.
    fn rewrite_partition_syntax(sql: &str) -> Option<String> {
        let after_of = Self::preprocess_partition_of(sql);
        let rewritten = Self::preprocess_strip_partition_by(&after_of);
        if rewritten != sql {
            Some(rewritten)
        } else {
            None
        }
    }

    /// Stage-0 partitioning: rewrite a child
    /// `CREATE TABLE [IF NOT EXISTS] [schema.]child PARTITION OF …` declaration
    /// into a plain empty-column `CREATE TABLE [IF NOT EXISTS] [schema.]child ()`
    /// that sqlparser 0.53 accepts (0.53 has no `PARTITION OF` grammar, so the
    /// original fails as "Expected: end of statement, found: PARTITION"). This
    /// only makes the statement parse; the parent-column clone happens later in
    /// the planner (the first layer with catalog access) keyed off
    /// [`Parser::extract_partition_of`] of the original SQL. Non-`PARTITION OF`
    /// SQL is returned unchanged.
    pub fn preprocess_partition_of(sql: &str) -> String {
        match Self::extract_partition_of(sql) {
            Some(spec) => {
                let ine = if spec.if_not_exists { "IF NOT EXISTS " } else { "" };
                let semi = if sql.trim_end().ends_with(';') { ";" } else { "" };
                format!("CREATE TABLE {ine}{child} (){semi}", child = spec.child)
            }
            None => sql.to_string(),
        }
    }

    /// Stage-0 partitioning: match `CREATE TABLE [IF NOT EXISTS]
    /// [schema.]child PARTITION OF [schema.]parent { FOR VALUES … | DEFAULT }
    /// [PARTITION BY …] [WITH (…)] [TABLESPACE …]` and capture the
    /// `(child, parent, bound)` reference. Returns `None` for any statement that
    /// is not a `CREATE TABLE … PARTITION OF …` child declaration — in
    /// particular a plain `CREATE TABLE t ()` or `CREATE TABLE t () INHERITS
    /// (…)` (no `PARTITION OF`) yields `None`, so the planner's empty-column
    /// disambiguation holds. Whitespace/newline tolerant, case-insensitive,
    /// quote-aware for the name tokens. Only plain `CREATE TABLE` is matched
    /// (not `TEMP`/`UNLOGGED`), the same deliberate scope as the sibling
    /// `preprocess_strip_inherits`.
    pub(crate) fn extract_partition_of(sql: &str) -> Option<PartitionOfSpec> {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        let after_create = Self::strip_kw(trimmed, "CREATE")?;
        let after_table = Self::strip_kw(after_create, "TABLE")?;
        let (after_head, if_not_exists) = match Self::strip_kw(after_table, "IF") {
            Some(rest) => {
                let rest = Self::strip_kw(rest, "NOT")?;
                let rest = Self::strip_kw(rest, "EXISTS")?;
                (rest, true)
            }
            None => (after_table, false),
        };
        let (child, after_child) = Self::read_object_name(after_head)?;
        let after_partition = Self::strip_kw(after_child, "PARTITION")?;
        let after_of = Self::strip_kw(after_partition, "OF")?;
        let (parent, after_parent) = Self::read_object_name(after_of)?;
        let bound_full = after_parent.trim();
        // A genuine child clause continues with FOR VALUES / DEFAULT; this guard
        // keeps a stray `PARTITION OF` in some other construct from mis-firing.
        if !(Self::starts_kw(bound_full, "FOR") || Self::starts_kw(bound_full, "DEFAULT")) {
            return None;
        }
        Some(PartitionOfSpec {
            if_not_exists,
            child,
            parent,
            bound: Self::trim_partition_of_bound(bound_full),
        })
    }

    /// Stage-0 partitioning: strip a parent `PARTITION BY RANGE|LIST|HASH (…)`
    /// clause (whole clause, balanced parens) off a `CREATE TABLE` so sqlparser
    /// 0.53 can parse the parent. 0.53 already accepts a single-column key
    /// (`PARTITION BY RANGE (a)`) but rejects multi-column / expression /
    /// opclass keys (`RANGE (a, (b+0))`, `HASH (a part_test_int4_ops)`);
    /// stripping the whole clause uniformly accepts them all, flattening the
    /// parent to a plain empty table (Stage-0: the parent holds no rows).
    /// Anything after the clause (`WITH (…)`, `TABLESPACE …`, `;`) is preserved.
    ///
    /// The scan matches only a `PARTITION BY {RANGE|LIST|HASH} (` at **paren
    /// depth 0** inside a `CREATE TABLE`, which excludes window
    /// `OVER (PARTITION BY …)` (always inside `OVER`'s parens, depth ≥ 1) even
    /// within a `CREATE TABLE … AS SELECT …`. Quote-aware, case-insensitive,
    /// whitespace/newline tolerant. Because [`Parser::parse`] only calls this on
    /// the post-failure fallback path, a single-column parent that already
    /// parses is never touched.
    // Byte-scanner idiom (bounds guarded by `while i < n` + explicit
    // `i + k <= n` checks), matching the sibling `preprocess_strip_inherits`.
    #[allow(clippy::indexing_slicing)]
    pub fn preprocess_strip_partition_by(sql: &str) -> String {
        if !crate::starts_with_icase(sql.trim_start(), "CREATE TABLE") {
            return sql.to_string();
        }
        // Cheap keyword pre-check before any byte walk.
        if !sql.to_ascii_uppercase().contains("PARTITION") {
            return sql.to_string();
        }
        let bytes = sql.as_bytes();
        let n = bytes.len();
        let mut i = 0usize;
        let mut depth = 0i32;
        while i < n {
            let c = bytes[i];
            if c == b'\'' || c == b'"' {
                i = Self::skip_quoted_span(bytes, i);
                continue;
            }
            if c == b'(' {
                depth += 1;
                i += 1;
                continue;
            }
            if c == b')' {
                depth -= 1;
                i += 1;
                continue;
            }
            // `PARTITION` at depth 0 with a left word boundary.
            if depth == 0
                && (c == b'P' || c == b'p')
                && i + 9 <= n
                && bytes[i..i + 9].eq_ignore_ascii_case(b"PARTITION")
                && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'))
            {
                // … whitespace … BY … whitespace … {RANGE|LIST|HASH} … '('
                let mut j = i + 9;
                let ws0 = j;
                while j < n && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j > ws0
                    && j + 2 <= n
                    && bytes[j..j + 2].eq_ignore_ascii_case(b"BY")
                    && (j + 2 >= n || !(bytes[j + 2].is_ascii_alphanumeric() || bytes[j + 2] == b'_'))
                {
                    let mut k = j + 2;
                    while k < n && bytes[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    let strat_start = k;
                    while k < n && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
                        k += 1;
                    }
                    let is_strategy = {
                        let s = &sql[strat_start..k];
                        s.eq_ignore_ascii_case("RANGE")
                            || s.eq_ignore_ascii_case("LIST")
                            || s.eq_ignore_ascii_case("HASH")
                    };
                    if is_strategy {
                        let mut m = k;
                        while m < n && bytes[m].is_ascii_whitespace() {
                            m += 1;
                        }
                        if m < n && bytes[m] == b'(' {
                            if let Some(close) = Self::matching_paren_bytes(bytes, m) {
                                // Also drop the whitespace run before PARTITION.
                                let mut left = i;
                                while left > 0 && bytes[left - 1].is_ascii_whitespace() {
                                    left -= 1;
                                }
                                let mut out = String::with_capacity(n);
                                out.push_str(&sql[..left]);
                                out.push_str(&sql[close + 1..]);
                                return out;
                            }
                        }
                    }
                }
            }
            i += 1;
        }
        sql.to_string()
    }

    /// Strip a leading ASCII keyword `kw` (case-insensitive) from `s` (after
    /// leading whitespace), requiring a right word boundary, and return the
    /// remainder with leading whitespace trimmed. `None` if `s` does not start
    /// with the whole keyword as a distinct token.
    #[allow(clippy::indexing_slicing)] // kw is ASCII and `starts_with_icase` proved `s` starts with it.
    fn strip_kw<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
        let s = s.trim_start();
        if !crate::starts_with_icase(s, kw) {
            return None;
        }
        let rest = &s[kw.len()..];
        if let Some(c) = rest.chars().next() {
            if c.is_alphanumeric() || c == '_' {
                return None;
            }
        }
        Some(rest.trim_start())
    }

    /// `starts_with_icase` plus a right word boundary — tests a clause's leading
    /// keyword without consuming it.
    #[allow(clippy::indexing_slicing)] // kw is ASCII and `starts_with_icase` proved `s` starts with it.
    fn starts_kw(s: &str, kw: &str) -> bool {
        let s = s.trim_start();
        if !crate::starts_with_icase(s, kw) {
            return false;
        }
        match s[kw.len()..].chars().next() {
            Some(c) => !(c.is_alphanumeric() || c == '_'),
            None => true,
        }
    }

    /// Read one object-name token from the front of `s` (after leading
    /// whitespace): a possibly schema-qualified, possibly double-quoted
    /// identifier, terminated by ASCII whitespace at quote depth 0. Returns the
    /// raw token (verbatim, quotes preserved) and the remaining slice.
    #[allow(clippy::indexing_slicing)] // Byte cursor bounded by `while i < n`; slices are on ASCII boundaries.
    fn read_object_name(s: &str) -> Option<(String, &str)> {
        let s = s.trim_start();
        let bytes = s.as_bytes();
        let n = bytes.len();
        if n == 0 {
            return None;
        }
        let mut i = 0usize;
        let mut in_quote = false;
        while i < n {
            let c = bytes[i];
            if c == b'"' {
                if in_quote {
                    // Doubled "" is an embedded quote — stay inside.
                    if i + 1 < n && bytes[i + 1] == b'"' {
                        i += 2;
                        continue;
                    }
                    in_quote = false;
                } else {
                    in_quote = true;
                }
                i += 1;
                continue;
            }
            if !in_quote && c.is_ascii_whitespace() {
                break;
            }
            i += 1;
        }
        if i == 0 {
            return None;
        }
        Some((s[..i].to_string(), &s[i..]))
    }

    /// Trim a trailing sub-partition / storage tail (`PARTITION BY …`,
    /// `TABLESPACE …`) off a captured bound clause, leaving the `FOR VALUES …`
    /// / `DEFAULT` text (which may itself contain a `WITH (MODULUS …,
    /// REMAINDER …)` HASH bound — so `WITH` is deliberately NOT a cut point).
    /// Scans at paren depth 0, outside quotes. The result is recorded verbatim
    /// and never interpreted at Stage 0.
    #[allow(clippy::indexing_slicing)] // Byte cursor bounded by `while i < n`; slices are on ASCII boundaries.
    fn trim_partition_of_bound(bound: &str) -> String {
        let bytes = bound.as_bytes();
        let n = bytes.len();
        let mut i = 0usize;
        let mut depth = 0i32;
        while i < n {
            let c = bytes[i];
            if c == b'\'' || c == b'"' {
                i = Self::skip_quoted_span(bytes, i);
                continue;
            }
            if c == b'(' {
                depth += 1;
                i += 1;
                continue;
            }
            if c == b')' {
                depth -= 1;
                i += 1;
                continue;
            }
            if depth == 0 && c.is_ascii_whitespace() {
                let mut ws_end = i;
                while ws_end < n && bytes[ws_end].is_ascii_whitespace() {
                    ws_end += 1;
                }
                let rest = &bound[ws_end..];
                if Self::starts_kw(rest, "PARTITION") || Self::starts_kw(rest, "TABLESPACE") {
                    return bound[..i].trim_end().to_string();
                }
                i = ws_end;
                continue;
            }
            i += 1;
        }
        bound.trim_end().to_string()
    }

    /// If `bytes[i]` opens a `'`/`"` quoted span, return the index just past its
    /// closing quote (a doubled quote is an escape); otherwise return `i`.
    #[allow(clippy::indexing_slicing)] // Byte cursor bounded by `while k < n`; `i` is a valid caller index.
    fn skip_quoted_span(bytes: &[u8], i: usize) -> usize {
        let n = bytes.len();
        let q = bytes[i];
        if q != b'\'' && q != b'"' {
            return i;
        }
        let mut k = i + 1;
        while k < n {
            if bytes[k] == q {
                k += 1;
                if k < n && bytes[k] == q {
                    k += 1;
                    continue;
                }
                return k;
            }
            k += 1;
        }
        k
    }

    /// Index of the `)` matching the `(` at `open`, honoring quoted spans.
    /// `None` if unbalanced.
    #[allow(clippy::indexing_slicing)] // Byte cursor bounded by `while i < n`.
    fn matching_paren_bytes(bytes: &[u8], open: usize) -> Option<usize> {
        let n = bytes.len();
        let mut depth = 0i32;
        let mut i = open;
        while i < n {
            let c = bytes[i];
            if c == b'\'' || c == b'"' {
                i = Self::skip_quoted_span(bytes, i);
                continue;
            }
            if c == b'(' {
                depth += 1;
            } else if c == b')' {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            i += 1;
        }
        None
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
    /// - CREATE DATABASE BRANCH IF NOT EXISTS `<name>` FROM `<parent>` AS OF NOW
    /// - CREATE DATABASE BRANCH `<name>` WITH (option = value)
    pub fn parse_create_branch_sql(sql: &str) -> Result<(String, Option<String>, String, Option<String>, bool)> {
        let cleaned = sql.trim().to_string();
        let upper = cleaned.to_uppercase();

        // Extract branch name - first identifier after CREATE [DATABASE] BRANCH
        let name_start = if upper.starts_with("CREATE DATABASE BRANCH") {
            "CREATE DATABASE BRANCH".len()
        } else {
            "CREATE BRANCH".len()
        };

        let mut after_create = cleaned[name_start..].trim_start();
        let if_not_exists = if after_create.to_uppercase().starts_with("IF NOT EXISTS") {
            let after_clause = &after_create["IF NOT EXISTS".len()..];
            if !after_clause.is_empty() && after_clause.chars().next().map(|c| c.is_whitespace()).unwrap_or(false) {
                after_create = after_clause.trim_start();
                true
            } else {
                false
            }
        } else {
            false
        };

        // Branch names accept three forms:
        //   - bare identifier:           CREATE BRANCH foo AS OF NOW
        //   - single-quoted string:      CREATE BRANCH 'foo' AS OF NOW
        //   - double-quoted identifier:  CREATE BRANCH "foo" AS OF NOW
        // Strip the surrounding quotes when present so the branch is
        // stored under its intended bare name (Quirk C from the
        // dashboard cutover: 'verify-branch' was being stored verbatim
        // including the quotes, making the branch unfindable).
        let (branch_name, name_end) = if after_create.starts_with('\'') {
            let rest = &after_create[1..];
            let close = rest
                .find('\'')
                .ok_or_else(|| Error::query_execution("CREATE BRANCH: unterminated quoted branch name"))?;
            (rest[..close].to_string(), 1 + close + 1)
        } else if after_create.starts_with('"') {
            let rest = &after_create[1..];
            let close = rest
                .find('"')
                .ok_or_else(|| Error::query_execution("CREATE BRANCH: unterminated double-quoted branch name"))?;
            (rest[..close].to_string(), 1 + close + 1)
        } else {
            let end = after_create
                .find(|c: char| c.is_whitespace() || c == ';')
                .unwrap_or(after_create.len());
            (after_create[..end].to_string(), end)
        };

        if branch_name.is_empty() {
            return Err(Error::query_execution("CREATE BRANCH requires a branch name"));
        }

        // Find AS OF clause (required)
        let remaining = after_create[name_end..].trim();
        let upper_remaining = remaining.to_uppercase();

        // Look for FROM clause (optional parent)
        let parent = if let Some(from_pos) = upper_remaining.find("FROM ") {
            let after_from = remaining[from_pos + 5..].trim_start();
            let from_end = after_from
                .find(|c: char| c.is_whitespace() || c == ';')
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
        let as_of_pos = upper_remaining
            .find("AS OF")
            .ok_or_else(|| Error::query_execution("CREATE BRANCH requires AS OF clause"))?;

        let after_as_of = remaining[as_of_pos + 5..].trim_start();

        // Find end of AS OF clause (WITH, WHERE, GROUP, ORDER, LIMIT, UNION, ;, or end)
        let as_of_end_keywords = ["WITH", "WHERE", "GROUP", "ORDER", "LIMIT", "UNION", ";"];
        let as_of_end = as_of_end_keywords
            .iter()
            .filter_map(|&kw| {
                if let Some(pos) = after_as_of.to_uppercase().find(kw) {
                    if pos == 0
                        || after_as_of
                            .chars()
                            .nth(pos.saturating_sub(1))
                            .map(|c| c.is_whitespace())
                            .unwrap_or(true)
                    {
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
            if opts.is_empty() {
                None
            } else {
                Some(opts)
            }
        } else {
            None
        };

        Ok((branch_name, parent, as_of_clause, with_options, if_not_exists))
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
        let name_end = remaining
            .find(|c: char| c.is_whitespace() || c == ';')
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
        let source_end = after_merge
            .find(|c: char| c.is_whitespace())
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
        let target_end = after_into
            .find(|c: char| c.is_whitespace() || c == ';')
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
            if opts.is_empty() {
                None
            } else {
                Some(opts)
            }
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
        let name_end = after_use
            .find(|c: char| c.is_whitespace() || c == ';')
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
        upper.starts_with("SHOW CLUSTER STATUS")
            || upper.starts_with("SHOW HA STATUS")
            || upper.starts_with("SHOW REPLICATION STATUS")
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
        let to_pos = upper
            .find("TO ")
            .ok_or_else(|| Error::query_execution("SWITCHOVER statement requires TO clause"))?;

        let after_to = cleaned[to_pos + 3..].trim_start();

        // Extract node identifier - may be quoted or unquoted
        let node_id = if after_to.starts_with('\'') || after_to.starts_with('"') {
            // Quoted identifier
            let quote_char = if after_to.starts_with('\'') { '\'' } else { '"' };
            let end_quote = after_to[1..]
                .find(quote_char)
                .ok_or_else(|| Error::query_execution("Unterminated quote in node identifier"))?;
            after_to[1..=end_quote].to_string()
        } else {
            // Unquoted identifier
            let end_pos = after_to
                .find(|c: char| c.is_whitespace() || c == ';')
                .unwrap_or(after_to.len());
            after_to[..end_pos].to_string()
        };

        if node_id.is_empty() {
            return Err(Error::query_execution(
                "SWITCHOVER TO requires a target node identifier",
            ));
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
        let check_pos = upper
            .find("CHECK ")
            .ok_or_else(|| Error::query_execution("SWITCHOVER CHECK statement malformed"))?;

        let after_check = cleaned[check_pos + 6..].trim_start();

        // Extract node identifier - may be quoted or unquoted
        let node_id = if after_check.starts_with('\'') || after_check.starts_with('"') {
            // Quoted identifier
            let quote_char = if after_check.starts_with('\'') { '\'' } else { '"' };
            let end_quote = after_check[1..]
                .find(quote_char)
                .ok_or_else(|| Error::query_execution("Unterminated quote in node identifier"))?;
            after_check[1..=end_quote].to_string()
        } else {
            // Unquoted identifier
            let end_pos = after_check
                .find(|c: char| c.is_whitespace() || c == ';')
                .unwrap_or(after_check.len());
            after_check[..end_pos].to_string()
        };

        if node_id.is_empty() {
            return Err(Error::query_execution(
                "SWITCHOVER CHECK requires a target node identifier",
            ));
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
        let for_pos = upper
            .find(" FOR ")
            .ok_or_else(|| Error::query_execution("SET NODE ALIAS requires FOR clause"))?;

        // Extract alias (between SET NODE ALIAS and FOR)
        let alias_part = cleaned[alias_start..for_pos].trim();
        let alias = if alias_part.to_uppercase() == "NULL" {
            None
        } else if alias_part.starts_with('\'') || alias_part.starts_with('"') {
            let quote_char = if alias_part.starts_with('\'') { '\'' } else { '"' };
            let end_quote = alias_part[1..]
                .find(quote_char)
                .ok_or_else(|| Error::query_execution("Unterminated quote in alias"))?;
            Some(alias_part[1..=end_quote].to_string())
        } else {
            Some(alias_part.to_string())
        };

        // Extract node identifier (after FOR)
        let after_for = cleaned[for_pos + 5..].trim();
        let node_id = if after_for.starts_with('\'') || after_for.starts_with('"') {
            let quote_char = if after_for.starts_with('\'') { '\'' } else { '"' };
            let end_quote = after_for[1..]
                .find(quote_char)
                .ok_or_else(|| Error::query_execution("Unterminated quote in node identifier"))?;
            after_for[1..=end_quote].to_string()
        } else {
            let end_pos = after_for
                .find(|c: char| c.is_whitespace() || c == ';')
                .unwrap_or(after_for.len());
            after_for[..end_pos].to_string()
        };

        if node_id.is_empty() {
            return Err(Error::query_execution(
                "SET NODE ALIAS requires a node identifier after FOR",
            ));
        }

        Ok((node_id, alias))
    }
}

/// Which ALTER SEQUENCE clause a regex match represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeqClauseKind {
    Restart,
    Increment,
    MinValue,
    MaxValue,
    Cache,
    Cycle,
    Start,
    As,
    OwnedBy,
}

/// Parsed `ALTER SEQUENCE [IF EXISTS] <name> <actions...>`.
///
/// Each `Option` is `None` when the corresponding clause was absent. The
/// doubly-wrapped fields encode a tri-state:
/// * `restart`: `None` = no RESTART; `Some(None)` = `RESTART` (to start);
///   `Some(Some(n))` = `RESTART WITH n`.
/// * `min_value`/`max_value`: `Some(Some(n))` = explicit value; `Some(None)`
///   = `NO MINVALUE`/`NO MAXVALUE` (reset to the type/sign default).
/// * `owned_by`: `Some(Some((t, c)))` = `OWNED BY t.c`; `Some(None)` =
///   `OWNED BY NONE`.
///
/// The executor applies only the present actions to the persisted definition.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct AlterSequenceAction {
    pub name: String,
    pub if_exists: bool,
    pub restart: Option<Option<i64>>,
    pub increment: Option<i64>,
    pub min_value: Option<Option<i64>>,
    pub max_value: Option<Option<i64>>,
    pub cache: Option<i64>,
    pub cycle: Option<bool>,
    pub start_value: Option<i64>,
    pub owned_by: Option<Option<(String, String)>>,
    pub data_type: Option<String>,
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
        let result = parser.parse_one("CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT NOT NULL)");
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_insert() {
        let parser = Parser::new();
        let result = parser.parse_one("INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com')");
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_error() {
        let parser = Parser::new();
        let result = parser.parse_one("SELECT FROM");
        assert!(result.is_err());
    }

    // Stage-0 PARTITION BY / PARTITION OF / ATTACH-DETACH pre-parse rewrites.
    mod partition_stage0 {
        use super::*;

        // ---- preprocess_strip_partition_by (parent, whole-clause strip) ----

        #[test]
        fn strips_single_column_range_key() {
            assert_eq!(
                Parser::preprocess_strip_partition_by("CREATE TABLE t (a int) PARTITION BY RANGE (a)"),
                "CREATE TABLE t (a int)"
            );
        }

        #[test]
        fn strips_multi_column_and_expression_keys() {
            assert_eq!(
                Parser::preprocess_strip_partition_by("CREATE TABLE t (a int, b int) PARTITION BY RANGE (a, b)"),
                "CREATE TABLE t (a int, b int)"
            );
            // Nested/expression key must balance correctly.
            assert_eq!(
                Parser::preprocess_strip_partition_by("CREATE TABLE t (a int, b int) PARTITION BY RANGE (a, (b+0))"),
                "CREATE TABLE t (a int, b int)"
            );
            assert_eq!(
                Parser::preprocess_strip_partition_by("CREATE TABLE t (a text) PARTITION BY LIST (lower(a))"),
                "CREATE TABLE t (a text)"
            );
        }

        #[test]
        fn strips_opclass_token_in_hash_key() {
            assert_eq!(
                Parser::preprocess_strip_partition_by(
                    "CREATE TABLE t (a int) PARTITION BY HASH (a part_test_int4_ops)"
                ),
                "CREATE TABLE t (a int)"
            );
        }

        #[test]
        fn preserves_trailing_with_and_tablespace_and_semicolon() {
            assert_eq!(
                Parser::preprocess_strip_partition_by(
                    "CREATE TABLE t (a int) PARTITION BY LIST (a) WITH (fillfactor = 70);"
                ),
                "CREATE TABLE t (a int) WITH (fillfactor = 70);"
            );
            assert_eq!(
                Parser::preprocess_strip_partition_by("CREATE TABLE t (a int) PARTITION BY RANGE (a) TABLESPACE ts1"),
                "CREATE TABLE t (a int) TABLESPACE ts1"
            );
        }

        #[test]
        fn tolerates_newlines_and_case() {
            assert_eq!(
                Parser::preprocess_strip_partition_by("create table t (a int)\n  partition by range (a)"),
                "create table t (a int)"
            );
        }

        #[test]
        fn does_not_touch_window_over_partition_by_in_ctas() {
            // The window PARTITION BY lives inside OVER(...) at paren depth ≥ 1
            // and must be left byte-identical.
            let sql = "CREATE TABLE t AS SELECT rank() OVER (PARTITION BY a ORDER BY b) FROM s";
            assert_eq!(Parser::preprocess_strip_partition_by(sql), sql);
        }

        #[test]
        fn passthrough_without_partition_syntax() {
            let a = "CREATE TABLE t (a int, b text)";
            assert_eq!(Parser::preprocess_strip_partition_by(a), a);
            let b = "SELECT * FROM t WHERE a = 1";
            assert_eq!(Parser::preprocess_strip_partition_by(b), b);
        }

        // ---- preprocess_partition_of (child rewrite to empty-column CREATE) ----

        #[test]
        fn rewrites_for_values_forms() {
            assert_eq!(
                Parser::preprocess_partition_of(
                    "CREATE TABLE parted_si_p_even PARTITION OF parted_si FOR VALUES IN (0)"
                ),
                "CREATE TABLE parted_si_p_even ()"
            );
            assert_eq!(
                Parser::preprocess_partition_of(
                    "CREATE TABLE c PARTITION OF p FOR VALUES FROM (0) TO (10) WITH (autovacuum_enabled = false)"
                ),
                "CREATE TABLE c ()"
            );
            assert_eq!(
                Parser::preprocess_partition_of(
                    "create table part_aa_bb partition of list_parted FOR VALUES IN ('aa', 'bb')"
                ),
                "CREATE TABLE part_aa_bb ()"
            );
        }

        #[test]
        fn rewrites_multi_column_bounds() {
            assert_eq!(
                Parser::preprocess_partition_of(
                    "create table part1 partition of range_parted for values from ('a', 1) to ('a', 10)"
                ),
                "CREATE TABLE part1 ()"
            );
        }

        #[test]
        fn rewrites_default_and_default_subpartitioned() {
            assert_eq!(
                Parser::preprocess_partition_of("create table part_default partition of list_parted default"),
                "CREATE TABLE part_default ()"
            );
            // A DEFAULT child that is itself sub-partitioned: its own
            // PARTITION BY tail is dropped by the rewrite too.
            assert_eq!(
                Parser::preprocess_partition_of(
                    "create table part_default partition of list_parted default partition by range(b)"
                ),
                "CREATE TABLE part_default ()"
            );
        }

        #[test]
        fn rewrites_schema_qualified_and_if_not_exists_and_newlines() {
            assert_eq!(
                Parser::preprocess_partition_of(
                    "CREATE TABLE stats_import.part_child_1\n  PARTITION OF stats_import.part_parent\n  FOR VALUES FROM (0) TO (10)\n  WITH (autovacuum_enabled = false);"
                ),
                "CREATE TABLE stats_import.part_child_1 ();"
            );
            assert_eq!(
                Parser::preprocess_partition_of("CREATE TABLE IF NOT EXISTS c PARTITION OF p FOR VALUES IN (1)"),
                "CREATE TABLE IF NOT EXISTS c ()"
            );
        }

        #[test]
        fn does_not_touch_inherits_empty_columns() {
            // `CREATE TABLE fail () INHERITS (partitioned2)` has empty columns
            // but no PARTITION OF — it must be returned unchanged so the
            // planner's empty-column disambiguation stays correct.
            let sql = "CREATE TABLE fail () INHERITS (partitioned2)";
            assert_eq!(Parser::preprocess_partition_of(sql), sql);
            assert!(Parser::extract_partition_of(sql).is_none());
        }

        #[test]
        fn partition_of_passthrough_without_syntax() {
            let a = "CREATE TABLE t (a int)";
            assert_eq!(Parser::preprocess_partition_of(a), a);
            let b = "INSERT INTO t VALUES (1)";
            assert_eq!(Parser::preprocess_partition_of(b), b);
        }

        // ---- extract_partition_of (captured (child, parent, bound) spec) ----

        #[test]
        fn extract_captures_names_and_verbatim_bound() {
            let spec = Parser::extract_partition_of(
                "CREATE TABLE stats_import.part_child_1 PARTITION OF stats_import.part_parent FOR VALUES FROM (0) TO (10) WITH (autovacuum_enabled = false)"
            )
            .expect("spec");
            assert!(!spec.if_not_exists);
            assert_eq!(spec.child, "stats_import.part_child_1");
            assert_eq!(spec.parent, "stats_import.part_parent");
            // Verbatim bound keeps a trailing storage `WITH (…)` (never a cut
            // point, so HASH `FOR VALUES WITH (MODULUS …)` survives intact).
            assert_eq!(
                spec.bound,
                "FOR VALUES FROM (0) TO (10) WITH (autovacuum_enabled = false)"
            );

            let def =
                Parser::extract_partition_of("create table part_def partition of range_parted default").expect("spec");
            assert_eq!(def.parent, "range_parted");
            assert_eq!(def.bound, "default");
        }

        // ---- is_partition_attach_detach_statement (accept-as-no-op) ----

        #[test]
        fn detects_attach_detach_partition() {
            assert!(Parser::is_partition_attach_detach_statement(
                "alter table parted_copytest attach partition parted_copytest_a1 for values in(1)"
            ));
            assert!(Parser::is_partition_attach_detach_statement(
                "ALTER TABLE mlparted ATTACH PARTITION mlparted1 FOR VALUES FROM (1, 2) TO (1, 10)"
            ));
            assert!(Parser::is_partition_attach_detach_statement(
                "ALTER TABLE t DETACH PARTITION c"
            ));
            assert!(Parser::is_partition_attach_detach_statement(
                "ALTER TABLE t DETACH PARTITION c CONCURRENTLY"
            ));
            assert!(Parser::is_partition_attach_detach_statement(
                "ALTER TABLE t DETACH PARTITION c FINALIZE"
            ));
            assert!(Parser::is_partition_attach_detach_statement(
                "ALTER INDEX idx ATTACH PARTITION idx_child"
            ));
        }

        #[test]
        fn ignores_non_attach_detach_alters_and_other_statements() {
            assert!(!Parser::is_partition_attach_detach_statement(
                "ALTER TABLE t ADD COLUMN c int"
            ));
            assert!(!Parser::is_partition_attach_detach_statement(
                "ALTER TABLE t RENAME TO t2"
            ));
            assert!(!Parser::is_partition_attach_detach_statement("SELECT * FROM t"));
            assert!(!Parser::is_partition_attach_detach_statement(
                "CREATE TABLE c PARTITION OF p DEFAULT"
            ));
        }

        // Strict-additive safety: the phrase inside a string literal or quoted
        // identifier must NOT trigger the no-op — a currently-passing ALTER
        // that stores the words as data (audit/DDL-logging schemas) mutates the
        // schema and must reach the real ALTER path, not be swallowed as 0 rows.
        #[test]
        fn phrase_inside_quotes_is_not_treated_as_partition_alter() {
            assert!(!Parser::is_partition_attach_detach_statement(
                "ALTER TABLE t ADD COLUMN c int DEFAULT 'attach partition'"
            ));
            assert!(!Parser::is_partition_attach_detach_statement(
                "ALTER TABLE t ADD CONSTRAINT ck CHECK (op <> 'DETACH PARTITION')"
            ));
            assert!(!Parser::is_partition_attach_detach_statement(
                "ALTER TABLE t RENAME COLUMN \"attach partition\" TO c"
            ));
        }

        // ---- parse() strip-and-reparse: only fires on failing SQL ----

        #[test]
        fn parse_accepts_child_partition_of_as_empty_columns() {
            let parser = Parser::new();
            let stmt = parser
                .parse_one("CREATE TABLE c PARTITION OF p FOR VALUES IN (1)")
                .expect("child parses");
            match stmt {
                sqlparser::ast::Statement::CreateTable(ct) => {
                    assert_eq!(ct.name.to_string(), "c");
                    assert!(
                        ct.columns.is_empty(),
                        "child rewrite yields empty columns for planner copy"
                    );
                }
                other => panic!("expected CreateTable, got {other:?}"),
            }
        }

        #[test]
        fn parse_accepts_multi_column_parent_key() {
            let parser = Parser::new();
            assert!(parser
                .parse_one("CREATE TABLE t (a int, b int) PARTITION BY RANGE (a, b)")
                .is_ok());
            // Single-column parent already parsed pre-change; still parses.
            assert!(parser
                .parse_one("CREATE TABLE t (a int) PARTITION BY RANGE (a)")
                .is_ok());
        }
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

    // ---- SEQ-3: ALTER SEQUENCE custom pre-parse path ---------------------
    mod alter_sequence {
        use super::*;

        #[test]
        fn detects_alter_sequence() {
            assert!(Parser::is_alter_sequence("ALTER SEQUENCE s RESTART"));
            assert!(Parser::is_alter_sequence("  alter sequence s INCREMENT BY 2"));
            assert!(!Parser::is_alter_sequence("ALTER TABLE t ADD COLUMN c INT"));
            assert!(!Parser::is_alter_sequence("CREATE SEQUENCE s"));
        }

        #[test]
        fn restart_bare_and_with_value() {
            let a = Parser::parse_alter_sequence("ALTER SEQUENCE s RESTART").unwrap();
            assert_eq!(a.name, "s");
            assert_eq!(a.restart, Some(None));

            let b = Parser::parse_alter_sequence("ALTER SEQUENCE s RESTART WITH 100").unwrap();
            assert_eq!(b.restart, Some(Some(100)));

            let c = Parser::parse_alter_sequence("ALTER SEQUENCE s RESTART 250").unwrap();
            assert_eq!(c.restart, Some(Some(250)));
        }

        #[test]
        fn if_exists_and_set_options_any_order() {
            // Options in an arbitrary order all parse (mirrors the
            // sqlparser-strictness the CREATE preprocess works around).
            let a = Parser::parse_alter_sequence(
                "ALTER SEQUENCE IF EXISTS s CYCLE CACHE 50 MAXVALUE 999 INCREMENT BY 7 MINVALUE 3 START WITH 3",
            )
            .unwrap();
            assert_eq!(a.name, "s");
            assert!(a.if_exists);
            assert_eq!(a.increment, Some(7));
            assert_eq!(a.min_value, Some(Some(3)));
            assert_eq!(a.max_value, Some(Some(999)));
            assert_eq!(a.cache, Some(50));
            assert_eq!(a.cycle, Some(true));
            assert_eq!(a.start_value, Some(3));
        }

        #[test]
        fn no_minvalue_no_maxvalue_no_cycle() {
            let a = Parser::parse_alter_sequence("ALTER SEQUENCE s NO MINVALUE NO MAXVALUE NO CYCLE").unwrap();
            assert_eq!(a.min_value, Some(None));
            assert_eq!(a.max_value, Some(None));
            assert_eq!(a.cycle, Some(false));
        }

        #[test]
        fn negative_values_and_increment_by_omitted_keyword() {
            let a = Parser::parse_alter_sequence("ALTER SEQUENCE s INCREMENT -1 MINVALUE -100 MAXVALUE -1").unwrap();
            assert_eq!(a.increment, Some(-1));
            assert_eq!(a.min_value, Some(Some(-100)));
            assert_eq!(a.max_value, Some(Some(-1)));
        }

        #[test]
        fn owned_by_table_col_and_none() {
            let a = Parser::parse_alter_sequence("ALTER SEQUENCE s OWNED BY orders.id").unwrap();
            assert_eq!(a.owned_by, Some(Some(("orders".to_string(), "id".to_string()))));

            let b = Parser::parse_alter_sequence("ALTER SEQUENCE s OWNED BY NONE").unwrap();
            assert_eq!(b.owned_by, Some(None));

            // Schema-qualified owner collapses public. and keeps the last two parts.
            let c = Parser::parse_alter_sequence("ALTER SEQUENCE s OWNED BY public.orders.id").unwrap();
            assert_eq!(c.owned_by, Some(Some(("orders".to_string(), "id".to_string()))));
        }

        #[test]
        fn as_type_clause() {
            let a = Parser::parse_alter_sequence("ALTER SEQUENCE s AS bigint").unwrap();
            assert_eq!(a.data_type, Some("bigint".to_string()));
            let b = Parser::parse_alter_sequence("ALTER SEQUENCE s AS smallint").unwrap();
            assert_eq!(b.data_type, Some("smallint".to_string()));
        }

        #[test]
        fn quoted_sequence_name() {
            let a = Parser::parse_alter_sequence(r#"ALTER SEQUENCE "MySeq" RESTART WITH 5"#).unwrap();
            // Quoted name keeps its case (matches normalize_ident).
            assert_eq!(a.name, "MySeq");
            assert_eq!(a.restart, Some(Some(5)));
        }

        #[test]
        fn trailing_semicolon_ok() {
            let a = Parser::parse_alter_sequence("ALTER SEQUENCE s RESTART WITH 9;").unwrap();
            assert_eq!(a.restart, Some(Some(9)));
        }

        #[test]
        fn unsupported_clause_errors() {
            // A clause we do not model must produce a clear error, never a
            // silently-accepted no-op.
            let err = Parser::parse_alter_sequence("ALTER SEQUENCE s FROBNICATE 3").unwrap_err();
            assert!(
                err.to_string().to_lowercase().contains("unsupported")
                    || err.to_string().to_lowercase().contains("malformed"),
                "{err}"
            );
        }

        #[test]
        fn missing_name_errors() {
            assert!(Parser::parse_alter_sequence("ALTER SEQUENCE").is_err());
            assert!(Parser::parse_alter_sequence("ALTER SEQUENCE IF EXISTS").is_err());
        }
    }
}
