//! v09.5: re-index cleans changed and deleted files by key, once per chunk.
#![cfg(feature = "code-graph")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use heliosdb_nano::code_graph::{code_index_with_embedder, CodeIndexOptions, DefinitionHint, Embedder, NoopEmbedder};
use heliosdb_nano::{EmbeddedDatabase, Error, Result, Value};

fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    db
}

fn put(db: &EmbeddedDatabase, table: &str, path: &str, content: &str) {
    db.execute_params(
        &format!("DELETE FROM {table} WHERE path = $1"),
        &[Value::String(path.into())],
    )
    .unwrap();
    db.execute_params(
        &format!("INSERT INTO {table} (path, lang, content) VALUES ($1, 'python', $2)"),
        &[Value::String(path.into()), Value::String(content.into())],
    )
    .unwrap();
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

fn count(db: &EmbeddedDatabase, sql: &str) -> i64 {
    int(&db.query(sql, &[]).unwrap()[0].values[0])
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "<null>".into(),
        other => format!("{other:?}"),
    }
}

/// The index with ids removed: symbols as (path, qualified, kind, line) and
/// refs as (path, from qualified, kind, line, to_name, target qualified when
/// the resolution is exact).
fn image(db: &EmbeddedDatabase) -> (BTreeSet<String>, BTreeSet<String>) {
    let syms = db
        .query(
            "SELECT f.path, s.qualified, s.kind, s.line_start FROM _hdb_code_symbols s \
             JOIN _hdb_code_files f ON f.node_id = s.file_id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| r.values.iter().map(text).collect::<Vec<_>>().join("|"))
        .collect();
    let q = db
        .query("SELECT node_id, qualified FROM _hdb_code_symbols", &[])
        .unwrap()
        .iter()
        .map(|r| (int(&r.values[0]), text(&r.values[1])))
        .collect::<std::collections::HashMap<_, _>>();
    let refs = db
        .query(
            "SELECT f.path, r.from_symbol, r.kind, r.line, r.to_name, r.to_symbol, r.resolution \
             FROM _hdb_code_symbol_refs r JOIN _hdb_code_files f ON f.node_id = r.file_id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| {
            let target = match (&r.values[5], text(&r.values[6]).as_str()) {
                (Value::Null, _) => "<none>".to_string(),
                (v, "exact") => q.get(&int(v)).cloned().unwrap_or_else(|| "<dangling>".into()),
                _ => "<bound>".to_string(),
            };
            format!(
                "{}|{}|{}|{}|{}|{}",
                text(&r.values[0]),
                q.get(&int(&r.values[1]))
                    .cloned()
                    .unwrap_or_else(|| "<dangling>".into()),
                text(&r.values[2]),
                text(&r.values[3]),
                text(&r.values[4]),
                target
            )
        })
        .collect();
    (syms, refs)
}

fn no_dangling(db: &EmbeddedDatabase) {
    assert_eq!(
        count(
            db,
            "SELECT COUNT(*) FROM _hdb_code_symbols s WHERE s.file_id NOT IN (SELECT node_id FROM _hdb_code_files)"
        ),
        0
    );
    assert_eq!(
        count(
            db,
            "SELECT COUNT(*) FROM _hdb_code_symbol_refs r WHERE r.to_symbol IS NOT NULL \
             AND r.to_symbol NOT IN (SELECT node_id FROM _hdb_code_symbols)"
        ),
        0
    );
    assert_eq!(
        count(
            db,
            "SELECT COUNT(*) FROM _hdb_code_symbol_refs r WHERE r.from_symbol NOT IN (SELECT node_id FROM _hdb_code_symbols)"
        ),
        0
    );
}

const HELPER: &str = "def helper():\n    return 42\n";
const CALLER: &str = "def run():\n    return helper()\n";

#[test]
fn deleted_source_row_is_removed_from_index() -> Result<()> {
    let db = setup();
    put(&db, "src", "helper.py", HELPER);
    put(&db, "src", "caller.py", CALLER);
    db.code_index(CodeIndexOptions::for_table("src"))?;
    assert_eq!(db.lsp_definition("helper", &DefinitionHint::default())?.len(), 1);
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM _hdb_code_symbol_refs WHERE to_name = 'helper' AND to_symbol IS NOT NULL"
        ),
        1
    );

    db.execute("DELETE FROM src WHERE path = 'helper.py'")?;
    let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
    assert_eq!(stats.files_removed, 1);
    assert!(db.lsp_definition("helper", &DefinitionHint::default())?.is_empty());
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM _hdb_code_files WHERE path = 'helper.py'"),
        0
    );
    // The inbound reference is kept but no longer points at a symbol.
    let rows = db.query(
        "SELECT to_symbol, resolution FROM _hdb_code_symbol_refs WHERE to_name = 'helper'",
        &[],
    )?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Null);
    assert_eq!(text(&rows[0].values[1]), "unresolved");
    no_dangling(&db);
    // A second run has nothing to remove.
    assert_eq!(db.code_index(CodeIndexOptions::for_table("src"))?.files_removed, 0);
    Ok(())
}

#[test]
fn rename_file_moves_symbols() -> Result<()> {
    let db = setup();
    put(&db, "src", "old.py", HELPER);
    put(&db, "src", "caller.py", CALLER);
    db.code_index(CodeIndexOptions::for_table("src"))?;
    db.execute("DELETE FROM src WHERE path = 'old.py'")?;
    put(&db, "src", "new.py", HELPER);
    let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
    assert_eq!(stats.files_removed, 1);
    let defs = db.lsp_definition("helper", &DefinitionHint::default())?;
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].path, "new.py");
    // The caller's reference was rebound to the moved definition.
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM _hdb_code_symbol_refs WHERE to_name = 'helper' AND to_symbol IS NOT NULL"
        ),
        1
    );
    no_dangling(&db);
    Ok(())
}

#[test]
fn two_source_tables_same_path_do_not_share_hash() -> Result<()> {
    let db = setup();
    db.execute("CREATE TABLE src2 (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    put(&db, "src", "same.py", HELPER);
    put(&db, "src2", "same.py", "def other():\n    return 1\n");
    let a = db.code_index(CodeIndexOptions::for_table("src"))?;
    assert_eq!(a.files_parsed, 1);
    // The identical path in the second table has different content: it must
    // be parsed, not skipped because the first table's hash is stored for it.
    let b = db.code_index(CodeIndexOptions::for_table("src2"))?;
    assert_eq!(b.files_parsed, 1, "stats: {b:?}");
    assert_eq!(b.files_unchanged, 0);
    assert_eq!(db.lsp_definition("other", &DefinitionHint::default())?.len(), 1);
    // Indexing the second table must not remove the first table's file.
    assert_eq!(b.files_removed, 0);
    assert_eq!(db.lsp_definition("helper", &DefinitionHint::default())?.len(), 1);
    // Both are unchanged on a repeat.
    assert_eq!(db.code_index(CodeIndexOptions::for_table("src"))?.files_unchanged, 1);
    assert_eq!(db.code_index(CodeIndexOptions::for_table("src2"))?.files_unchanged, 1);
    Ok(())
}

/// An embedder that works for `ok_calls` batches and then refuses.
struct FailAfter {
    ok_calls: usize,
    seen: AtomicUsize,
}

impl Embedder for FailAfter {
    fn embed(&self, _text: &str) -> Result<Option<Vec<f32>>> {
        if self.seen.fetch_add(1, Ordering::SeqCst) >= self.ok_calls {
            return Err(Error::query_execution("embedder refused"));
        }
        Ok(None)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Option<Vec<f32>>>> {
        if self.seen.fetch_add(1, Ordering::SeqCst) >= self.ok_calls {
            return Err(Error::query_execution("embedder refused"));
        }
        Ok(vec![None; texts.len()])
    }
}

#[test]
fn chunked_run_failure_in_second_chunk_keeps_first_chunk_and_reindexes_rest() -> Result<()> {
    let db = setup();
    for i in 0..4 {
        put(
            &db,
            "src",
            &format!("f{i}.py"),
            &format!("def fn_{i}():\n    return {i}\n"),
        );
    }
    let mut opts = CodeIndexOptions::for_table("src");
    opts.chunk_size = Some(2);
    opts.parallelism = Some(1);
    let failing = Box::new(FailAfter {
        ok_calls: 1,
        seen: AtomicUsize::new(0),
    });
    let err = code_index_with_embedder(&db, opts.clone(), failing).unwrap_err();
    assert!(err.to_string().contains("embedder refused"), "{err}");
    // The first chunk landed whole; the second left nothing behind.
    assert_eq!(count(&db, "SELECT COUNT(*) FROM _hdb_code_files"), 2);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM _hdb_code_symbols"), 2);
    no_dangling(&db);
    // A rerun skips the committed files and indexes the rest.
    let stats = code_index_with_embedder(&db, opts, Box::new(NoopEmbedder))?;
    assert_eq!(stats.files_unchanged, 2);
    assert_eq!(stats.files_parsed, 2);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM _hdb_code_files"), 4);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM _hdb_code_symbols"), 4);
    no_dangling(&db);
    Ok(())
}

/// After edit / add / delete the symbols and refs equal a from-scratch index
/// of the same corpus.
#[test]
fn incremental_image_equals_from_scratch_image() -> Result<()> {
    for chunk in [None, Some(2usize)] {
        let corpus = |db: &EmbeddedDatabase| {
            put(
                db,
                "src",
                "a.py",
                "def a1():\n    return a2()\n\ndef a2():\n    return b1()\n",
            );
            put(db, "src", "b.py", "def b1():\n    return c1()\n");
            put(
                db,
                "src",
                "c.py",
                "def c1():\n    return a1()\n\ndef c2():\n    return 0\n",
            );
            put(db, "src", "d.py", "def d1():\n    return a1() + b1()\n");
        };
        let mut opts = CodeIndexOptions::for_table("src");
        opts.chunk_size = chunk;

        let inc = setup();
        corpus(&inc);
        inc.code_index(opts.clone())?;
        // edit b, add e, delete c, edit d
        put(
            &inc,
            "src",
            "b.py",
            "def b1():\n    return c9()\n\ndef b2():\n    return a1()\n",
        );
        put(&inc, "src", "e.py", "def e1():\n    return b2() + a2()\n");
        inc.execute("DELETE FROM src WHERE path = 'c.py'")?;
        put(
            &inc,
            "src",
            "d.py",
            "def d1():\n    return a1()\n\ndef d2():\n    return e1()\n",
        );
        inc.code_index(opts.clone())?;
        no_dangling(&inc);

        let fresh = setup();
        corpus(&fresh);
        put(
            &fresh,
            "src",
            "b.py",
            "def b1():\n    return c9()\n\ndef b2():\n    return a1()\n",
        );
        put(&fresh, "src", "e.py", "def e1():\n    return b2() + a2()\n");
        fresh.execute("DELETE FROM src WHERE path = 'c.py'")?;
        put(
            &fresh,
            "src",
            "d.py",
            "def d1():\n    return a1()\n\ndef d2():\n    return e1()\n",
        );
        fresh.code_index(opts)?;

        let (si, ri) = image(&inc);
        let (sf, rf) = image(&fresh);
        assert_eq!(si, sf, "symbols differ (chunk {chunk:?})");
        assert_eq!(ri, rf, "refs differ (chunk {chunk:?})");
    }
    Ok(())
}

#[test]
fn concurrent_code_index_calls_serialize() -> Result<()> {
    let db = Arc::new(setup());
    for i in 0..6 {
        put(
            &db,
            "src",
            &format!("f{i}.py"),
            &format!("def fn_{i}():\n    return {i}\n"),
        );
    }
    let mut handles = Vec::new();
    for _ in 0..3 {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            db.code_index(CodeIndexOptions::for_table("src"))
        }));
    }
    let mut parsed = 0;
    for h in handles {
        parsed += h.join().unwrap()?.files_parsed;
    }
    assert_eq!(parsed, 6, "every file is parsed by exactly one of the calls");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM _hdb_code_symbols"), 6);
    no_dangling(&db);
    Ok(())
}

#[test]
fn edit_one_file_replaces_only_its_rows() -> Result<()> {
    let db = setup();
    put(&db, "src", "helper.py", HELPER);
    put(&db, "src", "caller.py", CALLER);
    db.code_index(CodeIndexOptions::for_table("src"))?;
    let caller_ids_before: BTreeSet<i64> = db
        .query("SELECT node_id FROM _hdb_code_symbols WHERE name = 'run'", &[])?
        .iter()
        .map(|r| int(&r.values[0]))
        .collect();
    put(
        &db,
        "src",
        "helper.py",
        "def helper():\n    return 43\n\ndef extra():\n    return 1\n",
    );
    let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
    assert_eq!(stats.files_parsed, 1);
    assert_eq!(stats.files_unchanged, 1);
    let caller_ids_after: BTreeSet<i64> = db
        .query("SELECT node_id FROM _hdb_code_symbols WHERE name = 'run'", &[])?
        .iter()
        .map(|r| int(&r.values[0]))
        .collect();
    assert_eq!(caller_ids_before, caller_ids_after, "an untouched file keeps its rows");
    assert_eq!(db.lsp_definition("extra", &DefinitionHint::default())?.len(), 1);
    // The inbound reference was rebound to the re-created symbol.
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM _hdb_code_symbol_refs WHERE to_name = 'helper' AND to_symbol IS NOT NULL"
        ),
        1
    );
    no_dangling(&db);
    Ok(())
}

// ---------------------------------------------------------------------------
// v09.6: targeted re-index
// ---------------------------------------------------------------------------

fn targeted(paths: &[&str]) -> CodeIndexOptions {
    let mut o = CodeIndexOptions::for_table("src");
    o.only_paths = Some(paths.iter().map(|p| (*p).to_string()).collect());
    o
}

fn three_files(db: &EmbeddedDatabase) {
    put(db, "src", "a.py", "def a1():\n    return b1()\n");
    put(db, "src", "b.py", "def b1():\n    return 1\n");
    put(db, "src", "c.py", "def c1():\n    return a1()\n");
}

#[test]
fn targeted_reindex_touches_only_listed_paths() -> Result<()> {
    let db = setup();
    three_files(&db);
    db.code_index(CodeIndexOptions::for_table("src"))?;
    // Both a.py and b.py change on disk, but only a.py is named.
    put(
        &db,
        "src",
        "a.py",
        "def a1():\n    return b1()\n\ndef a2():\n    return 2\n",
    );
    put(
        &db,
        "src",
        "b.py",
        "def b1():\n    return 1\n\ndef b2():\n    return 2\n",
    );
    let stats = db.code_index(targeted(&["a.py"]))?;
    assert_eq!(stats.files_seen, 1);
    assert_eq!(stats.files_parsed, 1);
    assert_eq!(db.lsp_definition("a2", &DefinitionHint::default())?.len(), 1);
    assert!(
        db.lsp_definition("b2", &DefinitionHint::default())?.is_empty(),
        "an unlisted file is left alone"
    );
    // The untouched files keep their rows and their inbound reference.
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM _hdb_code_symbol_refs WHERE to_name = 'a1' AND to_symbol IS NOT NULL"
        ),
        1
    );
    no_dangling(&db);
    // A listed path whose content did not change is seen and skipped.
    let again = db.code_index(targeted(&["a.py"]))?;
    assert_eq!((again.files_seen, again.files_unchanged, again.files_parsed), (1, 1, 0));
    Ok(())
}

#[test]
fn targeted_delete_removes_symbols() -> Result<()> {
    let db = setup();
    three_files(&db);
    db.code_index(CodeIndexOptions::for_table("src"))?;
    db.execute("DELETE FROM src WHERE path = 'b.py'")?;
    let stats = db.code_index(targeted(&["b.py"]))?;
    assert_eq!(stats.files_removed, 1);
    assert!(db.lsp_definition("b1", &DefinitionHint::default())?.is_empty());
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM _hdb_code_files WHERE path = 'b.py'"),
        0
    );
    assert_eq!(count(&db, "SELECT COUNT(*) FROM _hdb_code_files"), 2);
    no_dangling(&db);
    // A listed path that was never indexed and has no source row is a no-op.
    let none = db.code_index(targeted(&["never.py"]))?;
    assert_eq!((none.files_seen, none.files_removed), (0, 0));
    Ok(())
}

#[test]
fn targeted_empty_list_is_noop() -> Result<()> {
    let db = setup();
    three_files(&db);
    db.code_index(CodeIndexOptions::for_table("src"))?;
    put(&db, "src", "a.py", "def changed():\n    return 0\n");
    let stats = db.code_index(targeted(&[]))?;
    assert_eq!((stats.files_seen, stats.files_parsed, stats.files_removed), (0, 0, 0));
    assert!(db.lsp_definition("changed", &DefinitionHint::default())?.is_empty());
    Ok(())
}

#[test]
fn targeted_equals_full_run_images() -> Result<()> {
    let edit = |db: &EmbeddedDatabase| {
        put(
            db,
            "src",
            "a.py",
            "def a1():\n    return c1()\n\ndef a3():\n    return b1()\n",
        );
        db.execute("DELETE FROM src WHERE path = 'c.py'").unwrap();
        put(db, "src", "d.py", "def d1():\n    return a1() + b1()\n");
    };
    let t = setup();
    three_files(&t);
    t.code_index(CodeIndexOptions::for_table("src"))?;
    edit(&t);
    t.code_index(targeted(&["a.py", "c.py", "d.py"]))?;
    let f = setup();
    three_files(&f);
    f.code_index(CodeIndexOptions::for_table("src"))?;
    edit(&f);
    f.code_index(CodeIndexOptions::for_table("src"))?;
    assert_eq!(image(&t), image(&f));
    no_dangling(&t);
    Ok(())
}

#[test]
fn targeted_run_with_many_paths_batches_the_lookup() -> Result<()> {
    let db = setup();
    for i in 0..30 {
        put(
            &db,
            "src",
            &format!("m{i}.py"),
            &format!("def m{i}():\n    return {i}\n"),
        );
    }
    db.code_index(CodeIndexOptions::for_table("src"))?;
    let names: Vec<String> = (0..30).map(|i| format!("m{i}.py")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let stats = db.code_index(targeted(&refs))?;
    assert_eq!(
        (stats.files_seen, stats.files_unchanged, stats.files_parsed),
        (30, 30, 0)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// v09.7: cross-file references are bound while they are inserted
// ---------------------------------------------------------------------------

fn corpus_with_cross_refs(db: &EmbeddedDatabase) {
    for i in 0..12 {
        let next = (i + 1) % 12;
        let other = (i * 5 + 3) % 12;
        put(
            db,
            "src",
            &format!("m{i:02}.py"),
            &format!(
                "def f{i}():\n    return f{next}() + f{other}() + shared()\n\ndef shared():\n    return {i}\n\n\
                 def g{i}():\n    return external_{i}()\n"
            ),
        );
    }
}

/// No ref is left without a target while its name is defined, and the image of
/// a chunked run equals the single-chunk one and the old full pass.
#[test]
fn cold_index_ref_images_equal_reference() -> Result<()> {
    let build = |chunk: Option<usize>, skip: bool| -> Result<EmbeddedDatabase> {
        let db = setup();
        corpus_with_cross_refs(&db);
        let mut o = CodeIndexOptions::for_table("src");
        o.chunk_size = chunk;
        o.skip_cross_file_resolve = skip;
        db.code_index(o)?;
        if skip {
            heliosdb_nano::code_graph::storage::resolve_cross_file_references(&db)?;
        }
        Ok(db)
    };
    let reference = build(None, true)?;
    for chunk in [None, Some(64), Some(1), Some(5)] {
        let db = build(chunk, false)?;
        // Target ids differ with insertion history only through ordering, which is
        // the same here; compare the full image including the bound targets.
        let (s, r) = image(&db);
        let (sr, rr) = image(&reference);
        assert_eq!(s, sr, "symbols differ (chunk {chunk:?})");
        assert_eq!(r, rr, "refs differ (chunk {chunk:?})");
        // Every name that is defined is bound; `external_*` stays unresolved.
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM _hdb_code_symbol_refs WHERE to_symbol IS NULL AND to_name LIKE 'f%'"
            ),
            0,
            "chunk {chunk:?}"
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM _hdb_code_symbol_refs WHERE to_symbol IS NOT NULL AND to_name LIKE 'external_%'"
            ),
            0
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM _hdb_code_symbol_refs WHERE to_symbol IS NULL AND resolution <> 'unresolved'"
            ),
            0
        );
        no_dangling(&db);
        // The ids of the bound targets equal the reference too (same insertion order).
        let targets = |d: &EmbeddedDatabase| -> Vec<(i64, i64, String)> {
            d.query(
                "SELECT file_id, to_symbol, resolution FROM _hdb_code_symbol_refs WHERE to_symbol IS NOT NULL ORDER BY edge_id",
                &[],
            )
            .unwrap()
            .iter()
            .map(|r| (int(&r.values[0]), int(&r.values[1]), text(&r.values[2])))
            .collect()
        };
        assert_eq!(targets(&db), targets(&reference), "chunk {chunk:?}");
    }
    Ok(())
}

/// A name defined in two chunks is `heuristic` for every ref, including the
/// ref written (as `exact`) before the second definition existed, and every
/// such ref points at the lowest node_id.
#[test]
fn duplicate_name_across_chunks_is_heuristic_everywhere() -> Result<()> {
    let db = setup();
    put(&db, "src", "a_def.py", "def shared():\n    return 1\n");
    put(&db, "src", "b_caller.py", "def run():\n    return shared()\n");
    put(&db, "src", "c_def.py", "def shared():\n    return 2\n");
    put(&db, "src", "d_caller.py", "def run2():\n    return shared()\n");
    let mut o = CodeIndexOptions::for_table("src");
    o.chunk_size = Some(1);
    let stats = db.code_index(o)?;
    assert_eq!(stats.chunks_processed, 4);
    let lowest = count(&db, "SELECT MIN(node_id) FROM _hdb_code_symbols WHERE name = 'shared'");
    let rows = db.query(
        "SELECT to_symbol, resolution FROM _hdb_code_symbol_refs WHERE to_name = 'shared'",
        &[],
    )?;
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert_eq!(int(&r.values[0]), lowest);
        assert_eq!(text(&r.values[1]), "heuristic");
    }
    assert!(stats.tail_statements >= 1);
    Ok(())
}

#[test]
fn import_rebound_call_binds_to_symbol() -> Result<()> {
    let db = setup();
    db.execute("CREATE TABLE rs (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    db.execute_params(
        "INSERT INTO rs VALUES ('a.rs', 'rust', $1)",
        &[Value::String(
            "use crate::b::helper;\npub fn run() { helper(); }\n".into(),
        )],
    )?;
    db.execute_params(
        "INSERT INTO rs VALUES ('b.rs', 'rust', $1)",
        &[Value::String("pub fn helper() {}\n".into())],
    )?;
    db.code_index(CodeIndexOptions::for_table("rs"))?;
    let rows = db.query(
        "SELECT to_symbol, resolution, to_name FROM _hdb_code_symbol_refs WHERE kind = 'CALLS' AND to_name LIKE '%helper'",
        &[],
    )?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_ne!(
        rows[0].values[0],
        Value::Null,
        "the call through the import has a target: {rows:?}"
    );
    let helper = count(&db, "SELECT node_id FROM _hdb_code_symbols WHERE name = 'helper'");
    assert_eq!(int(&rows[0].values[0]), helper);
    assert_eq!(text(&rows[0].values[1]), "exact");
    Ok(())
}

#[test]
fn skip_then_resolve_binds_the_same_refs() -> Result<()> {
    let db = setup();
    corpus_with_cross_refs(&db);
    let mut o = CodeIndexOptions::for_table("src");
    o.skip_cross_file_resolve = true;
    db.code_index(o)?;
    let before = count(
        &db,
        "SELECT COUNT(*) FROM _hdb_code_symbol_refs WHERE to_symbol IS NULL",
    );
    assert!(before > 0);
    let bound = heliosdb_nano::code_graph::storage::resolve_cross_file_references(&db)?;
    assert!(bound > 0);
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM _hdb_code_symbol_refs WHERE to_symbol IS NULL"
        ),
        before - bound as i64
    );
    Ok(())
}
