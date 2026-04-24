//! Code-graph pilot runner.
//!
//! Indexes every Rust / Python / TypeScript file under a given
//! directory into a persistent HeliosDB-Nano instance at
//! `.helios-index/heliosdb-data`, then runs a handful of LSP-shaped
//! queries and prints timings. This is the forcing-function
//! described in `FEATURE_REQUEST_pilot_helios_corpus.md`, run against
//! whatever repo you point it at.
//!
//! Run:
//!
//! ```bash
//! cargo run --release --features code-graph --example code_graph_pilot -- src
//! ```
//!
//! Arguments:
//!   [1] (optional) source directory, defaults to `src`
//!   [2] (optional) .helios-index base directory, defaults to `.helios-index`
//!
//! The pilot prints:
//!   - File / symbol / ref counts
//!   - Parse + index wall time
//!   - 4 canonical query timings (lsp_definition, lsp_references,
//!     lsp_call_hierarchy, lsp_hover)
//!   - Totals (corpus size, ops/sec-equivalent)

#![cfg(feature = "code-graph")]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use heliosdb_nano::{
    code_graph::{CodeIndexOptions, DefinitionHint},
    EmbeddedDatabase, Result, Value,
};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let source_dir = args.get(1).cloned().unwrap_or_else(|| "src".into());
    let index_base = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| ".helios-index".into());

    println!("=== HeliosDB-Nano code-graph pilot ===");
    println!("source:       {source_dir}");
    println!("index base:   {index_base}");

    let data_dir = Path::new(&index_base).join("heliosdb-data");
    fs::create_dir_all(&data_dir).map_err(|e| {
        heliosdb_nano::Error::storage(format!("failed to create {data_dir:?}: {e}"))
    })?;

    let db = EmbeddedDatabase::new(&data_dir)?;
    // Bootstrap source table. IF NOT EXISTS so repeated runs work.
    db.execute(
        r#"CREATE TABLE IF NOT EXISTS src (
             path TEXT PRIMARY KEY,
             lang TEXT,
             content TEXT,
             size_bytes INTEGER
           )"#,
    )?;

    // ------------------------------------------------------------------
    // Step 1 — walk + upsert the source tree
    // ------------------------------------------------------------------
    let walk_start = Instant::now();
    let mut files_seen = 0u64;
    let mut bytes_total = 0u64;
    let mut by_lang: std::collections::BTreeMap<&str, u64> = Default::default();
    walk(Path::new(&source_dir), &mut |path| -> Result<()> {
        let Some((lang_name, _)) = lang_from_path(path) else {
            return Ok(());
        };
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Ok(()),
        };
        let rel = path
            .strip_prefix(&source_dir)
            .unwrap_or(path)
            .display()
            .to_string();
        let bytes = content.len() as i64;
        // Parameterised insert so source contents with embedded
        // quotes / backslashes / `$1` literals round-trip cleanly.
        db.execute_params_returning(
            "DELETE FROM src WHERE path = $1",
            &[Value::String(rel.clone())],
        )?;
        db.execute_params_returning(
            "INSERT INTO src (path, lang, content, size_bytes) \
             VALUES ($1, $2, $3, $4)",
            &[
                Value::String(rel.clone()),
                Value::String(lang_name.to_string()),
                Value::String(content),
                Value::Int8(bytes),
            ],
        )?;
        files_seen += 1;
        bytes_total += bytes as u64;
        *by_lang.entry(lang_name).or_default() += 1;
        Ok(())
    })?;
    let walk_ms = walk_start.elapsed().as_millis();

    println!();
    println!("--- Step 1: walk + upsert ---");
    println!("files:        {files_seen}");
    println!("bytes:        {bytes_total}");
    for (lang, n) in &by_lang {
        println!("  {lang:<10}  {n} files");
    }
    println!("wall:         {walk_ms} ms");

    // ------------------------------------------------------------------
    // Step 2 — code_index (parse + symbol + cross-file resolver)
    // ------------------------------------------------------------------
    let idx_start = Instant::now();
    let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
    let idx_ms = idx_start.elapsed().as_millis();
    let sym_total = stats.symbols_written;
    let ref_total = stats.refs_written;
    println!();
    println!("--- Step 2: code_index ---");
    println!("files parsed:   {}", stats.files_parsed);
    println!("files skipped:  {}", stats.files_skipped);
    println!("symbols:        {sym_total}");
    println!("refs:           {ref_total}");
    println!("languages:      {:?}", stats.languages_seen);
    println!("wall:           {idx_ms} ms");
    if stats.files_parsed > 0 {
        let per_file = idx_ms as f64 / stats.files_parsed as f64;
        let per_kb = idx_ms as f64 / ((bytes_total as f64) / 1024.0);
        println!("throughput:     {per_file:.2} ms/file, {per_kb:.3} ms/KB");
    }

    // ------------------------------------------------------------------
    // Step 3 — canonical queries
    // ------------------------------------------------------------------
    println!();
    println!("--- Step 3: canonical queries ---");

    // Pick a handful of symbol names that almost certainly exist in
    // Nano's own source tree. Callers can edit these freely.
    let probes: &[&str] = &[
        "EmbeddedDatabase",
        "code_index",
        "lsp_definition",
        "ProductQuantizer",
    ];
    for &name in probes {
        let t = Instant::now();
        let defs = db.lsp_definition(name, &DefinitionHint::default())?;
        let ms = t.elapsed().as_millis();
        let preview = defs
            .first()
            .map(|d| format!("{}@{}:{}", d.path, d.line, d.qualified))
            .unwrap_or_else(|| "<no match>".into());
        println!(
            "lsp_definition({name:<22}): {n:>4} hits | {ms:>4} ms | {preview}",
            n = defs.len(),
        );
    }

    // lsp_references against the first definition we find for `code_index`.
    let defs = db.lsp_definition("code_index", &DefinitionHint::default())?;
    if let Some(first) = defs.first() {
        let t = Instant::now();
        let refs = db.lsp_references(first.symbol_id)?;
        let ms = t.elapsed().as_millis();
        println!(
            "lsp_references(code_index)          : {n:>4} refs | {ms:>4} ms",
            n = refs.len()
        );

        let t = Instant::now();
        let ch = db.lsp_call_hierarchy(
            first.symbol_id,
            heliosdb_nano::code_graph::lsp::CallDirection::Incoming,
            3,
        )?;
        let ms = t.elapsed().as_millis();
        println!(
            "lsp_call_hierarchy(code_index, in, 3): {n:>4} nodes | {ms:>4} ms",
            n = ch.len()
        );

        let t = Instant::now();
        let hv = db.lsp_hover(first.symbol_id)?;
        let ms = t.elapsed().as_millis();
        println!(
            "lsp_hover(code_index)               : {present} | {ms:>4} ms",
            present = if hv.is_some() { "ok" } else { "none" }
        );
    }

    // ------------------------------------------------------------------
    // Step 4 — raw-table introspection (sanity check)
    // ------------------------------------------------------------------
    println!();
    println!("--- Step 4: raw-table sanity ---");
    for (table, col) in &[
        ("_hdb_code_files", "node_id"),
        ("_hdb_code_symbols", "node_id"),
        ("_hdb_code_symbol_refs", "edge_id"),
    ] {
        let t = Instant::now();
        let rows = db.query(&format!("SELECT count(*) FROM {table}"), &[])?;
        let ms = t.elapsed().as_millis();
        let n: i64 = rows
            .first()
            .and_then(|r| r.values.first())
            .map(|v| match v {
                heliosdb_nano::Value::Int4(n) => *n as i64,
                heliosdb_nano::Value::Int8(n) => *n,
                _ => -1,
            })
            .unwrap_or(-1);
        println!("{table:<24} rows={n:>6} count_scan={ms:>4} ms  (pk={col})");
    }

    println!();
    println!("=== done ===");
    Ok(())
}

fn walk(root: &Path, f: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let md = match fs::metadata(&cur) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.is_dir() {
            // Skip common build/output directories.
            if let Some(name) = cur.file_name().and_then(|s| s.to_str()) {
                if matches!(name, "target" | "node_modules" | ".git" | "__pycache__" | ".helios-index") {
                    continue;
                }
            }
            if let Ok(rd) = fs::read_dir(&cur) {
                for e in rd.flatten() {
                    stack.push(e.path());
                }
            }
        } else if md.is_file() {
            f(&cur)?;
        }
    }
    Ok(())
}

fn lang_from_path(p: &Path) -> Option<(&'static str, &'static str)> {
    let ext = p.extension()?.to_str()?;
    match ext {
        "rs" => Some(("rust", "rust")),
        "py" => Some(("python", "python")),
        "ts" => Some(("typescript", "typescript")),
        "tsx" => Some(("tsx", "tsx")),
        "js" | "mjs" | "cjs" => Some(("javascript", "javascript")),
        _ => None,
    }
}
