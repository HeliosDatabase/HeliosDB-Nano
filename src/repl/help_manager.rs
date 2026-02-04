//! Help system for HeliosDB Lite REPL
//!
//! Provides feature-based help categories with responsive terminal layout.
//! Automatically detects terminal width and adjusts layout accordingly.

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
        terminal_size::terminal_size()
            .map(|(w, _)| w.0 as usize)
            .unwrap_or(80)
    }

    /// Print 2-column layout for narrow terminals (< 120 cols)
    fn print_help_2col() {
        println!("\n{}", "╔══════════════════════════════════════════════════════════════════════════════╗".cyan());
        println!("{}", "║              HeliosDB Lite - Interactive SQL REPL Help                      ║".cyan().bold());
        println!("{}", "╚══════════════════════════════════════════════════════════════════════════════╝".cyan());

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
        println!("{}", "║                             HeliosDB Lite - Interactive SQL REPL Help                                           ║".cyan().bold());
        println!("{}", "╚══════════════════════════════════════════════════════════════════════════════════════════════════════════════════╝".cyan());

        println!("\n{}", "Quick Start:".yellow().bold());
        println!("  {}  Show this help  │  {}  Show category help  │  {}  Exit REPL",
                 "\\h".cyan(), "\\h <category>".cyan(), "\\q".cyan());

        println!("\n{}", "Help Categories:".yellow().bold());
        println!("  {}  REPL basics       │  {}  Schema exploration  │  {}  Branch workflows",
                 "basics".cyan(), "schema".cyan(), "branching".cyan());
        println!("  {}  Time-travel queries  │  {}  Vector search      │  {}  Document storage",
                 "time-travel".cyan(), "vectors".cyan(), "documents".cyan());
        println!("  {}  AI agents        │  {}  AI inference       │  {}  Multi-tenancy",
                 "agents".cyan(), "ai".cyan(), "tenants".cyan());
        println!("  {}  Configuration    │  {}  SQL examples      │  {}  SQL syntax",
                 "settings".cyan(), "examples".cyan(), "sql".cyan());

        println!("\n{}", "Usage:".dimmed());
        println!("  {}  {}  {}",
                 "\\h vectors".dimmed(),
                 "- Show vector search help".dimmed(),
                 "│  \\h branching - Show branching help".dimmed());
        println!();
    }

    /// Help for basic REPL operations
    fn help_basics() {
        Self::print_category_header("Basics", "Fundamental REPL Operations");

        println!("\n{}", "Exit Commands:".yellow());
        Self::print_command("\\q, \\quit, \\exit", "Quit REPL");
        Self::print_example("\\q");

        println!("\n{}", "Help Commands:".yellow());
        Self::print_command("\\h, \\help, \\?", "Show help menu");
        Self::print_command("\\h <category>", "Show category-specific help");
        Self::print_example("\\h vectors");
        Self::print_example("\\h branching");

        println!("\n{}", "Display Toggles:".yellow());
        Self::print_command("\\timing", "Toggle query execution timing");
        Self::print_example("\\timing");
        Self::print_command("\\show lsn", "Toggle LSN (Log Sequence Number) display");
        Self::print_command("\\show branch", "Show current active branch");

        println!("\n{}", "Information:".yellow());
        Self::print_command("\\version", "Display HeliosDB version");
        Self::print_command("\\status", "Show database status");

        println!();
    }

    /// Help for schema exploration
    fn help_schema() {
        Self::print_category_header("Schema", "Table and View Exploration");

        println!("\n{}", "List Objects:".yellow());
        Self::print_command("\\d", "List all tables");
        Self::print_command("\\dt", "List tables only");
        Self::print_command("\\dv", "List views only");
        Self::print_command("\\di", "List indexes");

        println!("\n{}", "Describe Objects:".yellow());
        Self::print_command("\\d <table>", "Describe table schema");
        Self::print_example("\\d users");
        Self::print_command("\\dS <table>", "Show detailed table statistics");
        Self::print_example("\\dS orders");

        println!("\n{}", "Compression:".yellow());
        Self::print_command("\\compression <table>", "Show compression info for table");
        Self::print_example("\\compression events");
        Self::print_command("\\compression", "Show compression for all tables");

        println!("\n{}", "Indexes:".yellow());
        Self::print_command("\\di <table>", "Show indexes for specific table");
        Self::print_example("\\di products");

        println!();
    }

    /// Help for database branching
    fn help_branching() {
        Self::print_category_header("Branching", "Database Branching Workflows");

        println!("\n{}", "List and Switch:".yellow());
        Self::print_command("\\branches", "List all branches");
        Self::print_command("\\use <branch>", "Switch to branch");
        Self::print_example("\\use dev");
        Self::print_command("\\show branch", "Show current branch");

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
        println!("  {}", "(Coming in v3.2.0 - Named snapshots for point-in-time recovery)".dimmed());
        Self::print_command("\\snapshots", "List available snapshots (v3.2.0+)");
        Self::print_command("CREATE SNAPSHOT <name> [AT TIMESTAMP <ts>]", "Create named snapshot (v3.2.0+)");
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
        Self::print_command("\\vectors", "List all vector stores");
        Self::print_command("\\vector <name>", "Show store details");
        Self::print_example("\\vector embeddings");
        Self::print_command("\\vector create <name> <dims> [metric]", "Create new store");
        Self::print_example("\\vector create mystore 384 cosine");
        Self::print_command("\\vector delete <name>", "Delete vector store");
        Self::print_example("\\vector delete old_embeddings");
        Self::print_command("\\vector stats <name>", "Show detailed statistics");
        Self::print_example("\\vector stats embeddings");

        println!("\n{}", "SQL Vector Operations:".yellow());
        Self::print_command("CREATE TABLE t (embedding VECTOR(384))", "Define vector column");
        Self::print_command("CREATE INDEX idx ON t USING hnsw(embedding)", "Create HNSW index");
        Self::print_command("SELECT * FROM t ORDER BY embedding <-> query LIMIT 10", "K-NN search");
        println!("  {}", "Operators: <-> (L2), <#> (inner product), <=> (cosine)".dimmed());

        println!("\n{}", "Search:".yellow());
        Self::print_command("\\vector search <store> <query>", "Search vectors");
        Self::print_example("\\vector search embeddings [0.1, 0.2, ..., 0.9]");

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
        Self::print_command("\\collections", "List all collections");
        Self::print_command("\\collection <name>", "Show collection details");
        Self::print_example("\\collection articles");
        Self::print_command("\\docs <collection>", "List documents");
        Self::print_example("\\docs my-rag-docs");
        Self::print_command("\\doc <coll> <id>", "Get document details");
        Self::print_example("\\doc articles doc-123");
        Self::print_command("\\search-docs <query>", "Search across collections");
        Self::print_example("\\search-docs machine learning");

        println!("\n{}", "Import/Export:".yellow());
        Self::print_command("\\import-docs <file>", "Import documents from JSON/JSONL");
        Self::print_example("\\import-docs data/articles.jsonl");
        Self::print_command("\\export-docs <coll> <file>", "Export collection to file");

        println!("\n{}", "SQL Integration:".yellow());
        Self::print_command("INSERT INTO documents (collection, data) VALUES (...)", "Add document");
        Self::print_example("INSERT INTO documents (collection, data) VALUES ('articles', '{\"title\": \"...\"}'::json)");

        println!("\n{}", "Advanced Document Operations:".yellow());
        Self::print_command("\\doc chunks <collection> <id>", "Show document chunks");
        Self::print_example("\\doc chunks articles doc-123");
        Self::print_command("\\doc rechunk <coll> <id> <size>", "Re-chunk document");
        Self::print_example("\\doc rechunk articles doc-123 512");
        Self::print_command("\\rag <collection> <query> [k]", "RAG-style search with context");
        Self::print_example("\\rag articles \"What is machine learning?\" 5");

        println!("\n{}", "RAG Workflow:".green().bold());
        println!("  {}", "1. Import documents: \\import-docs data.jsonl".dimmed());
        println!("  {}", "2. Search semantically: \\search-docs \"query\"".dimmed());
        println!("  {}", "3. RAG search for context: \\rag <coll> <query>".dimmed());
        println!("  {}", "4. Use with AI agents for context-aware responses".dimmed());

        println!();
    }

    /// Help for AI agent operations
    fn help_agents() {
        Self::print_category_header("Agents", "AI Agent Sessions");

        println!("\n{}", "Session Commands:".yellow());
        Self::print_command("\\sessions", "List all agent sessions");
        Self::print_command("\\session-new <name>", "Create new session");
        Self::print_example("\\session-new my-chat");
        Self::print_command("\\session <id>", "Show session details");
        Self::print_example("\\session chat-1");
        Self::print_command("\\session-delete <id>", "Delete session");
        Self::print_example("\\session-delete old-chat");
        Self::print_command("\\chat <id>", "Interactive chat mode");
        Self::print_example("\\chat abc123");
        println!("  {}", "Type 'exit' or 'quit' to end chat".dimmed());
        Self::print_command("\\session-clear <id>", "Clear session messages");
        Self::print_example("\\session-clear chat-1");

        println!("\n{}", "Advanced Session Operations:".yellow());
        Self::print_command("\\session fork <id> <name>", "Fork session with history");
        Self::print_example("\\session fork abc123 new-branch");
        Self::print_command("\\session context <id>", "Show session context/state");
        Self::print_example("\\session context abc123");
        Self::print_command("\\session memory <id> <query>", "Semantic search in session");
        Self::print_example("\\session memory abc123 \"database optimization\"");
        Self::print_command("\\session summarize <id>", "Generate session summary");
        Self::print_example("\\session summarize abc123");

        println!("\n{}", "Agent Configuration:".yellow());
        Self::print_command("\\session-config <id> <key> <value>", "Configure session");
        Self::print_example("\\session-config analytics model gpt-4");
        Self::print_example("\\session-config analytics temperature 0.7");

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
        Self::print_command("\\ai templates", "List available AI templates");
        Self::print_command("\\ai template <name>", "Show template details");
        Self::print_example("\\ai template summarize");

        println!("\n{}", "Inference:".yellow());
        Self::print_command("\\ai infer <prompt>", "Run AI inference");
        Self::print_example("\\ai infer Explain vector databases in simple terms");
        Self::print_command("\\ai infer -t <template> <input>", "Use template");
        Self::print_example("\\ai infer -t summarize \"Long text here...\"");

        println!("\n{}", "Generation:".yellow());
        Self::print_command("\\ai generate schema <description>", "Generate schema from description");
        Self::print_example("\\ai generate schema User table with email and profile");
        Self::print_command("\\ai generate query <question>", "Generate SQL from question");
        Self::print_example("\\ai generate query What are top 10 users by activity?");

        println!("\n{}", "Embeddings & Models:".yellow());
        Self::print_command("\\ai models", "List available AI models");
        Self::print_command("\\ai embed <text>", "Generate embedding for text");
        Self::print_example("\\ai embed \"Hello world\"");
        Self::print_command("\\ai compare-schema <s1> <s2>", "Compare two table schemas");
        Self::print_example("\\ai compare-schema users_v1 users_v2");

        println!("\n{}", "Chat Mode:".yellow());
        Self::print_command("\\ai chat", "Start AI chat session");
        println!("  {}", "Interactive mode for multi-turn conversations".dimmed());

        println!("\n{}", "Configuration:".yellow());
        Self::print_command("\\ai config model <name>", "Set AI model");
        Self::print_example("\\ai config model gpt-4");
        Self::print_command("\\ai config temperature <value>", "Set temperature (0.0-2.0)");
        Self::print_example("\\ai config temperature 0.7");

        println!();
    }

    /// Help for multi-tenancy operations
    fn help_tenants() {
        Self::print_category_header("Multi-Tenancy", "Tenant Isolation and Management");

        println!("\n{}", "Tenant Management:".yellow());
        Self::print_command("\\tenant list", "List all tenants");
        Self::print_command("\\tenant create <name> [plan]", "Create new tenant");
        Self::print_example("\\tenant create acme-corp free");
        Self::print_example("\\tenant create big-co enterprise");
        Self::print_command("\\tenant delete <tenant>", "Delete tenant");
        Self::print_example("\\tenant delete old-tenant");
        Self::print_command("\\tenant info <tenant>", "Show tenant details");
        Self::print_example("\\tenant info acme-corp");

        println!("\n{}", "Context Switching:".yellow());
        Self::print_command("\\tenant use <tenant>", "Switch to tenant context");
        Self::print_example("\\tenant use acme-corp");
        Self::print_command("\\tenant current", "Show current tenant");
        Self::print_command("\\tenant clear", "Clear tenant context (admin mode)");

        println!("\n{}", "Plan Management:".yellow());
        Self::print_command("\\tenant plans", "List available plans");
        Self::print_command("\\tenant plan <tenant> <plan>", "Change tenant plan");
        Self::print_example("\\tenant plan acme-corp pro");
        println!("  {}", "Plans: free, starter, pro, enterprise".dimmed());

        println!("\n{}", "Quota Management:".yellow());
        Self::print_command("\\tenant quota <tenant>", "Show quota usage");
        Self::print_example("\\tenant quota acme-corp");
        Self::print_command("\\tenant quota-set <tenant> <storage_mb> <conns> <qps>", "Set custom quotas");
        Self::print_example("\\tenant quota-set acme-corp 5000 25 10000");
        println!("  {}", "Arguments: storage_mb, max_connections, max_qps".dimmed());

        println!("\n{}", "Row-Level Security (RLS):".yellow());
        Self::print_command("\\tenant rls create <table> <policy> <expr> <cmd>", "Create RLS policy");
        Self::print_example("\\tenant rls create customers isolation tenant_id=current_tenant() ALL");
        Self::print_command("\\tenant rls list <table>", "List policies for table");
        Self::print_example("\\tenant rls list customers");
        Self::print_command("\\tenant rls delete <table> <policy>", "Delete RLS policy");
        Self::print_example("\\tenant rls delete customers isolation");
        println!("  {}", "Commands: ALL, SELECT, INSERT, UPDATE, DELETE".dimmed());

        println!("\n{}", "Change Data Capture (CDC):".yellow());
        Self::print_command("\\tenant cdc-show [limit]", "Show CDC events");
        Self::print_example("\\tenant cdc-show 20");
        Self::print_command("\\tenant cdc-export <file>", "Export CDC to JSON");
        Self::print_example("\\tenant cdc-export changes.json");

        println!("\n{}", "Tenant Migration:".yellow());
        Self::print_command("\\tenant migrate-to <target>", "Start migration to target tenant");
        Self::print_example("\\tenant migrate-to new-tenant");
        Self::print_command("\\tenant migrate-status [tenant]", "Check migration status");
        Self::print_example("\\tenant migrate-status acme-corp");

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
        println!("  {}", "INSERT INTO customers VALUES (1, 'acme-corp', 'Alice', 'alice@acme.com')".dimmed());
        println!("  {}", "SELECT * FROM customers;  -- Only sees acme-corp data".dimmed());
        println!("  {}", "-- Upgrade plan".dimmed());
        println!("  {}", "\\tenant plan acme-corp pro".dimmed());

        println!();
    }

    /// Help for settings and configuration
    fn help_settings() {
        Self::print_category_header("Settings", "Configuration and Tuning");

        println!("\n{}", "View Configuration:".yellow());
        Self::print_command("\\config", "Show all configuration settings");
        Self::print_command("\\config <key>", "Show specific setting");
        Self::print_example("\\config max_connections");

        println!("\n{}", "Modify Settings:".yellow());
        Self::print_command("\\set <key> <value>", "Set configuration value");
        Self::print_example("\\set max_connections 100");
        Self::print_example("\\set cache_size 512MB");

        println!("\n{}", "Performance Commands:".yellow());
        Self::print_command("\\explain <query>", "Show query execution plan with storage layer analysis");
        Self::print_example("\\explain SELECT * FROM users WHERE id = 1");
        Self::print_command("\\explain analyze <query>", "Execute query and show actual timing and row counts");
        Self::print_example("\\explain analyze SELECT * FROM users");
        println!("  {}", "EXPLAIN shows: plan tree, filter pushdown, index usage, compression info".dimmed());
        println!("  {}", "EXPLAIN ANALYZE also shows: actual execution time, row count accuracy".dimmed());
        Self::print_command("\\profile <query>", "Profile query with timing");
        Self::print_example("\\profile SELECT * FROM orders WHERE total > 1000");
        Self::print_command("\\telemetry", "Show database statistics");
        Self::print_command("\\optimize", "Run query optimizer analysis");
        Self::print_command("\\optimize <table>", "Optimize specific table");
        Self::print_example("\\optimize users");
        Self::print_command("\\analyze", "Update table statistics");
        Self::print_command("\\vacuum", "Reclaim storage space");

        println!("\n{}", "Statistics:".yellow());
        Self::print_command("\\stats", "Show database statistics");
        Self::print_command("\\stats queries", "Show query performance stats");
        Self::print_command("\\stats cache", "Show cache statistics");
        Self::print_command("\\stats io", "Show I/O statistics");

        println!("\n{}", "Common Settings:".green().bold());
        println!("  {}", "• max_connections    - Connection limit".dimmed());
        println!("  {}", "• cache_size         - Memory cache size".dimmed());
        println!("  {}", "• wal_level          - Write-ahead log level".dimmed());
        println!("  {}", "• compression_level  - Data compression (0-9)".dimmed());

        println!();
    }

    /// Help with practical examples
    fn help_examples() {
        Self::print_category_header("Examples", "Practical SQL Examples");

        println!("\n{}", "Vector Search Example:".yellow());
        println!("  {}", "-- Create table with vector column".dimmed());
        println!("  {}", "CREATE TABLE products (id INT, name TEXT, embedding VECTOR(384))".dimmed());
        println!("  {}", "-- Insert with embedding".dimmed());
        println!("  {}", "INSERT INTO products VALUES (1, 'Widget', '[0.1, 0.2, ...]')".dimmed());
        println!("  {}", "-- Search similar products".dimmed());
        println!("  {}", "SELECT * FROM vector_search('products', '[0.5, ...]', 10)".dimmed());

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
        println!("  {}", "  FROM articles, to_tsquery('machine & learning') query".dimmed());
        println!("  {}", "  WHERE content_fts @@ query".dimmed());
        println!("  {}", "),".dimmed());
        println!("  {}", "vector_results AS (".dimmed());
        println!("  {}", "  SELECT * FROM vector_search('articles', $1, 10)".dimmed());
        println!("  {}", ")".dimmed());
        println!("  {}", "SELECT * FROM text_results UNION vector_results".dimmed());

        println!("\n{}", "Document RAG Pattern:".yellow());
        println!("  {}", "-- Import knowledge base".dimmed());
        println!("  {}", "\\import-docs knowledge/docs.jsonl".dimmed());
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
        println!("  {}", "ts_rank(<fts>, <query>)              - Relevance ranking".cyan());

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
            "basics", "schema", "branching", "time-travel", "vectors",
            "documents", "agents", "ai", "tenants", "settings", "examples", "sql",
        ];

        for cat in categories {
            HelpManager::print_help_category(cat);
        }
    }
}
