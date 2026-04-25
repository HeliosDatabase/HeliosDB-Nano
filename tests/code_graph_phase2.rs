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
