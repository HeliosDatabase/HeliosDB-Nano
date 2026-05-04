//! REPL shell implementation

use crate::{EmbeddedDatabase, Result, Error};
use super::{ReplConfig, MetaCommand, commands::MetaCommandResult};
use super::completer::SqlCompleter;
use super::formatter;
use rustyline::error::ReadlineError;
use rustyline::Editor;
use rustyline::Helper;
use rustyline::validate::Validator;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use colored::Colorize;
use std::time::Instant;

/// REPL shell
pub struct ReplShell {
    db: EmbeddedDatabase,
    config: ReplConfig,
    editor: Editor<ReplHelper, FileHistory>,
    show_timing: bool,
    current_branch: String,
    show_lsn: bool,
}

/// Helper for rustyline
struct ReplHelper {
    completer: SqlCompleter,
}

impl Helper for ReplHelper {}
impl Validator for ReplHelper {}
impl Highlighter for ReplHelper {}
impl Hinter for ReplHelper {
    type Hint = String;
}

impl rustyline::completion::Completer for ReplHelper {
    type Candidate = rustyline::completion::Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        self.completer.complete(line, pos, ctx)
    }
}

impl ReplShell {
    /// Get a reference to the database
    ///
    /// Useful for dump on shutdown functionality
    pub fn db(&self) -> &EmbeddedDatabase {
        &self.db
    }

    /// Strip SQL comments from input
    /// Handles both line comments (-- ...) and block comments (/* ... */)
    #[allow(clippy::indexing_slicing)]
    // SAFETY: All `chars[i]` and `chars[i+1]` accesses are guarded by `i < chars.len()` and `i+1 < chars.len()`
    fn strip_sql_comments(sql: &str) -> String {
        let mut result = String::with_capacity(sql.len());
        let chars: Vec<char> = sql.chars().collect();
        let mut i = 0;
        let mut in_single_quote = false;
        let mut in_double_quote = false;

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

    /// Create a new REPL shell
    pub fn new(db: EmbeddedDatabase, config: ReplConfig) -> Result<Self> {
        let mut completer = SqlCompleter::new();

        // Load table names for completion
        if let Ok(tables) = db.storage.catalog().list_tables() {
            completer.set_table_names(tables);
        }

        let helper = ReplHelper { completer };

        let mut editor = Editor::new()
            .map_err(|e| Error::Generic(format!("Failed to create editor: {}", e)))?;
        editor.set_helper(Some(helper));

        // Load history
        if let Some(history_path) = &config.history_path {
            let _ = editor.load_history(history_path);
        }

        let show_timing = config.show_timing;

        Ok(Self {
            db,
            config,
            editor,
            show_timing,
            current_branch: "main".to_string(),
            show_lsn: false,
        })
    }

    /// Run the REPL
    pub fn run(&mut self) -> Result<()> {
        self.print_banner();

        let mut multi_line_buffer = String::new();

        loop {
            // Determine prompt based on whether we're in multi-line mode
            let prompt = if multi_line_buffer.is_empty() {
                format!(
                    "{} {} ",
                    "heliosdb".green().bold(),
                    format!("[{}]", &self.current_branch).cyan()
                ) + ">"
            } else {
                "       -> ".yellow().to_string()
            };

            // Read line
            match self.editor.readline(&prompt) {
                Ok(line) => {
                    let trimmed = line.trim();

                    // Skip empty lines
                    if trimmed.is_empty() {
                        continue;
                    }

                    // Add to history
                    let _ = self.editor.add_history_entry(&line);

                    // Check for meta commands (only when not in multi-line mode)
                    if multi_line_buffer.is_empty() && trimmed.starts_with('\\') {
                        if let Some(meta_cmd) = MetaCommand::parse(trimmed) {
                            match meta_cmd.execute(&self.db, self.show_timing, Some(&self.config)) {
                                Ok(MetaCommandResult::Quit) => {
                                    println!("Goodbye!");
                                    break;
                                }
                                Ok(MetaCommandResult::ToggleTiming(new_state)) => {
                                    self.show_timing = new_state;
                                }
                                Ok(MetaCommandResult::SwitchBranch(branch_name)) => {
                                    self.current_branch = branch_name.clone();
                                    // Sync branch context to storage layer
                                    self.db.storage.set_current_branch(Some(branch_name.clone()));

                                    // Branch data isolation is now active
                                    if branch_name != "main" {
                                        println!("{}", format!("Switched to branch '{}' with data isolation enabled.", branch_name).green());
                                        println!("{}", "  Data changes on this branch are isolated from main.".dimmed());
                                    }
                                }
                                Ok(MetaCommandResult::ToggleLsn(_)) => {
                                    self.show_lsn = !self.show_lsn;
                                }
                                Ok(MetaCommandResult::ConfigReloaded(new_config)) => {
                                    // Apply the new configuration
                                    self.show_timing = new_config.show_timing;
                                    self.config = new_config;
                                }
                                Ok(MetaCommandResult::Continue) => {}
                                Err(e) => {
                                    eprintln!("{}", formatter::format_error(&e.to_string()));
                                }
                            }
                            continue;
                        } else {
                            eprintln!("{}", formatter::format_error(&format!(
                                "Unknown meta command: {}. Type \\h for help.",
                                trimmed
                            )));
                            continue;
                        }
                    }

                    // Strip comments from the line for proper semicolon detection
                    let stripped_line = Self::strip_sql_comments(trimmed);
                    let stripped_trimmed = stripped_line.trim();

                    // Skip comment-only lines entirely - don't add to buffer
                    if stripped_trimmed.is_empty() {
                        continue;
                    }

                    // Add stripped content to multi-line buffer
                    if !multi_line_buffer.is_empty() {
                        multi_line_buffer.push('\n');
                    }
                    multi_line_buffer.push_str(stripped_trimmed);

                    // Check if statement is complete (ends with semicolon after stripping comments)
                    if stripped_trimmed.ends_with(';') {
                        // Execute the statement
                        self.execute_sql(&multi_line_buffer);

                        // Clear buffer
                        multi_line_buffer.clear();

                        // Update table names for completion
                        if let Ok(tables) = self.db.storage.catalog().list_tables() {
                            if let Some(helper) = self.editor.helper_mut() {
                                helper.completer.set_table_names(tables);
                            }
                        }
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    // Ctrl-C - clear multi-line buffer
                    if !multi_line_buffer.is_empty() {
                        println!("^C");
                        multi_line_buffer.clear();
                    } else {
                        println!("Use \\q or Ctrl-D to exit");
                    }
                }
                Err(ReadlineError::Eof) => {
                    // Ctrl-D - exit
                    println!("Goodbye!");
                    break;
                }
                Err(err) => {
                    eprintln!("{}", formatter::format_error(&format!("Error: {}", err)));
                    break;
                }
            }
        }

        // Save history
        if let Some(history_path) = &self.config.history_path {
            let _ = self.editor.save_history(history_path);
        }

        Ok(())
    }

    /// Execute a SQL statement (expects comments already stripped)
    fn execute_sql(&mut self, sql: &str) {
        // Skip whitespace-only input (comments already stripped before calling this)
        if sql.trim().is_empty() {
            return;
        }

        let start = Instant::now();

        // Check for USE BRANCH statement
        if crate::sql::Parser::is_use_branch(sql) {
            match crate::sql::Parser::parse_use_branch_sql(sql) {
                Ok(branch_name) => {
                    self.current_branch = branch_name.clone();
                    // Sync branch context to storage layer
                    self.db.storage.set_current_branch(Some(branch_name.clone()));
                    println!("{}", format!("Switched to branch: {}", branch_name).green());

                    // Show isolation status
                    if branch_name != "main" {
                        println!("{}", "  Data isolation enabled for this branch.".dimmed());
                    }

                    if self.show_timing {
                        println!("{}", formatter::format_timing(start.elapsed().as_secs_f64()));
                    }
                    return;
                }
                Err(e) => {
                    eprintln!("{}", formatter::format_error(&e.to_string()));
                    return;
                }
            }
        }

        // Try to determine if this is a query (SELECT) or a command (INSERT, UPDATE, etc.)
        // Also treat UPDATE/DELETE/INSERT with RETURNING as queries.
        // EXPLAIN also returns results, so treat it as a query.
        // PRAGMA table_info(...) returns rows; route it through the query path.
        let sql_upper = sql.trim().to_uppercase();
        let has_returning = sql_upper.contains(" RETURNING ");
        let is_query = sql_upper.starts_with("SELECT")
            || sql_upper.starts_with("WITH")
            || sql_upper.starts_with("TABLE")
            || sql_upper.starts_with("VALUES")
            || sql_upper.starts_with("EXPLAIN")
            || sql_upper.starts_with("PRAGMA")
            // SHOW BRANCHES / SHOW DATABASE BRANCHES / SHOW <var> all
            // produce result rows; without this the executor still
            // returns the tuples but `db.execute()` discards them and
            // only the row count surfaces ("Query OK, N row(s) affected").
            || sql_upper.starts_with("SHOW")
            || has_returning;

        if is_query {
            // PRAGMA short-circuit: sqlparser doesn't recognise PRAGMA, so
            // we synthesise a fixed schema for `table_info(t)` and an empty
            // result schema for connection-tunable PRAGMAs (foreign_keys,
            // journal_mode, …) — everything still goes through `db.query`.
            if sql_upper.starts_with("PRAGMA") {
                if let Some((name, _arg)) = crate::sql::sqlite_compat::parse_pragma(sql) {
                    let schema = if name.eq_ignore_ascii_case("table_info") {
                        crate::Schema::new(vec![
                            crate::Column::new("cid", crate::DataType::Int4),
                            crate::Column::new("name", crate::DataType::Text),
                            crate::Column::new("type", crate::DataType::Text),
                            crate::Column::new("notnull", crate::DataType::Int4),
                            crate::Column::new("dflt_value", crate::DataType::Text),
                            crate::Column::new("pk", crate::DataType::Int4),
                        ])
                    } else {
                        crate::Schema::new(vec![])
                    };
                    match self.db.query(sql, &[]) {
                        Ok(results) => {
                            let duration = start.elapsed();
                            println!("\n{}", formatter::format_results(&results, &schema));
                            if self.show_timing {
                                println!("{}", formatter::format_timing(duration.as_secs_f64()));
                            }
                        }
                        Err(e) => eprintln!("{}", formatter::format_error(&e.to_string())),
                    }
                    return;
                }
            }

            // Execute query
            match self.db.query(sql, &[]) {
                Ok(results) => {
                    let duration = start.elapsed();

                    // Get schema from the first tuple if available
                    if let Ok(parser) = crate::sql::Parser::new().parse_one(sql) {
                        let catalog = self.db.storage.catalog();
                        let planner = crate::sql::Planner::with_catalog(&catalog);
                        if let Ok(plan) = planner.statement_to_plan(parser) {
                            let schema = plan.schema();
                            println!("\n{}", formatter::format_results(&results, &schema));
                            if self.show_timing {
                                println!("{}", formatter::format_timing(duration.as_secs_f64()));
                            }
                            if self.show_lsn {
                                if let Some(lsn) = self.db.current_lsn() {
                                    println!("{}", format!("LSN: {}", lsn).dimmed());
                                }
                            }
                            return;
                        }
                    }

                    // Fallback: just show tuple count
                    println!("{}", format!("Query returned {} row(s)", results.len()).dimmed());
                    if self.show_timing {
                        println!("{}", formatter::format_timing(duration.as_secs_f64()));
                    }
                    if self.show_lsn {
                        if let Some(lsn) = self.db.current_lsn() {
                            println!("{}", format!("LSN: {}", lsn).dimmed());
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{}", formatter::format_error(&e.to_string()));
                }
            }
        } else {
            // Execute command
            match self.db.execute(sql) {
                Ok(affected) => {
                    let duration = start.elapsed();

                    let message = if sql_upper.starts_with("CREATE") {
                        "Query OK".to_string()
                    } else if sql_upper.starts_with("DROP") {
                        "Query OK".to_string()
                    } else {
                        format!("Query OK, {} row(s) affected", affected)
                    };

                    println!("{}", message.green());
                    if self.show_timing {
                        println!("{}", formatter::format_timing(duration.as_secs_f64()));
                    }
                    if self.show_lsn {
                        if let Some(lsn) = self.db.current_lsn() {
                            println!("{}", format!("LSN: {}", lsn).dimmed());
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{}", formatter::format_error(&e.to_string()));
                }
            }
        }
    }

    /// Print welcome banner
    fn print_banner(&self) {
        println!();
        println!("╔═══════════════════════════════════════════════════════════════╗");
        println!("║  {} {}{}",
            "HeliosDB Nano".bold().cyan(),
            format!("v{}", env!("CARGO_PKG_VERSION")).bold().green(),
            "                                    ║"
        );
        println!("║  {}  ║", "PostgreSQL-compatible database with enterprise features".dimmed());
        println!("╚═══════════════════════════════════════════════════════════════╝");
        println!();

        // Current version features
        println!("{}", "Key Features:".bold());
        println!("  {} PostgreSQL wire protocol compatible", "•".cyan());
        println!("  {} Multi-tenancy with Row-Level Security (RLS)", "•".cyan());
        println!("  {} Change Data Capture (CDC) for migrations", "•".cyan());
        println!("  {} Database branching & time-travel queries", "•".cyan());
        println!("  {} Vector search with Product Quantization", "•".cyan());
        println!("  {} Encryption at rest (AES-256-GCM)", "•".cyan());
        println!();

        println!("Mode: {} (single-user, direct access)", "REPL".bold().yellow());
        println!("      For multi-user access, use: {} {}", "heliosdb-nano start".cyan(), "--help".dimmed());
        println!();

        println!("Commands: {} help | {} list tables | {} system views | {} quit",
            "\\h".cyan(), "\\d".cyan(), "\\dS".cyan(), "\\q".cyan());
        println!();
    }
}
