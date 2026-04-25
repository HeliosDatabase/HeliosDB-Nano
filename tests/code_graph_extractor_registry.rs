//! Symbol-extractor pluggability for runtime-registered grammars.

#![cfg(feature = "code-graph")]

use std::sync::Arc;

use heliosdb_nano::code_graph::{
    CodeIndexOptions, StaticLanguageExtractor, SymbolExtractor,
};
use heliosdb_nano::{EmbeddedDatabase, Value};

#[test]
fn unregistered_runtime_lang_is_skipped_with_zero_symbols() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    // Use an unknown language tag — the row should be skipped, not
    // crash, not emit symbols.
    db.execute_params_returning(
        "INSERT INTO src VALUES ($1, 'cobol_no_extractor', $2)",
        &[
            Value::String("a.cob".into()),
            Value::String("PROCEDURE DIVISION.\nDISPLAY 'hi'.\n".into()),
        ],
    )
    .unwrap();
    let stats = db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    assert_eq!(stats.files_skipped, 1);
    assert_eq!(stats.symbols_written, 0);
}

#[test]
fn registered_grammar_plus_extractor_yields_symbols() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();

    // Register tree-sitter-rust under a custom tag, plus an
    // extractor that delegates to the static Rust extractor.  The
    // indexer should pick both up and produce real symbols.
    db.register_grammar("rust_alias_for_extractor", tree_sitter_rust::LANGUAGE.into());
    let extractor = Arc::new(StaticLanguageExtractor {
        language: heliosdb_nano::code_graph::parse::Language::Rust,
    });
    let prior = db.register_extractor("rust_alias_for_extractor", extractor.clone() as Arc<dyn SymbolExtractor>);
    assert!(prior.is_none());
    assert!(db.registered_extractors().contains(&"rust_alias_for_extractor".to_string()));

    db.execute_params_returning(
        "INSERT INTO src VALUES ($1, 'rust_alias_for_extractor', $2)",
        &[
            Value::String("a.rs".into()),
            Value::String("pub fn alpha() {}\npub fn beta() {}\n".into()),
        ],
    )
    .unwrap();
    let stats = db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    assert_eq!(stats.files_skipped, 0);
    assert!(stats.symbols_written >= 2, "got: {stats:?}");

    db.unregister_extractor("rust_alias_for_extractor");
    db.unregister_grammar("rust_alias_for_extractor");
}

#[test]
fn registered_grammar_without_extractor_is_skipped() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();

    // Grammar registered, NO extractor → skip rather than emit
    // empty symbol set silently.
    db.register_grammar("rust_alias_no_extractor", tree_sitter_rust::LANGUAGE.into());
    db.execute_params_returning(
        "INSERT INTO src VALUES ($1, 'rust_alias_no_extractor', $2)",
        &[
            Value::String("a.rs".into()),
            Value::String("pub fn x() {}\n".into()),
        ],
    )
    .unwrap();
    let stats = db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    assert_eq!(stats.files_skipped, 1);
    assert_eq!(stats.symbols_written, 0);

    db.unregister_grammar("rust_alias_no_extractor");
}
