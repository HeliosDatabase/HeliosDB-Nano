//! The shell prints the rows of a SELECT made on a branch.
//!
//! The shell takes its column headers from planning the statement a second
//! time. That plan read main's catalog whatever branch the shell was on, so a
//! table that exists only on the branch printed `Query returned 1 row(s)` and no
//! rows, and a column added on the branch had no header. The engine's answer was
//! right; only the display was wrong.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::process::{Command, Stdio};

/// Run the statements through `heliosdb-nano repl --memory`, one per line, in a
/// directory of their own (the shell writes its history file where it runs).
fn shell(statements: &[&str]) -> String {
    let directory = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .args(["repl", "--memory"])
        .current_dir(directory.path())
        .env("RUST_LOG", "error")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        for statement in statements {
            writeln!(input, "{statement}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The cells of the table rows the shell printed, one `Vec` per row, headers included.
fn table_rows(output: &str) -> Vec<Vec<String>> {
    output
        .lines()
        .filter(|line| line.starts_with('│'))
        .map(|line| {
            line.trim_matches('│')
                .split('│')
                .map(|cell| cell.trim().to_string())
                .collect()
        })
        .collect()
}

#[test]
fn a_table_that_exists_only_on_the_branch_prints_its_rows() {
    let output = shell(&[
        "CREATE TABLE base (id INT PRIMARY KEY, v TEXT);",
        "INSERT INTO base VALUES (1, 'on main');",
        "CREATE BRANCH dev FROM main AS OF NOW;",
        "USE BRANCH dev;",
        "CREATE TABLE only_on_dev (id INT PRIMARY KEY, v TEXT);",
        "INSERT INTO only_on_dev VALUES (7, 'seven');",
        "SELECT id, v FROM only_on_dev;",
    ]);
    assert!(!output.contains("Query returned"), "{output}");
    let rows = table_rows(&output);
    assert!(rows.contains(&vec!["id".to_string(), "v".to_string()]), "{output}");
    assert!(rows.contains(&vec!["7".to_string(), "seven".to_string()]), "{output}");
}

#[test]
fn a_column_added_on_the_branch_has_its_header() {
    let output = shell(&[
        "CREATE TABLE base (id INT PRIMARY KEY, v TEXT);",
        "INSERT INTO base VALUES (1, 'one');",
        "CREATE BRANCH dev FROM main AS OF NOW;",
        "USE BRANCH dev;",
        "ALTER TABLE base ADD COLUMN extra INT;",
        "UPDATE base SET extra = 5 WHERE id = 1;",
        "SELECT * FROM base;",
    ]);
    assert!(!output.contains("Query returned"), "{output}");
    let rows = table_rows(&output);
    assert!(
        rows.contains(&vec!["id".to_string(), "v".to_string(), "extra".to_string()]),
        "{output}"
    );
    assert!(
        rows.contains(&vec!["1".to_string(), "one".to_string(), "5".to_string()]),
        "{output}"
    );
}

#[test]
fn main_is_printed_as_before() {
    let output = shell(&[
        "CREATE TABLE base (id INT PRIMARY KEY, v TEXT);",
        "INSERT INTO base VALUES (1, 'one');",
        "SELECT id, v FROM base;",
    ]);
    let rows = table_rows(&output);
    assert_eq!(
        rows,
        vec![
            vec!["id".to_string(), "v".to_string()],
            vec!["1".to_string(), "one".to_string()]
        ],
        "{output}"
    );
}
