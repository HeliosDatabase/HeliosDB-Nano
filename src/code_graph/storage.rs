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

use crate::{EmbeddedDatabase, Error, Result, Value};

use super::embed::{Embedder, NoopEmbedder};
use super::parse::{self, Language};
use super::resolver::{resolve_in_file, Resolution};
use super::symbols::{extract, Symbol, SymbolRef};

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
}

impl CodeIndexOptions {
    pub fn for_table(name: impl Into<String>) -> Self {
        Self {
            source_table: name.into(),
            embed_bodies: false,
            embed_endpoint: None,
            embed_bearer: None,
            force_reparse: false,
        }
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
/// [`INSERT_BATCH`] rows / call) and unchanged files are skipped by
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
    let existing_sha = fetch_file_sha_map(db)?;
    let mut stats = CodeIndexStats::default();
    let mut lang_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut touched = false;

    for file in &files {
        stats.files_seen += 1;
        // Resolve a parser + extractor pair: prefer the static
        // dispatch, fall back to the dynamic registry for runtime
        // grammars + extractors.
        let resolution = resolve_parser_and_extractor(&file.lang);
        let resolution = match resolution {
            Some(r) => r,
            None => {
                stats.files_skipped += 1;
                continue;
            }
        };
        lang_set.insert(file.lang.to_ascii_lowercase());

        // Content-hash gate: if the content hash matches the stored
        // sha and the caller did not set `force_reparse`, skip this
        // file entirely. Keeps warm re-indexes close to O(changed).
        let sha = sha256_hex(&file.content);
        let unchanged = existing_sha
            .get(&file.path)
            .map(|s| s == &sha)
            .unwrap_or(false);
        if unchanged && !opts.force_reparse {
            stats.files_unchanged += 1;
            continue;
        }
        touched = true;

        let tree = match &resolution {
            ParseExtractPair::Static(lang) => parse::parse(*lang, &file.content)?,
            ParseExtractPair::Dynamic { .. } => {
                parse::parse_by_name(&file.lang, &file.content)?
            }
        };
        let (symbols, refs) = match &resolution {
            ParseExtractPair::Static(lang) => extract(*lang, &file.content, &tree),
            ParseExtractPair::Dynamic { extractor } => {
                extractor.extract(&file.content, &tree)
            }
        };

        // Upsert the file row, get back the file_id. This also writes
        // the new sha256 so the next run can short-circuit.
        let file_id = upsert_file(db, file, &sha)?;

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

        let symbol_ids = insert_symbols(db, file_id, &symbols, embedder.as_ref(), &mut stats)?;
        let mut resolved = resolve_in_file(&symbols, &refs);
        // Scope-chain pass: upgrade unresolved CALLS/REFERENCES to
        // their imported qualified path when there's an unambiguous
        // matching IMPORTS edge.
        super::resolver::rebind_via_imports(&mut resolved);
        let refs_written = insert_refs(db, file_id, &symbol_ids, &resolved)?;

        stats.files_parsed += 1;
        stats.symbols_written += symbols.len() as u64;
        stats.refs_written += refs_written;
    }

    stats.languages_seen = lang_set.into_iter().collect();

    // Cross-file resolution only pays off if the corpus actually
    // changed this run.
    if touched {
        cross_file_resolve(db, &mut stats)?;
    }

    Ok(stats)
}

/// Max rows per multi-row `INSERT … VALUES (...), (...), …`. Each row
/// binds ~10 parameters, so 100 rows = ~1000 parameters per call —
/// well under any sane engine limit, while collapsing 77 K individual
/// writes on Nano's own `src/` into under 800 batched calls.
const INSERT_BATCH: usize = 100;

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

fn upsert_file(db: &EmbeddedDatabase, file: &SourceFile, sha: &str) -> Result<i64> {
    // Parameterised path — source strings (paths, languages, sha256s)
    // may contain arbitrary characters we refuse to hand-escape.
    let path_val = Value::String(file.path.clone());
    let lang_val = Value::String(file.lang.clone());
    let sha_val = Value::String(sha.to_string());

    let existing = db.query_params(
        "SELECT node_id FROM _hdb_code_files \
         WHERE source_table = 'indexed' AND path = $1",
        &[path_val.clone()],
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
         VALUES ('indexed', $1, $2, $3) RETURNING node_id",
        &[path_val, lang_val, sha_val],
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
    // Compute embeddings up-front so we can: (a) negotiate the
    // VECTOR column dimension once, (b) include body_vec in the
    // batched INSERT path. NoopEmbedder returns None for everything
    // → vectors stays empty → no schema change.
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
        // Pick a dimension from the first non-null vector and stick
        // with it. ensure_body_vec_column installs the column on
        // the first non-null run; subsequent runs require dim
        // match.
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

    let mut ids = Vec::with_capacity(symbols.len());
    // Batch N rows per INSERT. Multi-row VALUES collapses
    // parse+plan+WAL overhead; RETURNING gives us the ids back in
    // insert order so we can bind refs afterward.  When any
    // vector is present, we INSERT the body_vec column too;
    // otherwise we keep the legacy 10-column shape so the
    // embedder-less path stays unchanged.
    let with_vec = any_vec;
    let cols: usize = if with_vec { 11 } else { 10 };
    for (chunk_start, chunk) in symbols
        .chunks(INSERT_BATCH)
        .enumerate()
        .map(|(i, c)| (i * INSERT_BATCH, c))
    {
        let mut sql = String::with_capacity(cols * 8 * chunk.len() + 128);
        if with_vec {
            sql.push_str(
                "INSERT INTO _hdb_code_symbols \
                   (file_id, name, qualified, kind, signature, \
                    visibility, line_start, line_end, byte_start, byte_end, \
                    body_vec) \
                 VALUES ",
            );
        } else {
            sql.push_str(
                "INSERT INTO _hdb_code_symbols \
                   (file_id, name, qualified, kind, signature, \
                    visibility, line_start, line_end, byte_start, byte_end) \
                 VALUES ",
            );
        }
        let mut params: Vec<Value> = Vec::with_capacity(cols * chunk.len());
        for (row_idx, sym) in chunk.iter().enumerate() {
            if row_idx > 0 {
                sql.push(',');
            }
            sql.push('(');
            for col in 0..cols {
                if col > 0 {
                    sql.push(',');
                }
                sql.push('$');
                sql.push_str(&(row_idx * cols + col + 1).to_string());
            }
            sql.push(')');
            params.push(Value::Int8(file_id));
            params.push(Value::String(sym.name.clone()));
            params.push(Value::String(sym.qualified.clone()));
            params.push(Value::String(sym.kind.as_str().to_string()));
            params.push(Value::String(sym.signature.clone()));
            params.push(Value::String(sym.visibility.as_str().to_string()));
            params.push(Value::Int4(sym.line_start as i32));
            params.push(Value::Int4(sym.line_end as i32));
            params.push(Value::Int4(sym.byte_start as i32));
            params.push(Value::Int4(sym.byte_end as i32));
            if with_vec {
                let abs_idx = chunk_start + row_idx;
                let v = vectors
                    .get(abs_idx)
                    .and_then(|v| v.clone())
                    .map(Value::Vector)
                    .unwrap_or(Value::Null);
                params.push(v);
            }
        }
        sql.push_str(" RETURNING node_id");
        let (_, rows) = db.execute_params_returning(&sql, &params)?;
        if rows.len() != chunk.len() {
            return Err(Error::query_execution(format!(
                "batched INSERT returned {} rows, expected {}",
                rows.len(),
                chunk.len()
            )));
        }
        for row in &rows {
            let id = row
                .values
                .first()
                .and_then(|v| match v {
                    Value::Int4(n) => Some(*n as i64),
                    Value::Int8(n) => Some(*n),
                    _ => None,
                })
                .ok_or_else(|| {
                    Error::query_execution("symbol RETURNING yielded no id")
                })?;
            ids.push(id);
        }
    }
    Ok(ids)
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
    const COLS: usize = 7;
    let mut written = 0u64;
    for chunk in resolved.chunks(INSERT_BATCH) {
        let mut sql = String::with_capacity(COLS * 8 * chunk.len() + 128);
        sql.push_str(
            "INSERT INTO _hdb_code_symbol_refs \
               (file_id, from_symbol, to_symbol, to_name, kind, line, resolution) \
             VALUES ",
        );
        let mut params: Vec<Value> = Vec::with_capacity(COLS * chunk.len());
        for (row_idx, r) in chunk.iter().enumerate() {
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
            if row_idx > 0 {
                sql.push(',');
            }
            sql.push('(');
            for col in 0..COLS {
                if col > 0 {
                    sql.push(',');
                }
                sql.push('$');
                sql.push_str(&(row_idx * COLS + col + 1).to_string());
            }
            sql.push(')');
            params.push(Value::Int8(file_id));
            params.push(Value::Int8(from_id));
            params.push(to_val);
            params.push(Value::String(r.to_name.clone()));
            params.push(Value::String(r.kind_str.to_string()));
            params.push(Value::Int4(r.line as i32));
            params.push(Value::String(res.to_string()));
        }
        db.execute_params_returning(&sql, &params)?;
        written += chunk.len() as u64;
    }
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
