//! Help system for HeliosDB Lite REPL
//!
//! Provides feature-based help categories with responsive terminal layout.
//! Automatically detects terminal width and adjusts layout accordingly.

use super::registry::{self, Category};
use colored::Colorize;

/// Help system manager for the REPL
pub struct HelpManager;

impl HelpManager {
    /// Print main help, detecting terminal width for layout
    pub fn print_help_main() {
        let width = Self::get_terminal_width();
        if width >= 120 {
            Self::print_help_3col();
        } else {
            Self::print_help_2col();
        }
    }

    /// Print help for specific category
    pub fn print_help_category(category: &str) {
        match category.to_lowercase().as_str() {
            "basics" | "basic" => Self::help_basics(),
            "schema" => Self::help_schema(),
            "branching" | "branches" => Self::help_branching(),
            "time-travel" | "timetravel" | "travel" => Self::help_time_travel(),
            "vectors" | "vector" => Self::help_vectors(),
            "documents" | "docs" | "document" => Self::help_documents(),
            "agents" | "agent" | "sessions" => Self::help_agents(),
            "ai" => Self::help_ai(),
            "tenants" | "tenant" | "multi-tenancy" | "multitenancy" => Self::help_tenants(),
            "settings" | "config" | "configuration" => Self::help_settings(),
            "examples" | "example" => Self::help_examples(),
            "sql" => Self::help_sql(),
            _ => Self::print_unknown_category(category),
        }
    }

    /// Get terminal width, defaulting to 80 if detection fails
    fn get_terminal_width() -> usize {
        terminal_size::terminal_size().map(|(w, _)| w.0 as usize).unwrap_or(80)
    }

    /// Print 2-column layout for narrow terminals (< 120 cols)
    fn print_help_2col() {
        println!(
            "\n{}",
            "╔══════════════════════════════════════════════════════════════════════════════╗".cyan()
        );
        println!(
            "{}",
            "║              HeliosDB Nano - Interactive SQL REPL Help                      ║"
                .cyan()
                .bold()
        );
        println!(
            "{}",
            "╚══════════════════════════════════════════════════════════════════════════════╝".cyan()
        );

        println!("\n{}", "Quick Start:".yellow().bold());
        println!("  {}  Show this help", "\\h".cyan());
        println!("  {}  Show category help (e.g., \\h vectors)", "\\h <category>".cyan());
        println!("  {}  Exit REPL", "\\q".cyan());

        println!("\n{}", "Help Categories:".yellow().bold());
        println!("  {}  Fundamental REPL operations", "basics".cyan());
        println!("  {}  Table and view exploration", "schema".cyan());
        println!("  {}  Database branching workflows", "branching".cyan());
        println!("  {}  Query historical data", "time-travel".cyan());
        println!("  {}  Vector search and indexing", "vectors".cyan());
        println!("  {}  Document storage and RAG", "documents".cyan());
        println!("  {}  AI agent sessions", "agents".cyan());
        println!("  {}  AI inference and generation", "ai".cyan());
        println!("  {}  Multi-tenant isolation", "tenants".cyan());
        println!("  {}  Configuration and tuning", "settings".cyan());
        println!("  {}  Practical SQL examples", "examples".cyan());
        println!("  {}  SQL syntax reference", "sql".cyan());

        println!("\n{}", "Usage:".dimmed());
        println!("  {}  {}", "\\h vectors".dimmed(), "- Show vector search help".dimmed());
        println!("  {}  {}", "\\h branching".dimmed(), "- Show branching help".dimmed());
        println!();
    }

    /// Print 3-column layout for wide terminals (>= 120 cols)
    fn print_help_3col() {
        println!("\n{}", "╔══════════════════════════════════════════════════════════════════════════════════════════════════════════════════╗".cyan());
        println!("{}", "║                             HeliosDB Nano - Interactive SQL REPL Help                                           ║".cyan().bold());
        println!("{}", "╚══════════════════════════════════════════════════════════════════════════════════════════════════════════════════╝".cyan());

        println!("\n{}", "Quick Start:".yellow().bold());
        println!(
            "  {}  Show this help  │  {}  Show category help  │  {}  Exit REPL",
            "\\h".cyan(),
            "\\h <category>".cyan(),
            "\\q".cyan()
        );

        println!("\n{}", "Help Categories:".yellow().bold());
        println!(
            "  {}  REPL basics       │  {}  Schema exploration  │  {}  Branch workflows",
            "basics".cyan(),
            "schema".cyan(),
            "branching".cyan()
        );
        println!(
            "  {}  Time-travel queries  │  {}  Vector search      │  {}  Document storage",
            "time-travel".cyan(),
            "vectors".cyan(),
            "documents".cyan()
        );
        println!(
            "  {}  AI agents        │  {}  AI inference       │  {}  Multi-tenancy",
            "agents".cyan(),
            "ai".cyan(),
            "tenants".cyan()
        );
        println!(
            "  {}  Configuration    │  {}  SQL examples      │  {}  SQL syntax",
            "settings".cyan(),
            "examples".cyan(),
            "sql".cyan()
        );

        println!("\n{}", "Usage:".dimmed());
        println!(
            "  {}  {}  {}",
            "\\h vectors".dimmed(),
            "- Show vector search help".dimmed(),
            "│  \\h branching - Show branching help".dimmed()
        );
        println!();
    }

    /// Help for basic REPL operations
    fn help_basics() {
        Self::print_category_header("Basics", "Fundamental REPL Operations");

        println!("\n{}", "Exit Commands:".yellow());
        Self::print_entries(Category::Basics, "Exit Commands");

        println!("\n{}", "Help Commands:".yellow());
        Self::print_entries(Category::Basics, "Help Commands");

        println!("\n{}", "Display Toggles:".yellow());
        Self::print_entries(Category::Basics, "Display Toggles");

        println!("\n{}", "Display Settings:".yellow());
        Self::print_entries(Category::Basics, "Display Settings");

        println!("\n{}", "Information:".yellow());
        Self::print_entries(Category::Basics, "Information");

        println!();
    }

    /// Help for schema exploration
    fn help_schema() {
        Self::print_category_header("Schema", "Table and View Exploration");

        println!("\n{}", "List Objects:".yellow());
        Self::print_entries(Category::Schema, "List Objects");

        println!("\n{}", "Describe Objects:".yellow());
        Self::print_entries(Category::Schema, "Describe Objects");

        println!("\n{}", "Compression:".yellow());
        Self::print_entries(Category::Schema, "Compression");

        println!("\n{}", "Indexes:".yellow());
        Self::print_entries(Category::Schema, "Indexes");

        println!();
    }

    /// Help for database branching
    fn help_branching() {
        Self::print_category_header("Branching", "Database Branching Workflows");

        println!("\n{}", "List and Switch:".yellow());
        Self::print_entries(Category::Branching, "List and Switch");

        println!("\n{}", "Branch Management:".yellow());
        Self::print_command("CREATE BRANCH <name>", "Create new branch from current point");
        Self::print_example("CREATE BRANCH feature-xyz");
        Self::print_command("CREATE BRANCH <name> FROM <parent>", "Create branch from parent");
        Self::print_example("CREATE BRANCH experiment FROM main");

        Self::print_command("DROP BRANCH <name>", "Delete a branch");
        Self::print_example("DROP BRANCH old-feature");

        println!("\n{}", "Merging:".yellow());
        Self::print_command("MERGE BRANCH <source> INTO <target>", "Merge branches");
        Self::print_example("MERGE BRANCH dev INTO main");
        Self::print_command(
            "MERGE BRANCH <source> ABORT",
            "Abandon an unfinished merge; lists the tables it may have partially changed",
        );

        println!("\n{}", "Workflow Example:".green().bold());
        println!("  {}", "-- Create experimental branch".dimmed());
        println!("  {}", "CREATE BRANCH experiment".dimmed());
        println!("  {}", "\\use experiment".dimmed());
        println!("  {}", "-- Make changes, test...".dimmed());
        println!("  {}", "\\use main".dimmed());
        println!("  {}", "MERGE BRANCH experiment INTO main".dimmed());

        println!();
    }

    /// Help for time-travel queries
    fn help_time_travel() {
        Self::print_category_header("Time Travel", "Query Historical Data");

        println!("\n{}", "Query Past States:".yellow());
        Self::print_command("SELECT ... AS OF TIMESTAMP <ts>", "Query data at specific time");
        Self::print_example("SELECT * FROM users AS OF TIMESTAMP '2024-01-15 10:30:00'");

        Self::print_command("SELECT ... AS OF TRANSACTION <id>", "Query data at transaction");
        Self::print_example("SELECT * FROM orders AS OF TRANSACTION 12345");

        println!("\n{}", "Snapshots:".yellow());
        println!(
            "  {}",
            "(Coming in v3.2.0 - Named snapshots for point-in-time recovery)".dimmed()
        );
        Self::print_entries(Category::TimeTravel, "Snapshots");
        Self::print_command(
            "CREATE SNAPSHOT <name> [AT TIMESTAMP <ts>]",
            "Create named snapshot (v3.2.0+)",
        );
        Self::print_example("CREATE SNAPSHOT pre_migration AT TIMESTAMP '2024-01-15 10:00:00'");
        Self::print_command("DROP SNAPSHOT <name> [CASCADE]", "Delete snapshot (v3.2.0+)");

        println!("\n{}", "Version Comparison:".yellow());
        Self::print_command("SELECT ... BETWEEN TIMESTAMP <t1> AND <t2>", "Query changes in range");
        Self::print_example("SELECT * FROM products BETWEEN TIMESTAMP '2024-01-01' AND '2024-01-31'");

        println!("\n{}", "Use Cases:".green().bold());
        println!("  {}", "• Audit data changes over time".dimmed());
        println!("  {}", "• Debug data inconsistencies".dimmed());
        println!("  {}", "• Recover accidentally deleted data".dimmed());
        println!("  {}", "• Compare before/after states".dimmed());

        println!();
    }

    /// Help for vector operations
    fn help_vectors() {
        Self::print_category_header("Vectors", "Vector Search and Indexing");

        println!("\n{}", "Vector Store Commands:".yellow());
        Self::print_entries(Category::Vectors, "Vector Store Commands");

        println!("\n{}", "SQL Vector Operations:".yellow());
        Self::print_command("CREATE TABLE t (embedding VECTOR(384))", "Define vector column");
        Self::print_command("CREATE INDEX idx ON t USING hnsw(embedding)", "Create HNSW index");
        Self::print_command("SELECT * FROM t ORDER BY embedding <-> query LIMIT 10", "K-NN search");
        println!(
            "  {}",
            "Operators: <-> (L2), <#> (inner product), <=> (cosine)".dimmed()
        );

        println!("\n{}", "Supported Metrics:".green().bold());
        println!("  {}", "• cosine    - Cosine similarity (default)".dimmed());
        println!("  {}", "• euclidean - Euclidean distance (L2)".dimmed());
        println!("  {}", "• dot       - Dot product".dimmed());

        println!();
    }

    /// Help for document operations
    fn help_documents() {
        Self::print_category_header("Documents", "Document Storage and RAG");

        println!("\n{}", "Collection Commands:".yellow());
        Self::print_entries(Category::Documents, "Collection Commands");

        println!("\n{}", "Import/Export:".yellow());

        println!("\n{}", "SQL Integration:".yellow());
        Self::print_command("INSERT INTO documents (collection, data) VALUES (...)", "Add document");
        Self::print_example(
            "INSERT INTO documents (collection, data) VALUES ('articles', '{\"title\": \"...\"}'::json)",
        );

        println!("\n{}", "Advanced Document Operations:".yellow());
        Self::print_entries(Category::Documents, "Advanced Document Operations");

        println!("\n{}", "RAG Workflow:".green().bold());
        println!(
            "  {}",
            "1. Store documents with the SQL INSERT or the library API".dimmed()
        );
        println!("  {}", "2. Search semantically: \\search-docs \"query\"".dimmed());
        println!("  {}", "3. RAG search for context: \\rag <coll> <query>".dimmed());
        println!("  {}", "4. Use with AI agents for context-aware responses".dimmed());

        println!();
    }

    /// Help for AI agent operations
    fn help_agents() {
        Self::print_category_header("Agents", "AI Agent Sessions");

        println!("\n{}", "Session Commands:".yellow());
        Self::print_entries(Category::Agents, "Session Commands");
        println!("  {}", "Type 'exit' or 'quit' to end chat".dimmed());
        Self::print_entries(Category::Agents, "Session Commands #2");

        println!("\n{}", "Advanced Session Operations:".yellow());
        Self::print_entries(Category::Agents, "Advanced Session Operations");

        println!("\n{}", "Usage Example:".green().bold());
        println!("  {}", "\\session-new data-analyst".dimmed());
        println!("  {}", "\\chat data-analyst".dimmed());
        println!("  {}", "> What are the top selling products?".dimmed());
        println!("  {}", "> Show me sales trends for Q4".dimmed());
        println!("  {}", "> exit".dimmed());
        println!("  {}", "\\session summarize data-analyst".dimmed());

        println!();
    }

    /// Help for AI features
    fn help_ai() {
        Self::print_category_header("AI", "AI Inference and Generation");

        println!("\n{}", "Templates:".yellow());
        Self::print_entries(Category::Ai, "Templates");

        println!("\n{}", "Inference:".yellow());
        Self::print_entries(Category::Ai, "Inference");

        println!("\n{}", "Generation:".yellow());
        Self::print_entries(Category::Ai, "Generation");

        println!("\n{}", "Embeddings & Models:".yellow());
        Self::print_entries(Category::Ai, "Embeddings & Models");

        println!("\n{}", "Chat Mode:".yellow());
        println!("  {}", "Interactive mode for multi-turn conversations".dimmed());

        println!("\n{}", "Configuration:".yellow());

        println!();
    }

    /// Help for multi-tenancy operations
    fn help_tenants() {
        Self::print_category_header("Multi-Tenancy", "Tenant Isolation and Management");

        println!("\n{}", "Tenant Management:".yellow());
        Self::print_entries(Category::Tenants, "Tenant Management");

        println!("\n{}", "Context Switching:".yellow());
        Self::print_entries(Category::Tenants, "Context Switching");

        println!("\n{}", "Plan Management:".yellow());
        Self::print_entries(Category::Tenants, "Plan Management");
        println!("  {}", "Plans: free, starter, pro, enterprise".dimmed());

        println!("\n{}", "Quota Management:".yellow());
        Self::print_entries(Category::Tenants, "Quota Management");
        println!("  {}", "Arguments: storage_mb, max_connections, max_qps".dimmed());

        println!("\n{}", "Row-Level Security (RLS):".yellow());
        Self::print_entries(Category::Tenants, "Row-Level Security (RLS)");
        println!("  {}", "Commands: ALL, SELECT, INSERT, UPDATE, DELETE".dimmed());

        println!("\n{}", "Change Data Capture (CDC):".yellow());
        Self::print_entries(Category::Tenants, "Change Data Capture (CDC)");

        println!("\n{}", "Tenant Migration:".yellow());
        Self::print_entries(Category::Tenants, "Tenant Migration");

        println!("\n{}", "Isolation Modes:".green().bold());
        println!("  {}", "• SharedSchema - Tables shared with RLS (default)".dimmed());
        println!("  {}", "• DedicatedSchema - Separate schema per tenant".dimmed());
        println!("  {}", "• DedicatedDatabase - Separate database per tenant".dimmed());

        println!("\n{}", "Workflow Example:".green().bold());
        println!("  {}", "-- Create tenant".dimmed());
        println!("  {}", "\\tenant create acme-corp free".dimmed());
        println!("  {}", "-- Switch context".dimmed());
        println!("  {}", "\\tenant use acme-corp".dimmed());
        println!("  {}", "-- All queries now isolated to acme-corp".dimmed());
        println!(
            "  {}",
            "INSERT INTO customers VALUES (1, 'acme-corp', 'Alice', 'alice@acme.com')".dimmed()
        );
        println!("  {}", "SELECT * FROM customers;  -- Only sees acme-corp data".dimmed());
        println!("  {}", "-- Upgrade plan".dimmed());
        println!("  {}", "\\tenant plan acme-corp pro".dimmed());

        println!();
    }

    /// Help for settings and configuration
    fn help_settings() {
        Self::print_category_header("Settings", "Configuration and Tuning");

        println!("\n{}", "View Configuration:".yellow());
        Self::print_entries(Category::Settings, "View Configuration");

        println!("\n{}", "Performance Commands:".yellow());
        Self::print_entries(Category::Settings, "Performance Commands");
        println!(
            "  {}",
            "EXPLAIN shows: plan tree, filter pushdown, index usage, compression info".dimmed()
        );
        println!(
            "  {}",
            "EXPLAIN ANALYZE also shows: actual execution time, row count accuracy".dimmed()
        );
        Self::print_entries(Category::Settings, "Performance Commands #2");

        println!("\n{}", "Statistics:".yellow());
        Self::print_entries(Category::Settings, "Statistics");

        println!();
    }

    /// Help with practical examples
    fn help_examples() {
        Self::print_category_header("Examples", "Practical SQL Examples");

        println!("\n{}", "Vector Search Example:".yellow());
        println!("  {}", "-- Create table with vector column".dimmed());
        println!(
            "  {}",
            "CREATE TABLE products (id INT, name TEXT, embedding VECTOR(384))".dimmed()
        );
        println!("  {}", "-- Insert with embedding".dimmed());
        println!(
            "  {}",
            "INSERT INTO products VALUES (1, 'Widget', '[0.1, 0.2, ...]')".dimmed()
        );
        println!("  {}", "-- Search similar products".dimmed());
        println!(
            "  {}",
            "SELECT * FROM vector_search('products', '[0.5, ...]', 10)".dimmed()
        );

        println!("\n{}", "Time-Travel Analysis:".yellow());
        println!("  {}", "-- Compare data before and after migration".dimmed());
        println!("  {}", "SELECT * FROM users AS OF TIMESTAMP '2024-01-01'".dimmed());
        println!("  {}", "EXCEPT".dimmed());
        println!("  {}", "SELECT * FROM users AS OF TIMESTAMP '2024-02-01'".dimmed());

        println!("\n{}", "Branching Workflow:".yellow());
        println!("  {}", "-- Safe schema changes".dimmed());
        println!("  {}", "CREATE BRANCH schema_migration".dimmed());
        println!("  {}", "\\use schema_migration".dimmed());
        println!("  {}", "ALTER TABLE users ADD COLUMN verified BOOLEAN".dimmed());
        println!("  {}", "-- Test changes...".dimmed());
        println!("  {}", "\\use main".dimmed());
        println!("  {}", "MERGE BRANCH schema_migration INTO main".dimmed());

        println!("\n{}", "Hybrid Search (Fulltext + Vector):".yellow());
        println!("  {}", "WITH text_results AS (".dimmed());
        println!("  {}", "  SELECT id, ts_rank(content_fts, query) as rank".dimmed());
        println!(
            "  {}",
            "  FROM articles, to_tsquery('machine & learning') query".dimmed()
        );
        println!("  {}", "  WHERE content_fts @@ query".dimmed());
        println!("  {}", "),".dimmed());
        println!("  {}", "vector_results AS (".dimmed());
        println!("  {}", "  SELECT * FROM vector_search('articles', $1, 10)".dimmed());
        println!("  {}", ")".dimmed());
        println!("  {}", "SELECT * FROM text_results UNION vector_results".dimmed());

        println!("\n{}", "Document RAG Pattern:".yellow());
        println!("  {}", "-- Search for context".dimmed());
        println!("  {}", "\\search-docs \"How does authentication work?\"".dimmed());
        println!("  {}", "-- Use with agent".dimmed());
        println!("  {}", "\\chat support-bot".dimmed());

        println!();
    }

    /// Help for SQL syntax
    fn help_sql() {
        Self::print_category_header("SQL", "SQL Syntax Reference");

        println!("\n{}", "Data Definition:".yellow());
        println!("  {}", "CREATE TABLE <name> (<columns>)".cyan());
        println!("  {}", "ALTER TABLE <name> ADD/DROP/MODIFY COLUMN ...".cyan());
        println!("  {}", "DROP TABLE <name>".cyan());
        println!("  {}", "CREATE INDEX <name> ON <table>(<columns>)".cyan());

        println!("\n{}", "Data Manipulation:".yellow());
        println!("  {}", "INSERT INTO <table> VALUES (...)".cyan());
        println!("  {}", "UPDATE <table> SET <col>=<val> WHERE ...".cyan());
        println!("  {}", "DELETE FROM <table> WHERE ...".cyan());

        println!("\n{}", "Queries:".yellow());
        println!("  {}", "SELECT <columns> FROM <table> WHERE <condition>".cyan());
        println!("  {}", "SELECT ... JOIN ... ON ...".cyan());
        println!("  {}", "SELECT ... GROUP BY ... HAVING ...".cyan());
        println!("  {}", "SELECT ... ORDER BY ... LIMIT ...".cyan());

        println!("\n{}", "Vector Types:".yellow());
        println!("  {}", "VECTOR(n)        - Fixed-dimension vector".cyan());
        println!("  {}", "embedding VECTOR(384)".dimmed());

        println!("\n{}", "Temporal Queries:".yellow());
        println!("  {}", "AS OF TIMESTAMP <ts>".cyan());
        println!("  {}", "AS OF TRANSACTION <id>".cyan());
        println!("  {}", "BETWEEN TIMESTAMP <t1> AND <t2>".cyan());

        println!("\n{}", "Functions:".yellow());
        println!("  {}", "vector_search(<store>, <vec>, <k>)  - K-NN search".cyan());
        println!("  {}", "cosine_similarity(<v1>, <v2>)       - Vector similarity".cyan());
        println!("  {}", "to_tsquery(<text>)                   - Fulltext query".cyan());
        println!(
            "  {}",
            "ts_rank(<fts>, <query>)              - Relevance ranking".cyan()
        );

        println!("\n{}", "For more examples, use:".green().bold());
        println!("  {}", "\\h examples".dimmed());

        println!();
    }

    /// Print unknown category error
    fn print_unknown_category(cat: &str) {
        println!("\n{} {}", "Unknown help category:".red(), cat.red().bold());
        println!("\n{}", "Available categories:".yellow());
        println!("  {}", "basics, schema, branching, time-travel, vectors,".cyan());
        println!("  {}", "documents, agents, ai, tenants, settings, examples, sql".cyan());
        println!("\n{}", "Usage:".dimmed());
        println!("  {}", "\\h <category>".dimmed());
        println!("  {}", "\\h vectors    - Show vector search help".dimmed());
        println!("  {}", "\\h tenants    - Show multi-tenancy help".dimmed());
        println!();
    }

    // Helper methods for consistent formatting

    fn print_category_header(category: &str, description: &str) {
        let header = format!("╔═══ {} - {} ", category, description);
        let padding = "═".repeat(80_usize.saturating_sub(header.len() + 1));
        println!("\n{}{}{}", header.cyan(), padding.cyan(), "╗".cyan());
    }

    /// The commands the registry lists under one part of a category's help.
    fn print_entries(category: Category, section: &str) {
        for spec in registry::entries(category, section) {
            Self::print_command(&spec.display_form(), spec.summary);
            if spec.show_examples {
                for example in spec.examples {
                    Self::print_example(example);
                }
            }
        }
    }

    fn print_command(cmd: &str, desc: &str) {
        println!("  {:<30} {}", cmd.cyan(), desc);
    }

    fn print_example(example: &str) {
        println!("    {:<28} {}", "", format!("Example: {}", example).dimmed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_terminal_width_detection() {
        let width = HelpManager::get_terminal_width();
        assert!(width >= 80, "Terminal width should be at least 80");
    }

    #[test]
    fn test_category_matching() {
        // Test case-insensitive matching
        let categories = vec![
            ("basics", true),
            ("BASICS", true),
            ("Basic", true),
            ("schema", true),
            ("branching", true),
            ("branches", true),
            ("time-travel", true),
            ("timetravel", true),
            ("vectors", true),
            ("vector", true),
            ("documents", true),
            ("docs", true),
            ("agents", true),
            ("sessions", true),
            ("ai", true),
            ("tenants", true),
            ("tenant", true),
            ("multi-tenancy", true),
            ("multitenancy", true),
            ("settings", true),
            ("config", true),
            ("examples", true),
            ("sql", true),
            ("invalid", false),
        ];

        for (cat, _should_match) in categories {
            // Just ensure no panic occurs
            HelpManager::print_help_category(cat);
        }
    }

    #[test]
    fn test_help_main_renders() {
        // Ensure main help renders without panic
        HelpManager::print_help_main();
    }

    #[test]
    fn test_all_categories_render() {
        let categories = vec![
            "basics",
            "schema",
            "branching",
            "time-travel",
            "vectors",
            "documents",
            "agents",
            "ai",
            "tenants",
            "settings",
            "examples",
            "sql",
        ];

        for cat in categories {
            HelpManager::print_help_category(cat);
        }
    }
}
