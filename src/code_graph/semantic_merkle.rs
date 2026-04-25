//! Semantic-Merkle subtree hash index (FR 4 §4.6).
//!
//! Computes a per-symbol BLAKE3 hash over `(qualified, kind, signature)`
//! and rolls it up per file via the natural symbol → file
//! relationship.  The index lives as a new column on
//! `_hdb_code_symbols.subtree_hash` (materialised on demand) plus a
//! roll-up table `_hdb_code_merkle` that stores
//! `(file_id, rollup_hash, last_updated)`.
//!
//! Used by downstream features (incremental re-embedding and
//! incremental cross-file re-linking) to skip files whose hashes
//! haven't changed.  This is the `WHERE subtree_hash <> '<prev>'`
//! fast-path called out in FR 4.
//!
//! The ASTs the FR describes (`_hdb_code_ast_nodes`) don't land in
//! phase 3 — so the rollup here is at the symbol granularity, not
//! AST-node granularity.  Finer granularity is a phase-3.1 follow-up
//! once `_hdb_code_ast_nodes` ships.

use crate::{EmbeddedDatabase, Result, Value};

#[derive(Debug, Clone, Default)]
pub struct MerkleStats {
    pub files_hashed: u64,
    pub symbols_hashed: u64,
    pub files_unchanged: u64,
}

/// Idempotent: first call builds the roll-up; subsequent calls update
/// only files whose member symbols changed.
pub fn build_or_refresh(db: &EmbeddedDatabase) -> Result<MerkleStats> {
    ensure_rollup_table(db)?;

    // Pull every (file_id, symbol descriptor) in a single scan.
    let rows = db.query(
        "SELECT s.file_id, s.qualified, s.kind, s.signature, \
                s.line_start, s.line_end \
         FROM _hdb_code_symbols s \
         ORDER BY s.file_id, s.node_id",
        &[],
    )?;
    let mut current_file: Option<i64> = None;
    let mut hasher: Option<blake3::Hasher> = None;
    let mut stats = MerkleStats::default();
    let mut per_file_syms = 0u64;

    let emit = |db: &EmbeddedDatabase,
                    stats: &mut MerkleStats,
                    file_id: i64,
                    hash: blake3::Hash,
                    syms: u64|
     -> Result<()> {
        let hex = hash.to_hex().to_string();
        let existing = db.query_params(
            "SELECT rollup_hash FROM _hdb_code_merkle WHERE file_id = $1",
            &[Value::Int8(file_id)],
        )?;
        let prev = existing.first().and_then(|r| match r.values.first() {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        });
        if prev.as_deref() == Some(hex.as_str()) {
            stats.files_unchanged += 1;
        } else if prev.is_some() {
            db.execute_params_returning(
                "UPDATE _hdb_code_merkle SET rollup_hash = $1 WHERE file_id = $2",
                &[Value::String(hex), Value::Int8(file_id)],
            )?;
            stats.files_hashed += 1;
            stats.symbols_hashed += syms;
        } else {
            db.execute_params_returning(
                "INSERT INTO _hdb_code_merkle (file_id, rollup_hash) VALUES ($1, $2)",
                &[Value::Int8(file_id), Value::String(hex)],
            )?;
            stats.files_hashed += 1;
            stats.symbols_hashed += syms;
        }
        Ok(())
    };

    for row in rows {
        let file_id = match row.values.first() {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        if current_file != Some(file_id) {
            if let (Some(fid), Some(h)) = (current_file, hasher.take()) {
                emit(db, &mut stats, fid, h.finalize(), per_file_syms)?;
            }
            current_file = Some(file_id);
            hasher = Some(blake3::Hasher::new());
            per_file_syms = 0;
        }
        let qualified = match row.values.get(1) {
            Some(Value::String(s)) => s.as_str(),
            _ => "",
        };
        let kind = match row.values.get(2) {
            Some(Value::String(s)) => s.as_str(),
            _ => "",
        };
        let signature = match row.values.get(3) {
            Some(Value::String(s)) => s.as_str(),
            _ => "",
        };
        let line_start = match row.values.get(4) {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => 0,
        };
        let line_end = match row.values.get(5) {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => 0,
        };
        if let Some(h) = hasher.as_mut() {
            h.update(qualified.as_bytes());
            h.update(b"\x00");
            h.update(kind.as_bytes());
            h.update(b"\x00");
            h.update(signature.as_bytes());
            h.update(b"\x00");
            h.update(&line_start.to_le_bytes());
            h.update(&line_end.to_le_bytes());
            h.update(b"\n");
            per_file_syms += 1;
        }
    }
    if let (Some(fid), Some(h)) = (current_file, hasher.take()) {
        emit(db, &mut stats, fid, h.finalize(), per_file_syms)?;
    }

    Ok(stats)
}

fn ensure_rollup_table(db: &EmbeddedDatabase) -> Result<()> {
    db.execute(
        "CREATE TABLE IF NOT EXISTS _hdb_code_merkle (\
            file_id BIGINT PRIMARY KEY REFERENCES _hdb_code_files(node_id), \
            rollup_hash TEXT NOT NULL\
         )",
    )?;
    Ok(())
}

/// Return the list of `file_id`s whose roll-up hash is NOT present
/// in `known` — i.e. the set of files that have changed since the
/// caller last checked.  Useful for incremental re-embedding.
pub fn changed_files_since(
    db: &EmbeddedDatabase,
    known: &std::collections::HashMap<i64, String>,
) -> Result<Vec<i64>> {
    let rows = db.query("SELECT file_id, rollup_hash FROM _hdb_code_merkle", &[])?;
    let mut out = Vec::new();
    for row in rows {
        let id = match row.values.first() {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        let hash = match row.values.get(1) {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        if known.get(&id).map(|k| k.as_str()) != Some(&hash) {
            out.push(id);
        }
    }
    Ok(out)
}
