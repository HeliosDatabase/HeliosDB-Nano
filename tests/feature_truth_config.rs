#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! Feature truth, configuration: no key may be accepted and then ignored.
//!
//! The keys are read from the configuration itself (the default configuration, written
//! out and walked), so a new key is in this test the moment it exists. Each key must be
//! one of:
//!
//! - declared inert by the loader (`INERT_KEYS` in `src/config.rs`): setting it warns;
//! - in `[known_inert]` of `tests/feature_truth/config_keys.txt`: nothing acts on it and
//!   the loader does not say so, with the package that owns it;
//! - in `[no_test_sets_it]` there: no test sets it, so nothing shows that it has an
//!   effect, with the package that owns it;
//! - set by a test: some test file sets the key under its section's name.
//!
//! The test fails for a key in none of the four; for a key whose name appears nowhere
//! in the source outside the configuration file (nothing can be reading it) unless it
//! is in one of the two inert lists; for a listed key that no longer exists; and for a
//! `[no_test_sets_it]` key that a test now sets (remove its line).
//!
//! "Set by a test" is a text match. It shows that a test changes the key, not that the
//! test checks what the key does: the scope ledger has to read those tests.

#[path = "feature_truth_support/lists.rs"]
mod lists;

use heliosdb_nano::config::{Config, INERT_KEYS};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

/// Keys whose value is a table of the user's own names, or a value written as a table:
/// the walk stops there and takes the key itself.
const LEAVES: &[&str] = &[];

fn walk(prefix: &str, value: &serde_json::Value, out: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) if !map.is_empty() && !LEAVES.contains(&prefix) => {
            for (name, inner) in map {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                walk(&path, inner, out);
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.insert(prefix.to_string());
            }
        }
    }
}

/// Every key the configuration file accepts, as `section.key` (deeper for nested tables).
fn all_keys() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk("", &serde_json::to_value(Config::default()).unwrap(), &mut out);
    // Written only when set, so the default configuration does not show them.
    out.insert("profile".to_string());
    out.insert("strict_config".to_string());
    out
}

fn sources(dirs: &[&str], skip: &[&str]) -> Vec<(String, String)> {
    fn walk_dir(dir: &Path, skip: &[&str], out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.to_string_lossy().into_owned();
            if skip.iter().any(|s| name.ends_with(s)) {
                continue;
            }
            if path.is_dir() {
                walk_dir(&path, skip, out);
            } else if path
                .extension()
                .is_some_and(|e| e == "rs" || e == "toml" || e == "sh" || e == "py")
            {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    out.push((name, text));
                }
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    for dir in dirs {
        walk_dir(&root.join(dir), skip, &mut out);
    }
    out
}

/// The identifiers of a text, and those that are assigned (`name = …` in a
/// configuration text or in Rust, `name: …` in a struct literal).
#[derive(Default)]
struct Words {
    all: HashSet<String>,
    assigned: HashSet<String>,
}

fn words_of(text: &str) -> Words {
    let mut out = Words::default();
    let bytes = text.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i < bytes.len() {
        if !is_word(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_word(bytes[i]) {
            i += 1;
        }
        let word = &text[start..i];
        let mut j = i;
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        let next = bytes.get(j).copied();
        let after = bytes.get(j + 1).copied();
        if (next == Some(b'=') && after != Some(b'=')) || (next == Some(b':') && after != Some(b':')) {
            out.assigned.insert(word.to_string());
        }
        out.all.insert(word.to_string());
    }
    out
}

/// A file sets a key when it assigns the key's last name and names the key's sections.
fn sets(words: &Words, key: &str) -> bool {
    let mut parts: Vec<&str> = key.split('.').collect();
    let name = parts.pop().unwrap_or(key);
    words.assigned.contains(name) && parts.iter().all(|section| words.all.contains(*section))
}

#[test]
fn the_key_walk_and_the_matchers_work() {
    let keys = all_keys();
    assert!(keys.len() > 120, "only {} keys found", keys.len());
    for must in [
        "storage.path",
        "storage.durable_commit",
        "server.port",
        "encryption.rotation_interval_days",
        "materialized_views.auto_refresh_default",
        "profile",
    ] {
        assert!(keys.contains(must), "{must} not found among {} keys", keys.len());
    }
    let sets_text = |text: &str| sets(&words_of(text), "storage.durable_commit");
    assert!(sets_text("[storage]\ndurable_commit = true\n"));
    assert!(sets_text("config.storage.durable_commit = true;"));
    assert!(sets_text("StorageConfig { durable_commit: true, ..d }; // storage"));
    assert!(!sets_text("if c.storage.durable_commit == true {}"));
    assert!(!sets_text("not_durable_commit = 1 # storage"));
    assert!(!sets_text("durable_commit = true"), "the section must be named");
}

#[test]
fn every_inert_key_the_loader_declares_is_a_real_key() {
    let keys = all_keys();
    for (key, _) in INERT_KEYS {
        assert!(
            keys.contains(*key),
            "INERT_KEYS names `{key}`, which the configuration does not have"
        );
    }
}

#[test]
fn every_key_is_inert_and_listed_or_set_by_a_test() {
    let keys = all_keys();
    let file = lists::load("config_keys", 1);
    let declared: BTreeSet<&str> = INERT_KEYS.iter().map(|(k, _)| *k).collect();
    let known_inert: BTreeMap<&str, &str> = lists::section(&file, "known_inert")
        .iter()
        .map(|e| (e.key.as_str(), e.field(0)))
        .collect();
    let unset: BTreeMap<&str, &str> = lists::section(&file, "no_test_sets_it")
        .iter()
        .map(|e| (e.key.as_str(), e.field(0)))
        .collect();

    let tests = sources(
        &["tests", "src", "scripts"],
        &["src/config.rs", "feature_truth_config.rs", "/feature_truth"],
    );
    // Only test code counts as a test: integration tests, scripts, and the part of a
    // source file from its first `#[cfg(test)]` on.
    let tests: Vec<Words> = tests
        .into_iter()
        .filter_map(|(name, text)| {
            if name.contains("/src/") {
                let at = text.find("#[cfg(test)]")?;
                Some(words_of(&text[at..]))
            } else {
                Some(words_of(&text))
            }
        })
        .collect();
    let mut read: HashSet<String> = HashSet::new();
    for (_, text) in sources(&["src"], &["src/config.rs"]) {
        read.extend(words_of(&text).all);
    }

    let mut problems = Vec::new();
    for key in &keys {
        let k = key.as_str();
        let name = k.rsplit('.').next().unwrap_or(k);
        let listed = [declared.contains(k), known_inert.contains_key(k), unset.contains_key(k)];
        if listed.iter().filter(|l| **l).count() > 1 {
            problems.push(format!("{k}: in more than one list"));
            continue;
        }
        let read_somewhere = read.contains(name);
        let set_by_a_test = tests.iter().any(|words| sets(words, k));
        match listed {
            [true, _, _] | [_, true, _] => {}
            _ if !read_somewhere => problems.push(format!(
                "nothing outside src/config.rs names it, so nothing reads it: {k} | <owner> | add under [known_inert]"
            )),
            [_, _, true] if set_by_a_test => problems.push(format!(
                "{k}: a test sets it now: remove its line from [no_test_sets_it]"
            )),
            [_, _, true] => {}
            _ if set_by_a_test => {}
            _ => problems.push(format!("no test sets it: {k} | <owner>")),
        }
    }
    for (key, owner) in known_inert.iter().chain(unset.iter()) {
        if !keys.contains(*key) {
            problems.push(format!("{key}: listed for {owner}, the configuration has no such key"));
        }
    }
    for key in known_inert.keys() {
        if declared.contains(key) {
            problems.push(format!(
                "{key}: the loader declares it inert now: remove its line from [known_inert]"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{} keys, {} problems:\n{}",
        keys.len(),
        problems.len(),
        problems.join("\n")
    );
}
