//! Source-text refactor primitives — rename apply.
//!
//! Takes the same `(symbol_id, new_name)` pair as
//! `helios_lsp_rename_preview`, walks the rename plan against the
//! source rows, and writes the replacements back to the underlying
//! source table.  Identifier-boundary aware so `foo` doesn't match
//! `foobar`.
//!
//! Conflict detection: each touched row's content is hashed before
//! the apply.  If the hash differs from what was indexed (i.e.
//! someone edited the source between preview and apply), the apply
//! aborts.  Avoids racing with hand edits.

use sha2::{Digest, Sha256};

use crate::{EmbeddedDatabase, Error, Result, Value};

#[derive(Debug, Clone)]
pub struct RenameApplyOptions {
    /// User source table whose `(path, content)` rows hold the
    /// authoritative source we'll rewrite. Defaults to `"src"`,
    /// matching the FR-1 pilot convention.
    pub source_table: String,
    /// When true, runs the entire pipeline but skips the writes.
    /// Useful for paired previewing.
    pub dry_run: bool,
    /// When true, abort if any touched row's content hash differs
    /// from the indexed `_hdb_code_files.sha256` value.  When
    /// false, applies anyway (caller has accepted the risk).
    pub strict_hash_check: bool,
}

impl Default for RenameApplyOptions {
    fn default() -> Self {
        Self {
            source_table: "src".to_string(),
            dry_run: false,
            strict_hash_check: true,
        }
    }
}

impl RenameApplyOptions {
    pub fn dry_run() -> Self {
        Self { dry_run: true, ..Self::default() }
    }
    pub fn apply() -> Self {
        Self { dry_run: false, ..Self::default() }
    }
    pub fn with_source_table(mut self, t: impl Into<String>) -> Self {
        self.source_table = t.into();
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct RenameApplyStats {
    pub files_modified: u64,
    pub occurrences_replaced: u64,
    pub applied: bool,
    /// Source files that fell out of the apply because their on-disk
    /// content drifted since the last index pass.
    pub conflicted_paths: Vec<String>,
}

/// Apply a symbol rename.  Source-table column convention follows
/// the indexer's: `(path TEXT PRIMARY KEY, lang TEXT, content TEXT)`.
/// The `source_table` is read from the symbol's
/// `_hdb_code_files.source_table` column when present, defaulting
/// to `"src"` to match the FR-1 pilot.
pub fn rename_apply(
    db: &EmbeddedDatabase,
    symbol_id: i64,
    new_name: &str,
    opts: &RenameApplyOptions,
) -> Result<RenameApplyStats> {
    if new_name.trim().is_empty() {
        return Err(Error::query_execution(
            "rename_apply: new_name must not be empty",
        ));
    }

    // Symbol → its name.  Inline integer literal: symbol_id is a
    // server-generated BIGSERIAL we already trust.
    let sym_rows = db.query(
        &format!(
            "SELECT s.name FROM _hdb_code_symbols s WHERE s.node_id = {symbol_id}"
        ),
        &[],
    )?;
    let Some(row) = sym_rows.into_iter().next() else {
        return Ok(RenameApplyStats::default());
    };
    let old_name = match row.values.first() {
        Some(Value::String(s)) => s.clone(),
        _ => return Ok(RenameApplyStats::default()),
    };
    if old_name == new_name {
        // No-op; nothing to write.
        return Ok(RenameApplyStats {
            applied: !opts.dry_run,
            ..Default::default()
        });
    }

    // Definition site path + source_table + sha.  After #201 the
    // indexer writes the actual user-table name into
    // `_hdb_code_files.source_table` so we trust the column.  When
    // it disagrees with the explicit `opts.source_table` (different
    // index passes against different tables), the explicit value
    // wins.
    let def_rows = db.query(
        &format!(
            "SELECT f.path, f.source_table, f.sha256 \
             FROM _hdb_code_symbols s \
             JOIN _hdb_code_files f ON f.node_id = s.file_id \
             WHERE s.node_id = {symbol_id}"
        ),
        &[],
    )?;
    let (def_path, src_from_index, def_sha) = match def_rows.into_iter().next() {
        Some(r) => (
            r.values.first().and_then(string_of).unwrap_or_default(),
            r.values.get(1).and_then(string_of),
            r.values.get(2).and_then(string_of),
        ),
        None => return Ok(RenameApplyStats::default()),
    };
    let source_table = if opts.source_table != "src" {
        opts.source_table.clone()
    } else {
        src_from_index.unwrap_or_else(|| opts.source_table.clone())
    };

    let ref_rows = db.query(
        &format!(
            "SELECT f.path, f.sha256 \
             FROM _hdb_code_symbol_refs r \
             JOIN _hdb_code_files f ON f.node_id = r.file_id \
             WHERE r.to_symbol = {symbol_id}"
        ),
        &[],
    )?;
    // Dedup paths; track the indexed sha256 per path for the
    // conflict-detection step.
    let mut paths: std::collections::BTreeMap<String, Option<String>> =
        std::collections::BTreeMap::new();
    paths.insert(def_path.clone(), def_sha);
    for r in ref_rows {
        if let Some(p) = r.values.first().and_then(string_of) {
            let sha = r.values.get(1).and_then(string_of);
            paths.entry(p).or_insert(sha);
        }
    }

    let mut stats = RenameApplyStats::default();
    for (path, indexed_sha) in &paths {
        // Read the current content.
        let row = db.query_params(
            &format!("SELECT content FROM {source_table} WHERE path = $1"),
            &[Value::String(path.clone())],
        )?;
        let Some(t) = row.into_iter().next() else { continue };
        let content = match t.values.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        if opts.strict_hash_check {
            let live_sha = sha256_hex(&content);
            if let Some(prev) = indexed_sha {
                if prev != &live_sha {
                    stats.conflicted_paths.push(path.clone());
                    continue;
                }
            }
        }
        let (new_content, replacements) = replace_word(&content, &old_name, new_name);
        if replacements == 0 {
            continue;
        }
        stats.occurrences_replaced += replacements as u64;
        stats.files_modified += 1;
        if !opts.dry_run {
            db.execute_params(
                &format!("UPDATE {source_table} SET content = $1 WHERE path = $2"),
                &[Value::String(new_content), Value::String(path.clone())],
            )?;
        }
    }

    if !stats.conflicted_paths.is_empty() && opts.strict_hash_check {
        return Err(Error::query_execution(format!(
            "rename_apply: {} file(s) drifted since the last index pass: {}",
            stats.conflicted_paths.len(),
            stats.conflicted_paths.join(", ")
        )));
    }

    stats.applied = !opts.dry_run;
    Ok(stats)
}

fn sha256_hex(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    hex::encode(h.finalize())
}

fn string_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Replace whole-word occurrences of `old` with `new` in `text`.
/// Returns (rewritten_text, replacement_count).  Identifier
/// boundary characters: ASCII alphanumeric, `_`, `:`, `.` — same
/// set the entity linker uses.
fn replace_word(text: &str, old: &str, new: &str) -> (String, usize) {
    if old.is_empty() {
        return (text.to_string(), 0);
    }
    let mut out = String::with_capacity(text.len());
    let mut count = 0usize;
    let mut i = 0usize;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if i + old.len() <= bytes.len() && &bytes[i..i + old.len()] == old.as_bytes() {
            let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            let after_idx = i + old.len();
            let after_ok = after_idx == bytes.len() || !is_ident_char(bytes[after_idx]);
            if before_ok && after_ok {
                out.push_str(new);
                count += 1;
                i = after_idx;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    (out, count)
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_word_respects_boundaries() {
        let (out, n) = replace_word("foo bar foo_bar foobar", "foo", "qux");
        assert_eq!(n, 1);
        assert_eq!(out, "qux bar foo_bar foobar");
    }

    #[test]
    fn replace_word_handles_multiple_lines() {
        let (out, n) = replace_word("foo\nfoo()\nbar", "foo", "x");
        assert_eq!(n, 2);
        assert_eq!(out, "x\nx()\nbar");
    }

    #[test]
    fn replace_word_zero_when_no_match() {
        let (out, n) = replace_word("alpha beta", "gamma", "delta");
        assert_eq!(n, 0);
        assert_eq!(out, "alpha beta");
    }
}
