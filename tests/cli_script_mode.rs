//! Script and pipe mode of the shell: `repl -c`, `repl -f`, `--format`, exit codes.
//!
//! A script, a CI job or an agent runs SQL with one command, reads csv or json on
//! stdout, errors on stderr, and gets a non-zero exit code when a statement fails.
//! With none of the new flags the shell prints what it always did (pinned by
//! `tests/cli_shell_pipe.rs::legacy_pipe_output_is_pinned`).
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

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run `heliosdb-nano <args>` in `directory`, feeding `stdin`.
fn nano_in(directory: &Path, args: &[&str], stdin: &str) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .args(args)
        .current_dir(directory)
        .env("RUST_LOG", "error")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        // The child may exit before reading its input (a usage error).
        let _ = input.write_all(stdin.as_bytes());
    }
    let output = child.wait_with_output().unwrap();
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn nano(args: &[&str]) -> Run {
    let directory = tempfile::tempdir().unwrap();
    nano_in(directory.path(), args, "")
}

#[test]
fn command_json_prints_typed_row() {
    let run = nano(&["repl", "--memory", "-c", "SELECT 1 AS a", "--format", "json"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "[{\"a\":1}]\n");
    assert_eq!(run.stderr, "");
}

#[test]
fn sql_is_an_alias_of_repl() {
    let run = nano(&["sql", "--memory", "-c", "SELECT 1 AS a", "--format", "csv"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "a\n1\n");
}

#[test]
fn script_mode_prints_no_banner_and_no_starting_line() {
    let run = nano(&["repl", "--memory", "-c", "SELECT 1 AS a", "--format", "csv"]);
    assert!(!run.stdout.contains("Starting REPL"), "{}", run.stdout);
    assert!(!run.stdout.contains("HeliosDB Nano"), "{}", run.stdout);
    assert!(!run.stdout.contains("Goodbye"), "{}", run.stdout);
}

#[test]
fn syntax_error_exits_3_with_empty_stdout() {
    let run = nano(&["repl", "--memory", "-c", "SELEC 1"]);
    assert_eq!(run.code, Some(3), "{}", run.stderr);
    assert_eq!(run.stdout, "");
    assert!(run.stderr.contains("ERROR"), "{}", run.stderr);
}

#[test]
fn json_format_reports_each_failed_statement_as_one_json_object() {
    let run = nano(&["repl", "--memory", "-c", "SELEC 1", "--format", "json"]);
    assert_eq!(run.code, Some(3));
    let object: serde_json::Value = serde_json::from_str(run.stderr.trim()).unwrap();
    assert_eq!(object["statement"], 1);
    assert!(object["error"].as_str().unwrap().contains("parse"), "{object}");
}

#[test]
fn file_with_dollar_body_and_multiline_literal_runs_each_statement_once() {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("schema.sql");
    std::fs::write(
        &script,
        "-- a schema\n\
         CREATE TABLE t (id INT PRIMARY KEY, v TEXT);\n\
         CREATE FUNCTION inc(x INT) RETURNS INT AS $$\n\
         BEGIN\n\
         \x20 RETURN $1 + 1;\n\
         END;\n\
         $$ LANGUAGE plpgsql;\n\
         INSERT INTO t VALUES (1, 'first;\n\
         \x20 second');\n\
         SELECT id, length(v) AS n, inc(id) AS next FROM t;\n\
         SELECT count(*) AS rows FROM t",
    )
    .unwrap();
    let run = nano_in(
        directory.path(),
        &["repl", "--memory", "-f", script.to_str().unwrap(), "--format", "csv"],
        "",
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "id,n,next\n1,15,2\nrows\n1\n");
    assert_eq!(run.stderr, "");
}

#[test]
fn standard_input_as_a_file() {
    let directory = tempfile::tempdir().unwrap();
    let run = nano_in(
        directory.path(),
        &["repl", "--memory", "-f", "-", "--format", "csv"],
        "SELECT 1 AS a;\nSELECT 2 AS b",
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "a\n1\nb\n2\n");
}

#[test]
fn several_commands_run_in_order() {
    let run = nano(&[
        "repl",
        "--memory",
        "-c",
        "CREATE TABLE t (id INT)",
        "-c",
        "INSERT INTO t VALUES (4), (5)",
        "-c",
        "SELECT sum(id) AS s FROM t",
        "--format",
        "csv",
    ]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "s\n9\n");
}

#[test]
fn table_format_is_the_shells_table() {
    let run = nano(&["repl", "--memory", "-c", "SELECT 1 AS a, 'x' AS b"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let table: Vec<&str> = run.stdout.lines().filter(|line| line.starts_with('│')).collect();
    assert_eq!(table, vec!["│ a │ b │", "│ 1 │ x │"], "{}", run.stdout);
    assert!(run.stdout.contains("(1 row)"), "{}", run.stdout);
    assert!(
        !run.stdout.contains(" ms)"),
        "no timing line in script mode: {}",
        run.stdout
    );
}

#[test]
fn tuples_only_drops_the_header() {
    let run = nano(&["repl", "--memory", "-c", "SELECT 1 AS a", "--format", "csv", "-t"]);
    assert_eq!(run.stdout, "1\n");
}

#[test]
fn null_text_is_the_one_asked_for() {
    let run = nano(&[
        "repl",
        "--memory",
        "-c",
        "SELECT NULL AS a",
        "--format",
        "csv",
        "--null",
        "<none>",
    ]);
    assert_eq!(run.stdout, "a\n<none>\n");
}

/// The value types the writers have to spell, in one row.
const TYPES_SQL: &str = "CREATE TABLE ty (n TEXT, b BYTEA, j JSON, ts TIMESTAMP, v VECTOR(2), u TEXT, tricky TEXT, f DOUBLE PRECISION, i BIGINT);\n\
     INSERT INTO ty VALUES (NULL, '\\xdeadbeef', '{\"a\":[1,2]}', '2026-01-02 03:04:05', '[1,2.5]', 'héllo ✓ 日本', E'a,\"b\"\\nc|d\\te', 1.5, 9007199254740993);\n\
     SELECT * FROM ty";

#[test]
fn format_goldens() {
    let regenerate = std::env::var_os("HELIOS_REGENERATE_GOLDEN").is_some();
    for format in ["csv", "tsv", "json", "jsonl", "vertical", "markdown"] {
        let run = nano(&["repl", "--memory", "-c", TYPES_SQL, "--format", format]);
        assert_eq!(run.code, Some(0), "{format}: {}", run.stderr);
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cli/formats")
            .join(format!("{format}.out"));
        if regenerate {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &run.stdout).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(run.stdout, expected, "{format}");
    }
}

#[test]
fn json_golden_is_valid_json_with_the_right_types() {
    let run = nano(&["repl", "--memory", "-c", TYPES_SQL, "--format", "json"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let rows: serde_json::Value = serde_json::from_str(run.stdout.trim()).unwrap();
    let row = &rows[0];
    assert!(row["n"].is_null(), "{row}");
    assert_eq!(row["b"], "\\xdeadbeef");
    assert_eq!(row["j"], serde_json::json!({"a": [1, 2]}));
    assert_eq!(row["v"], serde_json::json!([1.0, 2.5]));
    assert_eq!(row["u"], "héllo ✓ 日本");
    assert_eq!(row["f"], 1.5);
    assert!(row["i"].is_number(), "{row}");
    assert!(row["ts"].as_str().unwrap().starts_with("2026-01-02T03:04:05"), "{row}");
}

/// Five INSERTs, the third fails: nothing of the five is left.
#[test]
fn single_transaction_rolls_back_dml() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let data = data.to_str().unwrap();
    let setup = nano_in(
        directory.path(),
        &["repl", "-d", data, "-c", "CREATE TABLE t (id INT PRIMARY KEY)"],
        "",
    );
    assert_eq!(setup.code, Some(0), "{}", setup.stderr);

    let script = directory.path().join("load.sql");
    std::fs::write(
        &script,
        "INSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2);\nINSERT INTO t VALUES (2);\nINSERT INTO t VALUES (4);\nINSERT INTO t VALUES (5);\n",
    )
    .unwrap();
    let load = nano_in(
        directory.path(),
        &["repl", "-d", data, "-1", "-f", script.to_str().unwrap()],
        "",
    );
    assert_eq!(load.code, Some(3), "{}", load.stderr);

    let count = nano_in(
        directory.path(),
        &[
            "repl",
            "-d",
            data,
            "-c",
            "SELECT count(*) AS n FROM t",
            "--format",
            "csv",
        ],
        "",
    );
    assert_eq!(count.code, Some(0), "{}", count.stderr);
    assert_eq!(count.stdout, "n\n0\n");
}

#[test]
fn single_transaction_help_states_ddl_limit() {
    let run = nano(&["repl", "--help"]);
    assert_eq!(run.code, Some(0));
    let help = run.stdout.replace('\n', " ");
    let at = help.find("--single-transaction").expect("flag in help");
    let text = &help[at..];
    assert!(text.contains("DDL"), "{text}");
}

#[test]
fn on_error_stop_off_continues_and_exits_3() {
    let run = nano(&[
        "repl",
        "--memory",
        "-c",
        "SELEC 1; SELECT 2 AS two",
        "--format",
        "csv",
        "--on-error-stop=false",
    ]);
    assert_eq!(run.code, Some(3), "{}", run.stderr);
    assert_eq!(run.stdout, "two\n2\n");
}

#[test]
fn on_error_stop_is_the_default() {
    let run = nano(&["repl", "--memory", "-c", "SELEC 1; SELECT 2 AS two", "--format", "csv"]);
    assert_eq!(run.code, Some(3), "{}", run.stderr);
    assert_eq!(run.stdout, "");
}

#[test]
fn missing_data_dir_exits_2() {
    let directory = tempfile::tempdir().unwrap();
    let blocker = directory.path().join("a-file");
    std::fs::write(&blocker, "not a directory").unwrap();
    let below = blocker.join("data");
    let run = nano_in(
        directory.path(),
        &["repl", "-d", below.to_str().unwrap(), "-c", "SELECT 1"],
        "",
    );
    assert_eq!(run.code, Some(2), "{}", run.stderr);
    assert_eq!(run.stdout, "");
    assert!(run.stderr.contains("ERROR"), "{}", run.stderr);
}

#[test]
fn a_script_file_that_cannot_be_read_exits_1() {
    let run = nano(&["repl", "--memory", "-f", "/nonexistent/script.sql"]);
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert_eq!(run.stdout, "");
    assert!(run.stderr.contains("cannot read"), "{}", run.stderr);
}

#[test]
fn script_mode_writes_no_history_file() {
    let directory = tempfile::tempdir().unwrap();
    let run = nano_in(
        directory.path(),
        &["repl", "--memory", "-c", "SELECT 1 AS a", "--format", "csv"],
        "",
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let left: Vec<_> = std::fs::read_dir(directory.path()).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_format_flag_without_a_command_or_file_is_refused() {
    let run = nano(&["repl", "--memory", "--format", "csv"]);
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(run.stderr.contains("-c") && run.stderr.contains("-f"), "{}", run.stderr);
}

#[test]
fn config_file_sets_format_and_null() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("repl.toml");
    std::fs::write(&config, "output_format = \"csv\"\nnull_display = \"-\"\n").unwrap();
    let run = nano_in(
        directory.path(),
        &[
            "repl",
            "--memory",
            "--config",
            config.to_str().unwrap(),
            "-c",
            "SELECT NULL AS n, 1 AS a",
        ],
        "",
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "n,a\n-,1\n");

    // The flag wins over the file.
    let run = nano_in(
        directory.path(),
        &[
            "repl",
            "--memory",
            "--config",
            config.to_str().unwrap(),
            "-c",
            "SELECT 1 AS a",
            "--format",
            "json",
        ],
        "",
    );
    assert_eq!(run.stdout, "[{\"a\":1}]\n");
}

#[test]
fn an_unreadable_config_file_exits_1() {
    let run = nano(&[
        "repl",
        "--memory",
        "--config",
        "/nonexistent/repl.toml",
        "-c",
        "SELECT 1",
    ]);
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert_eq!(run.stdout, "");
}
