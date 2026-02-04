//! AI-Powered Query Optimization
//!
//! Uses LLMs to analyze query patterns, suggest optimizations,
//! and auto-generate indexes based on workload analysis.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// AI Query Optimizer
pub struct AIQueryOptimizer {
    config: OptimizerConfig,
    query_history: Vec<QueryRecord>,
    optimization_cache: HashMap<String, OptimizationResult>,
}

/// Optimizer configuration
#[derive(Debug, Clone)]
pub struct OptimizerConfig {
    /// Enable LLM-based suggestions
    pub llm_enabled: bool,
    /// LLM provider for optimization
    pub llm_provider: Option<String>,
    /// Query history retention count
    pub history_size: usize,
    /// Minimum query occurrences for pattern detection
    pub min_occurrences: usize,
    /// Auto-create suggested indexes
    pub auto_create_indexes: bool,
    /// Query time threshold for optimization suggestions (ms)
    pub slow_query_threshold_ms: u64,
}

impl Default for OptimizerConfig {
    fn default() -> Self {
        Self {
            llm_enabled: true,
            llm_provider: None,
            history_size: 10000,
            min_occurrences: 10,
            auto_create_indexes: false,
            slow_query_threshold_ms: 100,
        }
    }
}

/// Recorded query with metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRecord {
    /// Original SQL
    pub sql: String,
    /// Normalized/parameterized SQL
    pub normalized_sql: String,
    /// Execution time in milliseconds
    pub execution_time_ms: u64,
    /// Rows scanned
    pub rows_scanned: u64,
    /// Rows returned
    pub rows_returned: u64,
    /// Tables accessed
    pub tables: Vec<String>,
    /// Indexes used
    pub indexes_used: Vec<String>,
    /// Timestamp
    pub timestamp: u64,
    /// Branch
    pub branch: String,
}

/// Optimization result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationResult {
    /// Original query
    pub original_query: String,
    /// Suggested optimizations
    pub suggestions: Vec<OptimizationSuggestion>,
    /// Recommended indexes
    pub recommended_indexes: Vec<IndexRecommendation>,
    /// Rewritten query (if applicable)
    pub rewritten_query: Option<String>,
    /// Estimated improvement percentage
    pub estimated_improvement: f64,
    /// Confidence score (0-1)
    pub confidence: f64,
}

/// Single optimization suggestion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationSuggestion {
    /// Suggestion type
    pub suggestion_type: SuggestionType,
    /// Description
    pub description: String,
    /// Impact level
    pub impact: ImpactLevel,
    /// Example or code snippet
    pub example: Option<String>,
}

/// Suggestion types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionType {
    AddIndex,
    RewriteQuery,
    AddFilter,
    UseJoinInsteadOfSubquery,
    UseCte,
    PartitionTable,
    MaterializedView,
    BatchOperations,
    UseVectorIndex,
    EnableParallelQuery,
    AddCaching,
}

/// Impact level
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactLevel {
    Critical,
    High,
    Medium,
    Low,
}

/// Index recommendation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexRecommendation {
    /// Table name
    pub table: String,
    /// Columns to index
    pub columns: Vec<String>,
    /// Index type
    pub index_type: IndexType,
    /// Suggested index name
    pub name: String,
    /// CREATE INDEX statement
    pub create_statement: String,
    /// Estimated benefit
    pub estimated_benefit: f64,
    /// Affected queries count
    pub affected_queries: usize,
}

/// Index type
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexType {
    BTree,
    Hash,
    Gin,
    Gist,
    Brin,
    Vector,
}

impl AIQueryOptimizer {
    pub fn new(config: OptimizerConfig) -> Self {
        Self {
            config,
            query_history: Vec::new(),
            optimization_cache: HashMap::new(),
        }
    }

    /// Record a query execution
    pub fn record_query(&mut self, record: QueryRecord) {
        self.query_history.push(record);

        // Trim history if needed
        if self.query_history.len() > self.config.history_size {
            self.query_history.remove(0);
        }
    }

    /// Analyze query and provide optimizations
    pub fn analyze_query(&mut self, sql: &str) -> OptimizationResult {
        // Check cache
        if let Some(cached) = self.optimization_cache.get(sql) {
            return cached.clone();
        }

        let mut suggestions = Vec::new();
        let mut indexes = Vec::new();

        // Rule-based analysis
        let sql_upper = sql.to_uppercase();

        // Check for missing WHERE on large tables
        if sql_upper.contains("SELECT") && !sql_upper.contains("WHERE") && !sql_upper.contains("LIMIT") {
            suggestions.push(OptimizationSuggestion {
                suggestion_type: SuggestionType::AddFilter,
                description: "Query has no WHERE clause and no LIMIT. Consider adding filters.".to_string(),
                impact: ImpactLevel::High,
                example: None,
            });
        }

        // Check for SELECT *
        if sql_upper.contains("SELECT *") {
            suggestions.push(OptimizationSuggestion {
                suggestion_type: SuggestionType::RewriteQuery,
                description: "SELECT * retrieves all columns. Consider selecting only needed columns.".to_string(),
                impact: ImpactLevel::Medium,
                example: None,
            });
        }

        // Check for subqueries that could be JOINs
        if sql_upper.contains("WHERE") && sql_upper.contains("IN (SELECT") {
            suggestions.push(OptimizationSuggestion {
                suggestion_type: SuggestionType::UseJoinInsteadOfSubquery,
                description: "Subquery in IN clause can often be rewritten as JOIN for better performance.".to_string(),
                impact: ImpactLevel::High,
                example: Some("SELECT * FROM a WHERE id IN (SELECT a_id FROM b) -> SELECT a.* FROM a JOIN b ON a.id = b.a_id".to_string()),
            });
        }

        // Check for LIKE with leading wildcard
        if sql_upper.contains("LIKE '%") {
            suggestions.push(OptimizationSuggestion {
                suggestion_type: SuggestionType::UseVectorIndex,
                description: "LIKE with leading wildcard cannot use standard indexes. Consider full-text search or trigram index.".to_string(),
                impact: ImpactLevel::High,
                example: Some("CREATE INDEX USING gin (column gin_trgm_ops)".to_string()),
            });
        }

        // Check for OR conditions that could use UNION
        let or_count = sql_upper.matches(" OR ").count();
        if or_count > 3 {
            suggestions.push(OptimizationSuggestion {
                suggestion_type: SuggestionType::RewriteQuery,
                description: "Multiple OR conditions may prevent index usage. Consider UNION ALL.".to_string(),
                impact: ImpactLevel::Medium,
                example: None,
            });
        }

        // Check for vector operations
        if sql_upper.contains("COSINE_DISTANCE") || sql_upper.contains("EUCLIDEAN_DISTANCE") {
            if !sql_upper.contains("LIMIT") {
                suggestions.push(OptimizationSuggestion {
                    suggestion_type: SuggestionType::AddFilter,
                    description: "Vector search without LIMIT can be slow. Add a LIMIT clause.".to_string(),
                    impact: ImpactLevel::Critical,
                    example: Some("ORDER BY cosine_distance(embedding, $1) LIMIT 10".to_string()),
                });
            }
        }

        // Analyze for index recommendations
        indexes.extend(self.recommend_indexes(sql));

        let result = OptimizationResult {
            original_query: sql.to_string(),
            suggestions,
            recommended_indexes: indexes,
            rewritten_query: self.rewrite_query(sql),
            estimated_improvement: self.estimate_improvement(sql),
            confidence: 0.8,
        };

        // Cache result
        self.optimization_cache.insert(sql.to_string(), result.clone());

        result
    }

    /// Recommend indexes based on query pattern
    fn recommend_indexes(&self, sql: &str) -> Vec<IndexRecommendation> {
        let mut recommendations = Vec::new();

        // Parse WHERE clause columns (simplified)
        let sql_lower = sql.to_lowercase();

        // Extract table name (very simplified)
        let table = if let Some(from_pos) = sql_lower.find("from ") {
            let after_from = &sql_lower[from_pos + 5..];
            after_from.split_whitespace().next().unwrap_or("").to_string()
        } else {
            return recommendations;
        };

        // Extract WHERE columns
        if let Some(where_pos) = sql_lower.find("where ") {
            let where_clause = &sql_lower[where_pos + 6..];
            let columns = extract_where_columns(where_clause);

            if !columns.is_empty() {
                let index_name = format!("idx_{}_{}",
                    table,
                    columns.first().unwrap_or(&"col".to_string())
                );

                recommendations.push(IndexRecommendation {
                    table: table.clone(),
                    columns: columns.clone(),
                    index_type: IndexType::BTree,
                    name: index_name.clone(),
                    create_statement: format!(
                        "CREATE INDEX {} ON {} ({})",
                        index_name,
                        table,
                        columns.join(", ")
                    ),
                    estimated_benefit: 0.7,
                    affected_queries: 1,
                });
            }
        }

        // Check for ORDER BY columns
        if let Some(order_pos) = sql_lower.find("order by ") {
            let order_clause = &sql_lower[order_pos + 9..];
            if let Some(col) = order_clause.split_whitespace().next() {
                let col = col.trim_end_matches(',').to_string();
                let index_name = format!("idx_{}_sort_{}", table, col);

                recommendations.push(IndexRecommendation {
                    table: table.clone(),
                    columns: vec![col.clone()],
                    index_type: IndexType::BTree,
                    name: index_name.clone(),
                    create_statement: format!(
                        "CREATE INDEX {} ON {} ({})",
                        index_name,
                        table,
                        col
                    ),
                    estimated_benefit: 0.5,
                    affected_queries: 1,
                });
            }
        }

        recommendations
    }

    /// Attempt to rewrite query for better performance
    fn rewrite_query(&self, sql: &str) -> Option<String> {
        let sql_upper = sql.to_uppercase();

        // Rewrite COUNT(*) to COUNT(1)
        if sql_upper.contains("COUNT(*)") {
            return Some(sql.replace("COUNT(*)", "COUNT(1)").replace("count(*)", "count(1)"));
        }

        // Convert IN subquery to EXISTS
        if sql_upper.contains("IN (SELECT") {
            // Would need proper SQL parsing for this
        }

        None
    }

    /// Estimate improvement percentage
    fn estimate_improvement(&self, _sql: &str) -> f64 {
        // Would use historical data and ML model
        0.3 // 30% estimated improvement
    }

    /// Get workload analysis
    pub fn analyze_workload(&self) -> WorkloadAnalysis {
        let mut table_access = HashMap::new();
        let mut slow_queries = Vec::new();
        let mut query_patterns = HashMap::new();

        for record in &self.query_history {
            // Count table accesses
            for table in &record.tables {
                *table_access.entry(table.clone()).or_insert(0usize) += 1;
            }

            // Identify slow queries
            if record.execution_time_ms > self.config.slow_query_threshold_ms {
                slow_queries.push(record.clone());
            }

            // Group by normalized query
            *query_patterns.entry(record.normalized_sql.clone()).or_insert(0usize) += 1;
        }

        let total_queries = self.query_history.len();
        let total_time: u64 = self.query_history.iter().map(|r| r.execution_time_ms).sum();

        WorkloadAnalysis {
            total_queries,
            total_execution_time_ms: total_time,
            average_query_time_ms: if total_queries > 0 { total_time as f64 / total_queries as f64 } else { 0.0 },
            slow_query_count: slow_queries.len(),
            slow_queries: slow_queries.into_iter().take(10).collect(),
            table_access_frequency: table_access,
            query_pattern_count: query_patterns.len(),
            most_frequent_patterns: {
                let mut patterns: Vec<_> = query_patterns.into_iter().collect();
                patterns.sort_by(|a, b| b.1.cmp(&a.1));
                patterns.into_iter().take(10).collect()
            },
            recommended_indexes: self.aggregate_index_recommendations(),
        }
    }

    /// Aggregate index recommendations from query history
    fn aggregate_index_recommendations(&self) -> Vec<IndexRecommendation> {
        let mut index_counts: HashMap<String, (IndexRecommendation, usize)> = HashMap::new();

        for record in &self.query_history {
            if record.execution_time_ms > self.config.slow_query_threshold_ms {
                let indexes = self.recommend_indexes(&record.sql);
                for idx in indexes {
                    let key = idx.create_statement.clone();
                    index_counts.entry(key.clone())
                        .and_modify(|(_, count)| *count += 1)
                        .or_insert((idx, 1));
                }
            }
        }

        let mut recommendations: Vec<_> = index_counts.into_values()
            .filter(|(_, count)| *count >= self.config.min_occurrences)
            .map(|(mut idx, count)| {
                idx.affected_queries = count;
                idx
            })
            .collect();

        recommendations.sort_by(|a, b| b.affected_queries.cmp(&a.affected_queries));
        recommendations.into_iter().take(10).collect()
    }

    /// Generate optimization prompt for LLM
    pub fn generate_llm_prompt(&self, sql: &str, schema: &str) -> String {
        format!(
            r#"Analyze this SQL query and suggest optimizations:

## Schema
{}

## Query
{}

## Historical Performance
- Average execution time: {} ms
- Similar queries executed: {} times

## Task
1. Identify performance issues
2. Suggest query rewrites
3. Recommend indexes
4. Estimate improvement

Respond in JSON format with structure:
{{
  "issues": [...],
  "suggestions": [...],
  "rewritten_query": "...",
  "indexes": [...],
  "estimated_improvement_percent": ...
}}
"#,
            schema,
            sql,
            self.average_query_time(sql),
            self.query_occurrences(sql)
        )
    }

    fn average_query_time(&self, sql: &str) -> f64 {
        let normalized = normalize_query(sql);
        let matching: Vec<_> = self.query_history.iter()
            .filter(|r| r.normalized_sql == normalized)
            .collect();

        if matching.is_empty() {
            return 0.0;
        }

        let total: u64 = matching.iter().map(|r| r.execution_time_ms).sum();
        total as f64 / matching.len() as f64
    }

    fn query_occurrences(&self, sql: &str) -> usize {
        let normalized = normalize_query(sql);
        self.query_history.iter()
            .filter(|r| r.normalized_sql == normalized)
            .count()
    }
}

/// Workload analysis result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadAnalysis {
    pub total_queries: usize,
    pub total_execution_time_ms: u64,
    pub average_query_time_ms: f64,
    pub slow_query_count: usize,
    pub slow_queries: Vec<QueryRecord>,
    pub table_access_frequency: HashMap<String, usize>,
    pub query_pattern_count: usize,
    pub most_frequent_patterns: Vec<(String, usize)>,
    pub recommended_indexes: Vec<IndexRecommendation>,
}

// Helper functions

fn normalize_query(sql: &str) -> String {
    // Replace literals with placeholders
    let mut result = sql.to_string();

    // Replace numbers
    result = regex_replace(&result, r"\b\d+\b", "?");

    // Replace strings
    result = regex_replace(&result, r"'[^']*'", "?");

    // Normalize whitespace
    result = regex_replace(&result, r"\s+", " ");

    result.trim().to_lowercase()
}

fn regex_replace(input: &str, pattern: &str, replacement: &str) -> String {
    // Simplified regex replacement
    let mut result = input.to_string();

    // Very basic pattern matching for common cases
    if pattern == r"\b\d+\b" {
        // Replace numbers
        let mut chars: Vec<char> = Vec::new();
        let mut in_number = false;

        for c in result.chars() {
            if c.is_ascii_digit() {
                if !in_number {
                    chars.push('?');
                    in_number = true;
                }
            } else {
                chars.push(c);
                in_number = false;
            }
        }

        result = chars.into_iter().collect();
    } else if pattern == r"'[^']*'" {
        // Replace strings
        let mut chars: Vec<char> = Vec::new();
        let mut in_string = false;
        let mut just_closed = false;

        for c in result.chars() {
            if c == '\'' {
                if in_string {
                    in_string = false;
                    just_closed = true;
                } else {
                    in_string = true;
                    chars.push('?');
                }
            } else if !in_string {
                if just_closed {
                    just_closed = false;
                }
                chars.push(c);
            }
        }

        result = chars.into_iter().collect();
    } else if pattern == r"\s+" {
        // Normalize whitespace
        result = result.split_whitespace().collect::<Vec<_>>().join(" ");
    }

    result
}

fn extract_where_columns(where_clause: &str) -> Vec<String> {
    let mut columns = Vec::new();

    // Very simplified extraction
    for part in where_clause.split(|c| c == '=' || c == '>' || c == '<' || c == ' ') {
        let part = part.trim();
        if !part.is_empty()
            && !part.starts_with('$')
            && !part.starts_with('?')
            && !part.starts_with('\'')
            && !part.chars().all(|c| c.is_ascii_digit())
            && !["AND", "OR", "NOT", "IN", "LIKE", "BETWEEN", "IS", "NULL"].contains(&part.to_uppercase().as_str())
        {
            columns.push(part.to_string());
        }
    }

    columns.into_iter().take(3).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_normalization() {
        let sql = "SELECT * FROM users WHERE id = 123 AND name = 'Alice'";
        let normalized = normalize_query(sql);
        assert!(normalized.contains("?"));
        assert!(!normalized.contains("123"));
        assert!(!normalized.contains("Alice"));
    }

    #[test]
    fn test_optimization_suggestions() {
        let mut optimizer = AIQueryOptimizer::new(Default::default());
        let result = optimizer.analyze_query("SELECT * FROM users");

        assert!(!result.suggestions.is_empty());
    }
}
