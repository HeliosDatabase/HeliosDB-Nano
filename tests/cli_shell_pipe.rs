//! The shell fed through a pipe: `heliosdb-nano repl --memory < script`.
//!
//! The legacy SQLite SDK drives the shell this way and scrapes what it prints,
//! so `legacy_pipe_output_is_pinned` pins the output of an ordinary script, with
//! the timing lines and the version in the banner normalised. The other tests
//! pin what the statement scanner and the statement outcome do for input that
//! used to fail, be cut in the wrong place, or be dropped.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// What a run of the shell wrote.
struct Run {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

/// Run `input` through `heliosdb-nano repl --memory` in a directory of its own
/// (the shell writes its history file where it runs).
fn shell_in(directory: &Path, input: &str) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .args(["repl", "--memory"])
        .current_dir(directory)
        .env("RUST_LOG", "error")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code(),
    }
}

fn shell(input: &str) -> Run {
    let directory = tempfile::tempdir().unwrap();
    shell_in(directory.path(), input)
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

fn row(cells: &[&str]) -> Vec<String> {
    cells.iter().map(|cell| cell.to_string()).collect()
}

/// Replace the parts of the output that change from run to run: the timing
/// lines `(1.63 ms)` / `(2.001 s)` and the version in the banner.
fn normalise(text: &str) -> String {
    text.lines()
        .map(|line| {
            let trimmed = line.trim();
            let is_timing = trimmed
                .strip_prefix('(')
                .and_then(|rest| rest.strip_suffix(" ms)").or_else(|| rest.strip_suffix(" s)")))
                .is_some_and(|number| number.parse::<f64>().is_ok());
            if is_timing {
                "(TIMING)".to_string()
            } else if line.contains("HeliosDB Nano v") {
                "║  HeliosDB Nano vVERSION".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cli")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn legacy_pipe_output_is_pinned() {
    let run = shell(&fixture("legacy_pipe.sql"));
    let actual = format!(
        "{}\n---- stderr ----\n{}\n",
        normalise(&run.stdout),
        normalise(&run.stderr)
    );
    let expected = fixture("legacy_pipe.out");
    if std::env::var_os("HELIOS_REGENERATE_GOLDEN").is_some() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli/legacy_pipe.out");
        std::fs::write(path, &actual).unwrap();
        return;
    }
    assert_eq!(actual, expected, "the piped output changed");
}

#[test]
fn multi_line_function_body_runs_once() {
    let run = shell(
        "CREATE FUNCTION f(x INT) RETURNS INT AS $$\n\
         BEGIN\n\
         \x20 RETURN $1 + 1;\n\
         END;\n\
         $$ LANGUAGE plpgsql;\n\
         SELECT f(1) AS r;\n",
    );
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    let rows = table_rows(&run.stdout);
    assert_eq!(rows, vec![row(&["r"]), row(&["2"])], "{}", run.stdout);
}

#[test]
fn multi_line_literal_keeps_inner_whitespace() {
    let run = shell(
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT);\n\
         INSERT INTO t VALUES (1, 'a;\n\
         \x20\x20 b');\n\
         SELECT length(v) AS n, v FROM t;\n",
    );
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    // 'a;' newline three spaces 'b' is seven characters.
    let rows = table_rows(&run.stdout);
    assert_eq!(rows.first(), Some(&row(&["n", "v"])), "{}", run.stdout);
    assert_eq!(
        rows.get(1).and_then(|r| r.first()).map(String::as_str),
        Some("7"),
        "{}",
        run.stdout
    );
}

#[test]
fn multi_line_block_comment_is_one_comment() {
    let run = shell("/* first line;\nsecond line; */\nSELECT 5 AS five;\n");
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    assert_eq!(
        table_rows(&run.stdout),
        vec![row(&["five"]), row(&["5"])],
        "{}",
        run.stdout
    );
}

#[test]
fn two_statements_on_one_line_both_run() {
    let run = shell("SELECT 1 AS a; SELECT 2 AS b;\n");
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    assert_eq!(
        table_rows(&run.stdout),
        vec![row(&["a"]), row(&["1"]), row(&["b"]), row(&["2"])],
        "{}",
        run.stdout
    );
}

#[test]
fn final_statement_without_semicolon_runs_at_eof() {
    let run = shell("SELECT 1 AS a;\nSELECT 2 AS b");
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    assert_eq!(
        table_rows(&run.stdout),
        vec![row(&["a"]), row(&["1"]), row(&["b"]), row(&["2"])],
        "{}",
        run.stdout
    );
    assert_eq!(run.code, Some(0));
}

#[test]
fn trailing_comment_after_a_semicolon_is_dropped() {
    let run = shell("SELECT 1 AS a; -- the first\n-- nothing more\n");
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    assert_eq!(
        table_rows(&run.stdout),
        vec![row(&["a"]), row(&["1"])],
        "{}",
        run.stdout
    );
}

#[test]
fn an_open_quote_at_eof_is_an_error_on_stderr() {
    let run = shell("SELECT 1 AS a;\nSELECT 'never closed;\n");
    assert!(
        run.stderr.contains("unterminated string literal"),
        "stderr: {}",
        run.stderr
    );
    assert_eq!(
        table_rows(&run.stdout),
        vec![row(&["a"]), row(&["1"])],
        "{}",
        run.stdout
    );
}

#[test]
fn an_open_block_comment_at_eof_is_an_error_on_stderr() {
    let run = shell("SELECT 1 AS a;\n/* never closed\n");
    assert!(
        run.stderr.contains("unterminated block comment"),
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn json_path_operators_survive() {
    let run = shell(
        "SELECT '{\"a\":{\"b\":1}}'::json #> '{a,b}' AS p;\n\
         SELECT '{\"a\":{\"b\":1}}'::json #>> '{a,b}' AS q;\n",
    );
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    assert_eq!(
        table_rows(&run.stdout),
        vec![row(&["p"]), row(&["1"]), row(&["q"]), row(&["1"])],
        "{}",
        run.stdout
    );
}

#[test]
fn a_backslash_command_after_a_comment_line_is_still_a_command() {
    let run = shell("-- list the tables\n\\dt\n");
    assert!(!run.stderr.contains("Unknown meta command"), "stderr: {}", run.stderr);
}

#[test]
fn a_dollar_quoted_literal_with_a_semicolon_is_one_statement() {
    let run = shell("SELECT $$a;b$$ AS s;\n");
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    assert_eq!(
        table_rows(&run.stdout),
        vec![row(&["s"]), row(&["a;b"])],
        "{}",
        run.stdout
    );
}

// --- the statement outcome: rows or an affected count, decided by the engine ---

#[test]
fn union_in_parentheses_prints_rows() {
    let run = shell("(SELECT 1 AS a) UNION (SELECT 2);\n");
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    assert_eq!(
        table_rows(&run.stdout),
        vec![row(&["a"]), row(&["1"]), row(&["2"])],
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains("Query OK"), "{}", run.stdout);
}

#[test]
fn execute_prepared_select_prints_rows() {
    let run = shell(
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT);\n\
         INSERT INTO t VALUES (1, 'a'), (2, 'b');\n\
         PREPARE p AS SELECT id, v FROM t WHERE id > $1;\n\
         EXECUTE p(1);\n",
    );
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    assert_eq!(
        table_rows(&run.stdout),
        vec![row(&["id", "v"]), row(&["2", "b"])],
        "{}",
        run.stdout
    );
}

#[test]
fn literal_with_returning_word_reports_affected() {
    let run = shell(
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT);\n\
         INSERT INTO t VALUES (9, 'x returning y');\n",
    );
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    assert!(run.stdout.contains("Query OK, 1 row(s) affected"), "{}", run.stdout);
    assert!(!run.stdout.contains("(0 rows)"), "{}", run.stdout);
}

#[test]
fn returning_prints_column_names() {
    let run = shell(
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT);\n\
         INSERT INTO t VALUES (1, 'a') RETURNING id AS new_id, v;\n\
         UPDATE t SET v = 'b' WHERE id = 1 RETURNING *;\n\
         DELETE FROM t WHERE id = 1 RETURNING id;\n",
    );
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    assert_eq!(
        table_rows(&run.stdout),
        vec![
            row(&["new_id", "v"]),
            row(&["1", "a"]),
            row(&["id", "v"]),
            row(&["1", "b"]),
            row(&["id"]),
            row(&["1"]),
        ],
        "{}",
        run.stdout
    );
}

#[test]
fn show_variable_prints_rows() {
    let run = shell("SHOW search_path;\nSHOW server_version;\n");
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    assert!(!run.stdout.contains("Query returned"), "{}", run.stdout);
    assert_eq!(table_rows(&run.stdout).len(), 4, "{}", run.stdout);
}

#[test]
fn select_under_search_path_prints_rows() {
    let run = shell(
        "CREATE SCHEMA s1;\n\
         CREATE TABLE s1.t2 (id INT PRIMARY KEY, v TEXT);\n\
         INSERT INTO s1.t2 VALUES (1, 'one');\n\
         SET search_path TO s1;\n\
         SELECT * FROM t2;\n",
    );
    assert!(!run.stdout.contains("Query returned"), "{}", run.stdout);
    assert_eq!(
        table_rows(&run.stdout),
        vec![row(&["id", "v"]), row(&["1", "one"])],
        "{}\n{}",
        run.stdout,
        run.stderr
    );
}

#[test]
fn select_inside_a_transaction_sees_its_own_writes() {
    let run = shell(
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT);\n\
         INSERT INTO t VALUES (1, 'a');\n\
         BEGIN;\n\
         INSERT INTO t VALUES (9, 'q');\n\
         SELECT * FROM t ORDER BY id;\n\
         ROLLBACK;\n\
         SELECT * FROM t ORDER BY id;\n",
    );
    assert!(run.stderr.trim().is_empty(), "stderr: {}", run.stderr);
    assert_eq!(
        table_rows(&run.stdout),
        vec![
            row(&["id", "v"]),
            row(&["1", "a"]),
            row(&["9", "q"]),
            row(&["id", "v"]),
            row(&["1", "a"]),
        ],
        "{}",
        run.stdout
    );
}

#[test]
fn select_on_branch_only_table_prints_rows() {
    let run = shell(
        "CREATE TABLE base (id INT PRIMARY KEY, v TEXT);\n\
         CREATE BRANCH dev FROM main AS OF NOW;\n\
         USE BRANCH dev;\n\
         CREATE TABLE only_on_dev (id INT PRIMARY KEY, v TEXT);\n\
         INSERT INTO only_on_dev VALUES (7, 'seven');\n\
         SELECT id, v FROM only_on_dev;\n",
    );
    assert_eq!(
        table_rows(&run.stdout),
        vec![row(&["id", "v"]), row(&["7", "seven"])],
        "{}\n{}",
        run.stdout,
        run.stderr
    );
}

// --- readable results: nothing terminal-only reaches a pipe ---

#[test]
fn pager_not_spawned_when_stdout_is_a_pipe() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("pager-ran");
    let pager = directory.path().join("pager.sh");
    std::fs::write(&pager, format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pager, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let rows: String = (0..200).map(|i| format!("SELECT {i} AS n;\n")).collect();
    let mut child = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .args(["repl", "--memory"])
        .current_dir(directory.path())
        .env("RUST_LOG", "error")
        .env("NO_COLOR", "1")
        .env("PAGER", &pager)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(rows.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(!marker.exists(), "the pager must not run when stdout is a pipe");
}

#[test]
fn expanded_mode_asked_for_applies_to_a_pipe() {
    let run = shell("\\x on\nSELECT 1 AS a, 'x' AS b;\n\\x off\nSELECT 2 AS a;\n");
    assert!(run.stdout.contains("-[ RECORD 1 ]"), "{}", run.stdout);
    assert!(run.stdout.contains("a | 1"), "{}", run.stdout);
    assert_eq!(
        table_rows(&run.stdout),
        vec![row(&["a"]), row(&["2"])],
        "{}",
        run.stdout
    );
}

#[test]
fn pset_null_text_applies_to_a_pipe() {
    let run = shell("\\pset null '(none)'\nSELECT NULL AS a;\n");
    assert!(run.stdout.contains("(none)"), "{}", run.stdout);
    assert!(!run.stdout.contains("NULL"), "{}", run.stdout);
}
