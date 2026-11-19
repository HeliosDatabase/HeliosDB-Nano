//! REPL shell implementation

use super::completer::SqlCompleter;
use super::display::{self, DisplaySettings, Terminal};
use super::formatter;
use super::lexer::StatementScanner;
use super::{commands::MetaCommandResult, MetaCommand, ReplConfig};
use crate::{EmbeddedDatabase, Error, Result, StatementOutcome};
use colored::Colorize;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::Validator;
use rustyline::Editor;
use rustyline::Helper;
use std::io::IsTerminal;
use std::time::Instant;

/// REPL shell
pub struct ReplShell {
    db: EmbeddedDatabase,
    config: ReplConfig,
    editor: Editor<ReplHelper, FileHistory>,
    show_timing: bool,
    show_lsn: bool,
    /// Print no banner and no farewell (`repl -q`).
    quiet: bool,
    /// `\x`, `\pset`, `\t`, `\a`: how rows are drawn.
    display: DisplaySettings,
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

    /// Create a new REPL shell
    pub fn new(db: EmbeddedDatabase, config: ReplConfig) -> Result<Self> {
        let mut completer = SqlCompleter::new();

        // Load table names for completion
        if let Ok(tables) = db.storage.catalog().list_tables() {
            completer.set_table_names(tables);
        }

        let helper = ReplHelper { completer };

        let mut editor = Editor::new().map_err(|e| Error::Generic(format!("Failed to create editor: {}", e)))?;
        editor.set_helper(Some(helper));

        // Load history
        if let Some(history_path) = &config.history_path {
            let _ = editor.load_history(history_path);
        }

        let show_timing = config.show_timing;
        let display = DisplaySettings::from_config(&config);

        Ok(Self {
            db,
            config,
            editor,
            show_timing,
            show_lsn: false,
            quiet: false,
            display,
        })
    }

    /// Print no banner and no farewell.
    pub fn set_quiet(&mut self, quiet: bool) {
        self.quiet = quiet;
    }

    /// Run the REPL
    pub fn run(&mut self) -> Result<()> {
        if !self.quiet {
            self.print_banner();
        }

        // One scanner reads the terminal and the pipe alike: it keeps the raw
        // lines of the open statement, cuts them at the semicolons outside a
        // quote, dollar quote or comment, and runs every statement it completes.
        let mut scanner = StatementScanner::new();
        let interactive = std::io::stdin().is_terminal();

        loop {
            // Determine prompt based on whether a statement is open
            let prompt = if !scanner.is_open() {
                format!(
                    "{} {} ",
                    "heliosdb".green().bold(),
                    // sprinter 6bc4a4040eb7 (B1): read the REPL session's
                    // binding every prompt — `SET heliosdb.branch` or a USE
                    // inside a script moves it too, so the shell keeps no copy.
                    format!("[{}]", self.db.current_branch()).cyan()
                ) + ">"
            } else {
                // The prompt names the open construct: `->` a statement,
                // `'>` a string, `$>` a dollar quote, `*>` a comment.
                format!("       {} ", scanner.continuation_prompt())
                    .yellow()
                    .to_string()
            };

            // Read line
            match self.editor.readline(&prompt) {
                Ok(line) => {
                    let trimmed = line.trim();

                    if !scanner.is_open() {
                        // Skip empty lines
                        if trimmed.is_empty() {
                            continue;
                        }

                        // Meta commands are recognised only when no statement is open
                        if trimmed.starts_with('\\') {
                            scanner.reset();
                            if interactive {
                                let _ = self.editor.add_history_entry(&line);
                            }
                            if self.run_meta_command(trimmed) {
                                break;
                            }
                            continue;
                        }
                    }

                    // The raw line, newline included, goes to the scanner:
                    // whitespace inside a literal is part of the statement.
                    let completed = scanner.feed(&format!("{line}\n"));
                    if !scanner.is_open() {
                        // Only whitespace and comments are left; drop them.
                        scanner.reset();
                    }
                    if !completed.is_empty() {
                        self.run_statements(&completed, interactive);
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    // Ctrl-C - clear the open statement
                    if scanner.is_open() {
                        println!("^C");
                        scanner.reset();
                    } else {
                        println!("Use \\q or Ctrl-D to exit");
                    }
                }
                Err(ReadlineError::Eof) => {
                    // End of input. A statement left without its terminator is
                    // run; an open quote or comment is an error.
                    if let Some(open) = scanner.open_construct() {
                        eprintln!(
                            "{}",
                            formatter::format_error(&format!(
                                "unterminated {open} at end of input; the statement was not run"
                            ))
                        );
                    } else {
                        let last = scanner.finish();
                        self.run_statements(&last, interactive);
                    }
                    // Ctrl-D - exit
                    if !self.quiet {
                        println!("Goodbye!");
                    }
                    break;
                }
                Err(err) => {
                    eprintln!("{}", formatter::format_error(&format!("Error: {}", err)));
                    break;
                }
            }
        }

        // Save history
        if interactive {
            if let Some(history_path) = &self.config.history_path {
                let _ = self.editor.save_history(history_path);
            }
        }

        Ok(())
    }

    /// Run the statements the scanner completed, in order, then refresh the
    /// table names completion offers.
    fn run_statements(&mut self, statements: &[String], interactive: bool) {
        for statement in statements {
            // Leading comments are dropped so the keyword checks see the
            // keyword; comments inside the statement go through untouched.
            let sql = crate::protocol::postgres::handler::pg_skip_leading_comments(statement).trim();
            if sql.is_empty() {
                continue;
            }
            if interactive {
                let _ = self.editor.add_history_entry(sql);
            }
            self.execute_sql(sql);
        }

        // Update table names for completion — the tables of
        // the branch the REPL is on (sprinter 6bc4a4040eb7, B3).
        let tables = {
            let _catalog_scope = self.db.embedded_catalog_scope();
            self.db.storage.catalog().list_tables()
        };
        if let Ok(tables) = tables {
            if let Some(helper) = self.editor.helper_mut() {
                helper.completer.set_table_names(tables);
            }
        }
    }

    /// `\x`, `\pset`, `\t`, `\a`: change how rows are drawn. Returns whether
    /// `command` was one of them.
    fn apply_display_command(&mut self, command: &MetaCommand) -> bool {
        let outcome = match command {
            MetaCommand::Expanded(value) => {
                display::parse_expanded(value.as_deref(), self.display.expanded).map(|mode| {
                    self.display.expanded = mode;
                    self.display.touched = true;
                    format!("Expanded display is {}.", display::expanded_name(mode))
                })
            }
            MetaCommand::Pset { option, value } => self.display.pset(option, value.as_deref()),
            MetaCommand::TuplesOnly(state) => {
                let value = state.map(|on| if on { "on" } else { "off" });
                self.display.pset("tuples_only", value)
            }
            MetaCommand::Aligned => self.display.pset("aligned", None),
            _ => return false,
        };
        match outcome {
            Ok(message) => println!("{message}"),
            Err(message) => eprintln!("{}", formatter::format_error(&message)),
        }
        true
    }

    /// Run one backslash command. Returns true when the shell is to quit.
    fn run_meta_command(&mut self, trimmed: &str) -> bool {
        let Some(meta_cmd) = MetaCommand::parse(trimmed) else {
            eprintln!(
                "{}",
                formatter::format_error(&format!("Unknown meta command: {}. Type \\h for help.", trimmed))
            );
            return false;
        };
        // The display commands change the shell's own settings.
        if self.apply_display_command(&meta_cmd) {
            return false;
        }
        match meta_cmd.execute(&self.db, self.show_timing, Some(&self.config)) {
            Ok(MetaCommandResult::Quit) => {
                if !self.quiet {
                    println!("Goodbye!");
                }
                return true;
            }
            Ok(MetaCommandResult::ToggleTiming(new_state)) => {
                self.show_timing = new_state;
            }
            Ok(MetaCommandResult::SwitchBranch(branch_name)) => {
                // sprinter 6bc4a4040eb7 (B1): `\use` has already
                // bound THIS REPL's session (`switch_branch`,
                // which refuses a branch that is not active);
                // the prompt reads that binding.
                // Branch data isolation is now active
                if branch_name != "main" {
                    println!(
                        "{}",
                        format!("Switched to branch '{}' with data isolation enabled.", branch_name).green()
                    );
                    println!("{}", "  Data changes on this branch are isolated from main.".dimmed());
                }
            }
            Ok(MetaCommandResult::ToggleLsn(_)) => {
                self.show_lsn = !self.show_lsn;
            }
            Ok(MetaCommandResult::ConfigReloaded(new_config)) => {
                // Apply the new configuration
                self.show_timing = new_config.show_timing;
                self.display = DisplaySettings::from_config(&new_config);
                self.config = new_config;
            }
            Ok(MetaCommandResult::Continue) => {}
            Err(e) => {
                eprintln!("{}", formatter::format_error(&e.to_string()));
            }
        }
        false
    }

    /// Execute one SQL statement (leading comments already dropped)
    fn execute_sql(&mut self, sql: &str) {
        // Skip whitespace-only input (comments already stripped before calling this)
        if sql.trim().is_empty() {
            return;
        }

        let start = Instant::now();

        // Check for USE BRANCH statement
        if crate::sql::Parser::is_use_branch(sql) {
            // sprinter 6bc4a4040eb7 (B1): bind THIS REPL's session through the
            // one resolver — an unknown, dropped or merged branch is refused
            // (this pre-detect used to set the engine-wide branch with no
            // existence check at all). No cache flush: the caches no longer
            // depend on a branch switch.
            let bound = crate::sql::Parser::parse_use_branch_sql(sql)
                .and_then(|branch_name| self.db.switch_branch(&branch_name).map(|_| branch_name));
            match bound {
                Ok(branch_name) => {
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

        // The engine decides whether the statement returned rows or only an
        // affected count, and names the columns of the rows it returned.
        match self.db.run_statement(sql) {
            Ok(StatementOutcome::Rows { columns, rows }) => {
                let duration = start.elapsed();
                // At a terminal (or once the user has asked) the rows are drawn by
                // the display settings; through a pipe the table is the old one.
                let terminal = Terminal::detect();
                let rendered = display::render(&self.display, &columns, &rows, terminal);
                println!();
                display::print_paged(&rendered.text, &self.display, terminal);
                if self.show_timing {
                    println!("{}", formatter::format_timing(duration.as_secs_f64()));
                }
                if self.show_lsn {
                    if let Some(lsn) = self.db.current_lsn() {
                        println!("{}", format!("LSN: {}", lsn).dimmed());
                    }
                }
            }
            Ok(StatementOutcome::Affected(affected)) => {
                let duration = start.elapsed();
                let message = super::runner::affected_message(sql, affected);

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

    /// Print welcome banner
    fn print_banner(&self) {
        println!();
        println!("╔═══════════════════════════════════════════════════════════════╗");
        println!(
            "║  {} {}{}",
            "HeliosDB Nano".bold().cyan(),
            format!("v{}", env!("CARGO_PKG_VERSION")).bold().green(),
            "                                    ║"
        );
        println!(
            "║  {}  ║",
            "PostgreSQL-compatible database with enterprise features".dimmed()
        );
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
        println!(
            "      For multi-user access, use: {} {}",
            "heliosdb-nano start".cyan(),
            "--help".dimmed()
        );
        println!();

        println!(
            "Commands: {} help | {} list tables | {} system views | {} quit",
            "\\h".cyan(),
            "\\d".cyan(),
            "\\dS".cyan(),
            "\\q".cyan()
        );
        println!();
    }
}
