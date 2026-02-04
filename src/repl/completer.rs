//! Auto-completion for SQL keywords and table names

use rustyline::completion::{Completer, Pair};
use rustyline::Context;
use rustyline::Result as RustyResult;

/// SQL keyword completer
pub struct SqlCompleter {
    keywords: Vec<String>,
    table_names: Vec<String>,
}

impl SqlCompleter {
    /// Create a new SQL completer
    pub fn new() -> Self {
        Self {
            keywords: Self::default_keywords(),
            table_names: Vec::new(),
        }
    }

    /// Update table names for completion
    pub fn set_table_names(&mut self, tables: Vec<String>) {
        self.table_names = tables;
    }

    /// Default SQL keywords
    fn default_keywords() -> Vec<String> {
        vec![
            // DDL
            "CREATE", "DROP", "ALTER", "TRUNCATE",
            "TABLE", "INDEX", "VIEW", "DATABASE", "SCHEMA",

            // DML
            "SELECT", "INSERT", "UPDATE", "DELETE",
            "FROM", "WHERE", "JOIN", "ON", "USING",
            "GROUP BY", "HAVING", "ORDER BY", "LIMIT", "OFFSET",
            "INNER JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN",
            "CROSS JOIN",

            // Clauses
            "AND", "OR", "NOT", "IN", "BETWEEN", "LIKE", "IS",
            "NULL", "TRUE", "FALSE",
            "AS", "DISTINCT", "ALL", "ANY", "SOME", "EXISTS",

            // Functions
            "COUNT", "SUM", "AVG", "MIN", "MAX",
            "UPPER", "LOWER", "LENGTH", "SUBSTR", "TRIM",
            "NOW", "CURRENT_DATE", "CURRENT_TIME", "CURRENT_TIMESTAMP",

            // Data Types
            "INT", "INTEGER", "BIGINT", "SMALLINT",
            "FLOAT", "DOUBLE", "NUMERIC", "DECIMAL",
            "TEXT", "VARCHAR", "CHAR",
            "BOOLEAN", "BOOL",
            "DATE", "TIME", "TIMESTAMP", "TIMESTAMPTZ",
            "BYTEA", "JSONB",

            // Constraints
            "PRIMARY KEY", "FOREIGN KEY", "UNIQUE", "NOT NULL",
            "CHECK", "DEFAULT", "REFERENCES",

            // Transaction
            "BEGIN", "COMMIT", "ROLLBACK", "SAVEPOINT",
            "START TRANSACTION",

            // Other
            "SET", "SHOW", "EXPLAIN", "ANALYZE",
            "UNION", "INTERSECT", "EXCEPT",
            "WITH", "CASE", "WHEN", "THEN", "ELSE", "END",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// Find completions for the given word
    fn find_completions(&self, word: &str) -> Vec<String> {
        let word_upper = word.to_uppercase();
        let mut completions = Vec::new();

        // Match SQL keywords
        for keyword in &self.keywords {
            if keyword.starts_with(&word_upper) {
                completions.push(keyword.clone());
            }
        }

        // Match table names (case-insensitive)
        for table in &self.table_names {
            if table.to_uppercase().starts_with(&word_upper) {
                completions.push(table.clone());
            }
        }

        completions.sort();
        completions.dedup();
        completions
    }
}

impl Completer for SqlCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> RustyResult<(usize, Vec<Pair>)> {
        // Find the word to complete
        let start = line[..pos]
            .rfind(|c: char| c.is_whitespace() || "(),;".contains(c))
            .map(|i| i + 1)
            .unwrap_or(0);

        let word = &line[start..pos];

        if word.is_empty() {
            return Ok((pos, Vec::new()));
        }

        let completions = self.find_completions(word);

        let pairs = completions
            .into_iter()
            .map(|s| Pair {
                display: s.clone(),
                replacement: s,
            })
            .collect();

        Ok((start, pairs))
    }
}

impl Default for SqlCompleter {
    fn default() -> Self {
        Self::new()
    }
}
