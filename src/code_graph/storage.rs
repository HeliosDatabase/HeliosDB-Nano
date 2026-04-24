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

/// Languages phase 1 accepts. Extracted into an enum so SQL-surface
/// callers (phase 2) can advertise the set via a system view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedLanguage {
    Rust,
    Python,
}

impl SupportedLanguage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
        }
    }
}

impl From<SupportedLanguage> for Language {
    fn from(s: SupportedLanguage) -> Self {
        match s {
            SupportedLanguage::Rust => Language::Rust,
            SupportedLanguage::Python => Language::Python,
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

/// Run the indexer over `opts.source_table`. Creates the `_hdb_code_*`
/// tables on first call. Returns statistics; never mutates the source
/// table.
pub fn code_index(db: &EmbeddedDatabase, opts: CodeIndexOptions) -> Result<CodeIndexStats> {
    bootstrap_tables(db)?;
    mark_extension_installed();

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

    let files = fetch_source_files(db, &opts.source_table)?;
    let mut stats = CodeIndexStats::default();
    let mut lang_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for file in &files {
        stats.files_seen += 1;
        let Some(lang) = Language::from_lang_str(&file.lang) else {
            stats.files_skipped += 1;
            continue;
        };
        lang_set.insert(lang.as_str().to_string());

        // Phase 1: always rewrite. Phase 2 gates on content hash.
        let _ = opts.force_reparse;

        let tree = parse::parse(lang, &file.content)?;
        let (symbols, refs) = extract(lang, &file.content, &tree);

        // Upsert the file row, get back the file_id.
        let file_id = upsert_file(db, file)?;

        // Delete previous symbols/refs for this file so we re-populate
        // cleanly. Cross-file refs can point AT symbols in this file
        // from other files, so null those out first — the cross-file
        // resolver at the end of `code_index` rebinds them.
        //
        // Two-step rather than `UPDATE … WHERE to_symbol IN (SELECT …)`
        // — the SQL subquery-in-DML path isn't wired in this engine yet.
        let sym_rows = db.query(
            &format!(
                "SELECT node_id FROM _hdb_code_symbols WHERE file_id = {file_id}"
            ),
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
        let resolved = resolve_in_file(&symbols, &refs);
        let refs_written = insert_refs(db, file_id, &symbol_ids, &resolved)?;

        stats.files_parsed += 1;
        stats.symbols_written += symbols.len() as u64;
        stats.refs_written += refs_written;
    }

    stats.languages_seen = lang_set.into_iter().collect();

    // Cross-file resolution pass: for every unresolved ref, look up
    // the `to_name` in `_hdb_code_symbols` (full corpus) and rebind.
    // Single-candidate hits are marked `exact`; multi-candidate hits
    // bind to the first match with `heuristic`.
    cross_file_resolve(db, &mut stats)?;

    Ok(stats)
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

fn upsert_file(db: &EmbeddedDatabase, file: &SourceFile) -> Result<i64> {
    // Parameterised path — source strings (paths, languages, sha256s)
    // may contain arbitrary characters we refuse to hand-escape.
    let path_val = Value::String(file.path.clone());
    let lang_val = Value::String(file.lang.clone());
    let sha_val = file
        .sha256
        .as_ref()
        .map(|s| Value::String(s.clone()))
        .unwrap_or(Value::Null);

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
    let mut ids = Vec::with_capacity(symbols.len());
    for sym in symbols {
        if !sym.signature.is_empty() {
            let v = embedder.embed(&sym.signature)?;
            if v.is_some() {
                stats.embed_calls += 1;
            }
        }
        let (_, rows) = db.execute_params_returning(
            "INSERT INTO _hdb_code_symbols \
               (file_id, name, qualified, kind, signature, \
                visibility, line_start, line_end, byte_start, byte_end) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             RETURNING node_id",
            &[
                Value::Int8(file_id),
                Value::String(sym.name.clone()),
                Value::String(sym.qualified.clone()),
                Value::String(sym.kind.as_str().to_string()),
                Value::String(sym.signature.clone()),
                Value::String(sym.visibility.as_str().to_string()),
                Value::Int4(sym.line_start as i32),
                Value::Int4(sym.line_end as i32),
                Value::Int4(sym.byte_start as i32),
                Value::Int4(sym.byte_end as i32),
            ],
        )?;
        let id = rows
            .first()
            .and_then(|r| r.values.first())
            .and_then(|v| match v {
                Value::Int4(n) => Some(*n as i64),
                Value::Int8(n) => Some(*n),
                _ => None,
            })
            .ok_or_else(|| Error::query_execution("symbol RETURNING yielded no id"))?;
        ids.push(id);
    }
    Ok(ids)
}

fn insert_refs(
    db: &EmbeddedDatabase,
    file_id: i64,
    symbol_ids: &[i64],
    resolved: &[super::resolver::ResolvedRef],
) -> Result<u64> {
    // Build a name→id map for same-file lookups; reassign from the
    // resolved `to_idx` back to the actual `node_id` written above.
    let _ = symbol_ids;
    let mut written = 0u64;
    for r in resolved {
        let from_id = symbol_ids.get(r.from_idx).copied().ok_or_else(|| {
            Error::query_execution(format!("resolver produced invalid from_idx {}", r.from_idx))
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
        db.execute_params_returning(
            "INSERT INTO _hdb_code_symbol_refs \
               (file_id, from_symbol, to_symbol, to_name, kind, line, resolution) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            &[
                Value::Int8(file_id),
                Value::Int8(from_id),
                to_val,
                Value::String(r.to_name.clone()),
                Value::String(r.kind_str.to_string()),
                Value::Int4(r.line as i32),
                Value::String(res.to_string()),
            ],
        )?;
        written += 1;
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
