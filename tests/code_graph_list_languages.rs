//! `hdb_code_languages` system view: surfaces the live grammar set
//! (static + dynamically-registered) as a queryable table.

#![cfg(feature = "code-graph")]

use heliosdb_nano::EmbeddedDatabase;

#[test]
fn lists_all_static_languages() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let rows = db.query("SELECT name, source FROM hdb_code_languages", &[]).unwrap();
    let names: Vec<String> = rows
        .iter()
        .filter_map(|t| match t.values.first() {
            Some(heliosdb_nano::Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    for expected in [
        "go",
        "javascript",
        "markdown",
        "python",
        "rust",
        "sql",
        "tsx",
        "typescript",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected} in {names:?}");
    }
    // All static.
    for t in &rows {
        let source = match t.values.get(1) {
            Some(heliosdb_nano::Value::String(s)) => s.clone(),
            other => panic!("unexpected source: {other:?}"),
        };
        assert!(
            source == "static" || source == "runtime",
            "unexpected source: {source}"
        );
    }
}

#[test]
fn runtime_grammar_shows_up_after_registration() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.register_grammar("custom_lang_for_test", tree_sitter_rust::LANGUAGE.into());

    let rows = db
        .query(
            "SELECT name, source FROM hdb_code_languages WHERE name = 'custom_lang_for_test'",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    let source = match rows[0].values.get(1) {
        Some(heliosdb_nano::Value::String(s)) => s.clone(),
        other => panic!("unexpected source: {other:?}"),
    };
    assert_eq!(source, "runtime");

    db.unregister_grammar("custom_lang_for_test");
    let rows_after = db
        .query(
            "SELECT name FROM hdb_code_languages WHERE name = 'custom_lang_for_test'",
            &[],
        )
        .unwrap();
    assert!(rows_after.is_empty());
}

#[test]
fn runtime_grammar_overriding_static_shows_runtime_source() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    // Register a grammar under the same tag as a built-in to verify
    // shadowing surfaces as `runtime` in the view.
    db.register_grammar("python", tree_sitter_python::LANGUAGE.into());
    let rows = db
        .query(
            "SELECT source FROM hdb_code_languages WHERE name = 'python'",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    let source = match rows[0].values.first() {
        Some(heliosdb_nano::Value::String(s)) => s.clone(),
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(source, "runtime");

    // Restore for any subsequent tests in the same process — the
    // grammar registry is process-static.
    db.unregister_grammar("python");
}
