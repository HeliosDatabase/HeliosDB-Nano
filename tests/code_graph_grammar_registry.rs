//! Integration tests for runtime grammar registration.
//!
//! Verifies the public API on `EmbeddedDatabase`:
//! `register_grammar` / `unregister_grammar` / `registered_grammars`.
//!
//! The tree-sitter `Language` values are loaded from grammar crates we
//! already depend on (rust, python) — that's the same shape a caller
//! would use to register a WASM-loaded grammar, just with a wasm
//! runtime on the loading side instead of a Cargo dep.

#![cfg(feature = "code-graph")]

use heliosdb_nano::code_graph::parse::parse_by_name;
use heliosdb_nano::EmbeddedDatabase;

#[test]
fn register_then_parse_via_db_handle() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");

    // Register tree-sitter-rust under a custom name so we can confirm
    // the dynamic dispatch path is what's resolving the parse.
    let prior = db.register_grammar("custom_rust", tree_sitter_rust::LANGUAGE.into());
    assert!(prior.is_none(), "fresh registration should not return an existing entry");

    let tree = parse_by_name("custom_rust", "fn main() { let x = 1; }")
        .expect("registered grammar parses");
    assert_eq!(tree.root_node().kind(), "source_file");

    let names = db.registered_grammars();
    assert!(names.contains(&"custom_rust".to_string()), "have: {names:?}");

    let removed = db.unregister_grammar("custom_rust");
    assert!(removed.is_some(), "unregister returns the removed entry");

    let after = db.registered_grammars();
    assert!(!after.contains(&"custom_rust".to_string()));
}

#[test]
fn registry_overrides_builtin_then_restores() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");

    // Replace the built-in `python` grammar with rust under the same
    // tag — `parse_by_name("python", …)` should now reject Python
    // syntax with a Rust grammar (root kind `source_file`, not
    // `module`).
    let prior = db.register_grammar("python", tree_sitter_rust::LANGUAGE.into());
    let tree = parse_by_name("python", "fn main() {}").expect("override parses Rust");
    assert_eq!(tree.root_node().kind(), "source_file");

    // Restore.
    db.unregister_grammar("python");
    if let Some(p) = prior {
        db.register_grammar("python", p);
    }
    // After restore, the built-in python grammar parses again.
    let tree = parse_by_name("python", "x = 1\n").expect("builtin python");
    assert_eq!(tree.root_node().kind(), "module");
}

#[test]
fn parse_by_name_unknown_errors_with_useful_message() {
    let _db = EmbeddedDatabase::new_in_memory().expect("db");
    let err = parse_by_name("hypothetical_grammar_xyz", "data").expect_err("must error");
    let msg = err.to_string();
    assert!(
        msg.contains("no tree-sitter grammar registered"),
        "got: {msg}"
    );
    assert!(msg.contains("register_grammar"), "should mention API: {msg}");
}
