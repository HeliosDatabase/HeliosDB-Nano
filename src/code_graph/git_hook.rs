//! Git-hook helper. Reads a list of changed paths from stdin (one
//! per line — the format `git diff --name-only` produces), upserts
//! each file's content into a target `src` table, and re-runs
//! `code_index` over the table. Designed to be called from
//! `.githooks/post-commit`:
//!
//! ```bash
//! #!/usr/bin/env bash
//! git diff-tree --no-commit-id --name-only -r HEAD -- \
//!   | heliosdb-nano code-graph hook \
//!       --data-dir .helios-index/heliosdb-data \
//!       --source-table src
//! ```
//!
//! Deleted files (present in the diff but missing on disk) are
//! removed from the `src` table so the content-hash gate in
//! `code_index` reflects the working tree exactly.

use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

use crate::{EmbeddedDatabase, Error, Result, Value};

use super::storage::{code_index, CodeIndexOptions};

/// Run the hook. `stdin` is line-delimited paths relative to
/// `repo_root`. Returns the resulting `CodeIndexStats`.
pub fn run_from_stdin(
    data_dir: &Path,
    repo_root: &Path,
    source_table: &str,
) -> Result<super::storage::CodeIndexStats> {
    let paths: Vec<String> = io::stdin()
        .lock()
        .lines()
        .filter_map(|l| l.ok())
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    run(data_dir, repo_root, source_table, &paths)
}

/// Same as `run_from_stdin` but with an explicit path list — usable
/// from tests without redirecting stdin.
pub fn run(
    data_dir: &Path,
    repo_root: &Path,
    source_table: &str,
    paths: &[String],
) -> Result<super::storage::CodeIndexStats> {
    let db = if data_dir.as_os_str().is_empty() {
        EmbeddedDatabase::new_in_memory()?
    } else {
        fs::create_dir_all(data_dir)
            .map_err(|e| Error::storage(format!("create {data_dir:?}: {e}")))?;
        EmbeddedDatabase::new(data_dir)?
    };

    ensure_source_table(&db, source_table)?;

    for p in paths {
        let abs = if PathBuf::from(p).is_absolute() {
            PathBuf::from(p)
        } else {
            repo_root.join(p)
        };
        if !abs.exists() {
            // Deleted file — drop its row so the hash gate sees it gone.
            db.execute_params_returning(
                &format!("DELETE FROM \"{source_table}\" WHERE path = $1"),
                &[Value::String(p.clone())],
            )?;
            continue;
        }
        let Some(lang) = lang_from_path(&abs) else {
            continue;
        };
        let content = match fs::read_to_string(&abs) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let bytes = content.len() as i64;
        db.execute_params_returning(
            &format!("DELETE FROM \"{source_table}\" WHERE path = $1"),
            &[Value::String(p.clone())],
        )?;
        db.execute_params_returning(
            &format!(
                "INSERT INTO \"{source_table}\" (path, lang, content, size_bytes) \
                 VALUES ($1, $2, $3, $4)"
            ),
            &[
                Value::String(p.clone()),
                Value::String(lang.to_string()),
                Value::String(content),
                Value::Int8(bytes),
            ],
        )?;
    }

    // Content-hash gate in code_index will skip unchanged files.
    code_index(&db, CodeIndexOptions::for_table(source_table))
}

fn ensure_source_table(db: &EmbeddedDatabase, source_table: &str) -> Result<()> {
    db.execute(&format!(
        "CREATE TABLE IF NOT EXISTS \"{source_table}\" (\
            path TEXT PRIMARY KEY, lang TEXT, content TEXT, size_bytes INTEGER\
         )"
    ))?;
    Ok(())
}

fn lang_from_path(p: &Path) -> Option<&'static str> {
    let ext = p.extension()?.to_str()?;
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "ts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "js" | "mjs" | "cjs" => Some("javascript"),
        "go" => Some("go"),
        "md" | "markdown" => Some("markdown"),
        "sql" => Some("sql"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn processes_change_list() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(repo.join("b.rs"), "pub fn b() {}\n").unwrap();
        let stats =
            run(&data, &repo, "src", &["a.rs".into(), "b.rs".into()]).unwrap();
        assert_eq!(stats.files_parsed, 2);

        // Modify one, hook reports only that one. code_index scans
        // the full src table, so b.rs shows up as files_unchanged.
        std::fs::write(repo.join("a.rs"), "pub fn a2() {}\n").unwrap();
        let stats2 = run(&data, &repo, "src", &["a.rs".into()]).unwrap();
        assert_eq!(stats2.files_parsed, 1);
        assert_eq!(stats2.files_unchanged, 1);
    }

    #[test]
    fn removes_deleted_files() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("x.rs"), "pub fn x() {}\n").unwrap();
        run(&data, &repo, "src", &["x.rs".into()]).unwrap();

        std::fs::remove_file(repo.join("x.rs")).unwrap();
        run(&data, &repo, "src", &["x.rs".into()]).unwrap();

        let db = EmbeddedDatabase::new(&data).unwrap();
        let rows = db.query("SELECT COUNT(*) FROM src", &[]).unwrap();
        assert_eq!(
            match &rows[0].values[0] {
                Value::Int4(n) => *n as i64,
                Value::Int8(n) => *n,
                _ => -1,
            },
            0
        );
    }
}
