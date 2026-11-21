#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! Feature truth, shell: no backslash command may only pretend.
//!
//! Every command of the registry (`src/repl/registry.rs`) is run through the real shell
//! on an in-memory database, with each of its examples. A command is a stub when its
//! output says so (the phrases below). Commands that are stubs today are in
//! `tests/feature_truth/repl_commands.txt`, with the package of the 5.0 plan that owns
//! them and the text of today's output that gives them away: the test fails when a
//! command not listed there starts to pretend, and when a listed command stops printing
//! its text (so its line is removed when it lands). A command whose answer is canned
//! without saying so cannot be found by a phrase: those are listed by hand, with the
//! canned text.

#[path = "feature_truth_support/lists.rs"]
mod lists;

use heliosdb_nano::repl::registry;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Output that says a command did not do what it is named for.
const STUB_PHRASES: &[&str] = &[
    "not yet implemented",
    "not implemented",
    "not supported",
    "not yet supported",
    "coming soon",
    "placeholder",
    "nothing changed",
];

/// Statements run before every command, so that commands have something to show.
const PRELUDE: &str = "\
CREATE TABLE ft_users (id INT PRIMARY KEY, name TEXT);
INSERT INTO ft_users VALUES (1, 'ada');
CREATE INDEX ft_users_name ON ft_users (name);
SELECT 'prelude-done';
";

/// Run `input` through the shell on an in-memory database; (stdout and stderr, finished).
fn shell(input: &str) -> (String, bool) {
    let directory = tempfile::tempdir().expect("dir");
    // The output goes to files: a pipe nobody reads would stall a talkative command.
    let out_path = directory.path().join("stdout.txt");
    let err_path = directory.path().join("stderr.txt");
    let mut child = Command::new(env!("CARGO_BIN_EXE_heliosdb-nano"))
        .args(["repl", "--memory"])
        .current_dir(directory.path())
        .env("RUST_LOG", "error")
        .env("NO_COLOR", "1")
        .env("HOME", directory.path())
        .stdin(Stdio::piped())
        .stdout(std::fs::File::create(&out_path).expect("stdout file"))
        .stderr(std::fs::File::create(&err_path).expect("stderr file"))
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write");
    let deadline = Instant::now() + Duration::from_secs(60);
    let finished = loop {
        match child.try_wait().expect("wait") {
            Some(_) => break true,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let read = |path: &std::path::Path| String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).into_owned();
    (format!("{}\n{}", read(&out_path), read(&err_path)), finished)
}

/// What the shell printed for the command alone: everything after the prelude's marker row.
fn after_prelude(output: &str) -> &str {
    match output.rfind("prelude-done") {
        Some(at) => &output[at..],
        None => output,
    }
}

#[test]
fn every_backslash_command_does_something_or_is_a_known_stub() {
    let specs = registry::all();
    assert!(specs.len() > 80, "{} commands in the registry", specs.len());
    let jobs: Vec<(&'static str, &'static str)> = specs
        .iter()
        .flat_map(|spec| spec.examples.iter().map(move |example| (spec.name, *example)))
        .collect();
    let file = lists::load("repl_commands", 2);
    // Shell lines a command needs before it has anything to act on.
    let setup: BTreeMap<&str, String> = lists::section(&file, "setup")
        .iter()
        .map(|e| {
            (
                e.key.as_str(),
                e.fields.iter().skip(1).map(|l| format!("{l}\n")).collect::<String>(),
            )
        })
        .collect();
    let next = AtomicUsize::new(0);
    let outputs: Mutex<Vec<(&'static str, &'static str, String, bool)>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, Ordering::SeqCst);
                let Some((name, example)) = jobs.get(at) else { break };
                let before = setup.get(name).map(String::as_str).unwrap_or("");
                let (output, finished) = shell(&format!("{PRELUDE}{before}{example}\n"));
                outputs
                    .lock()
                    .unwrap()
                    .push((*name, *example, after_prelude(&output).to_string(), finished));
            });
        }
    });
    let outputs = outputs.into_inner().unwrap();
    if let Ok(dir) = std::env::var("FEATURE_TRUTH_DUMP") {
        std::fs::create_dir_all(&dir).unwrap();
        for (i, (name, example, output, _)) in outputs.iter().enumerate() {
            let file = format!("{dir}/{:03}-{}.txt", i, name.replace(['\\', ' ', '?', '!', '+'], "_"));
            std::fs::write(file, format!("{example}\n----\n{output}")).unwrap();
        }
    }

    let known: BTreeMap<&str, (&str, &str)> = lists::section(&file, "known_stubs")
        .iter()
        .map(|e| (e.key.as_str(), (e.field(1), e.field(0))))
        .collect();
    let names: BTreeSet<&str> = specs.iter().map(|s| s.name).collect();
    let mut problems = Vec::new();
    let mut still_stubs: BTreeSet<&str> = BTreeSet::new();
    for (name, example, output, finished) in &outputs {
        if !finished {
            problems.push(format!("{name}: `{example}` did not finish within 60 s"));
            continue;
        }
        let lower = output.to_lowercase();
        match known.get(*name) {
            Some((marker, _)) => {
                if output.contains(marker) {
                    still_stubs.insert(*name);
                }
            }
            None => {
                if let Some(phrase) = STUB_PHRASES.iter().find(|p| lower.contains(**p)) {
                    let line = output
                        .lines()
                        .find(|l| l.to_lowercase().contains(phrase))
                        .unwrap_or("")
                        .trim();
                    problems.push(format!(
                        "new stub: {name} | <owner> | {line}    (from `{example}`, phrase \"{phrase}\")"
                    ));
                }
            }
        }
    }
    for (name, (marker, owner)) in &known {
        if !names.contains(*name) {
            problems.push(format!("{name}: listed for {owner}, not in the registry"));
        } else if !still_stubs.contains(*name) {
            problems.push(format!(
                "{name}: no example prints \"{marker}\" any more: remove its line from repl_commands.txt (owner {owner})"
            ));
        }
    }
    problems.sort();
    assert!(
        problems.is_empty(),
        "{} problems:\n{}",
        problems.len(),
        problems.join("\n")
    );
}
