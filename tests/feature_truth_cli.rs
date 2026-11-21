#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! Feature truth, command line: no flag may be accepted and then ignored.
//!
//! Every subcommand and every flag is read from the binary's own `--help`, recursively
//! (no flag is hidden from help, which `no_flag_is_hidden_from_help` keeps true), so a
//! new flag is in this test the moment it exists. Each flag must be one of:
//!
//! - exercised: some test or test script passes it to its subcommand;
//! - in `[known_dead]` of `tests/feature_truth/cli_flags.txt`: accepted and ignored
//!   today, with the package that owns it;
//! - in `[not_exercised]` there: nothing passes it in a test yet, with the package that
//!   owns the subcommand. These are not known to be dead; they are not known to work.
//!
//! The test fails for a flag in none of the three, for a listed flag that no longer
//! exists, and for a `[not_exercised]` flag that a test now passes (remove its line).
//! "Exercised" is a text match (the flag and its subcommand's name in one test file):
//! it shows that a test passes the flag, not that the test checks what the flag does.

#[path = "feature_truth_support/lists.rs"]
mod lists;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

fn help(path: &[String]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .args(path)
        .arg("--help")
        .env("NO_COLOR", "1")
        .output()
        .expect("run --help");
    assert!(
        output.status.success(),
        "{path:?} --help: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The subcommands and the long flags a help text lists; a flag's short form follows it
/// after a space (`--output -o`).
fn parse_help(text: &str) -> (Vec<String>, Vec<String>) {
    let (mut commands, mut flags) = (Vec::new(), Vec::new());
    let mut section = "";
    for line in text.lines() {
        if !line.starts_with(' ') && line.trim_end().ends_with(':') {
            section = line.trim_end();
            continue;
        }
        if section == "Commands:" {
            // "  name  description": two spaces, then the name.
            if let Some(rest) = line.strip_prefix("  ") {
                if !rest.starts_with(' ') {
                    if let Some(name) = rest.split_whitespace().next() {
                        if name != "help" {
                            commands.push(name.to_string());
                        }
                    }
                }
            }
        } else if section.ends_with("ptions:") {
            // "  -s, --long <V>" or "      --long": the flag starts at column 2 or 6.
            let flag_line = (line.starts_with("  -") && !line.starts_with("   ")) || line.starts_with("      --");
            if flag_line {
                let mut short = None;
                for word in line.split_whitespace() {
                    if let Some(name) = word.strip_prefix("--") {
                        let name: String = name
                            .chars()
                            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                            .collect();
                        if !name.is_empty() && name != "help" && name != "version" {
                            flags.push(match short {
                                Some(s) => format!("--{name} {s}"),
                                None => format!("--{name}"),
                            });
                        }
                        break;
                    }
                    if word.len() == 3 && word.starts_with('-') && word.ends_with(',') {
                        short = Some(word.trim_end_matches(',').to_string());
                    }
                }
            }
        }
    }
    (commands, flags)
}

/// Every `subcommand path --flag` of the binary, with the flag's short form if it has one.
fn all_flags() -> BTreeMap<String, Option<String>> {
    let mut out = BTreeMap::new();
    let mut queue: Vec<Vec<String>> = vec![Vec::new()];
    while let Some(path) = queue.pop() {
        let (commands, flags) = parse_help(&help(&path));
        for flag in flags {
            let (long, short) = match flag.split_once(' ') {
                Some((long, short)) => (long.to_string(), Some(short.to_string())),
                None => (flag, None),
            };
            out.insert(format!("{} {long}", path.join(" ")).trim().to_string(), short);
        }
        for command in commands {
            let mut next = path.clone();
            next.push(command);
            queue.push(next);
        }
    }
    out
}

/// The text of every test and test script, except this file.
fn test_sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs" || e == "sh" || e == "py") {
                if path.file_name().is_some_and(|n| n == "feature_truth_cli.rs") {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&path) {
                    out.push((path.to_string_lossy().into_owned(), text));
                }
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    walk(&root.join("tests"), &mut out);
    walk(&root.join("scripts"), &mut out);
    out
}

fn has_word(text: &str, word: &str) -> bool {
    let mut from = 0;
    while let Some(at) = text[from..].find(word) {
        let start = from + at;
        let end = start + word.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        let inside = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !inside(before) && !inside(after) {
            return true;
        }
        from = end;
    }
    false
}

/// A flag is exercised when one file passes it (the long form as a word, or the short
/// form as a quoted argument) and names its subcommand.
fn exercised(entry: &str, short: Option<&str>, sources: &[(String, String)]) -> bool {
    let mut words: Vec<&str> = entry.split(' ').collect();
    let flag = words.pop().unwrap_or("");
    sources.iter().any(|(_, text)| {
        let passed = has_word(text, flag)
            || short.is_some_and(|s| text.contains(&format!("\"{s}\"")) || text.contains(&format!("'{s}'")));
        passed
            && match words.last() {
                Some(sub) => has_word(text, sub),
                None => true,
            }
    })
}

#[test]
fn the_help_reader_finds_commands_and_flags() {
    let text = "\
Usage: x [OPTIONS] <COMMAND>

Commands:
  start       Start the server
  code-graph  Things
  help        Print this message or the help of the given subcommand(s)

Options:
  -p, --port <PORT>
          Port to listen on [default: 5432]
      --tls-cert <TLS_CERT>  --not-a-flag in a description
      --memory               In-memory
  -h, --help                 Print help
  -V, --version              Print version
";
    let (commands, flags) = parse_help(text);
    assert_eq!(commands, ["start", "code-graph"]);
    assert_eq!(flags, ["--port -p", "--tls-cert", "--memory"]);
    assert!(has_word("x --port 5", "--port") && !has_word("x --port-range 5", "--port"));
    assert!(has_word("[\"start\", \"--memory\"]", "start") && !has_word("restart", "start"));
}

#[test]
fn no_flag_is_hidden_from_help() {
    // The enumeration reads --help: a hidden flag or subcommand would escape it.
    let main = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).unwrap();
    for needle in ["hide = true", "hide(true)", "hide_short_help", "hide_long_help"] {
        assert!(
            !main.contains(needle),
            "src/main.rs uses `{needle}`: list that flag by hand in this test"
        );
    }
}

#[test]
fn every_flag_is_exercised_or_listed() {
    let flags = all_flags();
    assert!(flags.len() > 80, "only {} flags found", flags.len());
    for must in [
        "start --port",
        "dump --connection",
        "restore --connection",
        "start --dump-schedule",
    ] {
        assert!(flags.contains_key(must), "{must} not found: the help reader is broken");
    }
    let sources = test_sources();
    let file = lists::load("cli_flags", 1);
    let dead: BTreeMap<&str, &str> = lists::section(&file, "known_dead")
        .iter()
        .map(|e| (e.key.as_str(), e.field(0)))
        .collect();
    let quiet: BTreeMap<&str, &str> = lists::section(&file, "not_exercised")
        .iter()
        .map(|e| (e.key.as_str(), e.field(0)))
        .collect();

    let mut problems = Vec::new();
    for (flag, short) in &flags {
        let used = exercised(flag, short.as_deref(), &sources);
        match (
            dead.contains_key(flag.as_str()),
            quiet.contains_key(flag.as_str()),
            used,
        ) {
            (true, true, _) => problems.push(format!("{flag}: in both lists")),
            (true, false, _) | (false, false, true) => {}
            (false, true, true) => problems.push(format!(
                "{flag}: a test passes it now: remove its line from [not_exercised]"
            )),
            (false, true, false) => {}
            (false, false, false) => problems.push(format!("no test passes it: {flag} | <owner>")),
        }
    }
    for (flag, owner) in dead.iter().chain(quiet.iter()) {
        if !flags.contains_key(*flag) {
            problems.push(format!("{flag}: listed for {owner}, the binary has no such flag"));
        }
    }
    assert!(
        problems.is_empty(),
        "{} flags, {} problems:\n{}",
        flags.len(),
        problems.len(),
        problems.join("\n")
    );
}
