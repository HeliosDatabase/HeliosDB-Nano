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
    ///
    /// With `None`, a run that has to parse more than
    /// [`DEFAULT_CHUNK_THRESHOLD`] files is split into chunks of
    /// [`DEFAULT_CHUNK_FILES`] anyway: one chunk is one transaction, so a
    /// single-chunk cold index of a very large corpus is one huge write group
    /// (bounded by the engine's single-transaction size limit) and a failure
    /// throws all of it away. A chunked run commits chunk by chunk; after a
    /// failure a rerun skips the committed files (their stored hash matches) and
    /// indexes the rest. Pass `Some(usize::MAX)` to force one chunk.
    pub chunk_size: Option<usize>,
    /// Skip the corpus-wide unresolved-reference rebinding pass.
    /// This leaves in-file references intact and keeps unresolved
    /// cross-file refs queryable by name, but avoids a large serial
    /// UPDATE workload on very large corpora.
    pub skip_cross_file_resolve: bool,
    /// Skip materialising `_hdb_code_symbol_refs`. Files and symbols
    /// are still indexed, but call/import/reference graph edges are
    /// omitted for fast first-pass KB builds.
    pub skip_symbol_refs: bool,
    /// Targeted run: read, hash and index only these source paths (a git hook
    /// or an agent edit that knows what it changed). A listed path with no
    /// source row but a stored file row is removed from the index; unlisted
    /// files are left alone, and the source table is not scanned (an empty
    /// list does nothing). `None` (default) = every row of the source table,
    /// and files that left the table are removed.
    ///
    /// Adding this field to a struct literal of `CodeIndexOptions` is a
    /// source break for code that does not use `..CodeIndexOptions::for_table(t)`.
    pub only_paths: Option<Vec<String>>,
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
            skip_cross_file_resolve: false,
            skip_symbol_refs: false,
            only_paths: None,
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
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
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
    /// Source files that were present in `_hdb_code_files` but no longer in
    /// the source table, and whose symbols, references and file row were
    /// removed by this run.
    pub files_removed: u64,
    /// Phase timers (milliseconds). `triage_ms` covers reading the source
    /// table and the stored hashes and classifying every file; the next five
    /// are the serial write phase split by statement family and add up to
    /// (almost all of) `write_elapsed_ms`; `projection_ms` is the graph-rag
    /// projection run by `EmbeddedDatabase::code_index` after the indexer.
    pub triage_ms: u64,
    pub cleanup_ms: u64,
    pub files_upsert_ms: u64,
    pub symbols_insert_ms: u64,
    pub refs_insert_ms: u64,
    pub cross_resolve_ms: u64,
    pub projection_ms: u64,
    /// Set when the graph-rag projection failed after the code tables were
    /// committed; the index itself succeeded.
    pub projection_error: Option<String>,
    /// UPDATE statements the tail resolution pass issued (refs whose target
    /// name was defined by a later chunk or an older run, and relabels of
    /// names that gained a second definition during the run).
    pub tail_statements: u64,
}

/// Sub-millisecond accumulators for the phase timers of one run; converted to
/// `CodeIndexStats` milliseconds once, so truncation does not eat the sum.
#[derive(Default)]
struct PhaseTimers {
    cleanup: std::time::Duration,
    files_upsert: std::time::Duration,
    symbols_insert: std::time::Duration,
    refs_insert: std::time::Duration,
    cross_resolve: std::time::Duration,
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

/// Declared AST indexes, keyed by `(database, index name)`.
///
/// The `database` component is `StorageEngine::instance_id` — the open
/// database that ran the `CREATE AST INDEX`. An AST index names a TABLE in
/// one database, so its declaration belongs to that database and not to the
/// process. Keyed by name alone (as it was) a process holding two
/// `EmbeddedDatabase`s got two wrong behaviours: a second database declaring
/// the same index name silently replaced the first one's settings, and the
/// auto-reparse hook fired for EVERY open database that wrote to a
/// same-named table — so a database that had declared no index at all would
/// start materialising `_hdb_code_*` tables of its own because an unrelated
/// database had declared one.
///
/// Still process-local rather than durable: a restart forgets every
/// declaration, which is a separate, pre-existing gap.
static AST_INDEXES: std::sync::OnceLock<std::sync::RwLock<HashMap<(u64, String), AstIndexMeta>>> =
    std::sync::OnceLock::new();

fn ast_registry() -> &'static std::sync::RwLock<HashMap<(u64, String), AstIndexMeta>> {
    AST_INDEXES.get_or_init(|| std::sync::RwLock::new(HashMap::new()))
}

/// Register (or replace) an AST index declaration for one database.  Called
/// by the CREATE AST INDEX dispatcher.
pub fn register_ast_index(database: u64, meta: AstIndexMeta) {
    let mut reg = ast_registry().write().unwrap_or_else(|p| p.into_inner());
    reg.insert((database, meta.index_name.clone()), meta);
}

/// Look up one database's AST index by name.
pub fn get_ast_index(database: u64, name: &str) -> Option<AstIndexMeta> {
    ast_registry()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(&(database, name.to_string()))
        .cloned()
}

/// Return this database's indexes whose `table == table_name`.  Used by the
/// auto_reparse hook to know which indexes to refresh when a
/// source table is mutated.
pub fn ast_indexes_for_table(database: u64, table_name: &str) -> Vec<AstIndexMeta> {
    ast_registry()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .filter(|((db, _), m)| *db == database && m.table == table_name && !m.paused)
        .map(|(_, m)| m.clone())
        .collect()
}

/// Flip the `paused` flag on one database's named index.  Returns `false` if
/// that database has no index by that name.
pub fn set_ast_index_paused(database: u64, name: &str, paused: bool) -> bool {
    let mut reg = ast_registry().write().unwrap_or_else(|p| p.into_inner());
    match reg.get_mut(&(database, name.to_string())) {
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
    // One indexer per database at a time: the keyed cleanup reads ids before
    // the chunk's transaction opens, and a second concurrent call would see
    // this one's transaction through the process-global slot.
    let indexer_mutex = indexer_lock(db.storage.instance_id());
    let _indexer_held = indexer_mutex.lock().unwrap_or_else(|e| e.into_inner());

    bootstrap_tables(db)?;
    mark_extension_installed();

    let triage_started = std::time::Instant::now();
    let only: Option<&[String]> = opts.only_paths.as_deref();
    // A targeted run with nothing to look at does no table scan at all.
    if only.is_some_and(<[String]>::is_empty) {
        return Ok(CodeIndexStats::default());
    }
    let files = fetch_source_files(db, &opts.source_table, only)?;
    let mut stored = fetch_stored_files(db, &opts.source_table, only)?;
    let mut stats = CodeIndexStats::default();
    let mut timers = PhaseTimers::default();
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
    let truncated = opts.force_reparse && only.is_none() && !stored.is_empty();
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
        stored.clear();
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
    // Files of this source table that left the source: stored but no longer
    // present. Computed from the source rows of ANY language (a row whose
    // language has no extractor is skipped, not removed).
    let present: std::collections::HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let mut removed_ids: Vec<i64> = stored
        .iter()
        .filter(|(path, _)| !present.contains(path.as_str()))
        .map(|(_, f)| f.id)
        .collect();
    removed_ids.sort_unstable();
    drop(present);
    // Hash on the dedicated pool when there is enough to hash; a handful of
    // files (a hook, an agent edit) stays serial and builds no pool.
    let workers = opts.resolved_parallelism();
    let mut pool: Option<rayon::ThreadPool> = None;
    let shas: Vec<String> = if files.len() >= PARALLEL_HASH_MIN && workers > 1 {
        use rayon::prelude::*;
        let built = build_index_pool(workers)?;
        let shas = built.install(|| files.par_iter().map(|f| sha256_hex(&f.content)).collect());
        pool = Some(built);
        shas
    } else {
        files.iter().map(|f| sha256_hex(&f.content)).collect()
    };
    let mut to_parse: Vec<(SourceFile, String, ParseExtractPair)> = Vec::new();
    for (file, sha) in files.into_iter().zip(shas) {
        stats.files_seen += 1;
        let resolution = match resolve_parser_and_extractor(&file.lang) {
            Some(r) => r,
            None => {
                stats.files_skipped += 1;
                continue;
            }
        };
        lang_set.insert(file.lang.to_ascii_lowercase());
        let unchanged = stored.get(&file.path).map(|f| f.sha == sha).unwrap_or(false);
        if unchanged && !opts.force_reparse {
            stats.files_unchanged += 1;
            continue;
        }
        to_parse.push((file, sha, resolution));
    }

    let mut touched = !to_parse.is_empty();
    // Files whose symbols go away (re-parsed or removed) must not be binding
    // targets of this run.
    let bind_at_insert = !opts.skip_cross_file_resolve && !opts.skip_symbol_refs;
    let mut names = if bind_at_insert && (!to_parse.is_empty() || !removed_ids.is_empty()) {
        let mut excluded: std::collections::HashSet<i64> = removed_ids.iter().copied().collect();
        for (file, _, _) in &to_parse {
            if let Some(sf) = stored.get(&file.path) {
                excluded.insert(sf.id);
            }
        }
        Some(NameMap::load(db, &excluded)?)
    } else {
        None
    };
    let mut tail = TailState::default();
    let older_refs_possible = !stored.is_empty();
    stats.triage_ms = triage_started.elapsed().as_millis() as u64;

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
    stats.parse_workers = workers as u32;

    let effective_chunk = opts
        .chunk_size
        .or_else(|| (to_parse.len() > DEFAULT_CHUNK_THRESHOLD).then_some(DEFAULT_CHUNK_FILES));
    let chunks: Vec<Vec<(SourceFile, String, ParseExtractPair)>> = match effective_chunk {
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
        match pool {
            Some(p) => Some(p),
            None => Some(build_index_pool(workers)?),
        }
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
    // sprinter 0d6695bf8a86: the per-chunk `db.begin()` / `db.commit()` below is
    // ENGINE-INTERNAL work on this handle, not the work of whatever connection
    // asked for the index. The process-global transaction slot is now owned by
    // the caller that opened it, and a statement arriving through a wire session
    // never sees it — so without this detach a `CREATE AST INDEX` sent over the
    // wire would open a transaction its own `drain_chunk` writes cannot see, and
    // every row would pay a WAL fsync: exactly the regression the Tier 1.1
    // prelude above exists to prevent. Restores the previous marker on Drop, so
    // the caller's own statement scope is untouched after this call returns.
    let _detached = crate::SessionBoundStatementGuard::detach();
    // sprinter 6bc4a4040eb7 (phase B2): on a branch too — B1 had to
    // autocommit every statement on a branch while transactions there were
    // refused; a chunk on a branch is one atomic transaction again, reading
    // its own branch writes through the branch view.
    let manage_txn = !db.in_transaction();
    // Keyed cleanup (ids read with autocommit reads that the secondary indexes
    // serve, then primary-key deletes) needs the indexes (main only) and its
    // own transaction. A caller-owned transaction or a branch keeps the
    // per-file path.
    let keyed = manage_txn && db.current_branch() == "main";

    // Files that left the source table: removed before any chunk, in their own
    // transaction (idempotent, and independent of the chunks' atomicity).
    if !removed_ids.is_empty() {
        let started = std::time::Instant::now();
        // Ids are read BEFORE the transaction opens (index-served autocommit
        // reads); inside it the removal only deletes by primary key.
        let removal_plan = if keyed {
            Some(plan_cleanup(db, &removed_ids)?)
        } else {
            None
        };
        if manage_txn {
            db.begin()?;
        }
        let removal = remove_files(db, &removed_ids, removal_plan.as_ref());
        if manage_txn {
            match removal {
                Ok(()) => db.commit()?,
                Err(e) => {
                    let _ = db.rollback();
                    return Err(e);
                }
            }
        } else {
            removal?;
        }
        stats.files_removed = removed_ids.len() as u64;
        touched = true;
        timers.cleanup += started.elapsed();
        stats.write_elapsed_ms += started.elapsed().as_millis() as u64;
    }

    // Tier 1.3 correctness guard: `skip_delete_stale` (set when we
    // truncated up front) tells `write_one_parsed` to skip the
    // SELECT-existing-symbols / DELETE-prior-refs / DELETE-prior-symbols
    // preamble — safe ONLY for the FIRST occurrence of a given path
    // in this call. If `source_table` happens to hold duplicate
    // `path` rows (e.g. a buggy upsert client), subsequent occurrences
    // need the normal delete-stale loop or symbols/refs accumulate.
    // We track which paths we've already drained per call.
    let mut processed_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    // -------- Per-chunk: parallel parse → transactional drain --------
    for chunk in chunks {
        let chunk_paths: Vec<String> = chunk.iter().map(|(f, _, _)| f.path.clone()).collect();
        let parse_started = std::time::Instant::now();
        let parsed: Vec<Result<ParsedFile>> = if let Some(pool) = &pool {
            use rayon::prelude::*;
            pool.install(|| chunk.into_par_iter().map(parse_extract_one).collect::<Vec<_>>())
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
        // Ids of the chunk's changed existing files, then what they own.
        let plan = if keyed {
            let started = std::time::Instant::now();
            let mut seen_ids = std::collections::HashSet::new();
            let ids: Vec<i64> = chunk_paths
                .iter()
                .filter(|path| !processed_paths.contains(path.as_str()))
                .filter_map(|path| stored.get(path).map(|f| f.id))
                .filter(|id| seen_ids.insert(*id))
                .collect();
            let plan = plan_cleanup(db, &ids)?;
            timers.cleanup += started.elapsed();
            Some(plan)
        } else {
            None
        };
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
            &mut timers,
            &stored,
            plan.as_ref(),
            names.as_mut().map(|n| (n, &mut tail)),
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
    if touched && !opts.skip_cross_file_resolve {
        let cross_started = std::time::Instant::now();
        match names.as_ref() {
            // The refs of this run were bound while they were inserted; only a
            // small tail is left (see `tail_resolve`).
            Some(names) => tail_resolve(db, names, &tail, older_refs_possible, manage_txn, &mut stats)?,
            // `skip_symbol_refs`: nothing was inserted, but older unbound refs
            // may now find a target: the full pass.
            None => {
                resolve_in_own_txn(db, manage_txn, |db| resolve_cross_file_references(db).map(|_| ()))?;
            }
        }
        timers.cross_resolve += cross_started.elapsed();
        stats.write_elapsed_ms += cross_started.elapsed().as_millis() as u64;
    } else if touched {
        tracing::debug!("code_index: skipped cross-file reference resolve");
    }

    stats.cleanup_ms = timers.cleanup.as_millis() as u64;
    stats.files_upsert_ms = timers.files_upsert.as_millis() as u64;
    stats.symbols_insert_ms = timers.symbols_insert.as_millis() as u64;
    stats.refs_insert_ms = timers.refs_insert.as_millis() as u64;
    stats.cross_resolve_ms = timers.cross_resolve.as_millis() as u64;
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
/// **two table-wide batches per chunk** (one for all symbols
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
    timers: &mut PhaseTimers,
    stored: &HashMap<String, StoredFile>,
    plan: Option<&CleanupPlan>,
    mut binder: Option<(&mut NameMap, &mut TailState)>,
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
    if let Some(plan) = plan {
        // Keyed path: one set-based cleanup for the chunk, then the file rows
        // by key (UPDATE by node_id for files already stored, one multi-row
        // INSERT for new ones). No per-file SELECT, none for new files.
        let started = std::time::Instant::now();
        apply_cleanup(db, plan)?;
        timers.cleanup += started.elapsed();

        let started = std::time::Instant::now();
        let mut slots: Vec<Option<i64>> = vec![None; parsed.len()];
        let mut new_idx: Vec<usize> = Vec::new();
        let mut dup_idx: Vec<usize> = Vec::new();
        for (i, p) in parsed.iter().enumerate() {
            if !processed_paths.insert(p.file.path.clone()) {
                dup_idx.push(i);
                continue;
            }
            match stored.get(&p.file.path) {
                Some(sf) => {
                    db.execute_params(
                        "UPDATE _hdb_code_files SET lang = $1, sha256 = $2 WHERE node_id = $3",
                        &[
                            Value::String(p.file.lang.clone()),
                            Value::String(p.sha.clone()),
                            Value::Int8(sf.id),
                        ],
                    )?;
                    slots[i] = Some(sf.id);
                }
                None => new_idx.push(i),
            }
        }
        if !new_idx.is_empty() {
            let list: Vec<(&SourceFile, &str)> = new_idx
                .iter()
                .filter_map(|&i| parsed.get(i).map(|p| (&p.file, p.sha.as_str())))
                .collect();
            let ids = insert_new_files(db, &opts.source_table, &list)?;
            for (k, &i) in new_idx.iter().enumerate() {
                slots[i] = ids.get(k).copied();
            }
        }
        timers.files_upsert += started.elapsed();
        // A path that occurs twice in the source table (no primary key on it):
        // the later occurrence takes the per-file path.
        for &i in &dup_idx {
            if let Some(p) = parsed.get(i) {
                let started = std::time::Instant::now();
                let id = upsert_file(db, &opts.source_table, &p.file, &p.sha)?;
                timers.files_upsert += started.elapsed();
                let started = std::time::Instant::now();
                delete_stale_symbols_and_refs(db, id)?;
                timers.cleanup += started.elapsed();
                slots[i] = Some(id);
            }
        }
        for (p, slot) in parsed.into_iter().zip(slots) {
            let id = slot.ok_or_else(|| Error::storage("code-graph file row was not assigned a key"))?;
            prepared.push((id, p));
        }
    } else {
        for p in parsed {
            let first_time = processed_paths.insert(p.file.path.clone());
            let skip_delete_stale = truncated_this_call && first_time;
            let started = std::time::Instant::now();
            let file_id = upsert_file(db, &opts.source_table, &p.file, &p.sha)?;
            timers.files_upsert += started.elapsed();
            if !skip_delete_stale {
                let started = std::time::Instant::now();
                delete_stale_symbols_and_refs(db, file_id)?;
                timers.cleanup += started.elapsed();
            }
            prepared.push((file_id, p));
        }
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

    let started = std::time::Instant::now();
    let symbol_ids: Vec<i64> = if all_symbols.is_empty() {
        Vec::new()
    } else {
        bulk_insert_symbols_batched(db, embedder, stats, &sym_owner_file_ids, &all_symbols)?
    };
    timers.symbols_insert += started.elapsed();
    if let Some((names, _)) = binder.as_mut() {
        for (sym, id) in all_symbols.iter().zip(symbol_ids.iter()) {
            names.add(&sym.name, *id);
        }
    }

    // Step 4 — flatten ALL refs across files. For each file slice
    // its symbol_ids out of the flat result; convert per-file
    // ResolvedRef.from_idx / to_idx into absolute symbol_ids;
    // collect into one Tuple vec; transaction-owned bounded SQL batches.
    let total_refs: usize = if opts.skip_symbol_refs {
        0
    } else {
        prepared.iter().map(|(_, p)| p.resolved.len()).sum()
    };
    if total_refs > 0 {
        let started = std::time::Instant::now();
        let written = bulk_insert_refs_batched(
            db,
            &prepared,
            &symbol_ids,
            &sym_ranges,
            binder.as_mut().map(|(n, t)| (&**n, &mut **t)),
        )?;
        timers.refs_insert += started.elapsed();
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
        let csv = stale_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        db.execute(&format!(
            "UPDATE _hdb_code_symbol_refs \
                SET to_symbol = NULL, resolution = 'unresolved' \
              WHERE to_symbol IN ({csv})"
        ))?;
    }
    db.execute(&format!("DELETE FROM _hdb_code_symbol_refs WHERE file_id = {file_id}"))?;
    db.execute(&format!("DELETE FROM _hdb_code_symbols WHERE file_id = {file_id}"))?;
    Ok(())
}

/// Insert engine-owned graph rows through the existing SQL transaction owner.
/// A chunk's DELETE may hold an incoming-FK graph exclusively until commit;
/// raw `bulk_insert_tuples` would acquire a different owner and, even without
/// that self-wait, its direct writes would escape the chunk's rollback.
/// Bound SQL/parameter construction while preserving input-order RETURNING IDs
/// for the later reference mapping. Every batch joins the caller/chunk owner.
fn insert_graph_rows(
    db: &EmbeddedDatabase,
    table: &str,
    schema: &crate::Schema,
    tuples: Vec<Tuple>,
) -> Result<Vec<i64>> {
    use std::fmt::Write as _;

    if tuples.is_empty() {
        return Ok(Vec::new());
    }
    let expected_key = match table {
        "_hdb_code_symbols" => "node_id",
        "_hdb_code_symbol_refs" => "edge_id",
        _ => return Err(Error::storage("unsupported code-graph row target")),
    };
    let key = schema
        .columns
        .first()
        .filter(|column| column.primary_key && column.name == expected_key)
        .ok_or_else(|| Error::storage("code-graph primary-key layout changed"))?;
    let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
    let columns = schema
        .columns
        .iter()
        .skip(1)
        .map(|column| quote(&column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut ids = Vec::with_capacity(tuples.len());
    let mut pending = tuples.into_iter();
    loop {
        let rows: Vec<_> = pending.by_ref().take(256).collect();
        if rows.is_empty() {
            break;
        }
        let expected_rows = rows.len();
        let mut sql = format!("INSERT INTO {} ({columns}) VALUES ", quote(table));
        let mut params = Vec::new();
        for (row_index, tuple) in rows.into_iter().enumerate() {
            if tuple.values.len() != schema.columns.len() || !matches!(tuple.values.first(), Some(Value::Null)) {
                return Err(Error::storage("code-graph row shape or generated primary key changed"));
            }
            if row_index != 0 {
                sql.push_str(", ");
            }
            sql.push('(');
            for (column, value) in tuple.values.into_iter().skip(1).enumerate() {
                if column != 0 {
                    sql.push_str(", ");
                }
                params.push(value);
                write!(sql, "${}", params.len())
                    .map_err(|error| Error::storage(format!("code-graph SQL construction: {error}")))?;
            }
            sql.push(')');
        }
        write!(sql, " RETURNING {}", quote(&key.name))
            .map_err(|error| Error::storage(format!("code-graph RETURNING construction: {error}")))?;
        let (affected, returned) = db.execute_params_returning(&sql, &params)?;
        if usize::try_from(affected).ok() != Some(expected_rows) || returned.len() != expected_rows {
            return Err(Error::storage("code-graph INSERT returned a different number of rows"));
        }
        for row in returned {
            let id = match row.values.as_slice() {
                [Value::Int8(id)] => *id,
                [Value::Int4(id)] => i64::from(*id),
                _ => return Err(Error::storage("code-graph INSERT returned an invalid primary key")),
            };
            if id <= 0 {
                return Err(Error::storage("code-graph INSERT returned a nonpositive primary key"));
            }
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Step 3 helper: bulk-insert all symbols across all files in a
/// chunk using transaction-owned, bounded SQL INSERT batches. Mirror of the
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

    // Batch the embed calls: collect every non-empty signature, embed them all
    // in ONE `embed_batch` call, then scatter results back to their original
    // indices (empty signatures stay None). This replaces the per-symbol serial
    // embed — the dominant ingest cost (one Mutex-guarded single-text ORT call
    // per symbol) — with a single batched inference. Semantics are preserved:
    // empty → None, `embed_calls` counts produced vectors, NoopEmbedder/Http
    // keep their per-text behavior via the trait's default `embed_batch`.
    let mut vectors: Vec<Option<Vec<f32>>> = vec![None; symbols.len()];
    let mut batch_texts: Vec<&str> = Vec::with_capacity(symbols.len());
    let mut batch_idx: Vec<usize> = Vec::with_capacity(symbols.len());
    for (i, sym) in symbols.iter().enumerate() {
        if !sym.signature.is_empty() {
            batch_texts.push(sym.signature.as_str());
            batch_idx.push(i);
        }
    }
    if !batch_texts.is_empty() {
        let embedded = embedder.embed_batch(&batch_texts)?;
        debug_assert_eq!(embedded.len(), batch_idx.len());
        for (slot, v) in batch_idx.iter().zip(embedded.into_iter()) {
            if v.is_some() {
                stats.embed_calls += 1;
            }
            vectors[*slot] = v;
        }
    }

    let any_vec = vectors.iter().any(Option::is_some);
    if any_vec {
        let dim = vectors.iter().find_map(|v| v.as_ref().map(|v| v.len())).unwrap_or(0);
        if dim == 0 {
            return Err(Error::query_execution("embedder returned a zero-length vector"));
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
    insert_graph_rows(db, "_hdb_code_symbols", &schema, tuples)
}

/// Step 4 helper: bulk-insert all refs across all files in a chunk
/// in transaction-owned, bounded SQL INSERT batches. For each file's resolved
/// refs, slice the per-file symbol_ids out of the flat batch result
/// and translate ResolvedRef.from_idx / to_idx into absolute ids.
fn bulk_insert_refs_batched(
    db: &EmbeddedDatabase,
    prepared: &[(i64, ParsedFile)],
    symbol_ids: &[i64],
    sym_ranges: &[(usize, usize)],
    mut binder: Option<(&NameMap, &mut TailState)>,
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
    let mut tails: Vec<RefTail> = Vec::new();
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
            let mut to_val = match r.to_idx {
                Some(idx) => file_symbol_ids
                    .get(idx)
                    .map(|id| Value::Int8(*id))
                    .unwrap_or(Value::Null),
                None => Value::Null,
            };
            let mut res = match r.resolution {
                Resolution::Exact => "exact",
                Resolution::Heuristic => "heuristic",
                Resolution::Unresolved => "unresolved",
            };
            let mut tail = RefTail::None;
            if let (Some((names, _)), true) = (binder.as_ref(), r.to_idx.is_none()) {
                // A cross-file reference: bind it to the corpus definition now.
                // Same rule as the old UPDATE pass: the lowest node_id wins and
                // the label is exact only for a name with one definition.
                // A ref an import rebound (`exact`/`heuristic` with no target
                // id) gets the same binding, and keeps `exact` only when the
                // import was unique and the name has one definition.
                let bare = last_segment(&r.to_name);
                let exact_ok = matches!(r.resolution, Resolution::Unresolved | Resolution::Exact);
                match names.first.get(bare) {
                    Some((id, count)) => {
                        to_val = Value::Int8(*id);
                        let exact = *count == 1 && exact_ok;
                        res = if exact { "exact" } else { "heuristic" };
                        if *count == 1 {
                            tail = RefTail::BoundExact { bare: bare.to_string() };
                        }
                    }
                    None => {
                        tail = RefTail::Unbound {
                            bare: bare.to_string(),
                            exact_ok,
                        };
                    }
                }
            }
            tails.push(tail);
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
        let edge_ids = insert_graph_rows(db, "_hdb_code_symbol_refs", &schema, tuples)?;
        if let Some((_, state)) = binder.as_mut() {
            for (edge_id, tail) in edge_ids.into_iter().zip(tails) {
                match tail {
                    RefTail::None => {}
                    RefTail::Unbound { bare, exact_ok } => state.unbound.push((edge_id, bare, exact_ok)),
                    RefTail::BoundExact { bare } => state.bound_exact.entry(bare).or_default().push(edge_id),
                }
            }
        }
    }
    Ok(written)
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
fn parse_extract_one(item: (SourceFile, String, ParseExtractPair)) -> Result<ParsedFile> {
    let (file, sha, resolution) = item;
    let tree = match &resolution {
        ParseExtractPair::Static(lang) => parse::parse(*lang, &file.content)?,
        ParseExtractPair::Dynamic { .. } => parse::parse_by_name(&file.lang, &file.content)?,
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
    Ok(ParsedFile {
        file,
        sha,
        symbols,
        resolved,
    })
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
    if super::parse::registered_grammars().iter().any(|g| g == &canonical) {
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
    // The engine's own change to a table it owns.
    crate::sql::dict::reserved::engine_ddl(|| {
        db.execute(&format!(
            "ALTER TABLE _hdb_code_symbols ADD COLUMN IF NOT EXISTS body_vec VECTOR({dim})"
        ))
    })?;
    Ok(())
}

/// A `_hdb_code_files` row of one source table: its key and stored hash.
struct StoredFile {
    id: i64,
    sha: String,
}

/// The stored files of ONE source table, by path. (Two source tables may
/// hold the same path; the hash of one must not stand for the other.)
fn fetch_stored_files(
    db: &EmbeddedDatabase,
    source_table: &str,
    only: Option<&[String]>,
) -> Result<HashMap<String, StoredFile>> {
    // Probe first — _hdb_code_files may not exist on the very first call.
    let probe = db.query("SELECT 1 FROM _hdb_code_files LIMIT 1", &[]);
    if probe.is_err() {
        return Ok(HashMap::new());
    }
    let mut rows = Vec::new();
    match only {
        None => {
            rows = db.query_params(
                "SELECT node_id, path, sha256 FROM _hdb_code_files WHERE source_table = $1",
                &[Value::String(source_table.to_string())],
            )?
        }
        Some(paths) => {
            for batch in paths.chunks(KEY_BATCH) {
                let marks = (2..=batch.len() + 1)
                    .map(|i| format!("${i}"))
                    .collect::<Vec<_>>()
                    .join(",");
                let mut params = vec![Value::String(source_table.to_string())];
                params.extend(batch.iter().map(|p| Value::String(p.clone())));
                rows.extend(db.query_params(
                    &format!(
                        "SELECT node_id, path, sha256 FROM _hdb_code_files WHERE source_table = $1 AND path IN ({marks})"
                    ),
                    &params,
                )?);
            }
        }
    }
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        let id = match row.values.first() {
            Some(Value::Int4(n)) => i64::from(*n),
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        let path = match row.values.get(1) {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let sha = match row.values.get(2) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        out.insert(path, StoredFile { id, sha });
    }
    Ok(out)
}

/// Process-local indexer lock per database. The keyed cleanup reads ids
/// before the chunk's transaction opens, so two `code_index` calls on one
/// database must not interleave (and the engine's transaction slot is
/// process-global, so a second call would otherwise see the first one's
/// transaction as the caller's).
fn indexer_lock(instance_id: u64) -> std::sync::Arc<std::sync::Mutex<()>> {
    use once_cell::sync::Lazy;
    use std::sync::{Arc, Mutex};
    static LOCKS: Lazy<Mutex<HashMap<u64, Arc<Mutex<()>>>>> = Lazy::new(|| Mutex::new(HashMap::new()));
    let mut map = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(map.entry(instance_id).or_insert_with(|| Arc::new(Mutex::new(()))))
}

const KEY_BATCH: usize = 1000;

/// A run that parses more files than this and has no explicit `chunk_size`
/// is chunked by [`DEFAULT_CHUNK_FILES`] (see `CodeIndexOptions::chunk_size`).
pub const DEFAULT_CHUNK_THRESHOLD: usize = 5000;
/// Files per chunk of a defaulted chunked run.
pub const DEFAULT_CHUNK_FILES: usize = 2000;

/// Corpus-wide `name -> (lowest node_id, number of definitions)`, the table
/// the cross-file pass used to rebuild with a scan. Loaded once per call
/// (without the symbols of files about to be re-parsed or removed) and
/// extended by every chunk's new symbols, so cross-file references can be
/// bound while they are inserted instead of by UPDATEs afterwards.
#[derive(Default)]
struct NameMap {
    first: HashMap<String, (i64, u32)>,
    /// Names that went from one definition to two or more during this run:
    /// refs labelled `exact` for them need relabelling in the tail.
    crossed: std::collections::HashSet<String>,
    added: bool,
}

impl NameMap {
    fn add(&mut self, name: &str, id: i64) {
        self.added = true;
        match self.first.get_mut(name) {
            Some(entry) => {
                entry.0 = entry.0.min(id);
                entry.1 += 1;
                if entry.1 == 2 {
                    self.crossed.insert(name.to_string());
                }
            }
            None => {
                self.first.insert(name.to_string(), (id, 1));
            }
        }
    }

    fn load(db: &EmbeddedDatabase, excluded_files: &std::collections::HashSet<i64>) -> Result<Self> {
        let mut map = NameMap::default();
        for row in db.query("SELECT name, node_id, file_id FROM _hdb_code_symbols", &[])? {
            let mut values = row.values.into_iter();
            let name = match values.next() {
                Some(Value::String(s)) => s,
                _ => continue,
            };
            let id = match value_i64(values.next().as_ref()) {
                Some(id) => id,
                None => continue,
            };
            if let Some(file_id) = value_i64(values.next().as_ref()) {
                if excluded_files.contains(&file_id) {
                    continue;
                }
            }
            map.add(&name, id);
        }
        // Loading is not a run-time addition: nothing "crossed" yet.
        map.crossed.clear();
        map.added = false;
        Ok(map)
    }
}

/// Refs this run inserted whose target may still change.
#[derive(Default)]
struct TailState {
    /// `(edge_id, bare name, exact_ok)`: inserted with NULL `to_symbol` because
    /// the name had no definition yet. `exact_ok` = it may be labelled `exact`
    /// when the name turns out to have one definition (a plain unresolved ref,
    /// or one an import rebound with a unique import).
    unbound: Vec<(i64, String, bool)>,
    /// Refs bound to a name that had exactly one definition when inserted.
    bound_exact: HashMap<String, Vec<i64>>,
}

/// How a to-be-inserted ref relates to the tail.
enum RefTail {
    None,
    Unbound { bare: String, exact_ok: bool },
    BoundExact { bare: String },
}

/// Files at or above this count are hashed on the parse pool.
const PARALLEL_HASH_MIN: usize = 64;

fn build_index_pool(workers: usize) -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .thread_name(|i| format!("hdb-code-index-{i}"))
        .build()
        .map_err(|e| Error::query_execution(format!("failed to build code-index thread pool ({e})")))
}

fn int_list(ids: &[i64]) -> String {
    ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
}

fn value_i64(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Int4(n)) => Some(i64::from(*n)),
        Some(Value::Int8(n)) => Some(*n),
        _ => None,
    }
}

/// What a set of files owns, collected with autocommit reads (index-served:
/// `file_id IN` and `to_symbol IN` on single tables) BEFORE the chunk's
/// transaction opens, so the transaction itself only deletes by primary key.
#[derive(Default)]
struct CleanupPlan {
    /// `_hdb_code_symbol_refs.edge_id` of refs that occur in the files.
    ref_ids: Vec<i64>,
    /// `_hdb_code_symbols.node_id` of the files' symbols.
    symbol_ids: Vec<i64>,
    /// Refs of OTHER files that point at those symbols (to be unresolved).
    inbound_ids: Vec<i64>,
}

fn plan_cleanup(db: &EmbeddedDatabase, file_ids: &[i64]) -> Result<CleanupPlan> {
    let mut plan = CleanupPlan::default();
    for batch in file_ids.chunks(KEY_BATCH) {
        let csv = int_list(batch);
        for row in db.query(
            &format!("SELECT node_id FROM _hdb_code_symbols WHERE file_id IN ({csv})"),
            &[],
        )? {
            if let Some(id) = value_i64(row.values.first()) {
                plan.symbol_ids.push(id);
            }
        }
        for row in db.query(
            &format!("SELECT edge_id FROM _hdb_code_symbol_refs WHERE file_id IN ({csv})"),
            &[],
        )? {
            if let Some(id) = value_i64(row.values.first()) {
                plan.ref_ids.push(id);
            }
        }
    }
    let deleting: std::collections::HashSet<i64> = plan.ref_ids.iter().copied().collect();
    for batch in plan.symbol_ids.chunks(KEY_BATCH) {
        let csv = int_list(batch);
        for row in db.query(
            &format!("SELECT edge_id FROM _hdb_code_symbol_refs WHERE to_symbol IN ({csv})"),
            &[],
        )? {
            if let Some(id) = value_i64(row.values.first()) {
                if !deleting.contains(&id) {
                    plan.inbound_ids.push(id);
                }
            }
        }
    }
    Ok(plan)
}

/// Apply a plan inside the chunk transaction: unresolve inbound refs, then
/// delete the files' refs and symbols, all by primary key.
fn apply_cleanup(db: &EmbeddedDatabase, plan: &CleanupPlan) -> Result<()> {
    for batch in plan.inbound_ids.chunks(KEY_BATCH) {
        db.execute(&format!(
            "UPDATE _hdb_code_symbol_refs SET to_symbol = NULL, resolution = 'unresolved' \
             WHERE edge_id IN ({})",
            int_list(batch)
        ))?;
    }
    for batch in plan.ref_ids.chunks(KEY_BATCH) {
        db.execute(&format!(
            "DELETE FROM _hdb_code_symbol_refs WHERE edge_id IN ({})",
            int_list(batch)
        ))?;
    }
    for batch in plan.symbol_ids.chunks(KEY_BATCH) {
        db.execute(&format!(
            "DELETE FROM _hdb_code_symbols WHERE node_id IN ({})",
            int_list(batch)
        ))?;
    }
    Ok(())
}

/// Insert `_hdb_code_files` rows for new files in multi-row batches and
/// return their ids in input order.
fn insert_new_files(db: &EmbeddedDatabase, source_table: &str, files: &[(&SourceFile, &str)]) -> Result<Vec<i64>> {
    use std::fmt::Write as _;
    let mut ids = Vec::with_capacity(files.len());
    for batch in files.chunks(200) {
        let mut sql = String::from("INSERT INTO _hdb_code_files (source_table, path, lang, sha256) VALUES ");
        let mut params: Vec<Value> = Vec::with_capacity(batch.len() * 4);
        for (i, (file, sha)) in batch.iter().enumerate() {
            if i != 0 {
                sql.push_str(", ");
            }
            let base = params.len();
            write!(sql, "(${}, ${}, ${}, ${})", base + 1, base + 2, base + 3, base + 4)
                .map_err(|e| Error::storage(format!("code-graph SQL construction: {e}")))?;
            params.push(Value::String(source_table.to_string()));
            params.push(Value::String(file.path.clone()));
            params.push(Value::String(file.lang.clone()));
            params.push(Value::String((*sha).to_string()));
        }
        sql.push_str(" RETURNING node_id");
        let (affected, returned) = db.execute_params_returning(&sql, &params)?;
        if usize::try_from(affected).ok() != Some(batch.len()) || returned.len() != batch.len() {
            return Err(Error::storage(
                "code-graph file INSERT returned a different number of rows",
            ));
        }
        for row in returned {
            ids.push(
                value_i64(row.values.first())
                    .ok_or_else(|| Error::storage("code-graph file INSERT returned an invalid key"))?,
            );
        }
    }
    Ok(ids)
}

/// Remove files that left the source table: their refs and symbols (keyed),
/// refs of other files that pointed at them (unresolved), the file rows.
fn remove_files(db: &EmbeddedDatabase, file_ids: &[i64], plan: Option<&CleanupPlan>) -> Result<()> {
    if file_ids.is_empty() {
        return Ok(());
    }
    if let Some(plan) = plan {
        apply_cleanup(db, plan)?;
    } else {
        for id in file_ids {
            delete_stale_symbols_and_refs(db, *id)?;
        }
    }
    for batch in file_ids.chunks(KEY_BATCH) {
        db.execute(&format!(
            "DELETE FROM _hdb_code_files WHERE node_id IN ({})",
            int_list(batch)
        ))?;
    }
    Ok(())
}

/// UPDATEs per transaction in the tail pass of a run that owns its transaction.
const TAIL_COMMIT_EVERY: u64 = 200;

/// Run `f` inside its own transaction when the indexer owns the transaction.
fn resolve_in_own_txn(
    db: &EmbeddedDatabase,
    manage_txn: bool,
    f: impl FnOnce(&EmbeddedDatabase) -> Result<()>,
) -> Result<()> {
    if manage_txn {
        db.begin()?;
    }
    let result = f(db);
    if manage_txn {
        match result {
            Ok(()) => db.commit()?,
            Err(e) => {
                let _ = db.rollback();
                return Err(e);
            }
        }
        Ok(())
    } else {
        result
    }
}

/// The corpus-wide pass: bind every ref with no target (`to_symbol IS NULL`)
/// whose last name segment is defined somewhere. For skip-then-resolve
/// workflows (`skip_cross_file_resolve`, or `skip_symbol_refs` runs); a normal
/// run binds while inserting and only runs the small tail. Returns the number
/// of refs bound. Label rule: the lowest `node_id` wins, `exact` for a name with
/// one definition (and, for a ref an import rebound, only if it was `exact`),
/// `heuristic` otherwise.
pub fn resolve_cross_file_references(db: &EmbeddedDatabase) -> Result<u64> {
    let names = NameMap::load(db, &std::collections::HashSet::new())?;
    let unbound = db.query(
        "SELECT edge_id, to_name, resolution FROM _hdb_code_symbol_refs WHERE to_symbol IS NULL",
        &[],
    )?;
    let mut updates: HashMap<(i64, &'static str), Vec<i64>> = HashMap::new();
    for row in unbound {
        let mut v = row.values.into_iter();
        let Some(edge_id) = value_i64(v.next().as_ref()) else {
            continue;
        };
        let Some(Value::String(to_name)) = v.next() else {
            continue;
        };
        let resolution = match v.next() {
            Some(Value::String(s)) => s,
            _ => String::new(),
        };
        if let Some((id, count)) = names.first.get(last_segment(&to_name)) {
            let exact_ok = resolution != "heuristic";
            let label = if *count == 1 && exact_ok { "exact" } else { "heuristic" };
            updates.entry((*id, label)).or_default().push(edge_id);
        }
    }
    let mut bound = 0u64;
    let mut keys: Vec<_> = updates.keys().copied().collect();
    keys.sort_unstable();
    for key in keys {
        let Some(edge_ids) = updates.get(&key) else { continue };
        for batch in edge_ids.chunks(KEY_BATCH) {
            db.execute(&format!(
                "UPDATE _hdb_code_symbol_refs SET to_symbol = {}, resolution = '{}' WHERE edge_id IN ({})",
                key.0,
                key.1,
                int_list(batch)
            ))?;
            bound += batch.len() as u64;
        }
    }
    Ok(bound)
}

/// What is left after binding at insert: refs whose target name a later chunk
/// (or an older run) defined, and relabels of names that gained a second
/// definition during the run. In-memory lists for this run's refs; one
/// autocommit read of older unbound refs, only when this run added symbols
/// and the store held files before it. UPDATEs are grouped (target, label)
/// by 1000 edge ids and committed every [`TAIL_COMMIT_EVERY`] statements.
fn tail_resolve(
    db: &EmbeddedDatabase,
    names: &NameMap,
    tail: &TailState,
    older_refs_possible: bool,
    manage_txn: bool,
    stats: &mut CodeIndexStats,
) -> Result<()> {
    let mut updates: HashMap<(i64, &'static str), Vec<i64>> = HashMap::new();
    let mut this_run: std::collections::HashSet<i64> = std::collections::HashSet::with_capacity(tail.unbound.len());
    for (edge_id, bare, exact_ok) in &tail.unbound {
        this_run.insert(*edge_id);
        if let Some((id, count)) = names.first.get(bare.as_str()) {
            let label = if *count == 1 && *exact_ok { "exact" } else { "heuristic" };
            updates.entry((*id, label)).or_default().push(*edge_id);
        }
    }
    if names.added && older_refs_possible {
        for row in db.query(
            "SELECT edge_id, to_name, resolution FROM _hdb_code_symbol_refs WHERE to_symbol IS NULL",
            &[],
        )? {
            let mut v = row.values.into_iter();
            let Some(edge_id) = value_i64(v.next().as_ref()) else {
                continue;
            };
            if this_run.contains(&edge_id) {
                continue;
            }
            let Some(Value::String(to_name)) = v.next() else {
                continue;
            };
            let resolution = match v.next() {
                Some(Value::String(s)) => s,
                _ => String::new(),
            };
            if let Some((id, count)) = names.first.get(last_segment(&to_name)) {
                let exact_ok = resolution != "heuristic";
                let label = if *count == 1 && exact_ok { "exact" } else { "heuristic" };
                updates.entry((*id, label)).or_default().push(edge_id);
            }
        }
    }
    let mut relabel: Vec<i64> = Vec::new();
    let mut crossed: Vec<&String> = names.crossed.iter().collect();
    crossed.sort();
    for name in crossed {
        if let Some(ids) = tail.bound_exact.get(name.as_str()) {
            relabel.extend(ids.iter().copied());
        }
    }
    if updates.is_empty() && relabel.is_empty() {
        return Ok(());
    }

    let mut statements = 0u64;
    let mut in_txn = false;
    let run = |statements: &mut u64, in_txn: &mut bool, sql: String| -> Result<()> {
        if manage_txn && !*in_txn {
            db.begin()?;
            *in_txn = true;
        }
        db.execute(&sql)?;
        *statements += 1;
        if manage_txn && *statements % TAIL_COMMIT_EVERY == 0 {
            db.commit()?;
            *in_txn = false;
        }
        Ok(())
    };
    let result = (|| -> Result<()> {
        let mut keys: Vec<_> = updates.keys().copied().collect();
        keys.sort_unstable();
        for key in keys {
            let Some(edge_ids) = updates.get(&key) else { continue };
            for batch in edge_ids.chunks(KEY_BATCH) {
                run(
                    &mut statements,
                    &mut in_txn,
                    format!(
                        "UPDATE _hdb_code_symbol_refs SET to_symbol = {}, resolution = '{}' WHERE edge_id IN ({})",
                        key.0,
                        key.1,
                        int_list(batch)
                    ),
                )?;
            }
        }
        for batch in relabel.chunks(KEY_BATCH) {
            run(
                &mut statements,
                &mut in_txn,
                format!(
                    "UPDATE _hdb_code_symbol_refs SET resolution = 'heuristic' WHERE edge_id IN ({})",
                    int_list(batch)
                ),
            )?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            if manage_txn && in_txn {
                db.commit()?;
            }
        }
        Err(e) => {
            if manage_txn && in_txn {
                let _ = db.rollback();
            }
            return Err(e);
        }
    }
    stats.tail_statements = statements;
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

    // Secondary indexes. They serve READS and foreign-key probes: a column
    // that only has REFERENCES is scanned on read, so `file_id = X`,
    // `to_symbol IN (..)` and the like need these. They do NOT make
    // `DELETE ... WHERE file_id = X` select its rows through the index:
    // UPDATE/DELETE row selection uses the primary key or a scan, so the
    // indexer reads ids first (index-served) and deletes by primary key.
    // `name` serves lsp_definition, `path` serves document symbols and the
    // definition path hint (UNIQUE(source_table, path) is composite and
    // cannot serve a `path = ..` probe). Idempotent; a store created by an
    // older build gains them (with a backfill) at the next call.
    // Secondary CREATE INDEX is deliberately unsupported on a branch.
    // These four nonunique indexes are main-only performance aids; branch
    // PK/FK semantics come from the table definitions above. Issuing this
    // known refusal inside a caller transaction would abort it even if the
    // error were ignored. All main-side bootstrap errors must propagate.
    if db.current_branch() != "main" {
        return Ok(());
    }
    db.execute("CREATE INDEX IF NOT EXISTS idx_hdb_code_symbols_file_id ON _hdb_code_symbols(file_id)")?;
    db.execute("CREATE INDEX IF NOT EXISTS idx_hdb_code_symbol_refs_file_id ON _hdb_code_symbol_refs(file_id)")?;
    db.execute("CREATE INDEX IF NOT EXISTS idx_hdb_code_symbol_refs_to_symbol ON _hdb_code_symbol_refs(to_symbol)")?;
    db.execute(
        "CREATE INDEX IF NOT EXISTS idx_hdb_code_symbol_refs_from_symbol ON _hdb_code_symbol_refs(from_symbol)",
    )?;
    db.execute("CREATE INDEX IF NOT EXISTS idx_hdb_code_symbols_name ON _hdb_code_symbols(name)")?;
    db.execute("CREATE INDEX IF NOT EXISTS idx_hdb_code_files_path ON _hdb_code_files(path)")?;
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

fn fetch_source_files(db: &EmbeddedDatabase, source_table: &str, only: Option<&[String]>) -> Result<Vec<SourceFile>> {
    // NB: a full run takes the whole corpus into memory (the pilot is thousands
    // of files, not millions). A targeted run reads only the listed paths: a
    // text primary-key IN list is index-served, so the cost follows the list.
    let mut out = Vec::new();
    let mut push_rows = |rows: Vec<Tuple>| {
        out.reserve(rows.len());
        for row in rows {
            // Move the strings out of the row instead of cloning them.
            let mut values = row.values.into_iter();
            let path = match values.next() {
                Some(Value::String(s)) => s,
                _ => continue,
            };
            let lang = match values.next() {
                Some(Value::String(s)) => s,
                _ => String::new(),
            };
            let content = match values.next() {
                Some(Value::String(s)) => s,
                _ => String::new(),
            };
            out.push(SourceFile {
                path,
                lang,
                content,
                sha256: None,
            });
        }
    };
    match only {
        None => push_rows(db.query(
            &format!(r#"SELECT "path", "lang", "content" FROM "{source_table}""#),
            &[],
        )?),
        Some(paths) => {
            for batch in paths.chunks(KEY_BATCH) {
                let marks = (1..=batch.len()).map(|i| format!("${i}")).collect::<Vec<_>>().join(",");
                let params: Vec<Value> = batch.iter().map(|p| Value::String(p.clone())).collect();
                push_rows(db.query_params(
                    &format!(r#"SELECT "path", "lang", "content" FROM "{source_table}" WHERE "path" IN ({marks})"#),
                    &params,
                )?);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Write helpers
// ---------------------------------------------------------------------------

fn upsert_file(db: &EmbeddedDatabase, source_table: &str, file: &SourceFile, sha: &str) -> Result<i64> {
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
                other => return Err(Error::query_execution(format!("unexpected file_id type: {other:?}"))),
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
                other => Err(Error::query_execution(format!("unexpected file_id type: {other:?}"))),
            };
        }
    }
    Err(Error::query_execution("RETURNING file_id yielded no rows"))
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
