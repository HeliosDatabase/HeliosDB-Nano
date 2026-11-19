//! Code-graph benchmark harness (v09.4).
//!
//! One command gives a before/after number for every code-graph speed claim:
//! cold index with a phase split, no-op re-index, re-index after editing 1 and
//! 10 files, after deleting 1 file, reopen + first query, p50/p95/p99 of the
//! LSP calls over a fixed-seed sample, graph search, the same calls through
//! `mcp::rpc::handle_rpc_with_db` (uncached and cached), rows per table,
//! data-dir bytes, peak RSS, and correctness counters.
//!
//! ```bash
//! cargo run --profile perf --features code-graph,graph-rag,mcp-endpoint \
//!     --example code_graph_bench -- --files 300 --mode memory --samples 200
//! cargo run --profile perf --features code-graph,graph-rag,mcp-endpoint \
//!     --example code_graph_bench -- --files 5000 --mode disk --dir /tmp/cgb
//! ```
//!
//! Output: text lines in the shape `benches/public/bench_tools.py parse`
//! reads (`label   <rate> files/s`), then one `CODE_GRAPH_BENCH_JSON {...}`
//! line with every metric. The corpus is generated from a fixed seed: Rust
//! files with cross-file calls, `use` imports, shared method names and a
//! handful of duplicate definitions, so the in-file resolver, the import
//! rebinding path and the cross-file pass are all exercised.

#![cfg(all(feature = "code-graph", feature = "graph-rag", feature = "mcp-endpoint"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use heliosdb_nano::code_graph::{CallDirection, CodeIndexOptions, CodeIndexStats, DefinitionHint};
use heliosdb_nano::graph_rag::{Direction, GraphRagOptions};
use heliosdb_nano::mcp::{handle_rpc_with_db, RpcRequest};
use heliosdb_nano::{EmbeddedDatabase, Result, Value};
use serde_json::{json, Value as Json};

struct Args {
    files: usize,
    mode_disk: bool,
    dir: Option<PathBuf>,
    samples: usize,
    seed: u64,
    parallelism: Option<usize>,
    chunk_size: Option<usize>,
    skip_equiv: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        files: 300,
        mode_disk: false,
        dir: None,
        samples: 200,
        seed: 0x5eed_c0de,
        parallelism: None,
        chunk_size: None,
        skip_equiv: false,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let next = |i: usize| argv.get(i + 1).cloned().unwrap_or_default();
        match argv[i].as_str() {
            "--files" => {
                a.files = next(i).parse().unwrap_or(300);
                i += 1;
            }
            "--mode" => {
                a.mode_disk = next(i) == "disk";
                i += 1;
            }
            "--dir" => {
                a.dir = Some(PathBuf::from(next(i)));
                i += 1;
            }
            "--samples" => {
                a.samples = next(i).parse().unwrap_or(200);
                i += 1;
            }
            "--seed" => {
                a.seed = next(i).parse().unwrap_or(a.seed);
                i += 1;
            }
            "--parallelism" => {
                a.parallelism = next(i).parse().ok();
                i += 1;
            }
            "--chunk-size" => {
                a.chunk_size = next(i).parse().ok();
                i += 1;
            }
            "--skip-equiv" => a.skip_equiv = true,
            other => eprintln!("ignoring unknown argument {other}"),
        }
        i += 1;
    }
    a
}

// ---------------------------------------------------------------------------
// Deterministic corpus
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const FNS_PER_FILE: usize = 12;

/// File `k` of an `n`-file corpus, version `v` (v > 0 appends edits).
fn gen_file(seed: u64, n: usize, k: usize, v: u32) -> String {
    let mut rng = Rng(seed ^ ((k as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1);
    let mut s = String::new();
    // Imports of two other modules' functions.
    let mut imported: Vec<(usize, usize)> = Vec::new();
    for _ in 0..2 {
        let j = rng.below(n);
        let a = rng.below(FNS_PER_FILE);
        if j != k {
            s.push_str(&format!("use crate::m{j}::f{j}_{a};\n"));
            imported.push((j, a));
        }
    }
    s.push_str(&format!("pub struct S{k} {{ pub v: i32 }}\n"));
    s.push_str(&format!(
        "impl S{k} {{ pub fn new() -> Self {{ S{k} {{ v: 0 }} }} pub fn helper(&self) -> i32 {{ self.v }} }}\n"
    ));
    for i in 0..FNS_PER_FILE {
        s.push_str(&format!("pub fn f{k}_{i}(x: i32) -> i32 {{\n"));
        // in-file call
        s.push_str(&format!("    let a = f{k}_{}(x);\n", (i + 1) % FNS_PER_FILE));
        // cross-file call (unique target name)
        let j = rng.below(n);
        let t = rng.below(FNS_PER_FILE);
        s.push_str(&format!("    let b = f{j}_{t}(a);\n"));
        // call through an import, when any
        if let Some((ij, ia)) = imported.get(i % 2) {
            s.push_str(&format!("    let c = f{ij}_{ia}(b);\n"));
        }
        // shared (duplicate) name: `helper` is defined in every file
        s.push_str("    let _ = S0::new().helper();\n");
        s.push_str("    a + b\n}\n");
    }
    for e in 0..v {
        s.push_str(&format!("pub fn edit_{k}_{e}(x: i32) -> i32 {{ f{k}_0(x) }}\n"));
    }
    s
}

fn path_of(k: usize) -> String {
    format!("src/m{k}.rs")
}

// ---------------------------------------------------------------------------
// Measurement helpers
// ---------------------------------------------------------------------------

fn pct(sorted_us: &[f64], p: f64) -> f64 {
    if sorted_us.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_us.len() as f64 - 1.0) * p).round() as usize;
    sorted_us[idx.min(sorted_us.len() - 1)]
}

fn dist(mut us: Vec<f64>) -> Json {
    us.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    json!({
        "n": us.len(),
        "p50_us": pct(&us, 0.50),
        "p95_us": pct(&us, 0.95),
        "p99_us": pct(&us, 0.99),
    })
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64() * 1000.0)
}

fn vm_hwm_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|n| n.parse().ok()))
        })
        .unwrap_or(0)
}

fn dir_bytes(p: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                total += dir_bytes(&path);
            } else if let Ok(m) = e.metadata() {
                total += m.len();
            }
        }
    }
    total
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        _ => 0,
    }
}

fn scalar(db: &EmbeddedDatabase, sql: &str) -> Result<i64> {
    let rows = db.query(sql, &[])?;
    Ok(rows.first().and_then(|r| r.values.first()).map(int).unwrap_or(0))
}

fn stats_json(s: &CodeIndexStats, wall_ms: f64) -> Json {
    json!({
        "wall_ms": wall_ms,
        "files_seen": s.files_seen,
        "files_parsed": s.files_parsed,
        "files_unchanged": s.files_unchanged,
        "files_removed": s.files_removed,
        "symbols_written": s.symbols_written,
        "refs_written": s.refs_written,
        "parse_ms": s.parse_elapsed_ms,
        "write_ms": s.write_elapsed_ms,
        "triage_ms": s.triage_ms,
        "cleanup_ms": s.cleanup_ms,
        "files_upsert_ms": s.files_upsert_ms,
        "symbols_insert_ms": s.symbols_insert_ms,
        "refs_insert_ms": s.refs_insert_ms,
        "cross_resolve_ms": s.cross_resolve_ms,
        "projection_ms": s.projection_ms,
        "chunks": s.chunks_processed,
        "tail_statements": s.tail_statements,
        "projection_error": s.projection_error,
    })
}

fn rpc_call(db: &EmbeddedDatabase, name: &str, args: Json) -> Json {
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: "tools/call".into(),
        params: json!({ "name": name, "arguments": args }),
    };
    serde_json::to_value(handle_rpc_with_db(db, req)).unwrap_or(Json::Null)
}

fn row_counts(db: &EmbeddedDatabase) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    for t in [
        "_hdb_code_files",
        "_hdb_code_symbols",
        "_hdb_code_symbol_refs",
        "_hdb_graph_nodes",
        "_hdb_graph_edges",
    ] {
        out.insert(
            t.to_string(),
            scalar(db, &format!("SELECT COUNT(*) FROM {t}")).unwrap_or(-1),
        );
    }
    out
}

fn open(args: &Args, fresh: bool) -> Result<(EmbeddedDatabase, Option<PathBuf>)> {
    if args.mode_disk {
        let dir = args
            .dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join(format!("code_graph_bench_{}", std::process::id())));
        if fresh && dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        std::fs::create_dir_all(&dir).map_err(|e| heliosdb_nano::Error::storage(format!("{e}")))?;
        Ok((EmbeddedDatabase::new(&dir)?, Some(dir)))
    } else {
        Ok((EmbeddedDatabase::new_in_memory()?, None))
    }
}

fn opts(args: &Args) -> CodeIndexOptions {
    let mut o = CodeIndexOptions::for_table("src");
    o.parallelism = args.parallelism;
    o.chunk_size = args.chunk_size;
    o
}

fn load_corpus(db: &EmbeddedDatabase, args: &Args) -> Result<f64> {
    let t = Instant::now();
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    db.begin()?;
    for k in 0..args.files {
        db.execute_params(
            "INSERT INTO src (path, lang, content) VALUES ($1, 'rust', $2)",
            &[
                Value::String(path_of(k)),
                Value::String(gen_file(args.seed, args.files, k, 0)),
            ],
        )?;
    }
    db.commit()?;
    Ok(t.elapsed().as_secs_f64() * 1000.0)
}

fn reindex(db: &EmbeddedDatabase, args: &Args) -> Result<(CodeIndexStats, f64)> {
    let (r, ms) = timed(|| db.code_index(opts(args)));
    Ok((r?, ms))
}

// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args = parse_args();
    let mut out: BTreeMap<&str, Json> = BTreeMap::new();
    let mut lines: Vec<String> = Vec::new();
    let rate = |label: &str, count: usize, ms: f64, lines: &mut Vec<String>| {
        let per_s = if ms > 0.0 { count as f64 / (ms / 1000.0) } else { 0.0 };
        lines.push(format!("{label}   {per_s:.1} files/s"));
    };

    out.insert(
        "config",
        json!({
            "files": args.files, "mode": if args.mode_disk { "disk" } else { "memory" },
            "samples": args.samples, "seed": args.seed, "fns_per_file": FNS_PER_FILE,
            "parallelism": args.parallelism, "chunk_size": args.chunk_size,
        }),
    );

    let (db, dir) = open(&args, true)?;
    let load_ms = load_corpus(&db, &args)?;
    out.insert("corpus_load_ms", json!(load_ms));

    // ---- cold index ----
    let (cold, cold_ms) = reindex(&db, &args)?;
    out.insert("cold_index", stats_json(&cold, cold_ms));
    rate(
        &format!("code_graph.cold_index_{}", args.files),
        args.files,
        cold_ms,
        &mut lines,
    );
    let counts = row_counts(&db);
    out.insert("rows_after_cold", json!(counts));
    out.insert("vm_hwm_kb_after_cold", json!(vm_hwm_kb()));

    // ---- no-op re-index ----
    let (noop, noop_ms) = reindex(&db, &args)?;
    out.insert("noop_reindex", stats_json(&noop, noop_ms));
    rate(
        &format!("code_graph.noop_reindex_{}", args.files),
        args.files,
        noop_ms,
        &mut lines,
    );

    // ---- edit 1 file, edit 10 files ----
    let mut version = 0u32;
    let mut edit = |ks: &[usize], version: &mut u32| -> Result<()> {
        *version += 1;
        for &k in ks {
            db.execute_params(
                "UPDATE src SET content = $1 WHERE path = $2",
                &[
                    Value::String(gen_file(args.seed, args.files, k, *version)),
                    Value::String(path_of(k)),
                ],
            )?;
        }
        Ok(())
    };
    edit(&[args.files / 2], &mut version)?;
    let (e1, e1_ms) = reindex(&db, &args)?;
    out.insert("reindex_1_file", stats_json(&e1, e1_ms));
    lines.push(format!(
        "code_graph.reindex_1_file_{}   {:.2} txn/s",
        args.files,
        1000.0 / e1_ms.max(0.001)
    ));
    let ten: Vec<usize> = (0..10).map(|i| (i * args.files / 10 + 1) % args.files).collect();
    edit(&ten, &mut version)?;
    let (e10, e10_ms) = reindex(&db, &args)?;
    out.insert("reindex_10_files", stats_json(&e10, e10_ms));

    // ---- delete 1 file ----
    let victim = args.files / 3;
    let victim_symbol = format!("f{victim}_3");
    let before_del = db.lsp_definition(&victim_symbol, &DefinitionHint::default())?.len();
    db.execute_params("DELETE FROM src WHERE path = $1", &[Value::String(path_of(victim))])?;
    let (del, del_ms) = reindex(&db, &args)?;
    out.insert("reindex_delete_1_file", stats_json(&del, del_ms));
    let still_resolvable = db.lsp_definition(&victim_symbol, &DefinitionHint::default())?.len();

    // ---- graph (counters need the projection to have run) ----
    let sample = {
        let mut rng = Rng(args.seed ^ 0xABCD);
        let rows = db.query(
            "SELECT node_id, name FROM _hdb_code_symbols WHERE kind = 'function'",
            &[],
        )?;
        let mut picks: Vec<(i64, String)> = Vec::new();
        if !rows.is_empty() {
            for _ in 0..args.samples {
                let r = &rows[rng.below(rows.len())];
                let name = match r.values.get(1) {
                    Some(Value::String(s)) => s.clone(),
                    _ => continue,
                };
                picks.push((int(&r.values[0]), name));
            }
        }
        picks
    };

    // ---- LSP latencies (embedded API) ----
    let mut def_us = Vec::new();
    let mut ref_us = Vec::new();
    let mut hier_us = Vec::new();
    let mut hover_us = Vec::new();
    for (id, name) in &sample {
        let t = Instant::now();
        let _ = db.lsp_definition(name, &DefinitionHint::default())?;
        def_us.push(t.elapsed().as_secs_f64() * 1e6);
        let t = Instant::now();
        let _ = db.lsp_references(*id)?;
        ref_us.push(t.elapsed().as_secs_f64() * 1e6);
        let t = Instant::now();
        let _ = db.lsp_call_hierarchy(*id, CallDirection::Incoming, 3)?;
        hier_us.push(t.elapsed().as_secs_f64() * 1e6);
        let t = Instant::now();
        let _ = db.lsp_hover(*id)?;
        hover_us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let p95 = |v: &Vec<f64>| {
        let mut s = v.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        pct(&s, 0.95)
    };
    lines.push(format!(
        "code_graph.lsp_definition_p95   {:.1} lookups/s",
        1e6 / p95(&def_us).max(0.001)
    ));
    out.insert(
        "lsp",
        json!({
            "definition": dist(def_us), "references": dist(ref_us),
            "call_hierarchy_in_depth3": dist(hier_us), "hover": dist(hover_us),
        }),
    );

    // ---- document symbols + graph search through MCP, uncached vs cached ----
    let mut docsym_us = Vec::new();
    let mut mcp_def_uncached = Vec::new();
    let mut mcp_def_cached = Vec::new();
    for (i, (_, name)) in sample.iter().enumerate() {
        let path = path_of((i * 7) % args.files);
        let t = Instant::now();
        let _ = rpc_call(&db, "helios_lsp_document_symbols", json!({ "path": path, "nonce": i }));
        docsym_us.push(t.elapsed().as_secs_f64() * 1e6);
        let t = Instant::now();
        let _ = rpc_call(&db, "helios_lsp_definition", json!({ "name": name, "nonce_u": i }));
        mcp_def_uncached.push(t.elapsed().as_secs_f64() * 1e6);
        let t = Instant::now();
        let _ = rpc_call(&db, "helios_lsp_definition", json!({ "name": name, "nonce_u": i }));
        mcp_def_cached.push(t.elapsed().as_secs_f64() * 1e6);
    }
    out.insert(
        "mcp",
        json!({
            "document_symbols": dist(docsym_us),
            "definition_uncached": dist(mcp_def_uncached),
            "definition_cached": dist(mcp_def_cached),
        }),
    );

    // ---- graph search ----
    let mut search_us = Vec::new();
    let mut stmts_per_search = 0u64;
    for (_, name) in sample.iter().take(args.samples.min(50)) {
        let o = GraphRagOptions {
            seed_text: name.clone(),
            hops: 2,
            limit: 30,
            direction: Direction::Both,
            ..Default::default()
        };
        let s0 = heliosdb_nano::graph_rag::search_statement_count();
        let t = Instant::now();
        let _ = db.graph_rag_search(&o)?;
        search_us.push(t.elapsed().as_secs_f64() * 1e6);
        stmts_per_search = stmts_per_search.max(heliosdb_nano::graph_rag::search_statement_count() - s0);
    }
    lines.push(format!(
        "code_graph.graph_search_p95   {:.1} lookups/s",
        1e6 / p95(&search_us).max(0.001)
    ));
    out.insert(
        "graph_search",
        json!({ "latency": dist(search_us), "max_statements_per_search": stmts_per_search }),
    );

    // ---- counters ----
    let node_without_symbol = {
        let syms: HashSet<i64> = db
            .query("SELECT node_id FROM _hdb_code_symbols", &[])?
            .iter()
            .map(|r| int(&r.values[0]))
            .collect();
        let mut missing = 0u64;
        for r in db.query(
            "SELECT source_ref FROM _hdb_graph_nodes WHERE source_ref LIKE 'code_symbol:%'",
            &[],
        )? {
            if let Some(Value::String(s)) = r.values.first() {
                let id = s.trim_start_matches("code_symbol:").parse::<i64>().unwrap_or(-1);
                if !syms.contains(&id) {
                    missing += 1;
                }
            }
        }
        missing
    };
    let bound_null = scalar(
        &db,
        "SELECT COUNT(*) FROM _hdb_code_symbol_refs WHERE resolution <> 'unresolved' AND to_symbol IS NULL",
    )?;
    out.insert(
        "counters",
        json!({
            "deleted_file_symbols_before_delete": before_del,
            "deleted_file_still_resolvable": still_resolvable,
            "deleted_file_rows_left": scalar(&db, &format!("SELECT COUNT(*) FROM _hdb_code_files WHERE path = '{}'", path_of(victim)))?,
            "graph_nodes_without_symbol": node_without_symbol,
            "refs_nonunresolved_with_null_to_symbol": bound_null,
            "statements_per_graph_search": stmts_per_search,
        }),
    );
    out.insert("rows_final", json!(row_counts(&db)));

    // ---- cross-checks ----
    if !args.skip_equiv {
        let mut checks: BTreeMap<&str, bool> = BTreeMap::new();
        // SQL join row counts equal a Rust-side join; index-served == scan.
        let files: HashMap<i64, String> = db
            .query("SELECT node_id, path FROM _hdb_code_files", &[])?
            .iter()
            .filter_map(|r| match r.values.get(1) {
                Some(Value::String(p)) => Some((int(&r.values[0]), p.clone())),
                _ => None,
            })
            .collect();
        let all_syms = db.query("SELECT node_id, file_id, name FROM _hdb_code_symbols", &[])?;
        let mut ok_join = true;
        let mut ok_scan = true;
        for (_, name) in sample.iter().take(25) {
            let via_api: Vec<i64> = db
                .lsp_definition(name, &DefinitionHint::default())?
                .iter()
                .map(|d| d.symbol_id)
                .collect();
            let mut by_scan: Vec<i64> = all_syms
                .iter()
                .filter(|r| matches!(r.values.get(2), Some(Value::String(n)) if n == name))
                .filter(|r| files.contains_key(&int(&r.values[1])))
                .map(|r| int(&r.values[0]))
                .collect();
            by_scan.sort_unstable();
            let mut sorted_api = via_api.clone();
            sorted_api.sort_unstable();
            ok_join &= sorted_api == by_scan;
            ok_scan &= by_scan.len() == via_api.len();
        }
        checks.insert("sql_join_equals_rust_join", ok_join);
        checks.insert("index_lookup_equals_scan", ok_scan);
        // parallelism = 1 equals parallelism = N, on a fresh in-memory copy
        let small = Args {
            files: args.files.min(60),
            mode_disk: false,
            dir: None,
            samples: 0,
            seed: args.seed,
            parallelism: Some(1),
            chunk_size: None,
            skip_equiv: true,
        };
        let image = |par: Option<usize>| -> Result<(i64, i64)> {
            let (d, _) = open(&small, true)?;
            load_corpus(&d, &small)?;
            let mut o = opts(&small);
            o.parallelism = par;
            d.code_index(o)?;
            Ok((
                scalar(&d, "SELECT COUNT(*) FROM _hdb_code_symbols")?,
                scalar(&d, "SELECT COUNT(*) FROM _hdb_code_symbol_refs")?,
            ))
        };
        checks.insert("parallelism_1_equals_parallelism_n", image(Some(1))? == image(Some(4))?);
        out.insert("cross_checks", json!(checks));
    }

    // ---- reopen + first query (disk only) ----
    out.insert("vm_hwm_kb", json!(vm_hwm_kb()));
    if let Some(d) = &dir {
        drop(db);
        out.insert("data_dir_bytes", json!(dir_bytes(d)));
        let (r, reopen_ms) = timed(|| EmbeddedDatabase::new(d));
        let db2 = r?;
        let name = sample.first().map(|s| s.1.clone()).unwrap_or_else(|| "f0_0".into());
        let (_, first_ms) = timed(|| db2.lsp_definition(&name, &DefinitionHint::default()));
        out.insert("reopen_ms", json!(reopen_ms));
        out.insert("first_query_after_reopen_ms", json!(first_ms));
    } else {
        out.insert("reopen_ms", Json::Null);
    }

    for l in &lines {
        println!("{l}");
    }
    println!(
        "CODE_GRAPH_BENCH_JSON {}",
        serde_json::to_string(&out).unwrap_or_default()
    );
    Ok(())
}
