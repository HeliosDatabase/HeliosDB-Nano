//! Phase-3 smoke tests for the graph-rag MVP. Feature flag
//! `graph-rag` implies `code-graph`.

#![cfg(feature = "graph-rag")]

use heliosdb_nano::{
    code_graph::CodeIndexOptions,
    graph_rag::{Direction, GraphRagOptions},
    EmbeddedDatabase, Result,
};

fn setup() -> Result<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    Ok(db)
}

fn insert(db: &EmbeddedDatabase, path: &str, lang: &str, content: &str) -> Result<()> {
    let p = path.replace('\'', "''");
    let l = lang.replace('\'', "''");
    let c = content.replace('\'', "''");
    db.execute(&format!(
        "INSERT INTO src (path, lang, content) VALUES ('{p}', '{l}', '{c}')"
    ))?;
    Ok(())
}

#[test]
fn project_and_search_finds_symbol() -> Result<()> {
    let db = setup()?;
    insert(
        &db,
        "repo.py",
        "python",
        "def helper():\n    return 42\n\ndef caller():\n    return helper()\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    let stats = db.graph_rag_project_symbols()?;
    assert!(stats.code_symbols_projected >= 2);

    let hits = db.graph_rag_search(&GraphRagOptions {
        seed_text: "helper".into(),
        seed_kinds: vec!["Function".into()],
        hops: 1,
        direction: Direction::Both,
        ..Default::default()
    })?;
    assert!(!hits.is_empty(), "expected at least one hit for 'helper'");
    assert!(hits.iter().any(|h| h.hop_distance == 0));
    Ok(())
}

#[test]
fn empty_seed_text_errors() -> Result<()> {
    let db = setup()?;
    db.graph_rag_project_symbols()?;
    let err = db.graph_rag_search(&GraphRagOptions::default());
    assert!(err.is_err(), "empty seed should error");
    Ok(())
}

#[test]
fn bfs_respects_hops() -> Result<()> {
    // a -> b -> c (both edges CALLS). Seed on "a", hops=1 should
    // return a and b but not c.
    let db = setup()?;
    insert(
        &db,
        "chain.py",
        "python",
        "def a():\n    return b()\n\ndef b():\n    return c()\n\ndef c():\n    return 1\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    db.graph_rag_project_symbols()?;

    let hits1 = db.graph_rag_search(&GraphRagOptions {
        seed_text: "a".into(),
        seed_kinds: vec!["Function".into()],
        hops: 1,
        ..Default::default()
    })?;
    let names1: Vec<_> = hits1.iter().filter_map(|h| h.title.clone()).collect();
    assert!(names1.iter().any(|t| t == "a"));
    // BFS of hops=1 from "a" should pick up the CALLS edge to b.
    // c is 2 hops away and must not be included at hops=1.
    assert!(!names1.iter().any(|t| t == "c"));

    let hits2 = db.graph_rag_search(&GraphRagOptions {
        seed_text: "a".into(),
        seed_kinds: vec!["Function".into()],
        hops: 2,
        ..Default::default()
    })?;
    let names2: Vec<_> = hits2.iter().filter_map(|h| h.title.clone()).collect();
    // With hops=2, c should show up via the a→b→c chain.
    assert!(names2.iter().any(|t| t == "c"));
    Ok(())
}
