//! v09.2: the MCP result cache never serves an answer computed before the
//! last write to its database.
#![cfg(all(feature = "mcp-endpoint", feature = "code-graph"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use std::sync::Mutex;

use heliosdb_nano::code_graph::CodeIndexOptions;
use heliosdb_nano::mcp::{call_tool, result_cache};
use heliosdb_nano::EmbeddedDatabase;
use serde_json::json;

/// The cache and its counters are process-global; serialise the tests.
static SERIAL: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    result_cache::_set_disabled_for_tests(false);
    result_cache::_set_ignore_epoch_for_tests(false);
    result_cache::_clear_for_tests();
    g
}

fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    db.execute("INSERT INTO src VALUES ('a.rs', 'rust', 'pub fn alpha() {}')")
        .unwrap();
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    db
}

fn count(db: &EmbeddedDatabase, name: &str) -> u64 {
    let out = call_tool(Some(db), "helios_lsp_definition", json!({ "name": name }));
    assert!(!out.is_error, "{:?}", out.payload);
    out.payload["count"].as_u64().unwrap()
}

#[test]
fn repeated_call_without_write_is_a_hit() {
    let _g = lock();
    let db = setup();
    assert_eq!(count(&db, "alpha"), 1);
    let before = result_cache::stats().hits;
    assert_eq!(count(&db, "alpha"), 1);
    assert_eq!(result_cache::stats().hits, before + 1);
}

#[test]
fn lsp_definition_reflects_reindex_via_db_execute() {
    let _g = lock();
    let db = setup();
    assert_eq!(count(&db, "beta"), 0);
    db.execute("UPDATE src SET content = 'pub fn alpha() {} pub fn beta() {}' WHERE path = 'a.rs'")
        .unwrap();
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    assert_eq!(count(&db, "beta"), 1, "answer computed before the write was served");
    assert!(result_cache::stats().stale_drops >= 1);
}

#[test]
fn lsp_definition_reflects_write_via_heliosdb_query_dml() {
    let _g = lock();
    let db = setup();
    assert_eq!(count(&db, "alpha"), 1);
    let out = call_tool(
        Some(&db),
        "heliosdb_query",
        json!({ "sql": "DELETE FROM _hdb_code_symbols WHERE name = 'alpha'" }),
    );
    assert!(!out.is_error, "{:?}", out.payload);
    assert_eq!(count(&db, "alpha"), 0);
}

#[test]
fn lsp_definition_reflects_reindex_via_code_index() {
    let _g = lock();
    let db = setup();
    assert_eq!(count(&db, "gamma"), 0);
    db.execute("INSERT INTO src VALUES ('b.rs', 'rust', 'pub fn gamma() {}')")
        .unwrap();
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    assert_eq!(count(&db, "gamma"), 1);
}

#[test]
fn lsp_definition_reflects_reindex_via_auto_reparse() {
    let _g = lock();
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    db.execute("CREATE AST INDEX idx_ar ON src (content) USING tree_sitter(lang) WITH (auto_reparse = true)")
        .unwrap();
    assert_eq!(count(&db, "delta"), 0);
    db.execute("INSERT INTO src VALUES ('d.rs', 'rust', 'pub fn delta() {}')")
        .unwrap();
    assert_eq!(count(&db, "delta"), 1);
}

#[test]
fn write_during_handler_is_not_published() {
    let _g = lock();
    let db = setup();
    // Unit-level counterpart lives in result_cache.rs
    // (epoch_moved_during_handler_is_not_published); here: a write between two
    // identical calls never leaves a stale entry reachable.
    assert_eq!(count(&db, "alpha"), 1);
    db.execute("DELETE FROM _hdb_code_symbols WHERE name = 'alpha'")
        .unwrap();
    assert_eq!(count(&db, "alpha"), 0);
    assert_eq!(count(&db, "alpha"), 0);
}

#[test]
fn capacity_zero_disables() {
    let _g = lock();
    let db = setup();
    result_cache::_set_disabled_for_tests(true);
    assert_eq!(count(&db, "alpha"), 1);
    assert_eq!(count(&db, "alpha"), 1);
    let s = result_cache::stats();
    assert_eq!(s.hits, 0);
    assert_eq!(s.size, 0);
    assert_eq!(s.capacity, 0);
    result_cache::_set_disabled_for_tests(false);
}

#[test]
fn entries_do_not_cross_databases() {
    let _g = lock();
    let a = setup();
    let b = EmbeddedDatabase::new_in_memory().unwrap();
    b.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    b.code_index(CodeIndexOptions::for_table("src")).unwrap();
    assert_eq!(count(&a, "alpha"), 1);
    assert_eq!(count(&b, "alpha"), 0);
}

/// The pre-fix lookup (no epoch check) serves the stale answer: this is what
/// the other tests in this file fail on without the epoch stamp.
#[test]
fn without_the_epoch_check_a_stale_answer_is_served() {
    let _g = lock();
    let db = setup();
    result_cache::_set_ignore_epoch_for_tests(true);
    assert_eq!(count(&db, "beta"), 0);
    db.execute("UPDATE src SET content = 'pub fn alpha() {} pub fn beta() {}' WHERE path = 'a.rs'")
        .unwrap();
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    assert_eq!(
        count(&db, "beta"),
        0,
        "the old cache lookup serves the pre-write answer"
    );
    result_cache::_set_ignore_epoch_for_tests(false);
    assert_eq!(count(&db, "beta"), 1);
}
