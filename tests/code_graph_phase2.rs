//! Phase-2 regressions: TypeScript support, cross-file resolver,
//! `CREATE EXTENSION hdb_code` DDL. Enable with
//! `--features code-graph`.

#![cfg(feature = "code-graph")]

use heliosdb_nano::{
    code_graph::{CodeIndexOptions, DefinitionHint},
    EmbeddedDatabase, Result, Value,
};

fn setup() -> Result<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        "CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)",
    )?;
    Ok(db)
}

fn insert(db: &EmbeddedDatabase, path: &str, lang: &str, content: &str) -> Result<()> {
    let p = path.replace('\'', "''");
    let l = lang.replace('\'', "''");
    let c = content.replace('\'', "''");
    db.execute(&format!(
        "INSERT INTO src (path, lang, content) VALUES ('{p}', '{l}', '{c}')"
    ))?;
    Ok(())
}

#[test]
fn typescript_extracts_class_and_method() -> Result<()> {
    let db = setup()?;
    insert(
        &db,
        "user.ts",
        "typescript",
        "export class UserRepository {\n  find(id: number): User | null { return null; }\n}\n",
    )?;
    let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
    assert!(stats.files_parsed >= 1, "ts file should parse");

    let defs = db.lsp_definition("UserRepository", &DefinitionHint::default())?;
    assert!(!defs.is_empty(), "UserRepository not found");
    let methods = db.lsp_definition("find", &DefinitionHint::default())?;
    assert!(!methods.is_empty(), "find method not found");
    Ok(())
}

#[test]
fn create_extension_hdb_code_bootstraps_tables() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // Works without any prior code_index call.
    db.execute("CREATE EXTENSION hdb_code")?;
    // The _hdb_code_symbols table should now exist (code_index
    // bootstraps on first use; `CREATE EXTENSION` marks the extension
    // and makes the catalog aware that the code-graph is active).
    // We verify via the installed flag.
    assert!(heliosdb_nano::code_graph::storage::is_extension_installed());
    Ok(())
}

#[test]
fn create_extension_unknown_errors() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let err = db.execute("CREATE EXTENSION does_not_exist");
    assert!(err.is_err(), "expected error on unknown extension, got {err:?}");
    Ok(())
}

#[test]
fn create_extension_unknown_if_not_exists_is_noop() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // IF NOT EXISTS with an unavailable extension is a silent no-op.
    // Models PG's permissive handling of defensive migrations.
    db.execute("CREATE EXTENSION IF NOT EXISTS plpgsql")?;
    Ok(())
}

#[test]
fn go_extracts_function_and_struct() -> Result<()> {
    let db = setup()?;
    insert(
        &db,
        "x.go",
        "go",
        "package p\n\ntype Account struct { ID int }\n\nfunc (a *Account) Name() string { return \"\" }\n\nfunc Hello() string { return \"hi\" }\n",
    )?;
    let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
    assert!(stats.files_parsed >= 1);
    let defs = db.lsp_definition("Hello", &DefinitionHint::default())?;
    assert!(!defs.is_empty());
    let types = db.lsp_definition("Account", &DefinitionHint::default())?;
    assert!(!types.is_empty());
    let methods = db.lsp_definition("Name", &DefinitionHint::default())?;
    assert!(!methods.is_empty());
    Ok(())
}

#[test]
fn markdown_extracts_headings() -> Result<()> {
    let db = setup()?;
    insert(
        &db,
        "README.md",
        "markdown",
        "# Overview\n\nSome text.\n\n## Install\n\nMore.\n\n### Docker\n\nEven more.\n",
    )?;
    let _ = db.code_index(CodeIndexOptions::for_table("src"))?;
    let overview = db.lsp_definition("Overview", &DefinitionHint::default())?;
    assert!(!overview.is_empty(), "markdown heading not extracted");
    let install = db.lsp_definition("Install", &DefinitionHint::default())?;
    assert!(!install.is_empty());
    Ok(())
}

#[test]
fn sql_extracts_tables_and_functions() -> Result<()> {
    let db = setup()?;
    insert(
        &db,
        "schema.sql",
        "sql",
        "CREATE TABLE users (id INT);\n\
         CREATE VIEW recent_users AS SELECT * FROM users;\n",
    )?;
    let _ = db.code_index(CodeIndexOptions::for_table("src"))?;
    // Tree-sitter-sequel may produce different node names across
    // versions; verify at least one symbol was written from the SQL file.
    let rows = db.query(
        "SELECT COUNT(*) FROM _hdb_code_symbols s JOIN _hdb_code_files f \
         ON f.node_id = s.file_id WHERE f.lang = 'sql'",
        &[],
    )?;
    match &rows[0].values[0] {
        heliosdb_nano::Value::Int4(n) => {
            assert!(*n >= 1, "sql symbol count was {n}")
        }
        heliosdb_nano::Value::Int8(n) => {
            assert!(*n >= 1, "sql symbol count was {n}")
        }
        v => panic!("expected integer count, got {v:?}"),
    }
    Ok(())
}

#[test]
fn rust_imports_extracted() -> Result<()> {
    let db = setup()?;
    insert(
        &db,
        "m.rs",
        "rust",
        "use std::collections::HashMap;\nuse crate::foo::Bar;\n\npub fn entry() {}\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    let rows = db.query(
        "SELECT to_name FROM _hdb_code_symbol_refs WHERE kind = 'IMPORTS'",
        &[],
    )?;
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(names.iter().any(|n| n.contains("HashMap")), "got {names:?}");
    assert!(names.iter().any(|n| n.contains("crate::foo::Bar")), "got {names:?}");
    Ok(())
}

#[test]
fn python_imports_extracted() -> Result<()> {
    let db = setup()?;
    insert(
        &db,
        "x.py",
        "python",
        "import os\nfrom collections import defaultdict\n\ndef run():\n    return 1\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    let rows = db.query(
        "SELECT to_name FROM _hdb_code_symbol_refs WHERE kind = 'IMPORTS'",
        &[],
    )?;
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(names.iter().any(|n| n.contains("import os")), "got {names:?}");
    assert!(names.iter().any(|n| n.contains("defaultdict")), "got {names:?}");
    Ok(())
}

#[test]
fn go_imports_extracted() -> Result<()> {
    let db = setup()?;
    insert(
        &db,
        "m.go",
        "go",
        "package p\n\nimport \"fmt\"\n\nfunc Go() { fmt.Println(\"hi\") }\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    let rows = db.query(
        "SELECT to_name FROM _hdb_code_symbol_refs WHERE kind = 'IMPORTS'",
        &[],
    )?;
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(names.iter().any(|n| n.contains("\"fmt\"")), "got {names:?}");
    Ok(())
}

#[test]
fn cross_file_ref_resolves() -> Result<()> {
    let db = setup()?;
    // `caller.py` calls `helper()` from `helper.py`; the in-file
    // resolver can't find it, but the cross-file pass should rebind.
    insert(
        &db,
        "helper.py",
        "python",
        "def helper():\n    return 42\n",
    )?;
    insert(
        &db,
        "caller.py",
        "python",
        "def run():\n    return helper()\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;

    // The ref from run → helper should be resolved cross-file.
    let rows = db.query(
        "SELECT resolution FROM _hdb_code_symbol_refs WHERE to_name = 'helper'",
        &[],
    )?;
    assert!(!rows.is_empty(), "no helper ref row found");
    let res = match rows[0].values.first() {
        Some(Value::String(s)) => s.clone(),
        _ => panic!("resolution column missing"),
    };
    assert!(
        res == "exact" || res == "heuristic",
        "cross-file ref should be resolved, got {res:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// BUGS_CODE_INDEX_FK_VIOLATION_v3_21_1.md acceptance fixtures
// ---------------------------------------------------------------------------
//
// Pilot client (`heliosdb-codekb-mcp`) reports that any `code_index`
// call against a populated KB raises:
//
//   Foreign key constraint
//     'fk__hdb_code_symbol_refs_from_symbol___hdb_code_symbols' violated:
//     cannot delete row from '_hdb_code_symbols' - referenced by
//     '_hdb_code_symbol_refs'
//
// preceded by a slow `DELETE FROM _hdb_code_symbol_refs WHERE file_id = ?`.
// Both `--force` and incremental-with-changed-file paths surface it.
// Acceptance: these three fixtures must succeed end to end on every
// release.

const PHASE2_FIXTURE: &[(&str, &str, &str)] = &[
    ("a.rs", "rust", "pub fn alpha() { beta(); }\npub fn beta() {}\npub struct S;\nimpl S { pub fn m(&self) -> i32 { 0 } }\n"),
    ("b.rs", "rust", "use crate::a::S;\npub fn make() -> S { S }\npub fn caller() { let s = make(); s.m(); }\n"),
    ("c.rs", "rust", "pub trait T { fn run(&self); }\npub struct U;\nimpl T for U { fn run(&self) {} }\n"),
    ("d.py", "python", "def hello():\n    print('hi')\n\nclass Foo:\n    def bar(self):\n        return hello()\n"),
    ("e.py", "python", "from d import Foo\n\ndef use_foo():\n    f = Foo()\n    return f.bar()\n"),
];

fn populate_phase2_corpus(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    for (path, lang, content) in PHASE2_FIXTURE {
        db.execute_params_returning(
            "INSERT INTO src VALUES ($1, $2, $3)",
            &[
                Value::String((*path).into()),
                Value::String((*lang).into()),
                Value::String((*content).into()),
            ],
        )?;
    }
    Ok(())
}

#[test]
fn ingest_twice_against_populated_kb() -> Result<()> {
    // Pilot path: first ingest succeeds (cold), second ingest with no
    // content changes goes through the SHA gate and short-circuits.
    let db = EmbeddedDatabase::new_in_memory()?;
    populate_phase2_corpus(&db)?;

    let stats1 = db.code_index(CodeIndexOptions::for_table("src"))?;
    assert!(stats1.symbols_written > 0, "first ingest wrote no symbols");
    assert!(stats1.refs_written > 0, "first ingest wrote no refs");

    let stats2 = db.code_index(CodeIndexOptions::for_table("src"))?;
    assert_eq!(
        stats2.files_unchanged as usize,
        PHASE2_FIXTURE.len(),
        "expected all {} files to be SHA-gated unchanged on the second ingest, got {} unchanged + {} parsed",
        PHASE2_FIXTURE.len(),
        stats2.files_unchanged,
        stats2.files_parsed,
    );
    assert_eq!(stats2.files_parsed, 0, "no files should be re-parsed when SHAs match");
    Ok(())
}

#[test]
fn ingest_twice_with_one_changed_file_against_populated_kb() -> Result<()> {
    // Daily incremental workflow: a single file's content changed.
    // The per-file `delete-stale → re-insert` path runs for that file
    // only. Must NOT raise the FK violation.
    let db = EmbeddedDatabase::new_in_memory()?;
    populate_phase2_corpus(&db)?;

    db.code_index(CodeIndexOptions::for_table("src"))?;

    // Touch a file whose symbols are referenced by other files (b.rs
    // imports from a.rs, so a.rs is a juicy target).
    db.execute_params_returning(
        "UPDATE src SET content = $1 WHERE path = $2",
        &[
            Value::String(
                "pub fn alpha() { beta(); gamma(); }\npub fn beta() {}\npub fn gamma() {}\npub struct S;\nimpl S { pub fn m(&self) -> i32 { 0 } }\n".into(),
            ),
            Value::String("a.rs".into()),
        ],
    )?;

    let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
    assert_eq!(stats.files_parsed, 1, "exactly one file should be re-parsed");
    assert!(stats.files_unchanged >= (PHASE2_FIXTURE.len() - 1) as u64);
    Ok(())
}

#[test]
fn force_reparse_against_populated_kb() -> Result<()> {
    // `--force` workflow: TRUNCATE + bulk re-insert. Must succeed end
    // to end with non-zero `symbols_written` (the failure mode the
    // pilot saw was Err(...) returned, plugin logging at WARN, no
    // `code_index` summary line — so the wall-clock looked fast but
    // no work persisted).
    let db = EmbeddedDatabase::new_in_memory()?;
    populate_phase2_corpus(&db)?;

    db.code_index(CodeIndexOptions::for_table("src"))?;

    let mut opts = CodeIndexOptions::for_table("src");
    opts.force_reparse = true;
    let stats = db.code_index(opts)?;
    assert!(
        stats.symbols_written > 0,
        "force-reparse must persist non-zero symbols (was {})",
        stats.symbols_written
    );
    assert!(
        stats.refs_written > 0,
        "force-reparse must persist non-zero refs (was {})",
        stats.refs_written
    );
    Ok(())
}
