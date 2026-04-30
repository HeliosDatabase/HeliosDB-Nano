//! Storage layer for the code-graph. Materialises a set of flat-prefixed
//! tables the engine treats as plain user tables:
//!
//! - `_hdb_code_files` — one row per source file ingested.
//! - `_hdb_code_ast_nodes` — (phase 2) full AST node tree.
//! - `_hdb_code_symbols` — one row per named definition.
//! - `_hdb_code_symbol_refs` — one edge per resolved reference.
//!
//! Phase 1 writes `files`, `symbols`, and `symbol_refs` (no
//! `ast_nodes`; coarse-grain only). The DDL is issued idempotently on
//! first `code_index` call so embedded callers don't have to think
//! about schema management.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{EmbeddedDatabase, Error, Result, Tuple, Value};

use super::embed::{Embedder, NoopEmbedder};
use super::parse::{self, Language};
use super::resolver::{resolve_in_file, Resolution};
use super::symbols::{extract, Symbol};

/// Statically-supported languages. Mirrors the variants in
/// [`Language`] so the system view planned for phase 2
/// (`hdb_code.list_languages`) and the per-row
/// `_hdb_code_files.lang` column stay in sync.
///
/// Dynamically-registered grammars (via
/// [`crate::code_graph::parse::register_grammar`]) live alongside
/// these but aren't enumerated here — pull
/// [`crate::code_graph::parse::registered_grammars`] for the live
/// dynamic list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedLanguage {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
    Markdown,
    Sql,
}

impl SupportedLanguage {
    pub fn as_str(self) -> &'static str {
        Language::from(self).as_str()
    }

    /// Every statically-supported language. Stable order — callers
    /// rendering a system view can render these as-is.
    pub fn all() -> &'static [SupportedLanguage] {
        &[
            SupportedLanguage::Rust,
            SupportedLanguage::Python,
            SupportedLanguage::TypeScript,
            SupportedLanguage::Tsx,
            SupportedLanguage::JavaScript,
            SupportedLanguage::Go,
            SupportedLanguage::Markdown,
            SupportedLanguage::Sql,
        ]
    }
}

impl From<SupportedLanguage> for Language {
    fn from(s: SupportedLanguage) -> Self {
        match s {
            SupportedLanguage::Rust => Language::Rust,
            SupportedLanguage::Python => Language::Python,
            SupportedLanguage::TypeScript => Language::TypeScript,
            SupportedLanguage::Tsx => Language::Tsx,
            SupportedLanguage::JavaScript => Language::JavaScript,
            SupportedLanguage::Go => Language::Go,
            SupportedLanguage::Markdown => Language::Markdown,
            SupportedLanguage::Sql => Language::Sql,
        }
    }
}

impl From<Language> for SupportedLanguage {
    fn from(l: Language) -> Self {
        match l {
            Language::Rust => SupportedLanguage::Rust,
            Language::Python => SupportedLanguage::Python,
            Language::TypeScript => SupportedLanguage::TypeScript,
            Language::Tsx => SupportedLanguage::Tsx,
            Language::JavaScript => SupportedLanguage::JavaScript,
            Language::Go => SupportedLanguage::Go,
            Language::Markdown => SupportedLanguage::Markdown,
            Language::Sql => SupportedLanguage::Sql,
        }
    }
}

/// Source table expected to have at least `(path TEXT PRIMARY KEY,
/// content TEXT, lang TEXT)`. Other columns are fine and are ignored.
#[derive(Debug, Clone)]
pub struct CodeIndexOptions {
    /// User-table name whose rows will be parsed. Must have
    /// `(path TEXT PRIMARY KEY, content TEXT, lang TEXT)`.
    pub source_table: String,
    /// When true, bodies are embedded via the configured endpoint and
    /// written to `_hdb_code_symbols.body_vec`. When no endpoint is
    /// configured (default), bodies stay NULL but BM25 still works.
    pub embed_bodies: bool,
    /// Optional external embedding endpoint. `None` → `NoopEmbedder`.
    pub embed_endpoint: Option<String>,
    /// Optional bearer token for the embedding endpoint.
    pub embed_bearer: Option<String>,
    /// Re-extract even if the file row hash has not changed.
    /// Default `false` — set by incremental-reparse callers.
    pub force_reparse: bool,
    /// Worker count for the parallel parse + extract phase. `None` =
    /// auto (`min(num_cpus, 8)`); `Some(1)` forces serial execution
    /// (used by the equivalence test). The phase runs on a *dedicated*
    /// rayon `ThreadPool` so it never steals threads from the global
    /// pool that handles live OLTP traffic in daemon mode.
    pub parallelism: Option<usize>,
    /// Bound the in-flight memory footprint of the parse phase by
    /// processing the corpus in batches of `n` files: parse a chunk in
    /// parallel, drain its writes serially, then move to the next
    /// chunk. `None` (default) = single chunk = max parse throughput,
    /// optimal for small/medium corpora where holding all parsed
    /// trees + symbol buffers in RAM at once is fine. `Some(n)` caps
    /// peak memory at `n × avg_parsed_file_bytes` instead of
    /// `corpus_size × …`. Recommended for corpora ≥ 10 K files.
    pub chunk_size: Option<usize>,
}

impl CodeIndexOptions {
    pub fn for_table(name: impl Into<String>) -> Self {
        Self {
            source_table: name.into(),
            embed_bodies: false,
            embed_endpoint: None,
            embed_bearer: None,
            force_reparse: false,
            parallelism: None,
            chunk_size: None,
        }
    }

    /// Resolve the configured parallelism to a concrete worker count.
    /// Respects `Some(n)` (clamped to `>= 1`) and otherwise picks
    /// `min(num_cpus, 8)` so we don't fan out beyond the working set
    /// the parse phase can keep hot in cache.
    pub(crate) fn resolved_parallelism(&self) -> usize {
        if let Some(n) = self.parallelism {
            return n.max(1);
        }
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        cores.min(8).max(1)
    }
}

/// Result of a `code_index(...)` call. All counts are cumulative for
/// the single invocation.
#[derive(Debug, Clone, Default)]
pub struct CodeIndexStats {
    pub files_seen: u64,
    pub files_parsed: u64,
    pub files_skipped: u64,
    /// Files whose content-hash matched the stored sha256 and that
    /// therefore skipped the parse + re-insert cycle entirely.
    pub files_unchanged: u64,
    pub symbols_written: u64,
    pub refs_written: u64,
    pub embed_calls: u64,
    pub languages_seen: Vec<String>,
    /// Wall-clock time spent in the parallel parse + extract phase,
    /// in milliseconds. Excludes DB read/write time.
    pub parse_elapsed_ms: u64,
    /// Wall-clock time spent in the serial write phase (upserts +
    /// symbol/ref inserts + cross-file resolve), in milliseconds.
    pub write_elapsed_ms: u64,
    /// Worker count used by the parse phase (resolved from
    /// `CodeIndexOptions::parallelism`).
    pub parse_workers: u32,
    /// Number of parse → write batches the corpus was split into.
    /// `1` = single chunk (default for all corpora < `chunk_size`,
    /// or unbounded when `chunk_size = None`); `>1` only when the
    /// caller opted into bounded-memory mode via
    /// `CodeIndexOptions::chunk_size`.
    pub chunks_processed: u32,
}

/// Tracks whether `CREATE EXTENSION hdb_code` has been executed in the
/// current process. Purely advisory today — `code_index` also
/// bootstraps on first call, so callers can use either entry point.
static EXTENSION_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Record that the extension has been installed. Called from the
/// SQL-side `CREATE EXTENSION hdb_code` handler. Safe to call
/// repeatedly.
pub fn mark_extension_installed() {
    EXTENSION_INSTALLED.store(true, Ordering::Relaxed);
}

/// True when the extension has been installed (via DDL or, implicitly,
/// via a prior `code_index` call that ran the bootstrap).
pub fn is_extension_installed() -> bool {
    EXTENSION_INSTALLED.load(Ordering::Relaxed)
}

/// Metadata for a declared AST index. Held in process-local state;
/// `auto_reparse` is checked whenever user code inserts/updates the
/// source table.  `paused` temporarily suppresses auto-reparse (see
/// `hdb_code.pause` / `hdb_code.resume`).
#[derive(Debug, Clone)]
pub struct AstIndexMeta {
    pub index_name: String,
    pub table: String,
    pub content_col: String,
    pub lang_col: Option<String>,
    pub embed_endpoint: Option<String>,
    pub embed_bearer: Option<String>,
    pub embed_bodies: bool,
    pub auto_reparse: bool,
    pub resolve_cross_file: bool,
    pub paused: bool,
}

static AST_INDEXES: std::sync::OnceLock<
    std::sync::RwLock<HashMap<String, AstIndexMeta>>,
> = std::sync::OnceLock::new();

fn ast_registry() -> &'static std::sync::RwLock<HashMap<String, AstIndexMeta>> {
    AST_INDEXES.get_or_init(|| std::sync::RwLock::new(HashMap::new()))
}

/// Register (or replace) an AST index declaration.  Called by the
/// CREATE AST INDEX dispatcher.
pub fn register_ast_index(meta: AstIndexMeta) {
    let mut reg = ast_registry().write().unwrap_or_else(|p| p.into_inner());
    reg.insert(meta.index_name.clone(), meta);
}

/// Look up an AST index by name.
pub fn get_ast_index(name: &str) -> Option<AstIndexMeta> {
    ast_registry()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(name)
        .cloned()
}

/// Return all indexes whose `table == table_name`.  Used by the
/// auto_reparse hook to know which indexes to refresh when a
/// source table is mutated.
pub fn ast_indexes_for_table(table_name: &str) -> Vec<AstIndexMeta> {
    ast_registry()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .filter(|m| m.table == table_name && !m.paused)
        .cloned()
        .collect()
}

/// Flip the `paused` flag on the named index.  Returns `false` if
/// there is no index by that name.
pub fn set_ast_index_paused(name: &str, paused: bool) -> bool {
    let mut reg = ast_registry().write().unwrap_or_else(|p| p.into_inner());
    match reg.get_mut(name) {
        Some(m) => {
            m.paused = paused;
            true
        }
        None => false,
    }
}

/// Run the indexer over `opts.source_table`. Creates the `_hdb_code_*`
/// tables on first call. Returns statistics; never mutates the source
/// table.
///
/// Inserts are batched via multi-row `VALUES` (up to
/// engine-bypass bulk-insert primitive) and unchanged files are skipped by
/// comparing the row's SHA-256 against the stored hash. `force_reparse`
/// bypasses the sha gate.
pub fn code_index(db: &EmbeddedDatabase, opts: CodeIndexOptions) -> Result<CodeIndexStats> {
    let embedder: Box<dyn Embedder> = match opts.embed_endpoint.as_deref() {
        Some(url) if opts.embed_bodies => {
            let mut h = super::embed::HttpEmbedder::new(url);
            if let Some(tok) = &opts.embed_bearer {
                h = h.with_bearer(tok.clone());
            }
            Box::new(h)
        }
        _ => Box::new(NoopEmbedder),
    };
    code_index_with_embedder(db, opts, embedder)
}

/// Indexer entry-point that takes a pre-constructed embedder.
/// Used by tests + by `code-embed`-feature callers that want to
/// supply their own (e.g. fastembed) implementation without going
/// through the URL-based `HttpEmbedder` path.
pub fn code_index_with_embedder(
    db: &EmbeddedDatabase,
    opts: CodeIndexOptions,
    embedder: Box<dyn Embedder>,
) -> Result<CodeIndexStats> {
    bootstrap_tables(db)?;
    mark_extension_installed();

    let files = fetch_source_files(db, &opts.source_table)?;
    let mut existing_sha = fetch_file_sha_map(db)?;
    let mut stats = CodeIndexStats::default();
    let mut lang_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // -------- Tier 1.3: TRUNCATE fast path for force_reparse --------
    //
    // `force_reparse` against a populated KB takes the per-file
    // delete-then-insert path: DELETE refs, DELETE symbols, INSERT
    // fresh — for *every* row. On large corpora that's tens of
    // thousands of small DELETEs followed by tens of thousands of
    // INSERTs, which triggers RocksDB compaction storm and balloons
    // wall-clock by an order of magnitude (the pilot reported a
    // 1 h 55 m kill on this path).
    //
    // When the caller opts into `force_reparse`, the *intent* is
    // "rebuild everything from scratch." A single TRUNCATE per
    // table is cheap (one delete-range per CF, no per-row work) and
    // gets the in-flight write loop into the same shape it'd be on
    // a cold KB — every INSERT is fresh, no DELETE-then-INSERT
    // churn. Skipped when the tables are already empty (cold KB)
    // and when force_reparse is false (incremental).
    let truncated = opts.force_reparse && !existing_sha.is_empty();
    if truncated {
        // Order matters for FK semantics in case anyone added FKs
        // later: refs → symbols → ast_nodes → files.
        for tbl in [
            "_hdb_code_symbol_refs",
            "_hdb_code_symbols",
            "_hdb_code_ast_nodes",
            "_hdb_code_files",
        ] {
            // TRUNCATE returns zero rows-affected; we don't care
            // about the count, only that it succeeded. If a table
            // doesn't exist (legacy KB without ast_nodes), swallow
            // the error and keep going.
            let _ = db.execute(&format!("TRUNCATE {tbl}"));
        }
        existing_sha.clear();
        tracing::debug!("force_reparse + populated KB: truncated _hdb_code_* tables");
    }

    // -------- Read / triage phase (serial) --------
    //
    // Walk the corpus once: classify every row into "skip" (no
    // extractor), "unchanged" (sha matches, no force_reparse), or
    // "to_parse" (needs work). Classification has no DB writes and
    // no parser cost; doing it serially keeps the parallel phase's
    // input set tight and lets us count files_seen / files_skipped /
    // files_unchanged exactly the same way the serial implementation
    // did. Languages_seen is also accumulated here so the stats
    // remain identical even when no files reach the parse phase.
    let mut to_parse: Vec<(SourceFile, String, ParseExtractPair)> = Vec::new();
    for file in files.into_iter() {
        stats.files_seen += 1;
        let resolution = match resolve_parser_and_extractor(&file.lang) {
            Some(r) => r,
            None => {
                stats.files_skipped += 1;
                continue;
            }
        };
        lang_set.insert(file.lang.to_ascii_lowercase());
        let sha = sha256_hex(&file.content);
        let unchanged = existing_sha
            .get(&file.path)
            .map(|s| s == &sha)
            .unwrap_or(false);
        if unchanged && !opts.force_reparse {
            stats.files_unchanged += 1;
            continue;
        }
        to_parse.push((file, sha, resolution));
    }

    let touched = !to_parse.is_empty();

    // -------- Pipeline construction --------
    //
    // Parse + extract runs on a *dedicated* rayon ThreadPool sized
    // by the caller's `parallelism` setting (default
    // `min(num_cpus, 8)`). Pool is built fresh per call and dropped
    // at the end so it never steals threads from the global pool
    // that handles live OLTP traffic on daemon-mode servers.
    //
    // Optional chunking (`opts.chunk_size = Some(n)`) bounds peak
    // memory by interleaving parse + drain — parse a chunk, write
    // it, then move to the next chunk. Default `None` keeps the
    // single-chunk all-in-one path that's fastest for small/medium
    // corpora where memory isn't a concern.
    let workers = opts.resolved_parallelism();
    stats.parse_workers = workers as u32;

    let chunks: Vec<Vec<(SourceFile, String, ParseExtractPair)>> = match opts.chunk_size {
        None => {
            if to_parse.is_empty() {
                Vec::new()
            } else {
                vec![to_parse]
            }
        }
        Some(n) => {
            let n = n.max(1);
            let mut out = Vec::with_capacity(to_parse.len().div_ceil(n));
            let mut iter = to_parse.into_iter();
            loop {
                let chunk: Vec<_> = (&mut iter).take(n).collect();
                if chunk.is_empty() {
                    break;
                }
                out.push(chunk);
            }
            out
        }
    };

    let pool = if chunks.is_empty() {
        None
    } else {
        Some(
            rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .thread_name(|i| format!("hdb-code-index-{i}"))
                .build()
                .map_err(|e| Error::query_execution(format!(
                    "failed to build code-index thread pool ({e})"
                )))?,
        )
    };

    // -------- Tier 1.1 prelude: detect outer-txn vs self-managed --------
    //
    // Per-chunk writes are wrapped in an explicit transaction so the
    // engine pays one WAL fsync per chunk instead of per-statement —
    // tens-of-thousands → tens. ACID-positive (each chunk lands
    // atomically; with `chunk_size = None` the whole ingest is one
    // atomic commit). If the caller already has an outer transaction
    // open (rare — `code_index` is normally called outside one), we
    // honour theirs and skip our own begin/commit so we don't
    // accidentally commit the caller's pending work. The MCP plugin
    // commits its file-upsert txn before calling code_index, so this
    // path takes the self-managed branch.
    let manage_txn = !db.in_transaction();

    // Tier 1.3 correctness guard: `skip_delete_stale` (set when we
    // truncated up front) tells `write_one_parsed` to skip the
    // SELECT-existing-symbols / DELETE-prior-refs / DELETE-prior-symbols
    // preamble — safe ONLY for the FIRST occurrence of a given path
    // in this call. If `source_table` happens to hold duplicate
    // `path` rows (e.g. a buggy upsert client), subsequent occurrences
    // need the normal delete-stale loop or symbols/refs accumulate.
    // We track which paths we've already drained per call.
    let mut processed_paths: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    // -------- Per-chunk: parallel parse → transactional drain --------
    for chunk in chunks {
        let parse_started = std::time::Instant::now();
        let parsed: Vec<Result<ParsedFile>> = if let Some(pool) = &pool {
            use rayon::prelude::*;
            pool.install(|| {
                chunk
                    .into_par_iter()
                    .map(parse_extract_one)
                    .collect::<Vec<_>>()
            })
        } else {
            chunk.into_iter().map(parse_extract_one).collect::<Vec<_>>()
        };
        stats.parse_elapsed_ms += parse_started.elapsed().as_millis() as u64;
        stats.chunks_processed += 1;

        // Serial write phase for this chunk, wrapped in a single
        // transaction. The engine's catalog/ART/transaction state
        // isn't Sync for multi-writer access today, and the FR
        // explicitly keeps writes single-threaded. Walk results in
        // input order so output is deterministic regardless of worker
        // scheduling.
        let write_started = std::time::Instant::now();
        if manage_txn {
            db.begin()?;
        }
        let chunk_result = drain_chunk(
            db,
            &opts,
            embedder.as_ref(),
            &mut stats,
            parsed,
            truncated,
            &mut processed_paths,
        );
        if manage_txn {
            match chunk_result {
                Ok(()) => db.commit()?,
                Err(e) => {
                    let _ = db.rollback();
                    return Err(e);
                }
            }
        } else {
            chunk_result?;
        }
        stats.write_elapsed_ms += write_started.elapsed().as_millis() as u64;
    }

    stats.languages_seen = lang_set.into_iter().collect();

    // Cross-file resolution only pays off if the corpus actually
    // changed this run. Wrapped in its own transaction so its
    // batched UPDATE+SELECT chatter pays one fsync, not many.
    // Counted into the write timer because it's a serial DB
    // operation, not a parse one.
    if touched {
        let cross_started = std::time::Instant::now();
        if manage_txn {
            db.begin()?;
        }
        let cross_result = cross_file_resolve(db, &mut stats);
        if manage_txn {
            match cross_result {
                Ok(()) => db.commit()?,
                Err(e) => {
                    let _ = db.rollback();
                    return Err(e);
                }
            }
        } else {
            cross_result?;
        }
        stats.write_elapsed_ms += cross_started.elapsed().as_millis() as u64;
    }

    Ok(stats)
}

/// Drain one chunk's worth of parsed files into `_hdb_code_*`. Caller
/// wraps this in a transaction so the chunk is atomic and pays a
/// single fsync. Errors propagate so the caller can roll back.
///
/// `truncated_this_call` is the Tier 1.3 fast-path flag: when set, the
/// first occurrence of each path in this call skips the delete-stale
/// preamble (the tables were truncated upfront so there's nothing to
/// delete). `processed_paths` carries state across chunks so the same
/// path appearing twice in `source_table` (e.g. duplicate upserts)
/// still gets the second occurrence's preamble run, preventing
/// symbol/ref accumulation.
///
/// **Phase-2 batching (FR `parallel_writes`).** The previous
/// per-file path called `bulk_insert_tuples` once per file × two
/// (symbols + refs) — for the pilot's 666-file corpus that was
/// 1 332 batched calls per chunk. This implementation collapses to
/// **two `bulk_insert_tuples` calls per chunk** (one for all symbols
/// across all files, one for all refs across all files) by tracking
/// per-file slice boundaries through the batch. Per-file
/// `upsert_file` and the SELECT/DELETE preamble remain serial — they
/// have UPSERT semantics that don't bulk-trivially.
fn drain_chunk(
    db: &EmbeddedDatabase,
    opts: &CodeIndexOptions,
    embedder: &dyn Embedder,
    stats: &mut CodeIndexStats,
    parsed: Vec<Result<ParsedFile>>,
    truncated_this_call: bool,
    processed_paths: &mut std::collections::HashSet<String>,
) -> Result<()> {
    // Step 1 — short-circuit any worker-side parse errors. (Same
    // behaviour as the prior implementation; '?' propagation halts
    // the whole call.)
    let parsed: Vec<ParsedFile> = parsed.into_iter().collect::<Result<Vec<_>>>()?;
    if parsed.is_empty() {
        return Ok(());
    }

    // Step 2 — serial per-file preamble: upsert the file row, run
    // the SELECT/DELETE clean-up of stale symbols + refs (skipped on
    // the Tier 1.3 fast path for the FIRST occurrence of each path).
    // Output is a vector of (file_id, parsed_file) preserving input
    // order so cross-file batching below can map back unambiguously.
    let mut prepared: Vec<(i64, ParsedFile)> = Vec::with_capacity(parsed.len());
    for p in parsed {
        let first_time = processed_paths.insert(p.file.path.clone());
        let skip_delete_stale = truncated_this_call && first_time;
        let file_id = upsert_file(db, &opts.source_table, &p.file, &p.sha)?;
        if !skip_delete_stale {
            delete_stale_symbols_and_refs(db, file_id)?;
        }
        prepared.push((file_id, p));
    }

    // Step 3 — flatten ALL symbols across files. Track per-file
    // (start, count) ranges into the flat array so refs can map
    // back. Run the embedder once per symbol; the dimension check
    // and `ensure_body_vec_column` lift to the batch level.
    let total_syms: usize = prepared.iter().map(|(_, p)| p.symbols.len()).sum();
    let mut all_symbols: Vec<&Symbol> = Vec::with_capacity(total_syms);
    let mut sym_ranges: Vec<(usize, usize)> = Vec::with_capacity(prepared.len());
    let mut sym_owner_file_ids: Vec<i64> = Vec::with_capacity(total_syms);
    for (file_id, p) in &prepared {
        let start = all_symbols.len();
        all_symbols.extend(p.symbols.iter());
        sym_ranges.push((start, p.symbols.len()));
        sym_owner_file_ids.extend(std::iter::repeat(*file_id).take(p.symbols.len()));
    }

    let symbol_ids: Vec<i64> = if all_symbols.is_empty() {
        Vec::new()
    } else {
        bulk_insert_symbols_batched(db, embedder, stats, &sym_owner_file_ids, &all_symbols)?
    };

    // Step 4 — flatten ALL refs across files. For each file slice
    // its symbol_ids out of the flat result; convert per-file
    // ResolvedRef.from_idx / to_idx into absolute symbol_ids;
    // collect into one Tuple vec; one bulk_insert_tuples call.
    let total_refs: usize = prepared.iter().map(|(_, p)| p.resolved.len()).sum();
    if total_refs > 0 {
        let written = bulk_insert_refs_batched(db, &prepared, &symbol_ids, &sym_ranges)?;
        stats.refs_written += written;
    }

    // Per-file stat updates (matches the per-file write_one_parsed
    // accounting that callers / tests rely on).
    for (i, (_, p)) in prepared.iter().enumerate() {
        let _ = i; // index reserved for future per-file telemetry
        stats.files_parsed += 1;
        stats.symbols_written += p.symbols.len() as u64;
    }

    Ok(())
}

/// Step 2 helper: run the SELECT/UPDATE/DELETE preamble for a single
/// file (clean up stale symbols+refs left over from the previous
/// ingest of this file). Skipped by the caller when truncate_this_call
/// + first_time gives a clean-table guarantee.
fn delete_stale_symbols_and_refs(db: &EmbeddedDatabase, file_id: i64) -> Result<()> {
    let sym_rows = db.query(
        &format!("SELECT node_id FROM _hdb_code_symbols WHERE file_id = {file_id}"),
        &[],
    )?;
    let stale_ids: Vec<i64> = sym_rows
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::Int4(n)) => Some(*n as i64),
            Some(Value::Int8(n)) => Some(*n),
            _ => None,
        })
        .collect();
    if !stale_ids.is_empty() {
        let csv = stale_ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        db.execute(&format!(
            "UPDATE _hdb_code_symbol_refs \
                SET to_symbol = NULL, resolution = 'unresolved' \
              WHERE to_symbol IN ({csv})"
        ))?;
    }
    db.execute(&format!(
        "DELETE FROM _hdb_code_symbol_refs WHERE file_id = {file_id}"
    ))?;
    db.execute(&format!(
        "DELETE FROM _hdb_code_symbols WHERE file_id = {file_id}"
    ))?;
    Ok(())
}

/// Step 3 helper: bulk-insert all symbols across all files in a
/// chunk in a single `bulk_insert_tuples` call. Mirror of the
/// per-file `insert_symbols`; the only structural difference is
/// the `file_id_per_symbol` parallel vector that maps each symbol
/// row back to its owning file (the per-file `insert_symbols`
/// got `file_id` once and applied to every row).
fn bulk_insert_symbols_batched(
    db: &EmbeddedDatabase,
    embedder: &dyn Embedder,
    stats: &mut CodeIndexStats,
    file_id_per_symbol: &[i64],
    symbols: &[&Symbol],
) -> Result<Vec<i64>> {
    debug_assert_eq!(file_id_per_symbol.len(), symbols.len());

    let mut vectors: Vec<Option<Vec<f32>>> = Vec::with_capacity(symbols.len());
    for sym in symbols {
        let v = if !sym.signature.is_empty() {
            embedder.embed(&sym.signature)?
        } else {
            None
        };
        if v.is_some() {
            stats.embed_calls += 1;
        }
        vectors.push(v);
    }

    let any_vec = vectors.iter().any(Option::is_some);
    if any_vec {
        let dim = vectors
            .iter()
            .find_map(|v| v.as_ref().map(|v| v.len()))
            .unwrap_or(0);
        if dim == 0 {
            return Err(Error::query_execution(
                "embedder returned a zero-length vector",
            ));
        }
        ensure_body_vec_column(db, dim)?;
        for v in &vectors {
            if let Some(vec) = v {
                if vec.len() != dim {
                    return Err(Error::query_execution(format!(
                        "embedder dimension mismatch: expected {dim}, got {}",
                        vec.len()
                    )));
                }
            }
        }
    }

    let schema = db.storage.catalog().get_table_schema("_hdb_code_symbols")?;
    let n_cols = schema.columns.len();
    let expected_min_cols = if any_vec { 13 } else { 12 };
    if n_cols < expected_min_cols {
        return Err(Error::query_execution(format!(
            "_hdb_code_symbols schema has {} cols, fast path expects ≥ {}",
            n_cols, expected_min_cols
        )));
    }

    let mut tuples: Vec<Tuple> = Vec::with_capacity(symbols.len());
    for (idx, sym) in symbols.iter().enumerate() {
        let file_id = file_id_per_symbol[idx];
        let mut values: Vec<Value> = Vec::with_capacity(n_cols);
        values.push(Value::Null); // node_id — auto-fill
        values.push(Value::Int8(file_id));
        values.push(Value::String(sym.name.clone()));
        values.push(Value::String(sym.qualified.clone()));
        values.push(Value::String(sym.kind.as_str().to_string()));
        values.push(Value::String(sym.signature.clone()));
        values.push(Value::String(sym.visibility.as_str().to_string()));
        values.push(Value::Int4(sym.line_start as i32));
        values.push(Value::Int4(sym.line_end as i32));
        values.push(Value::Int4(sym.byte_start as i32));
        values.push(Value::Int4(sym.byte_end as i32));
        values.push(Value::Null); // parent_id
        if any_vec && n_cols >= 13 {
            let v = vectors
                .get(idx)
                .and_then(|v| v.clone())
                .map(Value::Vector)
                .unwrap_or(Value::Null);
            values.push(v);
        }
        while values.len() < n_cols {
            values.push(Value::Null);
        }
        tuples.push(Tuple::new(values));
    }
    let row_ids = db.bulk_insert_tuples("_hdb_code_symbols", tuples)?;
    Ok(row_ids.into_iter().map(|id| id as i64).collect())
}

/// Step 4 helper: bulk-insert all refs across all files in a chunk
/// in a single `bulk_insert_tuples` call. For each file's resolved
/// refs, slice the per-file symbol_ids out of the flat batch result
/// and translate ResolvedRef.from_idx / to_idx into absolute ids.
fn bulk_insert_refs_batched(
    db: &EmbeddedDatabase,
    prepared: &[(i64, ParsedFile)],
    symbol_ids: &[i64],
    sym_ranges: &[(usize, usize)],
) -> Result<u64> {
    let schema = db.storage.catalog().get_table_schema("_hdb_code_symbol_refs")?;
    let n_cols = schema.columns.len();
    if n_cols < 8 {
        return Err(Error::query_execution(format!(
            "_hdb_code_symbol_refs schema has {} cols, fast path expects ≥ 8",
            n_cols
        )));
    }

    let total: usize = prepared.iter().map(|(_, p)| p.resolved.len()).sum();
    let mut tuples: Vec<Tuple> = Vec::with_capacity(total);
    for (i, (file_id, p)) in prepared.iter().enumerate() {
        let (start, count) = sym_ranges[i];
        let file_symbol_ids = &symbol_ids[start..start + count];
        for r in &p.resolved {
            let from_id = file_symbol_ids.get(r.from_idx).copied().ok_or_else(|| {
                Error::query_execution(format!(
                    "resolver produced invalid from_idx {} for file_id {}",
                    r.from_idx, file_id
                ))
            })?;
            let to_val = match r.to_idx {
                Some(idx) => file_symbol_ids
                    .get(idx)
                    .map(|id| Value::Int8(*id))
                    .unwrap_or(Value::Null),
                None => Value::Null,
            };
            let res = match r.resolution {
                Resolution::Exact => "exact",
                Resolution::Heuristic => "heuristic",
                Resolution::Unresolved => "unresolved",
            };
            let mut values: Vec<Value> = Vec::with_capacity(n_cols);
            values.push(Value::Null); // edge_id — auto-fill
            values.push(Value::Int8(*file_id));
            values.push(Value::Int8(from_id));
            values.push(to_val);
            values.push(Value::String(r.to_name.clone()));
            values.push(Value::String(r.kind_str.to_string()));
            values.push(Value::Int4(r.line as i32));
            values.push(Value::String(res.to_string()));
            while values.len() < n_cols {
                values.push(Value::Null);
            }
            tuples.push(Tuple::new(values));
        }
    }
    let written = tuples.len() as u64;
    if !tuples.is_empty() {
        db.bulk_insert_tuples("_hdb_code_symbol_refs", tuples)?;
    }
    Ok(written)
}

/// Drain one parsed file into the `_hdb_code_*` tables. Caller wraps
/// in a transaction (Tier 1.1).
///
/// `skip_delete_stale` short-circuits the
/// SELECT-existing-symbols / UPDATE-inbound-refs / DELETE-symbols /
/// DELETE-refs preamble. Set when the indexer just truncated the
/// `_hdb_code_*` tables (force_reparse fast path) — the SELECT would
/// always return empty and the DELETEs would be no-ops, so skipping
/// them saves O(files) wasted SQL parses + RocksDB round-trips on
/// large corpora.
fn write_one_parsed(
    db: &EmbeddedDatabase,
    opts: &CodeIndexOptions,
    embedder: &dyn Embedder,
    stats: &mut CodeIndexStats,
    parsed: ParsedFile,
    skip_delete_stale: bool,
) -> Result<()> {
    let ParsedFile { file, sha, symbols, resolved } = parsed;

    // Upsert the file row, get back the file_id. This also writes
    // the new sha256 so the next run can short-circuit.
    let file_id = upsert_file(db, &opts.source_table, &file, &sha)?;

    if !skip_delete_stale {
        // Null inbound cross-file refs pointing at this file's old
        // symbols — the cross-file resolver at the end of the run
        // rebinds them. Uses a literal IN list (SELECT-then-UPDATE)
        // since `UPDATE … IN (SELECT …)` isn't wired in the DML path.
        let sym_rows = db.query(
            &format!("SELECT node_id FROM _hdb_code_symbols WHERE file_id = {file_id}"),
            &[],
        )?;
        let stale_ids: Vec<i64> = sym_rows
            .iter()
            .filter_map(|r| match r.values.first() {
                Some(Value::Int4(n)) => Some(*n as i64),
                Some(Value::Int8(n)) => Some(*n),
                _ => None,
            })
            .collect();
        if !stale_ids.is_empty() {
            let csv = stale_ids
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");
            db.execute(&format!(
                "UPDATE _hdb_code_symbol_refs \
                    SET to_symbol = NULL, resolution = 'unresolved' \
                  WHERE to_symbol IN ({csv})"
            ))?;
        }
        db.execute(&format!(
            "DELETE FROM _hdb_code_symbol_refs WHERE file_id = {file_id}"
        ))?;
        db.execute(&format!(
            "DELETE FROM _hdb_code_symbols WHERE file_id = {file_id}"
        ))?;
    }

    let symbol_ids = insert_symbols(db, file_id, &symbols, embedder, stats)?;
    let refs_written = insert_refs(db, file_id, &symbol_ids, &resolved)?;

    stats.files_parsed += 1;
    stats.symbols_written += symbols.len() as u64;
    stats.refs_written += refs_written;
    Ok(())
}

/// Per-file output of the parallel parse + extract phase. Holds only
/// owned data — no references back into the source corpus, so workers
/// can hand it back to the main thread cheaply.
struct ParsedFile {
    file: SourceFile,
    sha: String,
    symbols: Vec<Symbol>,
    resolved: Vec<super::resolver::ResolvedRef>,
}

/// Pure parse + extract + in-file resolve. Runs inside a rayon worker;
/// touches no DB state. Errors propagate back to the main thread, which
/// short-circuits the whole `code_index` call (matching the serial
/// implementation's `?`-propagation behaviour).
fn parse_extract_one(
    item: (SourceFile, String, ParseExtractPair),
) -> Result<ParsedFile> {
    let (file, sha, resolution) = item;
    let tree = match &resolution {
        ParseExtractPair::Static(lang) => parse::parse(*lang, &file.content)?,
        ParseExtractPair::Dynamic { .. } => {
            parse::parse_by_name(&file.lang, &file.content)?
        }
    };
    let (symbols, refs) = match &resolution {
        ParseExtractPair::Static(lang) => extract(*lang, &file.content, &tree),
        ParseExtractPair::Dynamic { extractor } => extractor.extract(&file.content, &tree),
    };
    let mut resolved = resolve_in_file(&symbols, &refs);
    let bodies: Vec<super::resolver::FunctionBody<'_>> = symbols
        .iter()
        .filter_map(|s| {
            let lo = (s.byte_start as usize).min(file.content.len());
            let hi = (s.byte_end as usize).min(file.content.len());
            if hi <= lo {
                return None;
            }
            Some(super::resolver::FunctionBody {
                line_start: s.line_start,
                line_end: s.line_end,
                body_text: &file.content[lo..hi],
            })
        })
        .collect();
    super::resolver::rebind_via_local_types(&mut resolved, &bodies);
    super::resolver::rebind_via_imports(&mut resolved);
    Ok(ParsedFile { file, sha, symbols, resolved })
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

/// Parser + extractor pair used by the indexer to handle both
/// static and dynamically-registered languages uniformly.
enum ParseExtractPair {
    Static(Language),
    Dynamic {
        extractor: std::sync::Arc<dyn super::symbols::SymbolExtractor>,
    },
}

fn resolve_parser_and_extractor(lang: &str) -> Option<ParseExtractPair> {
    if let Some(builtin) = Language::from_lang_str(lang) {
        return Some(ParseExtractPair::Static(builtin));
    }
    // Dynamic path: require BOTH a registered grammar AND a
    // registered extractor.  A grammar without an extractor would
    // parse to an empty symbol set silently — that's worse than
    // skipping the file outright.
    let canonical = lang.trim().to_ascii_lowercase();
    if super::parse::registered_grammars()
        .iter()
        .any(|g| g == &canonical)
    {
        if let Some(extractor) = super::symbols::registered_extractor(&canonical) {
            return Some(ParseExtractPair::Dynamic { extractor });
        }
    }
    None
}

/// Lazy install of `_hdb_code_symbols.body_vec VECTOR(<dim>)`.
///
/// Uses Nano's `ADD COLUMN IF NOT EXISTS` support — a no-op when
/// the column is already present, and the planner accepts it even
/// when LIMIT 0 SELECT probing would short-circuit before column
/// resolution and falsely report "exists".
///
/// When the column already exists at a different dimension, the
/// engine surfaces a column-conflict error from the underlying
/// schema check.
fn ensure_body_vec_column(db: &EmbeddedDatabase, dim: usize) -> Result<()> {
    db.execute(&format!(
        "ALTER TABLE _hdb_code_symbols ADD COLUMN IF NOT EXISTS body_vec VECTOR({dim})"
    ))?;
    Ok(())
}

fn fetch_file_sha_map(db: &EmbeddedDatabase) -> Result<HashMap<String, String>> {
    // Probe first — _hdb_code_files may not exist on the very first call.
    let probe = db.query("SELECT 1 FROM _hdb_code_files LIMIT 1", &[]);
    if probe.is_err() {
        return Ok(HashMap::new());
    }
    let rows = db.query("SELECT path, sha256 FROM _hdb_code_files", &[])?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        let path = match row.values.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let sha = match row.values.get(1) {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        out.insert(path, sha);
    }
    Ok(out)
}

fn cross_file_resolve(db: &EmbeddedDatabase, stats: &mut CodeIndexStats) -> Result<()> {
    // Build a corpus map name→(node_id, count) in one scan.
    let rows = db.query(
        "SELECT name, node_id FROM _hdb_code_symbols ORDER BY name, node_id",
        &[],
    )?;
    let mut first: std::collections::HashMap<String, (i64, u32)> = std::collections::HashMap::new();
    for row in rows {
        let name = match row.values.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let id = match row.values.get(1) {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        let entry = first.entry(name).or_insert((id, 0));
        entry.1 += 1;
    }

    let unresolved = db.query(
        "SELECT edge_id, to_name FROM _hdb_code_symbol_refs WHERE resolution = 'unresolved'",
        &[],
    )?;
    let mut rebound = 0u64;
    for row in unresolved {
        let edge_id = match row.values.first() {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        let to_name = match row.values.get(1) {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let bare = last_segment(&to_name);
        if let Some((id, count)) = first.get(bare) {
            let res = if *count == 1 { "exact" } else { "heuristic" };
            db.execute(&format!(
                "UPDATE _hdb_code_symbol_refs \
                   SET to_symbol = {id}, resolution = '{res}' \
                 WHERE edge_id = {edge_id}"
            ))?;
            rebound += 1;
        }
    }
    let _ = rebound;
    let _ = stats;
    Ok(())
}

fn last_segment(name: &str) -> &str {
    let bare = name.trim_end_matches(')');
    let bare = bare.split('(').next().unwrap_or(bare);
    if let Some(idx) = bare.rfind("::") {
        return &bare[idx + 2..];
    }
    if let Some(idx) = bare.rfind('.') {
        return &bare[idx + 1..];
    }
    bare
}

// ---------------------------------------------------------------------------
// DDL bootstrap
// ---------------------------------------------------------------------------

fn bootstrap_tables(db: &EmbeddedDatabase) -> Result<()> {
    // Idempotent — use CREATE TABLE IF NOT EXISTS. Planner + executor
    // already support the syntax (see drizzle_compat tests).
    db.execute(
        r#"CREATE TABLE IF NOT EXISTS _hdb_code_files (
             node_id    BIGSERIAL PRIMARY KEY,
             source_table TEXT NOT NULL,
             path       TEXT NOT NULL,
             lang       TEXT,
             sha256     TEXT,
             mtime      TIMESTAMP,
             summary    TEXT,
             UNIQUE(source_table, path)
           )"#,
    )?;
    // Phase 1 ships without a VECTOR column — the type requires an
    // explicit dimension and phase 1 has no embedder by default. Phase
    // 2 adds `body_vec VECTOR(n)` behind an option whose dimension is
    // negotiated with the embedding endpoint on first call.
    db.execute(
        r#"CREATE TABLE IF NOT EXISTS _hdb_code_symbols (
             node_id     BIGSERIAL PRIMARY KEY,
             file_id     BIGINT NOT NULL REFERENCES _hdb_code_files(node_id),
             name        TEXT NOT NULL,
             qualified   TEXT,
             kind        TEXT,
             signature   TEXT,
             visibility  TEXT,
             line_start  INTEGER,
             line_end    INTEGER,
             byte_start  INTEGER,
             byte_end    INTEGER,
             parent_id   BIGINT
           )"#,
    )?;
    db.execute(
        r#"CREATE TABLE IF NOT EXISTS _hdb_code_symbol_refs (
             edge_id     BIGSERIAL PRIMARY KEY,
             file_id     BIGINT NOT NULL REFERENCES _hdb_code_files(node_id),
             from_symbol BIGINT NOT NULL REFERENCES _hdb_code_symbols(node_id),
             to_symbol   BIGINT REFERENCES _hdb_code_symbols(node_id),
             to_name     TEXT,
             kind        TEXT,
             line        INTEGER,
             resolution  TEXT
           )"#,
    )?;

    // Tier 2 perf indexes (added v3.21.1+). Without these, the
    // per-file delete-stale path (`DELETE FROM … WHERE file_id = X`)
    // does a full table scan — a 35-second slow query for a 181-row
    // delete on a ~115 K-row refs table was observed in the field.
    // file_id-indexed deletes drop that to single-millisecond range
    // and are also the load-bearing index for the cross-file
    // resolver's UPDATE-by-file_id path. Idempotent.
    let _ = db.execute(
        "CREATE INDEX IF NOT EXISTS idx_hdb_code_symbols_file_id ON _hdb_code_symbols(file_id)",
    );
    let _ = db.execute(
        "CREATE INDEX IF NOT EXISTS idx_hdb_code_symbol_refs_file_id ON _hdb_code_symbol_refs(file_id)",
    );
    let _ = db.execute(
        "CREATE INDEX IF NOT EXISTS idx_hdb_code_symbol_refs_to_symbol ON _hdb_code_symbol_refs(to_symbol)",
    );
    let _ = db.execute(
        "CREATE INDEX IF NOT EXISTS idx_hdb_code_symbol_refs_from_symbol ON _hdb_code_symbol_refs(from_symbol)",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Source-table access
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SourceFile {
    path: String,
    lang: String,
    content: String,
    sha256: Option<String>,
}

fn fetch_source_files(db: &EmbeddedDatabase, source_table: &str) -> Result<Vec<SourceFile>> {
    // NB: we intentionally take the whole corpus into memory in phase
    // 1 — Nano's pilot is thousands of files, not millions. Phase 2's
    // trigger-based incremental reparse eliminates this batch.
    let rows = db.query(
        &format!(r#"SELECT "path", "lang", "content" FROM "{source_table}""#),
        &[],
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let path = match row.values.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let lang = match row.values.get(1) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let content = match row.values.get(2) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        out.push(SourceFile { path, lang, content, sha256: None });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Write helpers
// ---------------------------------------------------------------------------

fn upsert_file(
    db: &EmbeddedDatabase,
    source_table: &str,
    file: &SourceFile,
    sha: &str,
) -> Result<i64> {
    // Parameterised path — source strings (paths, languages, sha256s)
    // may contain arbitrary characters we refuse to hand-escape.
    let path_val = Value::String(file.path.clone());
    let lang_val = Value::String(file.lang.clone());
    let sha_val = Value::String(sha.to_string());
    let st_val = Value::String(source_table.to_string());

    let existing = db.query_params(
        "SELECT node_id FROM _hdb_code_files \
         WHERE source_table = $1 AND path = $2",
        &[st_val.clone(), path_val.clone()],
    )?;
    if let Some(row) = existing.first() {
        if let Some(v) = row.values.first() {
            let id = match v {
                Value::Int4(n) => *n as i64,
                Value::Int8(n) => *n,
                other => {
                    return Err(Error::query_execution(format!(
                        "unexpected file_id type: {other:?}"
                    )))
                }
            };
            db.execute_params_returning(
                "UPDATE _hdb_code_files SET lang = $1, sha256 = $2 WHERE node_id = $3",
                &[lang_val, sha_val, Value::Int8(id)],
            )?;
            return Ok(id);
        }
    }

    let (_, rows) = db.execute_params_returning(
        "INSERT INTO _hdb_code_files (source_table, path, lang, sha256) \
         VALUES ($1, $2, $3, $4) RETURNING node_id",
        &[st_val, path_val, lang_val, sha_val],
    )?;
    if let Some(row) = rows.first() {
        if let Some(v) = row.values.first() {
            return match v {
                Value::Int4(n) => Ok(*n as i64),
                Value::Int8(n) => Ok(*n),
                other => Err(Error::query_execution(format!(
                    "unexpected file_id type: {other:?}"
                ))),
            };
        }
    }
    Err(Error::query_execution("RETURNING file_id yielded no rows"))
}

fn insert_symbols(
    db: &EmbeddedDatabase,
    file_id: i64,
    symbols: &[Symbol],
    embedder: &dyn Embedder,
    stats: &mut CodeIndexStats,
) -> Result<Vec<i64>> {
    if symbols.is_empty() {
        return Ok(Vec::new());
    }

    // Compute embeddings up-front. NoopEmbedder returns None for
    // everything → vectors stays empty → no schema change.
    let mut vectors: Vec<Option<Vec<f32>>> = Vec::with_capacity(symbols.len());
    for sym in symbols {
        let v = if !sym.signature.is_empty() {
            embedder.embed(&sym.signature)?
        } else {
            None
        };
        if v.is_some() {
            stats.embed_calls += 1;
        }
        vectors.push(v);
    }

    let any_vec = vectors.iter().any(Option::is_some);
    if any_vec {
        let dim = vectors
            .iter()
            .find_map(|v| v.as_ref().map(|v| v.len()))
            .unwrap_or(0);
        if dim == 0 {
            return Err(Error::query_execution(
                "embedder returned a zero-length vector",
            ));
        }
        ensure_body_vec_column(db, dim)?;
        for v in &vectors {
            if let Some(vec) = v {
                if vec.len() != dim {
                    return Err(Error::query_execution(format!(
                        "embedder dimension mismatch: expected {dim}, got {}",
                        vec.len()
                    )));
                }
            }
        }
    }

    // Tier 2.4 fast path: build Tuple rows in column order and
    // bulk-insert via the engine's bypass-SQL primitive. The bulk
    // primitive writes directly to RocksDB (matching
    // `execute_plan_with_params`'s convention for `INSERT … RETURNING`)
    // so subsequent SQL DELETEs in the same outer txn don't pay the
    // O(N) `merge_with_write_set` cost that killed the v1 attempt.
    let schema = db.storage.catalog().get_table_schema("_hdb_code_symbols")?;
    let n_cols = schema.columns.len();
    let expected_min_cols = if any_vec { 13 } else { 12 };
    if n_cols < expected_min_cols {
        return Err(Error::query_execution(format!(
            "_hdb_code_symbols schema has {} cols, fast path expects ≥ {}",
            n_cols, expected_min_cols
        )));
    }

    let mut tuples: Vec<Tuple> = Vec::with_capacity(symbols.len());
    for (idx, sym) in symbols.iter().enumerate() {
        let mut values: Vec<Value> = Vec::with_capacity(n_cols);
        // Column order matches the CREATE TABLE in bootstrap_tables:
        //   node_id, file_id, name, qualified, kind, signature,
        //   visibility, line_start, line_end, byte_start, byte_end,
        //   parent_id, [body_vec]
        values.push(Value::Null); // node_id — auto-fill via bulk_insert_tuples
        values.push(Value::Int8(file_id));
        values.push(Value::String(sym.name.clone()));
        values.push(Value::String(sym.qualified.clone()));
        values.push(Value::String(sym.kind.as_str().to_string()));
        values.push(Value::String(sym.signature.clone()));
        values.push(Value::String(sym.visibility.as_str().to_string()));
        values.push(Value::Int4(sym.line_start as i32));
        values.push(Value::Int4(sym.line_end as i32));
        values.push(Value::Int4(sym.byte_start as i32));
        values.push(Value::Int4(sym.byte_end as i32));
        values.push(Value::Null); // parent_id — phase-1 leaves this null
        if any_vec && n_cols >= 13 {
            let v = vectors
                .get(idx)
                .and_then(|v| v.clone())
                .map(Value::Vector)
                .unwrap_or(Value::Null);
            values.push(v);
        }
        // Pad with Value::Null if the schema has more columns than
        // we know about (forward-compat).
        while values.len() < n_cols {
            values.push(Value::Null);
        }
        tuples.push(Tuple::new(values));
    }
    let row_ids = db.bulk_insert_tuples("_hdb_code_symbols", tuples)?;
    Ok(row_ids.into_iter().map(|id| id as i64).collect())
}

fn insert_refs(
    db: &EmbeddedDatabase,
    file_id: i64,
    symbol_ids: &[i64],
    resolved: &[super::resolver::ResolvedRef],
) -> Result<u64> {
    if resolved.is_empty() {
        return Ok(0);
    }

    // Tier 2.4 fast path — same as insert_symbols, see notes there.
    let schema = db.storage.catalog().get_table_schema("_hdb_code_symbol_refs")?;
    let n_cols = schema.columns.len();
    if n_cols < 8 {
        return Err(Error::query_execution(format!(
            "_hdb_code_symbol_refs schema has {} cols, fast path expects ≥ 8",
            n_cols
        )));
    }

    let mut tuples: Vec<Tuple> = Vec::with_capacity(resolved.len());
    for r in resolved {
        let from_id = symbol_ids.get(r.from_idx).copied().ok_or_else(|| {
            Error::query_execution(format!(
                "resolver produced invalid from_idx {}",
                r.from_idx
            ))
        })?;
        let to_val = match r.to_idx {
            Some(idx) => symbol_ids
                .get(idx)
                .map(|id| Value::Int8(*id))
                .unwrap_or(Value::Null),
            None => Value::Null,
        };
        let res = match r.resolution {
            Resolution::Exact => "exact",
            Resolution::Heuristic => "heuristic",
            Resolution::Unresolved => "unresolved",
        };
        // Column order from bootstrap_tables:
        //   edge_id, file_id, from_symbol, to_symbol, to_name, kind,
        //   line, resolution
        let mut values: Vec<Value> = Vec::with_capacity(n_cols);
        values.push(Value::Null); // edge_id — auto-fill
        values.push(Value::Int8(file_id));
        values.push(Value::Int8(from_id));
        values.push(to_val);
        values.push(Value::String(r.to_name.clone()));
        values.push(Value::String(r.kind_str.to_string()));
        values.push(Value::Int4(r.line as i32));
        values.push(Value::String(res.to_string()));
        while values.len() < n_cols {
            values.push(Value::Null);
        }
        tuples.push(Tuple::new(values));
    }
    let written = tuples.len() as u64;
    db.bulk_insert_tuples("_hdb_code_symbol_refs", tuples)?;
    Ok(written)
}

fn sql_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// Accessor used by `lsp::*` so callers don't need direct DB handles
// when walking file↔symbol pairs.
pub(super) fn file_path_by_id(db: &EmbeddedDatabase, file_id: i64) -> Result<Option<String>> {
    let rows = db.query(
        &format!("SELECT path FROM _hdb_code_files WHERE node_id = {file_id}"),
        &[],
    )?;
    Ok(rows.first().and_then(|r| match r.values.first() {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }))
}

pub(super) fn file_id_for_symbol(
    db: &EmbeddedDatabase,
    symbol_id: i64,
) -> Result<Option<i64>> {
    let rows = db.query(
        &format!("SELECT file_id FROM _hdb_code_symbols WHERE node_id = {symbol_id}"),
        &[],
    )?;
    Ok(rows.first().and_then(|r| r.values.first()).and_then(|v| match v {
        Value::Int4(n) => Some(*n as i64),
        Value::Int8(n) => Some(*n),
        _ => None,
    }))
}

// Utility kept for phase 2: turn a per-file rebinding map back into a
// stable closure used by cross-file resolution.
#[allow(dead_code)]
pub(super) fn qualified_index<'a>(symbols: &'a [Symbol]) -> HashMap<&'a str, usize> {
    let mut m = HashMap::new();
    for (i, s) in symbols.iter().enumerate() {
        m.insert(s.qualified.as_str(), i);
    }
    m
}
