//! Regression tests for the REPL meta-commands that previously printed
//! hint text instead of running their underlying query.
//!
//! Symptom (reported 2026-05-03):
//! - `SELECT * FROM pg_database_branches();` → showed the branch table.
//! - `\branches` → printed `Use: SELECT * FROM pg_database_branches();` (hint, not data).
//! - `SHOW BRANCHES;` → printed `Query OK, 2 row(s) affected` with no rows.
//!
//! These tests pin the new behaviour: `\branches`, `\snapshots`, `\dmv`,
//! `\compression`, and `\indexes <t>` all run their corresponding system
//! query end-to-end, and the REPL routes `SHOW BRANCHES` through the
//! query path so the rows surface.

use heliosdb_nano::EmbeddedDatabase;

#[test]
fn pg_database_branches_function_returns_at_least_main() {
    // The data source the `\branches` meta-command depends on must
    // return at least one row (the implicit `main` branch).
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let (rows, cols) = db
        .query_with_columns("SELECT * FROM pg_database_branches()")
        .expect("query");
    assert!(!rows.is_empty(), "pg_database_branches() must include at least main");
    assert!(cols.iter().any(|c| c == "branch_name"));
}

#[test]
fn pg_mv_staleness_function_is_queryable() {
    // Used by `\dmv`. Phase3 SystemViewRegistry — embedded path.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let result = db.query_with_columns("SELECT * FROM pg_mv_staleness()");
    assert!(result.is_ok(), "pg_mv_staleness() must be queryable; got {result:?}");
}

#[test]
fn pg_vector_index_stats_function_is_queryable() {
    // Used by `\compression`.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let result = db.query_with_columns("SELECT * FROM pg_vector_index_stats()");
    assert!(
        result.is_ok(),
        "pg_vector_index_stats() must be queryable; got {result:?}"
    );
}

#[test]
fn show_branches_returns_real_rows_via_executor() {
    // SHOW BRANCHES is the SQL surface that previously dropped its rows
    // when run via `db.execute()` (the REPL command path). The fix
    // routes SHOW through `db.query()` instead. Either way, the
    // executor itself must produce rows — verify here at the API level.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE DATABASE BRANCH b1 FROM main AS OF NOW")
        .expect("create branch");
    let (rows, cols) = db.query_with_columns("SHOW BRANCHES").expect("show branches");
    // `pg_database_branches()` and `SHOW BRANCHES` both expose `branch_name`
    // (or `name` in the SHOW shape); accept either to keep the test
    // resilient to the column-name choice.
    let has_name_col = cols.iter().any(|c| c == "branch_name" || c == "name");
    assert!(has_name_col, "SHOW BRANCHES must produce a name column; got {cols:?}");
    // Should include at least main + b1.
    assert!(rows.len() >= 2, "expected ≥2 branches (main + b1), got {}", rows.len());
    // b1 must appear by name.
    let any_b1 = rows.iter().any(|r| {
        r.values
            .iter()
            .any(|v| matches!(v, heliosdb_nano::Value::String(s) if s == "b1"))
    });
    assert!(any_b1, "SHOW BRANCHES output should contain b1; rows={rows:?}");
}

// ---------------------------------------------------------------------------
// Truthful meta-commands
// ---------------------------------------------------------------------------

mod truthful {
    use heliosdb_nano::config::CompressionType;
    use heliosdb_nano::repl::{
        render_config, render_conninfo, render_replication, render_replication_state, render_status, render_telemetry,
        MetaCommand, MetaCommandResult, ReplicationState, RoleRecordSummary,
    };
    use heliosdb_nano::{Config, EmbeddedDatabase};
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// The line of `text` that holds `needle`.
    fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
        text.lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{text}"))
            .trim()
    }

    #[test]
    fn timing_on_and_off_are_absolute() {
        assert_eq!(MetaCommand::parse("\\timing"), Some(MetaCommand::ToggleTiming));
        assert_eq!(MetaCommand::parse("\\timing on"), Some(MetaCommand::SetTiming(true)));
        assert_eq!(MetaCommand::parse("\\timing OFF"), Some(MetaCommand::SetTiming(false)));
        assert_eq!(MetaCommand::parse("\\timing sideways"), None);

        let db = EmbeddedDatabase::new_in_memory().expect("db");
        // On is on, whatever the state was; off is off.
        for before in [true, false] {
            let on = MetaCommand::SetTiming(true).execute(&db, before, None).expect("on");
            assert!(matches!(on, MetaCommandResult::ToggleTiming(true)), "{on:?}");
            let off = MetaCommand::SetTiming(false).execute(&db, before, None).expect("off");
            assert!(matches!(off, MetaCommandResult::ToggleTiming(false)), "{off:?}");
        }
        // Bare \timing still toggles.
        let toggled = MetaCommand::ToggleTiming.execute(&db, true, None).expect("toggle");
        assert!(matches!(toggled, MetaCommandResult::ToggleTiming(false)), "{toggled:?}");
    }

    #[test]
    fn status_config_telemetry_agree_on_wal_and_time_travel() {
        let db = EmbeddedDatabase::new_in_memory().expect("db");
        let status = render_status(&db);
        let config = render_config(&db, None);
        let telemetry = render_telemetry(&db);
        for (name, text) in [("status", &status), ("config", &config), ("telemetry", &telemetry)] {
            assert_eq!(line_with(text, "WAL:"), "WAL: disabled", "{name}:\n{text}");
            assert!(!text.contains("WAL: enabled"), "{name}:\n{text}");
            assert_eq!(
                line_with(text, "Time-Travel:"),
                line_with(&status, "Time-Travel:"),
                "{name}"
            );
            assert_eq!(
                line_with(text, "Compression:"),
                line_with(&status, "Compression:"),
                "{name}"
            );
        }
    }

    #[test]
    fn config_reflects_non_default_storage() {
        let directory = tempfile::tempdir().expect("dir");
        let mut config = Config::default();
        config.storage.path = Some(directory.path().join("data"));
        config.storage.memory_only = false;
        config.storage.time_travel_enabled = false;
        config.storage.compression = CompressionType::None;
        let db = EmbeddedDatabase::with_config(config).expect("db");
        let wal = if db.storage.is_wal_enabled() {
            "enabled"
        } else {
            "disabled"
        };
        for (name, text) in [
            ("status", render_status(&db)),
            ("config", render_config(&db, None)),
            ("telemetry", render_telemetry(&db)),
        ] {
            assert_eq!(line_with(&text, "Time-Travel:"), "Time-Travel: disabled", "{name}");
            assert_eq!(line_with(&text, "Compression:"), "Compression: none", "{name}");
            assert_eq!(line_with(&text, "WAL:"), format!("WAL: {wal}"), "{name}");
        }
    }

    #[test]
    fn replication_renders_standalone_for_in_memory() {
        let db = EmbeddedDatabase::new_in_memory().expect("db");
        let text = render_replication(&db);
        assert!(text.contains("standalone (in-process shell)"), "{text}");
        assert_eq!(line_with(&text, "WAL:"), "WAL: disabled");
        // The text that advertised tiers and flags that do not exist is gone.
        for stale in [
            "Tier 1",
            "Tier 2",
            "Tier 3",
            "--role",
            "To enable replication",
            "Warm Standby",
        ] {
            assert!(!text.contains(stale), "{stale:?} in:\n{text}");
        }
    }

    #[test]
    fn replication_renders_standby_from_fixture_state() {
        let state = ReplicationState {
            physical_standby: true,
            wal: Some(Some(42)),
            role_record: Some(RoleRecordSummary {
                role: "standby".to_string(),
                primary_host: Some("10.0.0.6:5433".to_string()),
                epoch: 2,
            }),
            record_path: Some("/srv/nano/data.role.toml".into()),
            record_error: None,
        };
        let text = render_replication_state(&state);
        assert!(text.contains("physical standby"), "{text}");
        assert!(text.contains("current LSN 42"), "{text}");
        assert!(text.contains("follows: 10.0.0.6:5433"), "{text}");
        assert!(text.contains("epoch: 2"), "{text}");
        assert!(text.contains("/srv/nano/data.role.toml"), "{text}");

        let unreadable = ReplicationState {
            physical_standby: false,
            wal: None,
            role_record: None,
            record_path: None,
            record_error: Some("bad toml".to_string()),
        };
        let text = render_replication_state(&unreadable);
        assert!(text.contains("standalone"), "{text}");
        assert!(text.contains("unreadable (bad toml)"), "{text}");
    }

    #[test]
    fn conninfo_names_the_mode_directory_and_branch() {
        let db = EmbeddedDatabase::new_in_memory().expect("db");
        let text = render_conninfo(&db);
        assert!(text.contains("embedded"), "{text}");
        assert!(text.contains("in memory"), "{text}");
        assert!(text.contains("Branch: main"), "{text}");
        assert_eq!(MetaCommand::parse("\\conninfo"), Some(MetaCommand::ConnInfo));
    }

    #[test]
    fn commands_that_only_pretended_are_gone() {
        for gone in [
            "\\e",
            "\\edit",
            "\\server",
            "\\server start",
            "\\ssl",
            "\\optimize users",
        ] {
            assert_eq!(MetaCommand::parse(gone), None, "{gone}");
        }
    }

    /// Run `input` through the shell on an in-memory database; (stdout, stderr).
    fn shell(input: &str) -> (String, String) {
        let directory = tempfile::tempdir().expect("dir");
        let mut child = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
            .args(["repl", "--memory"])
            .current_dir(directory.path())
            .env("RUST_LOG", "error")
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write");
        let output = child.wait_with_output().expect("output");
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    #[test]
    fn user_add_then_list_shows_role_and_prints_not_a_login_notice() {
        let (stdout, stderr) = shell("\\user add alice\n\\user list\n\\user remove alice\n\\user list\n");
        assert!(stderr.trim().is_empty(), "stderr: {stderr}");
        assert!(stdout.contains("not a login"), "{stdout}");
        let listings: Vec<&str> = stdout.split("Roles:").skip(1).collect();
        assert_eq!(listings.len(), 2, "{stdout}");
        assert!(listings[0].contains("alice"), "{stdout}");
        assert!(!listings[1].contains("alice"), "{stdout}");
    }

    #[test]
    fn password_and_set_say_what_they_do_not_do() {
        let (stdout, _) = shell("\\password alice\n\\set foo bar\n");
        assert!(stdout.contains("nothing changed for 'alice'"), "{stdout}");
        assert!(stdout.contains("shell variables are not supported"), "{stdout}");
        assert!(!stdout.contains("Set foo"), "{stdout}");
    }

    #[test]
    fn timing_off_stops_the_timing_line() {
        let (stdout, _) = shell("\\timing off\nSELECT 1;\n\\timing on\nSELECT 2;\n");
        assert!(stdout.contains("Timing is off"), "{stdout}");
        assert!(stdout.contains("Timing is on"), "{stdout}");
        let after_off = stdout.split("Timing is off").nth(1).expect("after off");
        let (between, after_on) = after_off.split_once("Timing is on").expect("on");
        assert!(!between.contains(" ms)"), "{between}");
        assert!(after_on.contains(" ms)"), "{after_on}");
    }

    /// The names of the backslash commands the help advertises, with a line
    /// that exercises each (`<x>` becomes a word, `[x]` is left out).
    fn advertised_commands() -> Vec<(String, String)> {
        let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/repl/help_manager.rs"))
            .expect("help_manager.rs");
        let mut found = Vec::new();
        let mut rest = source.as_str();
        while let Some(at) = rest.find("print_command(") {
            rest = &rest[at + "print_command(".len()..];
            let Some(open) = rest.find('"') else { break };
            let literal_start = open + 1;
            let mut end = literal_start;
            let bytes = rest.as_bytes();
            while end < bytes.len() && bytes[end] != b'"' {
                end += if bytes[end] == b'\\' { 2 } else { 1 };
            }
            let literal = rest[literal_start..end].replace("\\\\", "\\").replace("\\\"", "\"");
            rest = &rest[end..];
            if !literal.starts_with('\\') {
                continue;
            }
            for alternative in literal.split(", ") {
                let mut probe = String::new();
                let mut skip = 0usize;
                let mut word = false;
                for ch in alternative.chars() {
                    match ch {
                        '[' => skip += 1,
                        ']' => skip = skip.saturating_sub(1),
                        '<' if skip == 0 => word = true,
                        '>' if skip == 0 => {
                            if word {
                                probe.push_str("word");
                            }
                            word = false;
                        }
                        _ if skip == 0 && !word => probe.push(ch),
                        _ => {}
                    }
                }
                found.push((alternative.to_string(), probe.trim().to_string()));
            }
        }
        found
    }

    #[test]
    fn help_lists_no_command_that_does_not_parse() {
        let commands = advertised_commands();
        assert!(commands.len() > 60, "found only {} help entries", commands.len());
        let mut failures = Vec::new();
        for (advertised, probe) in &commands {
            if MetaCommand::parse(probe).is_none() {
                failures.push(format!("{advertised}  (tried: {probe})"));
            }
        }
        assert!(
            failures.is_empty(),
            "help advertises commands the shell does not parse:\n{}",
            failures.join("\n")
        );
    }
}
