//! Query Validation and Sandboxing
//!
//! Provides security controls for AI-generated queries including:
//! - SQL injection prevention
//! - Query complexity limits
//! - Resource usage caps
//! - Audit logging

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

// ============================================================================
// Static Lazy Regex Patterns (compiled once, reused safely)
// ============================================================================

// SAFETY: All regex patterns below are compile-time string literals that are known to be valid.
// expect() is appropriate here because invalid patterns represent programming errors, not runtime failures.

// Query normalization patterns
#[allow(clippy::expect_used)]
static RE_LINE_COMMENTS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"--[^\n]*").expect("Invalid LINE_COMMENTS regex pattern")
});
#[allow(clippy::expect_used)]
static RE_BLOCK_COMMENTS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"/\*[\s\S]*?\*/").expect("Invalid BLOCK_COMMENTS regex pattern")
});
#[allow(clippy::expect_used)]
static RE_WHITESPACE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\s+").expect("Invalid WHITESPACE regex pattern")
});

// Table detection patterns
#[allow(clippy::expect_used)]
static RE_FROM_TABLE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bFROM\s+([a-zA-Z_][a-zA-Z0-9_]*)").expect("Invalid FROM_TABLE regex pattern")
});
#[allow(clippy::expect_used)]
static RE_JOIN_TABLE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bJOIN\s+([a-zA-Z_][a-zA-Z0-9_]*)").expect("Invalid JOIN_TABLE regex pattern")
});
#[allow(clippy::expect_used)]
static RE_INTO_TABLE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bINTO\s+([a-zA-Z_][a-zA-Z0-9_]*)").expect("Invalid INTO_TABLE regex pattern")
});
#[allow(clippy::expect_used)]
static RE_UPDATE_TABLE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bUPDATE\s+([a-zA-Z_][a-zA-Z0-9_]*)").expect("Invalid UPDATE_TABLE regex pattern")
});
#[allow(clippy::expect_used)]
static RE_TABLE_KEYWORD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bTABLE\s+([a-zA-Z_][a-zA-Z0-9_]*)").expect("Invalid TABLE_KEYWORD regex pattern")
});

// SQL injection detection patterns
#[allow(clippy::expect_used)]
static RE_INJECTION_MULTI_STMT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i);\s*(DROP|DELETE|TRUNCATE|ALTER|GRANT|REVOKE)")
        .expect("Invalid INJECTION_MULTI_STMT regex pattern")
});
#[allow(clippy::expect_used)]
static RE_INJECTION_OR_EQUALS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)'\s*OR\s+'?\d*'?\s*=\s*'?\d*'?")
        .expect("Invalid INJECTION_OR_EQUALS regex pattern")
});
#[allow(clippy::expect_used)]
static RE_INJECTION_COMMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)'\s*;\s*--").expect("Invalid INJECTION_COMMENT regex pattern")
});
#[allow(clippy::expect_used)]
static RE_INJECTION_UNION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)UNION\s+(ALL\s+)?SELECT").expect("Invalid INJECTION_UNION regex pattern")
});
#[allow(clippy::expect_used)]
static RE_INJECTION_OUTFILE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)INTO\s+OUTFILE").expect("Invalid INJECTION_OUTFILE regex pattern")
});
#[allow(clippy::expect_used)]
static RE_INJECTION_LOAD_FILE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)LOAD_FILE\s*\(").expect("Invalid INJECTION_LOAD_FILE regex pattern")
});
#[allow(clippy::expect_used)]
static RE_INJECTION_SLEEP: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)SLEEP\s*\(").expect("Invalid INJECTION_SLEEP regex pattern")
});
#[allow(clippy::expect_used)]
static RE_INJECTION_BENCHMARK: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)BENCHMARK\s*\(").expect("Invalid INJECTION_BENCHMARK regex pattern")
});
#[allow(clippy::expect_used)]
static RE_INJECTION_EXEC: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)EXEC\s*\(").expect("Invalid INJECTION_EXEC regex pattern")
});
#[allow(clippy::expect_used)]
static RE_INJECTION_HEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"0x[0-9a-fA-F]{10,}").expect("Invalid INJECTION_HEX regex pattern")
});
#[allow(clippy::expect_used)]
static RE_INJECTION_CHAR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)CHAR\s*\(\s*\d+\s*\)").expect("Invalid INJECTION_CHAR regex pattern")
});
#[allow(clippy::expect_used)]
static RE_INJECTION_CONCAT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)CONCAT\s*\([^)]*'[^)]*\)").expect("Invalid INJECTION_CONCAT regex pattern")
});

/// Sandbox configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Allowed SQL operations
    pub allowed_operations: Vec<SqlOperation>,
    /// Blocked SQL operations
    pub blocked_operations: Vec<SqlOperation>,
    /// Allowed tables (empty = all)
    pub allowed_tables: Vec<String>,
    /// Blocked tables
    pub blocked_tables: Vec<String>,
    /// Maximum query complexity score
    pub max_complexity: Option<u32>,
    /// Maximum result rows
    pub max_rows: Option<usize>,
    /// Query timeout (ms)
    pub timeout_ms: Option<u64>,
    /// Memory limit (MB)
    pub memory_limit_mb: Option<usize>,
    /// Enable audit logging
    #[serde(default)]
    pub audit_enabled: bool,
    /// Block dangerous patterns
    #[serde(default = "default_true")]
    pub block_dangerous_patterns: bool,
    /// Allow subqueries
    #[serde(default = "default_true")]
    pub allow_subqueries: bool,
    /// Allow joins
    #[serde(default = "default_true")]
    pub allow_joins: bool,
    /// Maximum join depth
    pub max_join_depth: Option<usize>,
    /// Allow aggregations
    #[serde(default = "default_true")]
    pub allow_aggregations: bool,
    /// Allow CTEs (WITH clauses)
    #[serde(default = "default_true")]
    pub allow_ctes: bool,
    /// Allow window functions
    #[serde(default = "default_true")]
    pub allow_window_functions: bool,
}

fn default_true() -> bool {
    true
}

/// SQL operations
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "UPPERCASE")]
pub enum SqlOperation {
    Select,
    Insert,
    Update,
    Delete,
    Create,
    Drop,
    Alter,
    Truncate,
    Grant,
    Revoke,
    Execute,
    Explain,
    Analyze,
    Vacuum,
    Refresh,
    Copy,
}

/// Query sandbox
pub struct QuerySandbox {
    config: SandboxConfig,
    dangerous_patterns: Vec<DangerousPattern>,
}

/// Dangerous SQL pattern (uses static regex references for efficiency)
struct DangerousPattern {
    pattern: &'static Lazy<Regex>,
    description: &'static str,
    severity: Severity,
}

/// Pattern severity
#[derive(Debug, Clone, Copy)]
enum Severity {
    Critical, // Always block
    High,     // Block unless explicitly allowed
    Medium,   // Warn and may block
    Low,      // Warn only
}

/// Sandbox validation result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxResult {
    /// Whether query is allowed
    pub allowed: bool,
    /// Validation errors
    pub errors: Vec<ValidationError>,
    /// Validation warnings
    pub warnings: Vec<String>,
    /// Sanitized query (if applicable)
    pub sanitized_query: Option<String>,
    /// Complexity score
    pub complexity_score: u32,
    /// Estimated rows
    pub estimated_rows: Option<usize>,
    /// Detected operations
    pub operations: Vec<SqlOperation>,
    /// Detected tables
    pub tables: Vec<String>,
    /// Query rewrite suggestions
    pub suggestions: Vec<String>,
}

/// Validation error
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    /// Error code
    pub code: String,
    /// Error message
    pub message: String,
    /// Position in query (if applicable)
    pub position: Option<usize>,
    /// Suggestion to fix
    pub suggestion: Option<String>,
}

impl QuerySandbox {
    /// Create new query sandbox
    pub fn new(config: SandboxConfig) -> Self {
        let dangerous_patterns = Self::default_dangerous_patterns();

        Self {
            config,
            dangerous_patterns,
        }
    }

    /// Create sandbox with default security settings
    pub fn secure() -> Self {
        Self::new(SandboxConfig {
            allowed_operations: vec![
                SqlOperation::Select,
                SqlOperation::Insert,
                SqlOperation::Update,
                SqlOperation::Delete,
                SqlOperation::Explain,
            ],
            blocked_operations: vec![
                SqlOperation::Drop,
                SqlOperation::Truncate,
                SqlOperation::Grant,
                SqlOperation::Revoke,
                SqlOperation::Execute,
            ],
            allowed_tables: Vec::new(),
            blocked_tables: vec![
                "pg_".to_string(),
                "information_schema".to_string(),
                "_internal".to_string(),
            ],
            max_complexity: Some(100),
            max_rows: Some(10000),
            timeout_ms: Some(30000),
            memory_limit_mb: Some(256),
            audit_enabled: true,
            block_dangerous_patterns: true,
            allow_subqueries: true,
            allow_joins: true,
            max_join_depth: Some(5),
            allow_aggregations: true,
            allow_ctes: true,
            allow_window_functions: true,
        })
    }

    /// Create permissive sandbox (less restrictions)
    pub fn permissive() -> Self {
        Self::new(SandboxConfig {
            allowed_operations: vec![
                SqlOperation::Select,
                SqlOperation::Insert,
                SqlOperation::Update,
                SqlOperation::Delete,
                SqlOperation::Create,
                SqlOperation::Alter,
                SqlOperation::Explain,
                SqlOperation::Analyze,
            ],
            blocked_operations: vec![
                SqlOperation::Drop,
                SqlOperation::Truncate,
                SqlOperation::Grant,
                SqlOperation::Revoke,
            ],
            allowed_tables: Vec::new(),
            blocked_tables: Vec::new(),
            max_complexity: Some(500),
            max_rows: Some(100000),
            timeout_ms: Some(60000),
            memory_limit_mb: Some(1024),
            audit_enabled: false,
            block_dangerous_patterns: true,
            allow_subqueries: true,
            allow_joins: true,
            max_join_depth: None,
            allow_aggregations: true,
            allow_ctes: true,
            allow_window_functions: true,
        })
    }

    /// Validate query
    pub fn validate(&self, query: &str) -> SandboxResult {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let mut suggestions = Vec::new();

        // Normalize query
        let normalized = self.normalize_query(query);

        // Detect operations
        let operations = self.detect_operations(&normalized);

        // Detect tables
        let tables = self.detect_tables(&normalized);

        // Check blocked operations
        for op in &operations {
            if self.config.blocked_operations.contains(op) {
                errors.push(ValidationError {
                    code: "BLOCKED_OPERATION".to_string(),
                    message: format!("{:?} operation is not allowed", op),
                    position: None,
                    suggestion: None,
                });
            }

            if !self.config.allowed_operations.is_empty() &&
               !self.config.allowed_operations.contains(op) {
                errors.push(ValidationError {
                    code: "OPERATION_NOT_ALLOWED".to_string(),
                    message: format!("{:?} operation is not in allowed list", op),
                    position: None,
                    suggestion: None,
                });
            }
        }

        // Check blocked tables
        for table in &tables {
            let table_lower = table.to_lowercase();
            for blocked in &self.config.blocked_tables {
                if table_lower.starts_with(&blocked.to_lowercase()) {
                    errors.push(ValidationError {
                        code: "BLOCKED_TABLE".to_string(),
                        message: format!("Access to table '{}' is not allowed", table),
                        position: None,
                        suggestion: None,
                    });
                }
            }

            if !self.config.allowed_tables.is_empty() {
                let allowed = self.config.allowed_tables.iter()
                    .any(|t| t.eq_ignore_ascii_case(&table_lower));
                if !allowed {
                    errors.push(ValidationError {
                        code: "TABLE_NOT_ALLOWED".to_string(),
                        message: format!("Table '{}' is not in allowed list", table),
                        position: None,
                        suggestion: None,
                    });
                }
            }
        }

        // Check dangerous patterns
        if self.config.block_dangerous_patterns {
            for dp in &self.dangerous_patterns {
                if dp.pattern.is_match(&normalized) {
                    match dp.severity {
                        Severity::Critical | Severity::High => {
                            errors.push(ValidationError {
                                code: "DANGEROUS_PATTERN".to_string(),
                                message: dp.description.to_string(),
                                position: None,
                                suggestion: Some("Remove or escape the dangerous pattern".to_string()),
                            });
                        }
                        Severity::Medium => {
                            warnings.push(format!("Potential issue: {}", dp.description));
                        }
                        Severity::Low => {
                            warnings.push(format!("Note: {}", dp.description));
                        }
                    }
                }
            }
        }

        // Check query structure
        if !self.config.allow_subqueries && self.has_subquery(&normalized) {
            errors.push(ValidationError {
                code: "SUBQUERY_NOT_ALLOWED".to_string(),
                message: "Subqueries are not allowed".to_string(),
                position: None,
                suggestion: Some("Rewrite using JOINs or CTEs".to_string()),
            });
        }

        if !self.config.allow_joins && self.has_join(&normalized) {
            errors.push(ValidationError {
                code: "JOIN_NOT_ALLOWED".to_string(),
                message: "JOINs are not allowed".to_string(),
                position: None,
                suggestion: Some("Query tables separately".to_string()),
            });
        }

        if !self.config.allow_ctes && self.has_cte(&normalized) {
            errors.push(ValidationError {
                code: "CTE_NOT_ALLOWED".to_string(),
                message: "CTEs (WITH clauses) are not allowed".to_string(),
                position: None,
                suggestion: Some("Rewrite using subqueries".to_string()),
            });
        }

        if !self.config.allow_window_functions && self.has_window_function(&normalized) {
            errors.push(ValidationError {
                code: "WINDOW_FUNCTION_NOT_ALLOWED".to_string(),
                message: "Window functions are not allowed".to_string(),
                position: None,
                suggestion: None,
            });
        }

        // Calculate complexity
        let complexity_score = self.calculate_complexity(&normalized, &operations, &tables);

        if let Some(max) = self.config.max_complexity {
            if complexity_score > max {
                errors.push(ValidationError {
                    code: "COMPLEXITY_EXCEEDED".to_string(),
                    message: format!("Query complexity ({}) exceeds limit ({})", complexity_score, max),
                    position: None,
                    suggestion: Some("Simplify the query or break it into smaller queries".to_string()),
                });
            }
        }

        // Check join depth
        if let Some(max_depth) = self.config.max_join_depth {
            let join_depth = self.count_join_depth(&normalized);
            if join_depth > max_depth {
                errors.push(ValidationError {
                    code: "JOIN_DEPTH_EXCEEDED".to_string(),
                    message: format!("Join depth ({}) exceeds limit ({})", join_depth, max_depth),
                    position: None,
                    suggestion: Some("Reduce the number of joined tables".to_string()),
                });
            }
        }

        // Generate suggestions
        if operations.contains(&SqlOperation::Select) && !normalized.to_uppercase().contains("LIMIT") {
            suggestions.push("Consider adding LIMIT to prevent large result sets".to_string());
        }

        if normalized.to_uppercase().contains("SELECT *") {
            suggestions.push("Consider selecting specific columns instead of *".to_string());
        }

        let allowed = errors.is_empty();
        let sanitized_query = if allowed {
            Some(normalized)
        } else {
            None
        };

        SandboxResult {
            allowed,
            errors,
            warnings,
            sanitized_query,
            complexity_score,
            estimated_rows: None,
            operations,
            tables,
            suggestions,
        }
    }

    /// Sanitize query (escape dangerous characters)
    pub fn sanitize(&self, query: &str) -> String {
        let mut sanitized = query.to_string();

        // Remove null bytes
        sanitized = sanitized.replace('\0', "");

        // Escape single quotes (basic SQL injection prevention)
        // Note: This is simplified - production should use parameterized queries
        sanitized = sanitized.replace("''", "''''");

        sanitized
    }

    /// Normalize query for analysis
    fn normalize_query(&self, query: &str) -> String {
        // Remove comments using static patterns
        let without_line_comments = RE_LINE_COMMENTS.replace_all(query, " ");
        let without_block_comments = RE_BLOCK_COMMENTS.replace_all(&without_line_comments, " ");

        // Normalize whitespace
        let normalized = RE_WHITESPACE.replace_all(&without_block_comments, " ");

        normalized.trim().to_string()
    }

    /// Detect SQL operations in query
    fn detect_operations(&self, query: &str) -> Vec<SqlOperation> {
        let upper = query.to_uppercase();
        let mut ops = Vec::new();

        if upper.starts_with("SELECT") || upper.contains(" SELECT ") {
            ops.push(SqlOperation::Select);
        }
        if upper.starts_with("INSERT") {
            ops.push(SqlOperation::Insert);
        }
        if upper.starts_with("UPDATE") {
            ops.push(SqlOperation::Update);
        }
        if upper.starts_with("DELETE") {
            ops.push(SqlOperation::Delete);
        }
        if upper.starts_with("CREATE") {
            ops.push(SqlOperation::Create);
        }
        if upper.starts_with("DROP") {
            ops.push(SqlOperation::Drop);
        }
        if upper.starts_with("ALTER") {
            ops.push(SqlOperation::Alter);
        }
        if upper.starts_with("TRUNCATE") {
            ops.push(SqlOperation::Truncate);
        }
        if upper.starts_with("GRANT") {
            ops.push(SqlOperation::Grant);
        }
        if upper.starts_with("REVOKE") {
            ops.push(SqlOperation::Revoke);
        }
        if upper.starts_with("EXPLAIN") {
            ops.push(SqlOperation::Explain);
        }
        if upper.starts_with("ANALYZE") || upper.contains("ANALYZE") {
            ops.push(SqlOperation::Analyze);
        }
        if upper.starts_with("REFRESH") {
            ops.push(SqlOperation::Refresh);
        }

        ops
    }

    /// Detect tables in query
    fn detect_tables(&self, query: &str) -> Vec<String> {
        let mut tables = HashSet::new();

        // Use static lazy patterns for table detection
        let patterns: &[&Lazy<Regex>] = &[
            &RE_FROM_TABLE,
            &RE_JOIN_TABLE,
            &RE_INTO_TABLE,
            &RE_UPDATE_TABLE,
            &RE_TABLE_KEYWORD,
        ];

        for pattern in patterns {
            for cap in pattern.captures_iter(query) {
                if let Some(table) = cap.get(1) {
                    tables.insert(table.as_str().to_string());
                }
            }
        }

        tables.into_iter().collect()
    }

    /// Check if query has subqueries
    fn has_subquery(&self, query: &str) -> bool {
        let upper = query.to_uppercase();
        let open_parens = upper.matches('(').count();
        let select_count = upper.matches("SELECT").count();

        // If there's a SELECT inside parentheses, it's likely a subquery
        select_count > 1 && open_parens > 0
    }

    /// Check if query has JOINs
    fn has_join(&self, query: &str) -> bool {
        let upper = query.to_uppercase();
        upper.contains(" JOIN ")
    }

    /// Check if query has CTE
    fn has_cte(&self, query: &str) -> bool {
        let upper = query.to_uppercase();
        upper.trim().starts_with("WITH ")
    }

    /// Check if query has window functions
    fn has_window_function(&self, query: &str) -> bool {
        let upper = query.to_uppercase();
        upper.contains(" OVER ") || upper.contains(" OVER(")
    }

    /// Count join depth
    fn count_join_depth(&self, query: &str) -> usize {
        let upper = query.to_uppercase();
        upper.matches(" JOIN ").count()
    }

    /// Calculate query complexity score
    fn calculate_complexity(&self, query: &str, operations: &[SqlOperation], tables: &[String]) -> u32 {
        let mut score = 0u32;

        // Base score per operation
        score += (operations.len() * 5) as u32;

        // Tables accessed
        score += (tables.len() * 10) as u32;

        // Joins
        score += (self.count_join_depth(query) * 15) as u32;

        // Subqueries
        if self.has_subquery(query) {
            score += 20;
        }

        // CTEs
        if self.has_cte(query) {
            score += 15;
        }

        // Window functions
        if self.has_window_function(query) {
            score += 10;
        }

        // Aggregations
        let upper = query.to_uppercase();
        for agg in &["COUNT(", "SUM(", "AVG(", "MIN(", "MAX(", "GROUP BY"] {
            if upper.contains(agg) {
                score += 5;
            }
        }

        // DISTINCT
        if upper.contains("DISTINCT") {
            score += 5;
        }

        // ORDER BY
        if upper.contains("ORDER BY") {
            score += 3;
        }

        // UNION
        score += (upper.matches(" UNION ").count() * 15) as u32;

        score
    }

    /// Default dangerous patterns (uses static lazy-compiled regex patterns)
    fn default_dangerous_patterns() -> Vec<DangerousPattern> {
        vec![
            DangerousPattern {
                pattern: &RE_INJECTION_MULTI_STMT,
                description: "Multiple statements with dangerous operations",
                severity: Severity::Critical,
            },
            DangerousPattern {
                pattern: &RE_INJECTION_OR_EQUALS,
                description: "Potential SQL injection: OR condition",
                severity: Severity::Critical,
            },
            DangerousPattern {
                pattern: &RE_INJECTION_COMMENT,
                description: "Potential SQL injection: comment termination",
                severity: Severity::Critical,
            },
            DangerousPattern {
                pattern: &RE_INJECTION_UNION,
                description: "UNION-based query (potential injection)",
                severity: Severity::High,
            },
            DangerousPattern {
                pattern: &RE_INJECTION_OUTFILE,
                description: "File write attempt",
                severity: Severity::Critical,
            },
            DangerousPattern {
                pattern: &RE_INJECTION_LOAD_FILE,
                description: "File read attempt",
                severity: Severity::Critical,
            },
            DangerousPattern {
                pattern: &RE_INJECTION_SLEEP,
                description: "Time-based blind SQL injection",
                severity: Severity::High,
            },
            DangerousPattern {
                pattern: &RE_INJECTION_BENCHMARK,
                description: "Time-based blind SQL injection",
                severity: Severity::High,
            },
            DangerousPattern {
                pattern: &RE_INJECTION_EXEC,
                description: "Dynamic SQL execution",
                severity: Severity::Critical,
            },
            DangerousPattern {
                pattern: &RE_INJECTION_HEX,
                description: "Hex-encoded payload",
                severity: Severity::Medium,
            },
            DangerousPattern {
                pattern: &RE_INJECTION_CHAR,
                description: "Character code obfuscation",
                severity: Severity::Medium,
            },
            DangerousPattern {
                pattern: &RE_INJECTION_CONCAT,
                description: "String concatenation (potential injection)",
                severity: Severity::Low,
            },
        ]
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            allowed_operations: vec![
                SqlOperation::Select,
                SqlOperation::Insert,
                SqlOperation::Update,
                SqlOperation::Delete,
            ],
            blocked_operations: vec![
                SqlOperation::Drop,
                SqlOperation::Truncate,
                SqlOperation::Grant,
                SqlOperation::Revoke,
            ],
            allowed_tables: Vec::new(),
            blocked_tables: Vec::new(),
            max_complexity: Some(100),
            max_rows: Some(10000),
            timeout_ms: Some(30000),
            memory_limit_mb: Some(256),
            audit_enabled: false,
            block_dangerous_patterns: true,
            allow_subqueries: true,
            allow_joins: true,
            max_join_depth: Some(5),
            allow_aggregations: true,
            allow_ctes: true,
            allow_window_functions: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secure_sandbox_allows_select() {
        let sandbox = QuerySandbox::secure();
        let result = sandbox.validate("SELECT * FROM users WHERE id = 1");
        assert!(result.allowed);
    }

    #[test]
    fn test_secure_sandbox_blocks_drop() {
        let sandbox = QuerySandbox::secure();
        let result = sandbox.validate("DROP TABLE users");
        assert!(!result.allowed);
    }

    #[test]
    fn test_sql_injection_detection() {
        let sandbox = QuerySandbox::secure();
        let result = sandbox.validate("SELECT * FROM users WHERE name = '' OR '1'='1'");
        assert!(!result.allowed);
    }

    #[test]
    fn test_blocked_table() {
        let sandbox = QuerySandbox::secure();
        let result = sandbox.validate("SELECT * FROM pg_tables");
        assert!(!result.allowed);
    }

    #[test]
    fn test_complexity_calculation() {
        let sandbox = QuerySandbox::secure();
        let result = sandbox.validate(
            "SELECT a.*, b.*, c.* FROM a JOIN b ON a.id = b.a_id JOIN c ON b.id = c.b_id WHERE a.x > 10 GROUP BY a.id ORDER BY a.created_at"
        );
        assert!(result.complexity_score > 50);
    }
}
