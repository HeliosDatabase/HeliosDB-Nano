//! Script and pipe mode: `repl -c SQL`, `repl -f FILE`.
//!
//! The statements of the commands and files run in order through
//! [`EmbeddedDatabase::run_statement`], so rows and affected counts are decided
//! once, by the engine. Results go to stdout through the writer of the chosen
//! format, errors to stderr, and the exit code says how it went:
//!
//! * `0` every statement ran;
//! * `1` usage or a fatal error (a file that cannot be read);
//! * `2` the database could not be opened (decided by the caller);
//! * `3` a statement failed.
//!
//! Nothing here reads or writes a history file or prints a banner.

use super::formatter;
use super::writers::{writer_for, ResultWriter, WriterOptions};
use super::OutputFormat;
use crate::protocol::postgres::handler::{pg_skip_leading_comments, pg_split_sql_respecting_quotes};
use crate::{EmbeddedDatabase, StatementOutcome};
use std::io::{self, Read, Write};

/// Every statement ran.
pub const EXIT_OK: i32 = 0;
/// Usage, or a fatal error such as a file that cannot be read.
pub const EXIT_FATAL: i32 = 1;
/// The database could not be opened.
pub const EXIT_OPEN: i32 = 2;
/// A statement failed.
pub const EXIT_STATEMENT: i32 = 3;

/// What to run and how to print it.
#[derive(Debug, Clone)]
pub struct BatchOptions {
    /// `-c` strings, run first, in order.
    pub commands: Vec<String>,
    /// `-f` files, run after the commands, in order; `-` is standard input.
    pub files: Vec<String>,
    /// How rows are written.
    pub format: OutputFormat,
    /// The text a NULL is written as; `None` is the format's own.
    pub null: Option<String>,
    /// No header row and no row-count footer.
    pub tuples_only: bool,
    /// One transaction around everything: `BEGIN` first, `COMMIT` last,
    /// `ROLLBACK` at the first failure. DDL is not transactional in Nano, and a
    /// `CALL` or a user-defined function inside an explicit transaction is
    /// refused, so the rollback is complete only for data changes.
    pub single_transaction: bool,
    /// Stop at the first failing statement (otherwise run on and exit 3 at the end).
    pub on_error_stop: bool,
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            files: Vec::new(),
            format: OutputFormat::Table,
            null: None,
            tuples_only: false,
            single_transaction: false,
            on_error_stop: true,
        }
    }
}

/// How a statement is reported when it fails.
fn report_error(err: &mut dyn Write, format: OutputFormat, number: usize, sql: &str, message: &str) -> io::Result<()> {
    if matches!(format, OutputFormat::Json | OutputFormat::Jsonl) {
        let object = serde_json::json!({
            "error": message,
            "statement": number,
            "sql": sql,
        });
        writeln!(err, "{object}")
    } else {
        writeln!(err, "{}", formatter::format_error(message))
    }
}

/// What the shell prints for a statement that returned no rows.
pub(crate) fn affected_message(sql: &str, affected: u64) -> String {
    let upper = sql.trim().to_uppercase();
    if upper.starts_with("CREATE") || upper.starts_with("DROP") {
        "Query OK".to_string()
    } else {
        format!("Query OK, {affected} row(s) affected")
    }
}

/// Run the statements of `options` against `db`; returns the exit code.
///
/// `stdin` is read only when a file is `-`.
pub fn run_batch(
    db: &EmbeddedDatabase,
    options: &BatchOptions,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    // Read everything first: an unreadable file is a usage error, and nothing runs.
    let mut texts: Vec<String> = options.commands.clone();
    for file in &options.files {
        let mut text = String::new();
        let read = if file == "-" {
            stdin.read_to_string(&mut text).map(|_| ())
        } else {
            std::fs::read_to_string(file).map(|content| text = content)
        };
        if let Err(error) = read {
            let _ = writeln!(
                err,
                "{}",
                formatter::format_error(&format!(
                    "cannot read {}: {error}",
                    if file == "-" { "standard input" } else { file }
                ))
            );
            return EXIT_FATAL;
        }
        texts.push(text);
    }
    let statements: Vec<String> = texts
        .iter()
        .flat_map(|text| pg_split_sql_respecting_quotes(text))
        .collect();

    let mut writer: Box<dyn ResultWriter> = writer_for(
        options.format,
        WriterOptions {
            null: options.null.clone(),
            tuples_only: options.tuples_only,
        },
    );
    let mut failed = false;

    let mut run = |sql: &str, number: usize, out: &mut dyn Write, err: &mut dyn Write| -> io::Result<bool> {
        match db.run_statement(sql) {
            Ok(StatementOutcome::Rows { columns, rows }) => {
                writer.write_rows(out, &columns, &rows)?;
                Ok(true)
            }
            Ok(StatementOutcome::Affected(affected)) => {
                if options.format == OutputFormat::Table {
                    writeln!(out, "{}", affected_message(sql, affected))?;
                }
                Ok(true)
            }
            Err(error) => {
                report_error(err, options.format, number, sql, &error.to_string())?;
                Ok(false)
            }
        }
    };

    let outcome = (|| -> io::Result<()> {
        if options.single_transaction {
            if !run("BEGIN", 0, out, err)? {
                failed = true;
                return Ok(());
            }
        }
        for (index, statement) in statements.iter().enumerate() {
            let sql = pg_skip_leading_comments(statement).trim();
            if sql.is_empty() {
                continue;
            }
            if !run(sql, index + 1, out, err)? {
                failed = true;
                if options.single_transaction {
                    // Rolled back whatever the on-error setting: a half-applied script is not wanted.
                    let _ = run("ROLLBACK", 0, out, err)?;
                    return Ok(());
                }
                if options.on_error_stop {
                    return Ok(());
                }
            }
        }
        if options.single_transaction && !failed && !run("COMMIT", statements.len() + 1, out, err)? {
            failed = true;
        }
        Ok(())
    })();

    match outcome {
        Ok(()) => {}
        // The reader went away (`| head`): not a failure of the script.
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return EXIT_OK,
        Err(error) => {
            let _ = writeln!(
                err,
                "{}",
                formatter::format_error(&format!("cannot write the output: {error}"))
            );
            return EXIT_FATAL;
        }
    }
    let _ = out.flush();
    if failed {
        EXIT_STATEMENT
    } else {
        EXIT_OK
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn run(options: &BatchOptions) -> (i32, String, String) {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_batch(&db, options, &mut io::empty(), &mut out, &mut err);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    fn commands(commands: &[&str], format: OutputFormat) -> BatchOptions {
        BatchOptions {
            commands: commands.iter().map(|c| (*c).to_string()).collect(),
            format,
            ..BatchOptions::default()
        }
    }

    #[test]
    fn a_typed_json_row() {
        let (code, out, err) = run(&commands(&["SELECT 1 AS a"], OutputFormat::Json));
        assert_eq!((code, out.as_str(), err.as_str()), (0, "[{\"a\":1}]\n", ""));
    }

    #[test]
    fn a_failing_statement_exits_3_and_prints_nothing_on_stdout() {
        let (code, out, err) = run(&commands(&["SELEC 1"], OutputFormat::Table));
        assert_eq!(code, EXIT_STATEMENT);
        assert_eq!(out, "");
        assert!(err.starts_with("ERROR:"), "{err}");
    }

    #[test]
    fn json_errors_are_one_object_each() {
        let (code, _, err) = run(&commands(&["SELEC 1"], OutputFormat::Json));
        assert_eq!(code, EXIT_STATEMENT);
        let object: serde_json::Value = serde_json::from_str(err.trim()).unwrap();
        assert_eq!(object["statement"], 1);
        assert_eq!(object["sql"], "SELEC 1");
        assert!(object["error"].as_str().unwrap().contains("parse"));
    }

    #[test]
    fn on_error_stop_stops_and_off_continues() {
        let mut options = commands(&["SELEC 1; SELECT 2 AS two;"], OutputFormat::Csv);
        let (code, out, _) = run(&options);
        assert_eq!((code, out.as_str()), (EXIT_STATEMENT, ""));
        options.on_error_stop = false;
        let (code, out, _) = run(&options);
        assert_eq!((code, out.as_str()), (EXIT_STATEMENT, "two\n2\n"));
    }

    #[test]
    fn single_transaction_rolls_back_data_changes() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
        let options = BatchOptions {
            commands: vec![
                "INSERT INTO t VALUES (1);".into(),
                "INSERT INTO t VALUES (2);".into(),
                "INSERT INTO t VALUES (2);".into(),
                "INSERT INTO t VALUES (3);".into(),
            ],
            format: OutputFormat::Csv,
            single_transaction: true,
            ..BatchOptions::default()
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_batch(&db, &options, &mut io::empty(), &mut out, &mut err);
        assert_eq!(code, EXIT_STATEMENT);
        let (rows, _) = db.query_with_columns("SELECT id FROM t").unwrap();
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn a_file_that_cannot_be_read_is_a_usage_error() {
        let options = BatchOptions {
            files: vec!["/nonexistent/script.sql".into()],
            ..BatchOptions::default()
        };
        let (code, out, err) = run(&options);
        assert_eq!((code, out.as_str()), (EXIT_FATAL, ""));
        assert!(err.contains("cannot read /nonexistent/script.sql"), "{err}");
    }

    #[test]
    fn standard_input_is_a_file_named_dash() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let options = BatchOptions {
            files: vec!["-".into()],
            format: OutputFormat::Csv,
            ..BatchOptions::default()
        };
        let mut input: &[u8] = b"SELECT 1 AS a;\nSELECT 2 AS b";
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_batch(&db, &options, &mut input, &mut out, &mut err);
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(out).unwrap(), "a\n1\nb\n2\n");
    }

    #[test]
    fn affected_counts_print_only_in_the_table_format() {
        let (_, out, _) = run(&commands(
            &["CREATE TABLE t (id INT); INSERT INTO t VALUES (1);"],
            OutputFormat::Table,
        ));
        assert_eq!(out, "Query OK\nQuery OK, 1 row(s) affected\n");
        let (_, out, _) = run(&commands(
            &["CREATE TABLE t (id INT); INSERT INTO t VALUES (1);"],
            OutputFormat::Csv,
        ));
        assert_eq!(out, "");
    }
}
