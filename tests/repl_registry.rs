//! The command table the shell's help is rendered from.
//!
//! The parser in `commands.rs` stays hand-written; these tests keep the table
//! and the parser from drifting apart: every example in the table parses, every
//! alias is the same command, every command word the parser accepts is in the
//! table, and the help names only commands in the table.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::repl::registry::{self, split_args, Category};
use heliosdb_nano::repl::MetaCommand;
use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn every_spec_has_category_and_summary() {
    assert!(registry::all().len() > 80, "{} entries", registry::all().len());
    for spec in registry::all() {
        assert!(spec.name.starts_with('\\'), "{}", spec.name);
        assert!(!spec.summary.trim().is_empty(), "{} has no summary", spec.name);
        assert!(!spec.section.trim().is_empty(), "{} has no section", spec.name);
        assert!(!spec.examples.is_empty(), "{} has no example", spec.name);
        assert!(Category::ALL.contains(&spec.category), "{}", spec.name);
        for alias in spec.aliases {
            assert!(alias.starts_with('\\') && !alias.contains(' '), "{alias}");
        }
    }
}

#[test]
fn every_spec_example_parses() {
    let mut failures = Vec::new();
    for spec in registry::all() {
        for example in spec.examples {
            if MetaCommand::parse(example).is_none() {
                failures.push(format!("{}: {example}", spec.name));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "examples that do not parse:\n{}",
        failures.join("\n")
    );
}

#[test]
fn every_alias_parses_to_the_same_command() {
    let mut checked = 0;
    for spec in registry::all() {
        for alias in spec.aliases {
            let example = spec.examples[0];
            let Some(rest) = example.strip_prefix(spec.name) else {
                panic!(
                    "the first example of {} does not start with its name: {example}",
                    spec.name
                );
            };
            let aliased = format!("{alias}{rest}");
            let original = MetaCommand::parse(example);
            assert!(original.is_some(), "{example}");
            assert_eq!(MetaCommand::parse(&aliased), original, "{aliased} against {example}");
            checked += 1;
        }
    }
    assert!(checked >= 8, "only {checked} aliases checked");
}

/// The command words the parser matches: the `Some("...")` arms of `MetaCommand::parse`.
fn parsed_command_words() -> BTreeSet<String> {
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/repl/commands.rs")).expect("commands.rs");
    let start = source.find("pub fn parse(input: &str)").expect("parse");
    let mut words = BTreeSet::new();
    for line in source[start..].lines().skip(1) {
        // The arms of the outer match sit at twelve spaces.
        let Some(arm) = line.strip_prefix("            Some(") else {
            continue;
        };
        let Some((pattern, _)) = arm.split_once(") =>") else {
            continue;
        };
        for alternative in pattern.split('|') {
            let word = alternative.trim().trim_matches('"');
            if !word.is_empty() {
                words.insert(word.to_string());
            }
        }
    }
    words
}

#[test]
fn every_parsed_command_name_is_in_the_table() {
    let words = parsed_command_words();
    assert!(words.len() > 50, "found {} command words: {words:?}", words.len());
    let missing: Vec<&String> = words.iter().filter(|word| registry::find(word).is_none()).collect();
    assert!(missing.is_empty(), "parsed but not in the table: {missing:?}");
}

#[test]
fn every_table_word_is_parsed() {
    let parsed = parsed_command_words();
    for word in registry::command_words() {
        assert!(
            parsed.contains(word),
            "the table lists \\{word}, which the parser does not match"
        );
    }
}

fn help_output() -> String {
    let mut input = String::from("\\h\n");
    for category in Category::ALL {
        input.push_str(&format!("\\h {}\n", category.word()));
    }
    let directory = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .args(["repl", "--memory", "-q"])
        .current_dir(directory.path())
        .env("RUST_LOG", "error")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn help_output_names_only_registered_commands() {
    let words: BTreeSet<&str> = registry::command_words().into_iter().collect();
    let help = help_output();
    let mut listed = 0;
    for line in help.lines() {
        // A line that starts with a backslash command: two spaces, then `\word`.
        let Some(command) = line.strip_prefix("  \\") else {
            continue;
        };
        let word: String = command
            .chars()
            .take_while(|ch| !ch.is_whitespace() && *ch != ',')
            .collect();
        listed += 1;
        assert!(
            words.contains(word.as_str()),
            "help names `\\{word}`, which is not in the table: {line}"
        );
    }
    assert!(listed > 60, "only {listed} command lines in the help:\n{help}");
    // And every listed command of the table shows up in the help.
    for spec in registry::all().iter().filter(|spec| !spec.hidden) {
        assert!(
            help.contains(&spec.display_form()),
            "{} is not in the help",
            spec.display_form()
        );
    }
}

#[test]
fn hidden_commands_parse_but_are_not_listed() {
    let help = help_output();
    for spec in registry::all().iter().filter(|spec| spec.hidden) {
        assert!(MetaCommand::parse(spec.examples[0]).is_some());
        assert!(!help.contains(&spec.display_form()), "{} is listed", spec.name);
    }
}

#[test]
fn quoted_argument_is_one_argument() {
    assert_eq!(
        split_args("pset null '(null)'").unwrap(),
        vec!["pset", "null", "(null)"]
    );
    assert_eq!(split_args("d \"My Table\"").unwrap(), vec!["d", "My Table"]);
    assert_eq!(split_args("d  a   b ").unwrap(), vec!["d", "a", "b"]);
    assert_eq!(
        split_args(r#"pset null "a \"b\" c""#).unwrap(),
        vec!["pset", "null", "a \"b\" c"]
    );
    assert_eq!(split_args(r"d a\ b").unwrap(), vec!["d", "a b"]);
    assert_eq!(split_args(r"pset null 'a\b'").unwrap(), vec!["pset", "null", r"a\b"]);
    assert_eq!(split_args("pset null ''").unwrap(), vec!["pset", "null", ""]);
    assert_eq!(split_args("").unwrap(), Vec::<String>::new());
    assert!(split_args("d 'open").is_err());
    assert!(split_args("d \"open").is_err());
    assert!(split_args("d trailing\\").is_err());

    // Through the parser.
    assert_eq!(
        MetaCommand::parse("\\d \"My Table\""),
        Some(MetaCommand::DescribeTable("My Table".to_string()))
    );
    assert_eq!(
        MetaCommand::parse("\\pset null '(null)'"),
        Some(MetaCommand::Pset {
            option: "null".to_string(),
            value: Some("(null)".to_string())
        })
    );
    // An unterminated quote is refused, not guessed at.
    assert_eq!(MetaCommand::parse("\\d 'oops"), None);
}

#[test]
fn free_text_commands_keep_their_words_as_typed() {
    // Apostrophes in free text and SQL are not quotes to the splitter.
    assert!(MetaCommand::parse("\\profile SELECT * FROM t WHERE name = 'bob'").is_some());
    assert!(MetaCommand::parse("\\ai generate a user's table").is_some());
}

#[test]
fn lookup_by_word() {
    assert_eq!(registry::find("quit").map(|spec| spec.name), Some("\\q"));
    assert_eq!(
        registry::find("tenant").map(|spec| spec.category),
        Some(Category::Tenants)
    );
    assert!(registry::find("nonsense").is_none());
    assert!(registry::command_words().contains(&"pset"));
}
