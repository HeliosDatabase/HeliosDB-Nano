//! Natural Language to SQL Query Engine
//!
//! Converts natural language questions into SQL queries using LLM providers.
//! Features:
//! - Schema-aware SQL generation
//! - Query validation and sandboxing
//! - Result explanation
//! - Query history and caching
//! - Multi-dialect support

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use super::providers::{
    ChatMessage, LlmProvider, LlmRequest, MessageRole, ProviderError, ProviderResult,
};
use super::sandbox::{QuerySandbox, SandboxConfig, SandboxResult};

// ============================================================================
// Static Lazy Regex Patterns
// ============================================================================

// SAFETY: All regex patterns below are compile-time string literals that are known to be valid.
// expect() is appropriate here because invalid patterns represent programming errors, not runtime failures.

/// Pattern to extract SQL from LLM response
#[allow(clippy::expect_used)]
static RE_SQL_BLOCK: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"```(?:sql)?\s*([\s\S]*?)```").expect("Invalid SQL_BLOCK regex")
});

/// Pattern to match SELECT statement
#[allow(clippy::expect_used)]
static RE_SELECT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?is)(SELECT\s+[\s\S]+?)(?:;|$)").expect("Invalid SELECT regex")
});

/// Pattern to detect aggregation keywords
#[allow(clippy::expect_used)]
static RE_AGGREGATION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(count|sum|avg|average|total|minimum|maximum|min|max|group)\b")
        .expect("Invalid AGGREGATION regex")
});

/// Pattern to detect comparison keywords
#[allow(clippy::expect_used)]
static RE_COMPARISON: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(greater|less|more|fewer|equal|between|above|below|at least|at most)\b")
        .expect("Invalid COMPARISON regex")
});

/// Pattern to detect time-related keywords
#[allow(clippy::expect_used)]
static RE_TIME: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(today|yesterday|last|this|next|week|month|year|recent|latest|oldest)\b")
        .expect("Invalid TIME regex")
});

/// Pattern to detect sorting keywords
#[allow(clippy::expect_used)]
static RE_SORTING: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(top|bottom|first|last|highest|lowest|best|worst|most|least|order)\b")
        .expect("Invalid SORTING regex")
});

/// Pattern to detect limit keywords
#[allow(clippy::expect_used)]
static RE_LIMIT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(top\s+\d+|\d+\s+(?:results?|rows?|records?)|limit|first\s+\d+)\b")
        .expect("Invalid LIMIT regex")
});

// ============================================================================
// Configuration
// ============================================================================

/// NL Query engine configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NlQueryConfig {
    /// SQL dialect to generate
    #[serde(default = "default_dialect")]
    pub dialect: SqlDialect,
    /// Maximum results to return
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    /// Whether to validate generated SQL
    #[serde(default = "default_true")]
    pub validate_sql: bool,
    /// Sandbox configuration for validation
    pub sandbox_config: Option<SandboxConfig>,
    /// Whether to explain results
    #[serde(default = "default_true")]
    pub explain_results: bool,
    /// Cache TTL in seconds
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_secs: u64,
    /// Maximum cache entries
    #[serde(default = "default_cache_size")]
    pub max_cache_entries: usize,
    /// LLM temperature for SQL generation
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    /// Model to use (overrides provider default)
    pub model: Option<String>,
    /// Custom system prompt
    pub system_prompt: Option<String>,
    /// Enable query suggestions
    #[serde(default = "default_true")]
    pub enable_suggestions: bool,
    /// Enable auto-correction on syntax errors
    #[serde(default = "default_true")]
    pub auto_correct: bool,
    /// Maximum correction attempts
    #[serde(default = "default_max_corrections")]
    pub max_correction_attempts: usize,
}

fn default_dialect() -> SqlDialect {
    SqlDialect::PostgreSQL
}

fn default_max_results() -> usize {
    1000
}

fn default_true() -> bool {
    true
}

fn default_cache_ttl() -> u64 {
    300
}

fn default_cache_size() -> usize {
    1000
}

fn default_temperature() -> f32 {
    0.1
}

fn default_max_corrections() -> usize {
    2
}

impl Default for NlQueryConfig {
    fn default() -> Self {
        Self {
            dialect: SqlDialect::PostgreSQL,
            max_results: 1000,
            validate_sql: true,
            sandbox_config: None,
            explain_results: true,
            cache_ttl_secs: 300,
            max_cache_entries: 1000,
            temperature: 0.1,
            model: None,
            system_prompt: None,
            enable_suggestions: true,
            auto_correct: true,
            max_correction_attempts: 2,
        }
    }
}

/// SQL dialect
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SqlDialect {
    PostgreSQL,
    MySQL,
    SQLite,
    MSSQL,
    Oracle,
    HeliosDB,
}

impl SqlDialect {
    /// Get dialect-specific features description
    pub fn features_hint(&self) -> &'static str {
        match self {
            SqlDialect::PostgreSQL => {
                "PostgreSQL: Use double quotes for identifiers, supports ILIKE, array types, JSONB, CTEs, window functions"
            }
            SqlDialect::MySQL => {
                "MySQL: Use backticks for identifiers, LIMIT before OFFSET, no boolean type (use TINYINT)"
            }
            SqlDialect::SQLite => {
                "SQLite: Limited types, no RIGHT JOIN, use || for string concat, LIMIT before OFFSET"
            }
            SqlDialect::MSSQL => {
                "T-SQL: TOP instead of LIMIT, GETDATE() for now, square brackets for identifiers"
            }
            SqlDialect::Oracle => {
                "Oracle: ROWNUM for limiting, NVL instead of COALESCE, SYSDATE for now"
            }
            SqlDialect::HeliosDB => {
                "HeliosDB: PostgreSQL-compatible with vector support, VECTOR type, cosine_distance function"
            }
        }
    }
}

// ============================================================================
// Request and Response Types
// ============================================================================

/// Natural language query request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NlQueryRequest {
    /// Natural language question
    pub question: String,
    /// Schema context (tables/columns available)
    pub schema: Option<SchemaContext>,
    /// Conversation context (for follow-up questions)
    pub context: Option<ConversationContext>,
    /// Override configuration
    pub config: Option<NlQueryConfig>,
    /// User ID for audit
    pub user_id: Option<String>,
    /// Tenant ID for multi-tenancy
    pub tenant_id: Option<String>,
    /// Request metadata
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

/// Schema context for query generation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaContext {
    /// Available tables
    pub tables: Vec<TableSchema>,
    /// Database name
    pub database: Option<String>,
    /// Schema name
    pub schema: Option<String>,
    /// Additional context hints
    pub hints: Option<Vec<String>>,
}

/// Table schema information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchema {
    /// Table name
    pub name: String,
    /// Table description
    pub description: Option<String>,
    /// Columns
    pub columns: Vec<ColumnSchema>,
    /// Primary key columns
    pub primary_key: Option<Vec<String>>,
    /// Foreign keys
    pub foreign_keys: Option<Vec<ForeignKey>>,
    /// Indexes
    pub indexes: Option<Vec<IndexInfo>>,
    /// Sample data (for context)
    pub sample_values: Option<HashMap<String, Vec<String>>>,
    /// Row count estimate
    pub row_count: Option<usize>,
}

/// Column schema information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnSchema {
    /// Column name
    pub name: String,
    /// Data type
    pub data_type: String,
    /// Whether nullable
    pub nullable: bool,
    /// Description/comment
    pub description: Option<String>,
    /// Default value
    pub default_value: Option<String>,
    /// Is primary key
    #[serde(default)]
    pub is_primary_key: bool,
    /// Is unique
    #[serde(default)]
    pub is_unique: bool,
    /// Enum values (if applicable)
    pub enum_values: Option<Vec<String>>,
}

/// Foreign key information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKey {
    /// Foreign key name
    pub name: Option<String>,
    /// Local columns
    pub columns: Vec<String>,
    /// Referenced table
    pub ref_table: String,
    /// Referenced columns
    pub ref_columns: Vec<String>,
}

/// Index information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexInfo {
    /// Index name
    pub name: String,
    /// Indexed columns
    pub columns: Vec<String>,
    /// Is unique
    pub unique: bool,
    /// Index type
    pub index_type: Option<String>,
}

/// Conversation context for follow-up questions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationContext {
    /// Previous questions and SQL
    pub history: Vec<QueryHistoryEntry>,
    /// Entities mentioned in conversation
    pub entities: Option<HashMap<String, String>>,
    /// Session ID
    pub session_id: Option<String>,
}

/// Query history entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryHistoryEntry {
    /// Original question
    pub question: String,
    /// Generated SQL
    pub sql: String,
    /// Whether it was successful
    pub success: bool,
    /// Timestamp
    pub timestamp: Option<i64>,
}

/// Natural language query response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NlQueryResponse {
    /// Generated SQL query
    pub sql: String,
    /// Query explanation
    pub explanation: Option<String>,
    /// Confidence score (0.0 - 1.0)
    pub confidence: f32,
    /// Query analysis
    pub analysis: QueryAnalysis,
    /// Validation result
    pub validation: Option<SandboxResult>,
    /// Suggested alternative queries
    pub suggestions: Option<Vec<QuerySuggestion>>,
    /// Warnings
    pub warnings: Vec<String>,
    /// Token usage
    pub usage: Option<TokenUsage>,
    /// Whether the query was cached
    pub cached: bool,
    /// Processing time in ms
    pub processing_time_ms: u64,
}

/// Query analysis result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryAnalysis {
    /// Detected intent
    pub intent: QueryIntent,
    /// Tables involved
    pub tables: Vec<String>,
    /// Columns referenced
    pub columns: Vec<String>,
    /// Filters/conditions detected
    pub filters: Vec<DetectedFilter>,
    /// Aggregations detected
    pub aggregations: Vec<String>,
    /// Sorting detected
    pub sorting: Option<SortingInfo>,
    /// Limit detected
    pub limit: Option<usize>,
    /// Time range detected
    pub time_range: Option<TimeRange>,
    /// Entities extracted
    pub entities: Vec<ExtractedEntity>,
}

/// Query intent type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryIntent {
    /// Simple data retrieval
    Select,
    /// Aggregation/summary
    Aggregate,
    /// Comparison
    Compare,
    /// Ranking/top-N
    Rank,
    /// Time-series analysis
    TimeSeries,
    /// Search/filter
    Search,
    /// Count/existence
    Count,
    /// Join/relationship
    Join,
    /// Unknown
    Unknown,
}

/// Detected filter/condition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedFilter {
    /// Column name
    pub column: String,
    /// Operator (=, >, <, LIKE, etc.)
    pub operator: String,
    /// Filter value
    pub value: String,
}

/// Sorting information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SortingInfo {
    /// Column to sort by
    pub column: String,
    /// Direction
    pub direction: SortDirection,
}

/// Sort direction
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortDirection {
    Asc,
    Desc,
}

/// Time range
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeRange {
    /// Start (if specified)
    pub start: Option<String>,
    /// End (if specified)
    pub end: Option<String>,
    /// Relative description
    pub description: String,
}

/// Extracted entity
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedEntity {
    /// Entity text
    pub text: String,
    /// Entity type
    pub entity_type: String,
    /// Normalized value
    pub normalized: Option<String>,
}

/// Query suggestion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuerySuggestion {
    /// Suggestion text
    pub text: String,
    /// Generated SQL
    pub sql: Option<String>,
    /// Why this is suggested
    pub reason: String,
}

/// Token usage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
}

// ============================================================================
// Query Cache
// ============================================================================

/// Cached query entry
struct CachedQuery {
    response: NlQueryResponse,
    created: Instant,
    ttl: Duration,
}

/// Query cache
struct QueryCache {
    entries: HashMap<String, CachedQuery>,
    max_entries: usize,
}

impl QueryCache {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
        }
    }

    fn get(&mut self, key: &str) -> Option<NlQueryResponse> {
        if let Some(entry) = self.entries.get(key) {
            if entry.created.elapsed() < entry.ttl {
                let mut response = entry.response.clone();
                response.cached = true;
                return Some(response);
            }
            // Expired, remove it
            self.entries.remove(key);
        }
        None
    }

    fn insert(&mut self, key: String, response: NlQueryResponse, ttl: Duration) {
        // Evict old entries if at capacity
        if self.entries.len() >= self.max_entries {
            self.evict_oldest();
        }

        self.entries.insert(
            key,
            CachedQuery {
                response,
                created: Instant::now(),
                ttl,
            },
        );
    }

    fn evict_oldest(&mut self) {
        let oldest = self
            .entries
            .iter()
            .min_by_key(|(_, v)| v.created)
            .map(|(k, _)| k.clone());

        if let Some(key) = oldest {
            self.entries.remove(&key);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

// ============================================================================
// NL Query Engine
// ============================================================================

/// Natural Language Query Engine
pub struct NlQueryEngine {
    /// LLM provider
    llm: Arc<dyn LlmProvider>,
    /// Configuration
    config: NlQueryConfig,
    /// Query sandbox
    sandbox: QuerySandbox,
    /// Query cache
    cache: RwLock<QueryCache>,
}

impl NlQueryEngine {
    /// Create new NL Query engine
    pub fn new(llm: Arc<dyn LlmProvider>) -> Self {
        let config = NlQueryConfig::default();
        let sandbox_config = config.sandbox_config.clone().unwrap_or_default();

        Self {
            llm,
            config: config.clone(),
            sandbox: QuerySandbox::new(sandbox_config),
            cache: RwLock::new(QueryCache::new(config.max_cache_entries)),
        }
    }

    /// Create with custom configuration
    pub fn with_config(llm: Arc<dyn LlmProvider>, config: NlQueryConfig) -> Self {
        let sandbox_config = config.sandbox_config.clone().unwrap_or_default();

        Self {
            llm,
            config: config.clone(),
            sandbox: QuerySandbox::new(sandbox_config),
            cache: RwLock::new(QueryCache::new(config.max_cache_entries)),
        }
    }

    /// Convert natural language to SQL
    pub async fn translate(&self, request: NlQueryRequest) -> ProviderResult<NlQueryResponse> {
        let start = Instant::now();
        let config = request.config.as_ref().unwrap_or(&self.config);

        // Generate cache key
        let cache_key = self.generate_cache_key(&request);

        // Check cache
        if let Ok(mut cache) = self.cache.write() {
            if let Some(cached) = cache.get(&cache_key) {
                return Ok(cached);
            }
        }

        // Analyze the question
        let analysis = self.analyze_question(&request.question);

        // Build system prompt with schema context
        let system_prompt = self.build_system_prompt(&request, config);

        // Build user prompt
        let user_prompt = self.build_user_prompt(&request);

        // Generate SQL using LLM
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: system_prompt,
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: user_prompt,
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ];

        let llm_request = LlmRequest {
            messages,
            model: config.model.clone(),
            temperature: Some(config.temperature),
            max_tokens: Some(2000),
            ..Default::default()
        };

        let llm_response = self.llm.chat(llm_request).await?;

        // Extract SQL from response
        let mut sql = self.extract_sql(&llm_response.message.content)?;

        // Validate SQL
        let validation = if config.validate_sql {
            let result = self.sandbox.validate(&sql);

            // Auto-correct if needed
            if !result.allowed && config.auto_correct {
                for _ in 0..config.max_correction_attempts {
                    if let Ok(corrected) = self
                        .attempt_correction(&sql, &result, &request, config)
                        .await
                    {
                        let new_result = self.sandbox.validate(&corrected);
                        if new_result.allowed {
                            sql = corrected;
                            break;
                        }
                    }
                }
            }

            Some(self.sandbox.validate(&sql))
        } else {
            None
        };

        // Calculate confidence
        let confidence = self.calculate_confidence(&analysis, &validation, &sql);

        // Generate explanation
        let explanation = if config.explain_results {
            Some(self.generate_explanation(&request.question, &sql, &analysis))
        } else {
            None
        };

        // Generate suggestions
        let suggestions = if config.enable_suggestions {
            Some(self.generate_suggestions(&request, &analysis))
        } else {
            None
        };

        // Build warnings
        let mut warnings = Vec::new();
        if let Some(ref v) = validation {
            warnings.extend(v.warnings.clone());
        }
        if analysis.limit.is_none() && analysis.intent != QueryIntent::Aggregate {
            warnings.push(format!(
                "No LIMIT specified. Results will be capped at {} rows.",
                config.max_results
            ));
        }

        // Build response
        let processing_time_ms = start.elapsed().as_millis() as u64;
        let response = NlQueryResponse {
            sql,
            explanation,
            confidence,
            analysis,
            validation,
            suggestions,
            warnings,
            usage: llm_response.usage.map(|u| TokenUsage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            }),
            cached: false,
            processing_time_ms,
        };

        // Cache the response
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(
                cache_key,
                response.clone(),
                Duration::from_secs(config.cache_ttl_secs),
            );
        }

        Ok(response)
    }

    /// Clear the query cache
    pub fn clear_cache(&self) {
        if let Ok(mut cache) = self.cache.write() {
            cache.clear();
        }
    }

    /// Generate cache key from request
    fn generate_cache_key(&self, request: &NlQueryRequest) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();

        request.question.to_lowercase().trim().hash(&mut hasher);
        if let Some(ref schema) = request.schema {
            schema.database.hash(&mut hasher);
            schema.schema.hash(&mut hasher);
            for table in &schema.tables {
                table.name.hash(&mut hasher);
            }
        }
        if let Some(ref tenant_id) = request.tenant_id {
            tenant_id.hash(&mut hasher);
        }

        format!("{:x}", hasher.finish())
    }

    /// Analyze the natural language question
    fn analyze_question(&self, question: &str) -> QueryAnalysis {
        let lower = question.to_lowercase();

        // Detect intent
        let intent = self.detect_intent(&lower);

        // Extract entities
        let entities = self.extract_entities(question);

        // Detect aggregations
        let aggregations = if RE_AGGREGATION.is_match(&lower) {
            RE_AGGREGATION
                .find_iter(&lower)
                .map(|m| m.as_str().to_string())
                .collect()
        } else {
            Vec::new()
        };

        // Detect time range
        let time_range = self.detect_time_range(&lower);

        // Detect sorting
        let sorting = if RE_SORTING.is_match(&lower) {
            Some(SortingInfo {
                column: "detected_from_context".to_string(),
                direction: if lower.contains("lowest")
                    || lower.contains("bottom")
                    || lower.contains("least")
                    || lower.contains("oldest")
                {
                    SortDirection::Asc
                } else {
                    SortDirection::Desc
                },
            })
        } else {
            None
        };

        // Detect limit
        let limit = self.detect_limit(&lower);

        QueryAnalysis {
            intent,
            tables: Vec::new(),    // Will be populated from schema matching
            columns: Vec::new(),   // Will be populated from schema matching
            filters: Vec::new(),   // Will be populated from entity extraction
            aggregations,
            sorting,
            limit,
            time_range,
            entities,
        }
    }

    /// Detect query intent
    fn detect_intent(&self, question: &str) -> QueryIntent {
        let lower = question.to_lowercase();

        if lower.contains("how many") || lower.contains("count") || lower.contains("number of") {
            QueryIntent::Count
        } else if RE_AGGREGATION.is_match(&lower)
            && (lower.contains("total")
                || lower.contains("sum")
                || lower.contains("average")
                || lower.contains("avg"))
        {
            QueryIntent::Aggregate
        } else if RE_SORTING.is_match(&lower)
            && (lower.contains("top")
                || lower.contains("highest")
                || lower.contains("lowest")
                || lower.contains("best")
                || lower.contains("worst"))
        {
            QueryIntent::Rank
        } else if lower.contains("compare") || lower.contains("versus") || lower.contains(" vs ") {
            QueryIntent::Compare
        } else if RE_TIME.is_match(&lower)
            && (lower.contains("trend")
                || lower.contains("over time")
                || lower.contains("by month")
                || lower.contains("by year"))
        {
            QueryIntent::TimeSeries
        } else if lower.contains("find")
            || lower.contains("search")
            || lower.contains("where")
            || lower.contains("which")
        {
            QueryIntent::Search
        } else if lower.contains("join")
            || lower.contains("with")
            || lower.contains("related")
            || lower.contains("associated")
        {
            QueryIntent::Join
        } else if lower.contains("show")
            || lower.contains("list")
            || lower.contains("get")
            || lower.contains("select")
        {
            QueryIntent::Select
        } else {
            QueryIntent::Unknown
        }
    }

    /// Extract entities from question
    fn extract_entities(&self, question: &str) -> Vec<ExtractedEntity> {
        let mut entities = Vec::new();

        // Extract quoted strings
        let quote_re = Regex::new(r#"['"]([^'"]+)['"]"#).ok();
        if let Some(re) = quote_re {
            for cap in re.captures_iter(question) {
                if let Some(m) = cap.get(1) {
                    entities.push(ExtractedEntity {
                        text: m.as_str().to_string(),
                        entity_type: "quoted_value".to_string(),
                        normalized: Some(m.as_str().to_string()),
                    });
                }
            }
        }

        // Extract numbers
        let num_re = Regex::new(r"\b(\d+(?:\.\d+)?)\b").ok();
        if let Some(re) = num_re {
            for cap in re.captures_iter(question) {
                if let Some(m) = cap.get(1) {
                    entities.push(ExtractedEntity {
                        text: m.as_str().to_string(),
                        entity_type: "number".to_string(),
                        normalized: Some(m.as_str().to_string()),
                    });
                }
            }
        }

        // Extract date-like patterns
        let date_re = Regex::new(r"\b(\d{4}-\d{2}-\d{2}|\d{1,2}/\d{1,2}/\d{2,4})\b").ok();
        if let Some(re) = date_re {
            for cap in re.captures_iter(question) {
                if let Some(m) = cap.get(1) {
                    entities.push(ExtractedEntity {
                        text: m.as_str().to_string(),
                        entity_type: "date".to_string(),
                        normalized: Some(m.as_str().to_string()),
                    });
                }
            }
        }

        entities
    }

    /// Detect time range from question
    fn detect_time_range(&self, question: &str) -> Option<TimeRange> {
        let lower = question.to_lowercase();

        if lower.contains("today") {
            Some(TimeRange {
                start: Some("today".to_string()),
                end: Some("today".to_string()),
                description: "today".to_string(),
            })
        } else if lower.contains("yesterday") {
            Some(TimeRange {
                start: Some("yesterday".to_string()),
                end: Some("yesterday".to_string()),
                description: "yesterday".to_string(),
            })
        } else if lower.contains("last week") {
            Some(TimeRange {
                start: None,
                end: None,
                description: "last 7 days".to_string(),
            })
        } else if lower.contains("last month") {
            Some(TimeRange {
                start: None,
                end: None,
                description: "last 30 days".to_string(),
            })
        } else if lower.contains("last year") || lower.contains("past year") {
            Some(TimeRange {
                start: None,
                end: None,
                description: "last 365 days".to_string(),
            })
        } else if lower.contains("this week") {
            Some(TimeRange {
                start: None,
                end: None,
                description: "current week".to_string(),
            })
        } else if lower.contains("this month") {
            Some(TimeRange {
                start: None,
                end: None,
                description: "current month".to_string(),
            })
        } else if lower.contains("this year") {
            Some(TimeRange {
                start: None,
                end: None,
                description: "current year".to_string(),
            })
        } else {
            None
        }
    }

    /// Detect limit from question
    fn detect_limit(&self, question: &str) -> Option<usize> {
        // Pattern: "top N", "first N", "N results"
        let limit_re =
            Regex::new(r"(?i)(?:top|first|limit)\s+(\d+)|(\d+)\s+(?:results?|rows?|records?)")
                .ok()?;

        if let Some(cap) = limit_re.captures(question) {
            let num = cap
                .get(1)
                .or_else(|| cap.get(2))
                .and_then(|m| m.as_str().parse().ok());
            return num;
        }

        None
    }

    /// Build system prompt for LLM
    fn build_system_prompt(&self, request: &NlQueryRequest, config: &NlQueryConfig) -> String {
        let dialect_hint = config.dialect.features_hint();

        let mut prompt = config.system_prompt.clone().unwrap_or_else(|| {
            format!(
                r#"You are an expert SQL query generator. Convert natural language questions to SQL queries.

SQL Dialect: {:?}
{}

Rules:
1. Generate only the SQL query, no explanations
2. Use proper SQL syntax for the specified dialect
3. Add appropriate WHERE clauses for filters mentioned
4. Use JOINs when multiple tables are needed
5. Add ORDER BY for ranking/sorting questions
6. Add LIMIT for "top N" or bounded queries
7. Use appropriate aggregation functions (COUNT, SUM, AVG, etc.)
8. Handle NULL values appropriately
9. Use parameterized values where possible (use $1, $2, etc.)
10. Wrap the SQL in ```sql code blocks

Important:
- Only generate SELECT queries (no INSERT, UPDATE, DELETE, DROP, etc.)
- Do not include comments in the SQL
- Ensure all table and column names match the schema exactly"#,
                config.dialect, dialect_hint
            )
        });

        // Add schema context
        if let Some(ref schema) = request.schema {
            prompt.push_str("\n\nAvailable Schema:\n");
            for table in &schema.tables {
                prompt.push_str(&format!("\nTable: {}\n", table.name));
                if let Some(ref desc) = table.description {
                    prompt.push_str(&format!("  Description: {}\n", desc));
                }
                prompt.push_str("  Columns:\n");
                for col in &table.columns {
                    let mut col_desc = format!("    - {} ({})", col.name, col.data_type);
                    if !col.nullable {
                        col_desc.push_str(" NOT NULL");
                    }
                    if col.is_primary_key {
                        col_desc.push_str(" PRIMARY KEY");
                    }
                    if let Some(ref desc) = col.description {
                        col_desc.push_str(&format!(" -- {}", desc));
                    }
                    prompt.push_str(&format!("{}\n", col_desc));
                }

                // Add foreign key hints
                if let Some(ref fks) = table.foreign_keys {
                    for fk in fks {
                        prompt.push_str(&format!(
                            "  Foreign Key: {} -> {}.{}\n",
                            fk.columns.join(", "),
                            fk.ref_table,
                            fk.ref_columns.join(", ")
                        ));
                    }
                }
            }

            // Add hints
            if let Some(ref hints) = schema.hints {
                prompt.push_str("\nHints:\n");
                for hint in hints {
                    prompt.push_str(&format!("- {}\n", hint));
                }
            }
        }

        // Add conversation context
        if let Some(ref ctx) = request.context {
            if !ctx.history.is_empty() {
                prompt.push_str("\n\nRecent Query History:\n");
                for entry in ctx.history.iter().rev().take(3) {
                    prompt.push_str(&format!("Q: {}\nSQL: {}\n\n", entry.question, entry.sql));
                }
            }
        }

        prompt
    }

    /// Build user prompt
    fn build_user_prompt(&self, request: &NlQueryRequest) -> String {
        format!(
            "Convert this question to SQL:\n\n{}",
            request.question
        )
    }

    /// Extract SQL from LLM response
    fn extract_sql(&self, response: &str) -> ProviderResult<String> {
        // Try to extract from code block first
        if let Some(caps) = RE_SQL_BLOCK.captures(response) {
            if let Some(sql) = caps.get(1) {
                return Ok(sql.as_str().trim().to_string());
            }
        }

        // Try to find SELECT statement directly
        if let Some(caps) = RE_SELECT.captures(response) {
            if let Some(sql) = caps.get(1) {
                return Ok(sql.as_str().trim().to_string());
            }
        }

        // If nothing found, return the whole response trimmed
        let trimmed = response.trim();
        if trimmed.to_uppercase().starts_with("SELECT") {
            Ok(trimmed.to_string())
        } else {
            Err(ProviderError::Api(
                "Could not extract valid SQL from response".to_string(),
            ))
        }
    }

    /// Attempt to correct invalid SQL
    async fn attempt_correction(
        &self,
        sql: &str,
        validation: &SandboxResult,
        request: &NlQueryRequest,
        config: &NlQueryConfig,
    ) -> ProviderResult<String> {
        let errors: Vec<String> = validation.errors.iter().map(|e| e.message.clone()).collect();

        let correction_prompt = format!(
            r#"The following SQL query has validation errors. Please fix them.

Original SQL:
```sql
{}
```

Errors:
{}

Please provide a corrected SQL query that addresses these issues.
Only output the corrected SQL in a ```sql code block."#,
            sql,
            errors.join("\n- ")
        );

        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: self.build_system_prompt(request, config),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: correction_prompt,
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ];

        let llm_request = LlmRequest {
            messages,
            model: config.model.clone(),
            temperature: Some(0.0), // Lower temperature for corrections
            max_tokens: Some(2000),
            ..Default::default()
        };

        let response = self.llm.chat(llm_request).await?;
        self.extract_sql(&response.message.content)
    }

    /// Calculate confidence score
    fn calculate_confidence(
        &self,
        analysis: &QueryAnalysis,
        validation: &Option<SandboxResult>,
        sql: &str,
    ) -> f32 {
        let mut confidence = 0.5; // Base confidence

        // Intent detection confidence
        if analysis.intent != QueryIntent::Unknown {
            confidence += 0.1;
        }

        // Validation passed
        if let Some(ref v) = validation {
            if v.allowed {
                confidence += 0.2;
            } else {
                confidence -= 0.2;
            }
        }

        // Has entities that map to values
        if !analysis.entities.is_empty() {
            confidence += 0.05 * analysis.entities.len() as f32;
        }

        // SQL structure checks
        let upper_sql = sql.to_uppercase();
        if upper_sql.contains("SELECT") {
            confidence += 0.05;
        }
        if upper_sql.contains("FROM") {
            confidence += 0.05;
        }
        if upper_sql.contains("WHERE") && !analysis.filters.is_empty() {
            confidence += 0.05;
        }

        // Cap at 0.95 (never 100% confident)
        confidence.clamp(0.1, 0.95)
    }

    /// Generate explanation for the query
    fn generate_explanation(
        &self,
        question: &str,
        sql: &str,
        analysis: &QueryAnalysis,
    ) -> String {
        let mut parts = Vec::new();

        // Intent description
        let intent_desc = match analysis.intent {
            QueryIntent::Select => "retrieving data",
            QueryIntent::Aggregate => "calculating aggregated values",
            QueryIntent::Compare => "comparing data",
            QueryIntent::Rank => "ranking results",
            QueryIntent::TimeSeries => "analyzing data over time",
            QueryIntent::Search => "searching for specific records",
            QueryIntent::Count => "counting records",
            QueryIntent::Join => "combining data from multiple tables",
            QueryIntent::Unknown => "querying data",
        };
        parts.push(format!("This query is {} based on your question.", intent_desc));

        // Tables involved
        if !analysis.tables.is_empty() {
            parts.push(format!(
                "It queries the {} table(s).",
                analysis.tables.join(", ")
            ));
        }

        // Aggregations
        if !analysis.aggregations.is_empty() {
            parts.push(format!(
                "It uses {} aggregation(s).",
                analysis.aggregations.join(", ")
            ));
        }

        // Sorting
        if let Some(ref sort) = analysis.sorting {
            parts.push(format!(
                "Results are sorted by {} in {} order.",
                sort.column,
                match sort.direction {
                    SortDirection::Asc => "ascending",
                    SortDirection::Desc => "descending",
                }
            ));
        }

        // Limit
        if let Some(limit) = analysis.limit {
            parts.push(format!("Limited to {} results.", limit));
        }

        // Time range
        if let Some(ref tr) = analysis.time_range {
            parts.push(format!("Filtered to {}.", tr.description));
        }

        parts.join(" ")
    }

    /// Generate query suggestions
    fn generate_suggestions(
        &self,
        request: &NlQueryRequest,
        analysis: &QueryAnalysis,
    ) -> Vec<QuerySuggestion> {
        let mut suggestions = Vec::new();

        // Suggest adding limit if not present
        if analysis.limit.is_none() && analysis.intent == QueryIntent::Select {
            suggestions.push(QuerySuggestion {
                text: "Add a limit to your query for better performance".to_string(),
                sql: None,
                reason: "Unbounded queries can be slow on large tables".to_string(),
            });
        }

        // Suggest time filter for aggregate queries
        if analysis.intent == QueryIntent::Aggregate && analysis.time_range.is_none() {
            suggestions.push(QuerySuggestion {
                text: "Consider adding a time filter (e.g., 'last month')".to_string(),
                sql: None,
                reason: "Time-bounded aggregations are often more meaningful".to_string(),
            });
        }

        // Suggest related queries based on intent
        match analysis.intent {
            QueryIntent::Count => {
                suggestions.push(QuerySuggestion {
                    text: format!(
                        "Show me the actual {} instead of just the count",
                        if request.question.contains("user") {
                            "users"
                        } else if request.question.contains("order") {
                            "orders"
                        } else {
                            "records"
                        }
                    ),
                    sql: None,
                    reason: "See the underlying data".to_string(),
                });
            }
            QueryIntent::Rank => {
                suggestions.push(QuerySuggestion {
                    text: "Show me the bottom/lowest instead".to_string(),
                    sql: None,
                    reason: "View the opposite end of the ranking".to_string(),
                });
            }
            _ => {}
        }

        suggestions
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_intent_count() {
        let engine = create_test_engine();
        let analysis = engine.analyze_question("How many users are there?");
        assert_eq!(analysis.intent, QueryIntent::Count);
    }

    #[test]
    fn test_detect_intent_rank() {
        let engine = create_test_engine();
        let analysis = engine.analyze_question("Show me the top 10 products by sales");
        assert_eq!(analysis.intent, QueryIntent::Rank);
    }

    #[test]
    fn test_detect_intent_aggregate() {
        let engine = create_test_engine();
        let analysis = engine.analyze_question("What is the total revenue?");
        assert_eq!(analysis.intent, QueryIntent::Aggregate);
    }

    #[test]
    fn test_detect_limit() {
        let engine = create_test_engine();
        let analysis = engine.analyze_question("Show me the top 5 customers");
        assert_eq!(analysis.limit, Some(5));
    }

    #[test]
    fn test_detect_time_range() {
        let engine = create_test_engine();

        let analysis = engine.analyze_question("Show orders from last week");
        assert!(analysis.time_range.is_some());
        assert_eq!(
            analysis.time_range.as_ref().map(|t| t.description.as_str()),
            Some("last 7 days")
        );
    }

    #[test]
    fn test_extract_entities() {
        let engine = create_test_engine();
        let entities = engine.extract_entities("Find users with name 'John' and age 25");

        assert!(entities.iter().any(|e| e.text == "John"));
        assert!(entities.iter().any(|e| e.text == "25"));
    }

    #[test]
    fn test_sql_extraction_from_code_block() {
        let engine = create_test_engine();
        let response = r#"Here's the SQL query:

```sql
SELECT * FROM users WHERE status = 'active'
```

This will return all active users."#;

        let sql = engine.extract_sql(response).unwrap();
        assert_eq!(sql, "SELECT * FROM users WHERE status = 'active'");
    }

    #[test]
    fn test_sql_extraction_direct() {
        let engine = create_test_engine();
        let response = "SELECT name, email FROM users LIMIT 10";

        let sql = engine.extract_sql(response).unwrap();
        assert_eq!(sql, "SELECT name, email FROM users LIMIT 10");
    }

    #[test]
    fn test_cache_key_generation() {
        let engine = create_test_engine();

        let request1 = NlQueryRequest {
            question: "Show all users".to_string(),
            schema: None,
            context: None,
            config: None,
            user_id: None,
            tenant_id: None,
            metadata: None,
        };

        let request2 = NlQueryRequest {
            question: "SHOW ALL USERS".to_string(),
            schema: None,
            context: None,
            config: None,
            user_id: None,
            tenant_id: None,
            metadata: None,
        };

        // Same question with different case should have same cache key
        assert_eq!(
            engine.generate_cache_key(&request1),
            engine.generate_cache_key(&request2)
        );
    }

    fn create_test_engine() -> NlQueryEngine {
        // Create a mock provider for testing
        struct MockProvider;

        #[async_trait::async_trait]
        impl LlmProvider for MockProvider {
            fn name(&self) -> &str {
                "mock"
            }

            async fn list_models(&self) -> ProviderResult<Vec<super::super::providers::ModelInfo>> {
                Ok(vec![])
            }

            async fn chat(
                &self,
                _request: LlmRequest,
            ) -> ProviderResult<super::super::providers::LlmResponse> {
                Ok(super::super::providers::LlmResponse {
                    id: "test".to_string(),
                    model: "mock".to_string(),
                    message: ChatMessage {
                        role: MessageRole::Assistant,
                        content: "SELECT * FROM users".to_string(),
                        name: None,
                        function_call: None,
                        tool_calls: None,
                        tool_call_id: None,
                    },
                    finish_reason: Some("stop".to_string()),
                    usage: None,
                })
            }

            async fn chat_stream(
                &self,
                _request: LlmRequest,
            ) -> ProviderResult<
                Box<
                    dyn futures::Stream<
                            Item = ProviderResult<super::super::providers::StreamChunk>,
                        > + Send
                        + Unpin,
                >,
            > {
                Err(ProviderError::Api("Not implemented".to_string()))
            }

            fn count_tokens(&self, text: &str, _model: &str) -> ProviderResult<usize> {
                Ok(text.len() / 4)
            }

            fn supports_model(&self, _model: &str) -> bool {
                true
            }

            fn model_info(&self, _model: &str) -> Option<super::super::providers::ModelInfo> {
                None
            }
        }

        NlQueryEngine::new(Arc::new(MockProvider))
    }
}
