//! Unified EXPLAIN options for SQL and REPL
//!
//! This module provides a unified options structure that combines:
//! - PostgreSQL-compatible EXPLAIN options (ANALYZE, VERBOSE, FORMAT, COSTS, etc.)
//! - HeliosDB extensions (STORAGE, AI, WHY_NOT, INDEXES, STATISTICS)
//!
//! # SQL Syntax
//!
//! ```sql
//! -- PostgreSQL-compatible
//! EXPLAIN SELECT * FROM users;
//! EXPLAIN ANALYZE SELECT * FROM users WHERE id = 1;
//! EXPLAIN (ANALYZE, VERBOSE) SELECT * FROM users;
//! EXPLAIN (FORMAT JSON) SELECT * FROM users;
//!
//! -- HeliosDB Extensions
//! EXPLAIN (STORAGE) SELECT * FROM orders;
//! EXPLAIN (AI) SELECT * FROM users WHERE status = 'active';
//! EXPLAIN (ANALYZE, STORAGE, WHY_NOT) SELECT * FROM orders;
//! ```
//!
//! # REPL Syntax
//!
//! ```text
//! \explain SELECT * FROM users
//! \explain analyze SELECT * FROM users
//! \explain verbose format json SELECT * FROM users
//! \explain storage ai SELECT * FROM orders
//! ```

use serde::{Deserialize, Serialize};

use super::explain::{ExplainFormat, ExplainMode};

/// Unified EXPLAIN options structure
///
/// Combines SQL parser options with advanced explain features.
/// Used by both SQL EXPLAIN command and REPL \explain meta-command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplainOptions {
    // ─────────────────────────────────────────────────────────────────────────
    // Core options (PostgreSQL-compatible)
    // ─────────────────────────────────────────────────────────────────────────

    /// Execute query and show actual statistics (EXPLAIN ANALYZE)
    pub analyze: bool,

    /// Show additional plan details (EXPLAIN VERBOSE)
    pub verbose: bool,

    /// Output format (TEXT, JSON, YAML, TREE)
    pub format: ExplainFormatOption,

    /// Show cost estimates (default: true)
    pub costs: bool,

    /// Show buffer usage statistics (requires ANALYZE)
    pub buffers: bool,

    /// Show timing information (requires ANALYZE, default: true with ANALYZE)
    pub timing: bool,

    /// Show summary statistics at end
    pub summary: bool,

    // ─────────────────────────────────────────────────────────────────────────
    // HeliosDB Extensions
    // ─────────────────────────────────────────────────────────────────────────

    /// Show storage layer details (column modes, bloom filters, zone maps, compression)
    pub storage: bool,

    /// Enable AI-powered natural language explanations
    pub ai: bool,

    /// Enable Why-Not analysis (why optimizations weren't applied)
    pub why_not: bool,

    /// Show index analysis (used/unused indexes, recommendations)
    pub indexes: bool,

    /// Show table/column statistics information
    pub statistics: bool,
}

impl Default for ExplainOptions {
    fn default() -> Self {
        Self {
            analyze: false,
            verbose: false,
            format: ExplainFormatOption::Text,
            costs: true,  // PostgreSQL default
            buffers: false,
            timing: true,  // Default on when ANALYZE is used
            summary: false,
            storage: false,
            ai: false,
            why_not: false,
            indexes: false,
            statistics: false,
        }
    }
}

impl ExplainOptions {
    /// Create new ExplainOptions with defaults
    pub fn new() -> Self {
        Self::default()
    }

    /// Create options for simple EXPLAIN
    pub fn standard() -> Self {
        Self::default()
    }

    /// Create options for EXPLAIN ANALYZE
    pub fn analyze() -> Self {
        Self {
            analyze: true,
            timing: true,
            summary: true,
            ..Self::default()
        }
    }

    /// Create options for EXPLAIN VERBOSE
    pub fn verbose() -> Self {
        Self {
            verbose: true,
            ..Self::default()
        }
    }

    /// Create options for full analysis (all features)
    pub fn full() -> Self {
        Self {
            analyze: true,
            verbose: true,
            costs: true,
            buffers: true,
            timing: true,
            summary: true,
            storage: true,
            ai: false,  // AI requires LLM endpoint
            why_not: true,
            indexes: true,
            statistics: true,
            ..Self::default()
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Builder methods
    // ─────────────────────────────────────────────────────────────────────────

    /// Enable ANALYZE mode
    pub fn with_analyze(mut self) -> Self {
        self.analyze = true;
        self
    }

    /// Enable VERBOSE mode
    pub fn with_verbose(mut self) -> Self {
        self.verbose = true;
        self
    }

    /// Set output format
    pub fn with_format(mut self, format: ExplainFormatOption) -> Self {
        self.format = format;
        self
    }

    /// Enable storage feature reporting
    pub fn with_storage(mut self) -> Self {
        self.storage = true;
        self
    }

    /// Enable AI explanations
    pub fn with_ai(mut self) -> Self {
        self.ai = true;
        self
    }

    /// Enable Why-Not analysis
    pub fn with_why_not(mut self) -> Self {
        self.why_not = true;
        self
    }

    /// Enable index analysis
    pub fn with_indexes(mut self) -> Self {
        self.indexes = true;
        self
    }

    /// Enable statistics reporting
    pub fn with_statistics(mut self) -> Self {
        self.statistics = true;
        self
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Conversion methods
    // ─────────────────────────────────────────────────────────────────────────

    /// Convert to ExplainMode for ExplainPlanner
    ///
    /// Priority: AI > Analyze (with why_not) > Verbose > Standard
    pub fn to_explain_mode(&self) -> ExplainMode {
        if self.ai {
            ExplainMode::AI
        } else if self.analyze && self.why_not {
            ExplainMode::Analyze
        } else if self.verbose {
            ExplainMode::Verbose
        } else if self.analyze {
            // ANALYZE without WHY_NOT still uses Verbose for cost info
            ExplainMode::Verbose
        } else {
            ExplainMode::Standard
        }
    }

    /// Convert to ExplainFormat for ExplainPlanner
    pub fn to_explain_format(&self) -> ExplainFormat {
        match self.format {
            ExplainFormatOption::Text => ExplainFormat::Text,
            ExplainFormatOption::Json => ExplainFormat::JSON,
            ExplainFormatOption::Yaml => ExplainFormat::YAML,
            ExplainFormatOption::Tree => ExplainFormat::Tree,
        }
    }

    /// Check if any extended features are enabled
    pub fn has_extended_features(&self) -> bool {
        self.storage || self.ai || self.why_not || self.indexes || self.statistics
    }

    /// Check if actual execution is required
    pub fn requires_execution(&self) -> bool {
        self.analyze || self.buffers
    }
}

/// EXPLAIN output format options
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExplainFormatOption {
    /// Human-readable text output (default)
    #[default]
    Text,
    /// JSON structured output
    Json,
    /// YAML structured output
    Yaml,
    /// Tree visualization with ANSI colors
    Tree,
}

impl ExplainFormatOption {
    /// Parse format from string (case-insensitive)
    pub fn from_str(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "JSON" => Self::Json,
            "YAML" => Self::Yaml,
            "TREE" => Self::Tree,
            "XML" => Self::Text,  // XML not supported, fallback to Text
            _ => Self::Text,
        }
    }

    /// Get format name for display
    pub fn name(&self) -> &'static str {
        match self {
            Self::Text => "TEXT",
            Self::Json => "JSON",
            Self::Yaml => "YAML",
            Self::Tree => "TREE",
        }
    }
}

impl std::fmt::Display for ExplainFormatOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_options() {
        let opts = ExplainOptions::default();
        assert!(!opts.analyze);
        assert!(!opts.verbose);
        assert!(opts.costs);
        assert!(!opts.storage);
        assert_eq!(opts.format, ExplainFormatOption::Text);
    }

    #[test]
    fn test_analyze_options() {
        let opts = ExplainOptions::analyze();
        assert!(opts.analyze);
        assert!(opts.timing);
        assert!(opts.summary);
    }

    #[test]
    fn test_builder_pattern() {
        let opts = ExplainOptions::new()
            .with_analyze()
            .with_storage()
            .with_format(ExplainFormatOption::Json);

        assert!(opts.analyze);
        assert!(opts.storage);
        assert_eq!(opts.format, ExplainFormatOption::Json);
    }

    #[test]
    fn test_to_explain_mode() {
        assert_eq!(ExplainOptions::standard().to_explain_mode(), ExplainMode::Standard);
        assert_eq!(ExplainOptions::verbose().to_explain_mode(), ExplainMode::Verbose);

        let ai_opts = ExplainOptions::new().with_ai();
        assert_eq!(ai_opts.to_explain_mode(), ExplainMode::AI);

        let analyze_why_not = ExplainOptions::new().with_analyze().with_why_not();
        assert_eq!(analyze_why_not.to_explain_mode(), ExplainMode::Analyze);
    }

    #[test]
    fn test_format_from_str() {
        assert_eq!(ExplainFormatOption::from_str("json"), ExplainFormatOption::Json);
        assert_eq!(ExplainFormatOption::from_str("JSON"), ExplainFormatOption::Json);
        assert_eq!(ExplainFormatOption::from_str("yaml"), ExplainFormatOption::Yaml);
        assert_eq!(ExplainFormatOption::from_str("tree"), ExplainFormatOption::Tree);
        assert_eq!(ExplainFormatOption::from_str("unknown"), ExplainFormatOption::Text);
    }
}
